//! Local time, through the process-wide UTC offset resolved once at startup.

use std::sync::OnceLock;

use time::error::IndeterminateOffset;
use time::{OffsetDateTime, UtcOffset};

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// Caches the offset [`local_offset`] returns, falling back to UTC and returning the error when
/// the host's offset is indeterminate. The binary calls this before the tokio runtime starts,
/// since the lookup's `localtime_r` reads `TZ`, which a concurrent environment write can race on
/// Unix systems other than macOS, illumos, and NetBSD.
pub fn init_local_offset() -> Result<(), IndeterminateOffset> {
    let offset = UtcOffset::current_local_offset();
    _ = LOCAL_OFFSET.set(offset.unwrap_or(UtcOffset::UTC));
    offset.map(|_| ())
}

/// The cached local offset, or UTC when [`init_local_offset`] never ran, as in tests.
pub fn local_offset() -> UtcOffset {
    LOCAL_OFFSET.get().copied().unwrap_or(UtcOffset::UTC)
}

/// The current time at [`local_offset`].
pub fn now_local() -> OffsetDateTime {
    OffsetDateTime::now_utc().to_offset(local_offset())
}
