//! Permission rule grammar: `tool(specifier)` strings parsed into matchable rules.
//!
//! The form mirrors Claude Code's for transferable muscle memory. A rule names a tool
//! (case-insensitive) and an optional specifier. `bash` specifiers match the command string in
//! exact, prefix (`cargo test:*`), or wildcard (`git *`) shapes; every other tool's specifier is a
//! gitignore-style path glob. A bare tool name, `tool()`, or `tool(*)` is tool-wide.
//!
//! Because a `bash` command is an unparsed string, matching is best-effort UX rather than a
//! security boundary (see `docs/design/tools/permissions.md`). The asymmetry is deliberate: an
//! allow rule refuses to match a compound command, while a deny rule matches any segment of one, so
//! widening stays conservative and revoking stays aggressive.

use anyhow::{Context, Result};
use globset::{Glob, GlobMatcher};
use regex::Regex;

use crate::permission::Target;

// ── Rule ──

/// One parsed permission rule. `tool` is lowercased at parse time so matching is case-insensitive.
#[derive(Debug, Clone)]
pub(crate) struct Rule {
    tool: String,
    spec: Spec,
}

/// The matchable body of a rule, chosen by the rule's tool at parse time.
#[derive(Debug, Clone)]
enum Spec {
    /// Tool-wide: a bare name, `tool()`, or `tool(*)`. Matches every call to the tool.
    Any,
    Bash(BashSpec),
    /// Gitignore-style path glob for `edit` / `write` / `read` / `glob` / `grep`.
    Path(GlobMatcher),
}

/// How a `bash` specifier matches a command string.
#[derive(Debug, Clone)]
enum BashSpec {
    /// `cargo build`: the command must equal this exactly.
    Exact(String),
    /// `cargo test:*`: the command must start with this prefix.
    Prefix(String),
    /// `git *`: glob over the command, compiled to an anchored regex.
    Wildcard(Regex),
}

impl Rule {
    /// Parses a `tool(specifier)` string. The first unescaped `(` opens the specifier and the
    /// trailing `)` closes it; everything else is a bare tool name. Path globs and wildcard regexes
    /// compile here so a malformed rule fails at config load rather than mid-turn.
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let (tool, spec_str) = match (raw.find('('), raw.strip_suffix(')')) {
            (Some(open), Some(_)) => (&raw[..open], &raw[open + 1..raw.len() - 1]),
            _ => (raw, ""),
        };

        let tool = tool.trim().to_lowercase();
        anyhow::ensure!(!tool.is_empty(), "permission rule {raw:?} has no tool name");

        let spec_str = spec_str.trim();
        let spec = if spec_str.is_empty() || spec_str == "*" {
            Spec::Any
        } else if tool == "bash" {
            BashSpec::parse(spec_str)
        } else {
            let glob = Glob::new(spec_str)
                .with_context(|| format!("invalid path glob in permission rule {raw:?}"))?;
            Spec::Path(glob.compile_matcher())
        };

        Ok(Self { tool, spec })
    }

    /// Whether this rule matches a call to `tool` with `target`. `deny` selects the matching
    /// discipline for compound `bash` commands: a deny rule matches any chained segment, an allow
    /// rule matches only a single non-compound command.
    pub(crate) fn matches(&self, tool: &str, target: &Target<'_>, deny: bool) -> bool {
        if self.tool != tool {
            return false;
        }
        match (&self.spec, target) {
            (Spec::Any, _) => true,
            (Spec::Bash(spec), Target::Command(cmd)) => spec.matches(cmd, deny),
            (
                Spec::Path(glob),
                Target::Path {
                    canonical,
                    relative,
                },
            ) => glob.is_match(canonical) || relative.is_some_and(|r| glob.is_match(r)),
            _ => false,
        }
    }
}

impl BashSpec {
    fn parse(spec: &str) -> Spec {
        if let Some(prefix) = spec.strip_suffix(":*") {
            Spec::Bash(Self::Prefix(prefix.trim_end().to_owned()))
        } else if spec.contains('*') {
            // An unparsable glob can never compile here: `glob_to_regex` only emits valid syntax.
            Spec::Bash(Self::Wildcard(glob_to_regex(spec)))
        } else {
            Spec::Bash(Self::Exact(spec.to_owned()))
        }
    }

