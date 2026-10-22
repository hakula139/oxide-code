//! Regex content search tool.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use indoc::formatdoc;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::search::{NO_FILES_FOUND, SearchRoot, Unreadable};
use super::spill::MAX_SPILL_BYTES;
use super::view::{GrepFileGroup, GrepMatchLine, ToolResultView};
use super::{Prepared, Tool, ToolCtx, ToolMetadata, ToolOutput, ToolSpec, extract_input_field};
use crate::file_tracker::MAX_TRACKED_FILE_SIZE;
use crate::permission::{PermissionClass, ReadFilter, WalkFilter};
use crate::util::text::{format_size, plural};

const NO_MATCHES_FOUND: &str = "No matches found";
const RESULTS_LIMIT_PREFIX: &str = "(Results limited to ";

const DEFAULT_HEAD_LIMIT: usize = 250;
const MAX_LINE_CHARS: usize = 500;
/// The largest file a directory walk searches, so a broad search skips large generated files. A
/// search rooted at one file reads it up to [`MAX_TRACKED_FILE_SIZE`], as `read` does.
const MAX_WALK_FILE_SIZE: u64 = 1024 * 1024;
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Withholds each file a `read` deny or ask rule matches from the results.
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
        super::search::prepare_search_root(input, ctx)
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
                "description": "Limit output to first N entries (default: 250, 0 for no row limit)"
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
        Ok(GrepOutput { content, title }) => ToolOutput::success(content).with_title(title),
        Err(msg) => ToolOutput::error(msg),
    }
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

/// What a search found: the text the model reads and the title the UI shows for it.
#[derive(Debug)]
struct GrepOutput {
    content: String,
    title: String,
}

fn grep_files(params: &GrepParams<'_>) -> Result<GrepOutput, String> {
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
        size_limit,
        mut unreadable,
        withheld,
    } = collect_files(
        &root.base,
        include_matcher.as_ref(),
        &read_filter,
        &params.cancel,
    );

    let cwd = &root.cwd;
    let texts = texts(&files, size_limit, &params.cancel, &mut unreadable);
    let GrepOutput { mut content, title } = match params.output_mode {
        OutputMode::FilesWithMatches => format_files_with_matches(texts, cwd, &re, head_limit),
        OutputMode::Count => format_count(texts, cwd, &re, head_limit),
        OutputMode::Content => format_content(texts, cwd, &re, params.context, head_limit),
    };

    push_skipped_note(&mut content, &skipped_large, size_limit, cwd);
    super::search::push_unreadable_note(&mut content, &unreadable, cwd);
    super::search::push_withheld_note(&mut content, withheld);

    Ok(GrepOutput { content, title })
}

struct CollectedFiles {
    files: Vec<PathBuf>,
    skipped_large: Vec<(PathBuf, u64)>,
    /// The size in bytes above which a file went to `skipped_large`.
    size_limit: u64,
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
            && meta.len() > MAX_TRACKED_FILE_SIZE
        {
            skipped_large.push((base.to_path_buf(), meta.len()));
        } else {
            files.push(base.to_path_buf());
        }
        return CollectedFiles {
            files,
            skipped_large,
            size_limit: MAX_TRACKED_FILE_SIZE,
            unreadable: Vec::new(),
            withheld: 0,
        };
    }

    let mut skipped_large = Vec::new();
    let mut unreadable = Vec::new();
    let mut withheld = 0;
    let entries = super::search::walk_files(base, cancel, &mut unreadable)
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
                && meta.len() > MAX_WALK_FILE_SIZE
            {
                skipped_large.push((entry.path().to_path_buf(), meta.len()));
                return false;
            }
            true
        });
    let files = super::search::newest_first(entries)
        .into_iter()
        .map(ignore::DirEntry::into_path)
        .collect();
    withheld += super::search::withhold_unreadable(&mut unreadable, read_filter);

    CollectedFiles {
        files,
        skipped_large,
        size_limit: MAX_WALK_FILE_SIZE,
        unreadable,
        withheld,
    }
}

/// Each of `files` that reads as text within `size_limit` bytes, with its content, until `cancel`
/// fires. Each file whose read fails goes to `unreadable`, except one that vanished after the walk
/// found it.
fn texts<'a>(
    files: &'a [PathBuf],
    size_limit: u64,
    cancel: &'a CancellationToken,
    unreadable: &'a mut Vec<Unreadable>,
) -> impl Iterator<Item = (&'a Path, String)> + 'a {
    files
        .iter()
        .take_while(|_| !cancel.is_cancelled())
        .filter_map(move |path| match read_text(path, size_limit) {
            Ok(text) => text.map(|text| (path.as_path(), text)),
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    unreadable.push(Unreadable::from_read(path, &e));
                }
                None
            }
        })
}

