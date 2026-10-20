//! Session persistence: JSONL transcripts under `$XDG_DATA_HOME/ox/sessions/`, with resume,
//! listing, and background AI titles. [`handle::SessionHandle`] fronts the actor task that owns
//! the file and appends each batch of commands in one write.

mod actor;
mod chain;
pub mod display;
pub mod entry;
pub mod handle;
pub(crate) mod history;
pub mod list_view;
mod path;
pub(crate) mod post_compact;
pub mod resolver;
mod sanitize;
pub mod saved;
mod state;
pub mod store;
pub(crate) mod title_generator;
