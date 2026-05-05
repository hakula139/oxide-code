//! Terminal UI.
//!
//! ratatui + crossterm with a `tokio::select!` event loop. [`app::App`]
//! is the root state; [`components`] owns the chat / input / status
//! regions; [`markdown`] renders assistant text; [`theme`] centralizes all styling.

pub(crate) mod app;
pub(crate) mod components;
pub(crate) mod event;
pub(crate) mod glyphs;
pub(crate) mod markdown;
pub(crate) mod modal;
pub(crate) mod pending_calls;
pub(crate) mod terminal;
pub(crate) mod theme;
pub(crate) mod wrap;
