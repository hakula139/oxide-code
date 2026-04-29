use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{
    ReadExcerptLine, Tool, ToolMetadata, ToolOutput, ToolResultView, display_cwd_path,
    extract_input_field, summarize_path_call,
};

const DEFAULT_LINE_LIMIT: usize = 2000;
/// Per-file size cap for `read` (10 MB). Accommodates typical large
/// config / log files while rejecting accidental binary dumps.
const MAX_READ_FILE_SIZE: u64 = 10 * 1024 * 1024;

pub(crate) struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read the contents of a file with line numbers."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "The line number to start reading from (1-indexed, default: 1)"
                },
                "limit": {
                    "type": "integer",
                    "description": "The number of lines to read (default: 2000)"
                }
            },
            "required": ["file_path"]
        })
    }

    fn icon(&self) -> &'static str {
        "→"
    }

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "file_path")
    }

    fn summarize_call(&self, input: &serde_json::Value) -> String {
        summarize_path_call(self.name(), input, "file_path")
    }

    fn result_view(
        &self,
        input: &serde_json::Value,
        content: &str,
        _metadata: &ToolMetadata,
    ) -> Option<ToolResultView> {
        let path = display_cwd_path(extract_input_field(input, "file_path")?);
        read_excerpt_view(path, content)
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
    file_path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

// ── Execution ──

async fn run(raw: serde_json::Value) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let name = super::file_name(&input.file_path);
    ToolOutput::from_result(read_file(&input.file_path, input.offset, input.limit).await)
        .with_title(format!("Read {name}"))
}

async fn read_file(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Error reading {path}: {e}"))?;

    // Reject non-regular files: pseudo-files like /dev/urandom report
    // len() == 0, bypassing the size gate below, and would stream without
    // bound through tokio::fs::read.
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        return Err(format!(
            "{path} is a directory, not a file. Use the glob tool to list directory contents."
        ));
    }
    if !file_type.is_file() {
        return Err(format!(
            "{path} is not a regular file (fifo, socket, or device); refusing to read.",
        ));
    }

    if metadata.len() > MAX_READ_FILE_SIZE {
        let mb = super::bytes_to_mb(metadata.len());
        let limit_mb = MAX_READ_FILE_SIZE / (1024 * 1024);
        return Err(format!(
            "File is too large ({mb:.1} MB, max {limit_mb} MB). \
             Use offset and limit to read specific portions.",
        ));
    }

    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("Error reading {path}: {e}"))?;

    if super::is_binary(&bytes) {
        return Err("File appears to be binary. Use the bash tool to inspect binary files.".into());
    }

    let text = String::from_utf8_lossy(&bytes);
    let text = strip_bom(&text);

    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    if total_lines == 0 {
        return Ok("(empty file)".into());
    }

    // offset is 1-indexed; 0 is treated as 1.
    let offset = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(DEFAULT_LINE_LIMIT).max(1);
    let start_idx = offset - 1;
    if start_idx >= total_lines {
        return Err(format!(
            "Offset {offset} is beyond the end of the file ({total_lines} lines).",
        ));
    }

    // Per-line cap (truncate_line) and the row cap (limit) keep this
    // bounded; the byte safety net lives in ToolRegistry::run.
    let mut output = String::new();
    let mut num_shown: usize = 0;
    for (i, line) in lines[start_idx..].iter().enumerate().take(limit) {
        let line_num = start_idx + i + 1;
        if !output.is_empty() {
            output.push('\n');
        }
        _ = write!(output, "{line_num}\t{}", super::truncate_line(line));
        num_shown += 1;
    }

    if num_shown < total_lines {
        let last_shown = offset + num_shown - 1;
        _ = write!(
            output,
            "\n\n(Showing lines {offset}–{last_shown} of {total_lines} total)"
        );
    }

    Ok(output)
}

// ── Formatting ──

