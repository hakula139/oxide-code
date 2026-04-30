//! Per-session file-change tracker.
//!
//! Records what every Read / Write / Edit observed on disk so later
//! Edit / Write calls can refuse to clobber a file the user changed
//! externally between turns. The tracker is shared by the tool dispatch
//! path (one `Arc` clone per tool that mutates files) and by the
//! session actor (drains it on finish to persist
//! [`Entry::FileSnapshot`][crate::session::entry::Entry::FileSnapshot]
//! lines). On resume the loader hands the recovered snapshots back via
//! [`FileTracker::restore_verified`].
//!
//! See `docs/research/design/file-tracking.md` for the contract this
//! module implements: strict Read-before-Edit gate, mtime + size fast
//! path with xxh64 fallback, persist-on-finish + verify-on-resume.
//!
//! Concurrency: `std::sync::Mutex<HashMap<...>>`. Lock acquisitions are
//! microseconds (small struct, no I/O held under lock), and there is no
//! cross-task workflow to coordinate — same exception shape as
//! [`crate::session::handle::SharedState`].

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use xxhash_rust::xxh64::xxh64;

/// Seed for `xxh64` over file bytes. Zero is the convention used
/// elsewhere in the crate (`session::path::sanitize_cwd`, billing
/// fingerprints); private so callers can't compute hashes that would
/// compare unequal to the ones produced here.
const HASH_SEED: u64 = 0;

// ── FileTracker ──

/// Shared map of file paths to their last-observed disk state. Cloned
/// via `Arc` into every tool that mutates files (Read / Edit / Write)
/// and into [`SessionState`][crate::session::state::SessionState] so
/// `finish_entry` can drain it for persistence.
#[derive(Default)]
pub(crate) struct FileTracker {
    by_path: Mutex<HashMap<PathBuf, FileState>>,
}

/// Per-file disk state captured at the most recent Read / Write / Edit.
/// Compared against `stat()` on subsequent gate checks.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileState {
    /// xxh64 of the file bytes the model last observed (Read) or
    /// produced (Edit / Write).
    content_hash: u64,
    /// `metadata.modified()` at capture. Compared against the current
    /// mtime to short-circuit the rehash path.
    mtime: SystemTime,
    /// `metadata.len()` at capture. Same role as `mtime` but cheaper
    /// to flip — drift in either field triggers the rehash.
    size: u64,
    /// Full reads gate Edit through; partial reads gate it shut.
    last_view: LastView,
    /// Wall-clock time the entry was inserted. Persisted in
    /// [`FileSnapshot::recorded_at`] so resume can pick the newest
    /// survivor when the same path appears more than once.
    recorded_at: OffsetDateTime,
}

/// Distinguishes a full file Read from a ranged Read. Edit and Write
/// gate through `Full`; `Partial` is recorded so re-Reads at the same
/// range hit the cache, but won't satisfy the modification gate.
///
/// One enum used both in-memory and on-disk so resume doesn't need a
/// conversion layer. The `kind` discriminator keeps the JSONL shape
/// readable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LastView {
    Full,
    Partial { offset: usize, limit: usize },
}

/// Selects the user-facing error wording emitted by the gate. Both
/// flows share the same staleness logic; only the verb differs
/// (`editing` vs `writing to`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GatePurpose {
    Edit,
    Write,
}

