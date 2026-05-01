//! `/diff` — show uncommitted git changes inline.
//!
//! Runs `git diff HEAD` (or `git diff --cached` in a fresh repo with
//! no commits yet) and appends the names of untracked files that
//! aren't gitignored. Output is capped so a runaway diff can't lock
//! the render loop.

use std::fmt::Write as _;
use std::process::{Command, Stdio};

use super::context::SlashContext;
use super::registry::SlashCommand;

/// Cap diff output so a 1 GB binary diff can't freeze rendering.
/// Sized for ~1300 lines at 50 chars — comfortably above a typical
/// PR-sized review.
const MAX_BYTES: usize = 64 * 1024;

pub(crate) struct Diff;

impl SlashCommand for Diff {
    fn name(&self) -> &'static str {
        "diff"
    }

    fn description(&self) -> &'static str {
        "show uncommitted git changes"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) {
        match collect_diff() {
            Ok(text) if text.trim().is_empty() => {
                ctx.chat.push_system_message("No uncommitted changes.");
            }
            Ok(text) => ctx.chat.push_system_message(text),
            Err(e) => ctx.chat.push_error(&format!("/diff: {e}")),
        }
    }
}

/// Gathers tracked + untracked diff text. Falls back to
/// `git diff --cached` when HEAD doesn't resolve (fresh repo) so the
/// command still produces useful output before the first commit.
fn collect_diff() -> Result<String, String> {
    if !inside_git_repo() {
        return Err("not inside a git repository".to_owned());
    }

    let tracked = if has_head() {
        run_git(&["diff", "HEAD"])?
    } else {
        run_git(&["diff", "--cached"])?
    };
    let untracked = run_git(&["ls-files", "--others", "--exclude-standard"])?;

    Ok(truncate(format_diff(&tracked, &untracked)))
}

fn format_diff(tracked: &str, untracked: &str) -> String {
    let mut out = String::new();
    let tracked = tracked.trim_end();
    if !tracked.is_empty() {
        out.push_str(tracked);
    }
    let untracked = untracked.trim();
    if !untracked.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Untracked files:");
        for line in untracked.lines() {
            _ = write!(out, "\n  {line}");
        }
    }
    out
}

fn inside_git_repo() -> bool {
    run_git(&["rev-parse", "--is-inside-work-tree"]).is_ok_and(|s| s.trim() == "true")
}

fn has_head() -> bool {
    run_git(&["rev-parse", "--verify", "HEAD"]).is_ok()
}

fn run_git(args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr.trim();
        return Err(if msg.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            msg.to_owned()
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Cuts on a UTF-8 boundary and appends a one-line footer noting how
/// many bytes were dropped.
fn truncate(s: String) -> String {
    if s.len() <= MAX_BYTES {
        return s;
    }
    let cut = s
        .char_indices()
        .take_while(|(i, _)| *i < MAX_BYTES)
        .last()
        .map_or(0, |(i, c)| i + c.len_utf8());
    let dropped = s.len() - cut;
    let mut t = s[..cut].to_owned();
    _ = write!(t, "\n\n(truncated: {dropped} more bytes)");
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_diff ──

    #[test]
    fn format_diff_tracked_only_renders_verbatim() {
        let body = format_diff("diff --git a/x b/x\n+foo\n", "");
        assert_eq!(body, "diff --git a/x b/x\n+foo");
    }

    #[test]
    fn format_diff_untracked_only_lists_files_under_heading() {
        let body = format_diff("", "new.rs\nother.rs\n");
        assert_eq!(body, "Untracked files:\n  new.rs\n  other.rs");
    }

    #[test]
    fn format_diff_combined_separates_with_blank_line() {
        let body = format_diff("diff --git a/x b/x\n+foo\n", "new.rs\n");
        assert_eq!(
            body,
            "diff --git a/x b/x\n+foo\n\nUntracked files:\n  new.rs",
        );
    }

    #[test]
    fn format_diff_both_empty_yields_empty_string() {
        // The execute path treats empty as "No uncommitted changes" —
        // pin the contract here so a future change in trim semantics
        // doesn't accidentally start emitting "Untracked files:" alone.
        assert_eq!(format_diff("", ""), "");
        assert_eq!(format_diff("   \n", "  \n"), "");
    }

    // ── truncate ──

    #[test]
    fn truncate_short_input_unchanged() {
        let s = "abc".to_owned();
        assert_eq!(truncate(s), "abc");
    }

    #[test]
    fn truncate_oversized_input_appends_footer_with_dropped_byte_count() {
        let s = "a".repeat(MAX_BYTES + 100);
        let got = truncate(s);
        assert!(got.starts_with(&"a".repeat(MAX_BYTES)));
        assert!(
            got.ends_with("(truncated: 100 more bytes)"),
            "footer missing or wrong: {}",
            &got[got.len().saturating_sub(40)..],
        );
    }

    #[test]
    fn truncate_cuts_on_utf8_boundary_when_limit_lands_mid_char() {
        // Build a string whose byte length crosses MAX_BYTES exactly
        // inside a multi-byte char. The cut point must back up to the
        // preceding boundary so the resulting string is valid UTF-8.
        let prefix = "a".repeat(MAX_BYTES - 1);
        let s = format!("{prefix}€trailing"); // '€' is 3 bytes
        let got = truncate(s);
        assert!(got.is_char_boundary(got.len()));
        assert!(got.contains("(truncated:"));
    }
}
