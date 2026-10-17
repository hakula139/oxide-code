//! Per-session Read-before-Edit gate with xxh64 staleness detection.

use std::collections::HashMap;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::warn;
use xxhash_rust::xxh64::xxh64;

use crate::util::sync::lock_or_recover;

const HASH_SEED: u64 = 0;
pub(crate) const MAX_TRACKED_FILE_SIZE: u64 = 10 * 1024 * 1024;

// ── FileTracker ──

/// Per-session record of files the agent has read, gating subsequent Edit / Write tool calls.
///
/// The gate enforces three invariants before mutation:
///
/// - **Read-before-Edit**: every Edit / Write must follow a `Read` of the same path in the session.
/// - **Full coverage for Write**: overwriting a file needs every line of its current content read,
///   in one Read or across several. An Edit accepts any Read of the current content.
/// - **Freshness**: mutating tools must pass the current bytes through
///   [`Self::verify_current_content`], and only matching xxh64 content counts as unchanged.
///
/// State is persisted into the session JSONL as [`FileSnapshot`]s and re-verified against disk on
/// resume by [`Self::restore_verified`].
#[derive(Default)]
pub(crate) struct FileTracker {
    by_path: Mutex<HashMap<PathBuf, FileState>>,
}

/// What the tracker knows about one file's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileState {
    pub(crate) content_hash: u64,
    pub(crate) last_view: LastView,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) recorded_at: OffsetDateTime,
    /// The tool call that recorded this state. Resume restores the state only while the
    /// transcript still holds that call's result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_use_id: Option<String>,
}

impl FileTracker {
    /// Returns `CacheHit` when a file already tracked as `Full` is read in full again unchanged.
    /// Reads of unchanged content accumulate their line windows, and changed content starts over.
    /// A read that returns content attributes the state to its tool call, `tool_use_id`.
    pub(crate) fn record_read(
        &self,
        path: &Path,
        bytes: &[u8],
        view: LastView,
        tool_use_id: &str,
    ) -> RecordRead {
        let content_hash = xxh64(bytes, HASH_SEED);
        let mut by_path = self.lock();

        if let Some(state) = by_path.get_mut(path)
            && state.content_hash == content_hash
        {
            let outcome = if state.last_view == LastView::Full && view == LastView::Full {
                RecordRead::CacheHit
            } else {
                RecordRead::Inserted
            };
            let seen = std::mem::replace(&mut state.last_view, LastView::Full);
            state.last_view = seen.union(view);
            if outcome == RecordRead::Inserted {
                state.tool_use_id = Some(tool_use_id.to_owned());
            }
            return outcome;
        }

        by_path.insert(
            path.to_path_buf(),
            FileState {
                content_hash,
                last_view: view,
                recorded_at: OffsetDateTime::now_utc(),
                tool_use_id: Some(tool_use_id.to_owned()),
            },
        );
        RecordRead::Inserted
    }

    /// Validates the Read-before-Edit gate against the current on-disk bytes. Write also needs
    /// every line of the current content read.
    pub(crate) fn verify_current_content(
        &self,
        path: &Path,
        current_bytes: &[u8],
        purpose: GatePurpose,
    ) -> Result<(), GateError> {
        let by_path = self.lock();
        let Some(entry) = by_path.get(path) else {
            return Err(GateError::NeverRead {
                path: path.to_path_buf(),
                purpose,
            });
        };
        if xxh64(current_bytes, HASH_SEED) != entry.content_hash {
            return Err(GateError::ContentDrifted {
                path: path.to_path_buf(),
                purpose,
            });
        }
        if purpose == GatePurpose::Write && entry.last_view != LastView::Full {
            return Err(GateError::PartialRead {
                path: path.to_path_buf(),
                purpose,
            });
        }
        Ok(())
    }

