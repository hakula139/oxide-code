use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;

use serde::Deserialize;

use super::{Tool, ToolOutput};

const DEFAULT_HEAD_LIMIT: usize = 250;
const MAX_LINE_LENGTH: usize = 500;
const MAX_FILE_SIZE: u64 = 1024 * 1024; // 1 MB — skip large files during search

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
                    "description": "Glob pattern to filter files (e.g. \"*.rs\", \"*.{ts,tsx}\")"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode: \"content\" shows matching lines (default), \"files_with_matches\" shows file paths only, \"count\" shows match counts per file"
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

    fn run(
        &self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(run(input))
    }
}

// ── Input ──

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    output_mode: Option<String>,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    head_limit: Option<usize>,
}

// ── Execution ──

async fn run(raw: serde_json::Value) -> ToolOutput {
    let input: Input = match serde_json::from_value(raw) {
        Ok(v) => v,
        Err(e) => {
            return ToolOutput {
                content: format!("Invalid input: {e}"),
                is_error: true,
            };
        }
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
            output_mode: output_mode.as_deref(),
            context: context.unwrap_or(0),
            case_insensitive,
            head_limit,
        })
    })
    .await
    {
        Ok(Ok(content)) => ToolOutput {
            content,
            is_error: false,
        },
        Ok(Err(msg)) => ToolOutput {
            content: msg,
            is_error: true,
        },
        Err(e) => ToolOutput {
            content: format!("Internal error: {e}"),
            is_error: true,
        },
    }
}

// ── Parameters ──

struct GrepParams<'a> {
    pattern: &'a str,
    search_path: Option<&'a str>,
    include_glob: Option<&'a str>,
    output_mode: Option<&'a str>,
    context: usize,
    case_insensitive: bool,
    head_limit: Option<usize>,
}

// ── Hidden Directory Filter ──

const HIDDEN_DIRS: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj"];

fn is_hidden_dir(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    HIDDEN_DIRS.iter().any(|d| *d == name.as_ref())
}

// ── Search ──

fn grep_files(params: &GrepParams<'_>) -> Result<String, String> {
    let pattern = if params.case_insensitive {
        format!("(?i){}", params.pattern)
    } else {
        params.pattern.to_owned()
    };
    let re = regex::Regex::new(&pattern).map_err(|e| format!("Invalid regex: {e}"))?;

    let cwd =
        std::env::current_dir().map_err(|e| format!("Failed to get working directory: {e}"))?;
    let base = params
        .search_path
        .map_or_else(|| cwd.clone(), std::path::PathBuf::from);

    if !base.exists() {
        return Err(format!("Path does not exist: {}", base.display()));
    }

    let include_pattern = params
        .include_glob
        .map(glob::Pattern::new)
        .transpose()
        .map_err(|e| format!("Invalid include pattern: {e}"))?;

    let mode = params.output_mode.unwrap_or("content");
    let head_limit = match params.head_limit {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => DEFAULT_HEAD_LIMIT,
    };

    let files = collect_files(&base, include_pattern.as_ref());

    Ok(match mode {
        "files_with_matches" => format_files_with_matches(&files, &re, head_limit),
        "count" => format_count(&files, &re, head_limit),
        _ => format_content(&files, &re, params.context, head_limit),
    })
}

fn collect_files(
    base: &std::path::Path,
    include_pattern: Option<&glob::Pattern>,
) -> Vec<std::path::PathBuf> {
    if base.is_file() {
        return vec![base.to_path_buf()];
    }

    let walker = walkdir::WalkDir::new(base)
        .into_iter()
        .filter_entry(|e| !is_hidden_dir(e));

    let mut files = Vec::new();
    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }

        if let Ok(meta) = entry.metadata()
            && meta.len() > MAX_FILE_SIZE
        {
            continue;
        }

        if let Some(pat) = include_pattern {
            let file_name = entry.file_name().to_string_lossy();
            if !pat.matches(&file_name) {
                continue;
            }
        }

        files.push(entry.into_path());
    }

    files
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

