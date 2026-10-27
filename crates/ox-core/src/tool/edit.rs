//! Exact string replacement tool for file editing.

use std::borrow::Cow;
use std::path::Path;
use std::pin::Pin;

use serde::Deserialize;

use super::file::FileVerb;
use super::view::{
    ApprovalBody, ApprovalPreview, DiffChunk, EditDiff, NumberedLine, ToolResultView,
};
use super::{Prepared, Tool, ToolCtx, ToolMetadata, ToolOutput, ToolSpec, extract_input_field};
use crate::file_tracker::{GatePurpose, LastView};
use crate::permission::PermissionClass;

pub(crate) struct EditTool;

impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".to_owned(),
            description: "Perform exact string replacement in a file. \
                          The old_string must be unique in the file unless replace_all is true. \
                          Read the file with the `read` tool in this session before editing it. \
                          A `read` of the lines being changed is enough. \
                          An edit also fails for a file that changed externally \
                          since the last `read`."
                .to_owned(),
            input_schema: input_schema(),
            class: PermissionClass::Edit,
            parallel_safe: false,
        }
    }

    fn path_field(&self) -> Option<&'static str> {
        Some("file_path")
    }

    fn icon(&self) -> &'static str {
        "✎"
    }

    fn result_view(
        &self,
        input: &serde_json::Value,
        _content: &str,
        metadata: &ToolMetadata,
        _cwd: &Path,
    ) -> Option<ToolResultView> {
        if let Some(diff) = &metadata.diff {
            return Some(ToolResultView::Diff(diff.clone()));
        }
        let old = input.get("old_string")?.as_str()?;
        let new = input.get("new_string")?.as_str()?;
        Some(ToolResultView::Diff(synthesize_diff(old, new)))
    }

    fn approval_preview<'a>(
        &'a self,
        name: &'a str,
        input: &'a serde_json::Value,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = ApprovalPreview> + Send + 'a>> {
        let old = extract_input_field(input, "old_string").unwrap_or_default();
        let new = extract_input_field(input, "new_string").unwrap_or_default();
        Box::pin(std::future::ready(ApprovalPreview {
            title: self.summarize_call(name, input, cwd),
            body: ApprovalBody::Diff(synthesize_chunk(old, new)),
        }))
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
                "description": "The absolute path to the file to modify"
            },
            "old_string": {
                "type": "string",
                "description": "The exact text to replace \
                                (must be unique unless replace_all is true)"
            },
            "new_string": {
                "type": "string",
                "description": "The text to replace it with (must differ from old_string)"
            },
            "replace_all": {
                "type": "boolean",
                "description": "Replace all occurrences (default: false)"
            }
        },
        "required": ["file_path", "old_string", "new_string"]
    })
}

#[derive(Deserialize)]
struct Input {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

// ── Execution ──

async fn run(raw: serde_json::Value, ctx: &ToolCtx) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(msg) => return ToolOutput::error(msg),
    };

    let path = &input.file_path;
    match edit_file(
        path,
        &input.old_string,
        &input.new_string,
        input.replace_all,
        ctx,
    )
    .await
    {
        Ok(Edited {
            replacements, diff, ..
        }) => {
            let content = match replacements {
                1 => format!("Successfully edited {path}."),
                n => format!("Replaced {n} occurrences in {path}."),
            };
            ToolOutput::success(content)
                .with_title(format!("Edited {}", super::file_name(path)))
                .with_diff(diff)
        }
        Err(msg) => ToolOutput::error(msg),
    }
}

/// Replaces `old_string` in the file at `path`, resolved against the context's cwd.
///
/// - `old_string` must occur verbatim, and exactly once without `replace_all`, so the model cannot
///   edit the wrong site.
/// - The Read-before-Edit gate refuses a file whose current content has not been read.
/// - Matching runs on LF-normalized text, and the write restores the file's dominant line ending.
async fn edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    ctx: &ToolCtx,
) -> Result<Edited, String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty.".to_owned());
    }

    if old_string == new_string {
        return Err("old_string and new_string are identical. No changes to make.".to_owned());
    }

    let file_path = ctx.cwd().join(path);
    let (_, content_bytes) = super::file::read_validated(&file_path, path, FileVerb::Edit).await?;
    let key = super::file::tracker_key(&file_path).await;
    ctx.tracker
        .verify_current_content(&key, &content_bytes, GatePurpose::Edit)
        .map_err(|e| e.to_string())?;

    let replacement = Replacement {
        path: path.to_owned(),
        old_string: old_string.to_owned(),
        new_string: new_string.to_owned(),
        replace_all,
    };
    let (expected, updated, edited, view) = super::run_blocking(move || {
        let (updated, edited, view) = replacement.apply(&content_bytes)?;
        Ok((content_bytes, updated, edited, view))
    })
    .await?;

    ctx.tracker
        .write_and_record(
            &ctx.tool_use_id,
            view,
            super::file::write_file(
                file_path,
                path.to_owned(),
                updated.into_bytes(),
                Some(expected),
            ),
        )
        .await
        .map_err(|error| format!("{error:#}"))?;
    Ok(edited)
}