    /// Drives `write` in a task of its own, then records the bytes it wrote under the tracker key
    /// it returns, attributed to the tool call `tool_use_id`, so the record lands with the write
    /// even when the caller is dropped mid-write.
    pub(crate) async fn write_and_record<W>(
        self: &Arc<Self>,
        tool_use_id: &str,
        write: W,
    ) -> std::io::Result<()>
    where
        W: Future<Output = std::io::Result<(PathBuf, Vec<u8>)>> + Send + 'static,
    {
        let tracker = Arc::clone(self);
        let tool_use_id = Some(tool_use_id.to_owned());
        tokio::spawn(async move {
            let (key, bytes) = write.await?;
            tracker.record_modify(key, &bytes, tool_use_id);
            Ok(())
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)))
    }

    /// Records bytes the agent just wrote. Always lands as [`LastView::Full`] so a subsequent Edit
    /// passes the gate without requiring a fresh Read.
    fn record_modify(&self, path: PathBuf, bytes: &[u8], tool_use_id: Option<String>) {
        let content_hash = xxh64(bytes, HASH_SEED);
        let now = OffsetDateTime::now_utc();
        self.lock().insert(
            path,
            FileState {
                content_hash,
                last_view: LastView::Full,
                recorded_at: now,
                tool_use_id,
            },
        );
    }

    pub(crate) fn snapshot_all(&self) -> Vec<FileSnapshot> {
        self.lock()
            .iter()
            .map(|(path, state)| FileSnapshot {
                path: path.clone(),
                state: state.clone(),
            })
            .collect()
    }

    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    /// Rehydrates from session JSONL on resume. On duplicate paths the snapshot with the newer
    /// `recorded_at` wins, and it must still match disk by content hash. Returns the dropped paths
    /// so the caller can warn that those files need a fresh Read before any mutation.
    pub(crate) fn restore_verified(&self, snapshots: Vec<FileSnapshot>) -> Vec<PathBuf> {
        let mut by_path = self.lock();
        let mut dropped = Vec::new();

        let mut latest = HashMap::<PathBuf, FileState>::new();
        for FileSnapshot { path, state } in snapshots {
            let replace = latest
                .get(&path)
                .is_none_or(|cur| state.recorded_at >= cur.recorded_at);
            if replace {
                latest.insert(path, state);
            }
        }

        for (path, state) in latest {
            if let Err(reason) = verify_on_disk(&path, state.content_hash) {
                warn!(
                    "dropping tracked file {} ({reason}, will require fresh Read)",
                    path.display()
                );
                dropped.push(path);
                continue;
            }
            let keep = by_path
                .get(&path)
                .is_none_or(|cur| state.recorded_at >= cur.recorded_at);
            if keep {
                by_path.insert(path, state);
            }
        }
        dropped.sort();
        dropped
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<PathBuf, FileState>> {
        lock_or_recover(&self.by_path, "FileTracker")
    }
}

fn verify_on_disk(path: &Path, content_hash: u64) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("stat failed: {error}"))?;
    validate_trackable_file(&metadata).map_err(|error| error.to_string())?;
    let bytes = std::fs::read(path).map_err(|error| format!("read failed: {error}"))?;
    if xxh64(&bytes, HASH_SEED) != content_hash {
        return Err("content drift".to_owned());
    }
    Ok(())
}

// ── LastView ──

/// Lines Read has shown of the tracked content, accumulated across Reads of that content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LastView {
    Full,
    /// Sorted, disjoint, inclusive 1-indexed line windows out of `total_lines`.
    Partial {
        ranges: Vec<(usize, usize)>,
        total_lines: usize,
    },
}

impl LastView {
    /// The view of a Read that showed lines `first..=last` of `total_lines`.
    pub(crate) fn lines(first: usize, last: usize, total_lines: usize) -> Self {
        Self::from_ranges(vec![(first, last)], total_lines)
    }

    fn union(self, other: Self) -> Self {
        match (self, other) {
            (
                Self::Partial {
                    mut ranges,
                    total_lines,
                },
                Self::Partial { ranges: more, .. },
            ) => {
                ranges.extend(more);
                Self::from_ranges(ranges, total_lines)
            }
            _ => Self::Full,
        }
    }

    fn from_ranges(mut ranges: Vec<(usize, usize)>, total_lines: usize) -> Self {
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for (first, last) in ranges {
            match merged.last_mut() {
                Some(prev) if first <= prev.1 + 1 => prev.1 = prev.1.max(last),
                _ => merged.push((first, last)),
            }
        }
        if merged == [(1, total_lines)] {
            Self::Full
        } else {
            Self::Partial {
                ranges: merged,
                total_lines,
            }
        }
    }
}

