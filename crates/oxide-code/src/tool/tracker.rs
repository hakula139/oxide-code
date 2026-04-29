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
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh64::xxh64;

/// Seed for `xxh64` over file bytes. Zero is the convention used
/// elsewhere in the crate (`session::path::sanitize_cwd`, billing
/// fingerprints) so the choice is consistent. Re-exported `pub(crate)`
/// so [`crate::tool::edit`] / [`crate::tool::write`] hash bytes against
/// the same seed as [`record_read`][FileTracker::record_read] —
/// otherwise [`confirm_drift_unchanged`][FileTracker::confirm_drift_unchanged]
/// would compare hashes computed under different seeds and never match.
pub(crate) const HASH_SEED: u64 = 0;

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
#[derive(Clone, Copy, Debug)]
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
        let path_buf = path.to_path_buf();
        let mut by_path = self.lock();
        let cache_hit = matches!(view, LastView::Full)
            && by_path
                .get(&path_buf)
                .is_some_and(|s| s.last_view == LastView::Full && s.content_hash == content_hash);
        by_path.insert(
            path_buf,
            FileState {
                content_hash,
                mtime,
                size,
                last_view: view,
            },
        );
        if cache_hit {
            RecordRead::CacheHit
        } else {
            RecordRead::Inserted
        }
    }

    /// Strict gate run before Edit / Write of an existing file.
    ///
    /// `Pass` on stat-match — the common case skips disk I/O. `Drift`
    /// hands the stored hash back so the caller (which is about to
    /// read the file anyway) computes the current hash via xxh64 and
    /// resolves with [`Self::confirm_drift_unchanged`]. `Reject`
    /// carries the user-facing error.
    ///
    /// Returning the stored hash from this method (rather than
    /// requiring a follow-up lookup) keeps lock acquisitions strictly
    /// minimal — one for the precheck, one for the post-edit record.
    pub(crate) fn pre_modify_check(
        &self,
        path: &Path,
        current_mtime: SystemTime,
        current_size: u64,
        purpose: GatePurpose,
    ) -> PreModifyCheck {
        let by_path = self.lock();
        let Some(entry) = by_path.get(path) else {
            return PreModifyCheck::Reject(message_no_entry(purpose));
        };
        if !matches!(entry.last_view, LastView::Full) {
            return PreModifyCheck::Reject(message_partial(purpose));
        }
        if entry.mtime == current_mtime && entry.size == current_size {
            PreModifyCheck::Pass
        } else {
            PreModifyCheck::Drift {
                stored_hash: entry.content_hash,
            }
        }
    }

    /// Resolves a [`PreModifyCheck::Drift`] outcome. `current_hash` is
    /// xxh64 of the file bytes the caller just read (under
    /// [`HASH_SEED`]). Matching hashes mean the mtime / size touch was
    /// a no-op (Windows cloud-sync timestamp shuffle) and the gate
    /// passes; mismatches surface the staleness error.
    ///
    /// `&self` keeps the API symmetric with [`Self::pre_modify_check`]
    /// even though the resolution is stateless — both calls form one
    /// gate from the caller's perspective.
    #[expect(
        clippy::unused_self,
        reason = "API symmetry with pre_modify_check; both halves of one logical gate"
    )]
    pub(crate) fn confirm_drift_unchanged(
        &self,
        stored_hash: u64,
        current_hash: u64,
        purpose: GatePurpose,
    ) -> Result<(), String> {
        if stored_hash == current_hash {
            Ok(())
        } else {
            Err(message_drift(purpose))
        }
    }

    /// Records the post-modify state of a file (Edit or Write success
    /// path). Always promotes the entry to `LastView::Full` because
    /// every Edit / Write writes a complete file body.
    pub(crate) fn record_modify(&self, path: &Path, bytes: &[u8], mtime: SystemTime, size: u64) {
        let content_hash = xxh64(bytes, HASH_SEED);
        self.lock().insert(
            path.to_path_buf(),
            FileState {
                content_hash,
                mtime,
                size,
                last_view: LastView::Full,
            },
        );
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, FileState>> {
        self.by_path.lock().expect("FileTracker mutex poisoned")
    }
}

/// Outcome of [`FileTracker::record_read`]. Distinct enum (not `bool`)
/// so the call site reads `RecordRead::CacheHit` rather than guessing
/// what `true` means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordRead {
    Inserted,
    CacheHit,
}

/// Outcome of [`FileTracker::pre_modify_check`]. See method doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreModifyCheck {
    Pass,
    Drift { stored_hash: u64 },
    Reject(String),
}

// ── Error messages ──

/// Stub returned to the model when a full Read finds the file
/// unchanged since the last full Read in this session. Saves emitting
/// the bytes again and signals that the prior Read is still authoritative.
pub(crate) const CACHE_HIT_STUB: &str =
    "File hasn't been modified since the last read. Returning already-read file.";