fn read_text(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if is_binary(&bytes) {
        return None;
    }
    std::str::from_utf8(&bytes).ok().map(String::from)
}

// ── Content Mode ──

fn format_content(
    files: &[std::path::PathBuf],
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

        let display_path = path.to_string_lossy();

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
        let _ = write!(output, "\n\n(Results limited to {head_limit} lines)");
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
            let mut entry = String::new();
            let _ = write!(entry, "{display_path}:{}:", line_num + 1);
            truncate_into(&mut entry, line);
            output_lines.push(entry);
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

    // Merge overlapping context ranges
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

    // Format with grep-style separators
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
            let sep = if match_indices.contains(&i) { ':' } else { '-' };
            let mut entry = String::new();
            let _ = write!(entry, "{display_path}:{}{sep}", i + 1);
            truncate_into(&mut entry, line);
            output_lines.push(entry);
        }
    }
}

// ── Files-with-Matches Mode ──

fn format_files_with_matches(
    files: &[std::path::PathBuf],
    re: &regex::Regex,
    head_limit: usize,
) -> String {
    let mut matching_files: Vec<(String, SystemTime)> = Vec::new();

    for path in files {
        let Some(text) = read_text(path) else {
            continue;
        };

        if text.lines().any(|line| re.is_match(line)) {
            let mtime = path
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            matching_files.push((path.to_string_lossy().into_owned(), mtime));
        }
    }

    // Sort by mtime descending (newest first)
    matching_files.sort_by(|a, b| b.1.cmp(&a.1));

    if matching_files.is_empty() {
        return "No files found".into();
    }

    let truncated = matching_files.len() > head_limit;
    matching_files.truncate(head_limit);

    let mut output: String = matching_files
        .iter()
        .map(|(p, _)| p.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    if truncated {
        let _ = write!(output, "\n\n(Results limited to {head_limit} files)");
    }

    output
}

// ── Count Mode ──

fn format_count(
    files: &[std::path::PathBuf],
    re: &regex::Regex,
    head_limit: usize,
) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut total_matches: usize = 0;

    for path in files {
        let Some(text) = read_text(path) else {
            continue;
        };

        let count = text.lines().filter(|line| re.is_match(line)).count();
        if count > 0 {
            total_matches += count;
            counts.push((path.to_string_lossy().into_owned(), count));
        }
    }

    if counts.is_empty() {
        return "No matches found".into();
    }

    let total_files = counts.len();
    let truncated = total_files > head_limit;
    counts.truncate(head_limit);

    let mut output: String = counts
        .iter()
        .map(|(p, c)| format!("{p}:{c}"))
        .collect::<Vec<_>>()
        .join("\n");

    let _ = write!(
        output,
        "\n\nFound {total_matches} total {} across {total_files} {}.",
        if total_matches == 1 {
            "occurrence"
        } else {
            "occurrences"
        },
        if total_files == 1 { "file" } else { "files" },
    );

    if truncated {
        let _ = write!(output, " (Results limited to {head_limit} files)");
    }

    output
}

// ── Formatting ──

