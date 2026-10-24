//! Acceptance of a displayed modal after the input burst it interrupted has ended.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crossterm::event::{KeyEvent, KeyEventKind};

use crate::now;

/// Quiet period after a target appears or a buffered key arrives.
pub(crate) const TYPE_AHEAD_GUARD: Duration = Duration::from_millis(300);

pub(crate) struct Acceptance {
    shown: Cell<bool>,
    guard_until: Cell<Instant>,
}

impl Default for Acceptance {
    fn default() -> Self {
        Self {
            shown: Cell::new(false),
            guard_until: Cell::new(now() + TYPE_AHEAD_GUARD),
        }
    }
}

impl Acceptance {
    /// Records whether the current frame shows the target, rearming when it appears.
    pub(crate) fn update(&self, shown: bool) {
        if shown && !self.shown.get() {
            self.restart();
        }
        self.shown.set(shown);
    }

    /// Invalidates the previous view when the modal changes its target or editing step.
    pub(crate) fn reset(&self) {
        self.shown.set(false);
        self.restart();
    }

    pub(crate) fn is_guarded(&self) -> bool {
        now() < self.guard_until.get()
    }

    /// Drops buffered keys, extending the quiet period through the input burst.
    pub(crate) fn guard(&self) -> bool {
        if self.is_guarded() {
            self.restart();
            return true;
        }
        false
    }

    pub(crate) fn accepts(&self, event: &KeyEvent) -> bool {
        if event.kind != KeyEventKind::Press {
            self.restart();
            return false;
        }
        !self.guard() && self.shown.get()
    }

    fn restart(&self) {
        self.guard_until.set(now() + TYPE_AHEAD_GUARD);
    }

    #[cfg(test)]
    pub(crate) fn expire(&self) {
        self.guard_until.set(now());
    }
}
