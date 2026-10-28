//! Directory locks around credential writes, compatible with the `proper-lockfile` locks Claude
//! Code takes on its config directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use tracing::{debug, warn};

use super::token::TOKEN_REQUEST_TIMEOUT;
use crate::util::sync::lock_or_recover;

// Match the `stale` and `update` defaults of `proper-lockfile`, which Claude Code locks its config
// directory with.
const LOCK_STALE_THRESHOLD: Duration = Duration::from_secs(10);
const LOCK_UPDATE_INTERVAL: Duration = Duration::from_secs(5);
/// How long a process waits for another to finish a refresh, which covers the holder's token
/// request plus one mtime update.
pub(super) const LOCK_WAIT: Duration = TOKEN_REQUEST_TIMEOUT.saturating_add(LOCK_UPDATE_INTERVAL);
pub(super) const LOCK_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Credential lock kept fresh by a background heartbeat. Drop stops the heartbeat and removes the
/// lock while still owned. Like `proper-lockfile`, a changed mtime signals takeover, so the
/// heartbeat stops and leaves the new owner's lock intact.
pub(super) struct LockGuard {
    path: PathBuf,
    /// The mtime this process last gave the lock, `None` once another process took it over.
    mtime: Arc<std::sync::Mutex<Option<SystemTime>>>,
    heartbeat: tokio::task::JoinHandle<()>,
}

impl LockGuard {
    fn hold(path: &Path) -> std::io::Result<Self> {
        let mtime = Arc::new(std::sync::Mutex::new(Some(lock_mtime(path)?)));
        let heartbeat = tokio::spawn({
            let path = path.to_owned();
            let mtime = Arc::clone(&mtime);
            async move {
                loop {
                    tokio::time::sleep(LOCK_UPDATE_INTERVAL).await;
                    let mut held = lock_or_recover(&mtime, "credentials lock");
                    if !held.is_some_and(|set| owns_lock(&path, set)) {
                        warn!(path = %path.display(), "another process took over the lock");
                        *held = None;
                        return;
                    }
                    match touch_dir(&path).and_then(|()| lock_mtime(&path)) {
                        Ok(set) => *held = Some(set),
                        Err(e) => {
                            debug!(path = %path.display(), error = %e, "failed to refresh lock");
                        }
                    }
                }
            }
        });
        Ok(Self {
            path: path.to_owned(),
            mtime,
            heartbeat,
        })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.heartbeat.abort();
        let held = *lock_or_recover(&self.mtime, "credentials lock");
        if !held.is_some_and(|set| owns_lock(&self.path, set)) {
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            debug!(path = %self.path.display(), error = %e, "failed to release lock");
        }
    }
}

/// Why [`wait_for_lock`] returned without the lock.
pub(super) enum Unlocked {
    /// Another process held the lock for all of [`LOCK_WAIT`].
    Held,
    Failed(anyhow::Error),
}

/// Directory-based advisory lock around credential writes. `mkdir` is the cross-platform atomic
/// primitive, and the guard `rmdir`s on drop.
pub(super) async fn acquire_lock(path: &Path) -> Result<LockGuard> {
    match wait_for_lock(path).await {
        Ok(lock) => Ok(lock),
        Err(Unlocked::Held) => bail!(
            "another process held {} for over {} s",
            path.display(),
            LOCK_WAIT.as_secs()
        ),
        Err(Unlocked::Failed(e)) => Err(e),
    }
}

/// Acquires `path`, waiting up to [`LOCK_WAIT`] and clearing locks whose mtime is older than
/// [`LOCK_STALE_THRESHOLD`].
pub(super) async fn wait_for_lock(path: &Path) -> Result<LockGuard, Unlocked> {
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        match std::fs::create_dir(path) {
            Ok(()) => {
                return LockGuard::hold(path).map_err(|e| {
                    Unlocked::Failed(
                        anyhow::Error::new(e)
                            .context(format!("failed to read lock {}", path.display())),
                    )
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale_lock(path)
                    && let Err(e) = std::fs::remove_dir_all(path)
                {
                    debug!(path = %path.display(), error = %e, "failed to clear stale lock");
                }
            }
            Err(e) => {
                return Err(Unlocked::Failed(
                    anyhow::Error::new(e)
                        .context(format!("failed to create lock at {}", path.display())),
                ));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Unlocked::Held);
        }
        tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
    }
}

fn is_stale_lock(path: &Path) -> bool {
    lock_mtime(path).is_ok_and(|t| t.elapsed().unwrap_or_default() > LOCK_STALE_THRESHOLD)
}

fn lock_mtime(path: &Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(path)?.modified()
}

/// Returns whether the lock at `path` still has the mtime this process last gave it, `set`.
fn owns_lock(path: &Path, set: SystemTime) -> bool {
    lock_mtime(path).is_ok_and(|mtime| mtime == set)
}

/// Sets the lock directory's mtime to now, as `proper-lockfile` refreshes a held lock.
fn touch_dir(path: &Path) -> std::io::Result<()> {
    open_dir(path)?.set_modified(SystemTime::now())
}

/// Opens a directory with enough access to set its mtime.
fn open_dir(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;

        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES,
        };

        // Windows opens a directory only with `FILE_FLAG_BACKUP_SEMANTICS`.
        std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }
    #[cfg(not(windows))]
    std::fs::File::open(path)
}

