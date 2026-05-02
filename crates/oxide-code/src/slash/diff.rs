//! `/diff` — show uncommitted git changes inline.
//!
//! Runs `git diff HEAD` (or `git diff --cached` in a fresh repo with
//! no commits yet) and appends the names of untracked files that
//! aren't gitignored. Output is capped so a runaway diff can't lock
//! the render loop.

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashOutcome};

/// Cap so a runaway binary diff can't freeze rendering. 64 KB sits
/// comfortably above a typical PR-sized review.
const MAX_BYTES: usize = 64 * 1024;

pub(crate) struct DiffCmd;

impl SlashCommand for DiffCmd {
    fn name(&self) -> &'static str {
        "diff"
    }

    fn description(&self) -> &'static str {
        "Show uncommitted working-tree changes (`git diff HEAD`) and the names of any untracked files"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let cwd = std::env::current_dir()
            .context("failed to read current directory")
            .map_err(|e| format!("{e:#}"))?;
        execute_in(&cwd, ctx)
    }
}

/// Body of [`DiffCmd::execute`] with cwd injected as data so tests can
/// drive it against a tempdir without touching process state.
fn execute_in(cwd: &Path, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
    let text = collect_diff_in(cwd).map_err(|e| format!("{e:#}"))?;
    if text.trim().is_empty() {
        ctx.chat.push_system_message("Working tree clean.");
    } else {
        ctx.chat.push_git_diff(text);
    }
    Ok(SlashOutcome::Local)
}

/// Gathers tracked + untracked diff text rooted at `cwd`. Falls back
/// to `git diff --cached` when HEAD doesn't resolve (fresh repo) so the
/// command still produces useful output before the first commit. Paths
/// with non-UTF-8 bytes render with `U+FFFD` substitutes rather than
/// fail.
fn collect_diff_in(cwd: &Path) -> Result<String> {
    if !inside_git_repo(cwd)? {
        bail!("not inside a git repository");
    }

    let tracked = if has_head(cwd) {
        run_git_in(cwd, &["diff", "HEAD"])?
    } else {
        run_git_in(cwd, &["diff", "--cached"])?
    };
    let untracked = run_git_in(cwd, &["ls-files", "--others", "--exclude-standard"])?;

    Ok(truncate(format_diff(&tracked, &untracked)))
}

