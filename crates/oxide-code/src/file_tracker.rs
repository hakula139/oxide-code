//! Per-session Read-before-Edit gate with xxh64 staleness detection.

use std::collections::HashMap;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::warn;
use xxhash_rust::xxh64::xxh64;

const HASH_SEED: u64 = 0;
pub(crate) const MAX_TRACKED_FILE_SIZE: u64 = 10 * 1024 * 1024;

// ── FileTracker ──

/// Per-session record of files the agent has read, gating subsequent Edit / Write tool calls.
///
/// The gate enforces three invariants before mutation:
///
/// - **Read-before-Edit**: every Edit / Write must follow a `Read` of the same path within the
///   session.
/// - **Full read required**: a partial (offset / limit) Read does not satisfy the gate; the agent
///   must Read the file in full before mutating it.
/// - **Freshness**: mutating tools must pass the current bytes through
///   [`Self::verify_current_content`]; only matching xxh64 content qualifies as unchanged.
///
/// State is persisted into the session JSONL as [`FileSnapshot`]s and re-verified against disk on
/// resume — see [`Self::restore_verified`].
#[derive(Default)]
pub(crate) struct FileTracker {
    by_path: Mutex<HashMap<PathBuf, FileState>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileState {
    content_hash: u64,
    mtime: SystemTime,
    size: u64,
    last_view: LastView,
    recorded_at: OffsetDateTime,
}

/// Extent of the most recent Read. Only `Full` satisfies the Edit / Write gate; `Partial` requires
/// the agent to re-Read the file in full before mutating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LastView {
    Full,
    Partial { offset: usize, limit: usize },
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
    /// Returns `CacheHit` when an unchanged full-file re-Read can be stubbed.
    pub(crate) fn record_read(
        &self,
        path: &Path,
        bytes: &[u8],
        mtime: SystemTime,
        size: u64,
        view: LastView,
    ) -> RecordRead {
        let content_hash = xxh64(bytes, HASH_SEED);
        let mut by_path = self.lock();

        if matches!(view, LastView::Full)
            && let Some(state) = by_path.get_mut(path)
            && state.last_view == LastView::Full
            && state.content_hash == content_hash
        {
            state.mtime = mtime;
            state.size = size;
            return RecordRead::CacheHit;
        }

        by_path.insert(
            path.to_path_buf(),
            FileState {
                content_hash,
                mtime,
                size,
                last_view: view,
                recorded_at: OffsetDateTime::now_utc(),
            },
        );
        RecordRead::Inserted
    }

