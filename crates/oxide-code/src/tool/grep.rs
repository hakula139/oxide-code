//! Regex content search tool.

use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::Deserialize;

use super::{
    GrepFileGroup, GrepMatchLine, Tool, ToolMetadata, ToolOutput, ToolResultView,
    extract_input_field,
};
use crate::permission::{PermissionClass, ReadDeny, WalkDeny};

const DEFAULT_HEAD_LIMIT: usize = 250;
const MAX_LINE_CHARS: usize = 500;
const MAX_GREP_FILE_SIZE: u64 = 1024 * 1024;

/// Withholds each file a `read` deny rule matches from the results.
#[cfg_attr(test, derive(Default))]
pub(crate) struct GrepTool {
    read_deny: ReadDeny,
}

impl GrepTool {
    pub(crate) fn new(read_deny: ReadDeny) -> Self {
        Self { read_deny }
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents using a regular expression."
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Read
    }

    fn gate_target(
        &self,
        input: &serde_json::Value,
        cwd: &std::path::Path,
    ) -> crate::permission::GateTarget {
        // The gated target is the search root (the `path` arg, defaulting to cwd), so a path-scoped
        // deny can block searching under a protected directory.
        let path = extract_input_field(input, "path").unwrap_or(".");
        crate::permission::GateTarget::for_path(path, cwd)
    }

    fn input_schema(&self) -> serde_json::Value {
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
                    "description": r#"Glob pattern to filter by filename (e.g. "*.rs", "*.{ts,tsx}"). Matches the basename only, not the full path."#
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": r#"Output mode: "content" shows matching lines (default), "files_with_matches" shows file paths only, "count" shows match counts per file"#
                },
                "context": {
                    "type": "integer",
                    "description": "Number of context lines to show before and after each match (content mode only)"
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
    ) -> Option<ToolResultView> {
        // Only content mode produces line-numbered rows, so the other modes fall through and keep
        // their summary header visible.
        let mode = input
            .get("output_mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("content");
        if mode != "content" {
            return None;
        }
        parse_content_view(content)
    }

    fn run(
        &self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(run(input, self.read_deny.clone()))
    }
}

// ── Input ──

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
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    output_mode: OutputMode,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    head_limit: Option<usize>,
}

// ── Execution ──

async fn run(raw: serde_json::Value, read_deny: ReadDeny) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(e) => return e,
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

    match tokio::task::spawn_blocking(move || {
        let cwd = super::working_dir()?;
        grep_files(&GrepParams {
            pattern: &pattern,
            cwd: &cwd,
            search_path: path.as_deref(),
            include_glob: include.as_deref(),
            output_mode,
            context: context.unwrap_or(0),
            case_insensitive,
            head_limit,
            read_deny,
        })
    })
    .await
    {
        Ok(Ok(content)) => {
            let title = grep_title(super::strip_withheld_note(&content));
            ToolOutput::success(content).with_title(title)
        }
        Ok(Err(msg)) => ToolOutput::error(msg),
        Err(e) => ToolOutput::error(format!("Internal error: {e}")),
    }
}

fn grep_title(output: &str) -> String {
    if matches!(output, "No matches found" | "No files found") {
        return "No matches found".into();
    }
    // Count / files-with-matches modes lead with "Found N ...".
    if let Some(line) = output.lines().find(|l| l.starts_with("Found ")) {
        return line.trim_end_matches('.').to_owned();
    }
    let match_count = output
        .lines()
        .filter_map(parse_match_line)
        .filter(|(_, m)| m.is_match)
        .count();
    let word = if match_count == 1 { "match" } else { "matches" };
    format!("{match_count} {word}")
}

// ── Search ──

struct GrepParams<'a> {
    pattern: &'a str,
    /// Canonical working directory. A relative `search_path` resolves against it, and result
    /// paths render relative to it.
    cwd: &'a Path,
    search_path: Option<&'a str>,
    include_glob: Option<&'a str>,
    output_mode: OutputMode,
    context: usize,
    case_insensitive: bool,
    head_limit: Option<usize>,
    read_deny: ReadDeny,
}