fn format_diff(tracked: &str, untracked: &str) -> String {
    let mut out = String::new();
    // Strip only git's trailing newline — keep any in-line trailing
    // whitespace on real context/change lines.
    let tracked = tracked.trim_end_matches('\n');
    if !tracked.trim().is_empty() {
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

/// Distinguish "git binary missing" (Err) from "git ran but we're
/// outside a work tree" (Ok(false)) so the user sees the actionable
/// spawn error instead of the misleading "not inside a git repository".
fn inside_git_repo(cwd: &Path) -> Result<bool> {
    let out = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .context("failed to spawn git — is it installed and on PATH?")?;
    Ok(out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true")
}

fn has_head(cwd: &Path) -> bool {
    run_git_in(cwd, &["rev-parse", "--verify", "HEAD"]).is_ok()
}

fn run_git_in(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to spawn git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!("{}", git_failure_message(args, &out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Trimmed stderr if non-empty, else `git <args> failed` so a
/// blank-stderr exit doesn't surface as the empty string.
fn git_failure_message(args: &[&str], stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    let msg = s.trim();
    if msg.is_empty() {
        format!("git {} failed", args.join(" "))
    } else {
        msg.to_owned()
    }
}

/// Cuts on a UTF-8 boundary so the prefix is always ≤ [`MAX_BYTES`],
/// then appends a one-line footer naming the dropped size.
fn truncate(s: String) -> String {
    if s.len() <= MAX_BYTES {
        return s;
    }
    let cut = s
        .char_indices()
        .take_while(|(i, c)| i + c.len_utf8() <= MAX_BYTES)
        .last()
        .map_or(0, |(i, c)| i + c.len_utf8());
    let dropped = s.len() - cut;
    let mut t = s[..cut].to_owned();
    _ = write!(
        t,
        "\n\n(truncated: {} more — run `git diff HEAD` for the full output)",
        format_size(dropped),
    );
    t
}

/// Render a byte count as `"N B"`, `"N.N KB"`, or `"N.N MB"`. The
/// fractional digit is integer-truncated (not rounded) for
/// deterministic output across platforms.
fn format_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        let whole = bytes / KB;
        let tenth = (bytes % KB) * 10 / KB;
        format!("{whole}.{tenth} KB")
    } else {
        let whole = bytes / MB;
        let tenth = (bytes % MB) * 10 / MB;
        format!("{whole}.{tenth} MB")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indoc::indoc;
    use tempfile::TempDir;

    use super::*;

    // The git-IO tests below shell out to a real `git` binary against
    // a tempdir to avoid racing other parallel tests on the process cwd.

    /// Spawn `git args...` against `cwd`, panicking on failure.
    fn git_setup(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git available on PATH");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "git {args:?} failed: {stderr}");
    }

    /// Tempdir initialized as a git repo with `user.email` and
    /// `user.name` set (else `git commit` fails on hermetic CI runners).
    fn fresh_repo() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        git_setup(&path, &["init", "--quiet", "--initial-branch=main"]);
        git_setup(&path, &["config", "user.email", "test@example.invalid"]);
        git_setup(&path, &["config", "user.name", "Test"]);
        (dir, path)
    }

    // ── execute ──

    #[test]
    fn execute_forwards_process_cwd_through_execute_in() {
        // `cargo test` runs from the workspace root (a git repo) so the
        // wrapper round-trips through `current_dir` without error.
        use crate::slash::test_session_info;
        use crate::tui::components::chat::ChatView;
        use crate::tui::theme::Theme;

        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let result = DiffCmd.execute("", &mut SlashContext::new(&mut chat, &info));
        assert_eq!(result, Ok(SlashOutcome::Local));
        assert_eq!(chat.entry_count(), 1);
        assert!(!chat.last_is_error());
    }

    // ── execute_in ──

    #[test]
    fn execute_in_clean_repo_pushes_no_changes_marker() {
        // Empty diff → friendly marker, not a blank message.
        use crate::slash::test_session_info;
        use crate::tui::components::chat::ChatView;
        use crate::tui::theme::Theme;

        let (_dir, repo) = fresh_repo();
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        execute_in(&repo, &mut SlashContext::new(&mut chat, &info))
            .expect("clean repo execute_in is Ok");
        assert_eq!(chat.entry_count(), 1);
        assert!(!chat.last_is_error());
    }

    #[test]
    fn execute_in_dirty_repo_pushes_diff_text() {
        // Pin exactly one block lands — no double-push between the
        // empty-trim guard and the body branch.
        use crate::slash::test_session_info;
        use crate::tui::components::chat::ChatView;
        use crate::tui::theme::Theme;

        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("note.txt"), "hello\n").unwrap();
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        execute_in(&repo, &mut SlashContext::new(&mut chat, &info))
            .expect("dirty repo execute_in is Ok");
        assert_eq!(chat.entry_count(), 1);
        assert!(!chat.last_is_error());
    }

    #[test]
    fn execute_in_outside_a_repo_returns_err_string() {
        // Pin the actionable wording survives the `anyhow → String`
        // boundary on the trait return type.
        use crate::slash::test_session_info;
        use crate::tui::components::chat::ChatView;
        use crate::tui::theme::Theme;

        let dir = tempfile::tempdir().unwrap();
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let err = execute_in(dir.path(), &mut SlashContext::new(&mut chat, &info))
            .expect_err("execute_in must error outside a repo");
        assert!(err.contains("not inside a git repository"), "{err}");
    }

    // ── collect_diff_in ──

    #[test]
    fn collect_diff_in_fresh_repo_is_empty_when_nothing_staged() {
        // Pre-first-commit path: `has_head` is false, so we fall back to
        // `git diff --cached` (empty). Empty result drives the execute
        // path's "Working tree clean." marker.
        let (_dir, repo) = fresh_repo();
        assert_eq!(collect_diff_in(&repo).unwrap(), "");
    }

    #[test]
    fn collect_diff_in_fresh_repo_lists_untracked_files() {
        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("new.txt"), "hi\n").unwrap();
        let body = collect_diff_in(&repo).unwrap();
        assert!(body.contains("Untracked files:"), "{body}");
        assert!(body.contains("new.txt"), "{body}");
    }

    #[test]
    fn collect_diff_in_after_commit_shows_unstaged_changes() {
        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("a.txt"), "first\n").unwrap();
        git_setup(&repo, &["add", "a.txt"]);
        git_setup(&repo, &["commit", "--quiet", "-m", "init"]);
        // Modify after commit so `git diff HEAD` has output.
        std::fs::write(repo.join("a.txt"), "first\nsecond\n").unwrap();
        let body = collect_diff_in(&repo).unwrap();
        assert!(body.contains("a.txt"), "diff body missing path: {body}");
        assert!(body.contains("+second"), "diff body missing add: {body}");
    }

    #[test]
    fn collect_diff_in_separates_tracked_changes_from_untracked_list() {
        // Both arms populated, exercising real git output (the
        // synthetic separator case is pinned in `format_diff_*`).
        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("a.txt"), "first\n").unwrap();
        git_setup(&repo, &["add", "a.txt"]);
        git_setup(&repo, &["commit", "--quiet", "-m", "init"]);
        std::fs::write(repo.join("a.txt"), "first\nedit\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "u\n").unwrap();
        let body = collect_diff_in(&repo).unwrap();
        assert!(body.contains("+edit"), "{body}");
        assert!(body.contains("Untracked files:"), "{body}");
        assert!(body.contains("untracked.txt"), "{body}");
    }

    #[test]
    fn collect_diff_in_returns_error_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let err = collect_diff_in(dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("not inside a git repository"),
            "{err:#}",
        );
    }

    // ── format_diff ──

    #[test]
    fn format_diff_tracked_only_renders_verbatim() {
        let body = format_diff("diff --git a/x b/x\n+foo\n", "");
        assert_eq!(body, "diff --git a/x b/x\n+foo");
    }

    #[test]
    fn format_diff_keeps_trailing_whitespace_on_diff_lines() {
        // Real diffs can include trailing-whitespace edits; only git's
        // trailing newline gets stripped.
        let body = format_diff(" context  \n+added  \n", "");
        assert_eq!(body, " context  \n+added  ");
    }

    #[test]
    fn format_diff_untracked_only_lists_files_under_heading() {
        let body = format_diff("", "new.rs\nother.rs\n");
        assert_eq!(
            body,
            indoc! {"
                Untracked files:
                  new.rs
                  other.rs"
            },
        );
    }

    #[test]
    fn format_diff_combined_separates_with_blank_line() {
        let body = format_diff("diff --git a/x b/x\n+foo\n", "new.rs\n");
        assert_eq!(
            body,
            indoc! {"
                diff --git a/x b/x
                +foo

                Untracked files:
                  new.rs"
            },
        );
    }

    #[test]
    fn format_diff_both_empty_yields_empty_string() {
        // Pin so a trim-semantics change can't emit "Untracked files:"
        // alone with no body.
        assert_eq!(format_diff("", ""), "");
        assert_eq!(format_diff("   \n", "  \n"), "");
    }

    // ── inside_git_repo ──

    #[test]
    fn inside_git_repo_returns_true_for_real_repo() {
        let (_dir, repo) = fresh_repo();
        assert!(inside_git_repo(&repo).unwrap());
    }

    #[test]
    fn inside_git_repo_returns_false_outside_a_repo() {
        // A bare tempdir with no `.git` is not a repo.
        let dir = tempfile::tempdir().unwrap();
        assert!(!inside_git_repo(dir.path()).unwrap());
    }

    // ── has_head ──

    #[test]
    fn has_head_is_false_in_fresh_repo_with_no_commits() {
        let (_dir, repo) = fresh_repo();
        assert!(!has_head(&repo));
    }

    #[test]
    fn has_head_is_true_after_first_commit() {
        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        git_setup(&repo, &["add", "a.txt"]);
        git_setup(&repo, &["commit", "--quiet", "-m", "init"]);
        assert!(has_head(&repo));
    }

    // ── run_git_in ──

    #[test]
    fn run_git_in_propagates_stderr_on_failure() {
        // `cat-file` of a missing SHA gives stable, version-independent
        // wording — flag-parser errors drift across git versions.
        let (_dir, repo) = fresh_repo();
        let err = run_git_in(&repo, &["cat-file", "-t", "deadbeef"]).unwrap_err();
        let msg = format!("{err:#}");
        // Both arms eager — `||` short-circuit would otherwise leave
        // the right arm unexecuted on git versions that echo the SHA.
        let has_sha = msg.contains("deadbeef");
        let has_not_valid = msg.to_ascii_lowercase().contains("not a valid");
        assert!(
            has_sha || has_not_valid,
            "expected git error to surface, got: {msg}",
        );
    }

    // ── git_failure_message ──

    #[test]
    fn git_failure_message_passes_through_trimmed_stderr() {
        assert_eq!(
            git_failure_message(&["status"], b"  fatal: not a git repo\n"),
            "fatal: not a git repo",
        );
    }

    #[test]
    fn git_failure_message_falls_back_to_synthetic_when_stderr_blank() {
        // Without the fallback an empty / whitespace-only stderr would
        // surface as the empty string.
        assert_eq!(git_failure_message(&["status"], b""), "git status failed",);
        assert_eq!(
            git_failure_message(&["diff", "HEAD"], b"  \n\t\n  "),
            "git diff HEAD failed",
        );
    }

    // ── truncate ──

    #[test]
    fn truncate_short_input_unchanged() {
        let s = "abc".to_owned();
        assert_eq!(truncate(s), "abc");
    }

    #[test]
    fn truncate_at_exact_cap_returns_input_unchanged() {
        // Boundary: gate is `<=`, so MAX_BYTES is in-bounds. Flipping
        // to `<` would footer every full-cap diff.
        let s = "a".repeat(MAX_BYTES);
        assert_eq!(truncate(s.clone()), s);
    }

    #[test]
    fn truncate_oversized_input_appends_footer_with_human_size() {
        let s = "a".repeat(MAX_BYTES + 100);
        let got = truncate(s);
        // Pin kept prefix and footer separately — an off-by-one in
        // either direction or a dropped recovery hint must fail here.
        let footer = "\n\n(truncated: 100 B more — run `git diff HEAD` for the full output)";
        assert_eq!(got.len(), MAX_BYTES + footer.len());
        assert_eq!(&got[..MAX_BYTES], &"a".repeat(MAX_BYTES));
        let tail = &got[got.len() - 80..];
        assert!(got.ends_with(footer), "footer drift, tail: {tail}");
    }

    #[test]
    fn truncate_cuts_on_utf8_boundary_and_never_exceeds_max_bytes() {
        // Final char straddles the cap; cut must back up to the
        // preceding boundary so the prefix never exceeds MAX_BYTES.
        let prefix = "a".repeat(MAX_BYTES - 1);
        let s = format!("{prefix}€trailing"); // '€' is 3 bytes
        let got = truncate(s);
        let footer_start = got.find("\n\n(truncated:").expect("footer present");
        assert_eq!(footer_start, MAX_BYTES - 1);
        assert!(got.is_char_boundary(footer_start));
    }

    // ── format_size ──

    #[test]
    fn format_size_truncates_to_one_decimal_for_kb() {
        // Fractional digit is integer truncation; pin both unit
        // boundaries so an off-by-one in `<` would fail visibly.
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024 - 1), "1023.9 KB");
    }

    #[test]
    fn format_size_switches_to_mb_above_one_megabyte() {
        let mb = 1024 * 1024;
        assert_eq!(format_size(mb), "1.0 MB");
        assert_eq!(format_size(mb * 3 / 2), "1.5 MB");
    }
}