impl FileTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records a successful Read, returning a cache-hit marker when
    /// the call was a redundant full-file re-Read of an unchanged
    /// file. Caller substitutes [`CACHE_HIT_STUB`] for the
    /// line-numbered excerpt to save tokens (matches claude-code's
    /// `"File hasn't been modified..."` shape).
    ///
    /// Stubs only fire for full Reads against a prior full Read with
    /// matching hash — partial Reads never short-circuit because the
    /// model may be asking for a different range.
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

        // Full re-read of an unchanged file: refresh mtime / size so
        // a phantom cloud-sync touch doesn't later trip the drift
        // path, but preserve `recorded_at` so the resume tiebreak
        // keeps reflecting the last state change, not the last
        // observation.
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

    /// Strict stat-only gate run before Edit / Write of an existing
    /// file. `Pass` on stat-match — the common case skips disk I/O.
    /// `NeedsBytes` hands the stored hash back so the caller (which
    /// will read the file anyway, or read it only on this branch)
    /// forwards the bytes to [`Self::verify_drift_bytes`]. `Err`
    /// covers the structural rejects (never read, partial read).
    ///
    /// Returning the stored hash from this method rather than
    /// requiring a follow-up lookup keeps lock acquisitions minimal —
    /// one for the precheck, one for the post-edit record.
    pub(crate) fn check_stat(
        &self,
        path: &Path,
        current_mtime: SystemTime,
        current_size: u64,
        purpose: GatePurpose,
    ) -> Result<StatCheck, GateError> {
        let by_path = self.lock();
        let Some(entry) = by_path.get(path) else {
            return Err(GateError::NeverRead { purpose });
        };
        if !matches!(entry.last_view, LastView::Full) {
            return Err(GateError::PartialRead { purpose });
        }
        if entry.mtime == current_mtime && entry.size == current_size {
            Ok(StatCheck::Pass)
        } else {
            Ok(StatCheck::NeedsBytes {
                stored_hash: entry.content_hash,
            })
        }
    }

    /// Resolves a [`StatCheck::NeedsBytes`] outcome: hashes `bytes`
    /// under the private seed and compares against `stored_hash`.
    /// Matching hashes mean the mtime / size touch was a no-op (e.g.
    /// Windows cloud-sync timestamp shuffle) and the gate passes;
    /// mismatches surface the staleness error.
    ///
    /// Associated function (no `&self`) because the tracker has no
    /// state to consult here — it's a pure pairing of the private
    /// seed with one comparison.
    pub(crate) fn verify_drift_bytes(
        bytes: &[u8],
        stored_hash: u64,
        purpose: GatePurpose,
    ) -> Result<(), GateError> {
        if xxh64(bytes, HASH_SEED) == stored_hash {
            Ok(())
        } else {
            Err(GateError::ContentDrifted { purpose })
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

    /// Snapshot every tracked file for persistence. Used by
    /// [`SessionState::finish_entries`][crate::session::state::SessionState]
    /// at session end — the actor batches the resulting entries into
    /// the same flush as the `Summary`.
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

    /// Restores tracker state from session JSONL. For each snapshot,
    /// re-`stat()`s the file: survivors (mtime + size match) reload
    /// into the in-memory map; mismatches and missing files drop
    /// silently so the model re-Reads on first access.
    ///
    /// Re-hashing every recently-edited file at session start would
    /// dominate startup on large repos, so we compare stat-only. The
    /// "false-negative drops the entry" worst case degrades to cold-
    /// start behavior, which is correct.
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
            // Latest-recorded wins on duplicate path: an existing
            // entry could be a later observation if this process
            // populated one before resume, so we keep the newer.
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

/// Persisted on-disk shape of one tracker entry. Wire-stable: the
/// session JSONL carries this as
/// [`Entry::FileSnapshot`][crate::session::entry::Entry::FileSnapshot].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FileSnapshot {
    pub(crate) path: PathBuf,
    pub(crate) content_hash: u64,
    /// Round-tripped as RFC3339; matches every other timestamp in the
    /// JSONL (`Header::created_at`, `Title::updated_at`, ...).
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) mtime: OffsetDateTime,
    pub(crate) size: u64,
    pub(crate) last_view: LastView,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) recorded_at: OffsetDateTime,
}

/// Outcome of [`FileTracker::record_read`]. Distinct enum (not `bool`)
/// so the call site reads `RecordRead::CacheHit` rather than guessing
/// what `true` means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordRead {
    Inserted,
    CacheHit,
}

/// Stat-only outcome of [`FileTracker::check_stat`]. `Pass` means the
/// caller may proceed; `NeedsBytes` means the caller must hand the
/// file bytes to [`FileTracker::verify_drift_bytes`] before
/// proceeding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatCheck {
    Pass,
    NeedsBytes { stored_hash: u64 },
}

/// Structural rejection from the gate. Rendered via `Display` into
/// the model-facing tool error; variants carry the `GatePurpose` so
/// the render swaps verb only (`editing` vs `writing to`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GateError {
    /// Caller hit the gate before a Read in this session recorded
    /// anything for the path.
    NeverRead { purpose: GatePurpose },
    /// Prior Read was ranged (offset / limit); the model never saw
    /// the full file, so a modification can't be trusted.
    PartialRead { purpose: GatePurpose },
    /// Post-stat rehash confirmed the file bytes diverged from what
    /// the model last observed.
    ContentDrifted { purpose: GatePurpose },
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verb = match self.purpose() {
            GatePurpose::Edit => "editing",
            GatePurpose::Write => "writing to",
        };
        match self {
            Self::NeverRead { .. } => write!(
                f,
                "File has not been read in this session. Use the Read tool first before {verb} it.",
            ),
            Self::PartialRead { .. } => write!(
                f,
                "File has only been read partially (with offset / limit). Read the full file before {verb} it.",
            ),
            Self::ContentDrifted { .. } => write!(
                f,
                "File has been modified externally since it was last read. Re-read it before {verb} it.",
            ),
        }
    }
}

