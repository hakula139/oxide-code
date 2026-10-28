//! Session persistence: one SQLite store at `$XDG_DATA_HOME/ox/sessions.db`, with resume, listing,
//! JSONL export and import, and background AI titles. [`handle::SessionHandle`] fronts the actor
//! task that owns the session's state and commits each batch of commands as one transaction.

mod actor;
pub mod display;
pub mod handle;
pub mod resolver;
mod sanitize;
pub mod saved;
mod state;
pub mod store;
pub(crate) mod title_generator;