// ── File Gates ──

/// Which mutating tool is asking for the gate. Carried into [`GateError`] so rendered errors can
/// name the blocked action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GatePurpose {
    Edit,
    Write,
}

impl GatePurpose {
    fn verb(self) -> &'static str {
        match self {
            Self::Edit => "editing",
            Self::Write => "writing to",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TrackableFileError {
    #[error("not a regular file: directory")]
    Directory,
    #[error("not a regular file")]
    NonRegular,
    #[error("too large to verify ({size} bytes, max {max} bytes)")]
    TooLarge { size: u64, max: u64 },
}

pub(crate) fn validate_trackable_file(metadata: &Metadata) -> Result<(), TrackableFileError> {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        return Err(TrackableFileError::Directory);
    }
    if !file_type.is_file() {
        return Err(TrackableFileError::NonRegular);
    }
    let size = metadata.len();
    if size > MAX_TRACKED_FILE_SIZE {
        return Err(TrackableFileError::TooLarge {
            size,
            max: MAX_TRACKED_FILE_SIZE,
        });
    }
    Ok(())
}

// ── FileSnapshot ──

/// JSONL-persisted record of one tracked file. Written into the session log on `finish` and read
/// back on resume by [`FileTracker::restore_verified`], which rehashes each path before
/// re-admitting it to the live tracker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileSnapshot {
    pub(crate) path: PathBuf,
    #[serde(flatten)]
    pub(crate) state: FileState,
}

// ── Outcomes ──

/// Outcome of [`FileTracker::record_read`].
///
/// `CacheHit` means a full re-Read found identical content already tracked as `Full`, so the caller
/// returns [`CACHE_HIT_STUB`] to save tokens. On `Inserted` the caller returns the real bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordRead {
    Inserted,
    CacheHit,
}

/// Reasons the Read-before-Edit / Write gate refuses a tool call. Each variant's `#[error]`
/// message tells the model how to recover (Read first / Read the rest / re-Read).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum GateError {
    #[error("File {} has not been read in this session. Read it before {} it.", .path.display(), .purpose.verb())]
    NeverRead { path: PathBuf, purpose: GatePurpose },
    #[error("File {} has only been read in part. Read the remaining lines, paging with `offset` and `limit` if needed, before {} it.", .path.display(), .purpose.verb())]
    PartialRead { path: PathBuf, purpose: GatePurpose },
    #[error("File {} has been modified externally since it was last read. Re-read it before {} it.", .path.display(), .purpose.verb())]
    ContentDrifted { path: PathBuf, purpose: GatePurpose },
}

