use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;

use super::{Tool, ToolOutput, extract_input_field, summarize_path_call};
use crate::file_tracker::{FileTracker, GatePurpose, StatCheck};

pub(crate) struct WriteTool {
    tracker: Arc<FileTracker>,
}

impl WriteTool {
    pub(crate) fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Write content to a file, creating it if it does not exist or overwriting if it does. \
         Overwriting an existing file requires that file to have been Read fully in this session \
         first; the same Read-before-Edit gate that protects the Edit tool refuses writes to files \
         the model hasn't seen and to files that changed externally since the last Read. \
         Creating a brand-new file bypasses the gate."
    }

    fn input_schema(&self) -> serde_json::Value {
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

    fn icon(&self) -> &'static str {
        "←"
    }

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "file_path")
    }

    fn summarize_call(&self, input: &serde_json::Value) -> String {
        summarize_path_call(self.name(), input, "file_path")
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
    content: String,
}

// ── Execution ──

async fn run(raw: serde_json::Value, tracker: Arc<FileTracker>) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let (result, is_new) = write_file(&input.file_path, &input.content, &tracker).await;
    let name = super::file_name(&input.file_path);
    let verb = if is_new { "Created" } else { "Updated" };
    ToolOutput::from_result(result).with_title(format!("{verb} {name}"))
}

async fn write_file(
    path: &str,
    content: &str,
    tracker: &FileTracker,
) -> (Result<String, String>, bool) {
    let file_path = Path::new(path);
    let pre_meta = match tokio::fs::metadata(path).await {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return (Err(format!("Error reading {path}: {e}")), false),
    };
    let is_new = pre_meta.is_none();

    // Existing files run the strict gate; new files bypass — there is
    // nothing to clobber.
    if let Some(meta) = &pre_meta
        && let Err(msg) = check_gate(file_path, meta, path, tracker).await
    {
        return (Err(msg), is_new);
    }

    if let Some(parent) = file_path.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return (Err(format!("Failed to create directory: {e}")), is_new);
    }

    if let Err(e) = tokio::fs::write(path, content).await {
        return (Err(format!("Failed to write file: {e}")), is_new);
    }

    tracker
        .record_modify_after_write(file_path, content.as_bytes())
        .await;

    let msg = if is_new {
        format!("Successfully created {path}.")
    } else {
        format!("Successfully updated {path}.")
    };
    (Ok(msg), is_new)
}