/// What [`edit_file`] changed.
#[derive(Debug)]
struct Edited {
    replacements: usize,
    diff: EditDiff,
}

/// One edit call's replacement, which runs on the blocking pool since a `replace_all` over a large
/// file is CPU-bound.
struct Replacement {
    /// The path as the call gave it, for error messages.
    path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
}

impl Replacement {
    /// The file's updated text, what changed, and the written-line view the tracker records, given
    /// its current `content`.
    fn apply(&self, content: &[u8]) -> Result<(String, Edited, LastView), String> {
        let path = &self.path;
        let content =
            std::str::from_utf8(content).map_err(|e| format!("Failed to read {path}: {e}"))?;

        let eol = dominant_eol(content);
        let content = normalize_eol(content);
        let old_string = normalize_eol(&self.old_string);
        let new_string = normalize_eol(&self.new_string);

        let replacements = content.matches(old_string.as_ref()).count();
        if replacements == 0 {
            return Err(format!(
                "Found no occurrence of old_string in {path}. \
                 Copy it exactly from the file, including whitespace and indentation."
            ));
        }

        if replacements > 1 && !self.replace_all {
            return Err(format!(
                "Found {replacements} occurrences of old_string in {path}. \
                 Set replace_all to true to replace them all, \
                 or add context to make old_string unique."
            ));
        }

        let (diff, written) = build_diff(&content, &old_string, &new_string);
        let updated = content.replace(old_string.as_ref(), new_string.as_ref());
        let view = LastView::from_ranges(written, updated.lines().count());
        Ok((apply_eol(updated, eol), Edited { replacements, diff }, view))
    }
}

// ── Diff Production ──

/// The diff of replacing every `old_string` in `original` with `new_string`, counted in one pass
/// over `original`, and the new-content lines each replacement writes. Old-side numbers count lines
/// of `original`, and new-side numbers count lines of the edited text.
fn build_diff(
    original: &str,
    old_string: &str,
    new_string: &str,
) -> (EditDiff, Vec<(usize, usize)>) {
    let hunk = synthesize_chunk(old_string, new_string);
    let old_breaks = old_string.matches('\n').count();
    let new_breaks = new_string.matches('\n').count();
    let writes_lines = !new_string.is_empty();
    let written_breaks = new_breaks - usize::from(new_string.ends_with('\n'));

    let mut first = None;
    let mut locations = Vec::new();
    let mut written = Vec::new();
    let (mut old_line, mut new_line, mut scanned) = (1, 1, 0);
    for (pos, _) in original.match_indices(old_string) {
        let breaks = original[scanned..pos].matches('\n').count();
        old_line += breaks;
        new_line += breaks;

        first.get_or_insert((old_line, new_line));
        locations.extend(anchor_line(&hunk, old_line, new_line));
        if writes_lines {
            written.push((new_line, new_line + written_breaks));
        }

        old_line += old_breaks;
        new_line += new_breaks;
        scanned = pos + old_string.len();
    }

    let (old_line, new_line) = first.unwrap_or((1, 1));
    (
        EditDiff {
            chunk: shift_chunk(hunk, old_line, new_line),
            locations,
        },
        written,
    )
}

/// The first removed line, or first added line for an insertion, offset from the hunk's 1-based
/// numbering by `old_line` or `new_line`. `None` for an empty hunk.
fn anchor_line(hunk: &DiffChunk, old_line: usize, new_line: usize) -> Option<usize> {
    let old = hunk.old.first().map(|line| line.number + old_line - 1);
    old.or_else(|| hunk.new.first().map(|line| line.number + new_line - 1))
}

/// `hunk`, numbered from line 1 on both sides, renumbered to start at `old_line` and `new_line`.
fn shift_chunk(mut hunk: DiffChunk, old_line: usize, new_line: usize) -> DiffChunk {
    for line in &mut hunk.old {
        line.number += old_line - 1;
    }
    for line in &mut hunk.new {
        line.number += new_line - 1;
    }
    hunk
}

fn split_into_diff_lines(s: &str, start_line: usize) -> Vec<NumberedLine> {
    s.lines()
        .enumerate()
        .map(|(i, text)| NumberedLine {
            number: start_line + i,
            text: text.to_owned(),
        })
        .collect()
}

fn trim_chunk(chunk: &mut DiffChunk) {
    let (prefix, suffix) = {
        let old_text: Vec<&str> = chunk.old.iter().map(|l| l.text.as_str()).collect();
        let new_text: Vec<&str> = chunk.new.iter().map(|l| l.text.as_str()).collect();
        common_boundaries(&old_text, &new_text)
    };
    chunk.old.truncate(chunk.old.len() - suffix);
    chunk.new.truncate(chunk.new.len() - suffix);
    chunk.old.drain(..prefix);
    chunk.new.drain(..prefix);
}