fn truncate_into(buf: &mut String, line: &str) {
    if line.len() <= MAX_LINE_LENGTH {
        buf.push_str(line);
    } else {
        let boundary = line.floor_char_boundary(MAX_LINE_LENGTH);
        buf.push_str(&line[..boundary]);
        let _ = write!(buf, "... [{} chars]", line.chars().count());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pattern: &str) -> GrepParams<'_> {
        GrepParams {
            pattern,
            search_path: None,
            include_glob: None,
            output_mode: None,
            context: 0,
            case_insensitive: false,
            head_limit: None,
        }
    }

    // ── run ──

    #[tokio::test]
    async fn run_finds_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), "fn main() {}\nfn helper() {}\n").unwrap();

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
        std::fs::write(dir.path().join("a.rs"), "fn foo() {}\nfn bar() {}\n").unwrap();
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
            "hello123\nworld456\nhello789\n",
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
        std::fs::write(dir.path().join("test.txt"), "Hello World\nhello world\n").unwrap();

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
        std::fs::write(dir.path().join("test.txt"), "aaa\nbbb\nccc\nddd\neee\n").unwrap();

        let mut p = params("ccc");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.context = 1;
        let result = grep_files(&p).unwrap();
        // Context line uses `-` separator, match uses `:`
        assert!(result.contains("test.txt:2-bbb"));
        assert!(result.contains("test.txt:3:ccc"));
        assert!(result.contains("test.txt:4-ddd"));
        // Lines outside context range should not appear
        assert!(!result.contains("aaa"));
        assert!(!result.contains("eee"));
    }

    #[test]
    fn grep_files_with_context_merges_adjacent_ranges() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.txt"),
            "a\nb\nMATCH1\nc\nd\nMATCH2\ne\nf\n",
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
            "MATCH1\na\nb\nc\nd\ne\nf\nMATCH2\n",
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
        // Middle lines should not appear
        assert!(!result.contains("test.txt:3"));
        assert!(!result.contains("test.txt:4"));
        assert!(!result.contains("test.txt:5"));
        assert!(!result.contains("test.txt:6"));
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
    fn grep_files_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

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
        let git = dir.path().join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::write(git.join("config"), "match me\n").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "match me\n").unwrap();

        let mut p = params("match");
        p.search_path = Some(dir.path().to_str().unwrap());
        let result = grep_files(&p).unwrap();
        assert!(result.contains("visible.txt"));
        assert!(!result.contains(".git"));
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
        p.output_mode = Some("files_with_matches");
        let result = grep_files(&p).unwrap();
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
        p.output_mode = Some("files_with_matches");
        p.head_limit = Some(2);
        let result = grep_files(&p).unwrap();
        assert!(result.contains("Results limited to 2 files"));
        let file_count = result.lines().filter(|l| l.contains(".txt")).count();
        assert_eq!(file_count, 2);
    }

    // ── grep_files (count mode) ──

    #[test]
    fn grep_files_count_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "aaa\nbbb\naaa\n").unwrap();

        let mut p = params("aaa");
        p.search_path = Some(dir.path().to_str().unwrap());
        p.output_mode = Some("count");
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
        p.output_mode = Some("count");
        p.head_limit = Some(2);
        let result = grep_files(&p).unwrap();
        // Summary should report all 5 files, not just the 2 shown
        assert!(result.contains("5 total occurrences across 5 files"));
        assert!(result.contains("Results limited to 2 files"));
        // Only 2 file lines shown
        let file_lines: Vec<_> = result.lines().filter(|l| l.ends_with(":1")).collect();
        assert_eq!(file_lines.len(), 2);
    }

    // ── grep_files (head_limit) ──

    #[test]
    fn grep_files_head_limit() {
        let dir = tempfile::tempdir().unwrap();
        let content = (0..20).fold(String::new(), |mut s, i| {
            let _ = writeln!(s, "match line {i}");
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

    // ── truncate_into ──

    #[test]
    fn truncate_into_short_unchanged() {
        let mut buf = String::new();
        truncate_into(&mut buf, "hello");
        assert_eq!(buf, "hello");
    }

    #[test]
    fn truncate_into_long_gets_truncated_with_indicator() {
        let long_line = "x".repeat(MAX_LINE_LENGTH + 100);
        let mut buf = String::new();
        truncate_into(&mut buf, &long_line);
        assert!(buf.starts_with(&"x".repeat(MAX_LINE_LENGTH)));
        assert!(buf.ends_with(&format!("[{} chars]", MAX_LINE_LENGTH + 100)));
    }
}
