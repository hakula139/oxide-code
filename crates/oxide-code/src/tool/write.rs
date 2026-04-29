use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use xxhash_rust::xxh64::xxh64;

use super::tracker::{FileTracker, GatePurpose, HASH_SEED, PreModifyCheck};
use super::{Tool, ToolOutput, extract_input_field, summarize_path_call};

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
        "Write content to a file, creating it if it does not exist or overwriting if it does."
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
    let pre_meta = tokio::fs::metadata(path).await;
    let is_new = matches!(&pre_meta, Err(e) if e.kind() == std::io::ErrorKind::NotFound);

    // Existing files run the strict gate; new files bypass — there is
    // nothing to clobber.
    if let Ok(meta) = &pre_meta
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

    if let Ok(meta) = tokio::fs::metadata(path).await
        && let Ok(mtime) = meta.modified()
    {
        tracker.record_modify(file_path, content.as_bytes(), mtime, meta.len());
    }

    let msg = if is_new {
        format!("Successfully created {path}.")
    } else {
        format!("Successfully updated {path}.")
    };
    (Ok(msg), is_new)
}

/// Runs the existing-file gate ladder. `Pass` short-circuits on
/// stat-match; `Drift` reads the file once to confirm a content-
/// preserving touch (cloud-sync) before letting the write proceed;
/// `Reject` surfaces the user-facing error.
async fn check_gate(
    file_path: &Path,
    meta: &std::fs::Metadata,
    path: &str,
    tracker: &FileTracker,
) -> Result<(), String> {
    let mtime = meta
        .modified()
        .map_err(|_| format!("Failed to read metadata for {path}"))?;
    match tracker.pre_modify_check(file_path, mtime, meta.len(), GatePurpose::Write) {
        PreModifyCheck::Pass => Ok(()),
        PreModifyCheck::Reject(msg) => Err(msg),
        PreModifyCheck::Drift { stored_hash } => {
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|e| format!("Error reading {path}: {e}"))?;
            tracker.confirm_drift_unchanged(
                stored_hash,
                xxh64(&bytes, HASH_SEED),
                GatePurpose::Write,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tracker::LastView;
    use super::*;

    fn tracker() -> Arc<FileTracker> {
        Arc::new(FileTracker::new())
    }

    /// Records a full Read of `path` so the gate has a baseline entry,
    /// mirroring what a real Read turn would have stored.
    fn seed_full_read(tracker: &FileTracker, path: &Path) {
        let bytes = std::fs::read(path).unwrap();
        let meta = std::fs::metadata(path).unwrap();
        tracker.record_read(
            path,
            &bytes,
            meta.modified().unwrap(),
            meta.len(),
            LastView::Full,
        );
    }

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

    // ── write_file ──

    #[tokio::test]
    async fn write_file_creates_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");

        let (result, is_new) =
            write_file(path.to_str().unwrap(), "content", &FileTracker::new()).await;
        assert!(result.unwrap().contains("created"));
        assert!(is_new);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content");
    }

    #[tokio::test]
    async fn write_file_existing_without_read_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old content").unwrap();

        let (result, is_new) =
            write_file(path.to_str().unwrap(), "new content", &FileTracker::new()).await;
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

        let tracker = FileTracker::new();
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

        let tracker = FileTracker::new();
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
    async fn write_file_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c.txt");

        let (result, _) = write_file(path.to_str().unwrap(), "deep", &FileTracker::new()).await;
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "deep");
    }

    #[tokio::test]
    async fn write_file_empty_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");

        let (result, _) = write_file(path.to_str().unwrap(), "", &FileTracker::new()).await;
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[tokio::test]
    async fn write_file_fails_when_parent_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "I am a file").unwrap();

        let path = blocker.join("child.txt");
        let (result, _) = write_file(path.to_str().unwrap(), "content", &FileTracker::new()).await;
        assert!(result.unwrap_err().contains("Failed to create directory"));
    }

    #[tokio::test]
    async fn write_file_unread_directory_hits_strict_gate() {
        // Existing directory: the gate fires before the OS would
        // reject the write because no Read entry exists.
        let dir = tempfile::tempdir().unwrap();
        let (result, _) =
            write_file(dir.path().to_str().unwrap(), "content", &FileTracker::new()).await;
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
        let tracker = FileTracker::new();
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
}
