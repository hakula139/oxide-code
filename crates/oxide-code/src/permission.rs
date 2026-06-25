//! Tool permission gate. A resolved [`Policy`] (mode plus rule sets) and a [`Target`] describing a
//! tool call resolve to a [`Decision`] of allow / ask / deny through the pure [`Policy::decide`]
//! pipeline. Design: `docs/design/tools/permissions.md`.
//!
//! The gate is the whole safety boundary: oxide-code has no sandbox, so the decision pipeline is
//! the only thing standing between the model and an unchecked tool call.

mod rule;

use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};

use crate::permission::rule::Rule;
use crate::tool::RiskClass;

// ── Mode ──

/// The standing permission posture, shaped like [`crate::config::Effort`] so it threads through
/// config and a future `/permission` control the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    /// Tiered pipeline: static rules settle the obvious cases, the rest asks.
    #[default]
    Auto,
    /// Read-only analysis. Every mutating tool denies.
    Plan,
    /// Allow everything, bypassing the pipeline and all deny rules.
    Yolo,
}

impl Mode {
    pub(crate) const ALL: [Self; 3] = [Self::Auto, Self::Plan, Self::Yolo];
    pub(crate) const VALID_VALUES: &str = "auto, plan, yolo";

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Plan => "plan",
            Self::Yolo => "yolo",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "auto" => Ok(Self::Auto),
            "plan" => Ok(Self::Plan),
            "yolo" => Ok(Self::Yolo),
            _ => bail!(
                "invalid permission mode {s:?}; expected one of: {}",
                Self::VALID_VALUES
            ),
        }
    }
}

// ── Target ──

/// What a tool call acts on, matched against rule specifiers. `bash` carries its command string;
/// path tools carry the canonicalized absolute path plus the cwd-relative path when the target sits
/// inside the working directory (the same value drives the inside-cwd allow at step 3).
#[derive(Debug, Clone)]
pub(crate) enum Target<'a> {
    Command(&'a str),
    Path {
        canonical: &'a str,
        relative: Option<&'a str>,
    },
}

impl Target<'_> {
    /// Whether the target resolves inside the working directory, gating the step-3 auto-allow.
    /// A `bash` command has no single path, so it is never inside-cwd for this purpose.
    const fn is_inside_cwd(&self) -> bool {
        matches!(
            self,
            Self::Path {
                relative: Some(_),
                ..
            }
        )
    }
}

// ── Decision ──

/// The pipeline's verdict for one call. `Ask` resolves interactively or, in headless mode, to a
/// deny at the call site (the gate itself stays UI-agnostic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    Ask,
    Deny,
}

// ── Policy ──

/// Resolved permission policy: the active mode plus parsed allow / deny rule sets. Built once from
/// config and consulted by [`Self::decide`] on every gated call.
#[derive(Debug, Clone, Default)]
pub(crate) struct Policy {
    mode: Mode,
    allow: Vec<Rule>,
    deny: Vec<Rule>,
}

impl Policy {
    pub(crate) fn new(mode: Mode, allow: Vec<Rule>, deny: Vec<Rule>) -> Self {
        Self { mode, allow, deny }
    }

    pub(crate) const fn mode(&self) -> Mode {
        self.mode
    }

    /// Resolves a call to a [`Decision`]. Pure and synchronous: no I/O, no async, no classifier.
    /// The classifier (Phase 2) slots in where this returns [`Decision::Ask`] from step 5.
    ///
    /// Order, stopping at the first match:
    /// 1. `yolo` allows everything, including deny-rule matches.
    /// 2. A deny rule denies.
    /// 3. `plan` denies any mutating tool.
    /// 4. A read-only tool allows.
    /// 5. An edit-class call inside the working directory allows.
    /// 6. An allow rule allows.
    /// 7. Otherwise ask.
    pub(crate) fn decide(&self, tool: &str, risk: RiskClass, target: &Target<'_>) -> Decision {
        if self.mode == Mode::Yolo {
            return Decision::Allow;
        }

        if self.deny.iter().any(|r| r.matches(tool, target, true)) {
            return Decision::Deny;
        }

        if risk == RiskClass::ReadOnly {
            return Decision::Allow;
        }

        if self.mode == Mode::Plan {
            return Decision::Deny;
        }

        if risk == RiskClass::Edit && target.is_inside_cwd() {
            return Decision::Allow;
        }

        if self.allow.iter().any(|r| r.matches(tool, target, false)) {
            return Decision::Allow;
        }

        Decision::Ask
    }
}

