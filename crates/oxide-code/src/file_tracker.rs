//! Per-session file-change tracker.
//!
//! Records what every Read / Write / Edit observed on disk so later
//! Edit / Write calls can refuse to clobber a file changed externally
//! between turns. Cloned via `Arc` into the file-mutating tools and
//! drained at session finish into
//! [`Entry::FileSnapshot`][crate::session::entry::Entry::FileSnapshot]
//! lines; resume rehydrates via [`FileTracker::restore_verified`].
//!
//! See `docs/research/design/file-tracking.md` for the contract: strict
//! Read-before-Edit gate, mtime + size fast path with xxh64 fallback,
//! persist-on-finish + verify-on-resume.
//!
//! Concurrency: `std::sync::Mutex<HashMap<...>>` — no I/O held under
//! the lock, same exception shape as
//! [`crate::session::handle::SharedState`].

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use xxhash_rust::xxh64::xxh64;

/// Seed for `xxh64` over file bytes. Zero matches the rest of the
/// crate (`sanitize_cwd`, billing fingerprints); private so callers
/// can't synthesize hashes that compare unequal to ours.
const HASH_SEED: u64 = 0;

// ── FileTracker ──

/// Shared map of file paths to their last-observed disk state.
#[derive(Default)]
pub(crate) struct FileTracker {
    by_path: Mutex<HashMap<PathBuf, FileState>>,
}

/// Per-file disk state captured at the most recent Read / Write / Edit
/// and compared against `stat()` on subsequent gate checks. `mtime` is
/// `SystemTime` to skip a conversion on every check; the persistence
/// boundary at [`FileSnapshot`] uses `OffsetDateTime` so the JSONL
/// stays RFC3339.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileState {
    /// xxh64 of the bytes the model last observed (Read) or produced
    /// (Edit / Write).
    content_hash: u64,
    /// `metadata.modified()` at capture; round-tripped through
    /// [`FileSnapshot::mtime`] only at finish / resume.
    mtime: SystemTime,
    /// `metadata.len()` at capture; drift in either field triggers
    /// the rehash.
    size: u64,
    /// Full reads gate Edit through; partial reads gate it shut.
    last_view: LastView,
    /// Wall-clock insert time. Persisted into
    /// [`FileSnapshot::recorded_at`] so resume can pick the newest
    /// survivor on duplicate paths.
    recorded_at: OffsetDateTime,
}

/// Full Read vs ranged Read. Edit and Write gate through `Full`;
/// `Partial` is recorded for cache-hit comparison but won't satisfy
/// the modification gate. Same shape on disk and in memory so resume
/// doesn't need a conversion layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LastView {
    Full,
    Partial { offset: usize, limit: usize },
}

/// Selects the gate verb (`editing` vs `writing to`). Staleness logic
/// is shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GatePurpose {
    Edit,
    Write,
}

impl FileTracker {
    /// Records a successful Read. Returns [`RecordRead::CacheHit`]
    /// when the call is a redundant full-file re-Read of an unchanged
    /// file so the caller can substitute [`CACHE_HIT_STUB`] and save
    /// tokens. Partial Reads never short-circuit — the model may want
    /// a different range.
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

        // Cache-hit refreshes mtime / size so a later phantom touch
        // doesn't trip the drift path, but preserves `recorded_at` so
        // the resume tiebreak still reflects the last state change.
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

    /// Stat-only gate run before Edit / Write of an existing file.
    /// `Pass` skips disk I/O on the common case; `NeedsBytes` hands
    /// the stored hash back so the caller can forward the file bytes
    /// to [`Self::verify_drift_bytes`] (one lock acquisition instead
    /// of two); `Err` covers the structural rejects (never read,
    /// partial read) and names the path.
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

    /// Resolves a [`StatCheck::NeedsBytes`] outcome by rehashing
    /// `bytes` against `stored_hash`. Matching hashes mean the
    /// mtime / size touch was content-preserving (e.g. cloud-sync
    /// timestamp shuffle) and the gate passes; mismatches surface
    /// the staleness error tagged with `path`. Associated function
    /// because the tracker has no state to consult here.
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

    /// Records the post-modify state of a file (Edit or Write success
    /// path). Always promotes the entry to `LastView::Full` because
    /// every Edit / Write writes a complete file body.
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

