//! Literal shell commands for explicit exec permission rules.

mod pattern;
mod walk;
mod word;

use self::walk::{Walk, basename};

// ── Command Target ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandTarget {
    segments: Vec<Segment>,
    complete: bool,
}

impl CommandTarget {
    pub(crate) fn parse(command: &str) -> Self {
        let mut walk = Walk::default();
        walk.script(command);
        Self {
            segments: walk.segments,
            complete: !walk.incomplete,
        }
    }

    pub(super) fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub(super) const fn is_complete(&self) -> bool {
        self.complete
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Segment {
    named: Vec<ShellWord>,
    program_start: usize,
}

impl Segment {
    pub(super) fn named(&self) -> &[ShellWord] {
        &self.named
    }

    pub(super) fn program(&self) -> &[ShellWord] {
        &self.named[self.program_start..]
    }
}

/// Drops a program's directory for deny and ask matching.
pub(super) fn bare_program(program: &[ShellWord]) -> Option<Vec<ShellWord>> {
    let (name, args) = program.split_first()?;
    let bare = basename(&name.text);
    (bare != name.text).then(|| {
        std::iter::once(ShellWord {
            text: bare.to_owned(),
            literal: name.literal,
        })
        .chain(args.iter().cloned())
        .collect()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellWord {
    text: String,
    literal: bool,
}

impl ShellWord {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) const fn is_literal(&self) -> bool {
        self.literal
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PatternWord {
    Literal(String),
    Any,
}

pub(super) fn pattern_words(pattern: &str) -> anyhow::Result<Vec<PatternWord>> {
    self::pattern::parse(pattern)
}
