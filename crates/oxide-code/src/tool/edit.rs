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

/// Per-file size cap for `edit` (10 MB). Generous because legitimate
/// edits sometimes target large config or data files.
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
        // Live path uses `metadata.diff_chunks` for real line numbers.
        // Resume path falls back to an input-derived synthesized chunk
        // (line 1) and recovers the count from `metadata.replacements`
        // or, for the oldest sessions, the success-message prose.
        // Empty `Some(vec![])` is treated as absent so a zero-chunks
        // regression doesn't surface as `(no change)`.
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

// ── Result View ──

/// Parses the replacement count from the success-path output returned
/// by [`edit_file`] when `replace_all` hits multiple matches — a
/// `"Replaced N occurrences in <path>."` string. Returns `None` for
/// the single-match shape (`"Successfully edited ..."`), in which case
/// the caller defaults to 1. Used only as a final fallback for resumed
/// sessions whose JSONL predates structured metadata.
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

/// Builds a single best-effort chunk from raw input strings.
///
/// Used by [`EditTool::result_view`] when the resumed session JSONL
/// carries no structured `diff_chunks`. Line numbers start at 1 —
/// they're the best we have without re-reading the (possibly already
/// mutated) file. Exposed `pub(crate)` so TUI snapshot tests can
/// build the same shape as the production resume path without
/// duplicating the trim policy.
pub(crate) fn synthesize_chunk(old: &str, new: &str) -> DiffChunk {
    let mut chunk = DiffChunk {
        old: split_into_diff_lines(old, 1),
        new: split_into_diff_lines(new, 1),
    };
    trim_chunk(&mut chunk);
    chunk
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

    // Strict gate: stat-match short-circuits, drift falls through to a
    // rehash against the bytes we're about to read anyway.
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
        FileTracker::verify_drift_bytes(&content_bytes, stored_hash, GatePurpose::Edit)
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

    // Diff chunks computed BEFORE the file write so we capture the
    // pre-edit positions while the original content is still in scope.
    // For non-`replace_all` we cap to the first match to mirror the
    // single-replacement semantics of `replacen`.
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

    if let Ok(meta) = tokio::fs::metadata(path).await
        && let Ok(mtime) = meta.modified()
    {
        tracker.record_modify(file_path, updated.as_bytes(), mtime, meta.len());
    }

    let message = if replace_all && match_count > 1 {
        format!("Replaced {match_count} occurrences in {path}.")
    } else {
        format!("Successfully edited {path}.")
    };
    Ok((message, match_count, chunks))
}

// ── Diff Production ──

/// Builds the per-match diff chunks from a successful edit.
/// `original`, `old_string`, and `new_string` are all post EOL
/// normalization. `take` is `usize::MAX` for `replace_all`, 1
/// otherwise. Each chunk carries real file line numbers (post-edit
/// positions on the `+` side) with common anchors trimmed.
///
/// The [`MAX_EDIT_FILE_SIZE`] cap bounds line counts at < 2^24, so
/// the running shift fits in `isize` and the post-edit line stays
/// positive. The `checked_*` calls below surface any overflow as a
/// panic — a wrong-but-plausible line number would be worse.
fn build_diff_chunks(
    original: &str,
    old_string: &str,
    new_string: &str,
    take: usize,
) -> Vec<DiffChunk> {
    let positions = match_positions(original, old_string, take);
    // Newline-count delta — even pure "\n" → "" rewrites (no
    // displayable lines on either side) still need to shift later
    // matches.
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

/// Returns up to `take` non-overlapping byte offsets where `pattern`
/// occurs in `haystack`. Mirrors what `String::replacen(.., take)` /
/// `String::replace` actually rewrite, so post-edit line numbers
/// computed from these offsets stay consistent with the file's new
/// state.
fn match_positions(haystack: &str, pattern: &str, take: usize) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut start = 0;
    while positions.len() < take {
        let Some(rel) = haystack[start..].find(pattern) else {
            break;
        };
        let abs = start + rel;
        positions.push(abs);
        // `pattern.len().max(1)` guards against pathological zero-byte
        // patterns that would loop forever; `edit_file` already rejects
        // empty `old_string` upstream, so this is defense-in-depth.
        start = abs + pattern.len().max(1);
    }
    positions
}

/// 1-based line number of the byte at `offset` in `content`. Counts
/// `\n` separators in the prefix; offsets at end-of-file map to the
/// line after the final newline.
fn line_at_byte(content: &str, offset: usize) -> usize {
    1 + content[..offset].matches('\n').count()
}

