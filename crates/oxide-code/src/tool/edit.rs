//! Exact string replacement tool for file editing.

use std::borrow::Cow;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;

use super::{
    DiffChunk, DiffLine, Tool, ToolMetadata, ToolOutput, ToolResultView, extract_input_field,
    summarize_path_call,
};
use crate::file_tracker::{FileTracker, GatePurpose, StatCheck};

const MAX_EDIT_FILE_SIZE: u64 = 10 * 1024 * 1024;

pub(crate) struct EditTool {
    tracker: Arc<FileTracker>,
}

impl EditTool {
    pub(crate) fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Perform exact string replacement in a file. \
         The old_string must be unique in the file unless replace_all is true. \
         The file must have been Read fully in this session first; \
         a Read-before-Edit gate refuses edits to files the model hasn't seen \
         and to files that changed externally since the last Read."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to modify"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to replace (must be unique unless replace_all is true)"
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

    fn icon(&self) -> &'static str {
        "✎"
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
        metadata: &ToolMetadata,
    ) -> Option<ToolResultView> {
        let old = input.get("old_string")?.as_str()?;
        let new = input.get("new_string")?.as_str()?;
        let replace_all = input
            .get("replace_all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let live_chunks = metadata.diff_chunks.as_ref().filter(|c| !c.is_empty());
        let chunks = live_chunks
            .cloned()
            .unwrap_or_else(|| vec![synthesize_chunk(old, new)]);
        let replacements = live_chunks
            .map(Vec::len)
            .or(metadata.replacements)
            .or_else(|| parse_replacement_count(content))
            .unwrap_or(1);
        Some(ToolResultView::Diff {
            chunks,
            replace_all,
            replacements,
        })
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
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

// ── Execution ──

async fn run(raw: serde_json::Value, tracker: Arc<FileTracker>) -> ToolOutput {
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
        &tracker,
    )
    .await
    {
        Ok((content, replacements, chunks)) => ToolOutput::from_result(Ok(content))
            .with_title(format!("Edited {name}"))
            .with_replacements(replacements)
            .with_diff_chunks(chunks),
        Err(msg) => ToolOutput::from_result(Err(msg)),
    }
}

/// Performs the in-place exact-string replacement.
///
/// Contract:
///
/// - `old_string` must occur verbatim in the file (whitespace and indentation included). Without
///   `replace_all`, ambiguous matches are rejected so the model cannot silently edit the wrong
///   site.
/// - The Read-before-Edit gate refuses files that haven't been read in this session and files
///   whose stat / hash drifted since the last Read.
/// - CRLF is preserved: the file is normalized to LF for matching, the dominant EOL is restored
///   on write so we don't flip line endings just because the caller used `\n` in `old_string`.
/// - Returns `(message, match_count, diff_chunks)` on success; `chunks` carry real post-edit line
///   numbers shifted by cumulative delta from earlier replacements.
async fn edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    tracker: &FileTracker,
) -> Result<(String, usize, Vec<DiffChunk>), String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty.".into());
    }

    if old_string == new_string {
        return Err("old_string and new_string are identical. No changes to make.".into());
    }

    let file_path = Path::new(path);
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Error reading {path}: {e}"))?;

    if metadata.len() > MAX_EDIT_FILE_SIZE {
        let mb = super::bytes_to_mb(metadata.len());
        let limit_mb = MAX_EDIT_FILE_SIZE / (1024 * 1024);
        return Err(format!(
            "File is too large ({mb:.1} MB, max {limit_mb} MB). \
             Use the bash tool for large-file edits.",
        ));
    }

    let pre_mtime = metadata
        .modified()
        .map_err(|e| format!("Error reading {path}: {e}"))?;
    let stat_check = tracker
        .check_stat(file_path, pre_mtime, metadata.len(), GatePurpose::Edit)
        .map_err(|e| e.to_string())?;

    let content_bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("Error reading {path}: {e}"))?;
    if let StatCheck::NeedsBytes { stored_hash } = stat_check {
        FileTracker::verify_drift_bytes(file_path, &content_bytes, stored_hash, GatePurpose::Edit)
            .map_err(|e| e.to_string())?;
    }
    let content =
        String::from_utf8(content_bytes).map_err(|e| format!("Error reading {path}: {e}"))?;

    let eol = dominant_eol(&content);
    let content = normalize_eol(&content);
    let old_string = normalize_eol(old_string);
    let new_string = normalize_eol(new_string);

    let match_count = content.matches(old_string.as_ref()).count();
    if match_count == 0 {
        return Err(format!(
            "old_string not found in {path}. Make sure the string matches exactly, \
             including whitespace and indentation."
        ));
    }

    if match_count > 1 && !replace_all {
        return Err(format!(
            "Found {match_count} occurrences of old_string in {path}. \
             Set replace_all to true to replace all, or provide more context \
             to make old_string unique."
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

    tokio::fs::write(path, &updated)
        .await
        .map_err(|e| format!("Failed to write {path}: {e}"))?;

    tracker
        .record_modify_after_write(file_path, updated.as_bytes())
        .await;

    let message = if replace_all && match_count > 1 {
        format!("Replaced {match_count} occurrences in {path}.")
    } else {
        format!("Successfully edited {path}.")
    };
    Ok((message, match_count, chunks))
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
                .expect("cumulative line-shift fits in isize for sub-MAX_EDIT_FILE_SIZE inputs");
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

fn parse_replacement_count(content: &str) -> Option<usize> {
    content
        .strip_prefix("Replaced ")?
        .split_ascii_whitespace()
        .next()?
        .parse()
        .ok()
}

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
    use crate::file_tracker::LastView;
    use crate::file_tracker::testing::{tracker, tracker_seeded};

    // ── result_view ──

    #[test]
    fn result_view_is_none_when_new_string_missing_with_old_string_present() {
        let tool = EditTool::new(Arc::new(FileTracker::default()));
        let view = tool.result_view(
            &serde_json::json!({"old_string": "x"}),
            "Successfully edited /tmp/x.",
            &ToolMetadata::default(),
        );
        assert!(view.is_none());
    }

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
            replacements: Some(99),
            ..ToolMetadata::default()
        };
        let view = EditTool::new(tracker()).result_view(
            &input,
            "Replaced 2 occurrences in /tmp/f.rs.",
            &metadata,
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
        let view = EditTool::new(tracker()).result_view(
            &input,
            "Successfully edited /tmp/f.rs.",
            &ToolMetadata::default(),
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
    fn result_view_falls_back_to_parsing_content_when_metadata_is_empty() {
        let input = serde_json::json!({
            "file_path": "/tmp/f.rs",
            "old_string": "a",
            "new_string": "b",
            "replace_all": true,
        });
        let view = EditTool::new(tracker()).result_view(
            &input,
            "Replaced 7 occurrences in /tmp/f.rs.",
            &ToolMetadata::default(),
        );
        assert_eq!(
            view,
            Some(ToolResultView::Diff {
                chunks: vec![DiffChunk {
                    old: vec![DiffLine {
                        number: 1,
                        text: "a".to_owned()
                    }],
                    new: vec![DiffLine {
                        number: 1,
                        text: "b".to_owned()
                    }],
                }],
                replace_all: true,
                replacements: 7,
            }),
        );
    }

    #[test]
    fn result_view_defaults_to_one_replacement_when_count_missing() {
        let input = serde_json::json!({
            "file_path": "/tmp/f.rs",
            "old_string": "a",
            "new_string": "b",
            "replace_all": true,
        });
        let view = EditTool::new(tracker()).result_view(
            &input,
            "Successfully edited /tmp/f.rs.",
            &ToolMetadata::default(),
        );
        assert_eq!(
            view,
            Some(ToolResultView::Diff {
                chunks: vec![DiffChunk {
                    old: vec![DiffLine {
                        number: 1,
                        text: "a".to_owned()
                    }],
                    new: vec![DiffLine {
                        number: 1,
                        text: "b".to_owned()
                    }],
                }],
                replace_all: true,
                replacements: 1,
            }),
        );
    }

    #[test]
    fn result_view_is_none_when_required_inputs_missing() {
        let input = serde_json::json!({"file_path": "/tmp/x"});
        assert!(
            EditTool::new(tracker())
                .result_view(&input, "edited", &ToolMetadata::default())
                .is_none(),
        );
    }

    #[test]
    fn result_view_is_none_when_field_type_is_wrong() {
        let bad_old = serde_json::json!({
            "file_path": "/tmp/x",
            "old_string": 42,
            "new_string": "b",
        });
        assert!(
            EditTool::new(tracker())
                .result_view(&bad_old, "edited", &ToolMetadata::default())
                .is_none(),
        );
        let bad_new = serde_json::json!({
            "file_path": "/tmp/x",
            "old_string": "a",
            "new_string": 42,
        });
        assert!(
            EditTool::new(tracker())
                .result_view(&bad_new, "edited", &ToolMetadata::default())
                .is_none(),
        );
    }

    // ── run ──

    #[tokio::test]
    async fn run_valid_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let output = run(
            serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "old_string": "hello",
                "new_string": "goodbye"
            }),
            Arc::new(tracker_seeded(&path)),
        )
        .await;

        assert!(!output.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
        assert_eq!(
            output.metadata.title.as_deref(),
            Some("Edited test.txt"),
            "success path attaches the Edited title",
        );
    }

    #[tokio::test]
    async fn run_missing_required_fields() {
        let output = run(
            serde_json::json!({
                "file_path": "/tmp/x",
                "old_string": "a"
            }),
            tracker(),
        )
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_without_prior_read_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let output = run(
            serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "old_string": "hello",
                "new_string": "goodbye",
            }),
            tracker(),
        )
        .await;

        assert!(output.is_error);
        assert!(
            output.content.contains("not been read"),
            "expected must-read-first rejection, got: {}",
            output.content,
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn run_edit_error_omits_edited_title() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let output = run(
            serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "old_string": "not present",
                "new_string": "x",
            }),
            Arc::new(tracker_seeded(&path)),
        )
        .await;

        assert!(output.is_error);
        assert_eq!(
            output.metadata.title, None,
            "error path must not claim the edit happened",
        );
    }

    #[tokio::test]
    async fn edit_tool_run_dispatches_through_trait_to_inner_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();
        let tracker = Arc::new(tracker_seeded(&path));
        let tool = EditTool::new(Arc::clone(&tracker));

        let output = tool
            .run(serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "old_string": "hello",
                "new_string": "goodbye",
            }))
            .await;

        assert!(!output.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
    }

    // ── edit_file ──

    #[tokio::test]
    async fn edit_file_replaces_unique_string() {
        let dir = tempfile::tempdir().unwrap();
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
            &tracker_seeded(&path),
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
    async fn edit_file_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();

        let (msg, replacements, chunks) = edit_file(
            path.to_str().unwrap(),
            "aaa",
            "ccc",
            true,
            &tracker_seeded(&path),
        )
        .await
        .unwrap();

        assert!(msg.contains("2 occurrences"));
        assert_eq!(replacements, 2);
        assert_eq!(chunks.len(), 2, "replace_all emits one chunk per match");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ccc bbb ccc");
    }

    #[tokio::test]
    async fn edit_file_replace_all_pins_replaced_n_occurrences_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.txt");
        std::fs::write(&path, "a a a").unwrap();
        let (msg, replacements, _chunks) = edit_file(
            path.to_str().unwrap(),
            "a",
            "b",
            true,
            &tracker_seeded(&path),
        )
        .await
        .unwrap();
        assert_eq!(
            msg,
            format!("Replaced 3 occurrences in {}.", path.display())
        );
        assert_eq!(replacements, 3);
    }

    #[tokio::test]
    async fn edit_file_replace_all_single_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let (msg, replacements, chunks) = edit_file(
            path.to_str().unwrap(),
            "hello",
            "goodbye",
            true,
            &tracker_seeded(&path),
        )
        .await
        .unwrap();

        assert!(msg.contains("Successfully edited"));
        assert_eq!(
            replacements, 1,
            "single-match replace_all still replaces once"
        );
        assert_eq!(chunks.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
    }

    #[tokio::test]
    async fn edit_file_crlf_matching_preserves_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\r\nline2\r\n").unwrap();

        edit_file(
            path.to_str().unwrap(),
            "line1\nline2",
            "a\nb",
            false,
            &tracker_seeded(&path),
        )
        .await
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"a\r\nb\r\n");
    }

    #[tokio::test]
    async fn edit_file_crlf_in_new_string_not_doubled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "aaa\r\nbbb\r\n").unwrap();

        edit_file(
            path.to_str().unwrap(),
            "aaa",
            "x\r\ny",
            false,
            &tracker_seeded(&path),
        )
        .await
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"x\r\ny\r\nbbb\r\n");
    }

    #[tokio::test]
    async fn edit_file_mixed_eol_normalized_to_dominant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.txt");
        std::fs::write(&path, "aaa\nbbb\r\nreplace_me\r\n").unwrap();

        edit_file(
            path.to_str().unwrap(),
            "replace_me",
            "replaced",
            false,
            &tracker_seeded(&path),
        )
        .await
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"aaa\r\nbbb\r\nreplaced\r\n");
    }

    #[tokio::test]
    async fn edit_file_mixed_eol_multiline_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.txt");
        std::fs::write(&path, "foo\nbar\r\nbaz\r\n").unwrap();

        edit_file(
            path.to_str().unwrap(),
            "foo\nbar",
            "a\nb",
            false,
            &tracker_seeded(&path),
        )
        .await
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"a\r\nb\r\nbaz\r\n");
    }

    #[tokio::test]
    async fn edit_file_rejects_empty_old_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();

        let err = edit_file(
            path.to_str().unwrap(),
            "",
            "x",
            false,
            &FileTracker::default(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("must not be empty"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[tokio::test]
    async fn edit_file_rejects_identical_strings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();

        let err = edit_file(
            path.to_str().unwrap(),
            "hello",
            "hello",
            false,
            &FileTracker::default(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("identical"));
    }

    #[tokio::test]
    async fn edit_file_rejects_string_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let err = edit_file(
            path.to_str().unwrap(),
            "nonexistent",
            "replacement",
            false,
            &tracker_seeded(&path),
        )
        .await
        .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn edit_file_without_prior_read_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let err = edit_file(
            path.to_str().unwrap(),
            "hello",
            "goodbye",
            false,
            &FileTracker::default(),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("not been read"),
            "expected must-read-first rejection, got: {err}",
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn edit_file_after_external_modification_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();
        let tracker = tracker_seeded(&path);
        std::fs::write(&path, "external edit").unwrap();

        let err = edit_file(path.to_str().unwrap(), "external", "ours", false, &tracker)
            .await
            .unwrap_err();
        assert!(
            err.contains("modified externally"),
            "drift error expected, got: {err}",
        );
    }

    #[tokio::test]
    async fn edit_file_phantom_drift_passes_via_hash_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "old content").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let stale_mtime = meta.modified().unwrap() - std::time::Duration::from_mins(1);

        let tracker = FileTracker::default();
        tracker.record_read(&path, &bytes, stale_mtime, meta.len(), LastView::Full);

        let result = edit_file(
            path.to_str().unwrap(),
            "old content",
            "new content",
            false,
            &tracker,
        )
        .await;
        result.expect("phantom drift must not block edit");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
    }

    #[tokio::test]
    async fn edit_file_partial_read_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let tracker = FileTracker::default();
        tracker.record_read(
            &path,
            &bytes,
            meta.modified().unwrap(),
            meta.len(),
            LastView::Partial {
                offset: 1,
                limit: 1,
            },
        );

        let err = edit_file(path.to_str().unwrap(), "hello", "goodbye", false, &tracker)
            .await
            .unwrap_err();
        assert!(
            err.contains("partially"),
            "expected partial-view rejection, got: {err}",
        );
    }

    #[tokio::test]
    async fn edit_file_rejects_nonexistent_file() {
        let err = edit_file(
            "/nonexistent/file.txt",
            "a",
            "b",
            false,
            &FileTracker::default(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("Error reading"));
    }

    #[tokio::test]
    async fn edit_file_rejects_non_utf8_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        let bytes = [0xff_u8, 0xfe, 0xfd];
        std::fs::write(&path, bytes).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let tracker = FileTracker::default();
        tracker.record_read(
            &path,
            &bytes,
            meta.modified().unwrap(),
            meta.len(),
            LastView::Full,
        );

        let err = edit_file(path.to_str().unwrap(), "a", "b", false, &tracker)
            .await
            .unwrap_err();
        assert!(
            err.contains("Error reading") && err.contains("utf-8"),
            "expected utf-8 decode failure, got: {err}",
        );
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();

        let err = edit_file(
            path.to_str().unwrap(),
            "aaa",
            "ccc",
            false,
            &tracker_seeded(&path),
        )
        .await
        .unwrap_err();
        assert!(err.contains("2 occurrences"));
    }

    #[tokio::test]
    async fn edit_file_rejects_too_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.txt");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_EDIT_FILE_SIZE + 1).unwrap();

        let err = edit_file(
            path.to_str().unwrap(),
            "a",
            "b",
            false,
            &FileTracker::default(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("too large"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_fails_if_write_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readonly.txt");
        std::fs::write(&path, "hello world").unwrap();
        let tracker = tracker_seeded(&path);

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&path, perms).unwrap();

        let err = edit_file(path.to_str().unwrap(), "hello", "goodbye", false, &tracker)
            .await
            .unwrap_err();
        assert!(err.contains("Failed to write"));
    }

    #[tokio::test]
    async fn edit_file_chunks_carry_real_file_line_numbers_for_replace_all() {
        let dir = tempfile::tempdir().unwrap();
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

        let (_, replacements, chunks) = edit_file(
            path.to_str().unwrap(),
            "B",
            "X",
            true,
            &tracker_seeded(&path),
        )
        .await
        .unwrap();

        assert_eq!(replacements, 2);
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
        let dir = tempfile::tempdir().unwrap();
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

        let (_, _, chunks) = edit_file(
            path.to_str().unwrap(),
            "B",
            "X\nY",
            true,
            &tracker_seeded(&path),
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
        let dir = tempfile::tempdir().unwrap();
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

        let (_, _, chunks) = edit_file(
            path.to_str().unwrap(),
            "X\nY",
            "Z",
            true,
            &tracker_seeded(&path),
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
        assert!(chunks[0].old.is_empty());
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
        assert!(match_positions("hello", "xyz", usize::MAX).is_empty());
    }

    #[test]
    fn match_positions_take_zero_is_empty() {
        assert!(match_positions("aXbXcX", "X", 0).is_empty());
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
        assert!(split_into_diff_lines("", 1).is_empty());
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
        assert!(chunk.old.is_empty());
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
        assert!(chunk.old.is_empty());
        assert!(chunk.new.is_empty());
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
    fn common_boundaries_empty_inputs_return_zero() {
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
    fn dominant_eol_lf_only() {
        assert_eq!(dominant_eol("a\nb\n"), "\n");
    }

    #[test]
    fn dominant_eol_crlf_only() {
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
    fn dominant_eol_no_newlines() {
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
        assert_eq!(apply_eol("a\nb\n".into(), "\r\n"), "a\r\nb\r\n");
    }

    #[test]
    fn apply_eol_lf_unchanged() {
        assert_eq!(apply_eol("a\nb\n".into(), "\n"), "a\nb\n");
    }

    // ── parse_replacement_count ──

    #[test]
    fn parse_replacement_count_extracts_leading_integer() {
        assert_eq!(
            parse_replacement_count("Replaced 3 occurrences in /tmp/x."),
            Some(3),
        );
    }

    #[test]
    fn parse_replacement_count_is_none_for_unrelated_messages() {
        assert_eq!(parse_replacement_count("Successfully edited /tmp/x."), None);
        assert_eq!(parse_replacement_count(""), None);
    }

    #[test]
    fn parse_replacement_count_requires_space_after_replaced() {
        assert_eq!(parse_replacement_count("Replaced7 occurrences in x."), None);
    }

    #[test]
    fn parse_replacement_count_is_none_when_only_prefix_present() {
        assert_eq!(parse_replacement_count("Replaced "), None);
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
        assert!(chunk.old.is_empty());
        assert_eq!(chunk.new.len(), 1);
        assert_eq!(chunk.new[0].text, "    body");
    }
}