fn message_no_entry(purpose: GatePurpose) -> String {
    format!(
        "File has not been read in this session. Use the Read tool first before {} it.",
        verb(purpose),
    )
}

fn message_partial(purpose: GatePurpose) -> String {
    format!(
        "File has only been read partially (with offset / limit). Read the full file before {} it.",
        verb(purpose),
    )
}

fn message_drift(purpose: GatePurpose) -> String {
    format!(
        "File has been modified externally since it was last read. Re-read it before {} it.",
        verb(purpose),
    )
}

fn verb(purpose: GatePurpose) -> &'static str {
    match purpose {
        GatePurpose::Edit => "editing",
        GatePurpose::Write => "writing to",
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

    // ── pre_modify_check ──

    #[test]
    fn pre_modify_check_no_entry_rejects_with_must_read_first() {
        let tracker = FileTracker::new();
        let path = Path::new("/tmp/a.rs");
        let check = tracker.pre_modify_check(path, UNIX_EPOCH, 0, GatePurpose::Edit);
        let PreModifyCheck::Reject(msg) = check else {
            panic!("expected Reject, got {check:?}");
        };
        assert!(
            msg.contains("not been read"),
            "must-read-first message: {msg}",
        );
        assert!(msg.contains("editing"), "verb is editing for Edit: {msg}");
    }

    #[test]
    fn pre_modify_check_no_entry_uses_writing_verb_for_write_purpose() {
        let tracker = FileTracker::new();
        let check =
            tracker.pre_modify_check(Path::new("/tmp/a.rs"), UNIX_EPOCH, 0, GatePurpose::Write);
        let PreModifyCheck::Reject(msg) = check else {
            panic!("expected Reject, got {check:?}");
        };
        assert!(msg.contains("writing to"), "verb is writing to: {msg}");
    }

    #[test]
    fn pre_modify_check_partial_view_rejects_with_full_read_required() {
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
        let check = tracker.pre_modify_check(path, UNIX_EPOCH, 5, GatePurpose::Edit);
        let PreModifyCheck::Reject(msg) = check else {
            panic!("expected Reject, got {check:?}");
        };
        assert!(msg.contains("partially"), "partial-view message: {msg}");
        assert!(msg.contains("Read the full file"), "{msg}");
    }

    #[test]
    fn pre_modify_check_stat_match_passes_without_drift() {
        let tracker = FileTracker::new();
        let path = Path::new("/tmp/a.rs");
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(path, b"hello", mtime, 5, LastView::Full);
        let check = tracker.pre_modify_check(path, mtime, 5, GatePurpose::Edit);
        assert_eq!(check, PreModifyCheck::Pass);
    }

    #[test]
    fn pre_modify_check_mtime_drift_returns_stored_hash() {
        let tracker = FileTracker::new();
        let path = Path::new("/tmp/a.rs");
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(path, b"hello", mtime, 5, LastView::Full);

        let drifted_mtime = mtime + Duration::from_secs(1);
        let check = tracker.pre_modify_check(path, drifted_mtime, 5, GatePurpose::Edit);

        assert_eq!(
            check,
            PreModifyCheck::Drift {
                stored_hash: xxh64(b"hello", HASH_SEED),
            },
            "mtime drift surfaces the stored hash for the caller to confirm",
        );
    }

    #[test]
    fn pre_modify_check_size_drift_returns_stored_hash() {
        let tracker = FileTracker::new();
        let path = Path::new("/tmp/a.rs");
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        _ = tracker.record_read(path, b"hello", mtime, 5, LastView::Full);

        let check = tracker.pre_modify_check(path, mtime, 999, GatePurpose::Edit);

        assert!(
            matches!(check, PreModifyCheck::Drift { .. }),
            "size drift triggers Drift even when mtime matched: {check:?}",
        );
    }

    // ── confirm_drift_unchanged ──

    #[test]
    fn confirm_drift_unchanged_matching_hash_passes() {
        // Stat said the file changed, but rehash matches — the mtime /
        // size touch was content-preserving (cloud-sync workaround).
        let tracker = FileTracker::new();
        let stored = xxh64(b"hello", HASH_SEED);
        let result = tracker.confirm_drift_unchanged(stored, stored, GatePurpose::Edit);
        assert!(result.is_ok());
    }

    #[test]
    fn confirm_drift_unchanged_differing_hash_rejects_with_modified_externally() {
        let tracker = FileTracker::new();
        let result = tracker.confirm_drift_unchanged(
            xxh64(b"old", HASH_SEED),
            xxh64(b"new", HASH_SEED),
            GatePurpose::Edit,
        );
        let err = result.expect_err("hash mismatch must reject");
        assert!(err.contains("modified externally"), "wording: {err}");
        assert!(err.contains("Re-read"), "{err}");
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

    // ── LastView serde round-trip ──

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