    /// Re-`stat()`s `path` after a successful Edit / Write and records
    /// the resulting state. Best-effort: a stat / `modified()` failure
    /// silently skips the record rather than reporting a successful
    /// write as failed; the next gate check rehashes if needed.
    pub(crate) async fn record_modify_after_write(&self, path: &Path, bytes: &[u8]) {
        if let Ok(meta) = tokio::fs::metadata(path).await
            && let Ok(mtime) = meta.modified()
        {
            self.record_modify(path, bytes, mtime, meta.len());
        }
    }

    /// Snapshot every tracked file for persistence. Drained at
    /// session finish and batched into the same flush as the
    /// `Summary`.
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

    /// Restores tracker state from session JSONL. Re-`stat()`s each
    /// snapshot: survivors (mtime + size match) reload; mismatches
    /// and missing files drop silently so the model re-Reads on first
    /// access. Stat-only rather than rehash because rehashing every
    /// recently-edited file would dominate startup on large repos —
    /// the worst case degrades to cold-start, which is correct.
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
            // Latest `recorded_at` wins on duplicate path — a live
            // entry written before resume could be newer than the
            // snapshot.
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
        self.by_path.lock().expect("FileTracker mutex poisoned")
    }
}

/// Persisted on-disk shape of one tracker entry. Wire-stable: carried
/// as [`Entry::FileSnapshot`][crate::session::entry::Entry::FileSnapshot]
/// in the session JSONL. `mtime` widens to `OffsetDateTime` for
/// RFC3339 alongside `Header::created_at` and friends; the
/// `OffsetDateTime ↔ SystemTime` round-trip is infallible.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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

/// Outcome of [`FileTracker::record_read`]. Named enum (not `bool`)
/// so call sites read `RecordRead::CacheHit` directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordRead {
    Inserted,
    CacheHit,
}

/// Stat-only outcome of [`FileTracker::check_stat`]. `NeedsBytes`
/// requires the caller to forward bytes to
/// [`FileTracker::verify_drift_bytes`] before proceeding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatCheck {
    Pass,
    NeedsBytes { stored_hash: u64 },
}

/// Structural rejection from the gate. Each variant carries the
/// offending `path` so the rendered `Display` names the file (matching
/// the codebase-wide `Error reading {path}: ...` convention) and the
/// `GatePurpose` for verb selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GateError {
    /// Gate hit before any Read in this session recorded the path.
    NeverRead { path: PathBuf, purpose: GatePurpose },
    /// Prior Read was ranged — the model never saw the full file.
    PartialRead { path: PathBuf, purpose: GatePurpose },
    /// Post-stat rehash confirmed the bytes diverged from the last
    /// observation.
    ContentDrifted { path: PathBuf, purpose: GatePurpose },
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verb = match self.purpose() {
            GatePurpose::Edit => "editing",
            GatePurpose::Write => "writing to",
        };
        let path = self.path().display();
        match self {
            Self::NeverRead { .. } => write!(
                f,
                "File {path} has not been read in this session. Use the Read tool first before {verb} it.",
            ),
            Self::PartialRead { .. } => write!(
                f,
                "File {path} has only been read partially (with offset / limit). Read the full file before {verb} it.",
            ),
            Self::ContentDrifted { .. } => write!(
                f,
                "File {path} has been modified externally since it was last read. Re-read it before {verb} it.",
            ),
        }
    }
}

impl GateError {
    fn path(&self) -> &Path {
        match self {
            Self::NeverRead { path, .. }
            | Self::PartialRead { path, .. }
            | Self::ContentDrifted { path, .. } => path,
        }
    }

    fn purpose(&self) -> GatePurpose {
        match self {
            Self::NeverRead { purpose, .. }
            | Self::PartialRead { purpose, .. }
            | Self::ContentDrifted { purpose, .. } => *purpose,
        }
    }
}

/// Stub returned in place of file bytes on a full re-Read of an
/// unchanged file. Signals that the prior Read is still authoritative.
pub(crate) const CACHE_HIT_STUB: &str =
    "File hasn't been modified since the last read. Returning already-read file.";

