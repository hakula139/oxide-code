//! Terminal UI.
//!
//! ratatui + crossterm with a `tokio::select!` event loop. [`app::App`] is the root state.
//! [`components`] owns the chat / input / status regions. [`markdown`] renders assistant text.
//! [`theme`] centralizes all styling.

pub(crate) mod action;
pub(crate) mod app;
mod components;
mod cursor;
mod git_probes;
pub(crate) mod glyphs;
mod markdown;
pub(crate) mod modal;
mod pending_calls;
pub(crate) mod terminal;
pub(crate) mod theme;
pub(crate) mod viewport;
mod wrap;

// ── Helpers ──

fn saturating_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}
