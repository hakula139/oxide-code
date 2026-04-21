use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolOutput, extract_input_field};

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
    ToolOutput::from_result(
        edit_file(
            &input.file_path,
            &input.old_string,
            &input.new_string,
            input.replace_all,
        )
        .await,
    )
    .with_title(format!("Edited {name}"))
}

async fn edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, String> {
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

    if replace_all && match_count > 1 {
        Ok(format!("Replaced {match_count} occurrences in {path}."))
    } else {
        Ok(format!("Successfully edited {path}."))
    }
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

    // Name / icon / schema coverage lives in `tool::tests`.

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

        let msg = edit_file(path.to_str().unwrap(), "aaa", "ccc", true)
            .await
            .unwrap();

        assert!(msg.contains("2 occurrences"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ccc bbb ccc");
    }

    #[tokio::test]
    async fn edit_file_replace_all_single_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let msg = edit_file(path.to_str().unwrap(), "hello", "goodbye", true)
            .await
            .unwrap();

        assert!(msg.contains("Successfully edited"));
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
