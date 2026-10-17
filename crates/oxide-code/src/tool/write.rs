//! File writing tool with directory creation.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde::Deserialize;

use super::{
    ApprovalBody, ApprovalPreview, Prepared, Tool, ToolCtx, ToolOutput, ToolSpec, WriteMode,
    extract_input_field, summarize_path_call,
};
use crate::file_tracker::{FileTracker, GatePurpose};
use crate::permission::PermissionClass;

pub(crate) struct WriteTool;

impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".to_owned(),
            description: "Write content to a file, creating it if it does not exist or \
                          overwriting it if it does. Before overwriting an existing file, Read \
                          all of it in this session, in one Read or by paging with offset and \
                          limit. The Read-before-Edit gate shared with the Edit tool also refuses \
                          files that changed externally since the last Read. Creating a new file \
                          skips the gate."
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
        "←"
    }

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "file_path")
    }

    fn summarize_call(&self, name: &str, input: &serde_json::Value, cwd: &Path) -> String {
        summarize_path_call(name, input, "file_path", cwd)
    }

    fn approval_preview(
        &self,
        name: &str,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> ApprovalPreview {
        let content = extract_input_field(input, "content").unwrap_or_default();
        let current = extract_input_field(input, "file_path")
            .map(|path| current_text(&cwd.join(path), path))
            .unwrap_or_default();
        ApprovalPreview {
            title: self.summarize_call(name, input, cwd),
            body: ApprovalBody::Diff(vec![super::edit::synthesize_chunk(&current, content)]),
        }
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(run(prepared.input, ctx))
    }
}

/// The text an overwrite of `path`, spelled `shown`, would replace. A missing file, or one the
/// write refuses (a symlink, a non-regular or oversized file), previews as empty.
fn current_text(path: &Path, shown: &str) -> String {
    let writable = std::fs::symlink_metadata(path)
        .is_ok_and(|meta| validate_write_target(shown, &meta).is_ok());
    if !writable {
        return String::new();
    }
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

// ── Input ──

fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "The absolute path to the file to write"
            },
            "content": {
                "type": "string",
                "description": "The content to write to the file"
            }
        },
        "required": ["file_path", "content"]
    })
}

#[derive(Deserialize)]
struct Input {
    file_path: String,
    content: String,
}

// ── Execution ──

async fn run(raw: serde_json::Value, ctx: &ToolCtx) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(e) => return e,
    };

    match write_file(&input.file_path, &input.content, ctx).await {
        Ok((content, is_new)) => {
            let verb = if is_new { "Created" } else { "Updated" };
            ToolOutput::success(content)
                .with_title(format!("{verb} {}", super::file_name(&input.file_path)))
        }
        Err(msg) => ToolOutput::error(msg),
    }
}

/// Returns `(message, is_new)` on success, where `is_new` means the write created the file. A
/// relative `path` resolves against the context's cwd.
async fn write_file(path: &str, content: &str, ctx: &ToolCtx) -> Result<(String, bool), String> {
    let file_path = ctx.cwd().join(path);
    let pre_meta = match tokio::fs::symlink_metadata(&file_path).await {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Error reading {path}: {e}")),
    };
    let is_new = pre_meta.is_none();

    if let Some(meta) = &pre_meta {
        validate_write_target(path, meta)?;
        check_gate(&file_path, path, &ctx.tracker).await?;
    }

    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directory {}: {e}", parent.display()))?;
    }

    let mode = if is_new {
        WriteMode::CreateNew
    } else {
        WriteMode::Overwrite
    };
    super::write_tracked(ctx, &file_path, content.as_bytes().to_vec(), mode)
        .await
        .map_err(|e| format!("Failed to write file: {e}"))?;

    let msg = if is_new {
        format!("Successfully created {path}.")
    } else {
        format!("Successfully updated {path}.")
    };
    Ok((msg, is_new))
}