    /// Validates the Read-before-Edit gate against the current on-disk bytes.
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
        if !matches!(entry.last_view, LastView::Full) {
            return Err(GateError::PartialRead {
                path: path.to_path_buf(),
                purpose,
            });
        }
        if xxh64(current_bytes, HASH_SEED) == entry.content_hash {
            Ok(())
        } else {
            Err(GateError::ContentDrifted {
                path: path.to_path_buf(),
                purpose,
            })
        }
    }

    /// Records bytes the agent just wrote. Always lands as [`LastView::Full`] so a subsequent Edit
    /// passes the gate without requiring a fresh Read.
    pub(crate) fn record_modify(&self, path: &Path, bytes: &[u8], mtime: SystemTime, size: u64) {
        let content_hash = xxh64(bytes, HASH_SEED);
        let now = OffsetDateTime::now_utc();
        self.lock().insert(
            path.to_path_buf(),
            FileState {
                content_hash,
                mtime,
                size,
                last_view: LastView::Full,
                recorded_at: now,
            },
        );
    }

    /// Best-effort post-write stat + record; warn-logs on stat failure so subsequent Edit calls
    /// re-Read the file rather than failing silently against a stale snapshot.
    pub(crate) async fn record_modify_after_write(&self, path: &Path, bytes: &[u8]) {
        match tokio::fs::metadata(path).await.and_then(|m| {
            let modified = m.modified()?;
            Ok((m.len(), modified))
        }) {
            Ok((size, mtime)) => self.record_modify(path, bytes, mtime, size),
            Err(e) => warn!(
                "skip post-write tracker update for {} (stat failed): {e}",
                path.display()
            ),
        }
    }

    pub(crate) fn snapshot_all(&self) -> Vec<FileSnapshot> {
        self.lock()
            .iter()
            .map(|(path, state)| FileSnapshot {
                path: path.clone(),
                content_hash: state.content_hash,
                mtime: OffsetDateTime::from(state.mtime),
                size: state.size,
                last_view: state.last_view,
                recorded_at: state.recorded_at,
            })
            .collect()
    }

    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    /// Rehydrates from session JSONL on resume. Each snapshot must still match disk by content
    /// hash — drifted or missing files are dropped, forcing a fresh Read before any mutation. On
    /// duplicate paths the entry with the newer `recorded_at` wins. Returns the dropped paths so
    /// the caller can warn the user that those files need a fresh Read.
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
            let current_size = metadata.len();
            if let Err(err) = validate_trackable_file(&metadata) {
                warn!(
                    "dropping tracked file {} ({err}, will require fresh Read)",
                    snap.path.display()
                );
                dropped.push(snap.path);
                continue;
            }
            let current_mtime = match metadata.modified() {
                Ok(mtime) => mtime,
                Err(e) => {
                    warn!(
                        "dropping tracked file {} (mtime failed, will require fresh Read): {e}",
                        snap.path.display()
                    );
                    dropped.push(snap.path);
                    continue;
                }
            };
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
                    "dropping tracked file {} (content drift: stored {}b vs current {}b)",
                    snap.path.display(),
                    snap.size,
                    current_size,
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
                        mtime: current_mtime,
                        size: current_size,
                        last_view: snap.last_view,
                        recorded_at: snap.recorded_at,
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
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) mtime: OffsetDateTime,
    pub(crate) size: u64,
    pub(crate) last_view: LastView,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) recorded_at: OffsetDateTime,
}

// ── Outcomes ──

/// Outcome of [`FileTracker::record_read`].
///
/// `CacheHit` means the path was already in the tracker with identical content (full-read +
/// matching xxh64); the caller should return [`CACHE_HIT_STUB`] instead of the real bytes to
/// save tokens. `Inserted` is the regular path — fresh read, snapshot stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordRead {
    Inserted,
    CacheHit,
}

/// Reasons the Read-before-Edit / Write gate refuses a tool call. Each variant's `#[error]`
/// message tells the model exactly how to recover (Read first / Read in full / re-Read).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum GateError {
    #[error("File {} needs a full Read before {} it.", .path.display(), .purpose.verb())]
    NeverRead { path: PathBuf, purpose: GatePurpose },
    #[error("File {} was read with offset / limit. Read the full file before {} it.", .path.display(), .purpose.verb())]
    PartialRead { path: PathBuf, purpose: GatePurpose },
    #[error("File {} has been modified externally since it was last read. Re-read it before {} it.", .path.display(), .purpose.verb())]
    ContentDrifted { path: PathBuf, purpose: GatePurpose },
}

