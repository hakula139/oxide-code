//! Shared utilities: env var lookup, private file writes, git probes, tracing init, path display,
//! text width, the local-offset cache, and extra TLS trust anchors.

pub(crate) mod env;
pub(crate) mod fs;
pub(crate) mod git;
pub(crate) mod log;
pub(crate) mod path;
pub(crate) mod text;
pub(crate) mod time;
pub(crate) mod tls;