fn grep_files(params: &GrepParams<'_>) -> Result<String, String> {
    // Patterns are line-oriented: the regex matches against each line individually, so `^` / `$`
    // anchor to line boundaries without needing the multi-line flag. Compiled-program and DFA
    // size caps keep adversarial patterns (e.g., catastrophic alternations) from OOMing.
    let re = regex::RegexBuilder::new(params.pattern)
        .case_insensitive(params.case_insensitive)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .map_err(|e| format!("Invalid regex: {e}"))?;

    let base = super::resolve_base_dir(params.search_path, params.cwd)?;

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

    let read_deny = params.read_deny.walk(
        &base,
        Path::new(params.search_path.unwrap_or(".")),
        params.cwd,
    );
    let collected = collect_files(&base, include_matcher.as_ref(), &read_deny);

    let cwd = params.cwd;
    let mut result = match params.output_mode {
        OutputMode::FilesWithMatches => {
            format_files_with_matches(&collected.files, cwd, &re, head_limit)
        }
        OutputMode::Count => format_count(&collected.files, cwd, &re, head_limit),
        OutputMode::Content => {
            format_content(&collected.files, cwd, &re, params.context, head_limit)
        }
    };

    append_skipped_warnings(&mut result, &collected.skipped_large, cwd);
    super::push_withheld_note(&mut result, collected.withheld);

    Ok(result)
}

struct CollectedFiles {
    files: Vec<PathBuf>,
    skipped_large: Vec<(PathBuf, u64)>,
    /// Files a `read` deny rule withheld.
    withheld: usize,
}

/// The files to search under `base`. A file `base` itself passed the gate, so only the files a walk
/// finds face `read_deny`.
fn collect_files(
    base: &Path,
    include_matcher: Option<&globset::GlobMatcher>,
    read_deny: &WalkDeny<'_>,
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
            withheld: 0,
        };
    }

    let mut skipped_large = Vec::new();
    let mut withheld = 0;
    let mut files_with_mtime: Vec<(PathBuf, std::time::SystemTime)> = super::walk_files(base)
        .filter(|entry| {
            include_matcher.is_none_or(|m| {
                let name = entry.file_name().to_string_lossy();
                m.is_match(name.as_ref())
            })
        })
        .filter(|entry| {
            let denied = read_deny.denies(entry.path());
            withheld += usize::from(denied);
            !denied
        })
        .filter(|entry| {
            if let Ok(meta) = entry.metadata()
                && meta.len() > MAX_GREP_FILE_SIZE
            {
                skipped_large.push((entry.path().to_path_buf(), meta.len()));
                return false;
            }
            true
        })
        .map(|entry| {
            let mtime = super::entry_mtime(&entry);
            (entry.into_path(), mtime)
        })
        .collect();

    files_with_mtime.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    CollectedFiles {
        files: files_with_mtime.into_iter().map(|(p, _)| p).collect(),
        skipped_large,
        withheld,
    }
}

fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if super::is_binary(&bytes) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn append_skipped_warnings(output: &mut String, skipped: &[(PathBuf, u64)], cwd: &Path) {
    if skipped.is_empty() {
        return;
    }
    let limit_mb = MAX_GREP_FILE_SIZE / (1024 * 1024);
    _ = write!(output, "\n\nSkipped (exceeds {limit_mb} MB size limit):");
    for (path, size) in skipped {
        let mb = super::bytes_to_mb(*size);
        let display = super::display_path(path, cwd);
        _ = write!(output, "\n  {display} ({mb:.1} MB)");
    }
}

// ── Content Mode ──

