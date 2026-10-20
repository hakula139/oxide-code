//! Shared utilities: env var lookup, private file writes, git probes, tracing init, path display,
//! mutex recovery, text width, the local-offset cache, and extra TLS trust anchors.

pub mod env;
pub(crate) mod fs;
pub mod git;
pub mod log;
pub mod path;
pub mod sync;
pub mod text;
pub mod time;
pub mod tls;
