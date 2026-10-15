//! Per-session Read-before-Edit gate with xxh64 staleness detection.

use std::collections::HashMap;
use std::fs::Metadata;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::warn;
use xxhash_rust::xxh64::xxh64;

const HASH_SEED: u64 = 0;
pub(crate) const MAX_TRACKED_FILE_SIZE: u64 = 10 * 1024 * 1024;

// ── Attribution ──

tokio::task_local! {
    static TOOL_USE_ID: String;
}

/// Runs `fut` with every tracker record it makes attributed to the tool call `tool_use_id`.
pub(crate) async fn attribute_to<F: Future>(tool_use_id: &str, fut: F) -> F::Output {
    TOOL_USE_ID.scope(tool_use_id.to_owned(), fut).await
}

fn current_tool_use_id() -> Option<String> {
    TOOL_USE_ID.try_with(Clone::clone).ok()
}

// ── FileTracker ──

/// Per-session record of files the agent has read, gating subsequent Edit / Write tool calls.
///
/// The gate enforces three invariants before mutation:
///
/// - **Read-before-Edit**: every Edit / Write must follow a `Read` of the same path within the
///   session.
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileState {
    content_hash: u64,
    last_view: LastView,
    recorded_at: OffsetDateTime,
    tool_use_id: Option<String>,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackableFileError {
    Directory,
    NonRegular,
    TooLarge { size: u64, max: u64 },
}

impl std::fmt::Display for TrackableFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Directory => f.write_str("not a regular file: directory"),
            Self::NonRegular => f.write_str("not a regular file"),
            Self::TooLarge { size, max } => {
                write!(f, "too large to verify ({size} bytes, max {max} bytes)")
            }
        }
    }
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