impl GateError {
    fn purpose(&self) -> GatePurpose {
        match self {
            Self::NeverRead { purpose }
            | Self::PartialRead { purpose }
            | Self::ContentDrifted { purpose } => *purpose,
        }
    }
}

/// Stub returned to the model when a full Read finds the file
/// unchanged since the last full Read in this session. Saves emitting
/// the bytes again and signals that the prior Read is still authoritative.
pub(crate) const CACHE_HIT_STUB: &str =
    "File hasn't been modified since the last read. Returning already-read file.";

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    // ── record_read ──

    #[test]
    fn record_read_first_full_read_inserts() {
        let tracker = FileTracker::new();
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
        let tracker = FileTracker::new();
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
        // Cache-hit is an observation, not a state change: `recorded_at`
        // must stay pinned so the resume tiebreak keeps picking the
        // real last-modify. But `mtime` must refresh so a phantom
        // cloud-sync touch with identical bytes doesn't later fall
        // into the `pre_modify_check` drift arm.
        let tracker = FileTracker::new();
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
        // Same path, same view, different content (different hash) — must
        // not be a cache hit. The mtime / size inputs are irrelevant to
        // the cache-hit decision; this is the regression for "did we
        // actually compare hashes?"
        let tracker = FileTracker::new();
        let path = Path::new("/tmp/a.rs");
        _ = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, LastView::Full);
        let outcome = tracker.record_read(path, b"world", UNIX_EPOCH, 5, LastView::Full);
        assert_eq!(outcome, RecordRead::Inserted);
    }

    #[test]
    fn record_read_partial_view_does_not_cache_hit() {
        // A partial Read can never short-circuit even if the bytes
        // match — the model is asking for a specific slice and may not
        // have seen the rest of the file.
        let tracker = FileTracker::new();
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
        // Prior was Partial, current is Full — even with matching
        // bytes, this is the model's first full view, so it's not a
        // redundant re-Read.
        let tracker = FileTracker::new();
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
        // Latest insert wins → entry is now Full, future re-Read with
        // matching bytes will cache-hit.
        let next = tracker.record_read(path, b"hello", UNIX_EPOCH, 5, LastView::Full);
        assert_eq!(next, RecordRead::CacheHit);
    }

    // ── check_stat ──

    #[test]
    fn check_stat_no_entry_errors_never_read() {
        let tracker = FileTracker::new();
        let path = Path::new("/tmp/a.rs");
        let result = tracker.check_stat(path, UNIX_EPOCH, 0, GatePurpose::Edit);
        assert_eq!(
            result,
            Err(GateError::NeverRead {
                purpose: GatePurpose::Edit,
            }),
        );
    }

    #[test]
    fn check_stat_no_entry_carries_write_purpose_for_write_gate() {
        let tracker = FileTracker::new();
        let result = tracker.check_stat(Path::new("/tmp/a.rs"), UNIX_EPOCH, 0, GatePurpose::Write);
        assert_eq!(
            result,
            Err(GateError::NeverRead {
                purpose: GatePurpose::Write,
            }),
        );
    }

    #[test]
    fn check_stat_partial_view_errors_partial_read() {
        let tracker = FileTracker::new();
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
                purpose: GatePurpose::Edit,
            }),
        );
    }

    #[test]
    fn check_stat_full_match_passes() {
        let tracker = FileTracker::new();
        let path = Path::new("/tmp/a.rs");
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(path, b"hello", mtime, 5, LastView::Full);
        let check = tracker.check_stat(path, mtime, 5, GatePurpose::Edit);
        assert_eq!(check, Ok(StatCheck::Pass));
    }

    #[test]
    fn check_stat_mtime_drift_returns_stored_hash() {
        let tracker = FileTracker::new();
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
        let tracker = FileTracker::new();
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
        // Stat said the file changed, but rehash matches — the mtime /
        // size touch was content-preserving (cloud-sync workaround).
        let stored = xxh64(b"hello", HASH_SEED);
        let result = FileTracker::verify_drift_bytes(b"hello", stored, GatePurpose::Edit);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn verify_drift_bytes_divergent_rejects_content_drifted() {
        let stored = xxh64(b"old", HASH_SEED);
        let result = FileTracker::verify_drift_bytes(b"new", stored, GatePurpose::Edit);
        assert_eq!(
            result,
            Err(GateError::ContentDrifted {
                purpose: GatePurpose::Edit,
            }),
        );
    }

    // ── GateError Display ──

    #[test]
    fn gate_error_never_read_renders_with_edit_verb() {
        let err = GateError::NeverRead {
            purpose: GatePurpose::Edit,
        };
        let msg = err.to_string();
        assert!(msg.contains("not been read"));
        assert!(msg.contains("editing"));
    }

    #[test]
    fn gate_error_never_read_renders_with_write_verb() {
        let err = GateError::NeverRead {
            purpose: GatePurpose::Write,
        };
        let msg = err.to_string();
        assert!(msg.contains("not been read"));
        assert!(msg.contains("writing to"));
    }

    #[test]
    fn gate_error_partial_read_renders_full_read_required() {
        let err = GateError::PartialRead {
            purpose: GatePurpose::Edit,
        };
        let msg = err.to_string();
        assert!(msg.contains("partially"));
        assert!(msg.contains("Read the full file"));
    }

    #[test]
    fn gate_error_content_drifted_renders_modified_externally() {
        let err = GateError::ContentDrifted {
            purpose: GatePurpose::Edit,
        };
        let msg = err.to_string();
        assert!(msg.contains("modified externally"));
        assert!(msg.contains("Re-read"));
    }

    // ── record_modify ──

    #[test]
    fn record_modify_updates_existing_entry_with_new_hash() {
        // After an edit, future gate checks compare against the new
        // bytes; the pre-edit hash must not survive.
        let tracker = FileTracker::new();
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
        // Edit reads / rewrites the whole file regardless of how it
        // was originally read — the new state is always a full view.
        let tracker = FileTracker::new();
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

    // ── snapshot_all ──

    #[test]
    fn snapshot_all_collects_every_tracked_file() {
        let tracker = FileTracker::new();
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
        assert!(matches!(
            snaps[1].last_view,
            LastView::Partial {
                offset: 0,
                limit: 2,
            },
        ));
    }

    #[test]
    fn snapshot_all_empty_tracker_returns_empty_vec() {
        let snaps = FileTracker::new().snapshot_all();
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

        let tracker = FileTracker::new();
        tracker.restore_verified(vec![snap.clone()]);

        let stored = tracker.lock().get(&path).cloned();
        let stored = stored.expect("matching stat restores the entry");
        assert_eq!(stored.content_hash, snap.content_hash);
        assert_eq!(stored.size, snap.size);
        assert_eq!(stored.last_view, LastView::Full);
    }

    #[test]
    fn restore_verified_size_drift_drops_snapshot() {
        // File on disk has different size than the snapshot: drop
        // silently. Subsequent Edit through a cold tracker fires the
        // standard "must read first" gate — that's the correct
        // degradation, not a panic.
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

        let tracker = FileTracker::new();
        tracker.restore_verified(vec![snap]);
        assert!(tracker.lock().get(&path).is_none());
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

        let tracker = FileTracker::new();
        tracker.restore_verified(vec![snap]);
        assert!(tracker.lock().is_empty());
    }

    #[test]
    fn restore_verified_keeps_newer_recorded_at_on_duplicate_path() {
        // Two snapshots for the same file (e.g., the file was written
        // twice across separate sessions and both shipped FileSnapshot
        // entries). Verify the later `recorded_at` wins so the model
        // sees the most recent observation when both still match disk.
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

        // Order older-then-newer to exercise the "incoming wins" branch;
        // newer-then-older is covered by the symmetric guard.
        let tracker = FileTracker::new();
        tracker.restore_verified(vec![older.clone(), newer.clone()]);
        let stored = tracker.lock().get(&path).cloned().unwrap();
        assert_eq!(stored.content_hash, 2, "newer recorded_at wins");

        let tracker = FileTracker::new();
        tracker.restore_verified(vec![newer, older]);
        let stored = tracker.lock().get(&path).cloned().unwrap();
        assert_eq!(stored.content_hash, 2, "older does not displace newer");
    }

    // ── Concurrency ──

    #[test]
    fn concurrent_record_read_does_not_corrupt_map() {
        // Eight threads each insert 100 unique paths. The mutex
        // serializes inserts; the test catches any future migration
        // to a non-thread-safe representation.
        let tracker = Arc::new(FileTracker::new());
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
        // Wire-stable shape — change here means an existing JSONL
        // becomes unreadable. Pin the discriminator and the field
        // shape (path-as-string, RFC3339 timestamps, kind tag).
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
        // The Full variant has no payload fields, so its JSON should
        // be exactly `{"kind":"full"}` — pin so a future inline-table
        // change doesn't silently break readers expecting just the tag.
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