    fn matches(&self, command: &str, deny: bool) -> bool {
        if deny {
            return split_segments(command).any(|seg| self.matches_segment(seg));
        }
        // Allow rules never widen a compound command: a single chained operator forfeits the match.
        !is_compound(command) && self.matches_segment(command.trim())
    }

    fn matches_segment(&self, segment: &str) -> bool {
        match self {
            Self::Exact(s) => segment == s,
            Self::Prefix(p) => segment == p || segment.starts_with(&format!("{p} ")),
            Self::Wildcard(re) => re.is_match(segment),
        }
    }
}

// ── Bash Command Helpers ──

/// Shell operators that chain one command into another. Used to split a command for deny matching
/// and to reject compound commands for allow matching.
const CHAIN_CHARS: [char; 4] = [';', '|', '&', '\n'];

/// Whether a command chains, pipes, redirects, or substitutes into another command. Best-effort:
/// it is the gate against an allow rule silently widening `cargo test` to `cargo test; rm -rf /`,
/// not a parser.
fn is_compound(command: &str) -> bool {
    command.contains(CHAIN_CHARS) || command.contains("$(") || command.contains('`')
}

/// Splits a command on chain operators into trimmed, non-empty segments. `&&` and `||` split on
/// their single chars and the empty halves drop out, so `a && b` yields `a`, `b`.
fn split_segments(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(CHAIN_CHARS)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Converts a bash wildcard specifier to an anchored regex: literal text is escaped and `*` becomes
/// `.*`, so `git *` matches `git status` but not `cargo gitx`.
fn glob_to_regex(glob: &str) -> Regex {
    let mut pattern = String::with_capacity(glob.len() + 4);
    pattern.push('^');
    for part in glob.split('*') {
        pattern.push_str(&regex::escape(part));
        pattern.push_str(".*");
    }
    // Each segment appended a trailing `.*`; drop the final one so the regex ends at the last
    // literal unless the glob itself ended in `*`.
    pattern.truncate(pattern.len() - 2);
    pattern.push('$');
    Regex::new(&pattern).expect("escaped glob is always valid regex")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Target<'_> {
        Target::Command(s)
    }

    fn path<'a>(canonical: &'a str, relative: Option<&'a str>) -> Target<'a> {
        Target::Path {
            canonical,
            relative,
        }
    }

    // ── Rule::parse ──

    #[test]
    fn parse_bare_tool_name_is_tool_wide() {
        let rule = Rule::parse("bash").unwrap();
        assert!(rule.matches("bash", &cmd("anything; rm -rf /"), false));
    }

    #[test]
    fn parse_empty_and_star_specifiers_are_tool_wide() {
        for raw in ["bash()", "bash(*)"] {
            let rule = Rule::parse(raw).unwrap();
            assert!(rule.matches("bash", &cmd("ls"), false), "{raw}");
        }
    }

    #[test]
    fn parse_lowercases_tool_name_for_case_insensitive_match() {
        let rule = Rule::parse("Bash(ls)").unwrap();
        assert!(rule.matches("bash", &cmd("ls"), false));
    }

    #[test]
    fn parse_rejects_empty_tool_name() {
        let err = Rule::parse("(ls)").unwrap_err().to_string();
        assert!(err.contains("no tool name"), "got: {err}");
    }

    #[test]
    fn parse_rejects_malformed_path_glob() {
        let err = Rule::parse("edit(src/**/[)").unwrap_err().to_string();
        assert!(err.contains("invalid path glob"), "got: {err}");
    }

    // ── BashSpec::matches ──

    #[test]
    fn bash_exact_matches_only_identical_command() {
        let rule = Rule::parse("bash(cargo build)").unwrap();
        assert!(rule.matches("bash", &cmd("cargo build"), false));
        assert!(!rule.matches("bash", &cmd("cargo build --release"), false));
    }

    #[test]
    fn bash_prefix_matches_command_and_its_arguments() {
        let rule = Rule::parse("bash(cargo test:*)").unwrap();
        assert!(rule.matches("bash", &cmd("cargo test"), false));
        assert!(rule.matches("bash", &cmd("cargo test --all"), false));
        // A different command that merely starts with the same letters must not match.
        assert!(!rule.matches("bash", &cmd("cargo testbench"), false));
    }

    #[test]
    fn bash_wildcard_anchors_both_ends() {
        let rule = Rule::parse("bash(git *)").unwrap();
        assert!(rule.matches("bash", &cmd("git status"), false));
        assert!(!rule.matches("bash", &cmd("cargo gitx"), false));
    }

    #[test]
    fn allow_prefix_refuses_compound_command() {
        // The load-bearing safety property: `cargo test:*` must not allow a chained `rm`.
        let rule = Rule::parse("bash(cargo test:*)").unwrap();
        assert!(!rule.matches("bash", &cmd("cargo test && rm -rf /"), false));
        assert!(!rule.matches("bash", &cmd("cargo test; rm -rf /"), false));
        assert!(!rule.matches("bash", &cmd("cargo test | tee out"), false));
        assert!(!rule.matches("bash", &cmd("cargo test $(rm -rf /)"), false));
    }

    #[test]
    fn deny_prefix_matches_any_segment_of_compound_command() {
        // The mirror property: a deny must fire even when the danger is chained behind a safe head.
        let rule = Rule::parse("bash(rm -rf:*)").unwrap();
        assert!(rule.matches("bash", &cmd("rm -rf /"), true));
        assert!(rule.matches("bash", &cmd("ls && rm -rf /tmp/x"), true));
        assert!(rule.matches("bash", &cmd("echo hi; rm -rf ."), true));
    }

    // ── Rule::matches (path) ──

    #[test]
    fn relative_path_glob_matches_the_relative_target() {
        // The shipped `.git/**` deny default protects the project's own `.git`, which is inside cwd
        // and therefore carries a relative path.
        let rule = Rule::parse("write(.git/**)").unwrap();
        assert!(rule.matches(
            "write",
            &path("/repo/.git/hooks/pre-commit", Some(".git/hooks/pre-commit")),
            true
        ));
        assert!(!rule.matches(
            "write",
            &path("/repo/src/main.rs", Some("src/main.rs")),
            true
        ));
    }

    #[test]
    fn relative_path_glob_does_not_match_an_out_of_cwd_absolute_path() {
        // A cwd-relative glob must not match an absolute path that resolved outside the working
        // directory; such targets fall through to ask rather than to a relative rule.
        let rule = Rule::parse("write(.git/**)").unwrap();
        assert!(!rule.matches("write", &path("/elsewhere/.git/config", None), true));
    }

    #[test]
    fn absolute_path_glob_matches_the_canonical_target() {
        // An absolute glob (e.g. a `~`-expanded rule) matches the canonical path even with no
        // relative component.
        let rule = Rule::parse("read(/etc/**)").unwrap();
        assert!(rule.matches("read", &path("/etc/passwd", None), false));
        assert!(!rule.matches("read", &path("/home/u/.config", None), false));
    }

    #[test]
    fn path_recursive_glob_spans_directories() {
        let rule = Rule::parse("edit(src/**)").unwrap();
        assert!(rule.matches("edit", &path("/repo/src/a/b.rs", Some("src/a/b.rs")), false));
    }

    #[test]
    fn rule_does_not_match_other_tools() {
        let rule = Rule::parse("bash(ls)").unwrap();
        assert!(!rule.matches("edit", &cmd("ls"), false));
    }

    #[test]
    fn bash_rule_ignores_path_target_and_vice_versa() {
        let bash_rule = Rule::parse("bash(ls)").unwrap();
        assert!(!bash_rule.matches("bash", &path("/x", None), false));

        let path_rule = Rule::parse("edit(src/**)").unwrap();
        assert!(!path_rule.matches("edit", &cmd("src/a"), false));
    }

    // ── glob_to_regex ──

    #[test]
    fn glob_to_regex_escapes_literals_and_expands_star() {
        // `.` is a regex metachar; it must stay literal so `a.b *` doesn't match `axb c`.
        let re = glob_to_regex("a.b *");
        assert!(re.is_match("a.b c"));
        assert!(!re.is_match("axb c"));
    }

    #[test]
    fn glob_to_regex_trailing_star_matches_any_suffix() {
        let re = glob_to_regex("git *");
        assert!(re.is_match("git commit -m x"));
    }

    // ── split_segments / is_compound ──

    #[test]
    fn split_segments_drops_empty_halves_of_double_operators() {
        let segs: Vec<_> = split_segments("a && b || c").collect();
        assert_eq!(segs, ["a", "b", "c"]);
    }

    #[test]
    fn is_compound_flags_chains_pipes_and_substitution() {
        for c in ["a; b", "a && b", "a | b", "a\nb", "a $(b)", "a `b`"] {
            assert!(is_compound(c), "{c:?} should be compound");
        }
        assert!(!is_compound("cargo test --all"));
    }
}
