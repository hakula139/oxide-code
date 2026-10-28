//! Exact string replacement tool for file editing.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::Deserialize;

use super::file::FileVerb;
use super::view::{
    ApprovalBody, ApprovalPreview, DiffChunk, EditDiff, NumberedLine, ToolResultView,
};
use super::{Prepared, Tool, ToolCtx, ToolDefinition, ToolOutput, ToolSpec, extract_input_field};
use crate::file_tracker::{GatePurpose, LastView};
use crate::permission::PermissionClass;

pub(crate) struct EditTool;

impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            definition: ToolDefinition {
                name: "edit".to_owned(),
                description: "Perform exact string replacement in a file. \
                              The old_string must be unique in the file \
                              unless replace_all is true. \
                              Copy it from the text after the tab in `read` output, \
                              without the line number or the tab. \
                              Read the file with the `read` tool in this session \
                              before editing it. \
                              A `read` of the lines being changed is enough. \
                              An edit also fails for a file that changed externally \
                              since the last `read`."
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

    fn icon(&self) -> &'static str {
        "✎"
    }

    fn approval_preview<'a>(
        &'a self,
        name: &'a str,
        prepared: &'a Prepared,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = ApprovalPreview> + Send + 'a>> {
        let old = extract_input_field(&prepared.input, "old_string").unwrap_or_default();
        let new = extract_input_field(&prepared.input, "new_string").unwrap_or_default();
        Box::pin(std::future::ready(ApprovalPreview {
            title: self.summarize_call(name, &prepared.input, cwd),
            body: ApprovalBody::Diff(synthesize_chunk(old, new)),
        }))
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(run(prepared, ctx))
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
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

// ── Execution ──

async fn run(prepared: Prepared, ctx: &ToolCtx) -> ToolOutput {
    let (input, path): (Input, _) = match prepared.into_path_input() {
        Ok(v) => v,
        Err(msg) => return ToolOutput::error(msg),
    };

    match edit_file(
        &path,
        &input.old_string,
        &input.new_string,
        input.replace_all,
        ctx,
    )
    .await
    {
        Ok(Edited { replacements, diff }) => {
            let shown = path.display();
            let content = match replacements {
                1 => format!("Successfully edited {shown}."),
                n => format!("Replaced {n} occurrences in {shown}."),
            };
            ToolOutput::success(content)
                .with_title(format!("Edited {}", super::file_name(&path)))
                .with_result_view(ToolResultView::Diff(diff))
        }
        Err(msg) => ToolOutput::error(msg),
    }
}

/// Replaces `old_string` in the file at `path`, which `prepare` resolved.
///
/// - `old_string` must occur once without `replace_all`, and each line break in it matches either
///   ending, so the model cannot edit the wrong site.
/// - The Read-before-Edit gate refuses a file whose current content has not been read.
/// - Only the matched spans change, and a replacement takes the ending of the span it replaces.
async fn edit_file(
    path: &Path,
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

    let content_bytes = super::file::read_validated(path, FileVerb::Edit).await?;
    let key = super::file::tracker_key(path).await;
    ctx.tracker
        .verify_current_content(&key, &content_bytes, GatePurpose::Edit)
        .map_err(|e| e.to_string())?;

    let replacement = Replacement {
        path: path.to_path_buf(),
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
            super::file::write_file(path.to_owned(), updated.into_bytes(), Some(expected)),
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
    /// The resolved file path, for error messages.
    path: PathBuf,
    old_string: String,
    new_string: String,
    replace_all: bool,
}

impl Replacement {
    /// The file's updated text, what changed, and the written-line view the tracker records, given
    /// its current `content`.
    fn apply(&self, content: &[u8]) -> Result<(String, Edited, LastView), String> {
        let path = self.path.display();
        let content = std::str::from_utf8(content).map_err(|_| {
            format!(
                "{path} is not valid UTF-8, so edit cannot change it. Use the bash tool to edit it."
            )
        })?;

        let old_string = normalize_eol(&self.old_string);
        let new_string = normalize_eol(&self.new_string);
        let ranges = match_ranges(content, &old_string)?;
        let replacements = ranges.len();
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

        let (diff, written) = build_diff(content, &old_string, &new_string, &ranges);
        let updated = splice(content, &ranges, &new_string);
        let view = LastView::from_ranges(written, updated.lines().count());
        Ok((updated, Edited { replacements, diff }, view))
    }
}

/// The byte ranges in `content` that match `old`, whose line breaks match either `\n` or `\r\n`.
/// A match never overlaps the one before it. Reports the error the model reads when `old` is too
/// large to compile.
fn match_ranges(content: &str, old: &str) -> Result<Vec<(usize, usize)>, String> {
    let pattern = old
        .split('\n')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join("\r?\n");
    let regex = regex::RegexBuilder::new(&pattern)
        .size_limit(MATCH_SIZE_LIMIT)
        .dfa_size_limit(MATCH_SIZE_LIMIT)
        .build()
        .map_err(|_| {
            "old_string is too large to match. Split the edit into smaller replacements.".to_owned()
        })?;
    Ok(regex
        .find_iter(content)
        .map(|found| (found.start(), found.end()))
        .collect())
}

/// The compiled size a match pattern may take. Each literal byte emits about 32 bytes of NFA
/// state, so an `old_string` over roughly 1 MiB that spans lines exceeds this.
const MATCH_SIZE_LIMIT: usize = 32 * 1024 * 1024;

/// `content` with each range in `ranges` replaced by `new_string`, converting its line breaks to
/// the ending found in the matched region, or to the dominant ending when the match has none.
fn splice(content: &str, ranges: &[(usize, usize)], new_string: &str) -> String {
    let dominant = dominant_eol(content);
    let mut converted: [Option<String>; 2] = [None, None];
    let mut updated = String::with_capacity(content.len());
    let mut last = 0;
    for &(start, end) in ranges {
        updated.push_str(&content[last..start]);
        let eol = region_eol(&content[start..end]).unwrap_or(dominant);
        let slot = usize::from(eol == "\r\n");
        let replacement =
            converted[slot].get_or_insert_with(|| apply_eol(new_string.to_owned(), eol));
        updated.push_str(replacement);
        last = end;
    }
    updated.push_str(&content[last..]);
    updated
}

/// The line ending of the first line break in `text`.
fn region_eol(text: &str) -> Option<&'static str> {
    let lf = text.find('\n')?;
    Some(if text[..lf].ends_with('\r') {
        "\r\n"
    } else {
        "\n"
    })
}

// ── Diff Production ──

/// The boundary-trimmed hunk of replacing `old` with `new`, numbered from line 1 on both sides.
pub(super) fn synthesize_chunk(old: &str, new: &str) -> DiffChunk {
    let mut chunk = DiffChunk {
        old: split_into_diff_lines(old, 1),
        new: split_into_diff_lines(new, 1),
    };
    trim_chunk(&mut chunk);
    chunk
}

/// The diff of replacing every range in `ranges` of `original` with `new_string`, and the
/// new-content lines each replacement writes. Old-side numbers count lines of `original`, and
/// new-side numbers count lines of the edited text.
fn build_diff(
    original: &str,
    old_string: &str,
    new_string: &str,
    ranges: &[(usize, usize)],
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
    for &(start, end) in ranges {
        let breaks = original[scanned..start].matches('\n').count();
        old_line += breaks;
        new_line += breaks;

        first.get_or_insert((old_line, new_line));
        locations.extend(anchor_line(&hunk, old_line, new_line));
        if writes_lines {
            written.push((new_line, new_line + written_breaks));
        }

        old_line += old_breaks;
        new_line += new_breaks;
        scanned = end;
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
pub(super) fn anchor_line(hunk: &DiffChunk, old_line: usize, new_line: usize) -> Option<usize> {
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
    use crate::util::fs::testing::make_read_only;
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

    mod approval_preview {
        use super::*;

        #[tokio::test]
        async fn renders_old_and_new_as_a_diff() {
            let tool = EditTool;
            let preview = tool
                .approval_preview(
                    "edit",
                    &crate::tool::local_testing::prepared(serde_json::json!({
                        "file_path": "/repo/src/main.rs",
                        "old_string": "let a = 1;",
                        "new_string": "let a = 2;",
                    })),
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
        async fn updates_every_hard_link_and_records_the_write() {
            let dir = canonical_tempdir();
            let path = dir.path().join("notes.txt");
            let alias = dir.path().join("alias.txt");
            fs::write(&path, "one\ntwo\n").unwrap();
            fs::hard_link(&path, &alias).unwrap();
            let ctx = seeded_ctx(&path);

            let first = local_testing::run(
                &EditTool,
                serde_json::json!({
                    "file_path": path,
                    "old_string": "two",
                    "new_string": "three",
                }),
                &ctx,
            )
            .await;

            assert!(!first.is_error, "{}", first.content);
            assert_eq!(fs::read_to_string(&path).unwrap(), "one\nthree\n");
            assert_eq!(fs::read_to_string(&alias).unwrap(), "one\nthree\n");

            // The second edit passes the Read-before-Edit gate without another read, which shows
            // the first recorded its write.
            let second = local_testing::run(
                &EditTool,
                serde_json::json!({
                    "file_path": path,
                    "old_string": "one",
                    "new_string": "zero",
                }),
                &ctx,
            )
            .await;

            assert!(!second.is_error, "{}", second.content);
            assert_eq!(fs::read_to_string(&alias).unwrap(), "zero\nthree\n");
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

            let output = local_testing::run(&EditTool, input, &ctx).await;

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

            let output = local_testing::run(&EditTool, input, &ctx).await;

            assert!(!output.is_error);
            assert_eq!(
                output.content,
                format!("Successfully edited {}.", path.display())
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "goodbye world");
        }

        #[cfg(target_os = "linux")]
        #[tokio::test]
        async fn edits_a_target_under_a_non_utf8_working_directory() {
            use std::os::unix::ffi::OsStringExt as _;

            let dir = canonical_tempdir();
            let cwd = dir
                .path()
                .join(std::ffi::OsString::from_vec(b"caf\xe9".to_vec()));
            fs::create_dir(&cwd).unwrap();
            let path = cwd.join("notes.txt");
            fs::write(&path, "old\n").unwrap();
            let ctx = ctx_with(&cwd, tracker_seeded(&path));

            let output = local_testing::run(
                &EditTool,
                serde_json::json!({
                    "file_path": "notes.txt",
                    "old_string": "old",
                    "new_string": "new",
                }),
                &ctx,
            )
            .await;

            assert!(!output.is_error, "{}", output.content);
            assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
            let entries: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(
                entries,
                [cwd.file_name().unwrap()],
                "the edit must not touch the lossy spelling",
            );
        }

        #[tokio::test]
        async fn missing_required_fields_reports_input_error() {
            let output = local_testing::run(
                &EditTool,
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

            let output = local_testing::run(
                &EditTool,
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

            let result = edit_file(&detour, "hello", "goodbye", false, &ctx).await;

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

            let result = edit_file(&path, "hello", "goodbye", false, &ctx).await;

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

            let err = edit_file(&path, "", "x", false, &temp_ctx())
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

            let err = edit_file(&path, "hello", "hello", false, &temp_ctx())
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
                &path,
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

            let err = edit_file(&path, "hello", "goodbye", false, &temp_ctx())
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

            let err = edit_file(&path, "external", "ours", false, &ctx)
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

            let err = edit_file(Path::new(path), "a", "b", false, &temp_ctx())
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

            let err = edit_file(&path, "a", "b", false, &ctx).await.unwrap_err();

            assert_eq!(
                err,
                format!(
                    "{} is not valid UTF-8, so edit cannot change it. \
                     Use the bash tool to edit it.",
                    path.display()
                ),
            );
        }

        #[tokio::test]
        async fn rejects_ambiguous_match() {
            let dir = canonical_tempdir();
            let path = dir.path().join("test.txt");
            fs::write(&path, "aaa bbb aaa").unwrap();

            let err = edit_file(&path, "aaa", "ccc", false, &seeded_ctx(&path))
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

            let err = edit_file(Path::new(path), "a", "b", false, &temp_ctx())
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

            let err = edit_file(&path, "a", "b", false, &temp_ctx())
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
            make_read_only(&path);

            let err = edit_file(&path, "hello", "goodbye", false, &ctx)
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

            let result = edit_file(&path, "hello", "goodbye", false, &ctx).await;

            result.unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "goodbye world");
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn rejects_non_regular_file() {
            let dir = canonical_tempdir();
            let path = dir.path().join("socket");
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

            let err = edit_file(&path, "a", "b", false, &temp_ctx())
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
            fn preserves_every_ending_outside_the_replaced_span() {
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
                        "aaa\nbbb\r\nreplaced\r\n",
                    ),
                    (
                        "mixed multiline",
                        "foo\nbar\r\nbaz\r\n",
                        "foo\nbar",
                        "a\nb",
                        "a\nb\r\nbaz\r\n",
                    ),
                    (
                        "stray crlf",
                        "aaa\nbbb\r\nccc\n",
                        "bbb",
                        "BBB",
                        "aaa\nBBB\r\nccc\n",
                    ),
                ] {
                    let replacement = Replacement {
                        path: PathBuf::from("test.txt"),
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
                    path: PathBuf::from("grow.txt"),
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

    mod match_ranges {
        use super::*;

        #[test]
        fn matches_each_occurrence_with_either_line_ending() {
            for (case, content, old, expected) in [
                ("one line", "a\nb\na\n", "a", vec![(0, 1), (4, 5)]),
                ("several lines", "a\nb\nc\n", "a\nb", vec![(0, 3)]),
                ("leading break", "a\nb\nc\n", "\nb", vec![(1, 3)]),
                ("lone break", "a\nb\n", "\n", vec![(1, 2), (3, 4)]),
                ("crlf match", "a\r\nb\r\n", "a\nb", vec![(0, 4)]),
                (
                    "leading break before crlf",
                    "a\r\nb\r\nc",
                    "\nb",
                    vec![(1, 4)],
                ),
                (
                    "resumes past a mismatch",
                    "a\nc\na\nb",
                    "a\nb",
                    vec![(4, 7)],
                ),
            ] {
                assert_eq!(match_ranges(content, old).unwrap(), expected, "{case}");
            }
        }

        #[test]
        fn reports_an_old_string_too_large_to_match() {
            let old = format!("{}\n{}", "a".repeat(700_000), "b".repeat(700_000));

            assert_eq!(
                match_ranges("", &old),
                Err(
                    "old_string is too large to match. Split the edit into smaller replacements."
                        .to_owned()
                ),
            );
        }
    }

    mod build_diff {
        use super::*;

        fn diff_of(original: &str, old: &str, new: &str) -> EditDiff {
            build_diff(original, old, new, &match_ranges(original, old).unwrap()).0
        }

        #[test]
        fn numbers_the_hunk_by_its_lines_in_the_file() {
            let diff = diff_of("A\nB\nC\n", "B", "X");

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
            let diff = diff_of("X\nY\nA\nX\nY\nB\nX\nY\nC\n", "X\nY", "Z");

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
            let diff = diff_of("fn foo()\nx\nfn foo()\n", "fn foo()", "fn foo()\n    body");

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
            let diff = diff_of("a b\n", "a", "a\n");

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
                let (_, written) =
                    build_diff(content, old, new, &match_ranges(content, old).unwrap());

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
