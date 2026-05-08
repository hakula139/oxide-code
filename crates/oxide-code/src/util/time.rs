//! Process-wide local-offset cache. `time::UtcOffset::current_local_offset` is documented as
//! unsound on multi-threaded Unix runtimes (calling `localtime_r` after another thread has
//! spawned can race the TZ database). Resolve once before the tokio runtime starts and read it
//! everywhere else.

use std::sync::OnceLock;

use time::UtcOffset;
use tracing::warn;

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// Captures the local offset on the calling thread. Safe only before tokio spawns workers; the
/// binary calls this once at startup.
pub(crate) fn init_local_offset() {
    _ = LOCAL_OFFSET.set(resolve_offset(UtcOffset::current_local_offset()));
}

/// Pure fallback for the offset lookup — split from `init_local_offset` so the warn-and-fallback
/// arm can be tested without depending on the host's TZ database state.
fn resolve_offset(result: Result<UtcOffset, time::error::IndeterminateOffset>) -> UtcOffset {
    result.unwrap_or_else(|e| {
        warn!("cannot read local timezone offset, falling back to UTC: {e}");
        UtcOffset::UTC
    })
}

/// Returns the cached local offset, falling back to UTC if `init_local_offset` was never called
/// (test paths and helper binaries).
pub(crate) fn local_offset() -> UtcOffset {
    LOCAL_OFFSET.get().copied().unwrap_or(UtcOffset::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── init_local_offset / local_offset ──

    #[test]
    fn init_is_idempotent_and_local_offset_returns_a_stable_cached_value() {
        // OnceLock is process-global, so we can only assert the contract that holds regardless of
        // OnceLock state: init must not panic when called more than once, and `local_offset` must
        // return the same value across repeat reads.
        init_local_offset();
        init_local_offset();
        let first = local_offset();
        let second = local_offset();
        assert_eq!(
            first, second,
            "local_offset must return a stable cached value across reads",
        );
    }

    // ── resolve_offset ──

    #[test]
    fn resolve_offset_passes_ok_through_unchanged() {
        let offset = UtcOffset::from_hms(8, 0, 0).unwrap();
        assert_eq!(resolve_offset(Ok(offset)), offset);
    }

    #[test]
    fn resolve_offset_falls_back_to_utc_on_err() {
        // `IndeterminateOffset` is the only error variant — used here as a stand-in for any host
        // TZ-database failure. The fallback path must not propagate the error.
        let err = Err(time::error::IndeterminateOffset);
        assert_eq!(resolve_offset(err), UtcOffset::UTC);
    }
}
