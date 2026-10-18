//! Regex content search tool.

use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use indoc::formatdoc;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::{
    GrepFileGroup, GrepMatchLine, NO_FILES_FOUND, Prepared, SearchRoot, Tool, ToolCtx,
    ToolMetadata, ToolOutput, ToolResultView, ToolSpec, Unreadable, extract_input_field,
};
use crate::permission::{PermissionClass, ReadFilter, WalkFilter};
use crate::util::text::plural;

const NO_MATCHES_FOUND: &str = "No matches found";
const RESULTS_LIMIT_PREFIX: &str = "(Results limited to ";

const DEFAULT_HEAD_LIMIT: usize = 250;
const MAX_LINE_CHARS: usize = 500;
const MAX_GREP_FILE_SIZE: u64 = 1024 * 1024;
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Withholds each file a `read` deny or ask rule matches from the results.
#[cfg_attr(test, derive(Default))]
pub(crate) struct GrepTool {
    read_filter: ReadFilter,
}

impl GrepTool {
    pub(crate) fn new(read_filter: ReadFilter) -> Self {
        Self { read_filter }
    }
}

impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".to_owned(),
            description: "Search file contents using a regular expression.".to_owned(),
            input_schema: input_schema(),
            class: PermissionClass::Read,
            parallel_safe: true,
        }
    }

    fn prepare(&self, input: serde_json::Value, ctx: &ToolCtx) -> Prepared {
        super::prepare_search_root(input, ctx)
    }

    fn icon(&self) -> &'static str {
        "⌕"
    }

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "pattern")
    }

    fn result_view(
        &self,
        input: &serde_json::Value,
        content: &str,
        _metadata: &ToolMetadata,
        _cwd: &Path,
    ) -> Option<ToolResultView> {
        // Only content mode produces line-numbered rows, so the other modes fall through and keep
        // their summary header visible.
        let mode = input
            .get("output_mode")
            .map_or(Ok(OutputMode::default()), OutputMode::deserialize);
        if !matches!(mode, Ok(OutputMode::Content)) {
            return None;
        }
        parse_content_view(content)
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(run(prepared, ctx, self.read_filter.clone()))
    }
}

// ── Input ──

fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "The regular expression pattern to search for"
            },
            "path": {
                "type": "string",
                "description": "File or directory to search in (default: current working directory)"
            },
            "include": {
                "type": "string",
                "description": "Glob pattern to filter by filename \
                                (e.g. \"*.rs\", \"*.{ts,tsx}\"). \
                                Matches the basename only, not the full path."
            },
            "output_mode": {
                "type": "string",
                "enum": ["content", "files_with_matches", "count"],
                "description": "Output mode: \"content\" shows matching lines (default), \
                                \"files_with_matches\" shows file paths only, \
                                \"count\" shows match counts per file"
            },
            "context": {
                "type": "integer",
                "description": "Number of context lines to show before and after each match \
                                (content mode only)"
            },
            "case_insensitive": {
                "type": "boolean",
                "description": "Case-insensitive search (default: false)"
            },
            "head_limit": {
                "type": "integer",
                "description": "Limit output to first N entries (default: 250, 0 for unlimited)"
            }
        },
        "required": ["pattern"]
    })
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    Content,
    FilesWithMatches,
    Count,
}

#[derive(Deserialize)]
struct Input {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
    #[serde(default)]
    output_mode: OutputMode,
    context: Option<usize>,
    #[serde(default)]
    case_insensitive: bool,
    head_limit: Option<usize>,
}

// ── Execution ──

async fn run(prepared: Prepared, ctx: &ToolCtx, read_filter: ReadFilter) -> ToolOutput {
    let Prepared { input: raw, access } = prepared;
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(msg) => return ToolOutput::error(msg),
    };

    let Input {
        pattern,
        path,
        include,
        output_mode,
        context,
        case_insensitive,
        head_limit,
    } = input;

    let workspace = Arc::clone(&ctx.workspace);
    let cancel = ctx.cancel.clone();
    match super::run_blocking(move || {
        grep_files(&GrepParams {
            pattern: &pattern,
            root: SearchRoot::new(&access, path.as_deref(), workspace)?,
            include_glob: include.as_deref(),
            output_mode,
            context: context.unwrap_or(0),
            case_insensitive,
            head_limit,
            read_filter,
            cancel,
        })
    })
    .await
    {
        Ok(content) => {
            let title = grep_title(super::strip_search_notes(&content));
            ToolOutput::success(content).with_title(title)
        }
        Err(msg) => ToolOutput::error(msg),
    }
}

fn grep_title(output: &str) -> String {
    if matches!(output, NO_MATCHES_FOUND | NO_FILES_FOUND) {
        return NO_MATCHES_FOUND.into();
    }
    // Count / files-with-matches modes lead with "Found N ...".
    if let Some(line) = output.lines().find(|l| l.starts_with("Found ")) {
        return line.to_owned();
    }
    let match_count = output
        .lines()
        .filter_map(parse_match_line)
        .filter(|(_, m)| m.is_match)
        .count();
    let word = plural(match_count, "match", "matches");
    format!("{match_count} {word}")
}

// ── Search ──

struct GrepParams<'a> {
    pattern: &'a str,
    /// The resolved file or directory to search. Result paths render relative to its `cwd`.
    root: SearchRoot,
    include_glob: Option<&'a str>,
    output_mode: OutputMode,
    context: usize,
    case_insensitive: bool,
    head_limit: Option<usize>,
    read_filter: ReadFilter,
    /// Stops the walk and the file reads once the call is interrupted.
    cancel: CancellationToken,
}

