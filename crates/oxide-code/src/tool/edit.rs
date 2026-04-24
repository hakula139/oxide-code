use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolMetadata, ToolOutput, ToolResultView, extract_input_field};

/// Per-file size cap for `edit` (10 MB). Generous because legitimate
/// edits sometimes target large config or data files.
const MAX_EDIT_FILE_SIZE: u64 = 10 * 1024 * 1024;

pub(crate) struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Perform exact string replacement in a file. \
         The old_string must be unique in the file unless replace_all is true."
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

    fn result_view(
        &self,
        input: &serde_json::Value,
        content: &str,
        metadata: &ToolMetadata,
    ) -> Option<ToolResultView> {
        let old = input.get("old_string")?.as_str()?.to_owned();
        let new = input.get("new_string")?.as_str()?.to_owned();
        let replace_all = input
            .get("replace_all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        // Live path: `run` sets `metadata.replacements` structurally.
        // Resume path (future commit): the session JSONL will persist
        // this too. Until then, the replay path falls back to parsing
        // the free-form success message.
        let replacements = metadata
            .replacements
            .or_else(|| parse_replacement_count(content))
            .unwrap_or(1);
        Some(ToolResultView::Diff {
            old,
            new,
            replace_all,
            replacements,
        })
    }

    fn run(
        &self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(run(input))
    }
}

// ── Result View ──

/// Parses the replacement count from the success-path output returned
/// by [`edit_file`] when `replace_all` hits multiple matches — a
/// `"Replaced N occurrences in <path>."` string. Returns `None` for
/// the single-match shape (`"Successfully edited ..."`), in which case
/// the caller defaults to 1.
///
/// The content-format contract this parser relies on is pinned by
/// the `edit_file_replace_all_pins_replaced_n_occurrences_format`
/// test so rewording the success string in `edit_file` breaks the
/// test, not the renderer silently.
fn parse_replacement_count(content: &str) -> Option<usize> {
    content
        .strip_prefix("Replaced ")?
        .split_ascii_whitespace()
        .next()?
        .parse()
        .ok()
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

async fn run(raw: serde_json::Value) -> ToolOutput {
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
    )
    .await
    {
        Ok((content, replacements)) => ToolOutput::from_result(Ok(content))
            .with_title(format!("Edited {name}"))
            .with_replacements(replacements),
        // Error path: leave `title` unset so the TUI falls back to
        // the neutral tool-call label — `✗ Edited {name}` would
        // read as a successful edit, contradicting the ✗ indicator.
        Err(msg) => ToolOutput::from_result(Err(msg)),
    }
}

async fn edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<(String, usize), String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty.".into());
    }

    if old_string == new_string {
        return Err("old_string and new_string are identical. No changes to make.".into());
    }

    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Error reading {path}: {e}"))?;

    if metadata.len() > MAX_EDIT_FILE_SIZE {
        #[expect(
            clippy::cast_precision_loss,
            reason = "file sizes are well within f64 range"
        )]
        let mb = metadata.len() as f64 / (1024.0 * 1024.0);
        let limit_mb = MAX_EDIT_FILE_SIZE / (1024 * 1024);
        return Err(format!(
            "File is too large ({mb:.1} MB, max {limit_mb} MB). \
             Use the bash tool for large-file edits.",
        ));
    }

    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("Error reading {path}: {e}"))?;

    let eol = dominant_eol(&content);
    let content = normalize_eol(content);
    let old_string = &normalize_eol(old_string.to_owned());
    let new_string = &normalize_eol(new_string.to_owned());

    let match_count = content.matches(old_string.as_str()).count();
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

    let updated = if replace_all {
        content.replace(old_string, new_string)
    } else {
        content.replacen(old_string, new_string, 1)
    };
    let updated = apply_eol(updated, eol);

    tokio::fs::write(path, &updated)
        .await
        .map_err(|e| format!("Failed to write {path}: {e}"))?;

    let message = if replace_all && match_count > 1 {
        format!("Replaced {match_count} occurrences in {path}.")
    } else {
        format!("Successfully edited {path}.")
    };
    Ok((message, match_count))
}

// ── Line Endings ──

/// Detects the dominant line ending style. Bare CR (`\r` without `\n`) is not
/// detected — such files are treated as LF and multi-line matches may fail.
fn dominant_eol(content: &str) -> &'static str {
    let crlf = content.matches("\r\n").count();
    // Each `\r\n` also contains a `\n`, so subtract to get the LF-only count.
    let lf_only = content.matches('\n').count() - crlf;
    if crlf > lf_only { "\r\n" } else { "\n" }
}

fn normalize_eol(content: String) -> String {
    if content.contains("\r\n") {
        content.replace("\r\n", "\n")
    } else {
        content
    }
}

