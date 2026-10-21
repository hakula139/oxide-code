//! File writing tool with directory creation.

use std::path::Path;
use std::pin::Pin;

use serde::Deserialize;

use super::file::WriteMode;
use super::view::{ApprovalBody, ApprovalPreview};
use super::{Prepared, Tool, ToolCtx, ToolOutput, ToolSpec, extract_input_field};
use crate::file_tracker::{FileTracker, GatePurpose};
use crate::permission::PermissionClass;

pub(crate) struct WriteTool;

impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".to_owned(),
            description: "Write content to a file, \
                          creating it if it does not exist or overwriting it if it does. \
                          Before overwriting an existing file, Read all of it in this session, \
                          in one Read or by paging with offset and limit. \
                          The Read-before-Edit gate shared with the Edit tool also refuses files \
                          that changed externally since the last Read. \
                          Creating a new file skips the gate."
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

    fn approval_preview<'a>(
        &'a self,
        name: &'a str,
        input: &'a serde_json::Value,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = ApprovalPreview> + Send + 'a>> {
        Box::pin(async move {
            let content = extract_input_field(input, "content").unwrap_or_default();
            let current = match extract_input_field(input, "file_path") {
                Some(path) => current_text(&cwd.join(path), path).await,
                None => String::new(),
            };
            ApprovalPreview {
                title: self.summarize_call(name, input, cwd),
                body: ApprovalBody::Diff(super::edit::synthesize_chunk(&current, content)),
            }
        })
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
async fn current_text(path: &Path, shown: &str) -> String {
    read_target(path, shown)
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
        Err(msg) => return ToolOutput::error(msg),
    };

    let path = &input.file_path;
    let name = super::file_name(path);
    match write_file(path, &input.content, ctx).await {
        Ok(WriteMode::CreateNew) => ToolOutput::success(format!("Successfully created {path}."))
            .with_title(format!("Created {name}")),
        Ok(WriteMode::Overwrite) => ToolOutput::success(format!("Successfully updated {path}."))
            .with_title(format!("Updated {name}")),
        Err(msg) => ToolOutput::error(msg),
    }
}

/// Writes `content` to `path`, resolved against the context's cwd, and returns the [`WriteMode`]
/// the write took.
async fn write_file(path: &str, content: &str, ctx: &ToolCtx) -> Result<WriteMode, String> {
    let file_path = ctx.cwd().join(path);
    let pre_meta = match tokio::fs::symlink_metadata(&file_path).await {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Failed to access {path}: {e}")),
    };

    if let Some(meta) = &pre_meta {
        validate_write_target(path, meta)?;
        check_gate(&file_path, path, &ctx.tracker).await?;
    }

    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directory {}: {e}", parent.display()))?;
    }

    let mode = if pre_meta.is_some() {
        WriteMode::Overwrite
    } else {
        WriteMode::CreateNew
    };
    super::file::write_tracked(ctx, &file_path, content.as_bytes().to_vec(), mode)
        .await
        .map_err(|e| format!("Failed to write {path}: {e}"))?;
    Ok(mode)
}

/// Validates an existing target's `metadata`. The gate pinned the path to its resolved location,
/// so a symlink there appeared afterward and is refused.
fn validate_write_target(path: &str, metadata: &std::fs::Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "{path} is a symlink. Refusing to write through it."
        ));
    }
    super::file::validate_file_target(path, metadata, "write")
}

async fn check_gate(path: &Path, shown: &str, tracker: &FileTracker) -> Result<(), String> {
    let bytes = read_target(path, shown).await?;
    tracker
        .verify_current_content(
            &super::file::tracker_key(path).await,
            &bytes,
            GatePurpose::Write,
        )
        .map_err(|e| e.to_string())
}

