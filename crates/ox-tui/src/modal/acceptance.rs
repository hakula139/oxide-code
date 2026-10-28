//! Acceptance of a displayed modal after the input burst it interrupted has ended.

use std::time::{Duration, Instant};

use crossterm::event::{KeyEvent, KeyEventKind};

use crate::now;

/// Quiet period after a target appears or a buffered key arrives.
pub(crate) const TYPE_AHEAD_GUARD: Duration = Duration::from_millis(300);

pub(crate) struct Acceptance {
    shown: bool,
    guard_until: Instant,
}

impl Default for Acceptance {
    fn default() -> Self {
        Self {
            shown: false,
            guard_until: now() + TYPE_AHEAD_GUARD,
        }
    }
}

impl Acceptance {
    /// Records whether the current frame shows the target, rearming when it appears.
    pub(crate) fn update(&mut self, shown: bool) {
        if shown && !self.shown {
            self.restart();
        }
        self.shown = shown;
    }

    /// Invalidates the previous view when the modal changes its target or editing step.
    pub(crate) fn reset(&mut self) {
        self.shown = false;
        self.restart();
    }

    pub(crate) fn is_guarded(&self) -> bool {
        now() < self.guard_until
    }

    /// Drops buffered keys, extending the quiet period through the input burst.
    pub(crate) fn guard(&mut self) -> bool {
        if self.is_guarded() {
            self.restart();
            return true;
        }
        false
    }

    pub(crate) fn accepts(&mut self, event: &KeyEvent) -> bool {
        if event.kind != KeyEventKind::Press {
            self.restart();
            return false;
        }
        !self.guard() && self.shown
    }

    fn restart(&mut self) {
        self.guard_until = now() + TYPE_AHEAD_GUARD;
    }

    #[cfg(test)]
    pub(crate) fn expire(&mut self) {
        self.guard_until = now();
    }
}