fn apply_eol(content: String, eol: &str) -> String {
    if eol == "\r\n" {
        content.replace('\n', "\r\n")
    } else {
        content
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    // ── result_view ──

    #[test]
    fn result_view_extracts_diff_from_structured_inputs() {
        let input = serde_json::json!({
            "file_path": "/tmp/f.rs",
            "old_string": "fn foo()",
            "new_string": "fn bar()",
        });
        let view = EditTool.result_view(
            &input,
            "Successfully edited /tmp/f.rs.",
            &ToolMetadata::default(),
        );
        assert_eq!(
            view,
            Some(ToolResultView::Diff {
                old: "fn foo()".to_owned(),
                new: "fn bar()".to_owned(),
                replace_all: false,
                replacements: 1,
            }),
        );
    }

    #[test]
    fn result_view_reads_replacements_from_metadata_on_live_path() {
        // Live path: `run` attaches `metadata.replacements` via
        // `with_replacements`, so the renderer does NOT need to
        // re-parse prose. Pin the structural source of truth.
        let input = serde_json::json!({
            "file_path": "/tmp/f.rs",
            "old_string": "a",
            "new_string": "b",
            "replace_all": true,
        });
        let metadata = ToolMetadata {
            replacements: Some(7),
            ..ToolMetadata::default()
        };
        // Content deliberately inconsistent with metadata — metadata
        // wins. This locks in the "structured over parsed" priority.
        let view = EditTool.result_view(&input, "Successfully edited /tmp/f.rs.", &metadata);
        assert_eq!(
            view,
            Some(ToolResultView::Diff {
                old: "a".to_owned(),
                new: "b".to_owned(),
                replace_all: true,
                replacements: 7,
            }),
        );
    }

    #[test]
    fn result_view_falls_back_to_parsing_content_when_metadata_lacks_replacements() {
        // Resume path: session transcripts don't yet persist
        // metadata, so the TUI re-parses the success message. This
        // is the only remaining use of `parse_replacement_count`.
        let input = serde_json::json!({
            "file_path": "/tmp/f.rs",
            "old_string": "a",
            "new_string": "b",
            "replace_all": true,
        });
        let view = EditTool.result_view(
            &input,
            "Replaced 7 occurrences in /tmp/f.rs.",
            &ToolMetadata::default(),
        );
        assert_eq!(
            view,
            Some(ToolResultView::Diff {
                old: "a".to_owned(),
                new: "b".to_owned(),
                replace_all: true,
                replacements: 7,
            }),
        );
    }

    #[test]
    fn result_view_defaults_to_one_replacement_when_count_missing() {
        // Single-match edits return `"Successfully edited ..."` —
        // `parse_replacement_count` returns None, caller defaults to 1.
        let input = serde_json::json!({
            "file_path": "/tmp/f.rs",
            "old_string": "a",
            "new_string": "b",
            "replace_all": true,
        });
        let view = EditTool.result_view(
            &input,
            "Successfully edited /tmp/f.rs.",
            &ToolMetadata::default(),
        );
        assert_eq!(
            view,
            Some(ToolResultView::Diff {
                old: "a".to_owned(),
                new: "b".to_owned(),
                replace_all: true,
                replacements: 1,
            }),
        );
    }

    #[test]
    fn result_view_returns_none_when_required_inputs_missing() {
        // Malformed call (e.g., model emitted JSON missing `new_string`)
        // degrades to None so the caller falls back to Text rather
        // than panicking.
        let input = serde_json::json!({"file_path": "/tmp/x"});
        assert!(
            EditTool
                .result_view(&input, "edited", &ToolMetadata::default())
                .is_none(),
        );
    }

    #[test]
    fn result_view_returns_none_when_field_type_is_wrong() {
        // Either string field being the wrong JSON type must degrade
        // to None so the caller falls back to Text rather than
        // panicking on `as_str()?`. Cover both sides explicitly since
        // they're parallel `?` chains.
        let bad_old = serde_json::json!({
            "file_path": "/tmp/x",
            "old_string": 42,
            "new_string": "b",
        });
        assert!(
            EditTool
                .result_view(&bad_old, "edited", &ToolMetadata::default())
                .is_none(),
        );
        let bad_new = serde_json::json!({
            "file_path": "/tmp/x",
            "old_string": "a",
            "new_string": 42,
        });
        assert!(
            EditTool
                .result_view(&bad_new, "edited", &ToolMetadata::default())
                .is_none(),
        );
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
    fn parse_replacement_count_returns_none_for_unrelated_messages() {
        assert_eq!(parse_replacement_count("Successfully edited /tmp/x."), None);
        assert_eq!(parse_replacement_count(""), None);
    }

    #[test]
    fn parse_replacement_count_requires_space_after_replaced() {
        // The leading `"Replaced "` prefix (with trailing space) is the
        // structural separator — `"Replaced7 occurrences ..."` is not
        // the format `edit_file` emits and must not parse, otherwise a
        // mutation that drops the space from the prefix would go
        // unnoticed.
        assert_eq!(parse_replacement_count("Replaced7 occurrences in x."), None);
    }

    // ── run ──

    #[tokio::test]
    async fn run_valid_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let output = run(serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "old_string": "hello",
            "new_string": "goodbye"
        }))
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
        let output = run(serde_json::json!({
            "file_path": "/tmp/x",
            "old_string": "a"
        }))
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_edit_error_omits_edited_title() {
        // Failing edits (old_string not found, missing file, etc.)
        // must leave `title` unset so the TUI header falls back to
        // the neutral call label rather than rendering
        // `✗ Edited <name>`, which contradicts the error indicator.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let output = run(serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "old_string": "not present",
            "new_string": "x",
        }))
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

        let (msg, replacements) = edit_file(path.to_str().unwrap(), "aaa", "ccc", true)
            .await
            .unwrap();

        assert!(msg.contains("2 occurrences"));
        assert_eq!(replacements, 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ccc bbb ccc");
    }

    #[tokio::test]
    async fn edit_file_replace_all_pins_replaced_n_occurrences_format() {
        // [`parse_replacement_count`] reads the replacement count out
        // of this exact string to drive the TUI's "applied to N
        // matches" footer. Rewording the prefix or spacing silently
        // breaks that parser — pin the full shape here so the
        // coupling is visible in this test file rather than only
        // manifesting as a missing footer in the rendered diff.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.txt");
        std::fs::write(&path, "a a a").unwrap();
        let (msg, replacements) = edit_file(path.to_str().unwrap(), "a", "b", true)
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

        let (msg, replacements) = edit_file(path.to_str().unwrap(), "hello", "goodbye", true)
            .await
            .unwrap();

        assert!(msg.contains("Successfully edited"));
        assert_eq!(
            replacements, 1,
            "single-match replace_all still replaces once"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
    }

    #[tokio::test]
    async fn edit_file_crlf_matching_preserves_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\r\nline2\r\n").unwrap();

        edit_file(path.to_str().unwrap(), "line1\nline2", "a\nb", false)
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

        // new_string contains \r\n — should be normalized before apply_eol.
        edit_file(path.to_str().unwrap(), "aaa", "x\r\ny", false)
            .await
            .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        // \r\n in new_string is normalized to \n, then restored to \r\n — not \r\r\n.
        assert_eq!(bytes, b"x\r\ny\r\nbbb\r\n");
    }

    #[tokio::test]
    async fn edit_file_mixed_eol_normalized_to_dominant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.txt");
        // 2 CRLF, 1 LF → dominant is CRLF.
        std::fs::write(&path, "aaa\nbbb\r\nreplace_me\r\n").unwrap();

        edit_file(path.to_str().unwrap(), "replace_me", "replaced", false)
            .await
            .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        // All line endings normalized to the dominant style (CRLF).
        assert_eq!(bytes, b"aaa\r\nbbb\r\nreplaced\r\n");
    }

    #[tokio::test]
    async fn edit_file_mixed_eol_multiline_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.txt");
        // LF between first two lines, CRLF after — previously failed to match.
        std::fs::write(&path, "foo\nbar\r\nbaz\r\n").unwrap();

        edit_file(path.to_str().unwrap(), "foo\nbar", "a\nb", false)
            .await
            .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        // Dominant is CRLF (2 vs 1), so all newlines become CRLF.
        assert_eq!(bytes, b"a\r\nb\r\nbaz\r\n");
    }

    #[tokio::test]
    async fn edit_file_rejects_empty_old_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();

        let err = edit_file(path.to_str().unwrap(), "", "x", false)
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

        let err = edit_file(path.to_str().unwrap(), "hello", "hello", false)
            .await
            .unwrap_err();
        assert!(err.contains("identical"));
    }

    #[tokio::test]
    async fn edit_file_rejects_string_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let err = edit_file(path.to_str().unwrap(), "nonexistent", "replacement", false)
            .await
            .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn edit_file_rejects_nonexistent_file() {
        let err = edit_file("/nonexistent/file.txt", "a", "b", false)
            .await
            .unwrap_err();
        assert!(err.contains("Error reading"));
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();

        let err = edit_file(path.to_str().unwrap(), "aaa", "ccc", false)
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

        let err = edit_file(path.to_str().unwrap(), "a", "b", false)
            .await
            .unwrap_err();
        assert!(err.contains("too large"));
    }

    #[tokio::test]
    async fn edit_file_fails_if_write_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readonly.txt");
        std::fs::write(&path, "hello world").unwrap();

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&path, perms).unwrap();

        let err = edit_file(path.to_str().unwrap(), "hello", "goodbye", false)
            .await
            .unwrap_err();
        assert!(err.contains("Failed to write"));
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
        assert_eq!(normalize_eol("a\r\nb\r\n".into()), "a\nb\n");
    }

    #[test]
    fn normalize_eol_lf_unchanged() {
        assert_eq!(normalize_eol("a\nb\n".into()), "a\nb\n");
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
}
