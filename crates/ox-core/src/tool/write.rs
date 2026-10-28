//! File writing tool with directory creation.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::Context as _;
use serde::Deserialize;

use super::file::FileVerb;
use super::view::{ApprovalBody, ApprovalPreview};
use super::{Prepared, Tool, ToolCtx, ToolDefinition, ToolOutput, ToolSpec};
use crate::file_tracker::{FileTracker, GatePurpose, LastView};
use crate::permission::PermissionClass;

pub(crate) struct WriteTool;

impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            definition: ToolDefinition {
                name: "write".to_owned(),
                description: "Write content to a file, \
                              creating it if it does not exist or overwriting it if it does. \
                              Before overwriting an existing file, read all of it with the \
                              `read` tool in this session, in one call or by paging with offset \
                              and limit. \
                              As with the `edit` tool, a write also fails for a file \
                              that changed externally since the last `read`. \
                              Creating a new file needs no prior `read`."
                    .to_owned(),
                input_schema: input_schema(),
            },
            class: PermissionClass::Edit,
            parallel_safe: false,
        }
    }

    fn path_field(&self) -> Option<&'static str> {
        Some("file_path")
    }

    fn prepare(&self, input: serde_json::Value, ctx: &ToolCtx) -> Result<Prepared, String> {
        Prepared::parse(input, |input: &Input| Ok(ctx.path_access(&input.file_path)))
    }

    fn icon(&self) -> &'static str {
        "←"
    }

    fn approval_preview<'a>(
        &'a self,
        name: &'a str,
        prepared: &'a Prepared,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = ApprovalPreview> + Send + 'a>> {
        Box::pin(async move {
            let input: Input = prepared.parsed();
            let current = current_text(&prepared.path()).await;
            ApprovalPreview {
                title: prepared.path_title(name, cwd),
                body: ApprovalBody::Diff(super::edit::synthesize_chunk(&current, &input.content)),
            }
        })
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(run(prepared, ctx))
    }
}

/// The text an overwrite of `path` would replace. A missing file, or one the write refuses (a
/// symlink, a non-regular or oversized file), previews as empty.
async fn current_text(path: &Path) -> String {
    super::file::read_validated(path, FileVerb::Write)
        .await
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
        "required": ["file_path", "content"],
        "additionalProperties": false
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    file_path: String,
    content: String,
}

// ── Execution ──

async fn run(prepared: Prepared, ctx: &ToolCtx) -> ToolOutput {
    let (input, path): (Input, _) = prepared.into_path_input();
    let shown = path.display();
    let name = super::file_name(&path);

    match write_file(&path, &input.content, ctx).await {
        Ok(WriteMode::CreateNew) => ToolOutput::success(format!("Successfully created {shown}."))
            .with_title(format!("Created {name}")),
        Ok(WriteMode::Overwrite) => ToolOutput::success(format!("Successfully updated {shown}."))
            .with_title(format!("Updated {name}")),
        Err(msg) => ToolOutput::error(msg),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteMode {
    CreateNew,
    Overwrite,
}

/// Writes `content` to the resolved `path` and returns the [`WriteMode`] it took.
async fn write_file(path: &Path, content: &str, ctx: &ToolCtx) -> Result<WriteMode, String> {
    write_file_with_parent(path, content, ctx, |parent| async move {
        tokio::fs::create_dir_all(parent).await
    })
    .await
}

async fn write_file_with_parent<F>(
    path: &Path,
    content: &str,
    ctx: &ToolCtx,
    create_parent: impl FnOnce(PathBuf) -> F + Send + 'static,
) -> Result<WriteMode, String>
where
    F: Future<Output = std::io::Result<()>> + Send,
{
    let pre_meta = match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Failed to access {}: {e}", path.display())),
    };

    let expected = if let Some(meta) = &pre_meta {
        validate_write_target(path, meta)?;
        Some(check_gate(path, &ctx.tracker).await?)
    } else {
        None
    };

    let mode = if pre_meta.is_some() {
        WriteMode::Overwrite
    } else {
        WriteMode::CreateNew
    };
    let file_path = path.to_owned();
    let bytes = content.as_bytes().to_vec();
    ctx.tracker
        .write_and_record(&ctx.tool_use_id, LastView::Full, async move {
            if let Some(parent) = file_path.parent() {
                create_parent(parent.to_owned())
                    .await
                    .with_context(|| format!("Failed to create directory {}", parent.display()))?;
            }
            super::file::write_file(file_path, bytes, expected).await
        })
        .await
        .map_err(|error| format!("{error:#}"))?;
    Ok(mode)
}

/// Refuses an existing target whose `metadata` shows a symlink. The gate resolved the path, so a
/// symlink there appeared afterward.
fn validate_write_target(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "{} is a symlink. Refusing to write through it.",
            path.display()
        ));
    }
    Ok(())
}