fn common_boundaries<T: Eq>(old: &[T], new: &[T]) -> (usize, usize) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    (prefix, suffix)
}

// ── Line Endings ──

fn dominant_eol(content: &str) -> &'static str {
    let crlf = content.matches("\r\n").count();
    let lf_only = content.matches('\n').count() - crlf;
    if crlf > lf_only { "\r\n" } else { "\n" }
}

fn normalize_eol(content: &str) -> Cow<'_, str> {
    if content.contains("\r\n") {
        Cow::Owned(content.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(content)
    }
}

fn apply_eol(content: String, eol: &str) -> String {
    if eol == "\r\n" {
        content.replace('\n', "\r\n")
    } else {
        content
    }
}

// ── Result View ──

/// The diff of replacing `old` with `new`, numbered from line 1, for a call that recorded none.
pub(super) fn synthesize_diff(old: &str, new: &str) -> EditDiff {
    let chunk = synthesize_chunk(old, new);
    let locations = anchor_line(&chunk, 1, 1).into_iter().collect();
    EditDiff { chunk, locations }
}

/// The boundary-trimmed hunk of replacing `old` with `new`, numbered from line 1 on both sides.
pub(super) fn synthesize_chunk(old: &str, new: &str) -> DiffChunk {
    let mut chunk = DiffChunk {
        old: split_into_diff_lines(old, 1),
        new: split_into_diff_lines(new, 1),
    };
    trim_chunk(&mut chunk);
    chunk
}

#[cfg(test)]
mod tests {
    use std::fs;

    use indoc::indoc;

    use super::*;
    use crate::file_tracker::testing::{tracker, tracker_seeded};
    use crate::file_tracker::{CACHE_HIT_STUB, LastView, MAX_TRACKED_FILE_SIZE};
    use crate::tool::local_testing::{self, ctx_with, temp_ctx};
    use crate::tool::read::ReadTool;
    use crate::tool::write::WriteTool;
    use crate::util::path::local_testing::canonical_tempdir;

    fn seeded_ctx(path: &Path) -> ToolCtx {
        ctx_with(&std::env::temp_dir(), tracker_seeded(path))
    }

    fn line(number: usize, text: &str) -> NumberedLine {
        NumberedLine {
            number,
            text: text.to_owned(),
        }
    }

    mod result_view {
        use super::*;

        #[test]
        fn prefers_the_recorded_diff() {
            let input = serde_json::json!({
                "file_path": "/tmp/f.rs",
                "old_string": "a",
                "new_string": "b",
                "replace_all": true,
            });
            let diff = EditDiff {
                chunk: DiffChunk {
                    old: vec![line(12, "a")],
                    new: vec![line(12, "b")],
                },
                locations: vec![12, 47],
            };
            let metadata = ToolMetadata {
                diff: Some(diff.clone()),
                ..ToolMetadata::default()
            };

            let view = EditTool.result_view(
                &input,
                "Replaced 2 occurrences in /tmp/f.rs.",
                &metadata,
                Path::new("/repo"),
            );

            assert_eq!(view, Some(ToolResultView::Diff(diff)));
        }

        #[test]
        fn without_a_recorded_diff_synthesizes_one_from_the_input() {
            let input = serde_json::json!({
                "file_path": "/tmp/f.rs",
                "old_string": "fn foo()",
                "new_string": "fn bar()",
            });

            let view = EditTool.result_view(
                &input,
                "Successfully edited /tmp/f.rs.",
                &ToolMetadata::default(),
                Path::new("/repo"),
            );

            assert_eq!(
                view,
                Some(ToolResultView::Diff(EditDiff {
                    chunk: DiffChunk {
                        old: vec![line(1, "fn foo()")],
                        new: vec![line(1, "fn bar()")],
                    },
                    locations: vec![1],
                })),
            );
        }

        #[test]
        fn missing_or_mistyped_required_inputs_uses_plain_output() {
            for input in [
                serde_json::json!({"old_string": "x"}),
                serde_json::json!({"file_path": "/tmp/x"}),
                serde_json::json!({"file_path": "/tmp/x", "old_string": 42, "new_string": "b"}),
                serde_json::json!({"file_path": "/tmp/x", "old_string": "a", "new_string": 42}),
            ] {
                assert_eq!(
                    EditTool.result_view(
                        &input,
                        "edited",
                        &ToolMetadata::default(),
                        Path::new("/repo")
                    ),
                    None,
                    "{input}",
                );
            }
        }
    }

    mod approval_preview {
        use super::*;

        #[tokio::test]
        async fn renders_old_and_new_as_a_diff() {
            let tool = EditTool;
            let preview = tool
                .approval_preview(
                    "edit",
                    &serde_json::json!({
                        "file_path": "/repo/src/main.rs",
                        "old_string": "let a = 1;",
                        "new_string": "let a = 2;",
                    }),
                    Path::new("/repo"),
                )
                .await;

            assert_eq!(preview.title, "Edit(src/main.rs)");
            assert_eq!(
                preview.body,
                ApprovalBody::Diff(DiffChunk {
                    old: vec![line(1, "let a = 1;")],
                    new: vec![line(1, "let a = 2;")],
                }),
            );
        }
    }

