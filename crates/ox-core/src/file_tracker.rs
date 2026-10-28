//! Per-session Read-before-Edit gate with xxh64 staleness detection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context as _, ensure};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio_util::task::TaskTracker;
use tracing::instrument::WithSubscriber as _;
use tracing::{debug, warn};
use xxhash_rust::xxh64::xxh64;

use crate::util::fs::read_regular_file;
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
///   in one Read or across several. An Edit accepts any Read of the current content and credits the
///   new lines it wrote, or the whole content when the tracked view already covered every line.
/// - **Freshness**: mutating tools must pass the current bytes through
///   [`Self::verify_current_content`], and only matching xxh64 content counts as unchanged.
///
/// State is persisted into the session JSONL as [`FileSnapshot`]s and re-verified against disk on
/// resume by [`Self::restored`].
#[derive(Default)]
pub(crate) struct FileTracker {
    by_path: Mutex<HashMap<PathBuf, FileState>>,
    writes: TaskTracker,
}

/// What the tracker knows about one file's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileState {
    pub(crate) content_hash: u64,
    pub(crate) last_view: LastView,
    /// Whether a Read showed this full content, so a re-Read may answer with [`CACHE_HIT_STUB`].
    #[serde(default, skip_serializing_if = "FileSource::is_read")]
    pub(crate) source: FileSource,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) recorded_at: OffsetDateTime,
    /// The tool call that recorded this state. Resume restores the state only while the transcript
    /// still holds that call's result.
    pub(crate) tool_use_id: String,
}

/// Whether a Read showed a tracked state whole, which decides whether a full re-Read may answer
/// from the cache. A Write or Edit supplies content only through tool input, so deduplicating a
/// Read against it would point at pre-edit content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileSource {
    #[default]
    Read,
    Modify,
}

impl FileSource {
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde calls skip_serializing_if with a reference"
    )]
    fn is_read(&self) -> bool {
        matches!(self, Self::Read)
    }
}

impl FileTracker {
    /// Builds the tracker of a resumed session from the `snapshots` it saved. On duplicate paths
    /// the snapshot with the newer `recorded_at` wins, and it must still match disk by content
    /// hash. Also returns the dropped paths, sorted, which need a fresh Read before any mutation.
    /// The hashing runs on the blocking pool, so it never stalls the async runtime.
    pub(crate) async fn restored(snapshots: Vec<FileSnapshot>) -> (Self, Vec<PathBuf>) {
        let mut latest = HashMap::<PathBuf, FileState>::new();
        for FileSnapshot { path, state } in snapshots {
            let replace = latest
                .get(&path)
                .is_none_or(|cur| state.recorded_at >= cur.recorded_at);
            if replace {
                latest.insert(path, state);
            }
        }

        let (latest, dropped) = tokio::task::spawn_blocking(move || {
            let mut dropped: Vec<_> = latest
                .extract_if(|path, state| {
                    verify_on_disk(path, state.content_hash)
                        .inspect_err(|e| {
                            debug!(
                                path = %path.display(),
                                error = %format_args!("{e:#}"),
                                "dropping a tracked file that changed on disk",
                            );
                        })
                        .is_err()
                })
                .map(|(path, _)| path)
                .collect();
            dropped.sort();
            (latest, dropped)
        })
        .await
        .expect("verifying restored snapshots never panics");
        let tracker = Self {
            by_path: Mutex::new(latest),
            ..Self::default()
        };
        (tracker, dropped)
    }

