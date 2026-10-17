//! Exact string replacement tool for file editing.

use std::borrow::Cow;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde::Deserialize;

use super::{
    ApprovalBody, ApprovalPreview, DiffChunk, DiffLine, Prepared, Tool, ToolCtx, ToolMetadata,
    ToolOutput, ToolResultView, ToolSpec, WriteMode, extract_input_field, summarize_path_call,
};
use crate::file_tracker::GatePurpose;
use crate::permission::PermissionClass;

pub(crate) struct EditTool;

impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".to_owned(),
            description: "Perform exact string replacement in a file. \
                          The old_string must be unique in the file unless replace_all is true. \
                          Read the file in this session before editing it. \
                          A Read of the lines being changed is enough. \
                          The Read-before-Edit gate refuses files that changed externally \
                          since the last Read."
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

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "file_path")
    }

    fn summarize_call(&self, name: &str, input: &serde_json::Value, cwd: &Path) -> String {
        summarize_path_call(name, input, "file_path", cwd)
    }

    fn result_view(
        &self,
        input: &serde_json::Value,
        _content: &str,
        metadata: &ToolMetadata,
        _cwd: &Path,
    ) -> Option<ToolResultView> {
        let old = input.get("old_string")?.as_str()?;
        let new = input.get("new_string")?.as_str()?;
        let replace_all = input
            .get("replace_all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let chunks = metadata
            .diff_chunks
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| vec![synthesize_chunk(old, new)]);
        let replacements = chunks.len();
        Some(ToolResultView::Diff {
            chunks,
            replace_all,
            replacements,
        })
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
            body: ApprovalBody::Diff(vec![synthesize_chunk(old, new)]),
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
        Err(e) => return e,
    };

    let name = super::file_name(&input.file_path);
    match edit_file(
        &input.file_path,
        &input.old_string,
        &input.new_string,
        input.replace_all,
        ctx,
    )
    .await
    {
        Ok((content, chunks)) => ToolOutput::success(content)
            .with_title(format!("Edited {name}"))
            .with_diff_chunks(chunks),
        Err(msg) => ToolOutput::error(msg),
    }
}

/// Performs the in-place exact-string replacement.
///
/// Contract:
///
/// - `old_string` must occur verbatim in the file (whitespace and indentation included). Without
///   `replace_all`, ambiguous matches fail so the model cannot silently edit the wrong site.
/// - The Read-before-Edit gate refuses files with no Read of their current content and files whose
///   content changed since that Read.
/// - CRLF is preserved: the file is normalized to LF for matching, and the dominant EOL is restored
///   on write so we don't flip line endings just because the caller used `\n` in `old_string`.
/// - Returns `(message, diff_chunks)` on success, with one chunk per replaced match. The chunks carry
///   real post-edit line numbers, shifted by the cumulative delta of earlier replacements.
/// - A relative `path` resolves against the context's cwd.
async fn edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    ctx: &ToolCtx,
) -> Result<(String, Vec<DiffChunk>), String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty.".to_owned());
    }

    if old_string == new_string {
        return Err("old_string and new_string are identical. No changes to make.".to_owned());
    }

    let file_path = ctx.cwd().join(path);
    let (_, content_bytes) = super::read_validated(&file_path, path, |metadata| {
        super::validate_file_target(path, metadata, "edit")
    })
    .await?;
    let key = super::tracker_key(&file_path).await;
    ctx.tracker
        .verify_current_content(&key, &content_bytes, GatePurpose::Edit)
        .map_err(|e| e.to_string())?;
    let content =
        String::from_utf8(content_bytes).map_err(|e| format!("Failed to read {path}: {e}"))?;

    let eol = dominant_eol(&content);
    let content = normalize_eol(&content);
    let old_string = normalize_eol(old_string);
    let new_string = normalize_eol(new_string);

    let match_count = content.matches(old_string.as_ref()).count();
    if match_count == 0 {
        return Err(format!(
            "Found no occurrence of old_string in {path}. \
             Copy it exactly from the file, including whitespace and indentation."
        ));
    }

    if match_count > 1 && !replace_all {
        return Err(format!(
            "Found {match_count} occurrences of old_string in {path}. \
             Set replace_all to true to replace them all, or add context to make old_string unique."
        ));
    }

    let chunks_take = if replace_all { usize::MAX } else { 1 };
    let chunks = build_diff_chunks(
        &content,
        old_string.as_ref(),
        new_string.as_ref(),
        chunks_take,
    );

    let updated = if replace_all {
        content.replace(old_string.as_ref(), new_string.as_ref())
    } else {
        content.replacen(old_string.as_ref(), new_string.as_ref(), 1)
    };
    let updated = apply_eol(updated, eol);

    super::write_tracked(ctx, &file_path, updated.into_bytes(), WriteMode::Overwrite)
        .await
        .map_err(|e| format!("Failed to write {path}: {e}"))?;

    let message = if replace_all && match_count > 1 {
        format!("Replaced {match_count} occurrences in {path}.")
    } else {
        format!("Successfully edited {path}.")
    };
    Ok((message, chunks))
}