fn read_excerpt_view(path: String, content: &str) -> Option<ToolResultView> {
    if content.trim() == "(empty file)" {
        return Some(ToolResultView::ReadExcerpt {
            path,
            lines: Vec::new(),
            total_lines: 0,
        });
    }

    let (body, footer) = split_read_footer(content);
    let mut lines = Vec::new();
    for line in body.lines() {
        lines.push(parse_read_line(line)?);
    }

    let total_lines = footer
        .and_then(parse_total_lines_footer)
        .or_else(|| lines.last().map(|line| line.number))?;
    Some(ToolResultView::ReadExcerpt {
        path,
        lines,
        total_lines,
    })
}

/// Splits the read tool's output on its `(Showing lines N–M of TOTAL total)`
/// view-shape footer. The footer is parsed here rather than carried in
/// metadata because the totals are a read-specific signal (line counts,
/// not byte counts); the byte safety net in [`crate::tool::ToolRegistry::run`]
/// uses a different metadata field. When the byte cap fires, the footer
/// is replaced by the truncation separator and this function returns
/// `None` for the footer — the caller falls through to the raw text view.
fn split_read_footer(content: &str) -> (&str, Option<&str>) {
    match content.split_once("\n\n") {
        Some((body, footer)) if footer.starts_with("(Showing lines ") => (body, Some(footer)),
        _ => (content, None),
    }
}

fn parse_read_line(line: &str) -> Option<ReadExcerptLine> {
    let (number, text) = line.split_once('\t')?;
    Some(ReadExcerptLine {
        number: number.parse().ok()?,
        text: text.to_owned(),
    })
}