    mod run {
        use super::*;

        #[tokio::test]
        async fn valid_edit_updates_file() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "hello world").unwrap();

            let input = serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "old_string": "hello",
                "new_string": "goodbye"
            });
            let ctx = seeded_ctx(&path);

            let output = local_testing::run(&EditTool, input, &ctx).await;

            assert!(!output.is_error);
            assert_eq!(
                output.content,
                format!("Successfully edited {}.", path.display())
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "goodbye world");
            assert_eq!(
                output.metadata.title.as_deref(),
                Some("Edited test.txt"),
                "success path attaches the Edited title",
            );
        }

        #[tokio::test]
        async fn replace_all_reports_every_replaced_occurrence() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "aaa bbb aaa").unwrap();
            let input = serde_json::json!({
                "file_path": path,
                "old_string": "aaa",
                "new_string": "ccc",
                "replace_all": true,
            });
            let ctx = seeded_ctx(&path);

            let output = run(input, &ctx).await;

            assert!(!output.is_error);
            assert_eq!(
                output.content,
                format!("Replaced 2 occurrences in {}.", path.display())
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "ccc bbb ccc");
        }

        #[tokio::test]
        async fn replace_all_with_a_single_match_reports_an_edit() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "hello world").unwrap();
            let input = serde_json::json!({
                "file_path": path,
                "old_string": "hello",
                "new_string": "goodbye",
                "replace_all": true,
            });
            let ctx = seeded_ctx(&path);

            let output = run(input, &ctx).await;

            assert!(!output.is_error);
            assert_eq!(
                output.content,
                format!("Successfully edited {}.", path.display())
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "goodbye world");
        }

        #[tokio::test]
        async fn missing_required_fields_reports_input_error() {
            let output = run(
                serde_json::json!({
                    "file_path": "/tmp/x",
                    "old_string": "a"
                }),
                &temp_ctx(),
            )
            .await;

            assert!(output.is_error);
            assert_eq!(output.content, "Invalid input: missing field `new_string`");
        }

        #[tokio::test]
        async fn edit_error_omits_edited_title() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "hello world").unwrap();

            let output = run(
                serde_json::json!({
                    "file_path": path.to_str().unwrap(),
                    "old_string": "not present",
                    "new_string": "x",
                }),
                &seeded_ctx(&path),
            )
            .await;

            assert!(output.is_error);
            assert_eq!(
                output.metadata.title, None,
                "error path must not claim the edit happened",
            );
        }
    }

    mod edit_file {
        use super::*;

        #[tokio::test]
        async fn a_hard_link_refusal_keeps_content_and_tracker_unchanged() {
            let dir = canonical_tempdir();
            let path = dir.path().join("original.txt");
            let alias = dir.path().join("alias.txt");
            fs::write(&path, "original").unwrap();
            fs::hard_link(&path, &alias).unwrap();
            let ctx = seeded_ctx(&path);
            let before = ctx.tracker.snapshot_all();

            let error = edit_file(path.to_str().unwrap(), "original", "changed", false, &ctx)
                .await
                .unwrap_err();

            assert!(error.contains("hard links"), "{error}");
            assert_eq!(fs::read_to_string(&path).unwrap(), "original");
            assert_eq!(fs::read_to_string(&alias).unwrap(), "original");
            assert_eq!(ctx.tracker.snapshot_all(), before);
        }

        #[tokio::test]
        async fn accepts_a_read_under_another_spelling_of_the_path() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join("sub")).unwrap();
            let path = dir.path().join("test.txt");
            fs::write(&path, "hello world").unwrap();
            let ctx = temp_ctx();

            let read =
                local_testing::run(&ReadTool, serde_json::json!({"file_path": path}), &ctx).await;
            assert!(!read.is_error, "{}", read.content);

            let detour = dir.path().join("sub").join("..").join("test.txt");

            let result = edit_file(detour.to_str().unwrap(), "hello", "goodbye", false, &ctx).await;

            result.unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "goodbye world");
        }

        #[tokio::test]
        async fn accepts_a_partial_read_of_current_content() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "hello world\nsecond line\n").unwrap();
            let bytes = fs::read(&path).unwrap();
            let ctx = temp_ctx();
            ctx.tracker
                .record_read(&path, &bytes, LastView::lines(1, 1, 2), "toolu_test");

            let result = edit_file(path.to_str().unwrap(), "hello", "goodbye", false, &ctx).await;

            result.unwrap();
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                "goodbye world\nsecond line\n",
            );
        }

        #[tokio::test]
        async fn an_edit_after_a_partial_read_keeps_a_full_read_from_cache_hitting() {
            let dir = canonical_tempdir();
            let path = dir.path().join("short.txt");
            let content = "line 1\nline 2\nline 3\n";
            fs::write(&path, content).unwrap();
            let ctx = ctx_with(dir.path(), tracker());

            let window = local_testing::run(
                &ReadTool,
                serde_json::json!({"file_path": path, "offset": 1, "limit": 1}),
                &ctx,
            )
            .await;
            assert!(!window.is_error, "{}", window.content);
            let edit = local_testing::run(
                &EditTool,
                serde_json::json!({
                    "file_path": path,
                    "old_string": "line 1\n",
                    "new_string": "line 1 changed\n",
                }),
                &ctx,
            )
            .await;
            assert!(!edit.is_error, "{}", edit.content);

            let full =
                local_testing::run(&ReadTool, serde_json::json!({"file_path": path}), &ctx).await;

            assert!(!full.is_error, "{}", full.content);
            assert_ne!(
                full.content, CACHE_HIT_STUB,
                "the edit changed a line the earlier read showed",
            );
            assert!(full.content.contains("line 3"), "{}", full.content);
        }

        #[tokio::test]
        async fn an_edit_after_a_partial_read_still_needs_the_rest_for_a_write() {
            let dir = canonical_tempdir();
            let path = dir.path().join("notes.txt");
            fs::write(&path, "one\ntwo\nthree\n").unwrap();
            let ctx = ctx_with(dir.path(), tracker());

            let read = local_testing::run(
                &ReadTool,
                serde_json::json!({"file_path": path, "limit": 1}),
                &ctx,
            )
            .await;
            assert!(!read.is_error, "{}", read.content);
            let edit = local_testing::run(
                &EditTool,
                serde_json::json!({
                    "file_path": path,
                    "old_string": "one",
                    "new_string": "uno",
                }),
                &ctx,
            )
            .await;
            assert!(!edit.is_error, "{}", edit.content);

            let write = local_testing::run(
                &WriteTool,
                serde_json::json!({"file_path": path, "content": "whole"}),
                &ctx,
            )
            .await;

            assert!(write.is_error);
            assert_eq!(
                write.content,
                format!(
                    "File {} has only been read in part. \
                     Read the remaining lines, paging with `offset` and `limit` if needed, \
                     before writing to it.",
                    path.display(),
                ),
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "uno\ntwo\nthree\n");
        }

        #[tokio::test]
        async fn rejects_empty_old_string() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "hello").unwrap();

            let err = edit_file(path.to_str().unwrap(), "", "x", false, &temp_ctx())
                .await
                .unwrap_err();

            assert_eq!(err, "old_string must not be empty.");
            assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
        }

        #[tokio::test]
        async fn rejects_identical_strings() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "hello").unwrap();

            let err = edit_file(path.to_str().unwrap(), "hello", "hello", false, &temp_ctx())
                .await
                .unwrap_err();

            assert_eq!(
                err,
                "old_string and new_string are identical. No changes to make."
            );
        }

        #[tokio::test]
        async fn rejects_string_not_found() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "hello world").unwrap();

            let err = edit_file(
                path.to_str().unwrap(),
                "nonexistent",
                "replacement",
                false,
                &seeded_ctx(&path),
            )
            .await
            .unwrap_err();
            assert_eq!(
                err,
                format!(
                    "Found no occurrence of old_string in {}. \
                     Copy it exactly from the file, including whitespace and indentation.",
                    path.display(),
                ),
            );
        }

        #[tokio::test]
        async fn without_prior_read_is_rejected() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "hello world").unwrap();

            let err = edit_file(
                path.to_str().unwrap(),
                "hello",
                "goodbye",
                false,
                &temp_ctx(),
            )
            .await
            .unwrap_err();

            assert_eq!(
                err,
                format!(
                    "File {} has not been read in this session. Read it before editing it.",
                    path.display(),
                ),
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "hello world");
        }

        #[tokio::test]
        async fn after_external_modification_is_rejected() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "hello world").unwrap();
            let ctx = seeded_ctx(&path);
            fs::write(&path, "external edit").unwrap();

            let err = edit_file(path.to_str().unwrap(), "external", "ours", false, &ctx)
                .await
                .unwrap_err();

            assert_eq!(
                err,
                format!(
                    "File {} has been modified externally since it was last read. \
                     Re-read it before editing it.",
                    path.display(),
                ),
            );
        }

        #[tokio::test]
        async fn rejects_nonexistent_file() {
            let path = "/nonexistent/file.txt";

            let err = edit_file(path, "a", "b", false, &temp_ctx())
                .await
                .unwrap_err();

            let cause = fs::metadata(path).unwrap_err();
            assert_eq!(err, format!("Failed to access {path}: {cause}"));
        }

        #[tokio::test]
        async fn rejects_non_utf8_content() {
            let dir = canonical_tempdir();
            let path = dir.path().join("blob.bin");
            let bytes = [0xff_u8, 0xfe, 0xfd];
            fs::write(&path, bytes).unwrap();
            let ctx = temp_ctx();
            ctx.tracker
                .record_read(&path, &bytes, LastView::Full, "toolu_test");

            let err = edit_file(path.to_str().unwrap(), "a", "b", false, &ctx)
                .await
                .unwrap_err();

            let cause = String::from_utf8(bytes.to_vec()).unwrap_err();
            assert_eq!(err, format!("Failed to read {}: {cause}", path.display()));
        }

        #[tokio::test]
        async fn rejects_ambiguous_match() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "aaa bbb aaa").unwrap();

            let err = edit_file(
                path.to_str().unwrap(),
                "aaa",
                "ccc",
                false,
                &seeded_ctx(&path),
            )
            .await
            .unwrap_err();
            assert_eq!(
                err,
                format!(
                    "Found 2 occurrences of old_string in {}. \
                     Set replace_all to true to replace them all, \
                     or add context to make old_string unique.",
                    path.display(),
                ),
            );
        }

        #[tokio::test]
        async fn rejects_directory() {
            let dir = canonical_tempdir();
            let path = dir.path().to_str().unwrap();

            let err = edit_file(path, "a", "b", false, &temp_ctx())
                .await
                .unwrap_err();

            assert_eq!(err, format!("{path} is a directory. Refusing to edit."));
        }

        #[tokio::test]
        async fn rejects_too_large_file() {
            let dir = canonical_tempdir();
            let path = dir.path().join("huge.txt");
            let f = fs::File::create(&path).unwrap();
            f.set_len(MAX_TRACKED_FILE_SIZE + 1).unwrap();

            let err = edit_file(path.to_str().unwrap(), "a", "b", false, &temp_ctx())
                .await
                .unwrap_err();

            assert_eq!(
                err,
                "File is too large (10.0 MiB, max 10 MiB). Use the bash tool to edit large files."
            );
        }

        #[tokio::test]
        async fn fails_on_read_only_file() {
            let dir = canonical_tempdir();
            let path = dir.path().join("readonly.txt");
            fs::write(&path, "hello world").unwrap();
            let ctx = seeded_ctx(&path);
            local_testing::make_read_only(&path);

            let err = edit_file(path.to_str().unwrap(), "hello", "goodbye", false, &ctx)
                .await
                .unwrap_err();

            let cause = fs::OpenOptions::new().write(true).open(&path).unwrap_err();
            assert_eq!(err, format!("Failed to write {}: {cause}", path.display()));
            assert_eq!(fs::read_to_string(&path).unwrap(), "hello world");
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn accepts_a_read_through_a_symlinked_directory() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join("real")).unwrap();
            std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
            let path = dir.path().join("real/test.txt");
            fs::write(&path, "hello world").unwrap();
            let ctx = temp_ctx();

            let via_link = dir.path().join("link/test.txt");
            let read =
                local_testing::run(&ReadTool, serde_json::json!({"file_path": via_link}), &ctx)
                    .await;
            assert!(!read.is_error, "{}", read.content);

            let result = edit_file(path.to_str().unwrap(), "hello", "goodbye", false, &ctx).await;

            result.unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "goodbye world");
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn rejects_non_regular_file() {
            let dir = canonical_tempdir();
            let path = dir.path().join("socket");
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

            let err = edit_file(path.to_str().unwrap(), "a", "b", false, &temp_ctx())
                .await
                .unwrap_err();

            assert_eq!(
                err,
                format!(
                    "{} is not a regular file (fifo, socket, or device). Refusing to edit.",
                    path.display(),
                ),
            );
        }
    }

    mod replacement {
        use super::*;

        mod apply {
            use super::*;

            #[test]
            fn preserves_dominant_line_endings_without_doubling_crlf() {
                for (name, content, old, new, expected) in [
                    (
                        "unique",
                        "fn foo() {}\nfn bar() {}\n",
                        "fn foo() {}",
                        "fn foo() -> i32 { 42 }",
                        "fn foo() -> i32 { 42 }\nfn bar() {}\n",
                    ),
                    (
                        "crlf",
                        "line1\r\nline2\r\n",
                        "line1\nline2",
                        "a\nb",
                        "a\r\nb\r\n",
                    ),
                    (
                        "new crlf",
                        "aaa\r\nbbb\r\n",
                        "aaa",
                        "x\r\ny",
                        "x\r\ny\r\nbbb\r\n",
                    ),
                    (
                        "mixed",
                        "aaa\nbbb\r\nreplace_me\r\n",
                        "replace_me",
                        "replaced",
                        "aaa\r\nbbb\r\nreplaced\r\n",
                    ),
                    (
                        "mixed multiline",
                        "foo\nbar\r\nbaz\r\n",
                        "foo\nbar",
                        "a\nb",
                        "a\r\nb\r\nbaz\r\n",
                    ),
                ] {
                    let replacement = Replacement {
                        path: "test.txt".to_owned(),
                        old_string: old.to_owned(),
                        new_string: new.to_owned(),
                        replace_all: false,
                    };

                    let (updated, edited, _) = replacement.apply(content.as_bytes()).unwrap();

                    assert_eq!(updated, expected, "{name}");
                    assert_eq!(edited.replacements, 1, "{name}");
                }
            }

            #[test]
            fn replace_all_numbers_the_first_hunk_and_lists_every_location() {
                let replacement = Replacement {
                    path: "grow.txt".to_owned(),
                    old_string: "B".to_owned(),
                    new_string: "X\nY".to_owned(),
                    replace_all: true,
                };

                let (updated, edited, _) = replacement.apply(b"A\nB\nC\nB\n").unwrap();

                assert_eq!(edited.replacements, 2);
                assert_eq!(
                    edited.diff,
                    EditDiff {
                        chunk: DiffChunk {
                            old: vec![line(2, "B")],
                            new: vec![line(2, "X"), line(3, "Y")],
                        },
                        locations: vec![2, 4],
                    }
                );
                assert_eq!(
                    updated,
                    indoc! {"
                        A
                        X
                        Y
                        C
                        X
                        Y
                    "}
                );
            }
        }
    }

    mod build_diff {
        use super::*;

        #[test]
        fn numbers_the_hunk_by_its_lines_in_the_file() {
            let (diff, _) = build_diff("A\nB\nC\n", "B", "X");

            assert_eq!(
                diff,
                EditDiff {
                    chunk: DiffChunk {
                        old: vec![line(2, "B")],
                        new: vec![line(2, "X")],
                    },
                    locations: vec![2],
                },
            );
        }

        #[test]
        fn locates_each_replacement_by_its_first_removed_line() {
            let (diff, _) = build_diff("X\nY\nA\nX\nY\nB\nX\nY\nC\n", "X\nY", "Z");

            assert_eq!(
                diff,
                EditDiff {
                    chunk: DiffChunk {
                        old: vec![line(1, "X"), line(2, "Y")],
                        new: vec![line(1, "Z")],
                    },
                    locations: vec![1, 4, 7],
                },
            );
        }

        #[test]
        fn locates_each_insertion_by_its_line_after_the_earlier_ones() {
            let (diff, _) = build_diff("fn foo()\nx\nfn foo()\n", "fn foo()", "fn foo()\n    body");

            assert_eq!(
                diff,
                EditDiff {
                    chunk: DiffChunk {
                        old: vec![],
                        new: vec![line(2, "    body")],
                    },
                    locations: vec![2, 5],
                },
            );
        }

        #[test]
        fn a_hunk_that_trims_to_nothing_has_no_locations() {
            let (diff, _) = build_diff("a b\n", "a", "a\n");

            assert_eq!(
                diff,
                EditDiff {
                    chunk: DiffChunk {
                        old: vec![],
                        new: vec![],
                    },
                    locations: vec![],
                },
            );
        }

        #[test]
        fn collects_the_new_lines_each_replacement_writes() {
            for (case, content, old, new, expected) in [
                ("one line", "A\nB\nC\n", "B", "X", vec![(2, 2)]),
                (
                    "ends with a line break",
                    "A\nB\nC\n",
                    "B",
                    "X\n",
                    vec![(2, 2)],
                ),
                ("several lines", "A\nB\nC\n", "B", "X\nY", vec![(2, 3)]),
                (
                    "shifted by an earlier replacement",
                    "a\na\na\n",
                    "a",
                    "x\ny",
                    vec![(1, 2), (3, 4), (5, 6)],
                ),
                (
                    "added lines before a later occurrence",
                    "a\nSEP\na\n",
                    "a",
                    "x\ny\nz",
                    vec![(1, 3), (5, 7)],
                ),
                (
                    "removed lines before a later occurrence",
                    "a\nSEP\nb\na\nSEP\n",
                    "a\nSEP\n",
                    "x\n",
                    vec![(1, 1), (3, 3)],
                ),
                ("deleted text", "a\nb\na\n", "a\n", "", vec![]),
                ("crlf", "a\r\nb\r\na\r\n", "a", "x", vec![(1, 1), (3, 3)]),
            ] {
                let (_, written) = build_diff(content, old, new);

                assert_eq!(written, expected, "{case}");
            }
        }
    }

    mod anchor_line {
        use super::*;

        #[test]
        fn takes_the_first_removed_line() {
            let hunk = DiffChunk {
                old: vec![line(2, "a"), line(3, "b")],
                new: vec![line(2, "X")],
            };
            assert_eq!(anchor_line(&hunk, 10, 20), Some(11));
        }

        #[test]
        fn takes_the_first_added_line_of_a_pure_insertion() {
            let hunk = DiffChunk {
                old: vec![],
                new: vec![line(2, "added")],
            };
            assert_eq!(anchor_line(&hunk, 10, 20), Some(21));
        }

        #[test]
        fn leaves_an_empty_hunk_unanchored() {
            let hunk = DiffChunk {
                old: vec![],
                new: vec![],
            };
            assert_eq!(anchor_line(&hunk, 10, 20), None);
        }
    }
    mod split_into_diff_lines {
        use super::*;

        #[test]
        fn numbers_from_start_line() {
            assert_eq!(
                split_into_diff_lines("a\nb", 47),
                vec![line(47, "a"), line(48, "b")],
            );
        }

        #[test]
        fn drops_trailing_newline() {
            assert_eq!(
                split_into_diff_lines("a\nb\n", 1),
                vec![line(1, "a"), line(2, "b")],
            );
        }

        #[test]
        fn yields_no_lines_for_empty_text() {
            assert_eq!(split_into_diff_lines("", 1), []);
        }
    }

    mod trim_chunk {
        use super::*;

        #[test]
        fn drops_matching_prefix_and_suffix_preserving_numbers() {
            let mut chunk = DiffChunk {
                old: vec![line(10, "anchor"), line(11, "old"), line(12, "tail")],
                new: vec![line(10, "anchor"), line(11, "new"), line(12, "tail")],
            };
            trim_chunk(&mut chunk);
            assert_eq!(chunk.old, vec![line(11, "old")]);
            assert_eq!(chunk.new, vec![line(11, "new")]);
        }

        #[test]
        fn pure_tail_insertion_strips_anchor() {
            let mut chunk = DiffChunk {
                old: vec![line(5, "fn foo()")],
                new: vec![line(5, "fn foo()"), line(6, "    return 42;")],
            };
            trim_chunk(&mut chunk);
            assert_eq!(chunk.old, []);
            assert_eq!(chunk.new, vec![line(6, "    return 42;")]);
        }

        #[test]
        fn fully_identical_collapses_both_sides() {
            let mut chunk = DiffChunk {
                old: vec![line(1, "a")],
                new: vec![line(1, "a")],
            };
            trim_chunk(&mut chunk);
            assert_eq!(chunk.old, []);
            assert_eq!(chunk.new, []);
        }
    }

    mod common_boundaries {
        use super::*;

        #[test]
        fn produces_prefix_and_suffix_counts() {
            let old = ["a", "b", "c", "d"];
            let new = ["a", "X", "Y", "d"];
            assert_eq!(common_boundaries(&old, &new), (1, 1));
        }

        #[test]
        fn disjoint_is_zero() {
            assert_eq!(common_boundaries(&["a"], &["b"]), (0, 0));
        }

        #[test]
        fn empty_inputs_have_zero_boundaries() {
            assert_eq!(common_boundaries::<&str>(&[], &[]), (0, 0));
        }

        #[test]
        fn asymmetric_lengths_capped_by_shorter_side() {
            assert_eq!(common_boundaries(&["a", "b", "a"], &["a", "X"]), (1, 0));
            assert_eq!(common_boundaries(&["a", "X"], &["a", "b", "a"]), (1, 0));
        }
    }

    mod dominant_eol {
        use super::*;

        #[test]
        fn selects_the_majority_and_defaults_to_lf() {
            for (case, input, expected) in [
                ("LF only", "a\nb\n", "\n"),
                ("CRLF only", "a\r\nb\r\n", "\r\n"),
                ("CRLF majority", "a\nb\r\nc\r\n", "\r\n"),
                ("LF majority", "a\nb\nc\r\n", "\n"),
                ("tie", "a\nb\r\n", "\n"),
                ("no newline", "no newlines", "\n"),
            ] {
                assert_eq!(dominant_eol(input), expected, "{case}");
            }
        }
    }

    mod normalize_eol {
        use super::*;

        #[test]
        fn converts_crlf_to_lf() {
            let out = normalize_eol("a\r\nb\r\n");
            assert_eq!(out, "a\nb\n");
            assert!(matches!(out, Cow::Owned(_)));
        }

        #[test]
        fn lf_input_borrows() {
            let out = normalize_eol("a\nb\n");
            assert_eq!(out, "a\nb\n");
            assert!(matches!(out, Cow::Borrowed(_)));
        }
    }

    mod apply_eol {
        use super::*;

        #[test]
        fn restores_requested_line_endings() {
            for (case, eol, expected) in [("CRLF", "\r\n", "a\r\nb\r\n"), ("LF", "\n", "a\nb\n")] {
                assert_eq!(apply_eol("a\nb\n".to_owned(), eol), expected, "{case}");
            }
        }
    }

    mod synthesize_chunk {
        use super::*;

        #[test]
        fn starts_numbering_at_one() {
            let chunk = synthesize_chunk("a\nb", "x\ny");
            assert_eq!(chunk.old[0].number, 1);
            assert_eq!(chunk.new[0].number, 1);
        }

        #[test]
        fn applies_trim() {
            let chunk = synthesize_chunk("fn foo()", "fn foo()\n    body");
            assert_eq!(chunk.old, []);
            assert_eq!(chunk.new.len(), 1);
            assert_eq!(chunk.new[0].text, "    body");
        }
    }
}