/// Validates the un-followed `metadata` of an existing target. The gate pinned the path to its
/// resolved location, so a symlink here appeared afterwards and is refused.
fn validate_write_target(path: &str, metadata: &std::fs::Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "{path} is a symlink. Refusing to write through it."
        ));
    }
    super::validate_file_target(path, metadata, "write")
}

async fn check_gate(path: &Path, shown: &str, tracker: &FileTracker) -> Result<(), String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("Error reading {shown}: {e}"))?;
    tracker
        .verify_current_content(&super::tracker_key(path).await, &bytes, GatePurpose::Write)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_tracker::MAX_TRACKED_FILE_SIZE;
    use crate::file_tracker::testing::seed_full_read;
    use crate::tool::read::ReadTool;
    use crate::tool::testing::{self, canonical_tempdir, ctx, symlink_file, temp_ctx};

    // ── approval_preview ──

    #[test]
    fn approval_preview_renders_content_as_an_all_added_diff() {
        let tool = WriteTool;
        let preview = tool.approval_preview(
            "write",
            &serde_json::json!({
                "file_path": "/repo/src/new.rs",
                "content": "fn main() {}",
            }),
            Path::new("/repo"),
        );

        assert_eq!(preview.title, "Write(src/new.rs)");
        let ApprovalBody::Diff(chunks) = preview.body else {
            panic!("write preview should be a diff");
        };
        let chunk = &chunks[0];
        assert!(chunk.old.is_empty(), "a new file has no removed lines");
        assert!(chunk.new.iter().any(|l| l.text.contains("fn main() {}")));
    }

    #[test]
    fn approval_preview_diffs_an_overwrite_against_the_current_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "keep\nold line\n").unwrap();
        let tool = WriteTool;

        let preview = tool.approval_preview(
            "write",
            &serde_json::json!({
                "file_path": path,
                "content": "keep\nnew line\n",
            }),
            dir.path(),
        );
        let ApprovalBody::Diff(chunks) = preview.body else {
            panic!("write preview should be a diff");
        };
        let removed: Vec<_> = chunks[0].old.iter().map(|l| l.text.as_str()).collect();
        let added: Vec<_> = chunks[0].new.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(removed, ["old line"]);
        assert_eq!(added, ["new line"]);
    }

    #[test]
    fn approval_preview_does_not_read_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "token").unwrap();
        let link = dir.path().join("link.txt");
        symlink_file(&secret, &link);
        let tool = WriteTool;

        let preview = tool.approval_preview(
            "write",
            &serde_json::json!({
                "file_path": link,
                "content": "new",
            }),
            dir.path(),
        );
        let ApprovalBody::Diff(chunks) = preview.body else {
            panic!("write preview should be a diff");
        };
        assert!(
            chunks[0].old.is_empty(),
            "a refused target previews as empty"
        );
    }

    // ── run ──

    #[tokio::test]
    async fn run_new_file_reports_creation() {
        let dir = canonical_tempdir();
        let path = dir.path().join("new.txt");

        let input = serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "content": "hello world"
        });

        let output = testing::run(&WriteTool, input, &ctx(dir.path())).await;

        assert!(!output.is_error);
        assert!(output.content.contains("created"));
        assert_eq!(output.metadata.title.as_deref(), Some("Created new.txt"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn run_missing_required_fields_reports_input_error() {
        let output = run(serde_json::json!({"file_path": "/tmp/x"}), &temp_ctx()).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_overwrites_existing_file_uses_updated_verb() {
        // Pins the `Updated` arm. `run_creates_file` covers `Created`.
        let dir = canonical_tempdir();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old").unwrap();
        let ctx = temp_ctx();
        seed_full_read(&ctx.tracker, &path);

        let output = run(
            serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "content": "new",
            }),
            &ctx,
        )
        .await;

        assert!(!output.is_error);
        assert_eq!(
            output.metadata.title.as_deref(),
            Some("Updated existing.txt"),
            "overwrite should surface as Updated",
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[tokio::test]
    async fn run_gate_rejection_omits_title() {
        let dir = canonical_tempdir();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old").unwrap();

        let output = run(
            serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "content": "new",
            }),
            &temp_ctx(),
        )
        .await;

        assert!(output.is_error);
        assert_eq!(
            output.metadata.title, None,
            "error path must not claim the write happened",
        );
    }

    // ── write_file ──

    #[tokio::test]
    async fn write_file_new_path_creates_content() {
        let dir = canonical_tempdir();
        let path = dir.path().join("new.txt");

        let (msg, is_new) = write_file(path.to_str().unwrap(), "content", &temp_ctx())
            .await
            .unwrap();
        assert!(msg.contains("created"));
        assert!(is_new);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content");
    }

    #[tokio::test]
    async fn write_file_resolves_a_relative_path_against_the_ctx_cwd() {
        let dir = canonical_tempdir();
        let ctx = ctx(dir.path());

        let (msg, is_new) = write_file("notes/new.txt", "content", &ctx).await.unwrap();

        assert_eq!(msg, "Successfully created notes/new.txt.");
        assert!(is_new);
        let path = dir.path().join("notes/new.txt");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content");
        assert_eq!(
            ctx.tracker
                .verify_current_content(&path, b"content", GatePurpose::Write),
            Ok(()),
        );
    }

    #[tokio::test]
    async fn write_file_existing_without_read_is_rejected() {
        let dir = canonical_tempdir();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old content").unwrap();

        let err = write_file(path.to_str().unwrap(), "new content", &temp_ctx())
            .await
            .unwrap_err();
        assert!(
            err.contains("has not been read"),
            "expected must-read-first error, got: {err}",
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old content");
    }

    #[tokio::test]
    async fn write_file_after_read_overwrites_existing() {
        let dir = canonical_tempdir();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old content").unwrap();

        let ctx = temp_ctx();
        seed_full_read(&ctx.tracker, &path);

        let (msg, is_new) = write_file(path.to_str().unwrap(), "new content", &ctx)
            .await
            .unwrap();
        assert!(msg.contains("updated"));
        assert!(!is_new);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
    }

    #[tokio::test]
    async fn write_file_needs_every_line_read_across_paged_reads() {
        let dir = canonical_tempdir();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let ctx = temp_ctx();
        let read = ReadTool;

        let first = testing::run(
            &read,
            serde_json::json!({"file_path": path, "limit": 2}),
            &ctx,
        )
        .await;
        assert!(!first.is_error, "{}", first.content);
        let err = write_file(path.to_str().unwrap(), "new", &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("only been read in part"), "{err}");

        let rest = testing::run(
            &read,
            serde_json::json!({"file_path": path, "offset": 3}),
            &ctx,
        )
        .await;
        assert!(!rest.is_error, "{}", rest.content);
        write_file(path.to_str().unwrap(), "new", &ctx)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[tokio::test]
    async fn write_file_after_external_modification_is_rejected() {
        // Read, then overwrite to simulate an external editor. The drift rehash surfaces the error.
        let dir = canonical_tempdir();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old content").unwrap();

        let ctx = temp_ctx();
        seed_full_read(&ctx.tracker, &path);
        std::fs::write(&path, "external edit").unwrap();

        let err = write_file(path.to_str().unwrap(), "our edit", &ctx)
            .await
            .unwrap_err();
        assert!(
            err.contains("modified externally"),
            "drift error expected, got: {err}",
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external edit");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_records_a_new_file_under_its_canonical_key() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        let ctx = temp_ctx();

        let via_link = dir.path().join("link/new.txt");
        write_file(via_link.to_str().unwrap(), "fresh", &ctx)
            .await
            .unwrap();

        let real = dir.path().join("real/new.txt");
        let result = ctx
            .tracker
            .verify_current_content(&real, b"fresh", GatePurpose::Write);
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn write_file_creates_parent_directories() {
        let dir = canonical_tempdir();
        let path = dir.path().join("a").join("b").join("c.txt");

        write_file(path.to_str().unwrap(), "deep", &temp_ctx())
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "deep");
    }

    #[tokio::test]
    async fn write_file_empty_content_creates_empty_file() {
        let dir = canonical_tempdir();
        let path = dir.path().join("empty.txt");

        write_file(path.to_str().unwrap(), "", &temp_ctx())
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[tokio::test]
    async fn write_file_fails_when_parent_is_a_file() {
        // Unix fails the stat with ENOTDIR, and Windows, which reports the path as missing, fails
        // creating the parent. Either error names the blocking path.
        let dir = canonical_tempdir();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "I am a file").unwrap();

        let path = blocker.join("child.txt");
        let err = write_file(path.to_str().unwrap(), "content", &temp_ctx())
            .await
            .unwrap_err();
        assert!(
            err.contains(blocker.to_str().unwrap()),
            "expected the error to name the blocking file, got: {err}",
        );
        assert_eq!(std::fs::read_to_string(&blocker).unwrap(), "I am a file");
    }

    #[tokio::test]
    async fn write_file_rejects_directory_before_gate() {
        let dir = canonical_tempdir();
        let err = write_file(dir.path().to_str().unwrap(), "content", &temp_ctx())
            .await
            .unwrap_err();
        assert!(
            err.contains("is a directory"),
            "expected directory rejection, got: {err}",
        );
    }

    #[tokio::test]
    async fn write_file_rejects_oversized_file_before_gate() {
        let dir = canonical_tempdir();
        let path = dir.path().join("huge.txt");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_TRACKED_FILE_SIZE + 1).unwrap();

        let err = write_file(path.to_str().unwrap(), "content", &temp_ctx())
            .await
            .unwrap_err();
        assert!(
            err.contains("too large"),
            "expected size-limit rejection, got: {err}",
        );
    }

    #[tokio::test]
    async fn write_file_refuses_a_dangling_symlink_at_the_final_component() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join("outside")).unwrap();
        let target = dir.path().join("outside").join("escape.txt");
        let link = dir.path().join("notes.md");
        symlink_file(&target, &link);

        let err = write_file(link.to_str().unwrap(), "payload", &temp_ctx())
            .await
            .unwrap_err();
        assert!(
            err.contains("is a symlink"),
            "expected symlink refusal, got: {err}"
        );
        assert!(!target.exists(), "the link target must not be created");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_rejects_non_regular_file_before_gate() {
        let dir = canonical_tempdir();
        let path = dir.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let err = write_file(path.to_str().unwrap(), "content", &temp_ctx())
            .await
            .unwrap_err();
        assert!(
            err.contains("not a regular file"),
            "expected non-regular-file rejection, got: {err}",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_fails_on_read_only_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = canonical_tempdir();
        let path = dir.path().join("locked.txt");
        std::fs::write(&path, "original").unwrap();
        let ctx = temp_ctx();
        seed_full_read(&ctx.tracker, &path);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let err = write_file(path.to_str().unwrap(), "overwrite", &ctx)
            .await
            .unwrap_err();
        assert!(
            err.contains("Failed to write file"),
            "expected write-failure error, got: {err}",
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_fails_when_parent_creation_is_denied() {
        // Missing parent under a read-only ancestor: pre-stat hits ENOENT so control reaches
        // `create_dir_all`, which then denies. Ancestor stays empty for tempdir cleanup.
        use std::os::unix::fs::PermissionsExt;

        let dir = canonical_tempdir();
        let readonly = dir.path().join("readonly");
        std::fs::create_dir(&readonly).unwrap();
        let path = readonly.join("nested").join("file.txt");
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o555)).unwrap();

        let err = write_file(path.to_str().unwrap(), "content", &temp_ctx())
            .await
            .unwrap_err();
        let expected = format!(
            "Failed to create directory {}: ",
            readonly.join("nested").display()
        );
        assert!(
            err.starts_with(&expected),
            "expected mkdir denial, got: {err}"
        );
    }
}