/// Returned in place of file bytes when the agent fully re-Reads an unchanged file. The tracker
/// already holds its content hash, so the stub saves tokens without weakening Read-before-Edit.
pub(crate) const CACHE_HIT_STUB: &str =
    "File hasn't been modified since the last read. Returning already-read file.";

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;
    use std::sync::Arc;

    use super::{FileTracker, LastView};

    pub(crate) fn tracker() -> Arc<FileTracker> {
        Arc::new(FileTracker::default())
    }

    pub(crate) fn seed_full_read(tracker: &FileTracker, path: &Path) {
        let bytes = std::fs::read(path).unwrap();
        tracker.record_read(path, &bytes, LastView::Full, "toolu_seed");
    }

    pub(crate) fn tracker_seeded(path: &Path) -> Arc<FileTracker> {
        let tracker = tracker();
        seed_full_read(&tracker, path);
        tracker
    }

    pub(crate) fn record_tracked_file(
        tracker: &FileTracker,
        path: &Path,
        bytes: &[u8],
        tool_use_id: Option<&str>,
    ) {
        std::fs::write(path, bytes).unwrap();
        tracker.record_modify(path.to_path_buf(), bytes, tool_use_id.map(str::to_owned));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn snapshot(path: &Path, content: &[u8], recorded_at: OffsetDateTime) -> FileSnapshot {
        FileSnapshot {
            path: path.to_path_buf(),
            state: FileState {
                content_hash: xxh64(content, HASH_SEED),
                last_view: LastView::Full,
                recorded_at,
                tool_use_id: None,
            },
        }
    }

    // ── record_read: attribution ──

    #[tokio::test]
    async fn record_read_and_write_and_record_attribute_each_state_to_its_call() {
        let dir = tempfile::tempdir().unwrap();
        let tracker = Arc::new(FileTracker::default());
        let read = Path::new("/tmp/read.rs");
        let paged = Path::new("/tmp/paged.rs");
        let written = dir.path().join("written.rs");

        tracker.record_read(read, b"r", LastView::Full, "toolu_read");
        tracker.record_read(paged, b"p", LastView::lines(1, 1, 2), "toolu_page_1");
        tracker.record_read(paged, b"p", LastView::lines(2, 2, 2), "toolu_page_2");
        tracker
            .write_and_record("toolu_write", write_file(written.clone(), b"w"))
            .await
            .unwrap();
        let reread = tracker.record_read(read, b"r", LastView::Full, "toolu_reread");

        assert_eq!(reread, RecordRead::CacheHit);
        let origins: BTreeMap<PathBuf, Option<String>> = tracker
            .snapshot_all()
            .into_iter()
            .map(|snap| (snap.path, snap.state.tool_use_id))
            .collect();
        assert_eq!(
            origins,
            BTreeMap::from([
                (paged.to_path_buf(), Some("toolu_page_2".to_owned())),
                (read.to_path_buf(), Some("toolu_read".to_owned())),
                (written, Some("toolu_write".to_owned())),
            ]),
            "the newest read that returned content owns the state, and a cache hit keeps it",
        );
    }

    /// Writes `bytes` to `path` and returns them under `path` as the tracker key.
    async fn write_file(path: PathBuf, bytes: &[u8]) -> std::io::Result<(PathBuf, Vec<u8>)> {
        tokio::fs::write(&path, bytes).await?;
        Ok((path, bytes.to_vec()))
    }

    // ── record_read ──

    #[test]
    fn record_read_first_full_read_inserts() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let outcome = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");
        assert_eq!(outcome, RecordRead::Inserted);
        let stored = tracker.lock().get(path).cloned().unwrap();
        assert_eq!(stored.content_hash, xxh64(b"hello", HASH_SEED));
        assert_eq!(stored.last_view, LastView::Full);
    }

    #[test]
    fn record_read_cache_hit_preserves_recorded_at() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");
        let first = tracker.lock().get(path).cloned().unwrap();
        let outcome = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");
        let second = tracker.lock().get(path).cloned().unwrap();
        assert_eq!(outcome, RecordRead::CacheHit);
        assert_eq!(first.recorded_at, second.recorded_at);
    }

    #[test]
    fn record_read_changed_content_inserts_not_cache_hit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");
        let outcome = tracker.record_read(path, b"world", LastView::Full, "toolu_test");
        assert_eq!(outcome, RecordRead::Inserted);
    }

    #[test]
    fn record_read_partial_view_does_not_cache_hit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let view = LastView::lines(1, 1, 5);
        _ = tracker.record_read(path, b"hello", view.clone(), "toolu_test");
        let outcome = tracker.record_read(path, b"hello", view, "toolu_test");
        assert_eq!(outcome, RecordRead::Inserted);
    }

    #[test]
    fn record_read_full_after_partial_does_not_cache_hit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::lines(1, 1, 5), "toolu_test");
        let outcome = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");
        assert_eq!(outcome, RecordRead::Inserted);
        let next = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");
        assert_eq!(next, RecordRead::CacheHit);
    }

    #[test]
    fn record_read_partial_after_full_keeps_full_for_unchanged_content() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");

        let outcome = tracker.record_read(path, b"hello", LastView::lines(2, 2, 5), "toolu_test");
        assert_eq!(
            outcome,
            RecordRead::Inserted,
            "partial reads return the slice"
        );
        assert_eq!(tracker.lock()[path].last_view, LastView::Full);
    }

    #[test]
    fn record_read_paging_unchanged_content_accumulates_to_full() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::lines(1, 2, 4), "toolu_test");
        let first = tracker.lock()[path].clone();

        let outcome = tracker.record_read(path, b"hello", LastView::lines(3, 4, 4), "toolu_test");
        assert_eq!(
            outcome,
            RecordRead::Inserted,
            "paged reads return the slice"
        );
        let paged = tracker.lock()[path].clone();
        assert_eq!(paged.last_view, LastView::Full);
        assert_eq!(paged.recorded_at, first.recorded_at);

        _ = tracker.record_read(path, b"world", LastView::lines(3, 4, 4), "toolu_test");
        assert_eq!(
            tracker.lock()[path].last_view,
            LastView::lines(3, 4, 4),
            "changed content starts coverage over",
        );
    }

    #[test]
    fn record_read_concurrent_files_preserve_every_entry() {
        let tracker = Arc::new(FileTracker::default());
        thread::scope(|s| {
            for t in 0..8u32 {
                let tracker = Arc::clone(&tracker);
                s.spawn(move || {
                    for i in 0..100u32 {
                        let path = PathBuf::from(format!("/tmp/t{t}/p{i}"));
                        _ = tracker.record_read(
                            &path,
                            &t.to_le_bytes(),
                            LastView::Full,
                            "toolu_test",
                        );
                    }
                });
            }
        });
        assert_eq!(tracker.lock().len(), 8 * 100);
    }

    // ── verify_current_content ──

    #[test]
    fn verify_current_content_full_match_passes() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");

        let result = tracker.verify_current_content(path, b"hello", GatePurpose::Edit);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn verify_current_content_unread_file_reports_the_blocked_action() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        for purpose in [GatePurpose::Edit, GatePurpose::Write] {
            assert_eq!(
                tracker.verify_current_content(path, b"", purpose),
                Err(GateError::NeverRead {
                    path: path.to_path_buf(),
                    purpose
                }),
            );
        }
    }

    #[test]
    fn verify_current_content_partial_view_blocks_write_but_not_edit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::lines(1, 1, 2), "toolu_test");

        let edit = tracker.verify_current_content(path, b"hello", GatePurpose::Edit);
        assert_eq!(edit, Ok(()));

        let write = tracker.verify_current_content(path, b"hello", GatePurpose::Write);
        assert_eq!(
            write,
            Err(GateError::PartialRead {
                path: path.to_path_buf(),
                purpose: GatePurpose::Write,
            }),
        );

        let drifted = tracker.verify_current_content(path, b"world", GatePurpose::Write);
        assert!(
            matches!(drifted, Err(GateError::ContentDrifted { .. })),
            "drift outranks coverage: {drifted:?}",
        );
    }

    #[test]
    fn verify_current_content_divergent_bytes_rejects_content_drift() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");

        let result = tracker.verify_current_content(path, b"world", GatePurpose::Edit);

        assert_eq!(
            result,
            Err(GateError::ContentDrifted {
                path: path.to_path_buf(),
                purpose: GatePurpose::Edit,
            }),
        );
    }

    // ── write_and_record ──

    #[tokio::test]
    async fn write_and_record_lands_the_bytes_as_a_full_view() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        let key = Path::new("/key/a.rs");
        let tracker = Arc::new(FileTracker::default());
        _ = tracker.record_read(key, b"hello", LastView::lines(1, 1, 2), "toolu_test");

        tracker
            .write_and_record("toolu_test", {
                let path = path.clone();
                async move {
                    tokio::fs::write(&path, b"world").await?;
                    Ok((key.to_path_buf(), b"world".to_vec()))
                }
            })
            .await
            .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"world");
        let stored = tracker.lock().get(key).cloned().unwrap();
        assert_eq!(stored.content_hash, xxh64(b"world", HASH_SEED));
        assert_eq!(stored.last_view, LastView::Full);
    }

    #[tokio::test]
    async fn write_and_record_records_a_write_its_dropped_caller_started() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        let tracker = Arc::new(FileTracker::default());

        let write = tracker.write_and_record("toolu_write", write_file(path.clone(), b"w"));
        _ = tokio::time::timeout(Duration::ZERO, write).await;
        for _ in 0..200 {
            if tracker.lock().contains_key(&path) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert_eq!(std::fs::read(&path).unwrap(), b"w");
        let stored = tracker.lock().get(&path).cloned().unwrap();
        assert_eq!(stored.content_hash, xxh64(b"w", HASH_SEED));
        assert_eq!(stored.tool_use_id.as_deref(), Some("toolu_write"));
    }

    #[tokio::test]
    async fn write_and_record_failed_write_keeps_the_previous_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("a.rs");
        let tracker = Arc::new(FileTracker::default());
        _ = tracker.record_read(&path, b"hello", LastView::Full, "toolu_test");

        let result = tracker
            .write_and_record("toolu_test", write_file(path.clone(), b"world"))
            .await;

        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::NotFound,
            "the parent directory is missing",
        );
        let stored = tracker.lock().get(&path).cloned().unwrap();
        assert_eq!(stored.content_hash, xxh64(b"hello", HASH_SEED));
    }

    // ── snapshot_all ──

    #[test]
    fn snapshot_all_collects_every_tracked_file() {
        let tracker = FileTracker::default();
        _ = tracker.record_read(Path::new("/tmp/a"), b"a", LastView::Full, "toolu_test");
        _ = tracker.record_read(
            Path::new("/tmp/b"),
            b"bbb",
            LastView::lines(1, 2, 3),
            "toolu_test",
        );

        let mut snaps = tracker.snapshot_all();
        snaps.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].path, Path::new("/tmp/a"));
        assert_eq!(snaps[0].state.content_hash, xxh64(b"a", HASH_SEED));
        assert_eq!(snaps[0].state.last_view, LastView::Full);
        assert_eq!(snaps[1].path, Path::new("/tmp/b"));
        assert_eq!(snaps[1].state.content_hash, xxh64(b"bbb", HASH_SEED));
        assert_eq!(snaps[1].state.last_view, LastView::lines(1, 2, 3));
    }

    #[test]
    fn snapshot_all_empty_tracker_is_empty() {
        let snaps = FileTracker::default().snapshot_all();
        assert_eq!(snaps, []);
    }

    // ── clear ──

    #[test]
    fn clear_drops_recorded_reads_so_subsequent_snapshot_is_empty() {
        let tracker = FileTracker::default();
        _ = tracker.record_read(Path::new("/tmp/a"), b"a", LastView::Full, "toolu_test");
        _ = tracker.record_read(Path::new("/tmp/b"), b"b", LastView::Full, "toolu_test");
        assert_eq!(tracker.snapshot_all().len(), 2);

        tracker.clear();

        assert_eq!(tracker.snapshot_all(), []);
    }

    // ── restore_verified ──

    #[test]
    fn restore_verified_matching_content_repopulates_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"persisted bytes").unwrap();

        let snap = snapshot(&path, b"persisted bytes", OffsetDateTime::now_utc());

        let tracker = FileTracker::default();
        tracker.restore_verified(vec![snap.clone()]);

        assert_eq!(tracker.lock().get(&path), Some(&snap.state));
    }

    #[test]
    fn restore_verified_content_drift_drops_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"new").unwrap();

        let snap = snapshot(&path, b"old", OffsetDateTime::now_utc());

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![snap]);
        assert_eq!(dropped, vec![path.clone()]);
        assert!(tracker.lock().get(&path).is_none());
    }

    #[test]
    fn restore_verified_non_regular_file_drops_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir");
        std::fs::create_dir(&path).unwrap();
        let snap = snapshot(&path, b"", OffsetDateTime::now_utc());

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![snap]);

        assert_eq!(dropped, vec![path.clone()]);
        assert!(tracker.lock().is_empty());
    }

    #[test]
    fn restore_verified_too_large_file_drops_snapshot_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.rs");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_TRACKED_FILE_SIZE + 1).unwrap();
        let snap = snapshot(&path, b"", OffsetDateTime::now_utc());

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![snap]);

        assert_eq!(dropped, vec![path.clone()]);
        assert!(tracker.lock().is_empty());
    }

    #[test]
    fn restore_verified_reports_dropped_paths_in_sorted_order() {
        let dir = tempfile::tempdir().unwrap();
        let kept_path = dir.path().join("kept.rs");
        let drifted_path = dir.path().join("drifted.rs");
        std::fs::write(&kept_path, b"alpha").unwrap();
        std::fs::write(&drifted_path, b"now larger").unwrap();
        let missing_path = PathBuf::from("/nonexistent/x.rs");

        let snaps = vec![
            snapshot(&kept_path, b"alpha", OffsetDateTime::now_utc()),
            snapshot(&drifted_path, b"", OffsetDateTime::now_utc()),
            snapshot(&missing_path, b"", OffsetDateTime::now_utc()),
        ];

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(snaps);
        let mut expected = vec![drifted_path, missing_path];
        expected.sort();
        assert_eq!(
            dropped, expected,
            "both the drifted and the missing snapshots must be reported",
        );
        let stored_paths: Vec<_> = tracker.lock().keys().cloned().collect();
        assert_eq!(stored_paths, vec![kept_path]);
    }

    #[test]
    fn restore_verified_verifies_only_newest_snapshot_per_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"x").unwrap();

        let mut older = snapshot(&path, b"stale", OffsetDateTime::UNIX_EPOCH);
        older.state.last_view = LastView::lines(1, 1, 2);
        let newer = snapshot(
            &path,
            b"x",
            OffsetDateTime::UNIX_EPOCH + Duration::from_mins(1),
        );

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![older.clone(), newer.clone()]);
        assert_eq!(dropped, [] as [PathBuf; 0]);
        let stored = tracker.lock().get(&path).cloned().unwrap();
        assert_eq!(stored.last_view, LastView::Full, "newer recorded_at wins");

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![newer, older]);
        assert_eq!(dropped, [] as [PathBuf; 0]);
        let stored = tracker.lock().get(&path).cloned().unwrap();
        assert_eq!(
            stored.last_view,
            LastView::Full,
            "older does not displace newer",
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_verified_unreadable_file_drops_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"content").unwrap();
        let snap = snapshot(&path, b"content", OffsetDateTime::now_utc());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![snap]);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(dropped, vec![path.clone()]);
        assert!(tracker.lock().is_empty());
    }

    // ── LastView::lines ──

    #[test]
    fn lines_covering_every_line_is_full() {
        assert_eq!(LastView::lines(1, 3, 3), LastView::Full);
        assert_eq!(
            LastView::lines(2, 3, 3),
            LastView::Partial {
                ranges: vec![(2, 3)],
                total_lines: 3,
            },
        );
    }

    // ── LastView::union ──

    #[test]
    fn union_merges_overlapping_and_adjacent_windows_until_full() {
        let view = LastView::lines(5, 6, 10)
            .union(LastView::lines(1, 2, 10))
            .union(LastView::lines(3, 3, 10))
            .union(LastView::lines(8, 9, 10))
            .union(LastView::lines(6, 6, 10));
        assert_eq!(
            view,
            LastView::Partial {
                ranges: vec![(1, 3), (5, 6), (8, 9)],
                total_lines: 10,
            },
        );

        assert_eq!(view.union(LastView::lines(4, 10, 10)), LastView::Full);
    }

    #[test]
    fn union_with_full_is_full() {
        assert_eq!(
            LastView::Full.union(LastView::lines(1, 1, 5)),
            LastView::Full
        );
        assert_eq!(
            LastView::lines(1, 1, 5).union(LastView::Full),
            LastView::Full
        );
    }

    // ── LastView ──

    #[test]
    fn last_view_full_serializes_kind_only() {
        let json = serde_json::to_value(LastView::Full).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "full"}));
    }

    #[test]
    fn last_view_partial_round_trips_through_json() {
        let view = LastView::lines(3, 5, 9);
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "partial", "ranges": [[3, 5]], "total_lines": 9}),
        );
        let parsed: LastView = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, view);
    }

    // ── TrackableFileError Display ──

    #[test]
    fn trackable_file_error_display_names_directory() {
        let err = TrackableFileError::Directory;
        assert_eq!(err.to_string(), "not a regular file: directory");
    }

    #[test]
    fn trackable_file_error_display_names_non_regular_file() {
        let err = TrackableFileError::NonRegular;
        assert_eq!(err.to_string(), "not a regular file");
    }

    #[test]
    fn trackable_file_error_display_names_size_limit() {
        let err = TrackableFileError::TooLarge { size: 12, max: 10 };
        assert_eq!(
            err.to_string(),
            "too large to verify (12 bytes, max 10 bytes)"
        );
    }

    // ── validate_trackable_file ──

    #[test]
    fn validate_trackable_file_accepts_regular_file_at_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracked.rs");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_TRACKED_FILE_SIZE).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();

        assert_eq!(validate_trackable_file(&metadata), Ok(()));
    }

    #[test]
    fn validate_trackable_file_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = std::fs::metadata(dir.path()).unwrap();

        assert_eq!(
            validate_trackable_file(&metadata),
            Err(TrackableFileError::Directory)
        );
    }

    #[test]
    fn validate_trackable_file_rejects_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.rs");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_TRACKED_FILE_SIZE + 1).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();

        assert_eq!(
            validate_trackable_file(&metadata),
            Err(TrackableFileError::TooLarge {
                size: MAX_TRACKED_FILE_SIZE + 1,
                max: MAX_TRACKED_FILE_SIZE,
            }),
        );
    }

    #[cfg(unix)]
    #[test]
    fn validate_trackable_file_rejects_non_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();

        assert_eq!(
            validate_trackable_file(&metadata),
            Err(TrackableFileError::NonRegular)
        );
    }

    // ── FileSnapshot ──

    #[test]
    fn file_snapshot_round_trips_through_json() {
        let snap = FileSnapshot {
            path: PathBuf::from("/tmp/a.rs"),
            state: FileState {
                content_hash: 0xDEAD_BEEF,
                last_view: LastView::Partial {
                    ranges: vec![(1, 2), (4, 5)],
                    total_lines: 9,
                },
                recorded_at: time::macros::datetime!(2026-04-29 12:34:56 UTC),
                tool_use_id: Some("toolu_1".to_owned()),
            },
        };

        let json = serde_json::to_value(&snap).unwrap();
        let parsed: FileSnapshot = serde_json::from_value(json.clone()).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "path": "/tmp/a.rs",
                "content_hash": 0xDEAD_BEEF_u64,
                "last_view": {"kind": "partial", "ranges": [[1, 2], [4, 5]], "total_lines": 9},
                "recorded_at": "2026-04-29T12:34:56Z",
                "tool_use_id": "toolu_1",
            }),
        );
        assert_eq!(parsed, snap);
    }

    // ── GateError Display ──

    #[test]
    fn gate_error_never_read_renders_with_path_and_edit_verb() {
        let err = GateError::NeverRead {
            path: PathBuf::from("/tmp/a.rs"),
            purpose: GatePurpose::Edit,
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/a.rs"), "path is named: {msg}");
        assert!(msg.contains("has not been read"));
        assert!(msg.contains("editing"));
    }

    #[test]
    fn gate_error_never_read_renders_with_write_verb() {
        let err = GateError::NeverRead {
            path: PathBuf::from("/tmp/a.rs"),
            purpose: GatePurpose::Write,
        };
        let msg = err.to_string();
        assert!(msg.contains("has not been read"));
        assert!(msg.contains("writing to"));
    }

    #[test]
    fn gate_error_partial_read_renders_with_path_and_remaining_lines() {
        let err = GateError::PartialRead {
            path: PathBuf::from("/tmp/a.rs"),
            purpose: GatePurpose::Write,
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/a.rs"), "path is named: {msg}");
        assert!(msg.contains("only been read in part"));
        assert!(msg.contains("Read the remaining lines"));
        assert!(msg.contains("writing to"));
    }

    #[test]
    fn gate_error_content_drifted_renders_with_path_and_modified_externally() {
        let err = GateError::ContentDrifted {
            path: PathBuf::from("/tmp/a.rs"),
            purpose: GatePurpose::Edit,
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/a.rs"), "path is named: {msg}");
        assert!(msg.contains("modified externally"));
        assert!(msg.contains("Re-read"));
    }
}