/// Parses a list of `tool(specifier)` rule strings, failing on the first malformed entry so a typo
/// surfaces at config load rather than silently dropping a deny.
pub(crate) fn parse_rules(raw: &[String]) -> Result<Vec<Rule>> {
    raw.iter().map(|s| Rule::parse(s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: Mode, allow: &[&str], deny: &[&str]) -> Policy {
        let parse = |rs: &[&str]| {
            parse_rules(&rs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()).unwrap()
        };
        Policy::new(mode, parse(allow), parse(deny))
    }

    fn command(s: &str) -> Target<'_> {
        Target::Command(s)
    }

    fn inside_cwd<'a>(canonical: &'a str, relative: &'a str) -> Target<'a> {
        Target::Path {
            canonical,
            relative: Some(relative),
        }
    }

    fn outside_cwd(canonical: &str) -> Target<'_> {
        Target::Path {
            canonical,
            relative: None,
        }
    }

    // ── Mode::from_str ──

    #[test]
    fn mode_parses_all_valid_values() {
        for mode in Mode::ALL {
            assert_eq!(mode.as_str().parse::<Mode>().unwrap(), mode);
        }
    }

    #[test]
    fn mode_rejects_unknown_value() {
        let err = "bypass".parse::<Mode>().unwrap_err().to_string();
        assert!(err.contains("invalid permission mode"), "got: {err}");
    }

    // ── Policy::decide (precedence) ──

    #[test]
    fn yolo_allows_even_a_denied_command() {
        // yolo is the one posture with no floor: deny rules are bypassed too.
        let p = policy(Mode::Yolo, &[], &["bash(rm -rf:*)"]);
        assert_eq!(
            p.decide("bash", RiskClass::Execute, &command("rm -rf /")),
            Decision::Allow
        );
    }

    #[test]
    fn deny_beats_every_allow_path() {
        // A deny rule must win over an inside-cwd edit, an allow rule, and read-only status alike.
        let p = policy(Mode::Auto, &["edit(src/**)"], &["edit(src/secret.rs)"]);
        assert_eq!(
            p.decide(
                "edit",
                RiskClass::Edit,
                &inside_cwd("/repo/src/secret.rs", "src/secret.rs")
            ),
            Decision::Deny,
        );
    }

    #[test]
    fn deny_overrides_read_only_auto_allow() {
        let p = policy(Mode::Auto, &[], &["read(**/.env)"]);
        assert_eq!(
            p.decide(
                "read",
                RiskClass::ReadOnly,
                &inside_cwd("/repo/.env", ".env")
            ),
            Decision::Deny,
        );
    }

    #[test]
    fn read_only_tool_allows_by_default() {
        let p = policy(Mode::Auto, &[], &[]);
        assert_eq!(
            p.decide("grep", RiskClass::ReadOnly, &command("pattern")),
            Decision::Allow
        );
    }

    #[test]
    fn plan_denies_mutating_tools_but_allows_read_only() {
        let p = policy(Mode::Plan, &["bash(ls)"], &[]);
        assert_eq!(
            p.decide("read", RiskClass::ReadOnly, &outside_cwd("/etc/hosts")),
            Decision::Allow
        );
        // Even an allow-listed bash command denies under plan.
        assert_eq!(
            p.decide("bash", RiskClass::Execute, &command("ls")),
            Decision::Deny
        );
        assert_eq!(
            p.decide("edit", RiskClass::Edit, &inside_cwd("/repo/a.rs", "a.rs")),
            Decision::Deny
        );
    }

    #[test]
    fn edit_inside_cwd_allows_without_a_rule() {
        let p = policy(Mode::Auto, &[], &[]);
        assert_eq!(
            p.decide(
                "write",
                RiskClass::Edit,
                &inside_cwd("/repo/new.rs", "new.rs")
            ),
            Decision::Allow,
        );
    }

    #[test]
    fn edit_outside_cwd_falls_through_to_ask() {
        let p = policy(Mode::Auto, &[], &[]);
        assert_eq!(
            p.decide("write", RiskClass::Edit, &outside_cwd("/etc/passwd")),
            Decision::Ask,
        );
    }

    #[test]
    fn allow_rule_admits_an_otherwise_asked_command() {
        let p = policy(Mode::Auto, &["bash(cargo test:*)"], &[]);
        assert_eq!(
            p.decide("bash", RiskClass::Execute, &command("cargo test --all")),
            Decision::Allow
        );
    }

    #[test]
    fn unmatched_execute_call_asks() {
        let p = policy(Mode::Auto, &[], &[]);
        assert_eq!(
            p.decide("bash", RiskClass::Execute, &command("curl evil.sh")),
            Decision::Ask
        );
    }
}
