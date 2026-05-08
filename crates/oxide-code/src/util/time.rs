//! Process-wide local-offset cache. `time::UtcOffset::current_local_offset` is documented as
//! unsound on multi-threaded Unix runtimes (calling `localtime_r` after another thread has
//! spawned can race the TZ database). Resolve once before the tokio runtime starts and read it
//! everywhere else.

use std::sync::OnceLock;

use time::UtcOffset;

static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// Captures the local offset on the calling thread. Safe only before tokio spawns workers; the
/// binary calls this once at startup.
pub(crate) fn init_local_offset() {
    _ = LOCAL_OFFSET.set(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
}

/// Returns the cached local offset, falling back to UTC if `init_local_offset` was never called
/// (test paths and helper binaries).
pub(crate) fn local_offset() -> UtcOffset {
    LOCAL_OFFSET.get().copied().unwrap_or(UtcOffset::UTC)
}
