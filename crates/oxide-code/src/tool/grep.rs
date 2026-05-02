use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::Deserialize;

use super::{
    GrepFileGroup, GrepMatchLine, Tool, ToolMetadata, ToolOutput, ToolResultView,
    extract_input_field,
};

const DEFAULT_HEAD_LIMIT: usize = 250;
/// Per-file size cap for `grep` (1 MB). Tighter than other file tools
/// because regex over very large files is the wrong tool — point users
/// at a dedicated streaming search instead.
const MAX_GREP_FILE_SIZE: u64 = 1024 * 1024;

pub(crate) struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents using a regular expression."
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
        // Only content mode produces line-numbered rows; other modes
        // fall through so their summary header stays visible.
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
        Box::pin(run(input))
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

async fn run(raw: serde_json::Value) -> ToolOutput {
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
        grep_files(&GrepParams {
            pattern: &pattern,
            search_path: path.as_deref(),
            include_glob: include.as_deref(),
            output_mode,
            context: context.unwrap_or(0),
            case_insensitive,
            head_limit,
        })
    })
    .await
    {
        Ok(result) => {
            let title = grep_title(result.as_deref().ok());
            ToolOutput::from_result(result).with_title(title)
        }
        Err(e) => ToolOutput {
            content: format!("Internal error: {e}"),
            is_error: true,
            metadata: super::ToolMetadata::default(),
        },
    }
}

fn grep_title(output: Option<&str>) -> String {
    match output {
        Some("No matches found" | "No files found") | None => "No matches found".into(),
        Some(text) => {
            // Count / files-with-matches modes lead with "Found N ...".
            if let Some(line) = text.lines().find(|l| l.starts_with("Found ")) {
                return line.trim_end_matches('.').to_owned();
            }
            // Content mode: count matches only — context lines have the
            // same `path:NUM` prefix but `is_match == false`.
            let match_count = text
                .lines()
                .filter_map(parse_match_line)
                .filter(|(_, m)| m.is_match)
                .count();
            let word = if match_count == 1 { "match" } else { "matches" };
            format!("{match_count} {word}")
        }
    }
}

// ── Search ──

struct GrepParams<'a> {
    pattern: &'a str,
    search_path: Option<&'a str>,
    include_glob: Option<&'a str>,
    output_mode: OutputMode,
    context: usize,
    case_insensitive: bool,
    head_limit: Option<usize>,
}

fn grep_files(params: &GrepParams<'_>) -> Result<String, String> {
    // Bound regex compilation. The default size limit is 10 MB; a pattern
    // like `a{100000}{100000}` would allocate a massive DFA per tool call.
    // 1 MB is plenty for any real-world search expression.
    let re = regex::RegexBuilder::new(params.pattern)
        .case_insensitive(params.case_insensitive)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .map_err(|e| format!("Invalid regex: {e}"))?;

    let base = super::resolve_base_dir(params.search_path)?;
    if !base.exists() {
        return Err(format!("Path does not exist: {}", base.display()));
    }

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

    let collected = collect_files(&base, include_matcher.as_ref());

    let mut result = match params.output_mode {
        OutputMode::FilesWithMatches => {
            format_files_with_matches(&collected.files, &base, &re, head_limit)
        }
        OutputMode::Count => format_count(&collected.files, &base, &re, head_limit),
        OutputMode::Content => {
            format_content(&collected.files, &base, &re, params.context, head_limit)
        }
    };

    append_skipped_warnings(&mut result, &collected.skipped_large, &base);

    Ok(result)
}

struct CollectedFiles {
    files: Vec<PathBuf>,
    skipped_large: Vec<(PathBuf, u64)>,
}

fn collect_files(base: &Path, include_matcher: Option<&globset::GlobMatcher>) -> CollectedFiles {
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
        };
    }

    let mut skipped_large = Vec::new();
    let mut files_with_mtime: Vec<(PathBuf, std::time::SystemTime)> = super::walk_files(base)
        .filter(|entry| {
            include_matcher.is_none_or(|m| {
                let name = entry.file_name().to_string_lossy();
                m.is_match(name.as_ref())
            })
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
    }
}

fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if super::is_binary(&bytes) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn append_skipped_warnings(output: &mut String, skipped: &[(PathBuf, u64)], base: &Path) {
    if skipped.is_empty() {
        return;
    }
    let limit_mb = MAX_GREP_FILE_SIZE / (1024 * 1024);
    _ = write!(output, "\n\nSkipped (exceeds {limit_mb} MB size limit):");
    for (path, size) in skipped {
        let mb = super::bytes_to_mb(*size);
        let display = super::display_path(path, base);
        _ = write!(output, "\n  {display} ({mb:.1} MB)");
    }
}