fn grep_files(params: &GrepParams<'_>) -> Result<String, String> {
    // Each line is matched alone, so `^` and `$` anchor at line boundaries without the multi-line
    // flag. The size limits keep an adversarial pattern from exhausting memory.
    let re = regex::RegexBuilder::new(params.pattern)
        .case_insensitive(params.case_insensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| format!("Invalid regex: {e}"))?;

    let include_matcher = params
        .include_glob
        .map(|g| globset::Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| format!("Invalid include pattern: {e}"))?;

    let head_limit = match params.head_limit {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => DEFAULT_HEAD_LIMIT,
    };

    let root = &params.root;
    let read_filter = params
        .read_filter
        .walk(&root.base, &root.spelled, &root.workspace);
    let CollectedFiles {
        files,
        skipped_large,
        mut unreadable,
        withheld,
    } = collect_files(
        &root.base,
        include_matcher.as_ref(),
        &read_filter,
        &params.cancel,
    );

    let cwd = &root.cwd;
    let texts = texts(&files, &params.cancel, &mut unreadable);
    let mut result = match params.output_mode {
        OutputMode::FilesWithMatches => format_files_with_matches(texts, cwd, &re, head_limit),
        OutputMode::Count => format_count(texts, cwd, &re, head_limit),
        OutputMode::Content => format_content(texts, cwd, &re, params.context, head_limit),
    };

    push_skipped_note(&mut result, &skipped_large, cwd);
    super::push_unreadable_note(&mut result, &unreadable, cwd);
    super::push_withheld_note(&mut result, withheld);

    Ok(result)
}

struct CollectedFiles {
    files: Vec<PathBuf>,
    skipped_large: Vec<(PathBuf, u64)>,
    /// Paths the walk could not read.
    unreadable: Vec<Unreadable>,
    /// Files and unreadable paths a `read` deny or ask rule withheld.
    withheld: usize,
}

/// The files to search under `base`. A file `base` itself passed the gate, so only the files a walk
/// finds face `read_filter`.
fn collect_files(
    base: &Path,
    include_matcher: Option<&globset::GlobMatcher>,
    read_filter: &WalkFilter<'_>,
    cancel: &CancellationToken,
) -> CollectedFiles {
    if base.is_file() {
        let mut files = Vec::new();
        let mut skipped_large = Vec::new();
        if let Ok(meta) = base.metadata()
            && meta.len() > MAX_GREP_FILE_SIZE
        {
            skipped_large.push((base.to_path_buf(), meta.len()));
        } else {
            files.push(base.to_path_buf());
        }
        return CollectedFiles {
            files,
            skipped_large,
            unreadable: Vec::new(),
            withheld: 0,
        };
    }

    let mut skipped_large = Vec::new();
    let mut unreadable = Vec::new();
    let mut withheld = 0;
    let entries = super::walk_files(base, cancel, &mut unreadable)
        .filter(|entry| {
            include_matcher.is_none_or(|m| {
                let name = entry.file_name().to_string_lossy();
                m.is_match(name.as_ref())
            })
        })
        .filter(|entry| {
            let held = read_filter.withholds(entry.path());
            withheld += usize::from(held);
            !held
        })
        .filter(|entry| {
            if let Ok(meta) = entry.metadata()
                && meta.len() > MAX_GREP_FILE_SIZE
            {
                skipped_large.push((entry.path().to_path_buf(), meta.len()));
                return false;
            }
            true
        });
    let files = super::newest_first(entries)
        .into_iter()
        .map(ignore::DirEntry::into_path)
        .collect();
    withheld += super::withhold_unreadable(&mut unreadable, read_filter);

    CollectedFiles {
        files,
        skipped_large,
        unreadable,
        withheld,
    }
}

/// Each of `files` that reads as text, with its content, until `cancel` fires. Each file whose read
/// fails goes to `unreadable`, except one that vanished after the walk found it.
fn texts<'a>(
    files: &'a [PathBuf],
    cancel: &'a CancellationToken,
    unreadable: &'a mut Vec<Unreadable>,
) -> impl Iterator<Item = (&'a Path, String)> + 'a {
    files
        .iter()
        .take_while(|_| !cancel.is_cancelled())
        .filter_map(move |path| match read_text(path) {
            Ok(text) => text.map(|text| (path.as_path(), text)),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    unreadable.push(Unreadable::from_read(path, &e));
                }
                None
            }
        })
}