    /// Returns `CacheHit` when a Read already tracked the file as `Full` and a Read of the same
    /// content in full follows unchanged. Reads of unchanged content accumulate their line windows,
    /// and changed content starts over. A read that returns content attributes the state to its
    /// tool call, `tool_use_id`.
    pub(crate) fn record_read(
        &self,
        path: &Path,
        bytes: &[u8],
        view: LastView,
        tool_use_id: &str,
    ) -> RecordRead {
        let content_hash = xxh64(bytes, HASH_SEED);
        let mut by_path = self.lock();

        let full = view == LastView::Full;
        if let Some(state) = by_path.get_mut(path)
            && state.content_hash == content_hash
        {
            let outcome =
                if state.source == FileSource::Read && state.last_view == LastView::Full && full {
                    RecordRead::CacheHit
                } else {
                    RecordRead::Inserted
                };
            let seen = std::mem::replace(&mut state.last_view, LastView::Full);
            state.last_view = seen.union(view);
            if outcome == RecordRead::Inserted {
                tool_use_id.clone_into(&mut state.tool_use_id);
                if full {
                    state.source = FileSource::Read;
                }
            }
            return outcome;
        }

        by_path.insert(
            path.to_path_buf(),
            FileState {
                content_hash,
                last_view: view,
                source: FileSource::Read,
                recorded_at: OffsetDateTime::now_utc(),
                tool_use_id: tool_use_id.to_owned(),
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
    /// it returns, with `written` saying which lines of them the model knows, attributed to the
    /// tool call `tool_use_id`, so the record lands with the write even when the caller is dropped
    /// mid-write. The owner must drain writes before resetting state or capturing final snapshots.
    pub(crate) async fn write_and_record<W>(
        self: &Arc<Self>,
        tool_use_id: &str,
        view: LastView,
        write: W,
    ) -> anyhow::Result<()>
    where
        W: Future<Output = anyhow::Result<(PathBuf, Vec<u8>)>> + Send + 'static,
    {
        let tracker = Arc::clone(self);
        let tool_use_id = tool_use_id.to_owned();
        self.writes
            .spawn(
                async move {
                    match write.await {
                        Ok((key, bytes)) => {
                            tracker.record_modify(key, &bytes, view, tool_use_id);
                            Ok(())
                        }
                        Err(error) => {
                            warn!(tool_use_id, error = %format_args!("{error:#}"), "workspace write failed");
                            Err(error)
                        }
                    }
                }
                .with_current_subscriber(),
            )
            .await
            .unwrap_or_else(|e| Err(e.into()))
    }

    /// Waits for admitted writes and their records. The owner must stop tool admission first.
    pub(crate) async fn drain_writes(&self) {
        self.writes.close();
        self.writes.wait().await;
    }

    pub(crate) fn reopen_writes(&self) {
        self.writes.reopen();
    }

    /// Records bytes the agent just wrote, as `view` describes them, so a subsequent Edit passes
    /// the gate without a fresh Read.
    fn record_modify(&self, path: PathBuf, bytes: &[u8], view: LastView, tool_use_id: String) {
        let content_hash = xxh64(bytes, HASH_SEED);
        let mut by_path = self.lock();
        let last_view = if by_path
            .get(&path)
            .is_some_and(|state| state.last_view == LastView::Full)
        {
            LastView::Full
        } else {
            view
        };
        by_path.insert(
            path,
            FileState {
                content_hash,
                last_view,
                source: FileSource::Modify,
                recorded_at: OffsetDateTime::now_utc(),
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

    /// Replaces the tracked state with `snapshots`, as [`Self::snapshot_all`] took them, without
    /// checking them against disk.
    pub(crate) fn reset_to(&self, snapshots: Vec<FileSnapshot>) {
        *self.lock() = snapshots
            .into_iter()
            .map(|FileSnapshot { path, state }| (path, state))
            .collect();
    }

    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<PathBuf, FileState>> {
        lock_or_recover(&self.by_path, "FileTracker")
    }
}

/// Checks that the file at `path` is still trackable and holds the content `content_hash` names.
fn verify_on_disk(path: &Path, content_hash: u64) -> anyhow::Result<()> {
    let (_, bytes) = read_regular_file(path, MAX_TRACKED_FILE_SIZE).context("read failed")?;
    ensure!(xxh64(&bytes, HASH_SEED) == content_hash, "content changed");
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

    pub(crate) fn from_ranges(mut ranges: Vec<(usize, usize)>, total_lines: usize) -> Self {
        if total_lines == 0 {
            return Self::Full;
        }
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

// ── FileSnapshot ──

/// Tracked file state persisted at session finish and revalidated against disk on resume.
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

/// Read-before-Edit / Write failures with recovery instructions for the model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum GateError {
    #[error(
        "File {} has not been read in this session. Read it before {} it.",
        .path.display(),
        .purpose.verb(),
    )]
    NeverRead { path: PathBuf, purpose: GatePurpose },
    #[error(
        "File {} has only been read in part. \
         Read the remaining lines, paging with `offset` and `limit` if needed, before {} it.",
        .path.display(),
        .purpose.verb(),
    )]
    PartialRead { path: PathBuf, purpose: GatePurpose },
    #[error(
        "File {} has been modified externally since it was last read. Re-read it before {} it.",
        .path.display(),
        .purpose.verb(),
    )]
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

    use tokio::sync::oneshot;

    use super::{FileTracker, LastView};

    pub(crate) fn tracker() -> Arc<FileTracker> {
        Arc::new(FileTracker::default())
    }

    pub(crate) async fn blocked_write(
        tracker: &Arc<FileTracker>,
        path: &Path,
        bytes: &[u8],
        tool_use_id: &str,
    ) -> oneshot::Sender<()> {
        let path = path.to_owned();
        let bytes = bytes.to_vec();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let mut caller =
            std::pin::pin!(
                tracker.write_and_record(tool_use_id, LastView::Full, async move {
                    started_tx.send(()).unwrap();
                    release_rx.await.map_err(std::io::Error::other)?;
                    tokio::fs::write(&path, &bytes).await?;
                    Ok((path, bytes))
                })
            );
        assert!(futures::poll!(caller.as_mut()).is_pending());
        started_rx.await.unwrap();
        release_tx
    }

    pub(crate) fn assert_writes_draining(tracker: &FileTracker) {
        assert!(tracker.writes.is_closed());
        assert!(!tracker.writes.is_empty());
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
        tool_use_id: &str,
    ) {
        std::fs::write(path, bytes).unwrap();
        tracker.record_modify(
            path.to_path_buf(),
            bytes,
            LastView::Full,
            tool_use_id.to_owned(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::thread;
    use std::time::Duration;

    use super::testing::tracker;
    use super::*;
    use crate::util::fs::testing::deny_read_access;

    /// Writes `bytes` to `path` and returns them under `path` as the tracker key.
    async fn write_file(path: PathBuf, bytes: &[u8]) -> anyhow::Result<(PathBuf, Vec<u8>)> {
        tokio::fs::write(&path, bytes).await?;
        Ok((path, bytes.to_vec()))
    }

    mod record_read {
        use super::*;

        #[test]
        fn first_full_read_inserts() {
            let tracker = FileTracker::default();
            let path = Path::new("/tmp/a.rs");

            let outcome = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");

            assert_eq!(outcome, RecordRead::Inserted);
            let stored = tracker.lock().get(path).cloned().unwrap();
            assert_eq!(stored.content_hash, xxh64(b"hello", HASH_SEED));
            assert_eq!(stored.last_view, LastView::Full);
        }

        #[test]
        fn cache_hit_preserves_recorded_at() {
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
        fn changed_content_inserts_not_cache_hit() {
            let tracker = FileTracker::default();
            let path = Path::new("/tmp/a.rs");
            _ = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");

            let outcome = tracker.record_read(path, b"world", LastView::Full, "toolu_test");

            assert_eq!(outcome, RecordRead::Inserted);
        }

        #[test]
        fn partial_view_does_not_cache_hit() {
            let tracker = FileTracker::default();
            let path = Path::new("/tmp/a.rs");
            let view = LastView::lines(1, 1, 5);
            _ = tracker.record_read(path, b"hello", view.clone(), "toolu_test");

            let outcome = tracker.record_read(path, b"hello", view, "toolu_test");

            assert_eq!(outcome, RecordRead::Inserted);
        }

        #[test]
        fn a_partial_read_does_not_lend_a_write_a_cache_hit() {
            let tracker = FileTracker::default();
            let path = Path::new("/tmp/a.rs");
            tracker.record_modify(
                path.to_path_buf(),
                b"hello",
                LastView::Full,
                "toolu_write".to_owned(),
            );
            _ = tracker.record_read(path, b"hello", LastView::lines(1, 1, 2), "toolu_read");

            let full = tracker.record_read(path, b"hello", LastView::Full, "toolu_read");

            assert_eq!(full, RecordRead::Inserted);
            assert_eq!(tracker.lock()[path].source, FileSource::Read);
            assert_eq!(
                tracker.record_read(path, b"hello", LastView::Full, "toolu_read"),
                RecordRead::CacheHit,
            );
        }

        #[test]
        fn full_after_partial_does_not_cache_hit() {
            let tracker = FileTracker::default();
            let path = Path::new("/tmp/a.rs");
            _ = tracker.record_read(path, b"hello", LastView::lines(1, 1, 5), "toolu_test");

            let outcome = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");
            let next = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");

            assert_eq!(outcome, RecordRead::Inserted);
            assert_eq!(next, RecordRead::CacheHit);
        }

        #[test]
        fn partial_after_full_keeps_full_for_unchanged_content() {
            let tracker = FileTracker::default();
            let path = Path::new("/tmp/a.rs");
            _ = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");

            let outcome =
                tracker.record_read(path, b"hello", LastView::lines(2, 2, 5), "toolu_test");

            assert_eq!(
                outcome,
                RecordRead::Inserted,
                "partial reads return the slice"
            );
            assert_eq!(tracker.lock()[path].last_view, LastView::Full);
        }

        #[test]
        fn paging_unchanged_content_accumulates_to_full() {
            let tracker = FileTracker::default();
            let path = Path::new("/tmp/a.rs");
            _ = tracker.record_read(path, b"hello", LastView::lines(1, 2, 4), "toolu_test");
            let first = tracker.lock()[path].clone();

            let outcome =
                tracker.record_read(path, b"hello", LastView::lines(3, 4, 4), "toolu_test");
            let paged = tracker.lock()[path].clone();
            _ = tracker.record_read(path, b"world", LastView::lines(3, 4, 4), "toolu_test");

            assert_eq!(
                outcome,
                RecordRead::Inserted,
                "paged reads return the slice"
            );
            assert_eq!(paged.last_view, LastView::Full);
            assert_eq!(paged.recorded_at, first.recorded_at);
            assert_eq!(
                tracker.lock()[path].last_view,
                LastView::lines(3, 4, 4),
                "changed content starts coverage over",
            );
        }

        #[test]
        fn concurrent_files_preserve_every_entry() {
            let tracker = FileTracker::default();

            thread::scope(|s| {
                for t in 0..8u32 {
                    let tracker = &tracker;
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
    }

    mod verify_current_content {
        use super::*;

        #[test]
        fn full_match_passes() {
            let tracker = FileTracker::default();
            let path = Path::new("/tmp/a.rs");
            _ = tracker.record_read(path, b"hello", LastView::Full, "toolu_test");

            let result = tracker.verify_current_content(path, b"hello", GatePurpose::Edit);

            assert_eq!(result, Ok(()));
        }

        #[test]
        fn unread_file_reports_the_blocked_action() {
            let tracker = FileTracker::default();
            let path = Path::new("/tmp/a.rs");

            for purpose in [GatePurpose::Edit, GatePurpose::Write] {
                let result = tracker.verify_current_content(path, b"", purpose);

                assert_eq!(
                    result,
                    Err(GateError::NeverRead {
                        path: path.to_path_buf(),
                        purpose
                    }),
                );
            }
        }

        #[test]
        fn partial_view_blocks_write_but_not_edit() {
            let tracker = FileTracker::default();
            let path = Path::new("/tmp/a.rs");
            _ = tracker.record_read(path, b"hello", LastView::lines(1, 1, 2), "toolu_test");

            let edit = tracker.verify_current_content(path, b"hello", GatePurpose::Edit);
            let write = tracker.verify_current_content(path, b"hello", GatePurpose::Write);
            let drifted = tracker.verify_current_content(path, b"world", GatePurpose::Write);

            assert_eq!(edit, Ok(()));
            assert_eq!(
                write,
                Err(GateError::PartialRead {
                    path: path.to_path_buf(),
                    purpose: GatePurpose::Write,
                }),
            );
            assert_eq!(
                drifted,
                Err(GateError::ContentDrifted {
                    path: path.to_path_buf(),
                    purpose: GatePurpose::Write,
                }),
                "drift outranks coverage",
            );
        }

        #[test]
        fn divergent_bytes_rejects_content_drift() {
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
    }

    mod write_and_record {
        use super::*;

        #[tokio::test]
        async fn lands_the_bytes_as_a_full_view() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.rs");
            let key = Path::new("/key/a.rs");
            let tracker = tracker();
            _ = tracker.record_read(key, b"hello", LastView::lines(1, 1, 2), "toolu_test");

            tracker
                .write_and_record("toolu_test", LastView::Full, {
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
            assert_eq!(stored.source, FileSource::Modify);
        }

        #[tokio::test]
        async fn an_edit_after_a_full_read_keeps_the_full_view() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.rs");
            let key = Path::new("/key/a.rs");
            let tracker = tracker();
            _ = tracker.record_read(key, b"old", LastView::Full, "toolu_read");

            tracker
                .write_and_record(
                    "toolu_edit",
                    LastView::from_ranges(vec![(2, 2)], 3),
                    async move {
                        tokio::fs::write(&path, b"new").await?;
                        Ok((key.to_path_buf(), b"new".to_vec()))
                    },
                )
                .await
                .unwrap();

            let stored = tracker.lock().get(key).cloned().unwrap();
            assert_eq!(stored.last_view, LastView::Full);
            assert_eq!(stored.source, FileSource::Modify);
        }

        #[tokio::test]
        async fn an_edit_after_a_partial_read_credits_only_the_written_lines() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.rs");
            let key = Path::new("/key/a.rs");
            let tracker = tracker();
            _ = tracker.record_read(key, b"old", LastView::lines(1, 1, 3), "toolu_read");

            tracker
                .write_and_record(
                    "toolu_edit",
                    LastView::from_ranges(vec![(2, 2)], 3),
                    async move {
                        tokio::fs::write(&path, b"new").await?;
                        Ok((key.to_path_buf(), b"new".to_vec()))
                    },
                )
                .await
                .unwrap();

            let stored = tracker.lock().get(key).cloned().unwrap();
            assert_eq!(stored.last_view, LastView::lines(2, 2, 3));
            assert_eq!(stored.source, FileSource::Modify);
        }

        #[tokio::test]
        async fn records_a_write_its_dropped_caller_started() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.rs");
            let tracker = tracker();
            let release =
                super::super::testing::blocked_write(&tracker, &path, b"w", "toolu_write").await;
            let mut drain = std::pin::pin!(tracker.drain_writes());

            assert!(futures::poll!(drain.as_mut()).is_pending());
            super::super::testing::assert_writes_draining(&tracker);
            assert!(!path.exists());
            assert_eq!(tracker.snapshot_all(), []);
            release.send(()).unwrap();
            drain.await;

            assert_eq!(std::fs::read(&path).unwrap(), b"w");
            let stored = tracker.lock().get(&path).cloned().unwrap();
            assert_eq!(stored.content_hash, xxh64(b"w", HASH_SEED));
            assert_eq!(stored.tool_use_id, "toolu_write");
        }

        #[tokio::test]
        async fn failed_write_keeps_the_previous_record() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("missing").join("a.rs");
            let tracker = tracker();
            _ = tracker.record_read(&path, b"hello", LastView::Full, "toolu_test");

            let result = tracker
                .write_and_record(
                    "toolu_test",
                    LastView::Full,
                    write_file(path.clone(), b"world"),
                )
                .await;

            assert_eq!(
                result
                    .unwrap_err()
                    .downcast::<std::io::Error>()
                    .unwrap()
                    .kind(),
                std::io::ErrorKind::NotFound,
                "the parent directory is missing",
            );
            let stored = tracker.lock().get(&path).cloned().unwrap();
            assert_eq!(stored.content_hash, xxh64(b"hello", HASH_SEED));
        }
    }

    mod snapshot_all {
        use super::*;

        #[test]
        fn collects_every_tracked_file() {
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
        fn empty_tracker_is_empty() {
            let tracker = FileTracker::default();

            let snaps = tracker.snapshot_all();

            assert_eq!(snaps, []);
        }
    }

    mod reset_to {
        use super::*;

        #[test]
        fn returns_to_the_snapshot_dropping_later_reads_and_changes() {
            let tracker = FileTracker::default();
            _ = tracker.record_read(Path::new("/tmp/a"), b"a", LastView::Full, "toolu_1");
            let checkpoint = tracker.snapshot_all();
            _ = tracker.record_read(Path::new("/tmp/a"), b"changed", LastView::Full, "toolu_2");
            _ = tracker.record_read(Path::new("/tmp/b"), b"b", LastView::Full, "toolu_2");

            tracker.reset_to(checkpoint.clone());

            assert_eq!(tracker.snapshot_all(), checkpoint);
        }
    }

    mod clear {
        use super::*;

        #[test]
        fn drops_recorded_reads_so_subsequent_snapshot_is_empty() {
            let tracker = FileTracker::default();
            _ = tracker.record_read(Path::new("/tmp/a"), b"a", LastView::Full, "toolu_test");
            _ = tracker.record_read(Path::new("/tmp/b"), b"b", LastView::Full, "toolu_test");
            assert_eq!(tracker.snapshot_all().len(), 2);

            tracker.clear();

            assert_eq!(tracker.snapshot_all(), []);
        }
    }

    mod restored {
        use super::*;

        fn snapshot(path: &Path, content: &[u8], recorded_at: OffsetDateTime) -> FileSnapshot {
            FileSnapshot {
                path: path.to_path_buf(),
                state: FileState {
                    content_hash: xxh64(content, HASH_SEED),
                    last_view: LastView::Full,
                    source: FileSource::Read,
                    recorded_at,
                    tool_use_id: "toolu_1".to_owned(),
                },
            }
        }

        #[tokio::test]
        async fn keeps_the_snapshots_that_match_disk() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.rs");
            std::fs::write(&path, b"persisted bytes").unwrap();
            let snap = snapshot(&path, b"persisted bytes", OffsetDateTime::now_utc());

            let (tracker, dropped) = FileTracker::restored(vec![snap.clone()]).await;

            assert_eq!(dropped, [] as [PathBuf; 0]);
            assert_eq!(tracker.snapshot_all(), [snap]);
        }

        #[tokio::test]
        async fn content_drift_drops_snapshot() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.rs");
            std::fs::write(&path, b"new").unwrap();

            let snap = snapshot(&path, b"old", OffsetDateTime::now_utc());

            let (tracker, dropped) = FileTracker::restored(vec![snap]).await;

            assert_eq!(dropped, vec![path.clone()]);
            assert_eq!(tracker.snapshot_all(), []);
        }

        #[tokio::test]
        async fn non_regular_file_drops_snapshot() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("subdir");
            std::fs::create_dir(&path).unwrap();
            let snap = snapshot(&path, b"", OffsetDateTime::now_utc());

            let (tracker, dropped) = FileTracker::restored(vec![snap]).await;

            assert_eq!(dropped, vec![path.clone()]);
            assert_eq!(tracker.snapshot_all(), []);
        }

        #[tokio::test]
        async fn too_large_file_drops_snapshot_without_reading() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("huge.rs");
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(MAX_TRACKED_FILE_SIZE + 1).unwrap();
            let snap = snapshot(&path, b"", OffsetDateTime::now_utc());

            let (tracker, dropped) = FileTracker::restored(vec![snap]).await;

            assert_eq!(dropped, vec![path.clone()]);
            assert_eq!(tracker.snapshot_all(), []);
        }

        #[tokio::test]
        async fn unreadable_file_drops_snapshot() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.rs");
            std::fs::write(&path, b"content").unwrap();
            let snap = snapshot(&path, b"content", OffsetDateTime::now_utc());
            let _denied = deny_read_access(&path);

            let (tracker, dropped) = FileTracker::restored(vec![snap]).await;

            assert_eq!(dropped, vec![path.clone()]);
            assert_eq!(tracker.snapshot_all(), []);
        }

        #[tokio::test]
        async fn final_symlink_drops_snapshot_even_when_content_matches() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.rs");
            let target = dir.path().join("target.rs");
            std::fs::write(&target, b"content").unwrap();
            crate::util::fs::testing::symlink_file(&target, &path);
            let snap = snapshot(&path, b"content", OffsetDateTime::now_utc());

            let (tracker, dropped) = FileTracker::restored(vec![snap]).await;

            assert_eq!(dropped, [path]);
            assert_eq!(tracker.snapshot_all(), []);
        }

        #[tokio::test]
        async fn reports_dropped_paths_in_sorted_order() {
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

            let (tracker, dropped) = FileTracker::restored(snaps).await;

            let mut expected = vec![drifted_path, missing_path];
            expected.sort();
            assert_eq!(
                dropped, expected,
                "both the drifted and the missing snapshots must be reported",
            );
            let stored_paths: Vec<_> = tracker.lock().keys().cloned().collect();
            assert_eq!(stored_paths, vec![kept_path]);
        }

        #[tokio::test]
        async fn verifies_only_newest_snapshot_per_path() {
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

            for snaps in [
                vec![older.clone(), newer.clone()],
                vec![newer.clone(), older.clone()],
            ] {
                let (tracker, dropped) = FileTracker::restored(snaps).await;

                assert_eq!(dropped, [] as [PathBuf; 0]);
                assert_eq!(
                    tracker.snapshot_all(),
                    std::slice::from_ref(&newer),
                    "newer recorded_at wins"
                );
            }
        }
    }

    mod last_view {
        use super::*;

        #[test]
        fn full_serializes_kind_only() {
            let json = serde_json::to_value(LastView::Full).unwrap();

            assert_eq!(json, serde_json::json!({"kind": "full"}));
        }

        #[test]
        fn partial_round_trips_through_json() {
            let view = LastView::lines(3, 5, 9);

            let json = serde_json::to_value(&view).unwrap();
            let parsed: LastView = serde_json::from_value(json.clone()).unwrap();

            assert_eq!(
                json,
                serde_json::json!({"kind": "partial", "ranges": [[3, 5]], "total_lines": 9}),
            );
            assert_eq!(parsed, view);
        }

        mod lines {
            use super::*;

            #[test]
            fn covering_every_line_is_full() {
                assert_eq!(LastView::lines(1, 3, 3), LastView::Full);
                assert_eq!(
                    LastView::lines(2, 3, 3),
                    LastView::Partial {
                        ranges: vec![(2, 3)],
                        total_lines: 3,
                    },
                );
            }

            #[test]
            fn an_emptied_file_is_full() {
                assert_eq!(LastView::from_ranges(Vec::new(), 0), LastView::Full);
            }
        }

        mod union {
            use super::*;

            #[test]
            fn merges_overlapping_and_adjacent_windows_until_full() {
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
            fn with_full_is_full() {
                assert_eq!(
                    LastView::Full.union(LastView::lines(1, 1, 5)),
                    LastView::Full
                );
                assert_eq!(
                    LastView::lines(1, 1, 5).union(LastView::Full),
                    LastView::Full
                );
            }
        }
    }

    mod file_snapshot {
        use super::*;

        #[test]
        fn round_trips_through_json() {
            let snap = FileSnapshot {
                path: PathBuf::from("/tmp/a.rs"),
                state: FileState {
                    content_hash: 0xDEAD_BEEF,
                    last_view: LastView::Partial {
                        ranges: vec![(1, 2), (4, 5)],
                        total_lines: 9,
                    },
                    source: FileSource::Modify,
                    recorded_at: time::macros::datetime!(2026-04-29 12:34:56 UTC),
                    tool_use_id: "toolu_1".to_owned(),
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
                    "source": "modify",
                    "recorded_at": "2026-04-29T12:34:56Z",
                    "tool_use_id": "toolu_1",
                }),
            );
            assert_eq!(parsed, snap);
        }

        #[test]
        fn older_snapshots_without_a_source_default_to_read() {
            let json = serde_json::json!({
                "path": "/tmp/a.rs",
                "content_hash": 1_u64,
                "last_view": {"kind": "full"},
                "recorded_at": "2026-04-29T12:34:56Z",
                "tool_use_id": "toolu_1",
            });

            let parsed: FileSnapshot = serde_json::from_value(json).unwrap();

            assert_eq!(parsed.state.source, FileSource::Read);
        }
    }

    mod gate_error {
        use super::*;

        #[test]
        fn never_read_names_the_path_and_the_blocked_action() {
            for (purpose, expected) in [
                (
                    GatePurpose::Edit,
                    "File /tmp/a.rs has not been read in this session. \
                     Read it before editing it.",
                ),
                (
                    GatePurpose::Write,
                    "File /tmp/a.rs has not been read in this session. \
                     Read it before writing to it.",
                ),
            ] {
                let err = GateError::NeverRead {
                    path: PathBuf::from("/tmp/a.rs"),
                    purpose,
                };

                assert_eq!(err.to_string(), expected, "{purpose:?}");
            }
        }

        #[test]
        fn partial_read_asks_for_the_remaining_lines() {
            let err = GateError::PartialRead {
                path: PathBuf::from("/tmp/a.rs"),
                purpose: GatePurpose::Write,
            };

            assert_eq!(
                err.to_string(),
                "File /tmp/a.rs has only been read in part. \
                 Read the remaining lines, paging with `offset` and `limit` if needed, \
                 before writing to it.",
            );
        }

        #[test]
        fn content_drifted_asks_for_a_fresh_read() {
            let err = GateError::ContentDrifted {
                path: PathBuf::from("/tmp/a.rs"),
                purpose: GatePurpose::Edit,
            };

            assert_eq!(
                err.to_string(),
                "File /tmp/a.rs has been modified externally since it was last read. \
                 Re-read it before editing it.",
            );
        }
    }

    mod attribution {
        use super::*;

        #[tokio::test]
        async fn credits_each_state_to_its_read_or_write_call() {
            let dir = tempfile::tempdir().unwrap();
            let tracker = tracker();
            let read = Path::new("/tmp/read.rs");
            let paged = Path::new("/tmp/paged.rs");
            let written = dir.path().join("written.rs");

            tracker.record_read(read, b"r", LastView::Full, "toolu_read");
            tracker.record_read(paged, b"p", LastView::lines(1, 1, 2), "toolu_page_1");
            tracker.record_read(paged, b"p", LastView::lines(2, 2, 2), "toolu_page_2");
            tracker
                .write_and_record(
                    "toolu_write",
                    LastView::Full,
                    write_file(written.clone(), b"w"),
                )
                .await
                .unwrap();
            let reread = tracker.record_read(read, b"r", LastView::Full, "toolu_reread");

            assert_eq!(reread, RecordRead::CacheHit);
            let origins: BTreeMap<PathBuf, String> = tracker
                .snapshot_all()
                .into_iter()
                .map(|snap| (snap.path, snap.state.tool_use_id))
                .collect();
            assert_eq!(
                origins,
                BTreeMap::from([
                    (paged.to_path_buf(), "toolu_page_2".to_owned()),
                    (read.to_path_buf(), "toolu_read".to_owned()),
                    (written, "toolu_write".to_owned()),
                ]),
                "the newest read that returned content owns the state, and a cache hit keeps it",
            );
        }
    }
}