/// Reads the existing target `path`, spelled `shown`. The no-follow open refuses a symlink, and
/// [`validate_write_target`] refuses a non-regular or oversized file.
async fn read_target(path: &Path, shown: &str) -> Result<Vec<u8>, String> {
    let (_, bytes) = super::file::read_validated(path, shown, |metadata| {
        validate_write_target(shown, metadata)
    })
    .await?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, Permissions};

    use super::*;
    use crate::file_tracker::MAX_TRACKED_FILE_SIZE;
    use crate::file_tracker::testing::seed_full_read;
    use crate::tool::local_testing::{self, canonical_tempdir, ctx, temp_ctx};
    use crate::tool::read::ReadTool;
    use crate::tool::view::{DiffChunk, DiffLine};
    use crate::util::fs::testing::symlink_file;

    mod approval_preview {
        use super::*;

        fn added_line(text: &str) -> ApprovalBody {
            ApprovalBody::Diff(DiffChunk {
                old: Vec::new(),
                new: vec![DiffLine {
                    number: 1,
                    text: text.to_owned(),
                }],
            })
        }

        #[tokio::test]
        async fn renders_a_new_file_as_an_all_added_diff() {
            let input = serde_json::json!({
                "file_path": "/repo/src/new.rs",
                "content": "fn main() {}",
            });

            let preview = WriteTool
                .approval_preview("write", &input, Path::new("/repo"))
                .await;

            assert_eq!(preview.title, "Write(src/new.rs)");
            assert_eq!(preview.body, added_line("fn main() {}"));
        }

        #[tokio::test]
        async fn without_a_file_path_renders_the_content_as_added() {
            let input = serde_json::json!({"content": "fn main() {}"});

            let preview = WriteTool
                .approval_preview("write", &input, Path::new("/repo"))
                .await;

            assert_eq!(preview.title, "Write");
            assert_eq!(preview.body, added_line("fn main() {}"));
        }

        #[tokio::test]
        async fn diffs_an_overwrite_against_the_current_content() {
            let dir = canonical_tempdir();
            let path = dir.path().join("notes.txt");
            fs::write(&path, "keep\nold line\n").unwrap();
            let input = serde_json::json!({
                "file_path": path,
                "content": "keep\nnew line\n",
            });

            let preview = WriteTool
                .approval_preview("write", &input, dir.path())
                .await;

            assert_eq!(
                preview.body,
                ApprovalBody::Diff(DiffChunk {
                    old: vec![DiffLine {
                        number: 2,
                        text: "old line".to_owned(),
                    }],
                    new: vec![DiffLine {
                        number: 2,
                        text: "new line".to_owned(),
                    }],
                }),
            );
        }

        #[tokio::test]
        async fn does_not_read_through_a_symlink() {
            let dir = canonical_tempdir();
            let secret = dir.path().join("secret.txt");
            fs::write(&secret, "token").unwrap();
            let link = dir.path().join("link.txt");
            symlink_file(&secret, &link);
            let input = serde_json::json!({
                "file_path": link,
                "content": "new",
            });

            let preview = WriteTool
                .approval_preview("write", &input, dir.path())
                .await;

            assert_eq!(preview.body, added_line("new"));
        }
    }

    mod run {
        use super::*;

        #[tokio::test]
        async fn new_file_reports_creation() {
            let dir = canonical_tempdir();
            let path = dir.path().join("new.txt");

            let input = serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "content": "hello world"
            });

            let output = local_testing::run(&WriteTool, input, &ctx(dir.path())).await;

            assert!(!output.is_error);
            assert_eq!(
                output.content,
                format!("Successfully created {}.", path.display())
            );
            assert_eq!(output.metadata.title.as_deref(), Some("Created new.txt"));
            assert_eq!(fs::read_to_string(&path).unwrap(), "hello world");
        }

        #[tokio::test]
        async fn missing_required_fields_reports_input_error() {
            let output = run(serde_json::json!({"file_path": "/tmp/x"}), &temp_ctx()).await;
            assert!(output.is_error);
            assert!(output.content.contains("Invalid input"));
        }

        #[tokio::test]
        async fn overwrites_existing_file_uses_updated_verb() {
            let dir = canonical_tempdir();
            let path = dir.path().join("existing.txt");
            fs::write(&path, "old").unwrap();
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
    }

    mod write_file {
        use super::*;

        #[tokio::test]
        async fn new_path_creates_content() {
            let dir = canonical_tempdir();
            let path = dir.path().join("new.txt");

            let mode = write_file(path.to_str().unwrap(), "content", &temp_ctx())
                .await
                .unwrap();

            assert_eq!(mode, WriteMode::CreateNew);
            assert_eq!(fs::read_to_string(&path).unwrap(), "content");
        }

        #[tokio::test]
        async fn resolves_a_relative_path_against_the_ctx_cwd() {
            let dir = canonical_tempdir();
            let ctx = ctx(dir.path());

            let mode = write_file("notes/new.txt", "content", &ctx).await.unwrap();

            assert_eq!(mode, WriteMode::CreateNew);
            let path = dir.path().join("notes/new.txt");
            assert_eq!(fs::read_to_string(&path).unwrap(), "content");
            assert_eq!(
                ctx.tracker
                    .verify_current_content(&path, b"content", GatePurpose::Write),
                Ok(()),
            );
        }

        #[tokio::test]
        async fn existing_without_read_is_rejected() {
            let dir = canonical_tempdir();
            let path = dir.path().join("existing.txt");
            fs::write(&path, "old content").unwrap();

            let err = write_file(path.to_str().unwrap(), "new content", &temp_ctx())
                .await
                .unwrap_err();
            assert!(
                err.contains("has not been read"),
                "expected must-read-first error, got: {err}",
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "old content");
        }

        #[tokio::test]
        async fn after_read_overwrites_existing() {
            let dir = canonical_tempdir();
            let path = dir.path().join("existing.txt");
            fs::write(&path, "old content").unwrap();

            let ctx = temp_ctx();
            seed_full_read(&ctx.tracker, &path);

            let mode = write_file(path.to_str().unwrap(), "new content", &ctx)
                .await
                .unwrap();

            assert_eq!(mode, WriteMode::Overwrite);
            assert_eq!(fs::read_to_string(&path).unwrap(), "new content");
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
            let err = write_file(path.to_str().unwrap(), "new", &ctx)
                .await
                .unwrap_err();
            assert!(err.contains("only been read in part"), "{err}");

            let rest = local_testing::run(
                &read,
                serde_json::json!({"file_path": path, "offset": 3}),
                &ctx,
            )
            .await;
            assert!(!rest.is_error, "{}", rest.content);
            write_file(path.to_str().unwrap(), "new", &ctx)
                .await
                .unwrap();
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

            let err = write_file(path.to_str().unwrap(), "our edit", &ctx)
                .await
                .unwrap_err();
            assert!(
                err.contains("modified externally"),
                "drift error expected, got: {err}",
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "external edit");
        }

        #[tokio::test]
        async fn creates_parent_directories() {
            let dir = canonical_tempdir();
            let path = dir.path().join("a").join("b").join("c.txt");

            write_file(path.to_str().unwrap(), "deep", &temp_ctx())
                .await
                .unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "deep");
        }

        #[tokio::test]
        async fn empty_content_creates_empty_file() {
            let dir = canonical_tempdir();
            let path = dir.path().join("empty.txt");

            write_file(path.to_str().unwrap(), "", &temp_ctx())
                .await
                .unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "");
        }

        #[tokio::test]
        async fn fails_when_parent_is_a_file() {
            // Unix fails the stat with ENOTDIR, and Windows, which reports the path as missing,
            // fails creating the parent. Either error names the blocking path.
            let dir = canonical_tempdir();
            let blocker = dir.path().join("blocker");
            fs::write(&blocker, "I am a file").unwrap();

            let path = blocker.join("child.txt");
            let err = write_file(path.to_str().unwrap(), "content", &temp_ctx())
                .await
                .unwrap_err();
            assert!(
                err.contains(blocker.to_str().unwrap()),
                "expected the error to name the blocking file, got: {err}",
            );
            assert_eq!(fs::read_to_string(&blocker).unwrap(), "I am a file");
        }

        #[tokio::test]
        async fn rejects_directory_before_gate() {
            let dir = canonical_tempdir();
            let path = dir.path().to_str().unwrap();

            let err = write_file(path, "content", &temp_ctx()).await.unwrap_err();

            assert_eq!(err, format!("{path} is a directory. Refusing to write."));
        }

        #[tokio::test]
        async fn rejects_oversized_file_before_gate() {
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
        async fn refuses_a_dangling_symlink_at_the_final_component() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join("outside")).unwrap();
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
        async fn records_a_new_file_under_its_canonical_key() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join("real")).unwrap();
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

        #[cfg(unix)]
        #[tokio::test]
        async fn rejects_non_regular_file_before_gate() {
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
        async fn fails_on_read_only_file() {
            use std::os::unix::fs::PermissionsExt;

            let dir = canonical_tempdir();
            let path = dir.path().join("locked.txt");
            fs::write(&path, "original").unwrap();
            let ctx = temp_ctx();
            seed_full_read(&ctx.tracker, &path);
            fs::set_permissions(&path, Permissions::from_mode(0o444)).unwrap();

            let err = write_file(path.to_str().unwrap(), "overwrite", &ctx)
                .await
                .unwrap_err();

            let cause = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap_err();
            assert_eq!(err, format!("Failed to write {}: {cause}", path.display()));
            assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn fails_when_parent_creation_is_denied() {
            // The missing target passes the stat, so the read-only ancestor denies
            // `create_dir_all`. It stays empty, so the temp dir still cleans up.
            use std::os::unix::fs::PermissionsExt;

            let dir = canonical_tempdir();
            let readonly = dir.path().join("readonly");
            fs::create_dir(&readonly).unwrap();
            let path = readonly.join("nested").join("file.txt");
            fs::set_permissions(&readonly, Permissions::from_mode(0o555)).unwrap();

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
}
