//! Process-wide local UTC offset, resolved once at startup.

use std::sync::OnceLock;

use time::UtcOffset;
use tracing::warn;

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// Resolves the offset [`local_offset`] returns. The binary calls this once before the tokio
/// runtime starts, since on Unix the lookup's `localtime_r` reads `TZ` from the environment and
/// races any other thread that writes it.
pub(crate) fn init_local_offset() {
    _ = LOCAL_OFFSET.set(resolve_offset(UtcOffset::current_local_offset()));
}

fn resolve_offset(result: Result<UtcOffset, time::error::IndeterminateOffset>) -> UtcOffset {
    result.unwrap_or_else(|e| {
        warn!("cannot read local timezone offset, falling back to UTC: {e}");
        UtcOffset::UTC
    })
}

/// The cached local offset, or UTC when [`init_local_offset`] never ran, as in tests.
pub(crate) fn local_offset() -> UtcOffset {
    LOCAL_OFFSET.get().copied().unwrap_or(UtcOffset::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_offset ──

    #[test]
    fn resolve_offset_keeps_a_determined_offset() {
        let offset = UtcOffset::from_hms(8, 0, 0).unwrap();

        assert_eq!(resolve_offset(Ok(offset)), offset);
    }

    #[test]
    fn resolve_offset_falls_back_to_utc_when_the_offset_is_indeterminate() {
        assert_eq!(
            resolve_offset(Err(time::error::IndeterminateOffset)),
            UtcOffset::UTC,
        );
    }
}