/// Shared test fixtures. Centralized so the five callers don't each
/// grow near-duplicates that subtly disagree on what a "seeded
/// tracker" means.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::SystemTime;

    use super::{FileTracker, LastView};

    /// Fresh `Arc<FileTracker>` for callers that need ownership
    /// (`ReadTool::new`, `EditTool::new`, `WriteTool::new`); plain
    /// callers use `FileTracker::default()`.
    pub(crate) fn tracker() -> Arc<FileTracker> {
        Arc::new(FileTracker::default())
    }

    /// Seeds `tracker` with a full Read of `path` from disk, mirroring
    /// a real Read turn.
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

    /// Fresh tracker pre-seeded with a full Read of `path` — the
    /// shape most edit-tool tests want.
    pub(crate) fn tracker_seeded(path: &Path) -> FileTracker {
        let tracker = FileTracker::default();
        seed_full_read(&tracker, path);
        tracker
    }

    /// Writes `bytes` to `path`, records the resulting state as a
    /// successful Edit / Write, and returns the captured
    /// `(mtime, size)` for assertions.
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
    fn record_read_redundant_full_read_returns_cache_hit() {
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
        // Cache-hit is an observation, not a state change: pin
        // `recorded_at` so the resume tiebreak keeps the real
        // last-modify, but refresh `mtime` so a later phantom touch
        // doesn't slip into the drift arm.
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
        // Same path / view, different content — pins that the
        // cache-hit decision compares hashes, not mtime / size.
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, LastView::Full);
        let outcome = tracker.record_read(path, b"world", UNIX_EPOCH, 5, LastView::Full);
        assert_eq!(outcome, RecordRead::Inserted);
    }

    #[test]
    fn record_read_partial_view_does_not_cache_hit() {
        // Partial Read never short-circuits — the model is asking
        // for a specific slice and may not have seen the rest.
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
        // Partial → Full is the model's first full view, never a
        // redundant re-Read even when the bytes match.
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
        // Now Full, so the next re-Read with matching bytes
        // cache-hits.
        let next = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, LastView::Full);
        assert_eq!(next, RecordRead::CacheHit);
    }

    // ── check_stat ──

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
    fn check_stat_full_match_passes() {
        let tracker = FileTracker::default();
        let path = Path::new("/tmp/a.rs");
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(path, b"hello", mtime, 5, LastView::Full);
        let check = tracker.check_stat(path, mtime, 5, GatePurpose::Edit);
        assert_eq!(check, Ok(StatCheck::Pass));
    }

    #[test]
    fn check_stat_mtime_drift_returns_stored_hash() {
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
    fn check_stat_size_drift_returns_stored_hash() {
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
        // Stat moved but rehash matches — content-preserving touch
        // (cloud-sync timestamp shuffle).
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
        // After an edit the pre-edit hash must not survive — gate
        // checks compare against the new bytes.
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
        // Edit / Write rewrites the whole file regardless of the
        // original view, so the new state is always Full.
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
        // Post-write helper re-stats and records the new hash /
        // mtime / size, so the next gate check `Pass`es without a
        // rehash.
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
        // Best-effort: a stat failure must not panic and must not
        // insert anything, so a successful write isn't reported as
        // failed.
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
    fn snapshot_all_empty_tracker_returns_empty_vec() {
        let snaps = FileTracker::default().snapshot_all();
        assert!(snaps.is_empty());
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
        // Size mismatch drops silently; the next Edit hits the
        // standard "must read first" gate via a cold tracker.
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
        // Size matches but mtime moved (between-session edit). Drop
        // so the next Edit re-Reads instead of trusting a stale hash.
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
        // Same path appearing twice (file written across separate
        // sessions): later `recorded_at` wins when both still match
        // disk.
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

        // Older first exercises "incoming wins"; newer first
        // covers the symmetric guard.
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
        // Eight threads, 100 unique paths each — catches any future
        // migration to a non-thread-safe representation.
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

    // ── FileSnapshot ──

    #[test]
    fn file_snapshot_round_trips_through_json() {
        // Wire-stable shape — pin the field layout (path-as-string,
        // RFC3339 timestamps, `kind` tag) so a change here surfaces
        // here, not at resume time on existing JSONL.
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
        // Pin `{"kind":"full"}` so a future inline-table change
        // doesn't silently break readers expecting just the tag.
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