/// The text of the file at `path`, or `None` when it is binary. The open refuses a symlink and
/// never blocks on a fifo, and the opened file must be a regular file within `size_limit` bytes,
/// so an entry swapped after the walk checked it is not read through.
fn read_text(path: &Path, size_limit: u64) -> io::Result<Option<String>> {
    let mut options = OpenOptions::new();
    options.read(true);
    let mut file = super::file::open_no_follow_blocking(path, options)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    if metadata.len() > size_limit {
        let limit = format_size(size_limit);
        return Err(io::Error::other(format!(
            "grew past the {limit} size limit"
        )));
    }

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if super::file::is_binary(&bytes) {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

fn push_skipped_note(output: &mut String, skipped: &[(PathBuf, u64)], limit: u64, cwd: &Path) {
    let lines: Vec<String> = skipped
        .iter()
        .map(|(path, size)| {
            let (path, size) = (super::display_path(path, cwd), format_size(*size));
            format!("{path} ({size})")
        })
        .collect();
    let limit = format_size(limit);
    let header = format!("Skipped (exceeds {limit} size limit):");
    super::search::push_path_note(output, &header, &lines);
}

// ── Content Mode ──

fn format_content<'a>(
    texts: impl Iterator<Item = (&'a Path, String)>,
    cwd: &Path,
    re: &regex::Regex,
    context: usize,
    head_limit: usize,
) -> GrepOutput {
    let mut rows = Rows::new(head_limit);

    for (path, text) in texts {
        if rows.full() {
            break;
        }

        let display_path = super::display_path(path, cwd);

        if context == 0 {
            search_no_context(&text, re, &display_path, &mut rows);
        } else {
            search_with_context(&text, re, &display_path, context, &mut rows);
        }
    }

    rows.render()
}

/// The content rows a search keeps: one past `head_limit`, which proves more output exists, so the
/// footer appears only when a row was dropped, and at most about [`MAX_SPILL_BYTES`], the most
/// the dispatcher keeps of a result, so `head_limit: 0` cannot buffer every match of a large tree.
struct Rows {
    kept: Vec<String>,
    bytes: u64,
    head_limit: usize,
    /// How many of the rows `render` keeps matched the pattern.
    matches: usize,
}

impl Rows {
    fn new(head_limit: usize) -> Self {
        Self {
            kept: Vec::new(),
            bytes: 0,
            head_limit,
            matches: 0,
        }
    }

    /// Returns whether the search can stop, since another row would be dropped.
    fn full(&self) -> bool {
        self.kept.len() > self.head_limit || self.bytes >= MAX_SPILL_BYTES
    }

    /// Keeps `row`, a line that matched the pattern when `is_match`
    /// and a context line or range separator otherwise.
    fn push(&mut self, row: String, is_match: bool) {
        if is_match && self.kept.len() < self.head_limit {
            self.matches += 1;
        }
        self.bytes += row.len() as u64 + 1;
        self.kept.push(row);
    }

    /// The kept rows, cut to `head_limit`, with a footer naming the limit that dropped any,
    /// titled with the match count they show.
    fn render(mut self) -> GrepOutput {
        if self.kept.is_empty() {
            return GrepOutput {
                content: NO_MATCHES_FOUND.into(),
                title: NO_MATCHES_FOUND.into(),
            };
        }

        let footer = if self.kept.len() > self.head_limit {
            self.kept.truncate(self.head_limit);
            Some(format!("{} lines", self.head_limit))
        } else if self.bytes >= MAX_SPILL_BYTES {
            let size = format_size(MAX_SPILL_BYTES);
            Some(format!("{size} of output"))
        } else {
            None
        };

        let mut content = self.kept.join("\n");
        if let Some(footer) = footer {
            _ = write!(content, "\n\n{RESULTS_LIMIT_PREFIX}{footer})");
        }
        let word = plural(self.matches, "match", "matches");
        GrepOutput {
            content,
            title: format!("{} {word}", self.matches),
        }
    }
}

fn search_no_context(text: &str, re: &regex::Regex, display_path: &str, rows: &mut Rows) {
    for (line_num, line) in text.lines().enumerate() {
        if rows.full() {
            return;
        }
        if re.is_match(line) {
            let truncated = super::truncate_line(line, MAX_LINE_CHARS);
            rows.push(format!("{display_path}:{}:{truncated}", line_num + 1), true);
        }
    }
}

fn search_with_context(
    text: &str,
    re: &regex::Regex,
    display_path: &str,
    context: usize,
    rows: &mut Rows,
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
        let end = idx.saturating_add(context).min(lines.len() - 1);
        if let Some(last) = ranges.last_mut()
            && start <= last.1 + 1
        {
            last.1 = end;
            continue;
        }
        ranges.push((start, end));
    }

    for (range_idx, &(start, end)) in ranges.iter().enumerate() {
        if rows.full() {
            return;
        }
        if range_idx > 0 {
            rows.push("--".to_owned(), false);
        }
        for (i, line) in lines.iter().enumerate().take(end + 1).skip(start) {
            if rows.full() {
                return;
            }
            let is_match = match_indices.binary_search(&i).is_ok();
            let sep = if is_match { ':' } else { '-' };
            let truncated = super::truncate_line(line, MAX_LINE_CHARS);
            rows.push(
                format!("{display_path}:{}{sep}{truncated}", i + 1),
                is_match,
            );
        }
    }
}

// ── Files-with-Matches Mode ──

fn format_files_with_matches<'a>(
    texts: impl Iterator<Item = (&'a Path, String)>,
    cwd: &Path,
    re: &regex::Regex,
    head_limit: usize,
) -> GrepOutput {
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
        return GrepOutput {
            content: NO_FILES_FOUND.into(),
            title: NO_MATCHES_FOUND.into(),
        };
    }

    let truncated = matching_files.len() > head_limit;
    matching_files.truncate(head_limit);
    let num_files = matching_files.len();
    let file_word = plural(num_files, "file", "files");

    let title = format!("Found {num_files} {file_word}");
    let files = matching_files.join("\n");
    let mut content = formatdoc! {"
        {title}
        {files}"
    };

    if truncated {
        _ = write!(content, "\n\n{RESULTS_LIMIT_PREFIX}{head_limit} files)");
    }

    GrepOutput { content, title }
}

// ── Count Mode ──