/// Where Claude Code's `proper-lockfile` locks the directory `dir`: beside its real path, with
/// `.lock` appended, so a symlinked `~/.claude` locks where its target does.
pub(super) fn sibling_lock(dir: &Path) -> PathBuf {
    let mut lock = dunce::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_owned())
        .into_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod acquire_lock {
        use super::*;

        /// Sets the lock's mtime to `at`, as its holder or a process taking it over would.
        fn set_lock_mtime(lock_path: &Path, at: SystemTime) -> SystemTime {
            open_dir(lock_path).unwrap().set_modified(at).unwrap();
            lock_mtime(lock_path).unwrap()
        }

        #[tokio::test]
        async fn creates_and_drop_removes() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");

            let guard = acquire_lock(&lock_path).await.unwrap();
            assert!(lock_path.exists());

            drop(guard);
            assert!(!lock_path.exists());
        }

        #[tokio::test(start_paused = true)]
        async fn clears_stale_lock() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");
            std::fs::create_dir(&lock_path).unwrap();
            set_lock_mtime(
                &lock_path,
                SystemTime::now() - LOCK_STALE_THRESHOLD - Duration::from_secs(1),
            );

            let _guard = acquire_lock(&lock_path).await.unwrap();
            assert!(!is_stale_lock(&lock_path));
        }

        #[tokio::test(start_paused = true)]
        async fn keeps_a_held_lock_fresh_until_dropped() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");
            let guard = acquire_lock(&lock_path).await.unwrap();
            let long_ago = SystemTime::now() - LOCK_STALE_THRESHOLD * 2;
            *guard.mtime.lock().unwrap() = Some(set_lock_mtime(&lock_path, long_ago));

            tokio::time::sleep(LOCK_UPDATE_INTERVAL + Duration::from_millis(1)).await;

            assert!(!is_stale_lock(&lock_path));
            drop(guard);
            assert!(!lock_path.exists());
        }

        #[tokio::test(start_paused = true)]
        async fn leaves_a_lock_another_process_took_over_alone() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");
            let guard = acquire_lock(&lock_path).await.unwrap();
            let other = set_lock_mtime(&lock_path, SystemTime::now() - Duration::from_secs(3));

            tokio::time::sleep(LOCK_UPDATE_INTERVAL * 2).await;

            assert_eq!(lock_mtime(&lock_path).unwrap(), other);
            assert_eq!(*guard.mtime.lock().unwrap(), None);
            drop(guard);
            assert!(lock_path.exists());
        }

        #[tokio::test]
        async fn leaves_a_lock_taken_over_before_the_next_refresh_alone() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");
            let guard = acquire_lock(&lock_path).await.unwrap();
            set_lock_mtime(&lock_path, SystemTime::now() - Duration::from_secs(3));

            drop(guard);

            assert!(lock_path.exists());
        }

        #[tokio::test(start_paused = true)]
        async fn reports_a_lock_held_past_the_wait() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");
            std::fs::create_dir(&lock_path).unwrap();

            let err = acquire_lock(&lock_path).await.err().expect("held lock");

            assert_eq!(
                format!("{err:#}"),
                format!("another process held {} for over 20 s", lock_path.display())
            );
        }

        #[tokio::test]
        async fn surfaces_io_error() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("missing").join("test.lock");

            let err = acquire_lock(&lock_path)
                .await
                .err()
                .expect("missing parent must fail");
            let msg = format!("{err:#}");
            assert!(msg.contains("failed to create lock"), "{msg}");
        }
    }

    mod lock_stale_threshold {
        use super::*;

        #[test]
        fn matches_proper_lockfiles_defaults() {
            assert_eq!(LOCK_STALE_THRESHOLD, Duration::from_secs(10));
        }
    }

    mod lock_update_interval {
        use super::*;

        #[test]
        fn matches_proper_lockfiles_defaults() {
            assert_eq!(LOCK_UPDATE_INTERVAL, Duration::from_secs(5));
        }
    }
}