// ── Diff Production ──

fn build_diff_chunks(
    original: &str,
    old_string: &str,
    new_string: &str,
    take: usize,
) -> Vec<DiffChunk> {
    let positions = match_positions(original, old_string, take);
    let shift_per_match = new_string.matches('\n').count().cast_signed()
        - old_string.matches('\n').count().cast_signed();

    positions
        .into_iter()
        .enumerate()
        .map(|(idx, byte_pos)| {
            let original_line = line_at_byte(original, byte_pos);
            let cumulative_shift = idx
                .cast_signed()
                .checked_mul(shift_per_match)
                .expect("cumulative line-shift fits in isize for tracked-size inputs");
            let new_line = original_line
                .checked_add_signed(cumulative_shift)
                .expect("post-edit line number stays positive for real match positions");
            let mut chunk = DiffChunk {
                old: split_into_diff_lines(old_string, original_line),
                new: split_into_diff_lines(new_string, new_line),
            };
            trim_chunk(&mut chunk);
            chunk
        })
        .collect()
}

fn match_positions(haystack: &str, pattern: &str, take: usize) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut start = 0;
    while positions.len() < take {
        let Some(rel) = haystack[start..].find(pattern) else {
            break;
        };
        let abs = start + rel;
        positions.push(abs);
        start = abs + pattern.len().max(1);
    }
    positions
}

fn line_at_byte(content: &str, offset: usize) -> usize {
    1 + content[..offset].matches('\n').count()
}

