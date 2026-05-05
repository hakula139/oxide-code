//! Per-session Read-before-Edit gate with mtime + xxh64 staleness detection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use xxhash_rust::xxh64::xxh64;

const HASH_SEED: u64 = 0;

// ── FileTracker ──

/// Per-session record of files the agent has read, gating subsequent Edit / Write tool calls.
///
/// The gate enforces three invariants before mutation:
///
/// - **Read-before-Edit**: every Edit / Write must follow a `Read` of the same path within the
///   session.
/// - **Full read required**: a partial (offset / limit) Read does not satisfy the gate; the agent
///   must Read the file in full before mutating it.
/// - **Freshness**: if (mtime, size) drifted since the recorded Read, the caller must rehash the
///   current bytes via [`Self::verify_drift_bytes`]; only matching xxh64 content qualifies as an
///   unchanged file.
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

    /// Fast-path gate: `Pass` if stat matches, `NeedsBytes` if rehash required.
    pub(crate) fn check_stat(
        &self,
        path: &Path,
        current_mtime: SystemTime,
        current_size: u64,
        purpose: GatePurpose,
    ) -> Result<StatCheck, GateError> {
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
        if entry.mtime == current_mtime && entry.size == current_size {
            Ok(StatCheck::Pass)
        } else {
            Ok(StatCheck::NeedsBytes {
                stored_hash: entry.content_hash,
            })
        }
    }

    /// Rehashes `bytes` against `stored_hash`; passes if content unchanged despite stat drift.
    pub(crate) fn verify_drift_bytes(
        path: &Path,
        bytes: &[u8],
        stored_hash: u64,
        purpose: GatePurpose,
    ) -> Result<(), GateError> {
        if xxh64(bytes, HASH_SEED) == stored_hash {
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

    /// Best-effort post-write stat + record; skips silently on stat failure.
    pub(crate) async fn record_modify_after_write(&self, path: &Path, bytes: &[u8]) {
        if let Ok(meta) = tokio::fs::metadata(path).await
            && let Ok(mtime) = meta.modified()
        {
            self.record_modify(path, bytes, mtime, meta.len());
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

    /// Rehydrates from session JSONL on resume. Each snapshot must still match disk on (mtime,
    /// size) — drifted or missing files are dropped, forcing a fresh Read before any mutation. On
    /// duplicate paths the entry with the newer `recorded_at` wins.
    pub(crate) fn restore_verified(&self, snapshots: Vec<FileSnapshot>) {
        let mut by_path = self.lock();
        for snap in snapshots {
            let Ok(meta) = std::fs::metadata(&snap.path) else {
                continue;
            };
            let Ok(current_mtime) = meta.modified() else {
                continue;
            };
            let stored_mtime = SystemTime::from(snap.mtime);
            if meta.len() != snap.size || current_mtime != stored_mtime {
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
                        size: snap.size,
                        last_view: snap.last_view,
                        recorded_at: snap.recorded_at,
                    },
                );
            }
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordRead {
    Inserted,
    CacheHit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatCheck {
    Pass,
    NeedsBytes { stored_hash: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum GateError {
    #[error("File {} has not been read in this session. Use the Read tool first before {} it.", .path.display(), .purpose.verb())]
    NeverRead { path: PathBuf, purpose: GatePurpose },
    #[error("File {} has only been read partially (with offset / limit). Read the full file before {} it.", .path.display(), .purpose.verb())]
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
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

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

    // ── check_stat ──

    #[test]
    fn check_stat_full_match_passes() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(path, b"hello", mtime, 5, LastView::Full);
        let check = tracker.check_stat(path, mtime, 5, GatePurpose::Edit);
        assert_eq!(check, Ok(StatCheck::Pass));
    }

    #[test]
    fn check_stat_no_entry_errors_never_read() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let result = tracker.check_stat(path, UNIX_EPOCH, 0, GatePurpose::Edit);
        assert_eq!(
            result,
            Err(GateError::NeverRead {
                path: path.to_path_buf(),
                purpose: GatePurpose::Edit,
            }),
        );
    }

    #[test]
    fn check_stat_no_entry_carries_write_purpose_for_write_gate() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let result = tracker.check_stat(path, UNIX_EPOCH, 0, GatePurpose::Write);
        assert_eq!(
            result,
            Err(GateError::NeverRead {
                path: path.to_path_buf(),
                purpose: GatePurpose::Write,
            }),
        );
    }

    #[test]
    fn check_stat_partial_view_errors_partial_read() {
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
        let result = tracker.check_stat(path, UNIX_EPOCH, 5, GatePurpose::Edit);
        assert_eq!(
            result,
            Err(GateError::PartialRead {
                path: path.to_path_buf(),
                purpose: GatePurpose::Edit,
            }),
        );
    }

    #[test]
    fn check_stat_mtime_drift_produces_stored_hash() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(path, b"hello", mtime, 5, LastView::Full);

        let drifted_mtime = mtime + Duration::from_secs(1);
        let check = tracker.check_stat(path, drifted_mtime, 5, GatePurpose::Edit);

        assert_eq!(
            check,
            Ok(StatCheck::NeedsBytes {
                stored_hash: xxh64(b"hello", HASH_SEED),
            }),
            "mtime drift surfaces the stored hash for the caller to confirm",
        );
    }

    #[test]
    fn check_stat_size_drift_produces_stored_hash() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(path, b"hello", mtime, 5, LastView::Full);

        let check = tracker.check_stat(path, mtime, 999, GatePurpose::Edit);

        assert_eq!(
            check,
            Ok(StatCheck::NeedsBytes {
                stored_hash: xxh64(b"hello", HASH_SEED),
            }),
            "size drift surfaces the stored hash even when mtime matched",
        );
    }

    // ── verify_drift_bytes ──

    #[test]
    fn verify_drift_bytes_phantom_drift_passes() {
        let stored = xxh64(b"hello", HASH_SEED);
        let result = FileTracker::verify_drift_bytes(
            Path::new("/tmp/a.rs"),
            b"hello",
            stored,
            GatePurpose::Edit,
        );
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn verify_drift_bytes_divergent_rejects_content_drifted() {
        let path = Path::new("/tmp/a.rs");
        let stored = xxh64(b"old", HASH_SEED);
        let result = FileTracker::verify_drift_bytes(path, b"new", stored, GatePurpose::Edit);
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
        assert!(msg.contains("not been read"));
        assert!(msg.contains("editing"));
    }

    #[test]
    fn gate_error_never_read_renders_with_write_verb() {
        let err = GateError::NeverRead {
            path: PathBuf::from("/tmp/a.rs"),
            purpose: GatePurpose::Write,
        };
        let msg = err.to_string();
        assert!(msg.contains("not been read"));
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
        assert!(msg.contains("partially"));
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

        let meta = std::fs::metadata(&path).unwrap();
        let check = tracker.check_stat(
            &path,
            meta.modified().unwrap(),
            meta.len(),
            GatePurpose::Edit,
        );
        assert_eq!(check, Ok(StatCheck::Pass));
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
        tracker.restore_verified(vec![snap]);
        assert!(tracker.lock().get(&path).is_none());
    }

    #[test]
    fn restore_verified_mtime_drift_drops_snapshot() {
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
            tracker.lock().get(&path).is_none(),
            "mtime mismatch must drop the snapshot even when size matches",
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

    #[test]
    fn restore_verified_keeps_newer_recorded_at_on_duplicate_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"x").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mtime_dt = OffsetDateTime::from(meta.modified().unwrap());

        let older = FileSnapshot {
            path: path.clone(),
            content_hash: 1,
            mtime: mtime_dt,
            size: meta.len(),
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::UNIX_EPOCH,
        };
        let newer = FileSnapshot {
            path: path.clone(),
            content_hash: 2,
            mtime: mtime_dt,
            size: meta.len(),
            last_view: LastView::Full,
            recorded_at: OffsetDateTime::UNIX_EPOCH + Duration::from_mins(1),
        };

        let tracker = FileTracker::default();
        tracker.restore_verified(vec![older.clone(), newer.clone()]);
        let stored = tracker.lock().get(&path).cloned().unwrap();
        assert_eq!(stored.content_hash, 2, "newer recorded_at wins");

        let tracker = FileTracker::default();
        tracker.restore_verified(vec![newer, older]);
        let stored = tracker.lock().get(&path).cloned().unwrap();
        assert_eq!(stored.content_hash, 2, "older does not displace newer");
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