fn parse_total_lines_footer(footer: &str) -> Option<usize> {
    let (_, total) = footer.split_once(" of ")?;
    total.strip_suffix(" total)")?.parse().ok()
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    // ── run ──

    #[tokio::test]
    async fn run_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();

        let output = run(serde_json::json!({
            "file_path": path.to_str().unwrap()
        }))
        .await;

        assert!(!output.is_error);
        assert_eq!(output.content, "1\thello\n2\tworld");
    }

    #[tokio::test]
    async fn run_missing_file_path() {
        let output = run(serde_json::json!({})).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    // ── read_file ──

    #[tokio::test]
    async fn read_file_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

        let result = read_file(path.to_str().unwrap(), None, None).await.unwrap();
        assert_eq!(result, "1\talpha\n2\tbeta\n3\tgamma");
    }

    #[tokio::test]
    async fn read_file_respects_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();

        let result = read_file(path.to_str().unwrap(), Some(2), Some(2))
            .await
            .unwrap();
        assert_eq!(result, "2\tb\n3\tc\n\n(Showing lines 2–3 of 5 total)");
    }

    #[tokio::test]
    async fn read_file_offset_zero_treated_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "first\nsecond\n").unwrap();

        let result = read_file(path.to_str().unwrap(), Some(0), None)
            .await
            .unwrap();
        assert_eq!(result, "1\tfirst\n2\tsecond");
    }

    #[tokio::test]
    async fn read_file_strips_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom.txt");
        std::fs::write(&path, "\u{feff}hello\n").unwrap();

        let result = read_file(path.to_str().unwrap(), None, None).await.unwrap();
        assert!(result.contains("1\thello"));
        assert!(!result.contains('\u{feff}'));
    }

    #[tokio::test]
    async fn read_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").unwrap();

        let result = read_file(path.to_str().unwrap(), None, None).await.unwrap();
        assert_eq!(result, "(empty file)");
    }

    #[tokio::test]
    async fn read_file_not_found() {
        let err = read_file("/nonexistent/path.txt", None, None)
            .await
            .unwrap_err();
        assert!(err.contains("Error reading"));
    }

    #[tokio::test]
    async fn read_file_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_file(dir.path().to_str().unwrap(), None, None)
            .await
            .unwrap_err();
        assert!(err.contains("is a directory"));
    }

    #[tokio::test]
    async fn read_file_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.txt");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_READ_FILE_SIZE + 1).unwrap();

        let err = read_file(path.to_str().unwrap(), None, None)
            .await
            .unwrap_err();
        assert!(err.contains("too large"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_rejects_non_regular_file() {
        // A unix-domain socket is a non-regular file with `metadata.len() == 0`,
        // which would bypass the size gate if `file_type().is_file()` were skipped.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let err = read_file(path.to_str().unwrap(), None, None)
            .await
            .unwrap_err();
        assert!(
            err.contains("not a regular file"),
            "expected non-regular-file error, got: {err}",
        );
    }

    #[tokio::test]
    async fn read_file_binary_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary.bin");
        std::fs::write(&path, b"hello\x00world").unwrap();

        let err = read_file(path.to_str().unwrap(), None, None)
            .await
            .unwrap_err();
        assert!(err.contains("binary"));
    }

    #[tokio::test]
    async fn read_file_offset_beyond_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "a\nb\n").unwrap();

        let err = read_file(path.to_str().unwrap(), Some(100), None)
            .await
            .unwrap_err();
        assert!(err.contains("beyond the end"));
    }

    // ── result_view ──

    #[test]
    fn result_view_builds_read_excerpt() {
        let cwd = std::env::current_dir().unwrap();
        let path = cwd.join("example.rs");
        let input = serde_json::json!({"file_path": path});
        let view = ReadTool
            .result_view(&input, "10\tfn main() {}\n11\t}", &ToolMetadata::default())
            .unwrap();

        assert_eq!(
            view,
            ToolResultView::ReadExcerpt {
                path: "example.rs".to_owned(),
                lines: vec![
                    ReadExcerptLine {
                        number: 10,
                        text: "fn main() {}".to_owned(),
                    },
                    ReadExcerptLine {
                        number: 11,
                        text: "}".to_owned(),
                    },
                ],
                total_lines: 11,
            },
        );
    }

    #[test]
    fn result_view_preserves_total_lines_from_footer() {
        let input = serde_json::json!({"file_path": "/tmp/example.rs"});
        let view = ReadTool
            .result_view(
                &input,
                indoc! { "\
                    2\tbeta
                    3\tgamma

                    (Showing lines 2–3 of 5 total)" },
                &ToolMetadata::default(),
            )
            .unwrap();

        assert_eq!(
            view,
            ToolResultView::ReadExcerpt {
                path: "/tmp/example.rs".to_owned(),
                lines: vec![
                    ReadExcerptLine {
                        number: 2,
                        text: "beta".to_owned(),
                    },
                    ReadExcerptLine {
                        number: 3,
                        text: "gamma".to_owned(),
                    },
                ],
                total_lines: 5,
            },
        );
    }

    #[test]
    fn result_view_handles_empty_file() {
        let input = serde_json::json!({"file_path": "/tmp/empty.rs"});
        let view = ReadTool
            .result_view(&input, "(empty file)", &ToolMetadata::default())
            .unwrap();

        assert_eq!(
            view,
            ToolResultView::ReadExcerpt {
                path: "/tmp/empty.rs".to_owned(),
                lines: Vec::new(),
                total_lines: 0,
            },
        );
    }

    #[test]
    fn result_view_falls_back_for_malformed_output() {
        let input = serde_json::json!({"file_path": "/tmp/example.rs"});
        assert!(
            ReadTool
                .result_view(&input, "not line-numbered", &ToolMetadata::default())
                .is_none()
        );
    }

    #[test]
    fn result_view_falls_back_for_missing_file_path() {
        let input = serde_json::json!({});
        assert!(
            ReadTool
                .result_view(&input, "1\tline", &ToolMetadata::default())
                .is_none()
        );
    }

    // ── strip_bom ──

    #[test]
    fn strip_bom_removes_bom() {
        assert_eq!(strip_bom("\u{feff}hello"), "hello");
    }

    #[test]
    fn strip_bom_no_bom_unchanged() {
        assert_eq!(strip_bom("hello"), "hello");
    }
}