fn format_content(
    files: &[PathBuf],
    cwd: &Path,
    re: &regex::Regex,
    context: usize,
    head_limit: usize,
) -> String {
    // Collecting one row past the limit proves more output exists, so the footer only appears when
    // a row was actually dropped.
    let capacity = head_limit.saturating_add(1);
    let mut output_lines: Vec<String> = Vec::new();

    for path in files {
        if output_lines.len() >= capacity {
            break;
        }

        let Some(text) = read_text(path) else {
            continue;
        };

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
        return "No matches found".into();
    }

    let truncated = output_lines.len() > head_limit;
    output_lines.truncate(head_limit);
    let mut output = output_lines.join("\n");

    if truncated {
        _ = write!(output, "\n\n(Results limited to {head_limit} lines)");
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
            output_lines.push("--".into());
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

fn format_files_with_matches(
    files: &[PathBuf],
    cwd: &Path,
    re: &regex::Regex,
    head_limit: usize,
) -> String {
    let mut matching_files: Vec<String> = Vec::new();

    for path in files {
        if matching_files.len() > head_limit {
            break;
        }

        let Some(text) = read_text(path) else {
            continue;
        };

        if text.lines().any(|line| re.is_match(line)) {
            matching_files.push(super::display_path(path, cwd));
        }
    }

    if matching_files.is_empty() {
        return "No files found".into();
    }

    let truncated = matching_files.len() > head_limit;
    matching_files.truncate(head_limit);
    let num_files = matching_files.len();
    let file_word = if num_files == 1 { "file" } else { "files" };

    let mut output = format!("Found {num_files} {file_word}\n");
    output.push_str(&matching_files.join("\n"));

    if truncated {
        _ = write!(output, "\n\n(Results limited to {head_limit} files)");
    }

    output
}

// ── Count Mode ──

fn format_count(files: &[PathBuf], cwd: &Path, re: &regex::Regex, head_limit: usize) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut total_matches: usize = 0;

    for path in files {
        let Some(text) = read_text(path) else {
            continue;
        };

        let count = text.lines().filter(|line| re.is_match(line)).count();
        if count > 0 {
            total_matches += count;
            counts.push((super::display_path(path, cwd), count));
        }
    }

    if counts.is_empty() {
        return "No matches found".into();
    }

    let total_files = counts.len();
    let truncated = total_files > head_limit;
    counts.truncate(head_limit);

    let mut output = format!(
        "Found {total_matches} total {} across {total_files} {}",
        if total_matches == 1 {
            "occurrence"
        } else {
            "occurrences"
        },
        if total_files == 1 { "file" } else { "files" },
    );
    for (p, c) in &counts {
        _ = write!(output, "\n{p}:{c}");
    }
    if truncated {
        _ = write!(output, "\n\n(Results limited to {head_limit} files)");
    }

    output
}

// ── Result View ──

/// Returns `None` for any unrecognised line so the block falls through to text.
fn parse_content_view(content: &str) -> Option<ToolResultView> {
    if content.contains("\n\nSkipped (exceeds ") {
        return None;
    }

    let trimmed = content.trim_end();
    if trimmed == "No matches found" {
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
        if line.starts_with("(Results limited to ") && line.ends_with(')') {
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
    use crate::permission::{Mode, Policy};
    use crate::tool::testing::canonical_tempdir;
    use crate::util::path::slash_separated;

    /// Content-mode params searching all of `cwd`, so result paths render relative to it.
    fn params<'a>(pattern: &'a str, cwd: &'a Path) -> GrepParams<'a> {
        GrepParams {
            pattern,
            cwd,
            search_path: None,
            include_glob: None,
            output_mode: OutputMode::Content,
            context: 0,
            case_insensitive: false,
            head_limit: None,
            read_deny: ReadDeny::default(),
        }
    }

    // ── gate_target ──

    #[test]
    fn gate_target_defaults_the_search_root_to_cwd() {
        // With no `path`, the search root is cwd, so a deny scoped to the project root still applies.
        let dir = tempfile::tempdir().unwrap();
        let cwd = dunce::canonicalize(dir.path()).unwrap();

        let target = GrepTool::default().gate_target(&serde_json::json!({"pattern": "x"}), &cwd);
        let crate::permission::GateTarget::Path { relative, .. } = target else {
            panic!("expected a path target");
        };
        assert_eq!(relative.as_deref(), Some(""));
    }

    // ── run ──

    #[tokio::test]
    async fn run_finds_pattern() {
        let dir = canonical_tempdir();
        std::fs::write(
            dir.path().join("test.rs"),
            indoc! {"
                fn main() {}
                fn helper() {}
            "},
        )
        .unwrap();

        let output = GrepTool::default()
            .run(serde_json::json!({
                "pattern": "fn main",
                "path": dir.path().to_str().unwrap()
            }))
            .await;

        assert!(!output.is_error);
        assert!(output.content.contains("test.rs:1:fn main()"));
    }

    #[tokio::test]
    async fn run_missing_pattern() {
        let output = GrepTool::default().run(serde_json::json!({})).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_invalid_regex_omits_title() {
        let output = GrepTool::default()
            .run(serde_json::json!({"pattern": "(unclosed"}))
            .await;
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
        // Context lines share the `path:NUM` prefix but must not be counted.
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
        let count = "Found 3 total occurrences across 2 files\na.rs:2\nb.rs:1";
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

    // ── grep_files (content mode) ──

    #[test]
    fn grep_files_basic() {
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
    fn grep_files_regex() {
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
    fn grep_files_case_insensitive() {
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
    fn grep_files_with_context() {
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
        // Context line uses `-` separator, match uses `:`.
        assert!(result.contains("test.txt:2-bbb"));
        assert!(result.contains("test.txt:3:ccc"));
        assert!(result.contains("test.txt:4-ddd"));
        // Lines outside context range should not appear.
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
    fn grep_files_single_file() {
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
        p.search_path = Some("test.txt");
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
        p.search_path = Some("src");
        assert_eq!(grep_files(&p).unwrap(), "src/lib.rs:1:hit");

        p.search_path = Some("src/lib.rs");
        assert_eq!(grep_files(&p).unwrap(), "src/lib.rs:1:hit");

        p.search_path = Some("../outside.txt");
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
        assert!(result.contains("small.txt:1:match here"));
        assert!(result.contains("Skipped"));
        assert!(result.contains("\n  large.txt ("));
    }

    #[test]
    fn grep_files_withholds_files_a_read_deny_rule_matches() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("a.rs"), "TOKEN=public\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.env"), "TOKEN=secret\n").unwrap();
        let deny = ["read(**/.env)".to_owned()];
        let policy = Policy::resolve(Mode::Auto, &[], &deny, dir.path()).unwrap();

        let mut p = params("TOKEN", dir.path());
        p.read_deny = policy.read_deny();
        assert_eq!(
            grep_files(&p).unwrap(),
            "a.rs:1:TOKEN=public\n\n(1 file withheld by read deny rules)"
        );

        p.pattern = "secret";
        let result = grep_files(&p).unwrap();
        assert_eq!(
            result,
            "No matches found\n\n(1 file withheld by read deny rules)"
        );
        let title = grep_title(crate::tool::strip_withheld_note(&result));
        assert_eq!(title, "No matches found");
        // The structured view would drop the note, so the text view shows it.
        let input = serde_json::json!({"pattern": "secret"});
        let view = GrepTool::default().result_view(&input, &result, &ToolMetadata::default());
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
        let deny = ["read(**/secrets/**)".to_owned()];
        let policy = Policy::resolve(Mode::Auto, &[], &deny, &cwd).unwrap();

        let mut p = params("TOKEN", &cwd);
        p.search_path = Some("config");
        p.read_deny = policy.read_deny();
        assert_eq!(
            grep_files(&p).unwrap(),
            format!(
                "{}:1:TOKEN=public\n\n(1 file withheld by read deny rules)",
                outside.join("app.toml").display()
            )
        );
    }

    #[test]
    fn grep_files_no_matches() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("test.txt"), "hello world\n").unwrap();

        let result = grep_files(&params("nonexistent", dir.path())).unwrap();
        assert_eq!(result, "No matches found");
    }

    #[test]
    fn grep_files_invalid_regex() {
        let err = grep_files(&params("[invalid", Path::new("/"))).unwrap_err();
        assert!(err.contains("Invalid regex"));
    }

    #[test]
    fn grep_files_nonexistent_path() {
        let dir = canonical_tempdir();
        let mut p = params("test", dir.path());
        p.search_path = Some("missing");
        let err = grep_files(&p).unwrap_err();
        assert!(err.starts_with("Cannot access missing: "), "{err}");
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
        p.search_path = Some("large.txt");
        let result = grep_files(&p).unwrap();
        assert!(result.contains("Skipped"));
        assert!(result.contains("large.txt"));
    }

    // ── grep_files (head_limit) ──

    #[test]
    fn grep_files_head_limit() {
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
            result, "test.txt:1:match\ntest.txt:2:match\ntest.txt:3:match",
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

    // ── grep_files (files_with_matches mode) ──

    #[test]
    fn grep_files_files_with_matches_mode() {
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

    // ── grep_files (count mode) ──

    #[test]
    fn grep_files_count_mode() {
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
        // Summary should report all 5 files, not just the 2 shown.
        assert!(result.contains("5 total occurrences across 5 files"));
        assert!(result.contains("Results limited to 2 files"));
        // Only 2 file lines shown.
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

    // ── result_view ──

    #[test]
    fn result_view_content_mode_groups_by_file() {
        let input = serde_json::json!({"pattern": "fn"});
        let view = GrepTool::default()
            .result_view(&input, "src/main.rs:10:fn main()", &ToolMetadata::default())
            .unwrap();
        assert_eq!(
            view,
            ToolResultView::GrepMatches {
                groups: vec![GrepFileGroup {
                    path: "src/main.rs".to_owned(),
                    lines: vec![GrepMatchLine {
                        number: 10,
                        text: "fn main()".to_owned(),
                        is_match: true,
                    }],
                }],
                truncated: false,
            },
        );
    }

    #[test]
    fn result_view_non_content_mode_falls_back_to_text() {
        // files_with_matches / count have a summary header that GrepMatches doesn't model, so the
        // text fallback keeps the "Found N ..." line visible.
        for mode in ["files_with_matches", "count"] {
            let input = serde_json::json!({"pattern": "fn", "output_mode": mode});
            let view =
                GrepTool::default().result_view(&input, "irrelevant", &ToolMetadata::default());
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
        // Groups by path, splits match (`:`) from context (`-`), accepts `--`, opens a second file.
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
        let content = indoc! {"
            src/main.rs:1:hit

            Skipped (exceeds 1 MB size limit):
              big.txt (5.0 MB)"
        };
        assert!(parse_content_view(content).is_none());
    }

    #[test]
    fn parse_content_view_falls_back_on_invalid_line() {
        // Any unrecognised row makes the whole view fall back to text, so none is silently dropped.
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
        // Path-internal `:` without digits (e.g., Windows `C:foo`) skips to the next `:NUM`.
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
