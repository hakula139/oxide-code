//! Regex content search tool.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::fmt::Write as _;
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use grep_matcher::LineTerminator;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use indoc::formatdoc;
use serde::{Deserialize, Serialize};
use strum::VariantArray;
use tokio_util::sync::CancellationToken;

use super::file::{is_binary, strip_bom};
use super::search::{
    NO_FILES_FOUND, SearchOutput, SearchRoot, SearchRow, SearchRows, Unreadable, search_arg,
};
use super::view::{GrepMatchLine, ToolResultView};
use super::{
    Prepared, StoredView, Tool, ToolCtx, ToolDefinition, ToolOutput, ToolSpec, extract_input_field,
};
use crate::file_tracker::MAX_TRACKED_FILE_SIZE;
use crate::permission::{PermissionClass, ReadFilter, WalkFilter};
use crate::util::fs::{FileReadError, read_regular_file};
use crate::util::text::{ELLIPSIS, format_size, plural};

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
            definition: ToolDefinition {
                name: "grep".to_owned(),
                description: "Search file contents with a Rust `regex` pattern, \
                              which supports no lookaround or backreferences. \
                              Each line is matched on its own, so a pattern cannot span lines \
                              or contain a line break. \
                              A directory search includes hidden files and skips VCS directories \
                              and files that `.gitignore` excludes."
                    .to_owned(),
                input_schema: input_schema(),
            },
            class: PermissionClass::Read,
            parallel_safe: true,
        }
    }

    fn prepare(&self, input: serde_json::Value, ctx: &ToolCtx) -> Result<Prepared, String> {
        Prepared::parse(input, |input: &Input| {
            Ok(ctx.path_access(search_arg(input.path.as_deref())))
        })
    }

    fn icon(&self) -> &'static str {
        "⌕"
    }

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "pattern")
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
                "enum": OutputMode::VARIANTS,
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
        "required": ["pattern"],
        "additionalProperties": false
    })
}

