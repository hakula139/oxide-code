use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolOutput};

const DEFAULT_LINE_LIMIT: usize = 2000;
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB
/// Cap on the formatted output size. Prevents a single minified line from
/// flooding the context window. Roughly 32K tokens at ~4 chars / token.
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

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

    ToolOutput::from_result(read_file(&input.file_path, input.offset, input.limit).await)
}

async fn read_file(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Error reading {path}: {e}"))?;

    if metadata.is_dir() {
        return Err(format!(
            "{path} is a directory, not a file. Use the glob tool to list directory contents."
        ));
    }

    if metadata.len() > MAX_FILE_SIZE {
        return Err(format!(
            "File is too large ({} bytes, max {MAX_FILE_SIZE}). \
             Use offset and limit to read specific portions.",
            metadata.len(),
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

    // offset is 1-indexed; 0 is treated as 1
    let offset = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(DEFAULT_LINE_LIMIT).max(1);
    let start_idx = offset - 1;

    if start_idx >= total_lines {
        return Err(format!(
            "Offset {offset} is beyond the end of the file ({total_lines} lines).",
        ));
    }

    // Width for line-number column: based on the last possible line we might show
    let last_possible_line = total_lines.min(start_idx + limit);
    let width = last_possible_line.to_string().len().max(1);

    // The byte budget prevents a single minified line from flooding context.
    let mut output = String::new();
    let mut num_shown: usize = 0;
    let mut truncated_by_bytes = false;

    for (i, line) in lines[start_idx..].iter().enumerate().take(limit) {
        let line_num = start_idx + i + 1;
        let truncated = super::truncate_line(line);

        // separator + line_number + tab + content
        let entry_len = 1 + width + 1 + truncated.len();
        if !output.is_empty() && output.len() + entry_len > MAX_OUTPUT_BYTES {
            truncated_by_bytes = true;
            break;
        }

        if !output.is_empty() {
            output.push('\n');
        }
        _ = write!(output, "{line_num:>width$}\t{truncated}");
        num_shown += 1;
    }

    if num_shown < total_lines || truncated_by_bytes {
        let last_shown = offset + num_shown - 1;
        _ = write!(
            output,
            "\n\n(Showing lines {offset}\u{2013}{last_shown} of {total_lines} total)"
        );
    }

    Ok(output)
}

// ── Formatting ──

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
        assert_eq!(
            output.content,
            indoc! {"
                1\thello
                2\tworld"}
        );
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
        assert_eq!(
            result,
            indoc! {"
                1\talpha
                2\tbeta
                3\tgamma"}
        );
    }

    #[tokio::test]
    async fn read_file_respects_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();

        let result = read_file(path.to_str().unwrap(), Some(2), Some(2))
            .await
            .unwrap();
        assert_eq!(
            result,
            indoc! {"
                2\tb
                3\tc

                (Showing lines 2\u{2013}3 of 5 total)"}
        );
    }

    #[tokio::test]
    async fn read_file_offset_zero_treated_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "first\nsecond\n").unwrap();

        let result = read_file(path.to_str().unwrap(), Some(0), None)
            .await
            .unwrap();
        assert_eq!(
            result,
            indoc! {"
                1\tfirst
                2\tsecond"}
        );
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
    async fn read_file_byte_budget_truncates_large_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let line = "x".repeat(600);
        let content = std::iter::repeat_n(format!("{line}\n"), 500).collect::<String>();
        std::fs::write(&path, &content).unwrap();

        let result = read_file(path.to_str().unwrap(), None, None).await.unwrap();

        assert!(result.len() < MAX_OUTPUT_BYTES + 200);
        assert!(result.contains("Showing lines"));
        assert!(!result.contains("500\t"));
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
        f.set_len(MAX_FILE_SIZE + 1).unwrap();

        let err = read_file(path.to_str().unwrap(), None, None)
            .await
            .unwrap_err();
        assert!(err.contains("too large"));
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