impl FileTracker {
    /// Returns `CacheHit` when a file already tracked as `Full` is read in full again unchanged.
    /// Reads of unchanged content accumulate their line windows, and changed content starts over.
    /// A read that returns content attributes the state to its tool call.
    pub(crate) fn record_read(&self, path: &Path, bytes: &[u8], view: LastView) -> RecordRead {
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
                state.tool_use_id = current_tool_use_id();
            }
            return outcome;
        }

        by_path.insert(
            path.to_path_buf(),
            FileState {
                content_hash,
                last_view: view,
                recorded_at: OffsetDateTime::now_utc(),
                tool_use_id: current_tool_use_id(),
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
    /// it returns, so the record lands with the write even when the caller is dropped mid-write.
    pub(crate) async fn write_and_record<W>(self: &Arc<Self>, write: W) -> std::io::Result<()>
    where
        W: Future<Output = std::io::Result<(PathBuf, Vec<u8>)>> + Send + 'static,
    {
        let tracker = Arc::clone(self);
        let tool_use_id = current_tool_use_id();
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
                content_hash: state.content_hash,
                last_view: state.last_view.clone(),
                recorded_at: state.recorded_at,
                tool_use_id: state.tool_use_id.clone(),
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

        let mut latest = HashMap::<PathBuf, FileSnapshot>::new();
        for snap in snapshots {
            let replace = latest
                .get(&snap.path)
                .is_none_or(|cur| snap.recorded_at >= cur.recorded_at);
            if replace {
                latest.insert(snap.path.clone(), snap);
            }
        }

        for snap in latest.into_values() {
            let metadata = match std::fs::metadata(&snap.path) {
                Ok(metadata) => metadata,
                Err(e) => {
                    warn!(
                        "dropping tracked file {} (stat failed, will require fresh Read): {e}",
                        snap.path.display()
                    );
                    dropped.push(snap.path);
                    continue;
                }
            };
            if let Err(err) = validate_trackable_file(&metadata) {
                warn!(
                    "dropping tracked file {} ({err}, will require fresh Read)",
                    snap.path.display()
                );
                dropped.push(snap.path);
                continue;
            }
            let current_bytes = match std::fs::read(&snap.path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!(
                        "dropping tracked file {} (read failed, will require fresh Read): {e}",
                        snap.path.display()
                    );
                    dropped.push(snap.path);
                    continue;
                }
            };
            if xxh64(&current_bytes, HASH_SEED) != snap.content_hash {
                warn!(
                    "dropping tracked file {} (content drift, will require fresh Read)",
                    snap.path.display()
                );
                dropped.push(snap.path);
                continue;
            }
            let keep = by_path
                .get(&snap.path)
                .is_none_or(|cur| snap.recorded_at >= cur.recorded_at);
            if keep {
                by_path.insert(
                    snap.path,
                    FileState {
                        content_hash: snap.content_hash,
                        last_view: snap.last_view,
                        recorded_at: snap.recorded_at,
                        tool_use_id: snap.tool_use_id,
                    },
                );
            }
        }
        dropped.sort();
        dropped
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, FileState>> {
        match self.by_path.lock() {
            Ok(guard) => guard,
            Err(e) => {
                tracing::warn!("FileTracker mutex poisoned, recovering");
                e.into_inner()
            }
        }
    }
}

// ── FileSnapshot ──

/// JSONL-persisted record of one tracked file. Written into the session log on `finish` and read
/// back on resume by [`FileTracker::restore_verified`], which rehashes each path before
/// re-admitting it to the live tracker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileSnapshot {
    pub(crate) path: PathBuf,
    pub(crate) content_hash: u64,
    pub(crate) last_view: LastView,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) recorded_at: OffsetDateTime,
    /// The tool call that recorded this state. Resume keeps the snapshot only while the
    /// transcript still holds that call's result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tool_use_id: Option<String>,
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
/// message tells the model exactly how to recover (Read first / Read the rest / re-Read).
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
        tracker.record_read(path, &bytes, LastView::Full);
    }

    pub(crate) fn tracker_seeded(path: &Path) -> Arc<FileTracker> {
        let tracker = tracker();
        seed_full_read(&tracker, path);
        tracker
    }

    pub(crate) fn record_tracked_file(tracker: &FileTracker, path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        tracker.record_modify(path.to_path_buf(), bytes, super::current_tool_use_id());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    // ── attribute_to ──

    #[tokio::test]
    async fn attribute_to_stamps_records_with_the_calling_tool_use() {
        let dir = tempfile::tempdir().unwrap();
        let tracker = Arc::new(FileTracker::default());
        let read = Path::new("/tmp/read.rs");
        let paged = Path::new("/tmp/paged.rs");
        let written = dir.path().join("written.rs");
        let outside = Path::new("/tmp/outside.rs");

        attribute_to("toolu_read", async {
            tracker.record_read(read, b"r", LastView::Full)
        })
        .await;
        attribute_to("toolu_page_1", async {
            tracker.record_read(paged, b"p", LastView::lines(1, 1, 2))
        })
        .await;
        attribute_to("toolu_page_2", async {
            tracker.record_read(paged, b"p", LastView::lines(2, 2, 2))
        })
        .await;
        attribute_to(
            "toolu_write",
            tracker.write_and_record(write_file(written.clone(), b"w")),
        )
        .await
        .unwrap();
        let reread = attribute_to("toolu_reread", async {
            tracker.record_read(read, b"r", LastView::Full)
        })
        .await;
        tracker.record_read(outside, b"o", LastView::Full);

        assert_eq!(reread, RecordRead::CacheHit);
        let origins: BTreeMap<PathBuf, Option<String>> = tracker
            .snapshot_all()
            .into_iter()
            .map(|snap| (snap.path, snap.tool_use_id))
            .collect();
        assert_eq!(
            origins,
            BTreeMap::from([
                (outside.to_path_buf(), None),
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

    // ── record_read ──

    #[test]
    fn record_read_first_full_read_inserts() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let outcome = tracker.record_read(path, b"hello", LastView::Full);
        assert_eq!(outcome, RecordRead::Inserted);
        let stored = tracker.lock().get(path).cloned().unwrap();
        assert_eq!(stored.content_hash, xxh64(b"hello", HASH_SEED));
        assert_eq!(stored.last_view, LastView::Full);
    }

    #[test]
    fn record_read_redundant_full_read_is_cache_hit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::Full);
        let outcome = tracker.record_read(path, b"hello", LastView::Full);
        assert_eq!(
            outcome,
            RecordRead::CacheHit,
            "second full read of unchanged file is a cache hit",
        );
    }

    #[test]
    fn record_read_cache_hit_preserves_recorded_at() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::Full);
        let first = tracker.lock().get(path).cloned().unwrap();
        let outcome = tracker.record_read(path, b"hello", LastView::Full);
        let second = tracker.lock().get(path).cloned().unwrap();
        assert_eq!(outcome, RecordRead::CacheHit);
        assert_eq!(first.recorded_at, second.recorded_at);
    }

    #[test]
    fn record_read_changed_content_inserts_not_cache_hit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::Full);
        let outcome = tracker.record_read(path, b"world", LastView::Full);
        assert_eq!(outcome, RecordRead::Inserted);
    }

    #[test]
    fn record_read_partial_view_does_not_cache_hit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let view = LastView::lines(1, 1, 5);
        _ = tracker.record_read(path, b"hello", view.clone());
        let outcome = tracker.record_read(path, b"hello", view);
        assert_eq!(outcome, RecordRead::Inserted);
    }

    #[test]
    fn record_read_full_after_partial_does_not_cache_hit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::lines(1, 1, 5));
        let outcome = tracker.record_read(path, b"hello", LastView::Full);
        assert_eq!(outcome, RecordRead::Inserted);
        let next = tracker.record_read(path, b"hello", LastView::Full);
        assert_eq!(next, RecordRead::CacheHit);
    }

    #[test]
    fn record_read_partial_after_full_keeps_full_for_unchanged_content() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::Full);

        let outcome = tracker.record_read(path, b"hello", LastView::lines(2, 2, 5));
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
        _ = tracker.record_read(path, b"hello", LastView::lines(1, 2, 4));
        let first = tracker.lock()[path].clone();

        let outcome = tracker.record_read(path, b"hello", LastView::lines(3, 4, 4));
        assert_eq!(
            outcome,
            RecordRead::Inserted,
            "paged reads return the slice"
        );
        let paged = tracker.lock()[path].clone();
        assert_eq!(paged.last_view, LastView::Full);
        assert_eq!(paged.recorded_at, first.recorded_at);

        _ = tracker.record_read(path, b"world", LastView::lines(3, 4, 4));
        assert_eq!(
            tracker.lock()[path].last_view,
            LastView::lines(3, 4, 4),
            "changed content starts coverage over",
        );
    }

    // ── verify_current_content ──

    #[test]
    fn verify_current_content_full_match_passes() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::Full);

        let result = tracker.verify_current_content(path, b"hello", GatePurpose::Edit);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn verify_current_content_no_entry_errors_never_read() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let result = tracker.verify_current_content(path, b"", GatePurpose::Edit);
        assert_eq!(
            result,
            Err(GateError::NeverRead {
                path: path.to_path_buf(),
                purpose: GatePurpose::Edit,
            }),
        );
    }

    #[test]
    fn verify_current_content_no_entry_carries_write_purpose() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let result = tracker.verify_current_content(path, b"", GatePurpose::Write);
        assert_eq!(
            result,
            Err(GateError::NeverRead {
                path: path.to_path_buf(),
                purpose: GatePurpose::Write,
            }),
        );
    }

    #[test]
    fn verify_current_content_partial_view_blocks_write_but_not_edit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", LastView::lines(1, 1, 2));

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
        _ = tracker.record_read(path, b"hello", LastView::Full);

        let result = tracker.verify_current_content(path, b"world", GatePurpose::Edit);

        assert_eq!(
            result,
            Err(GateError::ContentDrifted {
                path: path.to_path_buf(),
                purpose: GatePurpose::Edit,
            }),
        );
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

    // ── write_and_record ──

    #[tokio::test]
    async fn write_and_record_lands_the_bytes_as_a_full_view() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        let key = Path::new("/key/a.rs");
        let tracker = Arc::new(FileTracker::default());
        _ = tracker.record_read(key, b"hello", LastView::lines(1, 1, 2));

        tracker
            .write_and_record({
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

        let write = attribute_to(
            "toolu_write",
            tracker.write_and_record(write_file(path.clone(), b"w")),
        );
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
        _ = tracker.record_read(&path, b"hello", LastView::Full);

        let result = tracker
            .write_and_record(write_file(path.clone(), b"world"))
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
        _ = tracker.record_read(Path::new("/tmp/a"), b"a", LastView::Full);
        _ = tracker.record_read(Path::new("/tmp/b"), b"bbb", LastView::lines(1, 2, 3));

        let mut snaps = tracker.snapshot_all();
        snaps.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].path, Path::new("/tmp/a"));
        assert_eq!(snaps[0].content_hash, xxh64(b"a", HASH_SEED));
        assert_eq!(snaps[0].last_view, LastView::Full);
        assert_eq!(snaps[1].path, Path::new("/tmp/b"));
        assert_eq!(snaps[1].content_hash, xxh64(b"bbb", HASH_SEED));
        assert_eq!(snaps[1].last_view, LastView::lines(1, 2, 3));
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
        _ = tracker.record_read(Path::new("/tmp/a"), b"a", LastView::Full);
        _ = tracker.record_read(Path::new("/tmp/b"), b"b", LastView::Full);
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

        let snap = FileSnapshot {
            path: path.clone(),
            content_hash: xxh64(b"persisted bytes", HASH_SEED),
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
            tool_use_id: None,
        };

        let tracker = FileTracker::default();
        tracker.restore_verified(vec![snap.clone()]);

        let stored = tracker.lock().get(&path).cloned();
        let stored = stored.expect("matching content restores the entry");
        assert_eq!(stored.content_hash, snap.content_hash);
        assert_eq!(stored.last_view, LastView::Full);
    }

    #[test]
    fn restore_verified_content_drift_drops_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"new").unwrap();

        let snap = FileSnapshot {
            path: path.clone(),
            content_hash: xxh64(b"old", HASH_SEED),
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
            tool_use_id: None,
        };

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![snap]);
        assert_eq!(dropped, vec![path.clone()]);
        assert!(tracker.lock().get(&path).is_none());
    }

    #[test]
    fn restore_verified_missing_file_drops_snapshot() {
        let snap = FileSnapshot {
            path: PathBuf::from("/nonexistent/path/a.rs"),
            content_hash: 0,
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
            tool_use_id: None,
        };

        let tracker = FileTracker::default();
        tracker.restore_verified(vec![snap]);
        assert!(tracker.lock().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn restore_verified_unreadable_file_drops_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"content").unwrap();
        let snap = FileSnapshot {
            path: path.clone(),
            content_hash: xxh64(b"content", HASH_SEED),
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
            tool_use_id: None,
        };

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![snap]);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(dropped, vec![path.clone()]);
        assert!(tracker.lock().is_empty());
    }

    #[test]
    fn restore_verified_non_regular_file_drops_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir");
        std::fs::create_dir(&path).unwrap();
        let snap = FileSnapshot {
            path: path.clone(),
            content_hash: 0,
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
            tool_use_id: None,
        };

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
        let snap = FileSnapshot {
            path: path.clone(),
            content_hash: 0,
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
            tool_use_id: None,
        };

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![snap]);

        assert_eq!(dropped, vec![path.clone()]);
        assert!(tracker.lock().is_empty());
    }

    #[test]
    fn restore_verified_returns_paths_of_dropped_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let kept_path = dir.path().join("kept.rs");
        let drifted_path = dir.path().join("drifted.rs");
        std::fs::write(&kept_path, b"alpha").unwrap();
        std::fs::write(&drifted_path, b"now larger").unwrap();
        let missing_path = PathBuf::from("/nonexistent/x.rs");

        let snaps = vec![
            FileSnapshot {
                path: kept_path.clone(),
                content_hash: xxh64(b"alpha", HASH_SEED),
                last_view: LastView::Full,
                recorded_at: OffsetDateTime::now_utc(),
                tool_use_id: None,
            },
            FileSnapshot {
                path: drifted_path.clone(),
                content_hash: 0,
                last_view: LastView::Full,
                recorded_at: OffsetDateTime::now_utc(),
                tool_use_id: None,
            },
            FileSnapshot {
                path: missing_path.clone(),
                content_hash: 0,
                last_view: LastView::Full,
                recorded_at: OffsetDateTime::now_utc(),
                tool_use_id: None,
            },
        ];

        let tracker = FileTracker::default();
        let mut dropped = tracker.restore_verified(snaps);
        dropped.sort();
        let mut expected = vec![drifted_path, missing_path];
        expected.sort();
        assert_eq!(
            dropped, expected,
            "both the drifted and the missing snapshots must be reported",
        );
        assert!(tracker.lock().contains_key(&kept_path));
    }

    #[test]
    fn restore_verified_verifies_only_newest_snapshot_per_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"x").unwrap();
        let content_hash = xxh64(b"x", HASH_SEED);

        let older = FileSnapshot {
            path: path.clone(),
            content_hash: xxh64(b"stale", HASH_SEED),
            last_view: LastView::lines(1, 1, 2),
            recorded_at: OffsetDateTime::UNIX_EPOCH,
            tool_use_id: None,
        };
        let newer = FileSnapshot {
            path: path.clone(),
            content_hash,
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::UNIX_EPOCH + Duration::from_mins(1),
            tool_use_id: None,
        };

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

    // ── Concurrency ──

    #[test]
    fn concurrent_record_read_does_not_corrupt_map() {
        let tracker = Arc::new(FileTracker::default());
        thread::scope(|s| {
            for t in 0..8u32 {
                let tracker = Arc::clone(&tracker);
                s.spawn(move || {
                    for i in 0..100u32 {
                        let path = PathBuf::from(format!("/tmp/t{t}/p{i}"));
                        _ = tracker.record_read(&path, &t.to_le_bytes(), LastView::Full);
                    }
                });
            }
        });
        assert_eq!(tracker.lock().len(), 8 * 100);
    }

    #[test]
    fn lock_recovers_from_poisoned_mutex() {
        let tracker = Arc::new(FileTracker::default());
        let t = Arc::clone(&tracker);
        _ = std::thread::spawn(move || {
            let _guard = t.by_path.lock().unwrap();
            panic!("deliberate poison");
        })
        .join();
        // Mutex is now poisoned. record_read must recover via lock().
        let result = tracker.record_read(Path::new("/tmp/poison_test"), b"hello", LastView::Full);
        assert_eq!(result, RecordRead::Inserted);
        assert_eq!(tracker.lock().len(), 1);
    }

    // ── FileSnapshot ──

    #[test]
    fn file_snapshot_round_trips_through_json() {
        let snap = FileSnapshot {
            path: PathBuf::from("/tmp/a.rs"),
            content_hash: 0xDEAD_BEEF,
            last_view: LastView::Partial {
                ranges: vec![(1, 2), (4, 5)],
                total_lines: 9,
            },
            recorded_at: time::macros::datetime!(2026-04-29 12:34:56 UTC),
            tool_use_id: Some("toolu_1".to_owned()),
        };
        let json = serde_json::to_value(&snap).unwrap();
        assert_eq!(json["path"], "/tmp/a.rs");
        assert_eq!(json["content_hash"], 0xDEAD_BEEF_u64);
        assert_eq!(
            json["last_view"],
            serde_json::json!({"kind": "partial", "ranges": [[1, 2], [4, 5]], "total_lines": 9}),
        );
        assert_eq!(json["recorded_at"], "2026-04-29T12:34:56Z");
        assert_eq!(json["tool_use_id"], "toolu_1");

        let parsed: FileSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, snap);
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
}
