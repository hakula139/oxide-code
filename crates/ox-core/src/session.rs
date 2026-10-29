//! Session persistence: one SQLite store at `$XDG_DATA_HOME/ox/sessions.db`, with resume, listing,
//! JSONL export and import, and background AI titles. [`handle::SessionHandle`] fronts the
//! session's state and commits each write as one transaction under the state's lock.

pub mod display;
pub mod handle;
pub mod resolver;
mod sanitize;
pub mod saved;
mod state;
pub mod store;
pub(crate) mod title_generator;