fn format_count<'a>(
    texts: impl Iterator<Item = (&'a Path, String)>,
    cwd: &Path,
    re: &regex::Regex,
    head_limit: usize,
) -> GrepOutput {
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
        return GrepOutput {
            content: NO_MATCHES_FOUND.into(),
            title: NO_MATCHES_FOUND.into(),
        };
    }

    let total_files = counts.len();
    let truncated = total_files > head_limit;
    counts.truncate(head_limit);

    let title = format!(
        "Found {total_matches} total {} across {total_files} {}",
        plural(total_matches, "occurrence", "occurrences"),
        plural(total_files, "file", "files"),
    );
    let mut content = title.clone();
    for (p, c) in &counts {
        _ = write!(content, "\n{p}:{c}");
    }
    if truncated {
        _ = write!(content, "\n\n{RESULTS_LIMIT_PREFIX}{head_limit} files)");
    }

    GrepOutput { content, title }
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
    use std::collections::HashSet;
    use std::fs::{self, File};

    use indoc::indoc;

    use super::*;
    use crate::permission::Access;
    use crate::tool::local_testing::{self, ctx, deny_reads, search_root, temp_ctx};
    use crate::util::fs::testing::symlink_file;
    use crate::util::path::local_testing::canonical_tempdir;
    use crate::util::path::slash_separated;

    /// A `grep` tool without read rules.
    fn grep_tool() -> GrepTool {
        GrepTool::new(ReadFilter::default())
    }

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

    mod prepare {
        use super::*;

        #[test]
        fn defaults_the_search_root_to_cwd() {
            // A deny scoped to the project root must apply when `path` is omitted.
            let dir = canonical_tempdir();

            let prepared =
                grep_tool().prepare(serde_json::json!({"pattern": "x"}), &ctx(dir.path()));

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
    }

    mod result_view {
        use super::*;

        #[test]
        fn content_mode_groups_by_file() {
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
                let view = grep_tool().result_view(
                    &input,
                    "src/main.rs:10:fn main()",
                    &ToolMetadata::default(),
                    Path::new("/repo"),
                );

                assert_eq!(view.as_ref(), Some(&expected), "{input}");
            }
        }

        #[test]
        fn non_content_mode_falls_back_to_text() {
            for mode in ["files_with_matches", "count"] {
                let input = serde_json::json!({"pattern": "fn", "output_mode": mode});
                let view = grep_tool().result_view(
                    &input,
                    "irrelevant",
                    &ToolMetadata::default(),
                    Path::new("/repo"),
                );
                assert!(view.is_none(), "mode {mode}: expected None to fall back");
            }
        }
    }

    mod run {
        use super::*;

        #[tokio::test]
        async fn matching_pattern_reports_matches() {
            let dir = canonical_tempdir();
            fs::write(
                dir.path().join("test.rs"),
                indoc! {"
                    fn main() {}
                    fn helper() {}
                "},
            )
            .unwrap();

            let input = serde_json::json!({"pattern": "fn main"});

            let output = local_testing::run(&grep_tool(), input, &ctx(dir.path())).await;

            assert!(!output.is_error, "{}", output.content);
            assert_eq!(output.content, "test.rs:1:fn main() {}");
        }

        #[tokio::test]
        async fn missing_pattern_reports_input_error() {
            let output = local_testing::run(&grep_tool(), serde_json::json!({}), &temp_ctx()).await;

            assert!(output.is_error);
            assert_eq!(output.content, "Invalid input: missing field `pattern`");
        }

        #[tokio::test]
        async fn invalid_regex_omits_title() {
            let pattern = "(unclosed";
            let input = serde_json::json!({"pattern": pattern});

            let output = local_testing::run(&grep_tool(), input, &temp_ctx()).await;

            let cause = regex::Regex::new(pattern).unwrap_err();
            assert!(output.is_error);
            assert_eq!(output.content, format!("Invalid regex: {cause}"));
            assert_eq!(output.metadata.title, None, "errors are not a match count");
        }
    }

    mod grep_files {
        use super::*;

        #[test]
        fn titles_a_row_that_starts_like_a_summary_by_its_match_count() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("Found x.rs"), "hit\n").unwrap();

            let result = grep_files(&params("hit", dir.path())).unwrap();

            assert_eq!(result.content, "Found x.rs:1:hit");
            assert_eq!(result.title, "1 match");
        }

        #[test]
        fn plain_pattern_finds_matching_lines() {
            let dir = canonical_tempdir();
            fs::write(
                dir.path().join("a.rs"),
                indoc! {"
                    fn foo() {}
                    fn bar() {}
                "},
            )
            .unwrap();
            fs::write(dir.path().join("b.rs"), "fn baz() {}\n").unwrap();

            let result = grep_files(&params("fn foo", dir.path())).unwrap().content;
            assert_eq!(result, "a.rs:1:fn foo() {}");
        }

        #[test]
        fn regex_finds_matching_lines() {
            let dir = canonical_tempdir();
            fs::write(
                dir.path().join("test.txt"),
                indoc! {"
                    hello123
                    world456
                    hello789
                "},
            )
            .unwrap();

            let result = grep_files(&params(r"hello\d+", dir.path()))
                .unwrap()
                .content;
            assert_eq!(result, "test.txt:1:hello123\ntest.txt:3:hello789");
        }

        #[test]
        fn case_insensitive_matches_mixed_case() {
            let dir = canonical_tempdir();
            fs::write(
                dir.path().join("test.txt"),
                indoc! {"
                    Hello World
                    hello world
                "},
            )
            .unwrap();

            let mut p = params("hello", dir.path());
            p.case_insensitive = true;
            let result = grep_files(&p).unwrap().content;

            assert_eq!(result, "test.txt:1:Hello World\ntest.txt:2:hello world");
        }

        #[test]
        fn with_context_includes_surrounding_lines() {
            let dir = canonical_tempdir();
            fs::write(
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
            let result = grep_files(&p).unwrap().content;

            assert_eq!(
                result,
                indoc! {"
                    test.txt:2-bbb
                    test.txt:3:ccc
                    test.txt:4-ddd"
                },
            );
        }

        #[test]
        fn with_context_merges_adjacent_ranges() {
            let dir = canonical_tempdir();
            fs::write(
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
            let result = grep_files(&p).unwrap().content;

            // MATCH1 ctx → 2-4 and MATCH2 ctx → 5-7, so lines 4-5 bridge the gap and ranges merge.
            assert_eq!(
                result,
                indoc! {"
                    test.txt:2-b
                    test.txt:3:MATCH1
                    test.txt:4-c
                    test.txt:5-d
                    test.txt:6:MATCH2
                    test.txt:7-e"
                },
            );
        }

        #[test]
        fn with_context_separates_distant_ranges() {
            let dir = canonical_tempdir();
            fs::write(
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
            let GrepOutput {
                content: result,
                title,
            } = grep_files(&p).unwrap();

            // MATCH1 ctx → 1-2 and MATCH2 ctx → 7-8, so the gap forces a "--" separator.
            assert_eq!(
                result,
                indoc! {"
                    test.txt:1:MATCH1
                    test.txt:2-a
                    --
                    test.txt:7-f
                    test.txt:8:MATCH2"
                },
            );
            assert_eq!(title, "2 matches");
        }

        #[test]
        fn with_context_reports_no_matches_when_none_match() {
            let dir = canonical_tempdir();
            fs::write(
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
            let result = grep_files(&p).unwrap().content;

            assert_eq!(result, "No matches found");
        }

        #[test]
        fn with_context_respects_head_limit() {
            let dir = canonical_tempdir();
            fs::write(
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
            let result = grep_files(&p).unwrap().content;

            assert_eq!(
                result,
                indoc! {"
                    test.txt:1-before1
                    test.txt:2:MATCH
                    test.txt:3-after1

                    (Results limited to 3 lines)"
                },
            );
        }

        #[test]
        fn include_glob_skips_the_files_it_does_not_match() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("code.rs"), "fn test() {}\n").unwrap();
            fs::write(dir.path().join("readme.md"), "fn test()\n").unwrap();

            let mut p = params("fn test", dir.path());
            p.include_glob = Some("*.rs");
            let result = grep_files(&p).unwrap().content;

            assert_eq!(result, "code.rs:1:fn test() {}");
        }

        #[test]
        fn single_file_finds_matching_lines() {
            let dir = canonical_tempdir();
            fs::write(
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
            let result = grep_files(&p).unwrap().content;
            assert_eq!(result, "test.txt:2:beta");
        }

        #[test]
        fn single_file_over_the_walk_limit_searches_like_read() {
            // A spill file runs to 10 MiB, so searching one directly ignores the walk's 1 MiB cap.
            let dir = canonical_tempdir();
            let spill = "session/tool-results/toolu_1.txt";
            let lines = 21_000;
            let text = format!(
                "{}error: last line\n",
                format!("{}\n", "x".repeat(99)).repeat(lines)
            );
            fs::create_dir_all(dir.path().join("session/tool-results")).unwrap();
            fs::write(dir.path().join(spill), &text).unwrap();
            assert!(text.len() as u64 > 2 * MAX_WALK_FILE_SIZE);

            let mut p = params("^error", dir.path());
            p.root = search_root(dir.path(), Some(spill)).unwrap();
            let result = grep_files(&p).unwrap().content;

            assert_eq!(result, format!("{spill}:{}:error: last line", lines + 1));
        }

        #[test]
        fn renders_paths_relative_to_cwd() {
            let dir = canonical_tempdir();
            let cwd = dir.path().join("project");
            fs::create_dir_all(cwd.join("src")).unwrap();
            fs::write(cwd.join("src/lib.rs"), "hit\n").unwrap();
            let outside = dir.path().join("outside.txt");
            fs::write(&outside, "hit\n").unwrap();

            let mut p = params("hit", &cwd);
            p.root = search_root(&cwd, Some("src")).unwrap();
            assert_eq!(grep_files(&p).unwrap().content, "src/lib.rs:1:hit");

            p.root = search_root(&cwd, Some("src/lib.rs")).unwrap();
            assert_eq!(grep_files(&p).unwrap().content, "src/lib.rs:1:hit");

            p.root = search_root(&cwd, Some("../outside.txt")).unwrap();
            assert_eq!(
                grep_files(&p).unwrap().content,
                format!("{}:1:hit", slash_separated(&outside)),
                "hits outside cwd fall back to an absolute path",
            );
        }

        #[test]
        fn skips_binary() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("binary.bin"), b"match\x00here").unwrap();
            fs::write(dir.path().join("text.txt"), "match here\n").unwrap();

            let result = grep_files(&params("match", dir.path())).unwrap().content;
            assert_eq!(result, "text.txt:1:match here");
        }

        #[test]
        fn includes_hidden_paths_but_skips_vcs_dirs() {
            let dir = canonical_tempdir();
            let cargo = dir.path().join(".cargo");
            fs::create_dir(&cargo).unwrap();
            fs::write(cargo.join("config.toml"), "match me\n").unwrap();
            let git = dir.path().join(".git");
            fs::create_dir(&git).unwrap();
            fs::write(git.join("config"), "match me\n").unwrap();

            let result = grep_files(&params("match", dir.path())).unwrap().content;
            assert_eq!(result, ".cargo/config.toml:1:match me");
        }

        #[test]
        fn respects_gitignore() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join(".git")).unwrap();
            fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
            fs::write(dir.path().join("ignored.txt"), "match me\n").unwrap();
            fs::write(dir.path().join("tracked.txt"), "match me\n").unwrap();

            let result = grep_files(&params("match", dir.path())).unwrap().content;
            assert_eq!(result, "tracked.txt:1:match me");
        }

        #[test]
        fn warns_about_skipped_large_files() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("small.txt"), "match here\n").unwrap();
            let large = dir.path().join("large.txt");
            let f = File::create(&large).unwrap();
            f.set_len(MAX_WALK_FILE_SIZE + 1).unwrap();

            let result = grep_files(&params("match", dir.path())).unwrap().content;

            assert_eq!(
                result,
                indoc! {"
                    small.txt:1:match here

                    Skipped (exceeds 1 MiB size limit):
                      large.txt (1.0 MiB)"
                }
            );
        }

        #[test]
        fn lists_what_it_could_not_read_after_the_matches() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.rs"), "hit\n").unwrap();
            let locked = dir.path().join("locked");
            fs::create_dir(&locked).unwrap();
            fs::write(locked.join("b.rs"), "hit\n").unwrap();
            let secret = dir.path().join("secret.rs");
            fs::write(&secret, "hit\n").unwrap();
            let _denied = [
                local_testing::deny_read_access(&locked),
                local_testing::deny_read_access(&secret),
            ];

            let GrepOutput {
                content: result,
                title,
            } = grep_files(&params("hit", dir.path())).unwrap();

            let dir_cause = fs::read_dir(&locked).unwrap_err();
            let file_cause = fs::read(&secret).unwrap_err();
            assert_eq!(
                result,
                formatdoc! {"
                    a.rs:1:hit

                    Could not read:
                      locked: {dir_cause}
                      secret.rs: {file_cause}"
                }
            );
            assert_eq!(title, "1 match");
            let view = grep_tool().result_view(
                &serde_json::json!({"pattern": "hit"}),
                &result,
                &ToolMetadata::default(),
                Path::new("/repo"),
            );
            assert_eq!(view, None);
        }

        #[test]
        fn withholds_an_unreadable_directory_a_read_deny_rule_matches() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.rs"), "hit\n").unwrap();
            let secrets = dir.path().join("secrets");
            fs::create_dir(&secrets).unwrap();
            let _denied = local_testing::deny_read_access(&secrets);
            let mut p = params("hit", dir.path());
            p.read_filter = deny_reads(&["secrets"]);

            let result = grep_files(&p).unwrap().content;

            assert_eq!(
                result,
                indoc! {"
                    a.rs:1:hit

                    (1 file withheld by read deny or ask rules)"
                }
            );
        }

        #[test]
        fn without_matches_still_lists_what_it_could_not_read() {
            let dir = canonical_tempdir();
            let secret = dir.path().join("secret.rs");
            fs::write(&secret, "hit\n").unwrap();
            let _denied = local_testing::deny_read_access(&secret);

            let result = grep_files(&params("hit", dir.path())).unwrap().content;

            let cause = fs::read(&secret).unwrap_err();
            assert_eq!(
                result,
                formatdoc! {"
                    No matches found

                    Could not read:
                      secret.rs: {cause}"
                }
            );
        }

        #[test]
        fn withholds_files_a_read_deny_rule_matches() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.rs"), "TOKEN=public\n").unwrap();
            fs::create_dir(dir.path().join("sub")).unwrap();
            fs::write(dir.path().join("sub/.env"), "TOKEN=secret\n").unwrap();
            let mut p = params("TOKEN", dir.path());
            p.read_filter = deny_reads(&[".env"]);
            assert_eq!(
                grep_files(&p).unwrap().content,
                indoc! {"
                    a.rs:1:TOKEN=public

                    (1 file withheld by read deny or ask rules)"
                }
            );

            p.pattern = "secret";
            let GrepOutput {
                content: result,
                title,
            } = grep_files(&p).unwrap();
            assert_eq!(
                result,
                indoc! {"
                    No matches found

                    (1 file withheld by read deny or ask rules)"
                }
            );
            assert_eq!(title, "No matches found");
            // The structured view would drop the note, so the text view shows it.
            let input = serde_json::json!({"pattern": "secret"});
            let view = grep_tool().result_view(
                &input,
                &result,
                &ToolMetadata::default(),
                Path::new("/repo"),
            );
            assert_eq!(view, None);
        }

        #[test]
        fn unmatched_pattern_reports_no_matches() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("test.txt"), "hello world\n").unwrap();

            let result = grep_files(&params("nonexistent", dir.path())).unwrap();
            assert_eq!(result.content, "No matches found");
            assert_eq!(result.title, "No matches found");
        }

        #[test]
        fn invalid_regex_reports_error() {
            let pattern = "[invalid";

            let err = grep_files(&params(pattern, Path::new("/"))).unwrap_err();

            let cause = regex::Regex::new(pattern).unwrap_err();
            assert_eq!(err, format!("Invalid regex: {cause}"));
        }

        #[test]
        fn rejects_an_invalid_include_pattern() {
            let dir = canonical_tempdir();
            let mut p = params("test", dir.path());
            p.include_glob = Some("[bad");
            let err = grep_files(&p).unwrap_err();

            let cause = globset::Glob::new("[bad").unwrap_err();
            assert_eq!(err, format!("Invalid include pattern: {cause}"));
        }

        #[test]
        fn skips_a_searched_file_over_the_tracked_size_limit() {
            let dir = canonical_tempdir();
            let large = dir.path().join("large.txt");
            let f = File::create(&large).unwrap();
            f.set_len(MAX_TRACKED_FILE_SIZE + 1).unwrap();

            let mut p = params("match", dir.path());
            p.root = search_root(dir.path(), Some("large.txt")).unwrap();
            let result = grep_files(&p).unwrap().content;

            assert_eq!(
                result,
                indoc! {"
                    No matches found

                    Skipped (exceeds 10 MiB size limit):
                      large.txt (10.0 MiB)"
                }
            );
        }

        #[cfg(unix)]
        #[test]
        fn withholds_a_denied_file_below_a_symlinked_root() {
            let dir = canonical_tempdir();
            let (cwd, outside) = (dir.path().join("cwd"), dir.path().join("outside"));
            fs::create_dir(&cwd).unwrap();
            fs::create_dir_all(outside.join("secrets")).unwrap();
            fs::write(outside.join("secrets/db.key"), "TOKEN=secret\n").unwrap();
            fs::write(outside.join("app.toml"), "TOKEN=public\n").unwrap();
            std::os::unix::fs::symlink(&outside, cwd.join("config")).unwrap();
            let mut p = params("TOKEN", &cwd);
            p.root = search_root(&cwd, Some("config")).unwrap();
            p.read_filter = deny_reads(&["secrets"]);
            let app = outside.join("app.toml").display().to_string();
            assert_eq!(
                grep_files(&p).unwrap().content,
                formatdoc! {"
                    {app}:1:TOKEN=public

                    (1 file withheld by read deny or ask rules)"
                }
            );
        }

        #[test]
        fn head_limit_truncates_matching_lines() {
            let dir = canonical_tempdir();
            let content = (0..20).fold(String::new(), |mut s, i| {
                _ = writeln!(s, "match line {i}");
                s
            });
            fs::write(dir.path().join("test.txt"), &content).unwrap();

            let mut p = params("match", dir.path());
            p.head_limit = Some(5);

            let result = grep_files(&p).unwrap().content;

            assert_eq!(
                result,
                indoc! {"
                    test.txt:1:match line 0
                    test.txt:2:match line 1
                    test.txt:3:match line 2
                    test.txt:4:match line 3
                    test.txt:5:match line 4

                    (Results limited to 5 lines)"
                },
            );
        }

        #[test]
        fn head_limit_exactly_reached_is_not_flagged() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("test.txt"), "match\nmatch\nmatch\n").unwrap();

            let mut p = params("match", dir.path());
            p.head_limit = Some(3);
            let result = grep_files(&p).unwrap().content;
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
        fn head_limit_zero_means_unlimited() {
            let dir = canonical_tempdir();
            let content = (0..DEFAULT_HEAD_LIMIT + 10).fold(String::new(), |mut s, i| {
                _ = writeln!(s, "match line {i}");
                s
            });
            fs::write(dir.path().join("test.txt"), &content).unwrap();

            let mut p = params("match", dir.path());
            p.head_limit = Some(0);
            let result = grep_files(&p).unwrap().content;

            let expected: Vec<String> = (0..DEFAULT_HEAD_LIMIT + 10)
                .map(|i| format!("test.txt:{}:match line {i}", i + 1))
                .collect();
            assert_eq!(result, expected.join("\n"));
        }

        #[test]
        fn head_limit_across_multiple_files() {
            let dir = canonical_tempdir();
            for name in ["a.txt", "b.txt"] {
                fs::write(dir.path().join(name), "match\nmatch\nmatch\n").unwrap();
            }

            let mut p = params("match", dir.path());
            p.head_limit = Some(3);
            let result = grep_files(&p).unwrap().content;

            // Files with the same mtime may come in either order,
            // so the rows name whichever came first.
            let (rows, footer) = result.split_once("\n\n").unwrap();
            let (file, _) = rows.split_once(':').unwrap();

            assert!(["a.txt", "b.txt"].contains(&file), "{result}");
            assert_eq!(
                rows,
                formatdoc! {"
                    {file}:1:match
                    {file}:2:match
                    {file}:3:match"
                },
            );
            assert_eq!(footer, "(Results limited to 3 lines)");
        }

        #[test]
        fn files_with_matches_mode_lists_matching_paths() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.rs"), "fn foo() {}\n").unwrap();
            fs::write(dir.path().join("b.rs"), "fn bar() {}\n").unwrap();
            fs::write(dir.path().join("c.txt"), "no match\n").unwrap();

            let mut p = params("fn", dir.path());
            p.output_mode = OutputMode::FilesWithMatches;
            let GrepOutput {
                content: result,
                title,
            } = grep_files(&p).unwrap();
            let (header, files) = result.split_once('\n').unwrap();
            let mut files: Vec<_> = files.lines().collect();
            files.sort_unstable();

            assert_eq!(header, "Found 2 files");
            assert_eq!(files, ["a.rs", "b.rs"]);
            assert_eq!(title, "Found 2 files");
        }

        #[test]
        fn files_with_matches_lists_files_up_to_the_head_limit() {
            let dir = canonical_tempdir();
            for i in 0..5 {
                fs::write(dir.path().join(format!("{i}.txt")), "match\n").unwrap();
            }

            let mut p = params("match", dir.path());
            p.output_mode = OutputMode::FilesWithMatches;
            p.head_limit = Some(2);
            let result = grep_files(&p).unwrap().content;

            let (listing, footer) = result.split_once("\n\n").unwrap();
            let (header, files) = listing.split_once('\n').unwrap();
            let files: HashSet<_> = files.lines().collect();
            let all: HashSet<_> = ["0.txt", "1.txt", "2.txt", "3.txt", "4.txt"].into();

            assert_eq!(header, "Found 2 files");
            assert_eq!(files.len(), 2);
            assert!(files.is_subset(&all), "{files:?}");
            assert_eq!(footer, "(Results limited to 2 files)");
        }

        #[test]
        fn files_with_matches_exactly_at_limit_is_not_flagged() {
            let dir = canonical_tempdir();
            for i in 0..2 {
                fs::write(dir.path().join(format!("{i}.txt")), "match\n").unwrap();
            }

            let mut p = params("match", dir.path());
            p.output_mode = OutputMode::FilesWithMatches;
            p.head_limit = Some(2);
            let result = grep_files(&p).unwrap().content;

            let (header, files) = result.split_once('\n').unwrap();
            let mut files: Vec<_> = files.lines().collect();
            files.sort_unstable();

            assert_eq!(header, "Found 2 files");
            assert_eq!(files, ["0.txt", "1.txt"], "no further file was dropped");
        }

        #[test]
        fn files_with_matches_reports_no_files_without_a_match() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.rs"), "fn foo() {}\n").unwrap();

            let mut p = params("nonexistent", dir.path());
            p.output_mode = OutputMode::FilesWithMatches;
            let result = grep_files(&p).unwrap();

            assert_eq!(result.content, "No files found");
            assert_eq!(result.title, "No matches found");
        }

        #[test]
        fn count_mode_reports_occurrences() {
            let dir = canonical_tempdir();
            fs::write(
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
            let result = grep_files(&p).unwrap().content;

            assert_eq!(
                result,
                "Found 2 total occurrences across 1 file\ntest.txt:2"
            );
        }

        #[test]
        fn count_mode_summary_counts_files_past_the_head_limit() {
            let dir = canonical_tempdir();
            for i in 0..5 {
                fs::write(dir.path().join(format!("{i}.txt")), "match\n").unwrap();
            }

            let mut p = params("match", dir.path());
            p.output_mode = OutputMode::Count;
            p.head_limit = Some(2);
            let result = grep_files(&p).unwrap().content;

            // The summary counts all 5 files even though `head_limit` caps the listing at 2.
            let (listing, footer) = result.split_once("\n\n").unwrap();
            let (header, counts) = listing.split_once('\n').unwrap();
            let counts: HashSet<_> = counts.lines().collect();
            let all: HashSet<_> = ["0.txt:1", "1.txt:1", "2.txt:1", "3.txt:1", "4.txt:1"].into();

            assert_eq!(header, "Found 5 total occurrences across 5 files");
            assert_eq!(counts.len(), 2);
            assert!(counts.is_subset(&all), "{counts:?}");
            assert_eq!(footer, "(Results limited to 2 files)");
        }

        #[test]
        fn count_mode_summary_uses_singular_words_for_one() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.txt"), "match once\n").unwrap();

            let mut p = params("match", dir.path());
            p.output_mode = OutputMode::Count;
            let result = grep_files(&p).unwrap().content;

            assert_eq!(result, "Found 1 total occurrence across 1 file\na.txt:1");
        }

        #[test]
        fn count_mode_titles_and_leads_with_the_summary() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.txt"), "match\n").unwrap();
            fs::write(dir.path().join("b.txt"), "match\n").unwrap();

            let mut p = params("match", dir.path());
            p.output_mode = OutputMode::Count;
            let GrepOutput {
                content: result,
                title,
            } = grep_files(&p).unwrap();

            let (header, counts) = result.split_once('\n').unwrap();
            let counts: HashSet<_> = counts.lines().collect();
            assert_eq!(title, "Found 2 total occurrences across 2 files");
            assert_eq!(header, title);
            assert_eq!(counts, HashSet::from(["a.txt:1", "b.txt:1"]));
        }

        #[test]
        fn count_mode_without_matches_reports_no_matches() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("test.txt"), "other\n").unwrap();

            let mut p = params("absent", dir.path());
            p.output_mode = OutputMode::Count;
            let result = grep_files(&p).unwrap();
            assert_eq!(result.content, "No matches found");
            assert_eq!(result.title, "No matches found");
        }
    }

    mod texts {
        use super::*;

        #[test]
        fn records_a_failed_read_but_skips_a_file_that_vanished() {
            let dir = canonical_tempdir();
            let secret = dir.path().join("secret.txt");
            fs::write(&secret, "x").unwrap();
            let _denied = local_testing::deny_read_access(&secret);
            let files = [dir.path().join("gone.txt"), secret.clone()];
            let cancel = CancellationToken::new();
            let mut unreadable = Vec::new();

            let read: Vec<_> =
                texts(&files, MAX_WALK_FILE_SIZE, &cancel, &mut unreadable).collect();

            let cause = fs::read(&secret).unwrap_err().to_string();
            assert_eq!(read, []);
            assert_eq!(
                unreadable,
                [Unreadable {
                    path: Some(secret),
                    error: cause,
                }]
            );
        }
    }

    mod read_text {
        use super::*;

        #[test]
        fn reads_text_and_skips_binary() {
            let dir = canonical_tempdir();
            let text = dir.path().join("a.txt");
            let binary = dir.path().join("a.bin");
            fs::write(&text, "fn main() {}\n").unwrap();
            fs::write(&binary, b"\0\x01").unwrap();

            assert_eq!(
                read_text(&text, MAX_WALK_FILE_SIZE).unwrap().as_deref(),
                Some("fn main() {}\n"),
            );
            assert_eq!(read_text(&binary, MAX_WALK_FILE_SIZE).unwrap(), None);
        }

        #[test]
        fn refuses_a_file_past_the_size_limit() {
            let dir = canonical_tempdir();
            let path = dir.path().join("big.txt");
            let limit = 1024 * 1024;
            fs::write(&path, vec![b'a'; 1024 * 1024 + 1]).unwrap();

            let err = read_text(&path, limit).unwrap_err();

            assert_eq!(err.to_string(), "grew past the 1 MiB size limit");
        }

        #[test]
        fn refuses_a_symlink() {
            let dir = canonical_tempdir();
            let target = dir.path().join("target.txt");
            fs::write(&target, "SECRET=1").unwrap();
            let link = dir.path().join("link.txt");
            symlink_file(&target, &link);

            let err = read_text(&link, MAX_WALK_FILE_SIZE).unwrap_err();

            #[cfg(unix)]
            let cause = io::Error::from_raw_os_error(nix::errno::Errno::ELOOP as i32).to_string();
            #[cfg(windows)]
            let cause = "the final path component is a symlink";
            assert_eq!(err.to_string(), cause);
        }

        #[cfg(unix)]
        #[test]
        fn refuses_a_fifo_without_blocking() {
            let dir = canonical_tempdir();
            let path = dir.path().join("fifo");
            nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();

            let err = read_text(&path, MAX_WALK_FILE_SIZE).unwrap_err();

            assert_eq!(err.to_string(), "not a regular file");
        }
    }

    mod format_content {
        use super::*;

        #[test]
        fn stops_reading_files_once_the_rows_are_full() {
            let re = regex::Regex::new("m").unwrap();
            let texts = [
                (Path::new("/repo/a.rs"), "m\nm\n".to_owned()),
                (Path::new("/repo/b.rs"), "m\n".to_owned()),
            ];

            let output = format_content(texts.into_iter(), Path::new("/repo"), &re, 0, 1).content;

            assert_eq!(
                output,
                indoc! {"
                    a.rs:1:m

                    (Results limited to 1 lines)"
                },
            );
        }
    }

    mod rows {
        use super::*;

        mod render {
            use super::*;

            #[test]
            fn keeps_every_row_under_both_limits() {
                let mut rows = Rows::new(2);
                rows.push("a".to_owned(), true);
                rows.push("b".to_owned(), true);

                assert!(!rows.full());
                assert_eq!(rows.render().content, "a\nb");
            }

            #[test]
            fn cuts_to_the_head_limit_and_names_it() {
                let mut rows = Rows::new(2);
                for row in ["a", "b", "c"] {
                    rows.push(row.to_owned(), true);
                }

                assert!(rows.full());
                let rendered = rows.render();
                assert_eq!(
                    rendered.content,
                    indoc! {"
                        a
                        b

                        (Results limited to 2 lines)"
                    },
                );
                assert_eq!(rendered.title, "2 matches");
            }

            #[test]
            fn stops_at_the_spill_limit_without_a_head_limit() {
                let row = "x".repeat(1 << 20);
                let mut rows = Rows::new(usize::MAX);
                let mut pushed = 0;
                while !rows.full() {
                    rows.push(row.clone(), true);
                    pushed += 1;
                }

                let rendered = rows.render().content;

                assert_eq!(pushed, 10);
                let kept = vec![row; 10].join("\n");
                assert!(
                    rendered == format!("{kept}\n\n(Results limited to 10 MiB of output)"),
                    "the rows and the footer naming the byte limit",
                );
            }

            #[test]
            fn without_rows_reports_no_matches() {
                assert_eq!(Rows::new(2).render().content, "No matches found");
            }
        }
    }

    mod search_with_context {
        use super::*;

        #[test]
        fn context_past_the_file_shows_every_line_around_the_match() {
            let re = regex::Regex::new("b").unwrap();
            let mut rows = Rows::new(usize::MAX);

            search_with_context("a\nb\nc\n", &re, "f.txt", usize::MAX, &mut rows);

            assert_eq!(rows.kept, ["f.txt:1-a", "f.txt:2:b", "f.txt:3-c"]);
        }

        #[test]
        fn stops_between_ranges_once_the_rows_are_full() {
            let re = regex::Regex::new("a").unwrap();
            let mut rows = Rows::new(1);

            search_with_context("a\nx\nx\nx\na\n", &re, "f.txt", 1, &mut rows);

            assert_eq!(rows.kept, ["f.txt:1:a", "f.txt:2-x"]);
        }
    }

    mod parse_content_view {
        use super::*;

        #[test]
        fn no_matches_is_empty() {
            assert_eq!(
                parse_content_view("No matches found"),
                Some(ToolResultView::GrepMatches {
                    groups: Vec::new(),
                    truncated: false,
                }),
            );
        }

        #[test]
        fn groups_consecutive_lines_with_context_and_separator() {
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
        fn truncation_footer_sets_truncated_flag() {
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
        fn falls_back_when_skipped_warnings_present() {
            // GrepMatches can't hold the skipped-file warning, so the view falls back to show it.
            for content in [
                indoc! {"
                    src/main.rs:1:hit

                    Skipped (exceeds 1 MiB size limit):
                      big.txt (5 MiB)"
                },
                indoc! {"
                    No matches found

                    Skipped (exceeds 1 MiB size limit):
                      big.txt (5 MiB)"
                },
            ] {
                assert_eq!(parse_content_view(content), None, "{content}");
            }
        }

        #[test]
        fn falls_back_on_invalid_line() {
            assert!(parse_content_view("src/main.rs:1:hit\nunexpected line").is_none());
        }
    }

    mod parse_match_line {
        use super::*;

        #[test]
        fn distinguishes_match_from_context_separator() {
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
        fn skips_path_internal_colons_without_digit_separator() {
            let (path, m) = parse_match_line("C:foo:42:body").unwrap();
            assert_eq!(path, "C:foo");
            assert_eq!(m.number, 42);
            assert_eq!(m.text, "body");
            assert!(m.is_match);
        }

        #[test]
        fn rejects_a_line_of_another_shape() {
            assert!(parse_match_line("not a match line").is_none());
            assert!(parse_match_line("path:non_digits:text").is_none());
            assert!(parse_match_line("path:42").is_none());
        }
    }
}
