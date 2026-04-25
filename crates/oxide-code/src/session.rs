//! Session persistence.
//!
//! JSONL-based conversation logs under `$XDG_DATA_HOME/ox/sessions/`,
//! with resume, listing, fork-friendly concurrency, and background
//! AI title generation. See [`manager`] for the lifecycle entry point.

mod chain;
mod entry;
pub(crate) mod history;
pub(crate) mod list_view;
pub(crate) mod manager;
mod path;
pub(crate) mod resolver;
mod sanitize;
pub(crate) mod store;
pub(crate) mod title_generator;
pub(crate) mod writer;