/// Splits `s` into numbered diff lines starting at `start_line`. A
/// trailing newline is dropped so `"a\nb\n"` yields two entries — the
/// Edit-tool convention is line-ended `old_string`/`new_string` args,
/// and the renderer doesn't want a phantom blank tail row.
fn split_into_diff_lines(s: &str, start_line: usize) -> Vec<DiffLine> {
    s.lines()
        .enumerate()
        .map(|(i, text)| DiffLine {
            number: start_line + i,
            text: text.to_owned(),
        })
        .collect()
}

/// Drops common leading and trailing lines from `chunk.old` /
/// `chunk.new` in-place. Pure tail insertions like
/// `"fn foo()"` → `"fn foo()\n  body"` collapse the anchor on the old
/// side so only the real delta survives. Line numbers on surviving
/// entries are preserved — slicing keeps each `DiffLine` intact.
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

/// Returns `(prefix, suffix)` — the count of leading and trailing
/// elements that compare equal on both sides. Used by [`trim_chunk`]
/// to strip identical anchors from a diff.
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

/// Detects the dominant line ending style. Bare CR (`\r` without `\n`) is not
/// detected — such files are treated as LF and multi-line matches may fail.
fn dominant_eol(content: &str) -> &'static str {
    let crlf = content.matches("\r\n").count();
    // Each `\r\n` also contains a `\n`, so subtract to get the LF-only count.
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
    use indoc::indoc;

    use super::*;
    use crate::file_tracker::LastView;

    fn tracker() -> Arc<FileTracker> {
        Arc::new(FileTracker::new())
    }

    /// Records a full Read of `path` so the gate has a baseline entry,
    /// mirroring what a real Read turn would have stored.
    fn seeded_tracker(path: &Path) -> FileTracker {
        let tracker = FileTracker::new();
        let bytes = std::fs::read(path).unwrap();
        let meta = std::fs::metadata(path).unwrap();
        tracker.record_read(
            path,
            &bytes,
            meta.modified().unwrap(),
            meta.len(),
            LastView::Full,
        );
        tracker
    }

    // ── result_view ──

    #[test]
    fn result_view_prefers_structured_chunks_from_metadata_on_live_path() {
        // Live path: `run` attaches `metadata.diff_chunks` via
        // `with_diff_chunks`, so the renderer gets real file line
        // numbers. The chunks must win over input-based synthesis,
        // and `replacements` is derived from `chunks.len()` so a stale
        // legacy `metadata.replacements` cannot disagree with the
        // structural source of truth.
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
            // Stale value — must lose to chunks.len().
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
        // Resume path: session JSONL written before structured chunks
        // existed only carries `metadata.replacements` (or nothing).
        // The renderer still needs a chunk to draw, so we synthesize
        // one from the raw inputs starting at line 1.
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
        // Very-old-session fallback: neither `diff_chunks` nor
        // `replacements` were recorded. The success message is the
        // last remaining source of the count, so `parse_replacement_count`
        // still pulls it out — synthesized chunk pairs with parsed N.
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
        // Single-match edits return `"Successfully edited ..."` —
        // `parse_replacement_count` returns None, caller defaults to 1.
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
    fn result_view_returns_none_when_required_inputs_missing() {
        // Malformed call (e.g., model emitted JSON missing `new_string`)
        // degrades to None so the caller falls back to Text rather
        // than panicking.
        let input = serde_json::json!({"file_path": "/tmp/x"});
        assert!(
            EditTool::new(tracker())
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

        let output = run(
            serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "old_string": "hello",
                "new_string": "goodbye"
            }),
            Arc::new(seeded_tracker(&path)),
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
        // Strict gate fires before any rewrite.
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
        // Failing edits (old_string not found, missing file, etc.)
        // must leave `title` unset so the TUI header falls back to
        // the neutral call label rather than rendering
        // `✗ Edited <name>`, which contradicts the error indicator.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let output = run(
            serde_json::json!({
                "file_path": path.to_str().unwrap(),
                "old_string": "not present",
                "new_string": "x",
            }),
            Arc::new(seeded_tracker(&path)),
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
            &seeded_tracker(&path),
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
            &seeded_tracker(&path),
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
        // [`parse_replacement_count`] reads the replacement count out
        // of this exact string to drive the TUI's "applied to N
        // matches" footer. Rewording the prefix or spacing silently
        // breaks that parser — pin the full shape here so the
        // coupling is visible in this test file rather than only
        // manifesting as a missing footer in the rendered diff.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.txt");
        std::fs::write(&path, "a a a").unwrap();
        let (msg, replacements, _chunks) = edit_file(
            path.to_str().unwrap(),
            "a",
            "b",
            true,
            &seeded_tracker(&path),
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
            &seeded_tracker(&path),
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
            &seeded_tracker(&path),
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

        // new_string contains \r\n — should be normalized before apply_eol.
        edit_file(
            path.to_str().unwrap(),
            "aaa",
            "x\r\ny",
            false,
            &seeded_tracker(&path),
        )
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

        edit_file(
            path.to_str().unwrap(),
            "replace_me",
            "replaced",
            false,
            &seeded_tracker(&path),
        )
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

        edit_file(
            path.to_str().unwrap(),
            "foo\nbar",
            "a\nb",
            false,
            &seeded_tracker(&path),
        )
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

        // Empty `old_string` is rejected before the gate fires, so an
        // empty tracker is fine.
        let err = edit_file(path.to_str().unwrap(), "", "x", false, &FileTracker::new())
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
            &FileTracker::new(),
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
            &seeded_tracker(&path),
        )
        .await
        .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn edit_file_without_prior_read_is_rejected() {
        // Strict gate: existing file but no Read entry → must read first.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let err = edit_file(
            path.to_str().unwrap(),
            "hello",
            "goodbye",
            false,
            &FileTracker::new(),
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
        // Read at one mtime, then overwrite the bytes so pre_modify_check
        // returns Drift; the rehash catches the new bytes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();
        let tracker = seeded_tracker(&path);
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
    async fn edit_file_partial_read_is_rejected() {
        // A ranged Read does not satisfy the modification gate even
        // when the bytes happen to match.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let tracker = FileTracker::new();
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
            &FileTracker::new(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("Error reading"));
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
            &seeded_tracker(&path),
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

        // Size cap fires before the gate, so an empty tracker is fine.
        let err = edit_file(path.to_str().unwrap(), "a", "b", false, &FileTracker::new())
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
        let tracker = seeded_tracker(&path);

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
        // Pins the structural payload of `replace_all`: file with
        // 4 lines, "B" at lines 2 and 4, replaced with "X". The two
        // emitted chunks must carry their actual file positions —
        // not an off-by-one shift, not relative-to-snippet line 1.
        // Anchors the live-path producer end-to-end so a regression
        // in `match_positions` or `line_at_byte` surfaces here.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        std::fs::write(&path, "A\nB\nC\nB\n").unwrap();

        let (_, replacements, chunks) = edit_file(
            path.to_str().unwrap(),
            "B",
            "X",
            true,
            &seeded_tracker(&path),
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
        // Replace_all with a multi-line `new_string` shifts the file's
        // line count after each match. The post-edit `+` numbering on
        // the second chunk must reflect that shift — pin the exact
        // numbers so a missing `idx * shift_per_match` term in
        // `build_diff_chunks` regresses visibly.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grow.txt");
        std::fs::write(&path, "A\nB\nC\nB\n").unwrap();

        // "B" → "X\nY" adds one line per replacement.
        let (_, _, chunks) = edit_file(
            path.to_str().unwrap(),
            "B",
            "X\nY",
            true,
            &seeded_tracker(&path),
        )
        .await
        .unwrap();

        assert_eq!(chunks.len(), 2);
        // Match 0: original line 2 unaffected by prior shifts.
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
        // Match 1: original line 4, but match 0 added one line, so
        // the post-edit position is line 5.
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
        // Inverse of `..._growing_replace_all`: a multi-line `old`
        // collapsing to single-line `new` produces a negative shift.
        // The post-edit `+` numbering on later matches must reflect
        // each prior match having shrunk the file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shrink.txt");
        std::fs::write(&path, "X\nY\nA\nX\nY\nB\nX\nY\nC\n").unwrap();

        // "X\nY" → "Z" drops one line per replacement.
        let (_, _, chunks) = edit_file(
            path.to_str().unwrap(),
            "X\nY",
            "Z",
            true,
            &seeded_tracker(&path),
        )
        .await
        .unwrap();

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].old[0].number, 1);
        assert_eq!(chunks[0].new[0].number, 1);
        assert_eq!(chunks[1].old[0].number, 4);
        // Match 1: original line 4, shifted by -1 from match 0 → 3.
        assert_eq!(chunks[1].new[0].number, 3);
        assert_eq!(chunks[2].old[0].number, 7);
        // Match 2: original line 7, shifted by -2 → 5.
        assert_eq!(chunks[2].new[0].number, 5);
    }

    // ── build_diff_chunks ──

    #[test]
    fn build_diff_chunks_single_match_carries_real_position() {
        // Producer-level check separate from the integration test:
        // exercises `build_diff_chunks` directly to pin the line-number
        // contract independent of file-IO plumbing.
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
        // Non-`replace_all` calls pass `take = 1`; the producer must
        // not walk past the first match even when more exist. Mirrors
        // `replacen(.., 1)` semantics.
        let chunks = build_diff_chunks("B\nB\nB\n", "B", "X", 1);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].old[0].number, 1);
    }

    #[test]
    fn build_diff_chunks_applies_per_chunk_trim() {
        // Pure tail insertion at file line 5: anchor on old side
        // collapses, leaving only the inserted line on the new side.
        // Line numbers on the surviving entry are preserved.
        let chunks = build_diff_chunks(
            "x\nx\nx\nx\nfn foo()\n",
            "fn foo()",
            "fn foo()\n    return 42;",
            1,
        );
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].old.is_empty(), "anchor trimmed on old side");
        // New side starts at line 6 — line below the old anchor at 5.
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
        // "aaaa" with pattern "aa" must NOT yield positions 0,1,2 —
        // that's overlapping. Real `replace` rewrites left-to-right
        // non-overlapping, and the offsets must mirror that.
        assert_eq!(match_positions("aaaa", "aa", usize::MAX), vec![0, 2]);
    }

    #[test]
    fn match_positions_no_match_returns_empty() {
        assert!(match_positions("hello", "xyz", usize::MAX).is_empty());
    }

    #[test]
    fn match_positions_take_zero_returns_empty() {
        // `take == 0` short-circuits before the first search — pin
        // this so a regression to `<=` in the loop predicate would
        // surface as a single-result mismatch.
        assert!(match_positions("aXbXcX", "X", 0).is_empty());
    }

    // ── line_at_byte ──

    #[test]
    fn line_at_byte_first_line_is_one() {
        assert_eq!(line_at_byte("A\nB\n", 0), 1);
    }

    #[test]
    fn line_at_byte_after_newline_increments() {
        // Byte 2 sits at "B" — the third line under 1-based numbering
        // should still report line 2 (count of newlines before the
        // byte plus 1). The "2 newlines but third line" interpretation
        // would only kick in past the second newline.
        assert_eq!(line_at_byte("A\nB\nC\n", 2), 2);
        assert_eq!(line_at_byte("A\nB\nC\n", 4), 3);
    }

    #[test]
    fn line_at_byte_end_of_file_after_trailing_newline() {
        // Offset just past the final newline maps to the implicit
        // line after the last `\n` — used when an edit appends to
        // EOF. Pin the off-by-one so a "+ 1" mutation surfaces.
        assert_eq!(line_at_byte("A\n", 2), 2);
    }

    #[test]
    fn line_at_byte_end_of_file_without_trailing_newline() {
        // Files without a final newline: offset == content.len() must
        // still report the last line's number, not one past it. Tests
        // the slice's upper bound separately from the trailing-newline
        // case above.
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
        // "a\nb\n" is two displayable lines, not three. Matches the
        // Edit-tool convention of line-ended `old_string`/`new_string`.
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
        // Not reachable via `edit_file` (no-op edits rejected), but the
        // helper must terminate cleanly. The renderer's "(no change)"
        // branch covers the resulting empty chunk.
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
    fn common_boundaries_returns_prefix_and_suffix_counts() {
        let old = ["a", "b", "c", "d"];
        let new = ["a", "X", "Y", "d"];
        assert_eq!(common_boundaries(&old, &new), (1, 1));
    }

    #[test]
    fn common_boundaries_disjoint_returns_zero() {
        assert_eq!(common_boundaries(&["a"], &["b"]), (0, 0));
    }

    #[test]
    fn common_boundaries_empty_inputs_return_zero() {
        let empty: [&str; 0] = [];
        assert_eq!(common_boundaries(&empty, &empty), (0, 0));
    }

    #[test]
    fn common_boundaries_asymmetric_lengths_capped_by_shorter_side() {
        // `max_suffix = min(old.len(), new.len()) - prefix` —
        // dropping the `- prefix` term would let suffix overlap the
        // already-counted prefix. Pin the cap with a non-zero prefix
        // and asymmetric lengths.
        assert_eq!(common_boundaries(&["a", "b", "a"], &["a", "X"]), (1, 0));
        assert_eq!(common_boundaries(&["a", "X"], &["a", "b", "a"]), (1, 0));
    }

    // ── synthesize_chunk ──

    #[test]
    fn synthesize_chunk_starts_numbering_at_one() {
        // Resume-fallback shape: line 1 is the best the renderer has
        // when JSONL didn't carry real positions.
        let chunk = synthesize_chunk("a\nb", "x\ny");
        assert_eq!(chunk.old[0].number, 1);
        assert_eq!(chunk.new[0].number, 1);
    }

    #[test]
    fn synthesize_chunk_applies_trim() {
        // Mirrors live-path producer trim so the rendered output for
        // a resumed transcript matches what the live renderer would
        // produce — pure tail insertion still drops the anchor.
        let chunk = synthesize_chunk("fn foo()", "fn foo()\n    body");
        assert!(chunk.old.is_empty());
        assert_eq!(chunk.new.len(), 1);
        assert_eq!(chunk.new[0].text, "    body");
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
        // Pure-LF input must not allocate — the Cow lets the caller
        // skip a copy on the common case. `Cow::Borrowed` also locks
        // in that the returned reference ties back to the input.
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
}
