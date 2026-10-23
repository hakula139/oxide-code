//! Shared utilities: env var lookup, private file writes, git probes, tracing init, path display,
//! server error bodies, mutex recovery, text width, the local-offset cache, extra TLS trust
//! anchors, TOML errors that omit the source, and enum variant names for messages.

pub mod env;
pub(crate) mod fs;
pub mod git;
pub mod log;
pub mod path;
pub(crate) mod response;
pub mod sync;
pub mod text;
pub mod time;
pub mod tls;
pub mod toml;
pub(crate) mod variant;
