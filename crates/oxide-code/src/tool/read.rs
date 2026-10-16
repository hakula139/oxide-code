//! File reading tool with line numbers and pagination.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;

use super::{
    ReadExcerptLine, Tool, ToolMetadata, ToolOutput, ToolResultView, display_cwd_path,
    extract_input_field, summarize_path_call,
};
use crate::file_tracker::{CACHE_HIT_STUB, FileTracker, LastView, RecordRead};
use crate::permission::PermissionClass;

const DEFAULT_LINE_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;

/// Excerpt bytes one Read emits, leaving room under the dispatcher cap for the footer.
const OUTPUT_BUDGET: usize = super::MAX_OUTPUT_BYTES - 128;

const EMPTY_FILE_MARKER: &str = "(empty file)";

pub(crate) struct ReadTool {
    tracker: Arc<FileTracker>,
}

impl ReadTool {
    pub(crate) fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read the contents of a file with line numbers."
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Read
    }

    fn path_field(&self) -> Option<&'static str> {
        Some("file_path")
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
        let tracker = Arc::clone(&self.tracker);
        Box::pin(run(input, tracker))
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

async fn run(raw: serde_json::Value, tracker: Arc<FileTracker>) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(e) => return e,
    };

    match read_file(&input.file_path, input.offset, input.limit, &tracker).await {
        Ok(content) => ToolOutput::success(content)
            .with_title(format!("Read {}", super::file_name(&input.file_path))),
        Err(msg) => ToolOutput::error(msg),
    }
}

/// Reads `path` as text and returns a tab-separated `LINENO\tTEXT` excerpt.
///
/// Refuses directories, non-regular files (FIFO / socket / device, which can report `len() == 0`
/// and would otherwise bypass the size gate), files larger than `MAX_TRACKED_FILE_SIZE`, and files
/// with a NUL byte in the first 8 KiB. A leading UTF-8 BOM is stripped before line splitting.
///
/// The excerpt stops after `limit` lines or before `OUTPUT_BUDGET` bytes, and its footer names the
/// lines shown. Those lines are recorded with the [`FileTracker`] for the Read-before-Edit gate.
async fn read_file(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    tracker: &FileTracker,
) -> Result<String, String> {
    let (_, bytes) = super::read_validated(path, |metadata| {
        super::validate_file_target(path, metadata, "read")
    })
    .await?;

    if super::is_binary(&bytes) {
        return Err("File appears to be binary. Use the bash tool to inspect binary files.".into());
    }

    let key = super::tracker_key(path).await;

    let text = String::from_utf8_lossy(&bytes);
    let text = strip_bom(&text);

    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    if total_lines == 0 {
        let outcome = tracker.record_read(&key, &bytes, LastView::Full);
        return Ok(stub_or(outcome, EMPTY_FILE_MARKER).to_owned());
    }

    let offset_n = offset.unwrap_or(1).max(1);
    let limit_n = limit.unwrap_or(DEFAULT_LINE_LIMIT).max(1);
    let start_idx = offset_n - 1;
    if start_idx >= total_lines {
        return Err(format!(
            "Offset {offset_n} is beyond the end of the file ({total_lines} lines).",
        ));
    }

    let mut output = String::new();
    let mut num_shown: usize = 0;
    for (i, line) in lines[start_idx..].iter().enumerate().take(limit_n) {
        let line_num = start_idx + i + 1;
        let row_start = output.len();
        if !output.is_empty() {
            output.push('\n');
        }
        _ = write!(
            output,
            "{line_num}\t{}",
            super::truncate_line(line, MAX_LINE_CHARS)
        );
        if output.len() > OUTPUT_BUDGET {
            output.truncate(row_start);
            break;
        }
        num_shown += 1;
    }

    let last_shown = offset_n + num_shown - 1;
    if num_shown < total_lines {
        _ = write!(
            output,
            "\n\n(Showing lines {offset_n}–{last_shown} of {total_lines} total)"
        );
    }

    let view = LastView::lines(offset_n, last_shown, total_lines);
    let outcome = tracker.record_read(&key, &bytes, view);
    Ok(stub_or(outcome, &output).to_owned())
}

