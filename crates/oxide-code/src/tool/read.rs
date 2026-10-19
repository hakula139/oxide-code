//! File reading tool with line numbers and pagination.

use std::fmt::Write as _;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde::Deserialize;

use super::{
    Prepared, ReadExcerptLine, Tool, ToolCtx, ToolMetadata, ToolOutput, ToolResultView, ToolSpec,
    display_path, extract_input_field, summarize_path_call,
};
use crate::file_tracker::{CACHE_HIT_STUB, LastView, RecordRead};
use crate::permission::PermissionClass;

const DEFAULT_LINE_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;

/// Excerpt bytes one Read emits, leaving room under the dispatcher cap for the footer.
const OUTPUT_BUDGET: usize = super::MAX_OUTPUT_BYTES - 128;

const EMPTY_FILE_MARKER: &str = "(empty file)";

pub(crate) struct ReadTool;

impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".to_owned(),
            description: "Read the contents of a file with line numbers.".to_owned(),
            input_schema: input_schema(),
            class: PermissionClass::Read,
            parallel_safe: true,
        }
    }

    fn path_field(&self) -> Option<&'static str> {
        Some("file_path")
    }

    fn icon(&self) -> &'static str {
        "→"
    }

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "file_path")
    }

    fn summarize_call(&self, name: &str, input: &serde_json::Value, cwd: &Path) -> String {
        summarize_path_call(name, input, "file_path", cwd)
    }

    fn result_view(
        &self,
        input: &serde_json::Value,
        content: &str,
        _metadata: &ToolMetadata,
        cwd: &Path,
    ) -> Option<ToolResultView> {
        let path = display_path(Path::new(extract_input_field(input, "file_path")?), cwd);
        parse_excerpt_view(path, content)
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(run(prepared.input, ctx))
    }
}

// ── Input ──

fn input_schema() -> serde_json::Value {
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

#[derive(Deserialize)]
struct Input {
    file_path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

// ── Execution ──

async fn run(raw: serde_json::Value, ctx: &ToolCtx) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(msg) => return ToolOutput::error(msg),
    };

    match read_file(&input.file_path, input.offset, input.limit, ctx).await {
        Ok(content) => ToolOutput::success(content)
            .with_title(format!("Read {}", super::file_name(&input.file_path))),
        Err(msg) => ToolOutput::error(msg),
    }
}