// ── Content Mode ──

fn format_content(
    files: &[PathBuf],
    base: &Path,
    re: &regex::Regex,
    context: usize,
    head_limit: usize,
) -> String {
    let mut output_lines: Vec<String> = Vec::new();

    for path in files {
        if output_lines.len() >= head_limit {
            break;
        }

        let Some(text) = read_text(path) else {
            continue;
        };

        let display_path = super::display_path(path, base);

        if context == 0 {
            search_no_context(&text, re, &display_path, &mut output_lines, head_limit);
        } else {
            search_with_context(
                &text,
                re,
                &display_path,
                context,
                &mut output_lines,
                head_limit,
            );
        }
    }

    if output_lines.is_empty() {
        return "No matches found".into();
    }

    let truncated = output_lines.len() >= head_limit;
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
    head_limit: usize,
) {
    for (line_num, line) in text.lines().enumerate() {
        if output_lines.len() >= head_limit {
            return;
        }
        if re.is_match(line) {
            let truncated = super::truncate_line(line);
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
    head_limit: usize,
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
        if output_lines.len() >= head_limit {
            return;
        }
        if range_idx > 0 {
            output_lines.push("--".into());
        }
        for (i, line) in lines.iter().enumerate().take(end + 1).skip(start) {
            if output_lines.len() >= head_limit {
                return;
            }
            let sep = if match_indices.binary_search(&i).is_ok() {
                ':'
            } else {
                '-'
            };
            let truncated = super::truncate_line(line);
            output_lines.push(format!("{display_path}:{}{sep}{truncated}", i + 1));
        }
    }
}

// ── Files-with-Matches Mode ──

fn format_files_with_matches(
    files: &[PathBuf],
    base: &Path,
    re: &regex::Regex,
    head_limit: usize,
) -> String {
    let mut matching_files: Vec<String> = Vec::new();

    for path in files {
        if matching_files.len() >= head_limit {
            break;
        }

        let Some(text) = read_text(path) else {
            continue;
        };

        if text.lines().any(|line| re.is_match(line)) {
            matching_files.push(super::display_path(path, base));
        }
    }

    if matching_files.is_empty() {
        return "No files found".into();
    }

    let num_files = matching_files.len();
    let truncated = num_files >= head_limit;
    let file_word = if num_files == 1 { "file" } else { "files" };

    let mut output = format!("Found {num_files} {file_word}\n");
    output.push_str(&matching_files.join("\n"));

    if truncated {
        _ = write!(output, "\n\n(Results limited to {head_limit} files)");
    }

    output
}

// ── Count Mode ──

fn format_count(files: &[PathBuf], base: &Path, re: &regex::Regex, head_limit: usize) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut total_matches: usize = 0;

    for path in files {
        let Some(text) = read_text(path) else {
            continue;
        };

        let count = text.lines().filter(|line| re.is_match(line)).count();
        if count > 0 {
            total_matches += count;
            counts.push((super::display_path(path, base), count));
        }
    }

    if counts.is_empty() {
        return "No matches found".into();
    }

    let total_files = counts.len();
    let truncated = total_files > head_limit;
    counts.truncate(head_limit);

    // Summary first — the renderer's title-strip pass consumes it,
    // leaving a clean `paths` body. Mirrors `format_files_with_matches`.
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

/// Parses content-mode grep output into per-file groups. Returns `None`
/// for any unrecognised line — skipped-file warnings, malformed rows —
/// so the block falls through to the text body and the reader sees raw
/// output instead of a silently truncated render.
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

/// Parses one row of content-mode grep output. `path:NUM:text` is a
/// match; `path:NUM-text` is a context line. Scans colons left-to-right
/// to skip path-internal `:` (e.g., Windows `C:foo`) without digits
/// after.
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

    fn params(pattern: &str) -> GrepParams<'_> {
        GrepParams {
            pattern,
            search_path: None,
            include_glob: None,
            output_mode: OutputMode::Content,
            context: 0,
            case_insensitive: false,
            head_limit: None,
        }
    }

    // ── run ──

    #[tokio::test]
    async fn run_finds_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.rs"),
            indoc! {"
                fn main() {}
                fn helper() {}
            "},
        )
        .unwrap();

        let output = run(serde_json::json!({
            "pattern": "fn main",
            "path": dir.path().to_str().unwrap()
        }))
        .await;

        assert!(!output.is_error);
        assert!(output.content.contains("test.rs:1:fn main()"));
    }

    #[tokio::test]
    async fn run_missing_pattern() {
        let output = run(serde_json::json!({})).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    // ── grep_files (content mode) ──

    #[test]
    fn grep_files_basic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            indoc! {"
                fn foo() {}
                fn bar() {}
            "},
        )
        .unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn baz() {}\n").unwrap();

        let mut p = params("fn foo");
        p.search_path = Some(dir.path().to_str().unwrap());
        let result = grep_files(&p).unwrap();
        assert!(result.contains("a.rs:1:fn foo()"));
        assert!(!result.contains("bar"));
        assert!(!result.contains("baz"));
    }

    #[test]
    fn grep_files_regex() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                hello123
                world456
                hello789
            "},
        )
        .unwrap();

        let mut p = params(r"hello\d+");
        p.search_path = Some(dir.path().to_str().unwrap());
        let result = grep_files(&p).unwrap();
        assert!(result.contains("test.txt:1:hello123"));
        assert!(result.contains("test.txt:3:hello789"));
        assert!(!result.contains("world"));
    }

    #[test]
    fn grep_files_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                Hello World
                hello world
            "},
        )
        .unwrap();

        let mut p = params("hello");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.case_insensitive = true;
        let result = grep_files(&p).unwrap();
        assert!(result.contains("test.txt:1:Hello World"));
        assert!(result.contains("test.txt:2:hello world"));
    }

    #[test]
    fn grep_files_with_context() {
        let dir = tempfile::tempdir().unwrap();
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

        let mut p = params("ccc");
        p.search_path = Some(dir.path().to_str().unwrap());
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
        let dir = tempfile::tempdir().unwrap();
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

        let mut p = params("MATCH");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.context = 1;
        let result = grep_files(&p).unwrap();
        // With context=1, MATCH1 (line 3) shows lines 2-4 and MATCH2 (line 6) shows
        // lines 5-7. Lines 4 and 5 bridge the gap, so the ranges merge into one block
        // with no "--" separator.
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
        let dir = tempfile::tempdir().unwrap();
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

        let mut p = params("MATCH");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.context = 1;
        let result = grep_files(&p).unwrap();
        // MATCH1 (line 1) context=1 → lines 1-2; MATCH2 (line 8) → lines 7-8.
        // Gap between ranges, so a "--" separator should appear.
        assert!(result.contains("--"));
        assert!(result.contains("test.txt:1:MATCH1"));
        assert!(result.contains("test.txt:2-a"));
        assert!(result.contains("test.txt:7-f"));
        assert!(result.contains("test.txt:8:MATCH2"));
        // Middle lines should not appear.
        assert!(!result.contains("test.txt:3"));
        assert!(!result.contains("test.txt:4"));
        assert!(!result.contains("test.txt:5"));
        assert!(!result.contains("test.txt:6"));
    }

    #[test]
    fn grep_files_with_context_no_matches_in_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                aaa
                bbb
                ccc
            "},
        )
        .unwrap();

        let mut p = params("xyz");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.context = 1;
        let result = grep_files(&p).unwrap();
        assert_eq!(result, "No matches found");
    }

    #[test]
    fn grep_files_with_context_respects_head_limit() {
        let dir = tempfile::tempdir().unwrap();
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

        let mut p = params("MATCH");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.context = 1;
        p.head_limit = Some(3);
        let result = grep_files(&p).unwrap();
        assert!(result.contains("Results limited to 3 lines"));
        let entry_count = result.lines().filter(|l| l.contains("test.txt:")).count();
        assert_eq!(entry_count, 3);
    }

    #[test]
    fn grep_files_with_include_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("code.rs"), "fn test() {}\n").unwrap();
        std::fs::write(dir.path().join("readme.md"), "fn test()\n").unwrap();

        let mut p = params("fn test");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.include_glob = Some("*.rs");
        let result = grep_files(&p).unwrap();
        assert!(result.contains("code.rs"));
        assert!(!result.contains("readme.md"));
    }

    #[test]
    fn grep_files_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(
            &file,
            indoc! {"
                alpha
                beta
                gamma
            "},
        )
        .unwrap();

        let mut p = params("beta");
        p.search_path = Some(file.to_str().unwrap());
        let result = grep_files(&p).unwrap();
        assert!(result.contains("test.txt:2:beta"));
    }

    #[test]
    fn grep_files_skips_binary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("binary.bin"), b"match\x00here").unwrap();
        std::fs::write(dir.path().join("text.txt"), "match here\n").unwrap();

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
        let result = grep_files(&p).unwrap();
        assert!(result.contains("text.txt"));
        assert!(!result.contains("binary.bin"));
    }

    #[test]
    fn grep_files_skips_hidden_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("secret.txt"), "match me\n").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "match me\n").unwrap();

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
        let result = grep_files(&p).unwrap();
        assert!(result.contains("visible.txt"));
        assert!(!result.contains(".hidden"));
    }

    #[test]
    fn grep_files_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "match me\n").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "match me\n").unwrap();

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
        let result = grep_files(&p).unwrap();
        assert!(result.contains("tracked.txt"));
        assert!(!result.contains("ignored.txt"));
    }

    #[test]
    fn grep_files_warns_about_skipped_large_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("small.txt"), "match here\n").unwrap();
        let large = dir.path().join("large.txt");
        let f = std::fs::File::create(&large).unwrap();
        f.set_len(MAX_GREP_FILE_SIZE + 1).unwrap();

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
        let result = grep_files(&p).unwrap();
        assert!(result.contains("small.txt:1:match here"));
        assert!(result.contains("Skipped"));
        assert!(result.contains("large.txt"));
    }

    #[test]
    fn grep_files_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello world\n").unwrap();

        let mut p = params("nonexistent");
        p.search_path = Some(dir.path().to_str().unwrap());
        let result = grep_files(&p).unwrap();
        assert_eq!(result, "No matches found");
    }

    #[test]
    fn grep_files_invalid_regex() {
        let err = grep_files(&params("[invalid")).unwrap_err();
        assert!(err.contains("Invalid regex"));
    }

    #[test]
    fn grep_files_nonexistent_path() {
        let mut p = params("test");
        p.search_path = Some("/nonexistent/path");
        let err = grep_files(&p).unwrap_err();
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn grep_files_invalid_include_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = params("test");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.include_glob = Some("[bad");
        let err = grep_files(&p).unwrap_err();
        assert!(err.contains("Invalid include pattern"));
    }

    #[test]
    fn grep_files_single_file_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let large = dir.path().join("large.txt");
        let f = std::fs::File::create(&large).unwrap();
        f.set_len(MAX_GREP_FILE_SIZE + 1).unwrap();

        let mut p = params("match");
        p.search_path = Some(large.to_str().unwrap());
        let result = grep_files(&p).unwrap();
        assert!(result.contains("Skipped"));
        assert!(result.contains("large.txt"));
    }

    // ── grep_title ──

    #[test]
    fn grep_title_with_context_counts_matches_only() {
        // Context lines share the `path:NUM` prefix but `is_match == false`.
        let dir = tempfile::tempdir().unwrap();
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

        let mut p = params("MATCH");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.context = 1;
        let result = grep_files(&p).unwrap();
        assert_eq!(grep_title(Some(&result)), "2 matches");
    }

    // ── grep_files (files_with_matches mode) ──

    #[test]
    fn grep_files_files_with_matches_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn foo() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn bar() {}\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "no match\n").unwrap();

        let mut p = params("fn");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.output_mode = OutputMode::FilesWithMatches;
        let result = grep_files(&p).unwrap();
        assert!(result.starts_with("Found 2 files\n"));
        assert!(result.contains("a.rs"));
        assert!(result.contains("b.rs"));
        assert!(!result.contains("c.txt"));
    }

    #[test]
    fn grep_files_files_with_matches_truncated() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("{i}.txt")), "match\n").unwrap();
        }

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.output_mode = OutputMode::FilesWithMatches;
        p.head_limit = Some(2);
        let result = grep_files(&p).unwrap();
        assert!(result.starts_with("Found 2 files\n"));
        assert!(result.contains("Results limited to 2 files"));
        let file_count = result.lines().filter(|l| l.contains(".txt")).count();
        assert_eq!(file_count, 2);
    }

    #[test]
    fn grep_files_files_with_matches_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn foo() {}\n").unwrap();

        let mut p = params("nonexistent");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.output_mode = OutputMode::FilesWithMatches;
        let result = grep_files(&p).unwrap();
        assert_eq!(result, "No files found");
    }

    // ── grep_files (count mode) ──

    #[test]
    fn grep_files_count_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.txt"),
            indoc! {"
                aaa
                bbb
                aaa
            "},
        )
        .unwrap();

        let mut p = params("aaa");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.output_mode = OutputMode::Count;
        let result = grep_files(&p).unwrap();
        assert!(result.contains("test.txt:2"));
        assert!(result.contains("2 total occurrences"));
    }

    #[test]
    fn grep_files_count_mode_truncated() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("{i}.txt")), "match\n").unwrap();
        }

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
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
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "match once\n").unwrap();

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.output_mode = OutputMode::Count;
        let result = grep_files(&p).unwrap();
        assert!(result.contains("1 total occurrence across 1 file"));
        assert!(!result.contains("occurrences"));
        assert!(!result.contains(" files"));
    }

    #[test]
    fn grep_files_count_mode_summary_first() {
        // Summary heads the body so the renderer's title-strip removes it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "match\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "match\n").unwrap();

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.output_mode = OutputMode::Count;
        let result = grep_files(&p).unwrap();
        assert!(
            result.starts_with("Found 2 total occurrences across 2 files\n"),
            "summary must lead the body: {result:?}",
        );
    }

    // ── grep_files (head_limit) ──

    #[test]
    fn grep_files_head_limit() {
        let dir = tempfile::tempdir().unwrap();
        let content = (0..20).fold(String::new(), |mut s, i| {
            _ = writeln!(s, "match line {i}");
            s
        });
        std::fs::write(dir.path().join("test.txt"), &content).unwrap();

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.head_limit = Some(5);
        let result = grep_files(&p).unwrap();
        let lines: Vec<&str> = result.lines().collect();
        // 5 match lines + blank + truncation notice
        assert!(result.contains("Results limited to 5 lines"));
        assert!(lines.len() <= 8);
    }

    #[test]
    fn grep_files_head_limit_zero_means_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        let content = (0..DEFAULT_HEAD_LIMIT + 10).fold(String::new(), |mut s, i| {
            _ = writeln!(s, "match line {i}");
            s
        });
        std::fs::write(dir.path().join("test.txt"), &content).unwrap();

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.head_limit = Some(0);
        let result = grep_files(&p).unwrap();
        assert!(!result.contains("Results limited"));
        assert!(result.contains(&format!("test.txt:{}:", DEFAULT_HEAD_LIMIT + 1)));
    }

    #[test]
    fn grep_files_head_limit_across_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt"] {
            std::fs::write(dir.path().join(name), "match\nmatch\nmatch\n").unwrap();
        }

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.head_limit = Some(3);
        let result = grep_files(&p).unwrap();
        assert!(result.contains("Results limited to 3 lines"));
        let match_lines: Vec<_> = result.lines().filter(|l| l.contains(":match")).collect();
        assert_eq!(match_lines.len(), 3);
    }

    // ── result_view ──

    #[test]
    fn result_view_content_mode_groups_by_file() {
        let input = serde_json::json!({"pattern": "fn"});
        let view = GrepTool
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
        // files_with_matches and count have a summary header line that
        // the structured GrepMatches shape doesn't model — rendering
        // them through the default text body keeps the "Found N files"
        // / "Found N total occurrences" line visible to the reader.
        for mode in ["files_with_matches", "count"] {
            let input = serde_json::json!({"pattern": "fn", "output_mode": mode});
            let view = GrepTool.result_view(&input, "irrelevant", &ToolMetadata::default());
            assert!(view.is_none(), "mode {mode}: expected None to fall back");
        }
    }

    // ── parse_content_view ──

    #[test]
    fn parse_content_view_no_matches_returns_empty_groups() {
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
        // Groups by path, distinguishes match (`:`) from context (`-`),
        // accepts `--` separators, chains a second file into its own group.
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
        // Skipped-warning text isn't modelled by GrepMatches; fall back
        // so the warning stays visible in the rendered text body.
        let content = indoc! {"
            src/main.rs:1:hit

            Skipped (exceeds 1 MB size limit):
              big.txt (5.0 MB)"
        };
        assert!(parse_content_view(content).is_none());
    }

    #[test]
    fn parse_content_view_falls_back_on_invalid_line() {
        // Any unrecognised row triggers full fallback rather than a
        // partial render that silently drops information.
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
        // Path-internal `:` without digits after (e.g., Windows `C:foo`)
        // is skipped; the scanner accepts the next valid `:NUM` boundary.
        let (path, m) = parse_match_line("C:foo:42:body").unwrap();
        assert_eq!(path, "C:foo");
        assert_eq!(m.number, 42);
        assert_eq!(m.text, "body");
        assert!(m.is_match);
    }

    #[test]
    fn parse_match_line_returns_none_when_shape_does_not_match() {
        // No `:NUM:` / `:NUM-` pattern anywhere → fall through.
        assert!(parse_match_line("not a match line").is_none());
        // `path:` with non-digit content after → fall through.
        assert!(parse_match_line("path:non_digits:text").is_none());
        // `path:NUM` with no separator at all → fall through.
        assert!(parse_match_line("path:42").is_none());
    }
}
