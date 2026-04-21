use std::time::Duration;

use anyhow::Result;

/// Retry budget for advisory locks in this crate. Currently used by
/// the OAuth credential lock ([`crate::config::oauth`]); session files
/// no longer take a flock (concurrent resume is supported via the UUID
/// DAG instead, see [`crate::session::store::SessionStore::open_append`]).
/// Kept here so future lock-acquisition call sites stay uniform.
pub(crate) const MAX_RETRIES: u32 = 5;

/// Sleep duration between successive lock-acquisition attempts.
/// Shortened under `cfg(test)` so contention tests do not block CI
/// for seconds per run.
#[cfg(not(test))]
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Retries an advisory-lock acquisition with fixed-interval backoff.
///
/// `try_once` must return:
/// - `Ok(Some(value))` when the lock was acquired (returned to the
///   caller verbatim),
/// - `Ok(None)` when the lock is contended (the helper sleeps for
///   `interval` and tries again while the budget allows), or
/// - `Err(_)` for a genuine I/O failure that is not contention
///   (propagated immediately).
///
/// After `max_retries` contended attempts, `contention_err` is
/// invoked to produce the "all attempts exhausted" error.
///
/// Sleeps are `tokio::time::sleep`, so this must be called from an
/// async context. Matching the executor's clock (not `std::thread::sleep`)
/// keeps worker threads free to service other tasks during contention.
pub(crate) async fn retry_acquire<T, F, E>(
    mut try_once: F,
    max_retries: u32,
    interval: Duration,
    contention_err: E,
) -> Result<T>
where
    F: FnMut() -> Result<Option<T>>,
    E: FnOnce() -> anyhow::Error,
{
    for attempt in 0..=max_retries {
        match try_once()? {
            Some(value) => return Ok(value),
            None if attempt < max_retries => tokio::time::sleep(interval).await,
            None => return Err(contention_err()),
        }
    }
    unreachable!("loop body returns on every path")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use anyhow::anyhow;

    use super::*;

    // ── retry_acquire ──

    #[tokio::test]
    async fn retry_acquire_returns_immediately_on_success() {
        let result = retry_acquire(
            || Ok(Some(42u32)),
            3,
            Duration::from_millis(1),
            || anyhow!("unreachable"),
        )
        .await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn retry_acquire_retries_until_success_within_budget() {
        let calls = AtomicU32::new(0);
        let result = retry_acquire(
            || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                // First two calls contend, the third acquires.
                Ok(if n < 2 { None } else { Some("got it") })
            },
            3,
            Duration::from_millis(1),
            || anyhow!("should not be reached"),
        )
        .await;
        assert_eq!(result.unwrap(), "got it");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_acquire_returns_contention_err_after_exhausting_retries() {
        let calls = AtomicU32::new(0);
        let result: Result<()> = retry_acquire(
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            },
            2,
            Duration::from_millis(1),
            || anyhow!("contended"),
        )
        .await;
        // 1 initial + 2 retries = 3 attempts.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(result.unwrap_err().to_string(), "contended");
    }

    #[tokio::test]
    async fn retry_acquire_propagates_fatal_errors_without_retrying() {
        let calls = AtomicU32::new(0);
        let result: Result<()> = retry_acquire(
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(anyhow!("EIO"))
            },
            5,
            Duration::from_millis(1),
            || anyhow!("unreachable"),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.unwrap_err().to_string(), "EIO");
    }
}
