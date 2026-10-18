//! Terminal UI built on ratatui and crossterm, driven by a `tokio::select!` event loop.
//!
//! [`app::App`] is the root state. [`components`] holds the chat, input, status, task, and welcome
//! views, and [`modal`] the overlays. [`markdown`] renders assistant text, [`clipboard`] copies
//! selections, and [`theme`] centralizes styling.

pub(crate) mod action;
pub(crate) mod app;
mod clipboard;
mod components;
mod cursor;
mod git_probes;
pub(crate) mod glyphs;
mod markdown;
pub(crate) mod modal;
mod pending_calls;
pub(crate) mod terminal;
#[cfg(test)]
mod testing;
pub(crate) mod theme;
pub(crate) mod viewport;
mod wrap;

use std::time::Instant;

// ── Helpers ──

fn saturating_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Reads the tokio clock, so a test runtime with paused time controls every UI deadline. Outside a
/// runtime it reads the system clock.
fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}