fn stub_or(outcome: RecordRead, body: &str) -> &str {
    match outcome {
        RecordRead::CacheHit => CACHE_HIT_STUB,
        RecordRead::Inserted => body,
    }
}

// ── Formatting ──

fn read_excerpt_view(path: String, content: &str) -> Option<ToolResultView> {
    if content.trim() == EMPTY_FILE_MARKER {
        return Some(ToolResultView::ReadExcerpt {
            path,
            lines: Vec::new(),
            total_lines: 0,
        });
    }

    let (body, footer) = split_read_footer(content);
    let lines = body
        .lines()
        .map(parse_read_line)
        .collect::<Option<Vec<_>>>()?;

    let total_lines = footer
        .and_then(parse_total_lines_footer)
        .or_else(|| lines.last().map(|line| line.number))?;
    Some(ToolResultView::ReadExcerpt {
        path,
        lines,
        total_lines,
    })
}

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
    use crate::file_tracker::{GateError, GatePurpose, MAX_TRACKED_FILE_SIZE, testing::tracker};
    use crate::tool::MAX_OUTPUT_BYTES;
    use crate::tool::testing::{canonical_tempdir, symlink_file};

    // ── run ──

    #[tokio::test]
    async fn run_existing_file_renders_content() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();

        let output = run(
            serde_json::json!({"file_path": path.to_str().unwrap()}),
            tracker(),
        )
        .await;

        assert!(!output.is_error);
        assert_eq!(output.content, "1\thello\n2\tworld");
    }

    #[tokio::test]
    async fn run_records_full_read_in_tracker() {
        // A no-args read populates the tracker as Full so a follow-up Edit clears the gate.
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let tracker = tracker();

        let output = run(
            serde_json::json!({"file_path": path.to_str().unwrap()}),
            Arc::clone(&tracker),
        )
        .await;
        assert!(!output.is_error);

        let result = tracker.verify_current_content(&path, b"hello\n", GatePurpose::Edit);
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn run_full_reread_is_cache_hit_stub() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let tracker = tracker();

        let first = run(
            serde_json::json!({"file_path": path.to_str().unwrap()}),
            Arc::clone(&tracker),
        )
        .await;
        assert_eq!(first.content, "1\thello\n2\tworld");

        let second = run(
            serde_json::json!({"file_path": path.to_str().unwrap()}),
            tracker,
        )
        .await;
        assert!(!second.is_error);
        assert_eq!(second.content, CACHE_HIT_STUB);
    }

    #[tokio::test]
    async fn run_partial_reread_does_not_cache_hit() {
        // Partial reads always return the slice so the model can re-request a different range.
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(
            &path,
            indoc! {"
                a
                b
                c
            "},
        )
        .unwrap();
        let tracker = tracker();

        let json = serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "offset": 1,
            "limit": 2,
        });
        let first = run(json.clone(), Arc::clone(&tracker)).await;
        let second = run(json, tracker).await;
        assert_ne!(second.content, CACHE_HIT_STUB);
        assert_eq!(first.content, second.content);
    }

    #[tokio::test]
    async fn run_missing_file_path_reports_input_error() {
        let output = run(serde_json::json!({}), tracker()).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_read_error_omits_title() {
        let output = run(
            serde_json::json!({"file_path": "/nonexistent/path.txt"}),
            tracker(),
        )
        .await;
        assert!(output.is_error);
        assert_eq!(
            output.metadata.title, None,
            "error path must not claim the read happened",
        );
    }

    // ── read_file ──

    #[tokio::test]
    async fn read_file_existing_file_numbers_lines() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(
            &path,
            indoc! {"
                alpha
                beta
                gamma
            "},
        )
        .unwrap();

        let result = read_file(path.to_str().unwrap(), None, None, &FileTracker::default())
            .await
            .unwrap();
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
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(
            &path,
            indoc! {"
                a
                b
                c
                d
                e
            "},
        )
        .unwrap();

        let result = read_file(
            path.to_str().unwrap(),
            Some(2),
            Some(2),
            &FileTracker::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            indoc! {"
                2\tb
                3\tc

                (Showing lines 2–3 of 5 total)"}
        );
    }

    #[tokio::test]
    async fn read_file_window_covering_every_line_records_full() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let tracker = FileTracker::default();

        read_file(path.to_str().unwrap(), Some(1), Some(10), &tracker)
            .await
            .unwrap();

        let result = tracker.verify_current_content(&path, b"a\nb\n", GatePurpose::Edit);
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn read_file_default_window_on_long_file_records_partial() {
        let dir = canonical_tempdir();
        let path = dir.path().join("long.txt");
        let content = "x\n".repeat(DEFAULT_LINE_LIMIT + 1);
        std::fs::write(&path, &content).unwrap();
        let tracker = FileTracker::default();

        let output = read_file(path.to_str().unwrap(), None, None, &tracker)
            .await
            .unwrap();
        assert!(output.ends_with(&format!("of {} total)", DEFAULT_LINE_LIMIT + 1)));

        let edit = tracker.verify_current_content(&path, content.as_bytes(), GatePurpose::Edit);
        assert_eq!(edit, Ok(()), "any Read of the current content clears Edit");
        let write = tracker.verify_current_content(&path, content.as_bytes(), GatePurpose::Write);
        assert!(
            matches!(write, Err(GateError::PartialRead { .. })),
            "unseen lines keep Write closed: {write:?}",
        );
    }

    #[tokio::test]
    async fn read_file_stops_before_the_output_budget_and_records_partial() {
        let dir = canonical_tempdir();
        let path = dir.path().join("wide.txt");
        let content = format!("{}\n", "x".repeat(199)).repeat(1000);
        std::fs::write(&path, &content).unwrap();
        let tracker = FileTracker::default();

        let output = read_file(path.to_str().unwrap(), None, None, &tracker)
            .await
            .unwrap();

        assert!(output.len() <= MAX_OUTPUT_BYTES, "{}", output.len());
        let (body, footer) = output.rsplit_once("\n\n").unwrap();
        assert!(
            body.len() > OUTPUT_BUDGET - 210,
            "the budget is filled: {}",
            body.len()
        );
        let shown = body.lines().count();
        assert_eq!(footer, format!("(Showing lines 1–{shown} of 1000 total)"));
        let write = tracker.verify_current_content(&path, content.as_bytes(), GatePurpose::Write);
        assert!(
            matches!(write, Err(GateError::PartialRead { .. })),
            "lines past the budget keep Write closed: {write:?}",
        );
    }

    #[tokio::test]
    async fn read_file_truncates_lines_past_2000_chars() {
        let dir = canonical_tempdir();
        let path = dir.path().join("long_line.txt");
        let fits = "a".repeat(2000);
        let over = "b".repeat(2001);
        std::fs::write(&path, format!("{fits}\n{over}\n")).unwrap();

        let output = read_file(path.to_str().unwrap(), None, None, &FileTracker::default())
            .await
            .unwrap();
        assert_eq!(
            output,
            format!("1\t{fits}\n2\t{}... [2001 chars]", &over[..2000]),
        );
    }

    #[tokio::test]
    async fn read_file_offset_zero_treated_as_one() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "first\nsecond\n").unwrap();

        let result = read_file(
            path.to_str().unwrap(),
            Some(0),
            None,
            &FileTracker::default(),
        )
        .await
        .unwrap();
        assert_eq!(result, "1\tfirst\n2\tsecond");
    }

    #[tokio::test]
    async fn read_file_strips_bom() {
        let dir = canonical_tempdir();
        let path = dir.path().join("bom.txt");
        std::fs::write(&path, "\u{feff}hello\n").unwrap();

        let result = read_file(path.to_str().unwrap(), None, None, &FileTracker::default())
            .await
            .unwrap();
        assert!(result.contains("1\thello"));
        assert!(!result.contains('\u{feff}'));
    }

    #[tokio::test]
    async fn read_file_empty_file_renders_marker() {
        let dir = canonical_tempdir();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").unwrap();

        let result = read_file(path.to_str().unwrap(), None, None, &FileTracker::default())
            .await
            .unwrap();
        assert_eq!(result, "(empty file)");
    }

    #[tokio::test]
    async fn read_file_missing_file_reports_error() {
        let err = read_file("/nonexistent/path.txt", None, None, &FileTracker::default())
            .await
            .unwrap_err();
        assert!(err.contains("Error reading"));
    }

    #[tokio::test]
    async fn read_file_directory_reports_error() {
        let dir = canonical_tempdir();
        let err = read_file(
            dir.path().to_str().unwrap(),
            None,
            None,
            &FileTracker::default(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("is a directory"));
    }

    #[tokio::test]
    async fn read_file_oversized_file_reports_error() {
        let dir = canonical_tempdir();
        let path = dir.path().join("huge.txt");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_TRACKED_FILE_SIZE + 1).unwrap();

        let err = read_file(path.to_str().unwrap(), None, None, &FileTracker::default())
            .await
            .unwrap_err();
        assert!(
            err.contains("too large") && err.contains("Use the bash tool to read large files"),
            "{err}",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_rejects_non_regular_file() {
        // A unix-domain socket has `len() == 0` and would bypass the size gate without is_file().
        let dir = canonical_tempdir();
        let path = dir.path().join("sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let err = read_file(path.to_str().unwrap(), None, None, &FileTracker::default())
            .await
            .unwrap_err();
        assert!(
            err.contains("not a regular file"),
            "expected non-regular-file error, got: {err}",
        );
    }

    #[tokio::test]
    async fn read_file_binary_content_reports_error() {
        let dir = canonical_tempdir();
        let path = dir.path().join("binary.bin");
        std::fs::write(&path, b"hello\x00world").unwrap();

        let err = read_file(path.to_str().unwrap(), None, None, &FileTracker::default())
            .await
            .unwrap_err();
        assert!(err.contains("binary"));
    }

    #[tokio::test]
    async fn read_file_offset_beyond_end_reports_error() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "a\nb\n").unwrap();

        let err = read_file(
            path.to_str().unwrap(),
            Some(100),
            None,
            &FileTracker::default(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("beyond the end"));
    }

    #[tokio::test]
    async fn read_file_refuses_a_symlink_at_the_final_component() {
        // The gate pins the resolved path, so a link there was swapped in after the check and its
        // target was never gated.
        let dir = canonical_tempdir();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "SECRET=1\n").unwrap();
        let path = dir.path().join("notes.md");
        symlink_file(&secret, &path);

        let err = read_file(path.to_str().unwrap(), None, None, &FileTracker::default())
            .await
            .unwrap_err();
        assert!(!err.contains("SECRET"), "{err}");
    }

    // ── result_view ──

    #[test]
    fn result_view_builds_read_excerpt() {
        let cwd = std::env::current_dir().unwrap();
        let path = cwd.join("example.rs");
        let input = serde_json::json!({"file_path": path});
        let view = ReadTool::new(tracker())
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
        let view = ReadTool::new(tracker())
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
        let view = ReadTool::new(tracker())
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
            ReadTool::new(tracker())
                .result_view(&input, "not line-numbered", &ToolMetadata::default())
                .is_none()
        );
    }

    #[test]
    fn result_view_falls_back_for_missing_file_path() {
        let input = serde_json::json!({});
        assert!(
            ReadTool::new(tracker())
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