/// Returned in place of file bytes when the agent re-Reads a full, unchanged file. Saves tokens
/// without giving up the Read-before-Edit invariant — the gate already saw the original content.
pub(crate) const CACHE_HIT_STUB: &str =
    "File hasn't been modified since the last read. Returning already-read file.";

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::SystemTime;

    use super::{FileTracker, LastView};

    pub(crate) fn tracker() -> Arc<FileTracker> {
        Arc::new(FileTracker::default())
    }

    pub(crate) fn seed_full_read(tracker: &FileTracker, path: &Path) {
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

    pub(crate) fn tracker_seeded(path: &Path) -> FileTracker {
        let tracker = FileTracker::default();
        seed_full_read(&tracker, path);
        tracker
    }

    pub(crate) fn record_tracked_file(
        tracker: &FileTracker,
        path: &Path,
        bytes: &[u8],
    ) -> (SystemTime, u64) {
        std::fs::write(path, bytes).unwrap();
        let meta = std::fs::metadata(path).unwrap();
        let mtime = meta.modified().unwrap();
        let size = meta.len();
        tracker.record_modify(path, bytes, mtime, size);
        (mtime, size)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

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
        let outcome = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, LastView::Full);
        assert_eq!(outcome, RecordRead::Inserted);
        let stored = tracker.lock().get(path).cloned().unwrap();
        assert_eq!(stored.content_hash, xxh64(b"hello", HASH_SEED));
        assert_eq!(stored.size, 5);
        assert_eq!(stored.last_view, LastView::Full);
    }

    #[test]
    fn record_read_redundant_full_read_is_cache_hit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, LastView::Full);
        let outcome = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, LastView::Full);
        assert_eq!(
            outcome,
            RecordRead::CacheHit,
            "second full read of unchanged file is a cache hit",
        );
    }

    #[test]
    fn record_read_cache_hit_preserves_recorded_at_refreshes_mtime() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let t0 = UNIX_EPOCH + std::time::Duration::from_secs(1);
        let t1 = UNIX_EPOCH + std::time::Duration::from_secs(2);
        _ = tracker.record_read(path, b"hello", t0, 5, LastView::Full);
        let first = tracker.lock().get(path).cloned().unwrap();
        let outcome = tracker.record_read(path, b"hello", t1, 5, LastView::Full);
        let second = tracker.lock().get(path).cloned().unwrap();
        assert_eq!(outcome, RecordRead::CacheHit);
        assert_eq!(first.recorded_at, second.recorded_at);
        assert_eq!(second.mtime, t1);
    }

    #[test]
    fn record_read_changed_content_inserts_not_cache_hit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, LastView::Full);
        let outcome = tracker.record_read(path, b"world", UNIX_EPOCH, 5, LastView::Full);
        assert_eq!(outcome, RecordRead::Inserted);
    }

    #[test]
    fn record_read_partial_view_does_not_cache_hit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let view = LastView::Partial {
            offset: 1,
            limit: 5,
        };
        _ = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, view);
        let outcome = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, view);
        assert_eq!(outcome, RecordRead::Inserted);
    }

    #[test]
    fn record_read_full_after_partial_does_not_cache_hit() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(
            path,
            b"hello",
            UNIX_EPOCH,
            5,
            LastView::Partial {
                offset: 0,
                limit: 1,
            },
        );
        let outcome = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, LastView::Full);
        assert_eq!(outcome, RecordRead::Inserted);
        let next = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, LastView::Full);
        assert_eq!(next, RecordRead::CacheHit);
    }

    // ── verify_current_content ──

    #[test]
    fn verify_current_content_full_match_passes() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(path, b"hello", mtime, 5, LastView::Full);

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
    fn verify_current_content_partial_view_errors_partial_read() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(
            path,
            b"hello",
            UNIX_EPOCH,
            5,
            LastView::Partial {
                offset: 0,
                limit: 1,
            },
        );
        let result = tracker.verify_current_content(path, b"hello", GatePurpose::Edit);
        assert_eq!(
            result,
            Err(GateError::PartialRead {
                path: path.to_path_buf(),
                purpose: GatePurpose::Edit,
            }),
        );
    }

    #[test]
    fn verify_current_content_matching_bytes_ignore_stat_drift() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(path, b"hello", mtime, 5, LastView::Full);

        let result = tracker.verify_current_content(path, b"hello", GatePurpose::Edit);

        assert_eq!(
            result,
            Ok(()),
            "matching content stays safe even when metadata would drift",
        );
    }

    #[test]
    fn verify_current_content_divergent_bytes_rejects_content_drift() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(path, b"hello", mtime, 5, LastView::Full);

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
        assert!(msg.contains("needs a full Read"));
        assert!(msg.contains("editing"));
    }

    #[test]
    fn gate_error_never_read_renders_with_write_verb() {
        let err = GateError::NeverRead {
            path: PathBuf::from("/tmp/a.rs"),
            purpose: GatePurpose::Write,
        };
        let msg = err.to_string();
        assert!(msg.contains("needs a full Read"));
        assert!(msg.contains("writing to"));
    }

    #[test]
    fn gate_error_partial_read_renders_with_path_and_full_read_required() {
        let err = GateError::PartialRead {
            path: PathBuf::from("/tmp/a.rs"),
            purpose: GatePurpose::Edit,
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/a.rs"), "path is named: {msg}");
        assert!(msg.contains("offset / limit"));
        assert!(msg.contains("Read the full file"));
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

    // ── record_modify ──

    #[test]
    fn record_modify_updates_existing_entry_with_new_hash() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, LastView::Full);
        let new_mtime = UNIX_EPOCH + Duration::from_secs(1);
        tracker.record_modify(path, b"world", new_mtime, 5);

        let stored = tracker.lock().get(path).cloned().unwrap();
        assert_eq!(stored.content_hash, xxh64(b"world", HASH_SEED));
        assert_eq!(stored.mtime, new_mtime);
        assert_eq!(
            stored.last_view,
            LastView::Full,
            "modify always lands as Full"
        );
    }

    #[test]
    fn record_modify_promotes_partial_view_to_full() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(
            path,
            b"hello",
            UNIX_EPOCH,
            5,
            LastView::Partial {
                offset: 0,
                limit: 1,
            },
        );
        tracker.record_modify(path, b"world", UNIX_EPOCH, 5);
        assert_eq!(tracker.lock().get(path).unwrap().last_view, LastView::Full);
    }

    // ── record_modify_after_write ──

    #[tokio::test]
    async fn record_modify_after_write_records_disk_state() {
        let tracker = FileTracker::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"updated").unwrap();
        tracker.record_modify_after_write(&path, b"updated").await;

        let result = tracker.verify_current_content(&path, b"updated", GatePurpose::Edit);
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn record_modify_after_write_swallows_stat_failure_silently() {
        let tracker = FileTracker::default();
        let path = Path::new("/nonexistent/never-here.rs");
        tracker.record_modify_after_write(path, b"bytes").await;
        assert!(
            tracker.lock().get(path).is_none(),
            "stat failure must not insert anything",
        );
    }

    // ── snapshot_all ──

    #[test]
    fn snapshot_all_collects_every_tracked_file() {
        let tracker = FileTracker::default();
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(Path::new("/tmp/a"), b"a", mtime, 1, LastView::Full);
        _ = tracker.record_read(
            Path::new("/tmp/b"),
            b"bbb",
            mtime,
            3,
            LastView::Partial {
                offset: 0,
                limit: 2,
            },
        );

        let mut snaps = tracker.snapshot_all();
        snaps.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].path, Path::new("/tmp/a"));
        assert_eq!(snaps[0].size, 1);
        assert_eq!(snaps[0].last_view, LastView::Full);
        assert_eq!(snaps[1].path, Path::new("/tmp/b"));
        assert_eq!(snaps[1].size, 3);
        assert_eq!(
            snaps[1].last_view,
            LastView::Partial {
                offset: 0,
                limit: 2,
            },
        );
    }

    #[test]
    fn snapshot_all_empty_tracker_is_empty() {
        let snaps = FileTracker::default().snapshot_all();
        assert!(snaps.is_empty());
    }

    // ── clear ──

    #[test]
    fn clear_drops_recorded_reads_so_subsequent_snapshot_is_empty() {
        let tracker = FileTracker::default();
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(Path::new("/tmp/a"), b"a", mtime, 1, LastView::Full);
        _ = tracker.record_read(Path::new("/tmp/b"), b"b", mtime, 1, LastView::Full);
        assert_eq!(tracker.snapshot_all().len(), 2);

        tracker.clear();

        assert!(tracker.snapshot_all().is_empty());
    }

    // ── restore_verified ──

    #[test]
    fn restore_verified_matching_stat_repopulates_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"persisted bytes").unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let snap = FileSnapshot {
            path: path.clone(),
            content_hash: xxh64(b"persisted bytes", HASH_SEED),
            mtime: OffsetDateTime::from(meta.modified().unwrap()),
            size: meta.len(),
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
        };

        let tracker = FileTracker::default();
        tracker.restore_verified(vec![snap.clone()]);

        let stored = tracker.lock().get(&path).cloned();
        let stored = stored.expect("matching stat restores the entry");
        assert_eq!(stored.content_hash, snap.content_hash);
        assert_eq!(stored.size, snap.size);
        assert_eq!(stored.last_view, LastView::Full);
    }

    #[test]
    fn restore_verified_size_drift_drops_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"now this is longer").unwrap();

        let snap = FileSnapshot {
            path: path.clone(),
            content_hash: xxh64(b"old", HASH_SEED),
            mtime: OffsetDateTime::from(std::fs::metadata(&path).unwrap().modified().unwrap()),
            size: 3,
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
        };

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![snap]);
        assert_eq!(dropped, vec![path.clone()]);
        assert!(tracker.lock().get(&path).is_none());
    }

    #[test]
    fn restore_verified_same_size_content_drift_drops_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"new").unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let snap = FileSnapshot {
            path: path.clone(),
            content_hash: xxh64(b"old", HASH_SEED),
            mtime: OffsetDateTime::from(meta.modified().unwrap()),
            size: meta.len(),
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
        };

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![snap]);
        assert_eq!(
            dropped,
            vec![path.clone()],
            "same-size writes must still be detected by content hash",
        );
        assert!(tracker.lock().get(&path).is_none());
    }

    #[test]
    fn restore_verified_mtime_drift_keeps_matching_content_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"alpha").unwrap();
        let meta = std::fs::metadata(&path).unwrap();

        let snap = FileSnapshot {
            path: path.clone(),
            content_hash: xxh64(b"alpha", HASH_SEED),
            mtime: OffsetDateTime::from(meta.modified().unwrap()) - Duration::from_mins(1),
            size: meta.len(),
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
        };

        let tracker = FileTracker::default();
        tracker.restore_verified(vec![snap]);
        assert!(
            tracker.lock().get(&path).is_some(),
            "mtime mismatch alone must not drop a content-matching snapshot",
        );
    }

    #[test]
    fn restore_verified_missing_file_drops_snapshot() {
        let snap = FileSnapshot {
            path: PathBuf::from("/nonexistent/path/a.rs"),
            content_hash: 0,
            mtime: OffsetDateTime::UNIX_EPOCH,
            size: 0,
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
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
        let meta = std::fs::metadata(&path).unwrap();
        let snap = FileSnapshot {
            path: path.clone(),
            content_hash: xxh64(b"content", HASH_SEED),
            mtime: OffsetDateTime::from(meta.modified().unwrap()),
            size: meta.len(),
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
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
            mtime: OffsetDateTime::UNIX_EPOCH,
            size: 0,
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
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
            mtime: OffsetDateTime::UNIX_EPOCH,
            size: MAX_TRACKED_FILE_SIZE + 1,
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::now_utc(),
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
        let kept_meta = std::fs::metadata(&kept_path).unwrap();
        let drifted_meta = std::fs::metadata(&drifted_path).unwrap();
        let missing_path = PathBuf::from("/nonexistent/x.rs");

        let snaps = vec![
            FileSnapshot {
                path: kept_path.clone(),
                content_hash: xxh64(b"alpha", HASH_SEED),
                mtime: OffsetDateTime::from(kept_meta.modified().unwrap()),
                size: kept_meta.len(),
                last_view: LastView::Full,
                recorded_at: OffsetDateTime::now_utc(),
            },
            FileSnapshot {
                path: drifted_path.clone(),
                content_hash: 0,
                mtime: OffsetDateTime::from(drifted_meta.modified().unwrap()),
                size: 3,
                last_view: LastView::Full,
                recorded_at: OffsetDateTime::now_utc(),
            },
            FileSnapshot {
                path: missing_path.clone(),
                content_hash: 0,
                mtime: OffsetDateTime::UNIX_EPOCH,
                size: 0,
                last_view: LastView::Full,
                recorded_at: OffsetDateTime::now_utc(),
            },
        ];

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(snaps);
        assert_eq!(
            dropped,
            vec![missing_path, drifted_path],
            "both the size-drifted and the missing snapshots must be reported",
        );
        assert!(tracker.lock().contains_key(&kept_path));
    }

    #[test]
    fn restore_verified_verifies_only_newest_snapshot_per_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"x").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mtime_dt = OffsetDateTime::from(meta.modified().unwrap());
        let content_hash = xxh64(b"x", HASH_SEED);

        let older = FileSnapshot {
            path: path.clone(),
            content_hash: xxh64(b"stale", HASH_SEED),
            mtime: mtime_dt,
            size: meta.len(),
            last_view: LastView::Partial {
                offset: 1,
                limit: 1,
            },
            recorded_at: OffsetDateTime::UNIX_EPOCH,
        };
        let newer = FileSnapshot {
            path: path.clone(),
            content_hash,
            mtime: mtime_dt,
            size: meta.len(),
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::UNIX_EPOCH + Duration::from_mins(1),
        };

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![older.clone(), newer.clone()]);
        assert!(dropped.is_empty());
        let stored = tracker.lock().get(&path).cloned().unwrap();
        assert_eq!(stored.last_view, LastView::Full, "newer recorded_at wins");

        let tracker = FileTracker::default();
        let dropped = tracker.restore_verified(vec![newer, older]);
        assert!(dropped.is_empty());
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
                        _ = tracker.record_read(
                            &path,
                            &t.to_le_bytes(),
                            UNIX_EPOCH,
                            4,
                            LastView::Full,
                        );
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
        let result = tracker.record_read(
            Path::new("/tmp/poison_test"),
            b"hello",
            UNIX_EPOCH,
            5,
            LastView::Full,
        );
        assert_eq!(result, RecordRead::Inserted);
        assert_eq!(tracker.lock().len(), 1);
    }

    // ── FileSnapshot ──

    #[test]
    fn file_snapshot_round_trips_through_json() {
        let snap = FileSnapshot {
            path: PathBuf::from("/tmp/a.rs"),
            content_hash: 0xDEAD_BEEF,
            mtime: time::macros::datetime!(2026-04-29 12:00:00 UTC),
            size: 42,
            last_view: LastView::Partial {
                offset: 0,
                limit: 5,
            },
            recorded_at: time::macros::datetime!(2026-04-29 12:34:56 UTC),
        };
        let json = serde_json::to_value(&snap).unwrap();
        assert_eq!(json["path"], "/tmp/a.rs");
        assert_eq!(json["content_hash"], 0xDEAD_BEEF_u64);
        assert_eq!(json["mtime"], "2026-04-29T12:00:00Z");
        assert_eq!(json["size"], 42);
        assert_eq!(json["last_view"]["kind"], "partial");
        assert_eq!(json["last_view"]["offset"], 0);
        assert_eq!(json["last_view"]["limit"], 5);
        assert_eq!(json["recorded_at"], "2026-04-29T12:34:56Z");

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
        let view = LastView::Partial {
            offset: 0,
            limit: 5,
        };
        let json = serde_json::to_value(view).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "partial", "offset": 0, "limit": 5}),
        );
        let parsed: LastView = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, view);
    }
}
