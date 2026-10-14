//! Session persistence: JSONL transcripts under `$XDG_DATA_HOME/ox/sessions/`, with resume,
//! listing, and background AI titles. [`handle::SessionHandle`] fronts the actor task that owns
//! the file and appends each batch of commands in one write.

mod actor;
mod chain;
pub(crate) mod display;
pub(crate) mod entry;
pub(crate) mod handle;
pub(crate) mod history;
pub(crate) mod list_view;
mod path;
pub(crate) mod resolver;
mod sanitize;
mod state;
pub(crate) mod store;
pub(crate) mod title_generator;