async fn check_gate(path: &Path, tracker: &FileTracker) -> Result<Vec<u8>, String> {
    let bytes = super::file::read_validated(path, FileVerb::Write).await?;
    tracker
        .verify_current_content(
            &super::file::tracker_key(path).await,
            &bytes,
            GatePurpose::Write,
        )
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;
    use std::sync::Arc;

    use tokio::sync::{Notify, mpsc};
    use tokio_util::sync::CancellationToken;

    use super::ToolCtx;

    pub(crate) async fn write_with_blocked_parent(
        path: &Path,
        content: &str,
        ctx: &ToolCtx,
        started: mpsc::UnboundedSender<CancellationToken>,
        release: Arc<Notify>,
    ) -> Result<(), String> {
        let cancel = ctx.cancel.clone();
        super::write_file_with_parent(path, content, ctx, move |parent| async move {
            started.send(cancel).unwrap();
            release.notified().await;
            tokio::fs::create_dir_all(parent).await
        })
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::sync::Arc;

    use tokio::sync::{Notify, mpsc};
    use tracing::instrument::WithSubscriber as _;

    use super::*;
    use crate::file_tracker::MAX_TRACKED_FILE_SIZE;
    use crate::file_tracker::testing::{assert_writes_draining, seed_full_read};
    use crate::tool::local_testing::{self, ctx, temp_ctx};
    use crate::tool::read::ReadTool;
    use crate::tool::view::{DiffChunk, NumberedLine};
    use crate::util::fs::testing::{deny_subdir_creation, make_read_only, symlink_file};
    use crate::util::path::local_testing::canonical_tempdir;

    mod approval_preview {
        use super::*;

        fn added_line(text: &str) -> ApprovalBody {
            ApprovalBody::Diff(DiffChunk {
                old: Vec::new(),
                new: vec![NumberedLine {
                    number: 1,
                    text: text.to_owned(),
                }],
            })
        }

        fn prepare(input: serde_json::Value, cwd: &Path) -> Prepared {
            WriteTool.prepare(input, &ctx(cwd)).unwrap()
        }

        #[tokio::test]
        async fn renders_a_new_file_as_an_all_added_diff() {
            let dir = canonical_tempdir();
            let cwd = dir.path().to_path_buf();
            let prepared = prepare(
                serde_json::json!({
                    "file_path": "src/new.rs",
                    "content": "fn main() {}",
                }),
                &cwd,
            );

            let preview = WriteTool.approval_preview("write", &prepared, &cwd).await;

            assert_eq!(preview.title, "Write(src/new.rs)");
            assert_eq!(preview.body, added_line("fn main() {}"));
        }

        #[tokio::test]
        async fn diffs_an_overwrite_against_the_current_content() {
            let dir = canonical_tempdir();
            let path = dir.path().join("notes.txt");
            fs::write(&path, "keep\nold line\n").unwrap();
            let prepared = prepare(
                serde_json::json!({
                    "file_path": path,
                    "content": "keep\nnew line\n",
                }),
                dir.path(),
            );

            let preview = WriteTool
                .approval_preview("write", &prepared, dir.path())
                .await;

            assert_eq!(
                preview.body,
                ApprovalBody::Diff(DiffChunk {
                    old: vec![NumberedLine {
                        number: 2,
                        text: "old line".to_owned(),
                    }],
                    new: vec![NumberedLine {
                        number: 2,
                        text: "new line".to_owned(),
                    }],
                }),
            );
        }

        #[tokio::test]
        async fn previews_the_resolved_target_of_a_symlink() {
            let dir = canonical_tempdir();
            let target = dir.path().join("target.txt");
            fs::write(&target, "token").unwrap();
            let link = dir.path().join("link.txt");
            symlink_file(&target, &link);
            let prepared = prepare(
                serde_json::json!({
                    "file_path": link,
                    "content": "new",
                }),
                dir.path(),
            );

            let preview = WriteTool
                .approval_preview("write", &prepared, dir.path())
                .await;

            assert_eq!(preview.title, "Write(target.txt)");
            assert_eq!(
                preview.body,
                ApprovalBody::Diff(DiffChunk {
                    old: vec![NumberedLine {
                        number: 1,
                        text: "token".to_owned(),
                    }],
                    new: vec![NumberedLine {
                        number: 1,
                        text: "new".to_owned(),
                    }],
                }),
            );
        }
    }

    mod run {
        use super::*;

        #[tokio::test]
        async fn new_file_reports_creation() {
            let dir = canonical_tempdir();
            let path = dir.path().join("new.txt");
            let ctx = ctx(dir.path());

            let input = serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "content": "hello world"
            });

            let output = local_testing::run(&WriteTool, input, &ctx).await;

            assert!(!output.is_error);
            assert_eq!(
                output.content,
                format!("Successfully created {}.", path.display())
            );
            assert_eq!(output.metadata.title.as_deref(), Some("Created new.txt"));
            assert_eq!(fs::read_to_string(&path).unwrap(), "hello world");
        }

        #[tokio::test]
        async fn overwrites_existing_file_uses_updated_verb() {
            let dir = canonical_tempdir();
            let path = dir.path().join("existing.txt");
            fs::write(&path, "old").unwrap();
            let ctx = temp_ctx();
            seed_full_read(&ctx.tracker, &path);

            let output = local_testing::run(
                &WriteTool,
                serde_json::json!({
                    "file_path": path.to_str().unwrap(),
                    "content": "new",
                }),
                &ctx,
            )
            .await;

            assert!(!output.is_error);
            assert_eq!(
                output.content,
                format!("Successfully updated {}.", path.display())
            );
            assert_eq!(
                output.metadata.title.as_deref(),
                Some("Updated existing.txt"),
                "overwrite should surface as Updated",
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        }

        #[tokio::test]
        async fn gate_rejection_omits_title() {
            let dir = canonical_tempdir();
            let path = dir.path().join("existing.txt");
            fs::write(&path, "old").unwrap();

            let output = local_testing::run(
                &WriteTool,
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
    }

    mod write_file {
        use super::*;

        #[tokio::test]
        async fn new_path_creates_content() {
            let dir = canonical_tempdir();
            let path = dir.path().join("new.txt");
            let ctx = temp_ctx();

            let result = write_file(&path, "content", &ctx).await;

            assert_eq!(result.unwrap(), WriteMode::CreateNew);
            assert_eq!(fs::read_to_string(&path).unwrap(), "content");
        }

        #[tokio::test]
        async fn resolves_a_relative_path_against_the_ctx_cwd() {
            let dir = canonical_tempdir();
            let ctx = ctx(dir.path());
            let path = dir.path().join("notes/new.txt");

            let output = local_testing::run(
                &WriteTool,
                serde_json::json!({"file_path": "notes/new.txt", "content": "content"}),
                &ctx,
            )
            .await;

            assert!(path.parent().unwrap().is_dir());
            assert!(!output.is_error, "{}", output.content);
            assert_eq!(fs::read_to_string(&path).unwrap(), "content");
            assert_eq!(
                ctx.tracker
                    .verify_current_content(&path, b"content", GatePurpose::Write),
                Ok(()),
            );
        }

        #[tokio::test]
        async fn after_read_overwrites_existing() {
            let dir = canonical_tempdir();
            let path = dir.path().join("existing.txt");
            fs::write(&path, "old content").unwrap();

            let ctx = temp_ctx();
            seed_full_read(&ctx.tracker, &path);

            let result = write_file(&path, "new content", &ctx).await;

            assert_eq!(result.unwrap(), WriteMode::Overwrite);
            assert_eq!(fs::read_to_string(&path).unwrap(), "new content");
        }

        #[tokio::test]
        async fn a_change_after_the_gate_keeps_the_tracker_unchanged() {
            let dir = canonical_tempdir();
            let path = dir.path().join("existing.txt");
            fs::write(&path, "original").unwrap();
            let ctx = temp_ctx();
            seed_full_read(&ctx.tracker, &path);
            let before = ctx.tracker.snapshot_all();
            let changed = path.clone();

            let error = write_file_with_parent(&path, "ours", &ctx, |_| async move {
                fs::write(changed, "theirs")
            })
            .await
            .unwrap_err();

            assert!(error.contains("the file changed"), "{error}");
            assert_eq!(fs::read_to_string(&path).unwrap(), "theirs");
            assert_eq!(ctx.tracker.snapshot_all(), before);
        }

        #[tokio::test]
        async fn creates_parent_directories() {
            let dir = canonical_tempdir();
            let path = dir.path().join("a").join("b").join("c.txt");
            let ctx = temp_ctx();

            let result = write_file(&path, "deep", &ctx).await;

            assert!(path.parent().unwrap().is_dir());
            assert_eq!(result.unwrap(), WriteMode::CreateNew);
            assert_eq!(fs::read_to_string(&path).unwrap(), "deep");
        }

        #[tokio::test]
        async fn empty_content_creates_empty_file() {
            let dir = canonical_tempdir();
            let path = dir.path().join("empty.txt");
            let ctx = temp_ctx();

            let result = write_file(&path, "", &ctx).await;

            assert_eq!(result.unwrap(), WriteMode::CreateNew);
            assert_eq!(fs::read_to_string(&path).unwrap(), "");
        }

        #[cfg(target_os = "linux")]
        #[tokio::test]
        async fn writes_a_target_under_a_non_utf8_working_directory() {
            use std::os::unix::ffi::OsStringExt as _;

            let dir = canonical_tempdir();
            let cwd = dir
                .path()
                .join(std::ffi::OsString::from_vec(b"caf\xe9".to_vec()));
            fs::create_dir(&cwd).unwrap();
            let path = cwd.join("notes.txt");
            fs::write(&path, "old\n").unwrap();
            let ctx = ctx(&cwd);
            seed_full_read(&ctx.tracker, &path);
            let input = serde_json::json!({"file_path": "notes.txt", "content": "new\n"});

            let prepared = WriteTool.prepare(input.clone(), &ctx).unwrap();
            let preview = WriteTool.approval_preview("write", &prepared, &cwd).await;

            assert_eq!(
                preview.body,
                ApprovalBody::Diff(DiffChunk {
                    old: vec![NumberedLine {
                        number: 1,
                        text: "old".to_owned(),
                    }],
                    new: vec![NumberedLine {
                        number: 1,
                        text: "new".to_owned(),
                    }],
                }),
            );

            let output = local_testing::run(&WriteTool, input, &ctx).await;

            assert!(!output.is_error, "{}", output.content);
            assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
            let entries: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(
                entries,
                [cwd.file_name().unwrap()],
                "the write must not create a sibling beside the lossy spelling",
            );
        }

        #[tokio::test]
        async fn existing_without_read_is_rejected() {
            let dir = canonical_tempdir();
            let path = dir.path().join("existing.txt");
            fs::write(&path, "old content").unwrap();

            let err = write_file(&path, "new content", &temp_ctx())
                .await
                .unwrap_err();

            assert_eq!(
                err,
                format!(
                    "File {} has not been read in this session. Read it before writing to it.",
                    path.display(),
                ),
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "old content");
        }

        #[tokio::test]
        async fn needs_every_line_read_across_paged_reads() {
            let dir = canonical_tempdir();
            let path = dir.path().join("existing.txt");
            fs::write(&path, "one\ntwo\nthree\n").unwrap();
            let ctx = temp_ctx();
            let read = ReadTool;

            let first = local_testing::run(
                &read,
                serde_json::json!({"file_path": path, "limit": 2}),
                &ctx,
            )
            .await;
            assert!(!first.is_error, "{}", first.content);
            let err = write_file(&path, "new", &ctx).await.unwrap_err();
            assert_eq!(
                err,
                format!(
                    "File {} has only been read in part. \
                     Read the remaining lines, paging with `offset` and `limit` if needed, \
                     before writing to it.",
                    path.display(),
                ),
            );

            let rest = local_testing::run(
                &read,
                serde_json::json!({"file_path": path, "offset": 3}),
                &ctx,
            )
            .await;
            assert!(!rest.is_error, "{}", rest.content);

            let result = write_file(&path, "new", &ctx).await;

            assert_eq!(result.unwrap(), WriteMode::Overwrite);
            assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        }

        #[tokio::test]
        async fn after_external_modification_is_rejected() {
            let dir = canonical_tempdir();
            let path = dir.path().join("existing.txt");
            fs::write(&path, "old content").unwrap();

            let ctx = temp_ctx();
            seed_full_read(&ctx.tracker, &path);
            fs::write(&path, "external edit").unwrap();

            let err = write_file(&path, "our edit", &ctx).await.unwrap_err();

            assert_eq!(
                err,
                format!(
                    "File {} has been modified externally since it was last read. \
                     Re-read it before writing to it.",
                    path.display(),
                ),
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "external edit");
        }

        #[tokio::test]
        async fn fails_when_parent_is_a_file() {
            // Unix fails the stat with ENOTDIR, and Windows, which reports the path as missing,
            // fails creating the parent.
            let dir = canonical_tempdir();
            let blocker = dir.path().join("blocker");
            fs::write(&blocker, "I am a file").unwrap();
            let path = blocker.join("child.txt");

            let err = write_file(&path, "content", &temp_ctx()).await.unwrap_err();

            #[cfg(unix)]
            let expected = format!(
                "Failed to access {}: {}",
                path.display(),
                std::io::Error::from_raw_os_error(nix::errno::Errno::ENOTDIR as i32),
            );
            #[cfg(windows)]
            let expected = format!(
                "Failed to create directory {}: {}",
                blocker.display(),
                fs::create_dir_all(&blocker).unwrap_err(),
            );
            assert_eq!(err, expected);
            assert_eq!(fs::read_to_string(&blocker).unwrap(), "I am a file");
        }

        #[tokio::test]
        async fn rejects_directory_before_gate() {
            let dir = canonical_tempdir();
            let path = dir.path().to_str().unwrap();

            let err = write_file(Path::new(path), "content", &temp_ctx())
                .await
                .unwrap_err();

            assert_eq!(err, format!("{path} is a directory. Refusing to write."));
        }

        #[tokio::test]
        async fn rejects_oversized_file_before_gate() {
            let dir = canonical_tempdir();
            let path = dir.path().join("huge.txt");
            let f = fs::File::create(&path).unwrap();
            f.set_len(MAX_TRACKED_FILE_SIZE + 1).unwrap();

            let err = write_file(&path, "content", &temp_ctx()).await.unwrap_err();

            assert_eq!(
                err,
                "File is too large (10.0 MiB, max 10 MiB). Use the bash tool to write large files."
            );
        }

        #[tokio::test]
        async fn refuses_a_dangling_symlink_at_the_final_component() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join("outside")).unwrap();
            let target = dir.path().join("outside").join("escape.txt");
            let link = dir.path().join("notes.md");
            symlink_file(&target, &link);

            let err = write_file(&link, "payload", &temp_ctx()).await.unwrap_err();

            assert_eq!(
                err,
                format!(
                    "{} is a symlink. Refusing to write through it.",
                    link.display()
                ),
            );
            assert!(!target.exists(), "the link target must not be created");
        }

        #[tokio::test]
        async fn fails_on_read_only_file() {
            let dir = canonical_tempdir();
            let path = dir.path().join("locked.txt");
            fs::write(&path, "original").unwrap();
            let ctx = temp_ctx();
            seed_full_read(&ctx.tracker, &path);
            make_read_only(&path);

            let err = write_file(&path, "overwrite", &ctx).await.unwrap_err();

            let cause = fs::OpenOptions::new().write(true).open(&path).unwrap_err();
            assert_eq!(err, format!("Failed to write {}: {cause}", path.display()));
            assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        }

        #[tokio::test]
        async fn fails_when_parent_creation_is_denied() {
            // The missing target passes the stat, so the denial on its ancestor fails
            // `create_dir_all`.
            let dir = canonical_tempdir();
            let locked = dir.path().join("locked");
            fs::create_dir(&locked).unwrap();
            let nested = locked.join("nested");
            let path = nested.join("file.txt");
            let _denied = deny_subdir_creation(&locked);

            let err = write_file(&path, "content", &temp_ctx()).await.unwrap_err();

            let cause = fs::create_dir(&nested).unwrap_err();
            assert_eq!(
                err,
                format!("Failed to create directory {}: {cause}", nested.display()),
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn logs_parent_creation_failure_after_the_caller_is_dropped() {
            // Tracing caches callsite interest process-wide, including other test threads.
            const CHILD: &str = "PARENT_CREATION_LOG_TEST_CHILD";
            if std::env::var_os(CHILD).is_none() {
                let thread = std::thread::current();
                let name = thread.name().unwrap();
                let output = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", name])
                    .env(CHILD, "1")
                    .output()
                    .unwrap();

                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(output.status.success(), "{stdout}{stderr}");
                assert!(stdout.contains(&format!("test {name} ... ok")), "{stdout}");
                return;
            }

            let dir = canonical_tempdir();
            let parent = dir.path().join("nested");
            let path = parent.join("file.txt");
            let ctx = ctx(dir.path());
            let log_path = dir.path().join("events.log");
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(fs::File::create(&log_path).unwrap())
                .finish();
            let (started, mut started_rx) = mpsc::unbounded_channel();
            let release = Arc::new(Notify::new());
            let mut caller = Box::pin(
                testing::write_with_blocked_parent(
                    &path,
                    "content",
                    &ctx,
                    started,
                    Arc::clone(&release),
                )
                .with_subscriber(subscriber),
            );
            tokio::select! {
                token = started_rx.recv() => assert!(token.is_some()),
                result = caller.as_mut() => panic!("write ended before parent creation: {result:?}"),
            }

            drop(caller);
            let mut drain = std::pin::pin!(ctx.tracker.drain_writes());
            assert!(futures::poll!(drain.as_mut()).is_pending());
            assert_writes_draining(&ctx.tracker);
            assert!(!parent.exists());
            assert_eq!(ctx.tracker.snapshot_all(), []);
            assert_eq!(fs::read_to_string(&log_path).unwrap(), "");
            fs::write(&parent, "blocker").unwrap();
            release.notify_one();
            drain.await;

            let cause = fs::create_dir_all(&parent).unwrap_err();
            let log = fs::read_to_string(&log_path).unwrap();
            assert!(log.contains("workspace write failed"), "{log}");
            assert!(log.contains("tool_use_id=\"toolu_test\""), "{log}");
            assert!(
                log.contains(&format!(
                    "Failed to create directory {}: {cause}",
                    parent.display()
                )),
                "{log}",
            );
            assert!(!path.exists());
            assert_eq!(fs::read_to_string(&parent).unwrap(), "blocker");
            assert_eq!(ctx.tracker.snapshot_all(), []);
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn records_a_new_file_under_its_canonical_key() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join("real")).unwrap();
            std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
            let ctx = temp_ctx();

            let via_link = dir.path().join("link/new.txt");

            let result = write_file(&via_link, "fresh", &ctx).await;

            assert_eq!(result.unwrap(), WriteMode::CreateNew);
            let real = dir.path().join("real/new.txt");
            assert_eq!(fs::read_to_string(&real).unwrap(), "fresh");
            assert_eq!(
                ctx.tracker
                    .verify_current_content(&real, b"fresh", GatePurpose::Write),
                Ok(()),
            );
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn rejects_non_regular_file_before_gate() {
            let dir = canonical_tempdir();
            let path = dir.path().join("socket");
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

            let err = write_file(&path, "content", &temp_ctx()).await.unwrap_err();

            assert_eq!(
                err,
                format!(
                    "{} is not a regular file (fifo, socket, or device). Refusing to write.",
                    path.display(),
                ),
            );
        }
    }
}