/// Reads `path`, resolved against the context's cwd, as a `LINENO\tTEXT` excerpt of at most
/// `limit` lines and [`OUTPUT_BUDGET`] bytes, and records the lines shown in the context's tracker
/// for the Read-before-Edit gate. A footer names those lines when the excerpt leaves lines out.
///
/// Refuses directories, non-regular files, files over the tracked size cap, and binary files, and
/// strips a leading UTF-8 BOM.
async fn read_file(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    ctx: &ToolCtx,
) -> Result<String, String> {
    let file_path = ctx.cwd().join(path);
    let (_, bytes) = super::read_validated(&file_path, path, |metadata| {
        super::validate_file_target(path, metadata, "read")
    })
    .await?;

    if super::is_binary(&bytes) {
        return Err(
            "File appears to be binary. Use the bash tool to inspect binary files.".to_owned(),
        );
    }

    let key = super::tracker_key(&file_path).await;

    let text = String::from_utf8_lossy(&bytes);
    let text = strip_bom(&text);

    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    if total_lines == 0 {
        let outcome = ctx
            .tracker
            .record_read(&key, &bytes, LastView::Full, &ctx.tool_use_id);
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
    let outcome = ctx
        .tracker
        .record_read(&key, &bytes, view, &ctx.tool_use_id);
    Ok(stub_or(outcome, &output).to_owned())
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

fn stub_or(outcome: RecordRead, body: &str) -> &str {
    match outcome {
        RecordRead::CacheHit => CACHE_HIT_STUB,
        RecordRead::Inserted => body,
    }
}

// ── Result View ──

fn parse_excerpt_view(path: String, content: &str) -> Option<ToolResultView> {
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

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::file_tracker::{GateError, GatePurpose, MAX_TRACKED_FILE_SIZE};
    use crate::tool::MAX_OUTPUT_BYTES;
    use crate::tool::testing::{canonical_tempdir, ctx, temp_ctx};
    use crate::util::fs::testing::symlink_file;

    mod result_view {
        use super::*;

        #[test]
        fn builds_read_excerpt() {
            let input = serde_json::json!({"file_path": "/repo/example.rs"});
            let view = ReadTool
                .result_view(
                    &input,
                    "10\tfn main() {}\n11\t}",
                    &ToolMetadata::default(),
                    Path::new("/repo"),
                )
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
        fn preserves_total_lines_from_footer() {
            let input = serde_json::json!({"file_path": "/tmp/example.rs"});
            let view = ReadTool
                .result_view(
                    &input,
                    indoc! {"
                        2\tbeta
                        3\tgamma

                        (Showing lines 2–3 of 5 total)"
                    },
                    &ToolMetadata::default(),
                    Path::new("/repo"),
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
        fn handles_empty_file() {
            let input = serde_json::json!({"file_path": "/tmp/empty.rs"});
            let view = ReadTool
                .result_view(
                    &input,
                    "(empty file)",
                    &ToolMetadata::default(),
                    Path::new("/repo"),
                )
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
        fn falls_back_for_malformed_output() {
            let input = serde_json::json!({"file_path": "/tmp/example.rs"});
            assert!(
                ReadTool
                    .result_view(
                        &input,
                        "not line-numbered",
                        &ToolMetadata::default(),
                        Path::new("/repo"),
                    )
                    .is_none()
            );
        }

        #[test]
        fn falls_back_for_missing_file_path() {
            let input = serde_json::json!({});
            assert!(
                ReadTool
                    .result_view(
                        &input,
                        "1\tline",
                        &ToolMetadata::default(),
                        Path::new("/repo"),
                    )
                    .is_none()
            );
        }
    }

    mod run {
        use super::*;

        #[tokio::test]
        async fn existing_file_renders_content() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            std::fs::write(&path, "hello\nworld\n").unwrap();

            let output = run(
                serde_json::json!({"file_path": path.to_str().unwrap()}),
                &temp_ctx(),
            )
            .await;

            assert!(!output.is_error);
            assert_eq!(output.content, "1\thello\n2\tworld");
        }

        #[tokio::test]
        async fn records_full_read_in_tracker() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            std::fs::write(&path, "hello\n").unwrap();
            let ctx = temp_ctx();

            let output = run(
                serde_json::json!({"file_path": path.to_str().unwrap()}),
                &ctx,
            )
            .await;
            assert!(!output.is_error);

            let result = ctx
                .tracker
                .verify_current_content(&path, b"hello\n", GatePurpose::Edit);
            assert_eq!(result, Ok(()));
        }

        #[tokio::test]
        async fn full_reread_is_cache_hit_stub() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            std::fs::write(&path, "hello\nworld\n").unwrap();
            let ctx = temp_ctx();

            let first = run(
                serde_json::json!({"file_path": path.to_str().unwrap()}),
                &ctx,
            )
            .await;
            assert_eq!(first.content, "1\thello\n2\tworld");

            let second = run(
                serde_json::json!({"file_path": path.to_str().unwrap()}),
                &ctx,
            )
            .await;
            assert!(!second.is_error);
            assert_eq!(second.content, CACHE_HIT_STUB);
        }

        #[tokio::test]
        async fn partial_reread_does_not_cache_hit() {
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
            let ctx = temp_ctx();

            let json = serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "offset": 1,
                "limit": 2,
            });
            let first = run(json.clone(), &ctx).await;
            let second = run(json, &ctx).await;
            assert_ne!(second.content, CACHE_HIT_STUB);
            assert_eq!(first.content, second.content);
        }

        #[tokio::test]
        async fn resolves_a_relative_path_against_the_ctx_cwd() {
            let dir = canonical_tempdir();
            std::fs::write(dir.path().join("notes.txt"), "in the ctx cwd\n").unwrap();
            let ctx = ctx(dir.path());

            let output = run(serde_json::json!({"file_path": "notes.txt"}), &ctx).await;

            assert!(!output.is_error, "{}", output.content);
            assert_eq!(output.content, "1\tin the ctx cwd");
            let recorded = ctx.tracker.verify_current_content(
                &dir.path().join("notes.txt"),
                b"in the ctx cwd\n",
                GatePurpose::Edit,
            );
            assert_eq!(recorded, Ok(()));
        }

        #[tokio::test]
        async fn missing_file_path_reports_input_error() {
            let output = run(serde_json::json!({}), &temp_ctx()).await;
            assert!(output.is_error);
            assert!(output.content.contains("Invalid input"));
        }

        #[tokio::test]
        async fn read_error_omits_title() {
            let output = run(
                serde_json::json!({"file_path": "/nonexistent/path.txt"}),
                &temp_ctx(),
            )
            .await;
            assert!(output.is_error);
            assert_eq!(
                output.metadata.title, None,
                "error path must not claim the read happened",
            );
        }
    }

    mod read_file {
        use super::*;

        #[tokio::test]
        async fn existing_file_numbers_lines() {
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

            let result = read_file(path.to_str().unwrap(), None, None, &temp_ctx())
                .await
                .unwrap();
            assert_eq!(
                result,
                indoc! {"
                    1\talpha
                    2\tbeta
                    3\tgamma"
                }
            );
        }

        #[tokio::test]
        async fn respects_offset_and_limit() {
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

            let result = read_file(path.to_str().unwrap(), Some(2), Some(2), &temp_ctx())
                .await
                .unwrap();
            assert_eq!(
                result,
                indoc! {"
                    2\tb
                    3\tc

                    (Showing lines 2–3 of 5 total)"
                }
            );
        }

        #[tokio::test]
        async fn window_covering_every_line_records_full() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            std::fs::write(&path, "a\nb\n").unwrap();
            let ctx = temp_ctx();

            read_file(path.to_str().unwrap(), Some(1), Some(10), &ctx)
                .await
                .unwrap();

            let result = ctx
                .tracker
                .verify_current_content(&path, b"a\nb\n", GatePurpose::Edit);
            assert_eq!(result, Ok(()));
        }

        #[tokio::test]
        async fn default_window_on_long_file_records_partial() {
            let dir = canonical_tempdir();
            let path = dir.path().join("long.txt");
            let content = "x\n".repeat(DEFAULT_LINE_LIMIT + 1);
            std::fs::write(&path, &content).unwrap();
            let ctx = temp_ctx();

            let output = read_file(path.to_str().unwrap(), None, None, &ctx)
                .await
                .unwrap();
            assert!(output.ends_with(&format!("of {} total)", DEFAULT_LINE_LIMIT + 1)));

            let edit =
                ctx.tracker
                    .verify_current_content(&path, content.as_bytes(), GatePurpose::Edit);
            assert_eq!(edit, Ok(()), "any Read of the current content clears Edit");
            let write =
                ctx.tracker
                    .verify_current_content(&path, content.as_bytes(), GatePurpose::Write);
            assert!(
                matches!(write, Err(GateError::PartialRead { .. })),
                "unseen lines keep Write closed: {write:?}",
            );
        }

        #[tokio::test]
        async fn stops_before_the_output_budget_and_records_partial() {
            let dir = canonical_tempdir();
            let path = dir.path().join("wide.txt");
            let content = format!("{}\n", "x".repeat(199)).repeat(1000);
            std::fs::write(&path, &content).unwrap();
            let ctx = temp_ctx();

            let output = read_file(path.to_str().unwrap(), None, None, &ctx)
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
            let write =
                ctx.tracker
                    .verify_current_content(&path, content.as_bytes(), GatePurpose::Write);
            assert!(
                matches!(write, Err(GateError::PartialRead { .. })),
                "lines past the budget keep Write closed: {write:?}",
            );
        }

        #[tokio::test]
        async fn truncates_lines_past_2000_chars() {
            let dir = canonical_tempdir();
            let path = dir.path().join("long_line.txt");
            let fits = "a".repeat(2000);
            let over = "b".repeat(2001);
            std::fs::write(&path, format!("{fits}\n{over}\n")).unwrap();

            let output = read_file(path.to_str().unwrap(), None, None, &temp_ctx())
                .await
                .unwrap();
            assert_eq!(
                output,
                format!("1\t{fits}\n2\t{}... [2001 chars]", &over[..2000]),
            );
        }

        #[tokio::test]
        async fn offset_zero_treated_as_one() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            std::fs::write(&path, "first\nsecond\n").unwrap();

            let result = read_file(path.to_str().unwrap(), Some(0), None, &temp_ctx())
                .await
                .unwrap();
            assert_eq!(result, "1\tfirst\n2\tsecond");
        }

        #[tokio::test]
        async fn strips_bom() {
            let dir = canonical_tempdir();
            let path = dir.path().join("bom.txt");
            std::fs::write(&path, "\u{feff}hello\n").unwrap();

            let result = read_file(path.to_str().unwrap(), None, None, &temp_ctx())
                .await
                .unwrap();
            assert!(result.contains("1\thello"));
            assert!(!result.contains('\u{feff}'));
        }

        #[tokio::test]
        async fn empty_file_renders_marker() {
            let dir = canonical_tempdir();
            let path = dir.path().join("empty.txt");
            std::fs::write(&path, "").unwrap();

            let result = read_file(path.to_str().unwrap(), None, None, &temp_ctx())
                .await
                .unwrap();
            assert_eq!(result, "(empty file)");
        }

        #[tokio::test]
        async fn missing_file_reports_error() {
            let path = "/nonexistent/path.txt";

            let err = read_file(path, None, None, &temp_ctx()).await.unwrap_err();

            let cause = std::fs::metadata(path).unwrap_err();
            assert_eq!(err, format!("Failed to access {path}: {cause}"));
        }

        #[tokio::test]
        async fn directory_reports_error() {
            let dir = canonical_tempdir();
            let err = read_file(dir.path().to_str().unwrap(), None, None, &temp_ctx())
                .await
                .unwrap_err();
            assert!(err.contains("is a directory"));
        }

        #[tokio::test]
        async fn oversized_file_reports_error() {
            let dir = canonical_tempdir();
            let path = dir.path().join("huge.txt");
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(MAX_TRACKED_FILE_SIZE + 1).unwrap();

            let err = read_file(path.to_str().unwrap(), None, None, &temp_ctx())
                .await
                .unwrap_err();
            assert!(
                err.contains("too large") && err.contains("Use the bash tool to read large files"),
                "{err}",
            );
        }

        #[tokio::test]
        async fn binary_content_reports_error() {
            let dir = canonical_tempdir();
            let path = dir.path().join("binary.bin");
            std::fs::write(&path, b"hello\x00world").unwrap();

            let err = read_file(path.to_str().unwrap(), None, None, &temp_ctx())
                .await
                .unwrap_err();
            assert!(err.contains("binary"));
        }

        #[tokio::test]
        async fn offset_beyond_end_reports_error() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            std::fs::write(&path, "a\nb\n").unwrap();

            let err = read_file(path.to_str().unwrap(), Some(100), None, &temp_ctx())
                .await
                .unwrap_err();
            assert!(err.contains("beyond the end"));
        }

        #[tokio::test]
        async fn refuses_a_symlink_at_the_final_component() {
            // The gate pins the resolved path, so a link there was swapped in after the check and
            // its target was never gated.
            let dir = canonical_tempdir();
            let secret = dir.path().join("secret.txt");
            std::fs::write(&secret, "SECRET=1\n").unwrap();
            let path = dir.path().join("notes.md");
            symlink_file(&secret, &path);

            let err = read_file(path.to_str().unwrap(), None, None, &temp_ctx())
                .await
                .unwrap_err();
            assert!(!err.contains("SECRET"), "{err}");
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn rejects_non_regular_file() {
            // A unix socket has `len() == 0` and would bypass the size gate without is_file().
            let dir = canonical_tempdir();
            let path = dir.path().join("sock");
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

            let err = read_file(path.to_str().unwrap(), None, None, &temp_ctx())
                .await
                .unwrap_err();
            assert!(
                err.contains("not a regular file"),
                "expected non-regular-file error, got: {err}",
            );
        }
    }

    mod strip_bom {
        use super::*;

        #[test]
        fn removes_bom() {
            assert_eq!(strip_bom("\u{feff}hello"), "hello");
        }

        #[test]
        fn no_bom_unchanged() {
            assert_eq!(strip_bom("hello"), "hello");
        }
    }
}
