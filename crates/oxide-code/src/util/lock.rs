//! Async retry helper for advisory file locks.

use std::time::Duration;

use anyhow::Result;

/// Total attempts = `1 + MAX_RETRIES`; budget bounds the worst-case stall when another oxide
/// process holds the credentials lock.
pub(crate) const MAX_RETRIES: u32 = 5;

#[cfg(not(test))]
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Retries `try_once` with fixed-interval backoff. `Ok(Some)` = acquired,
/// `Ok(None)` = contended (retries), `Err` = fatal (propagated immediately).
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
    async fn retry_acquire_succeeds_immediately() {
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
    async fn retry_acquire_errors_with_contention_after_exhausting_retries() {
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