fn split_into_diff_lines(s: &str, start_line: usize) -> Vec<DiffLine> {
    s.lines()
        .enumerate()
        .map(|(i, text)| DiffLine {
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
    let max_prefix = old.len().min(new.len());
    let mut prefix = 0;
    while prefix < max_prefix && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let mut suffix = 0;
    while suffix < max_suffix && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
        suffix += 1;
    }
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

pub(crate) fn synthesize_chunk(old: &str, new: &str) -> DiffChunk {
    let mut chunk = DiffChunk {
        old: split_into_diff_lines(old, 1),
        new: split_into_diff_lines(new, 1),
    };
    trim_chunk(&mut chunk);
    chunk
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::file_tracker::testing::tracker_seeded;
    use crate::file_tracker::{LastView, MAX_TRACKED_FILE_SIZE};
    use crate::tool::read::ReadTool;
    use crate::tool::testing::{self, canonical_tempdir, ctx_with, temp_ctx};

    fn seeded_ctx(path: &Path) -> ToolCtx {
        ctx_with(&std::env::temp_dir(), tracker_seeded(path))
    }

    // ── result_view ──

    #[test]
    fn result_view_prefers_structured_chunks_from_metadata_on_live_path() {
        let input = serde_json::json!({
            "file_path": "/tmp/f.rs",
            "old_string": "a",
            "new_string": "b",
            "replace_all": true,
        });
        let chunks = vec![
            DiffChunk {
                old: vec![DiffLine {
                    number: 12,
                    text: "a".to_owned(),
                }],
                new: vec![DiffLine {
                    number: 12,
                    text: "b".to_owned(),
                }],
            },
            DiffChunk {
                old: vec![DiffLine {
                    number: 47,
                    text: "a".to_owned(),
                }],
                new: vec![DiffLine {
                    number: 47,
                    text: "b".to_owned(),
                }],
            },
        ];
        let metadata = ToolMetadata {
            diff_chunks: Some(chunks.clone()),
            ..ToolMetadata::default()
        };
        let view = EditTool.result_view(
            &input,
            "Replaced 2 occurrences in /tmp/f.rs.",
            &metadata,
            Path::new("/repo"),
        );
        assert_eq!(
            view,
            Some(ToolResultView::Diff {
                chunks,
                replace_all: true,
                replacements: 2,
            }),
        );
    }

    #[test]
    fn result_view_synthesizes_chunk_when_metadata_lacks_diff_chunks() {
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
            Some(ToolResultView::Diff {
                chunks: vec![DiffChunk {
                    old: vec![DiffLine {
                        number: 1,
                        text: "fn foo()".to_owned()
                    }],
                    new: vec![DiffLine {
                        number: 1,
                        text: "fn bar()".to_owned()
                    }],
                }],
                replace_all: false,
                replacements: 1,
            }),
        );
    }

    #[test]
    fn result_view_missing_or_mistyped_required_inputs_uses_plain_output() {
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

    // ── approval_preview ──

    #[tokio::test]
    async fn approval_preview_renders_old_and_new_as_a_diff() {
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
            ApprovalBody::Diff(vec![DiffChunk {
                old: vec![DiffLine {
                    number: 1,
                    text: "let a = 1;".to_owned(),
                }],
                new: vec![DiffLine {
                    number: 1,
                    text: "let a = 2;".to_owned(),
                }],
            }]),
        );
    }

    // ── run ──

    #[tokio::test]
    async fn run_valid_edit_updates_file() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let input = serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "old_string": "hello",
            "new_string": "goodbye"
        });

        let output = testing::run(&EditTool, input, &seeded_ctx(&path)).await;

        assert!(!output.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
        assert_eq!(
            output.metadata.title.as_deref(),
            Some("Edited test.txt"),
            "success path attaches the Edited title",
        );
    }

    #[tokio::test]
    async fn run_missing_required_fields_reports_input_error() {
        let output = run(
            serde_json::json!({
                "file_path": "/tmp/x",
                "old_string": "a"
            }),
            &temp_ctx(),
        )
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_edit_error_omits_edited_title() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

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

    // ── edit_file ──

    #[tokio::test]
    async fn edit_file_replaces_unique_string() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(
            &path,
            indoc! {"
                fn foo() {}
                fn bar() {}
            "},
        )
        .unwrap();

        edit_file(
            path.to_str().unwrap(),
            "fn foo() {}",
            "fn foo() -> i32 { 42 }",
            false,
            &seeded_ctx(&path),
        )
        .await
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content,
            indoc! {"
                fn foo() -> i32 { 42 }
                fn bar() {}
            "}
        );
    }

    #[tokio::test]
    async fn edit_file_replace_all_updates_every_match() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();

        let (msg, chunks) = edit_file(
            path.to_str().unwrap(),
            "aaa",
            "ccc",
            true,
            &seeded_ctx(&path),
        )
        .await
        .unwrap();

        assert_eq!(
            msg,
            format!("Replaced 2 occurrences in {}.", path.display())
        );
        assert_eq!(chunks.len(), 2, "replace_all emits one chunk per match");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ccc bbb ccc");
    }

    #[tokio::test]
    async fn edit_file_replace_all_single_match_updates_content() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let (msg, chunks) = edit_file(
            path.to_str().unwrap(),
            "hello",
            "goodbye",
            true,
            &seeded_ctx(&path),
        )
        .await
        .unwrap();

        assert!(msg.contains("Successfully edited"));
        assert_eq!(
            chunks.len(),
            1,
            "single-match replace_all still replaces once"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
    }

    #[tokio::test]
    async fn edit_file_crlf_matching_preserves_line_endings() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\r\nline2\r\n").unwrap();

        edit_file(
            path.to_str().unwrap(),
            "line1\nline2",
            "a\nb",
            false,
            &seeded_ctx(&path),
        )
        .await
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"a\r\nb\r\n");
    }

    #[tokio::test]
    async fn edit_file_crlf_in_new_string_not_doubled() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "aaa\r\nbbb\r\n").unwrap();

        edit_file(
            path.to_str().unwrap(),
            "aaa",
            "x\r\ny",
            false,
            &seeded_ctx(&path),
        )
        .await
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"x\r\ny\r\nbbb\r\n");
    }

    #[tokio::test]
    async fn edit_file_mixed_eol_normalized_to_dominant() {
        let dir = canonical_tempdir();
        let path = dir.path().join("mixed.txt");
        std::fs::write(&path, "aaa\nbbb\r\nreplace_me\r\n").unwrap();

        edit_file(
            path.to_str().unwrap(),
            "replace_me",
            "replaced",
            false,
            &seeded_ctx(&path),
        )
        .await
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"aaa\r\nbbb\r\nreplaced\r\n");
    }

    #[tokio::test]
    async fn edit_file_mixed_eol_multiline_match() {
        let dir = canonical_tempdir();
        let path = dir.path().join("mixed.txt");
        std::fs::write(&path, "foo\nbar\r\nbaz\r\n").unwrap();

        edit_file(
            path.to_str().unwrap(),
            "foo\nbar",
            "a\nb",
            false,
            &seeded_ctx(&path),
        )
        .await
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"a\r\nb\r\nbaz\r\n");
    }

    #[tokio::test]
    async fn edit_file_rejects_empty_old_string() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();

        let err = edit_file(path.to_str().unwrap(), "", "x", false, &temp_ctx())
            .await
            .unwrap_err();
        assert!(err.contains("must not be empty"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[tokio::test]
    async fn edit_file_rejects_identical_strings() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();

        let err = edit_file(path.to_str().unwrap(), "hello", "hello", false, &temp_ctx())
            .await
            .unwrap_err();
        assert!(err.contains("identical"));
    }

    #[tokio::test]
    async fn edit_file_rejects_string_not_found() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

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
    async fn edit_file_without_prior_read_is_rejected() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let err = edit_file(
            path.to_str().unwrap(),
            "hello",
            "goodbye",
            false,
            &temp_ctx(),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("has not been read"),
            "expected must-read-first rejection, got: {err}",
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn edit_file_after_external_modification_is_rejected() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();
        let ctx = seeded_ctx(&path);
        std::fs::write(&path, "external edit").unwrap();

        let err = edit_file(path.to_str().unwrap(), "external", "ours", false, &ctx)
            .await
            .unwrap_err();
        assert!(
            err.contains("modified externally"),
            "drift error expected, got: {err}",
        );
    }

    #[tokio::test]
    async fn edit_file_accepts_a_read_under_another_spelling_of_the_path() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();
        let ctx = temp_ctx();

        let read = testing::run(&ReadTool, serde_json::json!({"file_path": path}), &ctx).await;
        assert!(!read.is_error, "{}", read.content);

        let detour = dir.path().join("sub").join("..").join("test.txt");
        edit_file(detour.to_str().unwrap(), "hello", "goodbye", false, &ctx)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_accepts_a_read_through_a_symlinked_directory() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        let path = dir.path().join("real/test.txt");
        std::fs::write(&path, "hello world").unwrap();
        let ctx = temp_ctx();

        let via_link = dir.path().join("link/test.txt");
        let read = testing::run(&ReadTool, serde_json::json!({"file_path": via_link}), &ctx).await;
        assert!(!read.is_error, "{}", read.content);

        edit_file(path.to_str().unwrap(), "hello", "goodbye", false, &ctx)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
    }

    #[tokio::test]
    async fn edit_file_accepts_a_partial_read_of_current_content() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world\nsecond line\n").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let ctx = temp_ctx();
        ctx.tracker
            .record_read(&path, &bytes, LastView::lines(1, 1, 2), "toolu_test");

        edit_file(path.to_str().unwrap(), "hello", "goodbye", false, &ctx)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "goodbye world\nsecond line\n",
        );
    }

    #[tokio::test]
    async fn edit_file_rejects_nonexistent_file() {
        let path = "/nonexistent/file.txt";

        let err = edit_file(path, "a", "b", false, &temp_ctx())
            .await
            .unwrap_err();

        let cause = std::fs::metadata(path).unwrap_err();
        assert_eq!(err, format!("Failed to read {path}: {cause}"));
    }

    #[tokio::test]
    async fn edit_file_rejects_non_utf8_content() {
        let dir = canonical_tempdir();
        let path = dir.path().join("blob.bin");
        let bytes = [0xff_u8, 0xfe, 0xfd];
        std::fs::write(&path, bytes).unwrap();
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
    async fn edit_file_rejects_ambiguous_match() {
        let dir = canonical_tempdir();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();

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
    async fn edit_file_rejects_directory() {
        let dir = canonical_tempdir();

        let err = edit_file(dir.path().to_str().unwrap(), "a", "b", false, &temp_ctx())
            .await
            .unwrap_err();

        assert!(
            err.contains("is a directory"),
            "expected directory rejection, got: {err}",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_rejects_non_regular_file() {
        let dir = canonical_tempdir();
        let path = dir.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let err = edit_file(path.to_str().unwrap(), "a", "b", false, &temp_ctx())
            .await
            .unwrap_err();

        assert!(
            err.contains("not a regular file"),
            "expected non-regular-file rejection, got: {err}",
        );
    }

    #[tokio::test]
    async fn edit_file_rejects_too_large_file() {
        let dir = canonical_tempdir();
        let path = dir.path().join("huge.txt");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_TRACKED_FILE_SIZE + 1).unwrap();

        let err = edit_file(path.to_str().unwrap(), "a", "b", false, &temp_ctx())
            .await
            .unwrap_err();
        assert!(err.contains("too large"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_fails_if_write_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let dir = canonical_tempdir();
        let path = dir.path().join("readonly.txt");
        std::fs::write(&path, "hello world").unwrap();
        let ctx = seeded_ctx(&path);

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&path, perms).unwrap();

        let err = edit_file(path.to_str().unwrap(), "hello", "goodbye", false, &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("Failed to write"));
    }

    #[tokio::test]
    async fn edit_file_chunks_carry_real_file_line_numbers_for_replace_all() {
        let dir = canonical_tempdir();
        let path = dir.path().join("multi.txt");
        std::fs::write(
            &path,
            indoc! {"
                A
                B
                C
                B
            "},
        )
        .unwrap();

        let (_, chunks) = edit_file(path.to_str().unwrap(), "B", "X", true, &seeded_ctx(&path))
            .await
            .unwrap();

        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks[0].old,
            vec![DiffLine {
                number: 2,
                text: "B".to_owned()
            }]
        );
        assert_eq!(
            chunks[0].new,
            vec![DiffLine {
                number: 2,
                text: "X".to_owned()
            }]
        );
        assert_eq!(
            chunks[1].old,
            vec![DiffLine {
                number: 4,
                text: "B".to_owned()
            }]
        );
        assert_eq!(
            chunks[1].new,
            vec![DiffLine {
                number: 4,
                text: "X".to_owned()
            }]
        );
    }

    #[tokio::test]
    async fn edit_file_chunks_shift_new_side_for_growing_replace_all() {
        let dir = canonical_tempdir();
        let path = dir.path().join("grow.txt");
        std::fs::write(
            &path,
            indoc! {"
                A
                B
                C
                B
            "},
        )
        .unwrap();

        let (_, chunks) = edit_file(
            path.to_str().unwrap(),
            "B",
            "X\nY",
            true,
            &seeded_ctx(&path),
        )
        .await
        .unwrap();

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].old[0].number, 2);
        assert_eq!(
            chunks[0].new,
            vec![
                DiffLine {
                    number: 2,
                    text: "X".to_owned()
                },
                DiffLine {
                    number: 3,
                    text: "Y".to_owned()
                },
            ],
        );
        assert_eq!(chunks[1].old[0].number, 4);
        assert_eq!(
            chunks[1].new,
            vec![
                DiffLine {
                    number: 5,
                    text: "X".to_owned()
                },
                DiffLine {
                    number: 6,
                    text: "Y".to_owned()
                },
            ],
        );
    }

    #[tokio::test]
    async fn edit_file_chunks_shift_new_side_for_shrinking_replace_all() {
        let dir = canonical_tempdir();
        let path = dir.path().join("shrink.txt");
        std::fs::write(
            &path,
            indoc! {"
                X
                Y
                A
                X
                Y
                B
                X
                Y
                C
            "},
        )
        .unwrap();

        let (_, chunks) = edit_file(
            path.to_str().unwrap(),
            "X\nY",
            "Z",
            true,
            &seeded_ctx(&path),
        )
        .await
        .unwrap();

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].old[0].number, 1);
        assert_eq!(chunks[0].new[0].number, 1);
        assert_eq!(chunks[1].old[0].number, 4);
        assert_eq!(chunks[1].new[0].number, 3);
        assert_eq!(chunks[2].old[0].number, 7);
        assert_eq!(chunks[2].new[0].number, 5);
    }

    // ── build_diff_chunks ──

    #[test]
    fn build_diff_chunks_single_match_carries_real_position() {
        let chunks = build_diff_chunks("A\nB\nC\n", "B", "X", 1);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].old,
            vec![DiffLine {
                number: 2,
                text: "B".to_owned()
            }]
        );
        assert_eq!(
            chunks[0].new,
            vec![DiffLine {
                number: 2,
                text: "X".to_owned()
            }]
        );
    }

    #[test]
    fn build_diff_chunks_take_one_caps_at_first_match() {
        let chunks = build_diff_chunks("B\nB\nB\n", "B", "X", 1);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].old[0].number, 1);
    }

    #[test]
    fn build_diff_chunks_applies_per_chunk_trim() {
        let chunks = build_diff_chunks(
            indoc! {"
                x
                x
                x
                x
                fn foo()
            "},
            "fn foo()",
            "fn foo()\n    return 42;",
            1,
        );
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].old, []);
        assert_eq!(
            chunks[0].new,
            vec![DiffLine {
                number: 6,
                text: "    return 42;".to_owned()
            }],
        );
    }

    // ── match_positions ──

    #[test]
    fn match_positions_finds_non_overlapping_offsets() {
        assert_eq!(match_positions("aXbXcXd", "X", usize::MAX), vec![1, 3, 5]);
    }

    #[test]
    fn match_positions_take_limits_count() {
        assert_eq!(match_positions("aXbXcX", "X", 2), vec![1, 3]);
    }

    #[test]
    fn match_positions_advances_past_pattern_to_avoid_overlap() {
        assert_eq!(match_positions("aaaa", "aa", usize::MAX), vec![0, 2]);
    }

    #[test]
    fn match_positions_no_match_is_empty() {
        assert_eq!(
            match_positions("hello", "xyz", usize::MAX),
            [] as [usize; 0]
        );
    }

    #[test]
    fn match_positions_take_zero_is_empty() {
        assert_eq!(match_positions("aXbXcX", "X", 0), [] as [usize; 0]);
    }

    // ── line_at_byte ──

    #[test]
    fn line_at_byte_first_line_is_one() {
        assert_eq!(line_at_byte("A\nB\n", 0), 1);
    }

    #[test]
    fn line_at_byte_after_newline_increments() {
        assert_eq!(line_at_byte("A\nB\nC\n", 2), 2);
        assert_eq!(line_at_byte("A\nB\nC\n", 4), 3);
    }

    #[test]
    fn line_at_byte_end_of_file_after_trailing_newline() {
        assert_eq!(line_at_byte("A\n", 2), 2);
    }

    #[test]
    fn line_at_byte_end_of_file_without_trailing_newline() {
        assert_eq!(line_at_byte("AB", 2), 1);
        assert_eq!(line_at_byte("A\nB", 3), 2);
    }

    // ── split_into_diff_lines ──

    #[test]
    fn split_into_diff_lines_numbers_from_start_line() {
        assert_eq!(
            split_into_diff_lines("a\nb", 47),
            vec![
                DiffLine {
                    number: 47,
                    text: "a".to_owned()
                },
                DiffLine {
                    number: 48,
                    text: "b".to_owned()
                },
            ],
        );
    }

    #[test]
    fn split_into_diff_lines_drops_trailing_newline() {
        assert_eq!(
            split_into_diff_lines("a\nb\n", 1),
            vec![
                DiffLine {
                    number: 1,
                    text: "a".to_owned()
                },
                DiffLine {
                    number: 2,
                    text: "b".to_owned()
                },
            ],
        );
    }

    #[test]
    fn split_into_diff_lines_empty_yields_empty() {
        assert_eq!(split_into_diff_lines("", 1), []);
    }

    // ── trim_chunk ──

    #[test]
    fn trim_chunk_drops_matching_prefix_and_suffix_preserving_numbers() {
        let mut chunk = DiffChunk {
            old: vec![
                DiffLine {
                    number: 10,
                    text: "anchor".to_owned(),
                },
                DiffLine {
                    number: 11,
                    text: "old".to_owned(),
                },
                DiffLine {
                    number: 12,
                    text: "tail".to_owned(),
                },
            ],
            new: vec![
                DiffLine {
                    number: 10,
                    text: "anchor".to_owned(),
                },
                DiffLine {
                    number: 11,
                    text: "new".to_owned(),
                },
                DiffLine {
                    number: 12,
                    text: "tail".to_owned(),
                },
            ],
        };
        trim_chunk(&mut chunk);
        assert_eq!(
            chunk.old,
            vec![DiffLine {
                number: 11,
                text: "old".to_owned()
            }]
        );
        assert_eq!(
            chunk.new,
            vec![DiffLine {
                number: 11,
                text: "new".to_owned()
            }]
        );
    }

    #[test]
    fn trim_chunk_pure_tail_insertion_strips_anchor() {
        let mut chunk = DiffChunk {
            old: vec![DiffLine {
                number: 5,
                text: "fn foo()".to_owned(),
            }],
            new: vec![
                DiffLine {
                    number: 5,
                    text: "fn foo()".to_owned(),
                },
                DiffLine {
                    number: 6,
                    text: "    return 42;".to_owned(),
                },
            ],
        };
        trim_chunk(&mut chunk);
        assert_eq!(chunk.old, []);
        assert_eq!(
            chunk.new,
            vec![DiffLine {
                number: 6,
                text: "    return 42;".to_owned()
            }],
        );
    }

    #[test]
    fn trim_chunk_fully_identical_collapses_both_sides() {
        let mut chunk = DiffChunk {
            old: vec![DiffLine {
                number: 1,
                text: "a".to_owned(),
            }],
            new: vec![DiffLine {
                number: 1,
                text: "a".to_owned(),
            }],
        };
        trim_chunk(&mut chunk);
        assert_eq!(chunk.old, []);
        assert_eq!(chunk.new, []);
    }

    // ── common_boundaries ──

    #[test]
    fn common_boundaries_produces_prefix_and_suffix_counts() {
        let old = ["a", "b", "c", "d"];
        let new = ["a", "X", "Y", "d"];
        assert_eq!(common_boundaries(&old, &new), (1, 1));
    }

    #[test]
    fn common_boundaries_disjoint_is_zero() {
        assert_eq!(common_boundaries(&["a"], &["b"]), (0, 0));
    }

    #[test]
    fn common_boundaries_empty_inputs_have_zero_boundaries() {
        let empty: [&str; 0] = [];
        assert_eq!(common_boundaries(&empty, &empty), (0, 0));
    }

    #[test]
    fn common_boundaries_asymmetric_lengths_capped_by_shorter_side() {
        assert_eq!(common_boundaries(&["a", "b", "a"], &["a", "X"]), (1, 0));
        assert_eq!(common_boundaries(&["a", "X"], &["a", "b", "a"]), (1, 0));
    }

    // ── dominant_eol ──

    #[test]
    fn dominant_eol_lf_only_selects_lf() {
        assert_eq!(dominant_eol("a\nb\n"), "\n");
    }

    #[test]
    fn dominant_eol_crlf_only_selects_crlf() {
        assert_eq!(dominant_eol("a\r\nb\r\n"), "\r\n");
    }

    #[test]
    fn dominant_eol_mixed_favors_majority() {
        assert_eq!(dominant_eol("a\nb\r\nc\r\n"), "\r\n");
        assert_eq!(dominant_eol("a\nb\nc\r\n"), "\n");
    }

    #[test]
    fn dominant_eol_tie_defaults_to_lf() {
        assert_eq!(dominant_eol("a\nb\r\n"), "\n");
    }

    #[test]
    fn dominant_eol_no_newlines_defaults_to_lf() {
        assert_eq!(dominant_eol("no newlines"), "\n");
    }

    // ── normalize_eol ──

    #[test]
    fn normalize_eol_converts_crlf_to_lf() {
        let out = normalize_eol("a\r\nb\r\n");
        assert_eq!(out, "a\nb\n");
        assert!(matches!(out, Cow::Owned(_)));
    }

    #[test]
    fn normalize_eol_lf_input_borrows() {
        let out = normalize_eol("a\nb\n");
        assert_eq!(out, "a\nb\n");
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    // ── apply_eol ──

    #[test]
    fn apply_eol_inserts_cr_for_crlf() {
        assert_eq!(apply_eol("a\nb\n".to_owned(), "\r\n"), "a\r\nb\r\n");
    }

    #[test]
    fn apply_eol_lf_preserves_content() {
        assert_eq!(apply_eol("a\nb\n".to_owned(), "\n"), "a\nb\n");
    }

    // ── synthesize_chunk ──

    #[test]
    fn synthesize_chunk_starts_numbering_at_one() {
        let chunk = synthesize_chunk("a\nb", "x\ny");
        assert_eq!(chunk.old[0].number, 1);
        assert_eq!(chunk.new[0].number, 1);
    }

    #[test]
    fn synthesize_chunk_applies_trim() {
        let chunk = synthesize_chunk("fn foo()", "fn foo()\n    body");
        assert_eq!(chunk.old, []);
        assert_eq!(chunk.new.len(), 1);
        assert_eq!(chunk.new[0].text, "    body");
    }
}