/// The text of the file at `path`, or `None` when it is binary.
fn read_text(path: &Path) -> std::io::Result<Option<String>> {
    let bytes = std::fs::read(path)?;
    if super::is_binary(&bytes) {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

fn push_skipped_note(output: &mut String, skipped: &[(PathBuf, u64)], cwd: &Path) {
    let limit_mb = MAX_GREP_FILE_SIZE / (1024 * 1024);
    let lines: Vec<String> = skipped
        .iter()
        .map(|(path, size)| {
            let mb = super::bytes_to_mb(*size);
            format!("{} ({mb:.1} MB)", super::display_path(path, cwd))
        })
        .collect();
    let header = format!("Skipped (exceeds {limit_mb} MB size limit):");
    super::push_path_note(output, &header, &lines);
}

// ── Content Mode ──

fn format_content<'a>(
    texts: impl Iterator<Item = (&'a Path, String)>,
    cwd: &Path,
    re: &regex::Regex,
    context: usize,
    head_limit: usize,
) -> String {
    // Collecting one row past the limit proves more output exists, so the footer only appears when
    // a row was actually dropped.
    let capacity = head_limit.saturating_add(1);
    let mut output_lines: Vec<String> = Vec::new();

    for (path, text) in texts {
        if output_lines.len() >= capacity {
            break;
        }

        let display_path = super::display_path(path, cwd);

        if context == 0 {
            search_no_context(&text, re, &display_path, &mut output_lines, capacity);
        } else {
            search_with_context(
                &text,
                re,
                &display_path,
                context,
                &mut output_lines,
                capacity,
            );
        }
    }

    if output_lines.is_empty() {
        return NO_MATCHES_FOUND.into();
    }

    let truncated = output_lines.len() > head_limit;
    output_lines.truncate(head_limit);
    let mut output = output_lines.join("\n");

    if truncated {
        _ = write!(output, "\n\n{RESULTS_LIMIT_PREFIX}{head_limit} lines)");
    }

    output
}

fn search_no_context(
    text: &str,
    re: &regex::Regex,
    display_path: &str,
    output_lines: &mut Vec<String>,
    capacity: usize,
) {
    for (line_num, line) in text.lines().enumerate() {
        if output_lines.len() >= capacity {
            return;
        }
        if re.is_match(line) {
            let truncated = super::truncate_line(line, MAX_LINE_CHARS);
            output_lines.push(format!("{display_path}:{}:{truncated}", line_num + 1));
        }
    }
}

fn search_with_context(
    text: &str,
    re: &regex::Regex,
    display_path: &str,
    context: usize,
    output_lines: &mut Vec<String>,
    capacity: usize,
) {
    let lines: Vec<&str> = text.lines().collect();
    let match_indices: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| re.is_match(line))
        .map(|(i, _)| i)
        .collect();

    if match_indices.is_empty() {
        return;
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &idx in &match_indices {
        let start = idx.saturating_sub(context);
        let end = (idx + context).min(lines.len() - 1);
        if let Some(last) = ranges.last_mut()
            && start <= last.1 + 1
        {
            last.1 = end;
            continue;
        }
        ranges.push((start, end));
    }

    for (range_idx, &(start, end)) in ranges.iter().enumerate() {
        if output_lines.len() >= capacity {
            return;
        }
        if range_idx > 0 {
            output_lines.push("--".to_owned());
        }
        for (i, line) in lines.iter().enumerate().take(end + 1).skip(start) {
            if output_lines.len() >= capacity {
                return;
            }
            let sep = if match_indices.binary_search(&i).is_ok() {
                ':'
            } else {
                '-'
            };
            let truncated = super::truncate_line(line, MAX_LINE_CHARS);
            output_lines.push(format!("{display_path}:{}{sep}{truncated}", i + 1));
        }
    }
}

// ── Files-with-Matches Mode ──

fn format_files_with_matches<'a>(
    texts: impl Iterator<Item = (&'a Path, String)>,
    cwd: &Path,
    re: &regex::Regex,
    head_limit: usize,
) -> String {
    let mut matching_files: Vec<String> = Vec::new();

    for (path, text) in texts {
        if matching_files.len() > head_limit {
            break;
        }

        if text.lines().any(|line| re.is_match(line)) {
            matching_files.push(super::display_path(path, cwd));
        }
    }

    if matching_files.is_empty() {
        return NO_FILES_FOUND.into();
    }

    let truncated = matching_files.len() > head_limit;
    matching_files.truncate(head_limit);
    let num_files = matching_files.len();
    let file_word = plural(num_files, "file", "files");

    let files = matching_files.join("\n");
    let mut output = formatdoc! {"
        Found {num_files} {file_word}
        {files}"
    };

    if truncated {
        _ = write!(output, "\n\n{RESULTS_LIMIT_PREFIX}{head_limit} files)");
    }

    output
}

// ── Count Mode ──

fn format_count<'a>(
    texts: impl Iterator<Item = (&'a Path, String)>,
    cwd: &Path,
    re: &regex::Regex,
    head_limit: usize,
) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut total_matches: usize = 0;

    for (path, text) in texts {
        let count = text.lines().filter(|line| re.is_match(line)).count();
        if count > 0 {
            total_matches += count;
            counts.push((super::display_path(path, cwd), count));
        }
    }

    if counts.is_empty() {
        return NO_MATCHES_FOUND.into();
    }

    let total_files = counts.len();
    let truncated = total_files > head_limit;
    counts.truncate(head_limit);

    let mut output = format!(
        "Found {total_matches} total {} across {total_files} {}",
        plural(total_matches, "occurrence", "occurrences"),
        plural(total_files, "file", "files"),
    );
    for (p, c) in &counts {
        _ = write!(output, "\n{p}:{c}");
    }
    if truncated {
        _ = write!(output, "\n\n{RESULTS_LIMIT_PREFIX}{head_limit} files)");
    }

    output
}

// ── Result View ──

/// Returns `None` for any unrecognized line so the block falls through to text.
fn parse_content_view(content: &str) -> Option<ToolResultView> {
    let trimmed = content.trim_end();
    if trimmed == NO_MATCHES_FOUND {
        return Some(ToolResultView::GrepMatches {
            groups: Vec::new(),
            truncated: false,
        });
    }

    let mut groups: Vec<GrepFileGroup> = Vec::new();
    let mut truncated = false;

    for line in trimmed.lines() {
        if line.is_empty() || line == "--" {
            continue;
        }
        if line.starts_with(RESULTS_LIMIT_PREFIX) && line.ends_with(')') {
            truncated = true;
            continue;
        }
        let (path, match_line) = parse_match_line(line)?;
        match groups.last_mut() {
            Some(g) if g.path == path => g.lines.push(match_line),
            _ => groups.push(GrepFileGroup {
                path: path.to_owned(),
                lines: vec![match_line],
            }),
        }
    }

    Some(ToolResultView::GrepMatches { groups, truncated })
}

/// Parses one row: `path:NUM:text` (match) or `path:NUM-text` (context).
///
/// Paths can contain `:` (Windows drive letters, URL-shaped names), so the row commits on the first
/// colon followed by digits and a `:` / `-` separator, which marks the line-number column.
fn parse_match_line(line: &str) -> Option<(&str, GrepMatchLine)> {
    let mut search_start = 0;
    while let Some(off) = line[search_start..].find(':') {
        let colon = search_start + off;
        let after = &line[colon + 1..];
        let digit_end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if digit_end == 0 {
            search_start = colon + 1;
            continue;
        }
        let is_match = match after.as_bytes().get(digit_end) {
            Some(b':') => true,
            Some(b'-') => false,
            _ => {
                search_start = colon + 1;
                continue;
            }
        };
        let number = after[..digit_end].parse::<usize>().ok()?;
        let text = after[digit_end + 1..].to_owned();
        let path = &line[..colon];
        return Some((
            path,
            GrepMatchLine {
                number,
                text,
                is_match,
            },
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::permission::Access;
    use crate::tool::testing::{self, canonical_tempdir, ctx, deny_reads, search_root, temp_ctx};
    use crate::util::path::slash_separated;

    /// Content-mode params searching all of `cwd`, so result paths render relative to it.
    fn params<'a>(pattern: &'a str, cwd: &Path) -> GrepParams<'a> {
        GrepParams {
            pattern,
            root: search_root(cwd, None).unwrap(),
            include_glob: None,
            output_mode: OutputMode::Content,
            context: 0,
            case_insensitive: false,
            head_limit: None,
            read_filter: ReadFilter::default(),
            cancel: CancellationToken::new(),
        }
    }

    // ── prepare ──

    #[test]
    fn prepare_defaults_the_search_root_to_cwd() {
        // A deny scoped to the project root must apply when `path` is omitted.
        let dir = canonical_tempdir();

        let prepared =
            GrepTool::default().prepare(serde_json::json!({"pattern": "x"}), &ctx(dir.path()));

        let Access::Path {
            canonical,
            relative,
            ..
        } = prepared.access
        else {
            panic!("expected a path target");
        };
        assert_eq!(canonical, dir.path().to_str().unwrap());
        assert_eq!(relative.as_deref(), Some(""));
    }

    // ── run ──

    #[tokio::test]
    async fn run_matching_pattern_reports_matches() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.rs"),
            indoc! {"
                fn main() {}
                fn helper() {}
            "},
        )
        .unwrap();

        let input = serde_json::json!({"pattern": "fn main"});

        let output = testing::run(&GrepTool::default(), input, &ctx(dir.path())).await;

        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content, "test.rs:1:fn main() {}");
    }

    #[tokio::test]
    async fn run_missing_pattern_reports_input_error() {
        let output = testing::run(&GrepTool::default(), serde_json::json!({}), &temp_ctx()).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_invalid_regex_omits_title() {
        let input = serde_json::json!({"pattern": "(unclosed"});

        let output = testing::run(&GrepTool::default(), input, &temp_ctx()).await;
        assert!(output.is_error);
        assert!(
            output.content.contains("Invalid regex"),
            "{}",
            output.content
        );
        assert_eq!(output.metadata.title, None, "errors are not a match count");
    }

    // ── grep_title ──

    #[test]
    fn grep_title_with_context_counts_matches_only() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                a
                MATCH
                b
                c
                MATCH
                d
            "},
        )
        .unwrap();

        let mut p = params("MATCH", dir.path());
        p.context = 1;
        let result = grep_files(&p).unwrap();
        assert_eq!(grep_title(&result), "2 matches");
    }

    #[test]
    fn grep_title_summary_modes_reuse_the_found_line() {
        let count = indoc! {"
            Found 3 total occurrences across 2 files
            a.rs:2
            b.rs:1"
        };
        assert_eq!(
            grep_title(count),
            "Found 3 total occurrences across 2 files"
        );
    }

    #[test]
    fn grep_title_empty_results_say_no_matches() {
        assert_eq!(grep_title("No matches found"), "No matches found");
        assert_eq!(grep_title("No files found"), "No matches found");
    }

    // ── grep_files: content mode ──

    #[test]
    fn grep_files_plain_pattern_finds_matching_lines() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("a.rs"),
            indoc! {"
                fn foo() {}
                fn bar() {}
            "},
        )
        .unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn baz() {}\n").unwrap();

        let result = grep_files(&params("fn foo", dir.path())).unwrap();
        assert_eq!(result, "a.rs:1:fn foo() {}");
    }

    #[test]
    fn grep_files_regex_finds_matching_lines() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                hello123
                world456
                hello789
            "},
        )
        .unwrap();

        let result = grep_files(&params(r"hello\d+", dir.path())).unwrap();
        assert_eq!(result, "test.txt:1:hello123\ntest.txt:3:hello789");
    }

    #[test]
    fn grep_files_case_insensitive_matches_mixed_case() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                Hello World
                hello world
            "},
        )
        .unwrap();

        let mut p = params("hello", dir.path());
        p.case_insensitive = true;
        let result = grep_files(&p).unwrap();
        assert!(result.contains("test.txt:1:Hello World"));
        assert!(result.contains("test.txt:2:hello world"));
    }

    #[test]
    fn grep_files_with_context_includes_surrounding_lines() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                aaa
                bbb
                ccc
                ddd
                eee
            "},
        )
        .unwrap();

        let mut p = params("ccc", dir.path());
        p.context = 1;
        let result = grep_files(&p).unwrap();
        assert!(result.contains("test.txt:2-bbb"));
        assert!(result.contains("test.txt:3:ccc"));
        assert!(result.contains("test.txt:4-ddd"));
        assert!(!result.contains("aaa"));
        assert!(!result.contains("eee"));
    }

    #[test]
    fn grep_files_with_context_merges_adjacent_ranges() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                a
                b
                MATCH1
                c
                d
                MATCH2
                e
                f
            "},
        )
        .unwrap();

        let mut p = params("MATCH", dir.path());
        p.context = 1;
        let result = grep_files(&p).unwrap();
        // MATCH1 ctx → 2-4 and MATCH2 ctx → 5-7, so lines 4-5 bridge the gap and the ranges merge.
        assert!(!result.contains("--"));
        assert!(result.contains("test.txt:2-b"));
        assert!(result.contains("test.txt:3:MATCH1"));
        assert!(result.contains("test.txt:4-c"));
        assert!(result.contains("test.txt:5-d"));
        assert!(result.contains("test.txt:6:MATCH2"));
        assert!(result.contains("test.txt:7-e"));
    }

    #[test]
    fn grep_files_with_context_separates_distant_ranges() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                MATCH1
                a
                b
                c
                d
                e
                f
                MATCH2
            "},
        )
        .unwrap();

        let mut p = params("MATCH", dir.path());
        p.context = 1;
        let result = grep_files(&p).unwrap();
        // MATCH1 ctx → 1-2 and MATCH2 ctx → 7-8, so the gap forces a "--" separator.
        assert!(result.contains("--"));
        assert!(result.contains("test.txt:1:MATCH1"));
        assert!(result.contains("test.txt:2-a"));
        assert!(result.contains("test.txt:7-f"));
        assert!(result.contains("test.txt:8:MATCH2"));
        assert!(!result.contains("test.txt:3"));
        assert!(!result.contains("test.txt:4"));
        assert!(!result.contains("test.txt:5"));
        assert!(!result.contains("test.txt:6"));
    }

    #[test]
    fn grep_files_with_context_no_matches_in_file() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                aaa
                bbb
                ccc
            "},
        )
        .unwrap();

        let mut p = params("xyz", dir.path());
        p.context = 1;
        let result = grep_files(&p).unwrap();
        assert_eq!(result, "No matches found");
    }

    #[test]
    fn grep_files_with_context_respects_head_limit() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                before1
                MATCH
                after1
                gap
                before2
                MATCH
                after2
            "},
        )
        .unwrap();

        let mut p = params("MATCH", dir.path());
        p.context = 1;
        p.head_limit = Some(3);
        let result = grep_files(&p).unwrap();
        assert!(result.contains("Results limited to 3 lines"));
        let entry_count = result.lines().filter(|l| l.contains("test.txt:")).count();
        assert_eq!(entry_count, 3);
    }

    #[test]
    fn grep_files_with_include_filter() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("code.rs"), "fn test() {}\n").unwrap();
        std::fs::write(dir.path().join("readme.md"), "fn test()\n").unwrap();

        let mut p = params("fn test", dir.path());
        p.include_glob = Some("*.rs");
        let result = grep_files(&p).unwrap();
        assert_eq!(result, "code.rs:1:fn test() {}");
    }

    #[test]
    fn grep_files_single_file_finds_matching_lines() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                alpha
                beta
                gamma
            "},
        )
        .unwrap();

        let mut p = params("beta", dir.path());
        p.root = search_root(dir.path(), Some("test.txt")).unwrap();
        let result = grep_files(&p).unwrap();
        assert_eq!(result, "test.txt:2:beta");
    }

    #[test]
    fn grep_files_renders_paths_relative_to_cwd() {
        let dir = canonical_tempdir();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::write(cwd.join("src/lib.rs"), "hit\n").unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "hit\n").unwrap();

        let mut p = params("hit", &cwd);
        p.root = search_root(&cwd, Some("src")).unwrap();
        assert_eq!(grep_files(&p).unwrap(), "src/lib.rs:1:hit");

        p.root = search_root(&cwd, Some("src/lib.rs")).unwrap();
        assert_eq!(grep_files(&p).unwrap(), "src/lib.rs:1:hit");

        p.root = search_root(&cwd, Some("../outside.txt")).unwrap();
        assert_eq!(
            grep_files(&p).unwrap(),
            format!("{}:1:hit", slash_separated(&outside)),
            "hits outside cwd fall back to an absolute path",
        );
    }

    #[test]
    fn grep_files_skips_binary() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("binary.bin"), b"match\x00here").unwrap();
        std::fs::write(dir.path().join("text.txt"), "match here\n").unwrap();

        let result = grep_files(&params("match", dir.path())).unwrap();
        assert_eq!(result, "text.txt:1:match here");
    }

    #[test]
    fn grep_files_includes_hidden_paths_but_skips_vcs_dirs() {
        let dir = canonical_tempdir();
        let cargo = dir.path().join(".cargo");
        std::fs::create_dir(&cargo).unwrap();
        std::fs::write(cargo.join("config.toml"), "match me\n").unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::write(git.join("config"), "match me\n").unwrap();

        let result = grep_files(&params("match", dir.path())).unwrap();
        assert_eq!(result, ".cargo/config.toml:1:match me");
    }

    #[test]
    fn grep_files_respects_gitignore() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "match me\n").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "match me\n").unwrap();

        let result = grep_files(&params("match", dir.path())).unwrap();
        assert_eq!(result, "tracked.txt:1:match me");
    }

    #[test]
    fn grep_files_warns_about_skipped_large_files() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("small.txt"), "match here\n").unwrap();
        let large = dir.path().join("large.txt");
        let f = std::fs::File::create(&large).unwrap();
        f.set_len(MAX_GREP_FILE_SIZE + 1).unwrap();

        let result = grep_files(&params("match", dir.path())).unwrap();

        assert_eq!(
            result,
            indoc! {"
                small.txt:1:match here

                Skipped (exceeds 1 MB size limit):
                  large.txt (1.0 MB)"
            }
        );
    }

    #[test]
    fn grep_files_lists_what_it_could_not_read_after_the_matches() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("a.rs"), "hit\n").unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("b.rs"), "hit\n").unwrap();
        let secret = dir.path().join("secret.rs");
        std::fs::write(&secret, "hit\n").unwrap();
        let _denied = [
            testing::deny_read_access(&locked),
            testing::deny_read_access(&secret),
        ];

        let result = grep_files(&params("hit", dir.path())).unwrap();

        let dir_cause = std::fs::read_dir(&locked).unwrap_err();
        let file_cause = std::fs::read(&secret).unwrap_err();
        assert_eq!(
            result,
            formatdoc! {"
                a.rs:1:hit

                Could not read:
                  locked: {dir_cause}
                  secret.rs: {file_cause}"
            }
        );
        assert_eq!(
            grep_title(crate::tool::strip_search_notes(&result)),
            "1 match"
        );
        let view = GrepTool::default().result_view(
            &serde_json::json!({"pattern": "hit"}),
            &result,
            &ToolMetadata::default(),
            Path::new("/repo"),
        );
        assert_eq!(view, None);
    }

    #[test]
    fn grep_files_withholds_an_unreadable_directory_a_read_deny_rule_matches() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("a.rs"), "hit\n").unwrap();
        let secrets = dir.path().join("secrets");
        std::fs::create_dir(&secrets).unwrap();
        let _denied = testing::deny_read_access(&secrets);
        let mut p = params("hit", dir.path());
        p.read_filter = deny_reads(&["secrets"]);

        let result = grep_files(&p).unwrap();

        assert_eq!(
            result,
            indoc! {"
                a.rs:1:hit

                (1 file withheld by read deny or ask rules)"
            }
        );
    }

    #[test]
    fn grep_files_without_matches_still_lists_what_it_could_not_read() {
        let dir = canonical_tempdir();
        let secret = dir.path().join("secret.rs");
        std::fs::write(&secret, "hit\n").unwrap();
        let _denied = testing::deny_read_access(&secret);

        let result = grep_files(&params("hit", dir.path())).unwrap();

        let cause = std::fs::read(&secret).unwrap_err();
        assert_eq!(
            result,
            formatdoc! {"
                No matches found

                Could not read:
                  secret.rs: {cause}"
            }
        );
    }

    // ── texts ──

    #[test]
    fn texts_records_a_failed_read_but_skips_a_file_that_vanished() {
        let dir = canonical_tempdir();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "x").unwrap();
        let _denied = testing::deny_read_access(&secret);
        let files = [dir.path().join("gone.txt"), secret.clone()];
        let cancel = CancellationToken::new();
        let mut unreadable = Vec::new();

        let read: Vec<_> = texts(&files, &cancel, &mut unreadable).collect();

        let cause = std::fs::read(&secret).unwrap_err().to_string();
        assert_eq!(read, []);
        assert_eq!(
            unreadable,
            [Unreadable {
                path: Some(secret),
                error: cause,
            }]
        );
    }

    #[test]
    fn grep_files_withholds_files_a_read_deny_rule_matches() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("a.rs"), "TOKEN=public\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.env"), "TOKEN=secret\n").unwrap();
        let mut p = params("TOKEN", dir.path());
        p.read_filter = deny_reads(&[".env"]);
        assert_eq!(
            grep_files(&p).unwrap(),
            indoc! {"
                a.rs:1:TOKEN=public

                (1 file withheld by read deny or ask rules)"
            }
        );

        p.pattern = "secret";
        let result = grep_files(&p).unwrap();
        assert_eq!(
            result,
            indoc! {"
                No matches found

                (1 file withheld by read deny or ask rules)"
            }
        );
        let title = grep_title(crate::tool::strip_search_notes(&result));
        assert_eq!(title, "No matches found");
        // The structured view would drop the note, so the text view shows it.
        let input = serde_json::json!({"pattern": "secret"});
        let view = GrepTool::default().result_view(
            &input,
            &result,
            &ToolMetadata::default(),
            Path::new("/repo"),
        );
        assert_eq!(view, None);
    }

    #[cfg(unix)]
    #[test]
    fn grep_files_withholds_a_denied_file_below_a_symlinked_root() {
        let dir = canonical_tempdir();
        let (cwd, outside) = (dir.path().join("cwd"), dir.path().join("outside"));
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir_all(outside.join("secrets")).unwrap();
        std::fs::write(outside.join("secrets/db.key"), "TOKEN=secret\n").unwrap();
        std::fs::write(outside.join("app.toml"), "TOKEN=public\n").unwrap();
        std::os::unix::fs::symlink(&outside, cwd.join("config")).unwrap();
        let mut p = params("TOKEN", &cwd);
        p.root = search_root(&cwd, Some("config")).unwrap();
        p.read_filter = deny_reads(&["secrets"]);
        let app = outside.join("app.toml").display().to_string();
        assert_eq!(
            grep_files(&p).unwrap(),
            formatdoc! {"
                {app}:1:TOKEN=public

                (1 file withheld by read deny or ask rules)"
            }
        );
    }

    #[test]
    fn grep_files_unmatched_pattern_reports_no_matches() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("test.txt"), "hello world\n").unwrap();

        let result = grep_files(&params("nonexistent", dir.path())).unwrap();
        assert_eq!(result, "No matches found");
    }

    #[test]
    fn grep_files_invalid_regex_reports_error() {
        let err = grep_files(&params("[invalid", Path::new("/"))).unwrap_err();
        assert!(err.contains("Invalid regex"));
    }

    #[test]
    fn grep_files_invalid_include_pattern() {
        let dir = canonical_tempdir();
        let mut p = params("test", dir.path());
        p.include_glob = Some("[bad");
        let err = grep_files(&p).unwrap_err();
        assert!(err.contains("Invalid include pattern"));
    }

    #[test]
    fn grep_files_single_file_too_large() {
        let dir = canonical_tempdir();
        let large = dir.path().join("large.txt");
        let f = std::fs::File::create(&large).unwrap();
        f.set_len(MAX_GREP_FILE_SIZE + 1).unwrap();

        let mut p = params("match", dir.path());
        p.root = search_root(dir.path(), Some("large.txt")).unwrap();
        let result = grep_files(&p).unwrap();
        assert!(result.contains("Skipped"));
        assert!(result.contains("large.txt"));
    }

    // ── grep_files: head_limit ──

    #[test]
    fn grep_files_head_limit_truncates_matching_lines() {
        let dir = canonical_tempdir();
        let content = (0..20).fold(String::new(), |mut s, i| {
            _ = writeln!(s, "match line {i}");
            s
        });
        std::fs::write(dir.path().join("test.txt"), &content).unwrap();

        let mut p = params("match", dir.path());
        p.head_limit = Some(5);
        let result = grep_files(&p).unwrap();
        let lines: Vec<&str> = result.lines().collect();
        // 5 match lines + blank + truncation notice
        assert!(result.contains("Results limited to 5 lines"));
        assert!(lines.len() <= 8);
    }

    #[test]
    fn grep_files_head_limit_exactly_reached_is_not_flagged() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("test.txt"), "match\nmatch\nmatch\n").unwrap();

        let mut p = params("match", dir.path());
        p.head_limit = Some(3);
        let result = grep_files(&p).unwrap();
        assert_eq!(
            result,
            indoc! {"
                test.txt:1:match
                test.txt:2:match
                test.txt:3:match"
            },
            "no further match was dropped, so no limit footer",
        );
    }

    #[test]
    fn grep_files_head_limit_zero_means_unlimited() {
        let dir = canonical_tempdir();
        let content = (0..DEFAULT_HEAD_LIMIT + 10).fold(String::new(), |mut s, i| {
            _ = writeln!(s, "match line {i}");
            s
        });
        std::fs::write(dir.path().join("test.txt"), &content).unwrap();

        let mut p = params("match", dir.path());
        p.head_limit = Some(0);
        let result = grep_files(&p).unwrap();
        assert!(!result.contains("Results limited"));
        assert!(result.contains(&format!("test.txt:{}:", DEFAULT_HEAD_LIMIT + 1)));
    }

    #[test]
    fn grep_files_head_limit_across_multiple_files() {
        let dir = canonical_tempdir();
        for name in ["a.txt", "b.txt"] {
            std::fs::write(dir.path().join(name), "match\nmatch\nmatch\n").unwrap();
        }

        let mut p = params("match", dir.path());
        p.head_limit = Some(3);
        let result = grep_files(&p).unwrap();
        assert!(result.contains("Results limited to 3 lines"));
        let match_lines: Vec<_> = result.lines().filter(|l| l.contains(":match")).collect();
        assert_eq!(match_lines.len(), 3);
    }

    // ── grep_files: files_with_matches mode ──

    #[test]
    fn grep_files_files_with_matches_mode_lists_matching_paths() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("a.rs"), "fn foo() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn bar() {}\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "no match\n").unwrap();

        let mut p = params("fn", dir.path());
        p.output_mode = OutputMode::FilesWithMatches;
        let result = grep_files(&p).unwrap();
        assert!(result.starts_with("Found 2 files\n"));
        assert!(result.lines().any(|l| l == "a.rs"));
        assert!(result.lines().any(|l| l == "b.rs"));
        assert!(!result.contains("c.txt"));
    }

    #[test]
    fn grep_files_files_with_matches_truncated() {
        let dir = canonical_tempdir();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("{i}.txt")), "match\n").unwrap();
        }

        let mut p = params("match", dir.path());
        p.output_mode = OutputMode::FilesWithMatches;
        p.head_limit = Some(2);
        let result = grep_files(&p).unwrap();
        assert!(result.starts_with("Found 2 files\n"));
        assert!(result.contains("Results limited to 2 files"));
        let file_count = result.lines().filter(|l| l.contains(".txt")).count();
        assert_eq!(file_count, 2);
    }

    #[test]
    fn grep_files_files_with_matches_exactly_at_limit_is_not_flagged() {
        let dir = canonical_tempdir();
        for i in 0..2 {
            std::fs::write(dir.path().join(format!("{i}.txt")), "match\n").unwrap();
        }

        let mut p = params("match", dir.path());
        p.output_mode = OutputMode::FilesWithMatches;
        p.head_limit = Some(2);
        let result = grep_files(&p).unwrap();
        assert!(result.starts_with("Found 2 files\n"));
        assert!(
            !result.contains("Results limited"),
            "no further file was dropped: {result:?}",
        );
    }

    #[test]
    fn grep_files_files_with_matches_no_matches() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("a.rs"), "fn foo() {}\n").unwrap();

        let mut p = params("nonexistent", dir.path());
        p.output_mode = OutputMode::FilesWithMatches;
        let result = grep_files(&p).unwrap();
        assert_eq!(result, "No files found");
    }

    // ── grep_files: count mode ──

    #[test]
    fn grep_files_count_mode_reports_occurrences() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                aaa
                bbb
                aaa
            "},
        )
        .unwrap();

        let mut p = params("aaa", dir.path());
        p.output_mode = OutputMode::Count;
        let result = grep_files(&p).unwrap();
        assert!(result.contains("\ntest.txt:2"));
        assert!(result.contains("2 total occurrences"));
    }

    #[test]
    fn grep_files_count_mode_truncated() {
        let dir = canonical_tempdir();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("{i}.txt")), "match\n").unwrap();
        }

        let mut p = params("match", dir.path());
        p.output_mode = OutputMode::Count;
        p.head_limit = Some(2);
        let result = grep_files(&p).unwrap();
        // The summary counts all 5 files even though `head_limit` caps the listing at 2.
        assert!(result.contains("5 total occurrences across 5 files"));
        assert!(result.contains("Results limited to 2 files"));
        let file_lines: Vec<_> = result.lines().filter(|l| l.ends_with(":1")).collect();
        assert_eq!(file_lines.len(), 2);
    }

    #[test]
    fn grep_files_count_mode_singular() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("a.txt"), "match once\n").unwrap();

        let mut p = params("match", dir.path());
        p.output_mode = OutputMode::Count;
        let result = grep_files(&p).unwrap();
        assert!(result.contains("1 total occurrence across 1 file"));
        assert!(!result.contains("occurrences"));
        assert!(!result.contains(" files"));
    }

    #[test]
    fn grep_files_count_mode_summary_first() {
        // Pin summary as the first line so the renderer's title-strip removes it.
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("a.txt"), "match\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "match\n").unwrap();

        let mut p = params("match", dir.path());
        p.output_mode = OutputMode::Count;
        let result = grep_files(&p).unwrap();
        assert!(
            result.starts_with("Found 2 total occurrences across 2 files\n"),
            "summary must lead the body: {result:?}",
        );
    }

    #[test]
    fn grep_files_count_mode_without_matches_reports_no_matches() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("test.txt"), "other\n").unwrap();

        let mut p = params("absent", dir.path());
        p.output_mode = OutputMode::Count;
        assert_eq!(grep_files(&p).unwrap(), "No matches found");
    }

    // ── result_view ──

    #[test]
    fn result_view_content_mode_groups_by_file() {
        let expected = ToolResultView::GrepMatches {
            groups: vec![GrepFileGroup {
                path: "src/main.rs".to_owned(),
                lines: vec![GrepMatchLine {
                    number: 10,
                    text: "fn main()".to_owned(),
                    is_match: true,
                }],
            }],
            truncated: false,
        };

        for input in [
            serde_json::json!({"pattern": "fn"}),
            serde_json::json!({"pattern": "fn", "output_mode": "content"}),
        ] {
            let view = GrepTool::default().result_view(
                &input,
                "src/main.rs:10:fn main()",
                &ToolMetadata::default(),
                Path::new("/repo"),
            );

            assert_eq!(view.as_ref(), Some(&expected), "{input}");
        }
    }

    #[test]
    fn result_view_non_content_mode_falls_back_to_text() {
        for mode in ["files_with_matches", "count"] {
            let input = serde_json::json!({"pattern": "fn", "output_mode": mode});
            let view = GrepTool::default().result_view(
                &input,
                "irrelevant",
                &ToolMetadata::default(),
                Path::new("/repo"),
            );
            assert!(view.is_none(), "mode {mode}: expected None to fall back");
        }
    }

    // ── parse_content_view ──

    #[test]
    fn parse_content_view_no_matches_is_empty() {
        assert_eq!(
            parse_content_view("No matches found"),
            Some(ToolResultView::GrepMatches {
                groups: Vec::new(),
                truncated: false,
            }),
        );
    }

    #[test]
    fn parse_content_view_groups_consecutive_lines_with_context_and_separator() {
        let content = indoc! {r#"
            src/main.rs:10:fn main() {
            src/main.rs:11-    println!("hi");
            --
            src/main.rs:20:    helper();
            src/lib.rs:5:fn other()
        "#};
        assert_eq!(
            parse_content_view(content),
            Some(ToolResultView::GrepMatches {
                groups: vec![
                    GrepFileGroup {
                        path: "src/main.rs".to_owned(),
                        lines: vec![
                            GrepMatchLine {
                                number: 10,
                                text: "fn main() {".to_owned(),
                                is_match: true,
                            },
                            GrepMatchLine {
                                number: 11,
                                text: r#"    println!("hi");"#.to_owned(),
                                is_match: false,
                            },
                            GrepMatchLine {
                                number: 20,
                                text: "    helper();".to_owned(),
                                is_match: true,
                            },
                        ],
                    },
                    GrepFileGroup {
                        path: "src/lib.rs".to_owned(),
                        lines: vec![GrepMatchLine {
                            number: 5,
                            text: "fn other()".to_owned(),
                            is_match: true,
                        }],
                    },
                ],
                truncated: false,
            }),
        );
    }

    #[test]
    fn parse_content_view_truncation_footer_sets_truncated_flag() {
        let content = indoc! {"
            src/main.rs:1:hit

            (Results limited to 1 lines)"
        };
        assert_eq!(
            parse_content_view(content),
            Some(ToolResultView::GrepMatches {
                groups: vec![GrepFileGroup {
                    path: "src/main.rs".to_owned(),
                    lines: vec![GrepMatchLine {
                        number: 1,
                        text: "hit".to_owned(),
                        is_match: true,
                    }],
                }],
                truncated: true,
            }),
        );
    }

    #[test]
    fn parse_content_view_falls_back_when_skipped_warnings_present() {
        // GrepMatches can't hold the skipped-file warning, so the view falls back to show it.
        for content in [
            indoc! {"
                src/main.rs:1:hit

                Skipped (exceeds 1 MB size limit):
                  big.txt (5.0 MB)"
            },
            indoc! {"
                No matches found

                Skipped (exceeds 1 MB size limit):
                  big.txt (5.0 MB)"
            },
        ] {
            assert_eq!(parse_content_view(content), None, "{content}");
        }
    }

    #[test]
    fn parse_content_view_falls_back_on_invalid_line() {
        assert!(parse_content_view("src/main.rs:1:hit\nunexpected line").is_none());
    }

    // ── parse_match_line ──

    #[test]
    fn parse_match_line_distinguishes_match_from_context_separator() {
        let (path, m) = parse_match_line("src/main.rs:10:body").unwrap();
        assert_eq!(path, "src/main.rs");
        assert_eq!(m.number, 10);
        assert_eq!(m.text, "body");
        assert!(m.is_match);

        let (path, c) = parse_match_line("src/main.rs:11-context").unwrap();
        assert_eq!(path, "src/main.rs");
        assert_eq!(c.number, 11);
        assert_eq!(c.text, "context");
        assert!(!c.is_match);
    }

    #[test]
    fn parse_match_line_skips_path_internal_colons_without_digit_separator() {
        let (path, m) = parse_match_line("C:foo:42:body").unwrap();
        assert_eq!(path, "C:foo");
        assert_eq!(m.number, 42);
        assert_eq!(m.text, "body");
        assert!(m.is_match);
    }

    #[test]
    fn parse_match_line_is_none_when_shape_does_not_match() {
        assert!(parse_match_line("not a match line").is_none());
        assert!(parse_match_line("path:non_digits:text").is_none());
        assert!(parse_match_line("path:42").is_none());
    }
}
