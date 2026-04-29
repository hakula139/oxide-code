//! Session persistence.
//!
//! JSONL-based conversation logs under `$XDG_DATA_HOME/ox/sessions/`,
//! with resume, listing, fork-friendly concurrency, and background
//! AI title generation. The lifecycle entry point is
//! [`handle::SessionHandle`] — a cheap-to-clone handle in front of a
//! [`actor::run`] task that owns the file and absorbs each batch's
//! cmds in one buffered flush. `agent_turn` joins a tool round's
//! three writes (assistant message, tool-result message, sidecar
//! batch) through `tokio::join!` so they queue together and the
//! actor's drain collapses them into one flush.

mod actor;
mod chain;
mod entry;
pub(crate) mod handle;
pub(crate) mod history;
pub(crate) mod list_view;
mod path;
pub(crate) mod resolver;
mod sanitize;
mod state;
pub(crate) mod store;
pub(crate) mod title_generator;