#[derive(Clone, Copy, Default, Serialize, Deserialize, VariantArray)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    Content,
    FilesWithMatches,
    Count,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
    let (
        Input {
            pattern,
            path,
            include,
            output_mode,
            context,
            case_insensitive,
            head_limit,
        },
        base,
    ) = prepared.into_path_input();

    let workspace = Arc::clone(&ctx.workspace);
    let cancel = ctx.cancel.clone();
    super::run_blocking(move || {
        grep_files(&GrepParams {
            pattern: &pattern,
            root: SearchRoot::new(base, path.as_deref(), workspace)?,
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
    .map_or_else(ToolOutput::error, ToolOutput::from)
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

fn grep_files(params: &GrepParams<'_>) -> Result<SearchOutput, String> {
    let matcher = matcher(params.pattern, params.case_insensitive)?;
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
        unreadable,
        withheld,
    } = collect_files(
        &root.base,
        include_matcher.as_ref(),
        &read_filter,
        &params.cancel,
    );

    let context = match params.output_mode {
        OutputMode::Content => params.context,
        OutputMode::FilesWithMatches | OutputMode::Count => 0,
    };
    let mut search = Search::new(matcher, context, size_limit, &params.cancel, unreadable);
    let cwd = &root.cwd;
    let mut output = match params.output_mode {
        OutputMode::FilesWithMatches => {
            format_files_with_matches(&mut search, &files, cwd, head_limit)
        }
        OutputMode::Count => format_count(&mut search, &files, cwd, head_limit),
        OutputMode::Content => format_content(&mut search, &files, cwd, head_limit),
    };

    let unreadable = search.unreadable;
    push_skipped_note(&mut output.content, &skipped_large, size_limit, cwd);
    super::search::push_unreadable_note(&mut output.content, &unreadable, cwd);
    super::search::push_withheld_note(&mut output.content, withheld);
    if !skipped_large.is_empty() || !unreadable.is_empty() || withheld > 0 {
        output.view = Some(StoredView::Text);
    }

    Ok(output)
}

/// Compiles `pattern` into a matcher whose matches each lie within one line, where `^` and `$`
/// match at line boundaries, `$` also before a `\r\n`. A pattern holding a line break fails, while
/// a lone `\r` stays searchable. Size limits keep a hostile pattern from exhausting memory.
fn matcher(pattern: &str, case_insensitive: bool) -> Result<RegexMatcher, String> {
    // The builder parses the pattern wrapped in `(?:...)`, which would accept an unbalanced
    // `a)|(b` and quote the wrapped pattern in errors, so the pattern is checked alone first.
    regex_syntax::ParserBuilder::new()
        .utf8(false)
        .case_insensitive(case_insensitive)
        .build()
        .parse(pattern)
        .map_err(|e| format!("Invalid regex: {e}"))?;
    RegexMatcherBuilder::new()
        .case_insensitive(case_insensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .multi_line(true)
        // `crlf` also sets a CRLF line terminator, which would stop `\r` from matching, so the
        // terminator follows it.
        .crlf(true)
        .line_terminator(Some(b'\n'))
        .build(pattern)
        .map_err(|e| format!("Invalid regex: {e}"))
}

struct CollectedFiles {
    /// The files to search, newest first, then by path.
    files: Vec<PathBuf>,
    /// The files over `size_limit` with their sizes, by path.
    skipped_large: Vec<(PathBuf, u64)>,
    /// The size in bytes above which a file went to `skipped_large`.
    size_limit: u64,
    /// Paths the walk could not read, by path.
    unreadable: Vec<Unreadable>,
    /// Files and unreadable paths a `read` deny or ask rule withheld.
    withheld: usize,
}

/// A file the walk found, checked against the include glob, the read rules, and the size limit.
enum Found {
    File { path: PathBuf, modified: SystemTime },
    Large { path: PathBuf, size: u64 },
    Withheld,
}

/// The files to search under `base`. A file `base` itself passed the gate, so only the files a walk
/// finds face `read_filter`.
fn collect_files(
    base: &Path,
    include_matcher: Option<&globset::GlobMatcher>,
    read_filter: &WalkFilter,
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

    let (found, mut unreadable) = super::search::walk_files_parallel(base, cancel, |entry| {
        if include_matcher.is_some_and(|m| !m.is_match(entry.file_name())) {
            return None;
        }
        if read_filter.withholds(entry.path()) {
            return Some(Found::Withheld);
        }
        let metadata = entry.metadata().ok();
        if let Some(size) = metadata.as_ref().map(Metadata::len)
            && size > MAX_WALK_FILE_SIZE
        {
            let path = entry.into_path();
            return Some(Found::Large { path, size });
        }
        // An entry whose stat fails sorts oldest, and its read reports the failure.
        let modified = metadata
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let path = entry.into_path();
        Some(Found::File { path, modified })
    });

    let mut files = Vec::new();
    let mut skipped_large = Vec::new();
    let mut withheld = 0;
    for found in found {
        match found {
            Found::File { path, modified } => files.push((Reverse(modified), path)),
            Found::Large { path, size } => skipped_large.push((path, size)),
            Found::Withheld => withheld += 1,
        }
    }
    files.sort_unstable();
    skipped_large.sort_unstable();
    unreadable.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    withheld += super::search::withhold_unreadable(&mut unreadable, read_filter);

    CollectedFiles {
        files: files.into_iter().map(|(_, path)| path).collect(),
        skipped_large,
        size_limit: MAX_WALK_FILE_SIZE,
        unreadable,
        withheld,
    }
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

// ── File Search ──

/// Searches files one at a time with one matcher, collecting the files it could not read.
struct Search<'a> {
    searcher: Searcher,
    matcher: RegexMatcher,
    size_limit: u64,
    cancel: &'a CancellationToken,
    unreadable: Vec<Unreadable>,
}

/// A line a search reports.
enum Line<'b> {
    /// A matching line, or a context line around one, with its 1-based number and its bytes, line
    /// ending included.
    Text {
        number: u64,
        bytes: &'b [u8],
        is_match: bool,
    },
    /// The gap between two runs of matches and their context.
    Break,
}

impl<'a> Search<'a> {
    /// A search reporting `context` lines before and after each match. A file that grows past
    /// `size_limit` bytes before it is read counts as unreadable.
    fn new(
        matcher: RegexMatcher,
        context: usize,
        size_limit: u64,
        cancel: &'a CancellationToken,
        unreadable: Vec<Unreadable>,
    ) -> Self {
        // Binary detection and decoding happen before the search, under the rules `read` follows.
        let searcher = SearcherBuilder::new()
            .line_terminator(LineTerminator::byte(b'\n'))
            .line_number(true)
            .before_context(context)
            .after_context(context)
            .binary_detection(BinaryDetection::none())
            .bom_sniffing(false)
            .build();
        Self {
            searcher,
            matcher,
            size_limit,
            cancel,
            unreadable,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Searches the file at `path` as `read` would show it, passing each line it reports to
    /// `on_line` until that returns `false` or the call is interrupted. A file `read` refuses as
    /// binary reports nothing. The text drops a leading UTF-8 BOM and replaces invalid UTF-8 with
    /// U+FFFD, which `.` matches. A file whose read fails goes to `unreadable`, unless it vanished
    /// after the walk found it.
    ///
    /// The read refuses a symlink and never blocks on a fifo, and the opened file must be a regular
    /// file within the size limit, so an entry swapped after the walk checked it is not read
    /// through.
    fn file(&mut self, path: &Path, on_line: impl FnMut(Line<'_>) -> bool) {
        let bytes = match read_regular_file(path, self.size_limit) {
            Ok(bytes) => bytes,
            Err(FileReadError::Io(e)) if e.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                let error = match error {
                    FileReadError::Io(error) => error,
                    FileReadError::Directory | FileReadError::NonRegular => {
                        io::Error::other("not a regular file")
                    }
                    FileReadError::TooLarge { .. } => {
                        let limit = format_size(self.size_limit);
                        io::Error::other(format!("grew past the {limit} size limit"))
                    }
                };
                self.unreadable.push(Unreadable::from_read(path, &error));
                return;
            }
        };
        if is_binary(&bytes) {
            return;
        }
        let text = String::from_utf8_lossy(&bytes);
        let sink = LineSink {
            on_line,
            cancel: self.cancel,
        };
        if let Err(e) = self
            .searcher
            .search_slice(&self.matcher, strip_bom(&text).as_bytes(), sink)
        {
            self.unreadable.push(Unreadable::from_read(path, &e));
        }
    }
}

struct LineSink<'c, F> {
    on_line: F,
    cancel: &'c CancellationToken,
}

impl<F: FnMut(Line<'_>) -> bool> LineSink<'_, F> {
    fn report(&mut self, line: Line<'_>) -> bool {
        !self.cancel.is_cancelled() && (self.on_line)(line)
    }
}

impl<F: FnMut(Line<'_>) -> bool> Sink for LineSink<'_, F> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> io::Result<bool> {
        Ok(self.report(Line::Text {
            number: mat.line_number().unwrap_or_default(),
            bytes: mat.bytes(),
            is_match: true,
        }))
    }

    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> io::Result<bool> {
        Ok(self.report(Line::Text {
            number: context.line_number().unwrap_or_default(),
            bytes: context.bytes(),
            is_match: false,
        }))
    }

    fn context_break(&mut self, _: &Searcher) -> io::Result<bool> {
        Ok(self.report(Line::Break))
    }
}

/// A reported line's text without its line ending, with invalid UTF-8 replaced.
fn line_text(bytes: &[u8]) -> Cow<'_, str> {
    let line = bytes
        .strip_suffix(b"\n")
        .map_or(bytes, |line| line.strip_suffix(b"\r").unwrap_or(line));
    String::from_utf8_lossy(line)
}

// ── Content Mode ──

fn format_content(
    search: &mut Search<'_>,
    files: &[PathBuf],
    cwd: &Path,
    head_limit: usize,
) -> SearchOutput {
    let mut rows = SearchRows::new(head_limit);
    for path in files {
        if rows.full() || search.cancelled() {
            break;
        }
        let display_path = super::display_path(path, cwd);
        search.file(path, |line| {
            let Line::Text {
                number,
                bytes,
                is_match,
            } = line
            else {
                rows.push(SearchRow::Separator);
                return !rows.full();
            };
            let text = line_text(bytes);
            rows.push(SearchRow::Match {
                path: display_path.clone(),
                line: GrepMatchLine {
                    number: usize::try_from(number).unwrap_or(usize::MAX),
                    text: truncate_line(&text, MAX_LINE_CHARS).into_owned(),
                    is_match,
                },
            });
            !rows.full()
        });
    }

    format_matches(rows)
}

fn format_matches(rows: SearchRows) -> SearchOutput {
    let truncated = rows.full();
    let mut content = rows.render();
    if let Some(footer) = rows.limit_description("lines") {
        _ = write!(content, "\n\n{RESULTS_LIMIT_PREFIX}{footer})");
    }
    let groups = rows.into_groups();
    let matches = groups
        .iter()
        .flat_map(|group| &group.lines)
        .filter(|line| line.is_match)
        .count();
    let title = if content.is_empty() {
        NO_MATCHES_FOUND.clone_into(&mut content);
        content.clone()
    } else {
        format!("{matches} {}", plural(matches, "match", "matches"))
    };
    SearchOutput {
        content,
        title,
        view: Some(StoredView::View(ToolResultView::GrepMatches {
            groups,
            truncated,
        })),
    }
}

/// Caps a line at `max_chars` characters, appending `... [N chars]` with the original length so the
/// model can decide whether to read more. Borrows when the line already fits.
fn truncate_line(line: &str, max_chars: usize) -> Cow<'_, str> {
    if line.len() <= max_chars {
        return Cow::Borrowed(line);
    }
    let Some((boundary, _)) = line.char_indices().nth(max_chars) else {
        return Cow::Borrowed(line);
    };
    let total_chars = max_chars + line[boundary..].chars().count();
    Cow::Owned(format!(
        "{}{ELLIPSIS} [{total_chars} chars]",
        &line[..boundary]
    ))
}

// ── Files-with-Matches Mode ──

fn format_files_with_matches(
    search: &mut Search<'_>,
    files: &[PathBuf],
    cwd: &Path,
    head_limit: usize,
) -> SearchOutput {
    let mut rows = SearchRows::new(head_limit);
    for path in files {
        if rows.full() || search.cancelled() {
            break;
        }
        let mut matched = false;
        search.file(path, |line| {
            matched = matches!(line, Line::Text { is_match: true, .. });
            !matched
        });
        if matched {
            rows.push(SearchRow::Path(super::display_path(path, cwd)));
        }
    }

    if rows.kept.is_empty() {
        return SearchOutput::plain(NO_FILES_FOUND.into(), NO_MATCHES_FOUND.into());
    }

    let num_files = rows.kept.len();
    let file_word = plural(num_files, "file", "files");

    let title = format!("Found {num_files} {file_word}");
    let files = rows.render();
    let mut content = formatdoc! {"
        {title}
        {files}"
    };

    if let Some(footer) = rows.limit_description("files") {
        _ = write!(content, "\n\n{RESULTS_LIMIT_PREFIX}{footer})");
    }

    SearchOutput::plain(content, title)
}

// ── Count Mode ──

fn format_count(
    search: &mut Search<'_>,
    files: &[PathBuf],
    cwd: &Path,
    head_limit: usize,
) -> SearchOutput {
    let mut rows = SearchRows::new(head_limit);
    let mut total_matches: usize = 0;
    let mut total_files: usize = 0;

    for path in files {
        if search.cancelled() {
            break;
        }
        let mut count = 0;
        search.file(path, |line| {
            count += usize::from(matches!(line, Line::Text { is_match: true, .. }));
            true
        });
        if count > 0 {
            total_matches += count;
            total_files += 1;
            if !rows.full() {
                rows.push(SearchRow::Count {
                    path: super::display_path(path, cwd),
                    count,
                });
            }
        }
    }

    if total_files == 0 {
        return SearchOutput::plain(NO_MATCHES_FOUND.into(), NO_MATCHES_FOUND.into());
    }

    let title = format!(
        "Found {total_matches} total {} across {total_files} {}",
        plural(total_matches, "occurrence", "occurrences"),
        plural(total_files, "file", "files"),
    );
    let mut content = format!("{title}\n{}", rows.render());
    if let Some(footer) = rows.limit_description("files") {
        _ = write!(content, "\n\n{RESULTS_LIMIT_PREFIX}{footer})");
    }

    SearchOutput::plain(content, title)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs::{self, File};

    use indoc::indoc;

    use super::*;
    use crate::permission::{Access, PathTarget};
    use crate::tool::ToolRegistry;
    use crate::tool::local_testing::{
        self, ctx, deny_reads, search_root, temp_ctx, write_modified_at,
    };
    use crate::tool::view::GrepFileGroup;
    use crate::util::fs::testing::{deny_read_access, symlink_file};
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

    mod spec {
        use super::*;

        #[test]
        fn states_what_a_pattern_cannot_match() {
            let description = grep_tool().spec().definition.description;

            for rule in [
                "supports no lookaround or backreferences",
                "a pattern cannot span lines",
            ] {
                assert!(description.contains(rule), "should state: {rule}");
            }
        }
    }

    mod prepare {
        use super::*;

        #[test]
        fn defaults_the_search_root_to_cwd() {
            // A deny scoped to the project root must apply when `path` is omitted.
            let dir = canonical_tempdir();

            let prepared = grep_tool()
                .prepare(serde_json::json!({"pattern": "x"}), &ctx(dir.path()))
                .unwrap();

            let Access::Path(PathTarget {
                canonical,
                relative,
                ..
            }) = prepared.access
            else {
                panic!("expected a path target");
            };
            assert_eq!(canonical, dir.path().to_str().unwrap());
            assert_eq!(relative.as_deref(), Some(""));
        }
    }

    mod run {
        use super::*;

        #[tokio::test]
        async fn marks_notes_as_text() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("public.rs"), "hit\n").unwrap();
            fs::write(dir.path().join("secret.rs"), "hit\n").unwrap();
            let registry = ToolRegistry::new(vec![Box::new(GrepTool::new(deny_reads(
                &["secret.rs"],
                dir.path(),
            )))]);

            for (mode, body) in [
                ("content", "public.rs:1:hit"),
                ("files_with_matches", "Found 1 file\npublic.rs"),
                (
                    "count",
                    "Found 1 total occurrence across 1 file\npublic.rs:1",
                ),
            ] {
                let input = serde_json::json!({"pattern": "hit", "output_mode": mode});

                let output = registry
                    .run_input("grep", input.clone(), &ctx(dir.path()))
                    .await;
                assert!(!output.is_error, "{mode}: {}", output.content);

                assert_eq!(
                    output.content,
                    format!("{body}\n\n(1 file withheld by read deny or ask rules)")
                );
                assert_eq!(
                    output.metadata.result_view,
                    Some(StoredView::Text),
                    "{mode}"
                );
                assert_eq!(
                    registry.result_view(
                        "grep",
                        &input,
                        &output.content,
                        &output.metadata,
                        output.is_error,
                        dir.path()
                    ),
                    ToolResultView::Text {
                        content: output.content.clone(),
                    },
                    "{mode}"
                );
            }
        }

        #[tokio::test]
        async fn non_content_modes_store_no_view() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.rs"), "hit\n").unwrap();
            let registry = ToolRegistry::new(vec![Box::new(grep_tool())]);

            for mode in ["files_with_matches", "count"] {
                let input = serde_json::json!({"pattern": "hit", "output_mode": mode});

                let output = registry
                    .run_input("grep", input.clone(), &ctx(dir.path()))
                    .await;

                assert!(!output.is_error, "{mode}: {}", output.content);
                assert_eq!(output.metadata.result_view, None, "{mode}");
                assert_eq!(
                    registry.result_view(
                        "grep",
                        &input,
                        &output.content,
                        &output.metadata,
                        output.is_error,
                        dir.path()
                    ),
                    ToolResultView::Text {
                        content: output.content.clone(),
                    },
                    "{mode}"
                );
            }
        }

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
        async fn invalid_regex_omits_title() {
            let pattern = "(unclosed";
            let input = serde_json::json!({"pattern": pattern});

            let output = local_testing::run(&grep_tool(), input, &temp_ctx()).await;

            let cause = regex::Regex::new(pattern).unwrap_err();
            assert!(output.is_error);
            assert_eq!(output.content, format!("Invalid regex: {cause}"));
            assert_eq!(output.metadata.title, None, "errors are not a match count");
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn preserves_native_paths_and_context_after_metadata_round_trip() {
            use crate::tool::ToolMetadata;

            for name in ["name:12:part.rs", "name:12-part.rs", "line\nname.rs"] {
                let dir = canonical_tempdir();
                fs::write(dir.path().join(name), "before\nhit\nafter\n").unwrap();
                let input = serde_json::json!({"pattern": "hit", "context": 1});
                let registry = ToolRegistry::new(vec![Box::new(grep_tool())]);

                let output = registry
                    .run_input("grep", input.clone(), &ctx(dir.path()))
                    .await;
                assert!(!output.is_error, "{name:?}: {}", output.content);
                let metadata: ToolMetadata =
                    serde_json::from_value(serde_json::to_value(&output.metadata).unwrap())
                        .unwrap();

                assert_eq!(
                    output.content,
                    format!("{name}:1-before\n{name}:2:hit\n{name}:3-after"),
                    "{name:?}",
                );
                let expected = ToolResultView::GrepMatches {
                    groups: vec![GrepFileGroup {
                        path: name.to_owned(),
                        lines: vec![
                            GrepMatchLine {
                                number: 1,
                                text: "before".to_owned(),
                                is_match: false,
                            },
                            GrepMatchLine {
                                number: 2,
                                text: "hit".to_owned(),
                                is_match: true,
                            },
                            GrepMatchLine {
                                number: 3,
                                text: "after".to_owned(),
                                is_match: false,
                            },
                        ],
                    }],
                    truncated: false,
                };
                for cwd in [dir.path(), Path::new("/another/workspace")] {
                    assert_eq!(
                        registry.result_view(
                            "grep",
                            &input,
                            &output.content,
                            &metadata,
                            output.is_error,
                            cwd
                        ),
                        expected,
                        "{name:?} at {cwd:?}",
                    );
                }
            }
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
            let SearchOutput {
                content: result,
                title,
                ..
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
            let _denied = [deny_read_access(&locked), deny_read_access(&secret)];

            let SearchOutput {
                content: result,
                title,
                ..
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
        }

        #[test]
        fn withholds_an_unreadable_directory_a_read_deny_rule_matches() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.rs"), "hit\n").unwrap();
            let secrets = dir.path().join("secrets");
            fs::create_dir(&secrets).unwrap();
            let _denied = deny_read_access(&secrets);
            let mut p = params("hit", dir.path());
            p.read_filter = deny_reads(&["secrets"], dir.path());

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
            let _denied = deny_read_access(&secret);

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
            p.read_filter = deny_reads(&[".env"], dir.path());
            assert_eq!(
                grep_files(&p).unwrap().content,
                indoc! {"
                    a.rs:1:TOKEN=public

                    (1 file withheld by read deny or ask rules)"
                }
            );

            p.pattern = "secret";
            let SearchOutput {
                content: result,
                title,
                ..
            } = grep_files(&p).unwrap();
            assert_eq!(
                result,
                indoc! {"
                    No matches found

                    (1 file withheld by read deny or ask rules)"
                }
            );
            assert_eq!(title, "No matches found");
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
            for pattern in ["[invalid", "a)|(b"] {
                let err = grep_files(&params(pattern, Path::new("/"))).unwrap_err();

                let cause = regex::Regex::new(pattern).unwrap_err();
                assert_eq!(err, format!("Invalid regex: {cause}"), "{pattern}");
            }
        }

        #[test]
        fn rejects_a_pattern_holding_a_line_break() {
            for pattern in ["a\nb", r"a\nb"] {
                let err = grep_files(&params(pattern, Path::new("/"))).unwrap_err();

                assert_eq!(
                    err, r#"Invalid regex: the literal "\n" is not allowed in a regex"#,
                    "{pattern:?}"
                );
            }
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
            p.read_filter = deny_reads(&["secrets"], &cwd);
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
        fn head_limit_keeps_the_newest_files_rows() {
            let dir = canonical_tempdir();
            for (name, secs) in [("a.txt", 2_000_000), ("b.txt", 1_000_000)] {
                write_modified_at(&dir.path().join(name), "match\nmatch\n", secs);
            }

            let mut p = params("match", dir.path());
            p.head_limit = Some(3);
            let result = grep_files(&p).unwrap().content;

            assert_eq!(
                result,
                indoc! {"
                    a.txt:1:match
                    a.txt:2:match
                    b.txt:1:match

                    (Results limited to 3 lines)"
                },
            );
        }

        #[test]
        fn lists_files_newest_first_and_ties_and_notes_by_path() {
            let dir = canonical_tempdir();
            for (name, secs) in [
                ("b.rs", 1_000_000),
                ("new.rs", 2_000_000),
                ("a.rs", 1_000_000),
            ] {
                write_modified_at(&dir.path().join(name), "hit\n", secs);
            }
            for name in ["large_b.txt", "large_a.txt"] {
                let file = File::create(dir.path().join(name)).unwrap();
                file.set_len(MAX_WALK_FILE_SIZE + 1).unwrap();
            }
            let locked = ["locked_b", "locked_a"].map(|name| dir.path().join(name));
            for path in &locked {
                fs::create_dir(path).unwrap();
            }
            let _denied = locked.each_ref().map(|path| deny_read_access(path));

            let result = grep_files(&params("hit", dir.path())).unwrap().content;

            let cause = fs::read_dir(&locked[0]).unwrap_err();
            assert_eq!(
                result,
                formatdoc! {"
                    new.rs:1:hit
                    a.rs:1:hit
                    b.rs:1:hit

                    Skipped (exceeds 1 MiB size limit):
                      large_a.txt (1.0 MiB)
                      large_b.txt (1.0 MiB)

                    Could not read:
                      locked_a: {cause}
                      locked_b: {cause}"
                },
            );
        }

        #[test]
        fn files_with_matches_mode_lists_matching_paths() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.rs"), "fn foo() {}\n").unwrap();
            fs::write(dir.path().join("b.rs"), "fn bar() {}\n").unwrap();
            fs::write(dir.path().join("c.txt"), "no match\n").unwrap();

            let mut p = params("fn", dir.path());
            p.output_mode = OutputMode::FilesWithMatches;
            let SearchOutput {
                content: result,
                title,
                ..
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
            let SearchOutput {
                content: result,
                title,
                ..
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

    mod search {
        use super::*;

        mod file {
            use super::*;

            /// Searches `path` for `pattern` with `context` lines around matches, rendering lines
            /// as `number:text` for matches, `number-text` for context, and `--` for breaks.
            fn search_lines(
                pattern: &str,
                context: usize,
                path: &Path,
            ) -> (Vec<String>, Vec<Unreadable>) {
                let cancel = CancellationToken::new();
                let matcher = matcher(pattern, false).unwrap();
                let mut search =
                    Search::new(matcher, context, MAX_WALK_FILE_SIZE, &cancel, Vec::new());
                let mut lines = Vec::new();

                search.file(path, |line| {
                    lines.push(match line {
                        Line::Text {
                            number,
                            bytes,
                            is_match,
                        } => {
                            let separator = if is_match { ':' } else { '-' };
                            format!("{number}{separator}{}", line_text(bytes))
                        }
                        Line::Break => "--".to_owned(),
                    });
                    true
                });

                (lines, search.unreadable)
            }

            #[test]
            fn reports_matches_and_context_without_their_line_endings() {
                let dir = canonical_tempdir();
                let path = dir.path().join("a.txt");
                fs::write(&path, "a\r\nb\r\nc\r\nd\r\ne\r\nb\n").unwrap();

                let (lines, unreadable) = search_lines("b$", 1, &path);

                assert_eq!(lines, ["1-a", "2:b", "3-c", "--", "5-e", "6:b"]);
                assert_eq!(unreadable, []);
            }

            #[test]
            fn matches_a_carriage_return_within_a_line() {
                let dir = canonical_tempdir();
                let path = dir.path().join("a.txt");
                fs::write(&path, "a\rb\r\nab\n").unwrap();

                let (lines, unreadable) = search_lines(r"a\rb", 0, &path);

                assert_eq!(lines, ["1:a\rb"]);
                assert_eq!(unreadable, []);
            }

            #[test]
            fn skips_a_file_read_refuses_as_binary_and_searches_later_nul_lines() {
                let filler = "x\n".repeat(4096);
                for (case, contents, expected) in [
                    ("NUL beside the match", "hit\n\0".to_owned(), &[][..]),
                    ("NUL after a short first line", "hi\n\0".to_owned(), &[][..]),
                    (
                        "NUL past the first 8 KiB",
                        format!("hit\n{filler}\0\nhit\n"),
                        &["1:hit", "4099:hit"][..],
                    ),
                ] {
                    let dir = canonical_tempdir();
                    let path = dir.path().join("a.bin");
                    fs::write(&path, contents).unwrap();

                    let (lines, unreadable) = search_lines("hi", 0, &path);

                    assert_eq!(lines, expected, "{case}");
                    assert_eq!(unreadable, [], "{case}");
                }
            }

            #[test]
            fn skips_a_utf16_file_as_binary() {
                let dir = canonical_tempdir();
                let path = dir.path().join("a.txt");
                let text: Vec<u8> = "\u{feff}miss\nhit\n"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect();
                fs::write(&path, text).unwrap();

                let (lines, unreadable) = search_lines("hit", 0, &path);

                assert_eq!(lines, [] as [String; 0]);
                assert_eq!(unreadable, []);
            }

            #[test]
            fn matches_invalid_utf8_as_its_replacement_character() {
                let dir = canonical_tempdir();
                let path = dir.path().join("latin1.txt");
                fs::write(&path, b"caf\xE9 au lait\n").unwrap();

                for pattern in ["caf. au", "caf\u{FFFD}"] {
                    let (lines, unreadable) = search_lines(pattern, 0, &path);

                    assert_eq!(lines, ["1:caf\u{FFFD} au lait"], "{pattern}");
                    assert_eq!(unreadable, [], "{pattern}");
                }
            }

            #[test]
            fn drops_a_utf8_bom_from_the_first_line() {
                let dir = canonical_tempdir();
                let path = dir.path().join("a.txt");
                fs::write(&path, "\u{feff}hit\n").unwrap();

                let (lines, unreadable) = search_lines("^hit", 0, &path);

                assert_eq!(lines, ["1:hit"]);
                assert_eq!(unreadable, []);
            }

            #[test]
            fn records_a_failed_read_but_skips_a_file_that_vanished() {
                let dir = canonical_tempdir();
                let secret = dir.path().join("secret.txt");
                fs::write(&secret, "x").unwrap();
                let _denied = deny_read_access(&secret);

                let (gone_lines, gone_unreadable) =
                    search_lines("x", 0, &dir.path().join("gone.txt"));
                let (secret_lines, secret_unreadable) = search_lines("x", 0, &secret);

                assert_eq!(gone_lines, [] as [String; 0]);
                assert_eq!(gone_unreadable, []);
                let cause = fs::read(&secret).unwrap_err().to_string();
                assert_eq!(secret_lines, [] as [String; 0]);
                assert_eq!(
                    secret_unreadable,
                    [Unreadable {
                        path: Some(secret),
                        error: cause,
                    }]
                );
            }

            #[test]
            fn records_a_file_past_the_size_limit() {
                let dir = canonical_tempdir();
                let path = dir.path().join("big.txt");
                fs::write(&path, vec![b'a'; 1024 * 1024 + 1]).unwrap();

                let (_, unreadable) = search_lines("a", 0, &path);

                assert_eq!(
                    unreadable,
                    [Unreadable {
                        path: Some(path),
                        error: "grew past the 1 MiB size limit".to_owned(),
                    }]
                );
            }

            #[test]
            fn refuses_a_symlink() {
                let dir = canonical_tempdir();
                let target = dir.path().join("target.txt");
                fs::write(&target, "SECRET=1").unwrap();
                let link = dir.path().join("link.txt");
                symlink_file(&target, &link);

                let (lines, unreadable) = search_lines("SECRET", 0, &link);

                #[cfg(unix)]
                let cause =
                    io::Error::from_raw_os_error(nix::errno::Errno::ELOOP as i32).to_string();
                #[cfg(windows)]
                let cause = "the final path component is a symlink".to_owned();
                assert_eq!(lines, Vec::<String>::new());
                assert_eq!(
                    unreadable,
                    [Unreadable {
                        path: Some(link),
                        error: cause,
                    }]
                );
            }

            #[cfg(unix)]
            #[test]
            fn refuses_a_fifo_without_blocking() {
                let dir = canonical_tempdir();
                let path = dir.path().join("fifo");
                nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();

                let (_, unreadable) = search_lines("x", 0, &path);

                assert_eq!(
                    unreadable,
                    [Unreadable {
                        path: Some(path),
                        error: "not a regular file".to_owned(),
                    }]
                );
            }
        }
    }

    mod format_content {
        use super::*;

        #[test]
        fn stops_reading_files_once_the_rows_are_full() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.rs"), "m\nm\n").unwrap();
            let unvisited = dir.path().join("b.rs");
            fs::write(&unvisited, "m\n").unwrap();
            let _denied = deny_read_access(&unvisited);
            let files = [dir.path().join("a.rs"), unvisited];
            let cancel = CancellationToken::new();
            let mut search = Search::new(
                matcher("m", false).unwrap(),
                0,
                MAX_WALK_FILE_SIZE,
                &cancel,
                Vec::new(),
            );

            let output = format_content(&mut search, &files, dir.path(), 1).content;

            assert_eq!(search.unreadable, []);
            assert_eq!(
                output,
                indoc! {"
                    a.rs:1:m

                    (Results limited to 1 lines)"
                },
            );
        }

        #[test]
        fn shows_a_separator_only_with_a_row_after_it() {
            let dir = canonical_tempdir();
            let path = dir.path().join("f.txt");
            fs::write(&path, "a\nx\nx\nx\na\n").unwrap();
            let cancel = CancellationToken::new();

            for (head_limit, expected) in [
                (
                    3,
                    indoc! {"
                        f.txt:1:a
                        f.txt:2-x

                        (Results limited to 3 lines)"
                    },
                ),
                (
                    4,
                    indoc! {"
                        f.txt:1:a
                        f.txt:2-x
                        --
                        f.txt:4-x

                        (Results limited to 4 lines)"
                    },
                ),
            ] {
                let mut search = Search::new(
                    matcher("a", false).unwrap(),
                    1,
                    MAX_WALK_FILE_SIZE,
                    &cancel,
                    Vec::new(),
                );

                let output = format_content(
                    &mut search,
                    std::slice::from_ref(&path),
                    dir.path(),
                    head_limit,
                );

                assert_eq!(output.content, expected, "head_limit {head_limit}");
            }
        }
    }

    mod format_matches {
        use super::*;

        fn match_row(number: usize, text: &str) -> SearchRow {
            SearchRow::Match {
                path: "a.rs".to_owned(),
                line: GrepMatchLine {
                    number,
                    text: text.to_owned(),
                    is_match: true,
                },
            }
        }

        #[test]
        fn names_only_the_limit_that_omitted_a_row() {
            for (limit, expected, truncated) in [
                (3, "a.rs:1:a\na.rs:2:b\na.rs:3:c", false),
                (
                    2,
                    "a.rs:1:a\na.rs:2:b\n\n(Results limited to 2 lines)",
                    true,
                ),
            ] {
                let mut rows = SearchRows::new(limit);
                for (index, text) in ["a", "b", "c"].into_iter().enumerate() {
                    rows.push(match_row(index + 1, text));
                }

                let output = format_matches(rows);

                assert_eq!(output.content, expected, "limit {limit}");
                assert_eq!(output.title, format!("{limit} matches"), "limit {limit}");
                let expected_lines = ["a", "b", "c"]
                    .into_iter()
                    .take(limit)
                    .enumerate()
                    .map(|(index, text)| GrepMatchLine {
                        number: index + 1,
                        text: text.to_owned(),
                        is_match: true,
                    })
                    .collect();
                assert_eq!(
                    output.view,
                    Some(StoredView::View(ToolResultView::GrepMatches {
                        groups: vec![GrepFileGroup {
                            path: "a.rs".to_owned(),
                            lines: expected_lines
                        }],
                        truncated,
                    }))
                );
            }
        }

        #[test]
        fn without_rows_reports_no_matches() {
            let output = format_matches(SearchRows::new(2));

            assert_eq!(output.content, "No matches found");
            assert_eq!(output.title, "No matches found");
            assert_eq!(
                output.view,
                Some(StoredView::View(ToolResultView::GrepMatches {
                    groups: vec![],
                    truncated: false
                }))
            );
        }
    }

    mod truncate_line {
        use super::*;

        #[test]
        fn borrows_a_line_within_the_cap() {
            assert!(matches!(
                truncate_line("hello", MAX_LINE_CHARS),
                Cow::Borrowed("hello")
            ));
        }

        #[test]
        fn long_text_ends_with_ellipsis() {
            let long_line = "x".repeat(MAX_LINE_CHARS + 100);
            let result = truncate_line(&long_line, MAX_LINE_CHARS);
            let suffix = format!("... [{} chars]", MAX_LINE_CHARS + 100);
            assert_eq!(result.len(), MAX_LINE_CHARS + suffix.len());
            assert_eq!(&result[..MAX_LINE_CHARS], "x".repeat(MAX_LINE_CHARS));
            assert_eq!(&result[MAX_LINE_CHARS..], suffix);
        }

        #[test]
        fn multibyte_text_preserves_character_boundaries() {
            let mut line = "a".repeat(MAX_LINE_CHARS - 2);
            line.push('🦀');
            line.push_str(&"b".repeat(100));
            let result = truncate_line(&line, MAX_LINE_CHARS);

            let (prefix, suffix) = result.split_once("... [").unwrap();
            let expected_prefix = "a".repeat(MAX_LINE_CHARS - 2) + "🦀" + "b";
            assert_eq!(prefix, expected_prefix);
            assert_eq!(prefix.chars().count(), MAX_LINE_CHARS);
            assert_eq!(suffix, format!("{} chars]", MAX_LINE_CHARS + 99));
        }

        #[test]
        fn counts_characters_against_the_cap_not_bytes() {
            let line = "é".repeat(MAX_LINE_CHARS);
            let result = truncate_line(&line, MAX_LINE_CHARS);

            assert!(matches!(result, Cow::Borrowed(_)));
            assert_eq!(result.as_ref(), line);
        }
    }

    mod format_files_with_matches {
        use super::*;

        #[test]
        fn stops_reading_after_the_first_omitted_match() {
            let dir = canonical_tempdir();
            let mut files = Vec::new();
            for (name, text) in [
                ("first.rs", "hit\n"),
                ("miss.rs", "absent\n"),
                ("omitted.rs", "hit\n"),
                ("unvisited.rs", "hit\n"),
            ] {
                let path = dir.path().join(name);
                fs::write(&path, text).unwrap();
                files.push(path);
            }
            let _denied = deny_read_access(&files[3]);
            let cancel = CancellationToken::new();
            let mut search = Search::new(
                matcher("hit", false).unwrap(),
                0,
                MAX_WALK_FILE_SIZE,
                &cancel,
                Vec::new(),
            );

            let output = format_files_with_matches(&mut search, &files, dir.path(), 1);

            assert_eq!(search.unreadable, []);
            assert_eq!(output.title, "Found 1 file");
            assert_eq!(
                output.content,
                indoc! {"
                    Found 1 file
                    first.rs

                    (Results limited to 1 files)"
                },
            );
        }
    }

    mod format_count {
        use super::*;

        #[test]
        fn counts_every_file_while_retaining_only_the_requested_prefix() {
            let dir = canonical_tempdir();
            let mut files = Vec::new();
            for (name, text) in [
                ("first.rs", "hit\nhit\n"),
                ("miss.rs", "absent\n"),
                ("second.rs", "hit\n"),
                ("last.rs", "hit\nhit\nhit\n"),
            ] {
                let path = dir.path().join(name);
                fs::write(&path, text).unwrap();
                files.push(path);
            }
            let cancel = CancellationToken::new();
            let mut search = Search::new(
                matcher("hit", false).unwrap(),
                0,
                MAX_WALK_FILE_SIZE,
                &cancel,
                Vec::new(),
            );

            let output = format_count(&mut search, &files, dir.path(), 1);

            assert_eq!(output.title, "Found 6 total occurrences across 3 files");
            assert_eq!(
                output.content,
                indoc! {"
                    Found 6 total occurrences across 3 files
                    first.rs:2

                    (Results limited to 1 files)"
                },
            );
        }

        #[test]
        fn searches_nothing_once_cancelled() {
            let dir = canonical_tempdir();
            let path = dir.path().join("a.rs");
            fs::write(&path, "hit\n").unwrap();
            let cancel = CancellationToken::new();
            cancel.cancel();
            let mut search = Search::new(
                matcher("hit", false).unwrap(),
                0,
                MAX_WALK_FILE_SIZE,
                &cancel,
                Vec::new(),
            );

            let output = format_count(&mut search, &[path], dir.path(), 1);

            assert_eq!(output.content, NO_MATCHES_FOUND);
        }
    }
}