/// Runs the existing-file gate ladder. Stat-match short-circuits; on
/// drift the file is read once to confirm a content-preserving touch
/// (cloud-sync) before letting the write proceed. Structural
/// rejects (never-read, partial-view) surface the model-facing
/// `GateError` rendered via `Display`.
async fn check_gate(
    file_path: &Path,
    meta: &std::fs::Metadata,
    path: &str,
    tracker: &FileTracker,
) -> Result<(), String> {
    let mtime = meta
        .modified()
        .map_err(|e| format!("Error reading {path}: {e}"))?;
    let stat_check = tracker
        .check_stat(file_path, mtime, meta.len(), GatePurpose::Write)
        .map_err(|e| e.to_string())?;
    if let StatCheck::NeedsBytes { stored_hash } = stat_check {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| format!("Error reading {path}: {e}"))?;
        FileTracker::verify_drift_bytes(file_path, &bytes, stored_hash, GatePurpose::Write)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_tracker::LastView;
    use crate::file_tracker::testing::{seed_full_read, tracker};

    // ── run ──

    #[tokio::test]
    async fn run_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");

        let output = run(
            serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "content": "hello world"
            }),
            tracker(),
        )
        .await;

        assert!(!output.is_error);
        assert!(output.content.contains("created"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn run_missing_required_fields() {
        let output = run(serde_json::json!({"file_path": "/tmp/x"}), tracker()).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_overwrites_existing_file_uses_updated_verb() {
        // The new-vs-existing branch picks the title verb, so a fresh
        // `run` against an existing file pins the `Updated` arm — the
        // "Created" branch is already pinned by `run_creates_file`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old").unwrap();
        let tracker_arc = Arc::new(FileTracker::default());
        seed_full_read(&tracker_arc, &path);

        let output = run(
            serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "content": "new",
            }),
            tracker_arc,
        )
        .await;

        assert!(!output.is_error);
        assert_eq!(
            output.metadata.title.as_deref(),
            Some("Updated existing.txt"),
            "overwrite must surface as Updated, not Created",
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[tokio::test]
    async fn write_tool_run_dispatches_through_trait_to_inner_run() {
        // Mirrors `edit_tool_run_dispatches_through_trait_to_inner_run`:
        // the trait shim is a four-line `Box::pin(run(...))`, but it
        // owns the `Arc::clone` that hands the tracker to the future,
        // so the wiring still deserves a coverage anchor.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let tracker_arc = Arc::new(FileTracker::default());
        let tool = WriteTool::new(Arc::clone(&tracker_arc));

        let output = tool
            .run(serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "content": "hello",
            }))
            .await;

        assert!(!output.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    // ── write_file ──

    #[tokio::test]
    async fn write_file_creates_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");

        let (result, is_new) =
            write_file(path.to_str().unwrap(), "content", &FileTracker::default()).await;
        assert!(result.unwrap().contains("created"));
        assert!(is_new);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content");
    }

    #[tokio::test]
    async fn write_file_existing_without_read_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old content").unwrap();

        let (result, is_new) = write_file(
            path.to_str().unwrap(),
            "new content",
            &FileTracker::default(),
        )
        .await;
        let err = result.unwrap_err();
        assert!(
            err.contains("not been read"),
            "expected must-read-first error, got: {err}",
        );
        assert!(!is_new);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old content");
    }

    #[tokio::test]
    async fn write_file_after_read_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old content").unwrap();

        let tracker = FileTracker::default();
        seed_full_read(&tracker, &path);

        let (result, is_new) = write_file(path.to_str().unwrap(), "new content", &tracker).await;
        assert!(result.unwrap().contains("updated"));
        assert!(!is_new);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
    }

    #[tokio::test]
    async fn write_file_after_external_modification_is_rejected() {
        // Read at one mtime, then bump the mtime to simulate an
        // external editor saving over our state. The drift hash
        // mismatch surfaces the "modified externally" error.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old content").unwrap();

        let tracker = FileTracker::default();
        seed_full_read(&tracker, &path);
        std::fs::write(&path, "external edit").unwrap();

        let (result, _) = write_file(path.to_str().unwrap(), "our edit", &tracker).await;
        let err = result.unwrap_err();
        assert!(
            err.contains("modified externally"),
            "drift error expected, got: {err}",
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external edit");
    }

    #[tokio::test]
    async fn write_file_phantom_drift_passes_via_hash_match() {
        // Cloud-sync touch on the write side: stat says the file
        // changed, but the bytes haven't. The gate must rehash and
        // accept; without the fallback the write would be rejected
        // even though no real conflict exists.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "stable content").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let stale_mtime = meta.modified().unwrap() - std::time::Duration::from_mins(1);

        let tracker = FileTracker::default();
        tracker.record_read(&path, &bytes, stale_mtime, meta.len(), LastView::Full);

        let (result, is_new) = write_file(path.to_str().unwrap(), "fresh content", &tracker).await;
        result.expect("phantom drift must not block write");
        assert!(!is_new);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh content");
    }

    #[tokio::test]
    async fn write_file_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c.txt");

        let (result, _) = write_file(path.to_str().unwrap(), "deep", &FileTracker::default()).await;
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "deep");
    }

    #[tokio::test]
    async fn write_file_empty_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");

        let (result, _) = write_file(path.to_str().unwrap(), "", &FileTracker::default()).await;
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[tokio::test]
    async fn write_file_fails_when_parent_is_a_file() {
        // The parent component is a regular file, so `metadata()`
        // returns ENOTDIR. The stat error is surfaced via the same
        // `Error reading {path}: {e}` shape as edit.rs.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "I am a file").unwrap();

        let path = blocker.join("child.txt");
        let path_str = path.to_str().unwrap();
        let (result, is_new) = write_file(path_str, "content", &FileTracker::default()).await;
        let err = result.unwrap_err();
        assert!(
            err.starts_with("Error reading ") && err.contains(path_str),
            "expected stat error to mention the path, got: {err}",
        );
        assert!(
            !is_new,
            "ENOTDIR should not be classified as a new-file case"
        );
    }

    #[tokio::test]
    async fn write_file_unread_directory_hits_strict_gate() {
        // Existing directory: the gate fires before the OS would
        // reject the write because no Read entry exists.
        let dir = tempfile::tempdir().unwrap();
        let (result, _) = write_file(
            dir.path().to_str().unwrap(),
            "content",
            &FileTracker::default(),
        )
        .await;
        let err = result.unwrap_err();
        assert!(
            err.contains("not been read"),
            "expected must-read-first rejection for unread directory, got: {err}",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_fails_on_read_only_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.txt");
        std::fs::write(&path, "original").unwrap();
        let tracker = FileTracker::default();
        seed_full_read(&tracker, &path);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let (result, _) = write_file(path.to_str().unwrap(), "overwrite", &tracker).await;
        let err = result.unwrap_err();
        assert!(
            err.contains("Failed to write file"),
            "expected write-failure error, got: {err}",
        );
        // Permission denial did not corrupt the original bytes.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_fails_when_parent_creation_is_denied() {
        // Pin the create_dir_all failure branch: parent is missing AND
        // its ancestor is read-only, so mkdir denies. The pre-stat
        // returns NotFound (ENOENT walks past read-only into the
        // missing component), letting control reach the create_dir_all
        // call rather than short-circuiting at the metadata read. The
        // ancestor stays empty so tempdir cleanup can rmdir it without
        // re-chmod gymnastics.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let readonly = dir.path().join("readonly");
        std::fs::create_dir(&readonly).unwrap();
        let path = readonly.join("nested").join("file.txt");
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o555)).unwrap();

        let (result, is_new) =
            write_file(path.to_str().unwrap(), "content", &FileTracker::default()).await;

        let err = result.unwrap_err();
        assert!(
            err.starts_with("Failed to create directory:"),
            "expected mkdir denial, got: {err}",
        );
        assert!(
            is_new,
            "missing parent path is still classified as a new-file case",
        );
    }
}
