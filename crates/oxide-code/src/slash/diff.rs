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
use super::registry::SlashCommand;

/// Cap diff output so a 1 GB binary diff can't freeze rendering.
/// 64 KB sits comfortably above a typical PR-sized review.
const MAX_BYTES: usize = 64 * 1024;

pub(crate) struct DiffCmd;

impl SlashCommand for DiffCmd {
    fn name(&self) -> &'static str {
        "diff"
    }

    fn description(&self) -> &'static str {
        "show uncommitted git changes"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<(), String> {
        let cwd = std::env::current_dir().map_err(|e| format!("{e:#}"))?;
        execute_in(&cwd, ctx)
    }
}

/// Body of [`DiffCmd::execute`] with the cwd injected as data so tests
/// can drive it against a tempdir without touching process state.
/// Empty trees still emit a friendly marker via the same
/// `SystemMessageBlock` `/help` and `/status` use; non-empty diffs go
/// through `GitDiffBlock` so red / green row backgrounds and the
/// line-number gutter mirror the Edit-tool diff body.
fn execute_in(cwd: &Path, ctx: &mut SlashContext<'_>) -> Result<(), String> {
    let text = collect_diff_in(cwd).map_err(|e| format!("{e:#}"))?;
    if text.trim().is_empty() {
        ctx.chat.push_system_message("No uncommitted changes.");
    } else {
        ctx.chat.push_git_diff(text);
    }
    Ok(())
}

/// Gathers tracked + untracked diff text rooted at `cwd`. Falls back
/// to `git diff --cached` when HEAD doesn't resolve (fresh repo) so
/// the command still produces useful output before the first commit.
/// `cwd` is taken as data (not from process state) so tests can run
/// against a tempdir without racing other parallel tests.
///
/// Untracked filenames pass through `String::from_utf8_lossy`; a path
/// containing non-UTF-8 bytes will render with `U+FFFD` substitutes
/// rather than fail. Acceptable trade-off: the user still sees that
/// the file exists, just under a sanitized name.
fn collect_diff_in(cwd: &Path) -> Result<String> {
    if !inside_git_repo(cwd) {
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

fn inside_git_repo(cwd: &Path) -> bool {
    run_git_in(cwd, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|s| s.trim() == "true")
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
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr.trim();
        if msg.is_empty() {
            bail!("git {} failed", args.join(" "));
        }
        bail!("{msg}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Cuts on a UTF-8 boundary so the prefix is always ≤ [`MAX_BYTES`],
/// then appends a one-line footer naming the dropped size in KB
/// (more readable than a raw byte count when the cap is in tens of KB).
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
    _ = write!(t, "\n\n(truncated: {} more)", format_size(dropped));
    t
}

/// Render a byte count as a short human-readable size: `< 1 KB` →
/// `"N B"`, `< 1 MB` → `"N.N KB"`, otherwise `"N.N MB"`. The
/// fractional digit is integer truncation (not rounding) — sufficient
/// for the truncation footer and deterministic across platforms.
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

    // The git-IO sections below shell out to the real `git` binary
    // against a tempdir so each test exercises the real IO path
    // without racing other parallel tests on the process cwd. CI
    // runners and local dev shells both have `git` on PATH.

    /// Spawn `git args...` against `cwd`, panicking on failure.
    /// Used by tests to set up tempdir state.
    fn git_setup(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git available on PATH");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr),
        );
    }

    /// Build a tempdir initialized as a git repo, with `user.email`
    /// and `user.name` configured (otherwise `git commit` fails on
    /// hermetic CI runners with no global config). Returns the
    /// `TempDir` (drop it to clean up) and an owned `PathBuf` so
    /// tests don't fight the lifetime borrow.
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
        // `cargo test` runs with the workspace root as cwd — itself a
        // git repo — so the wrapper round-trips through `current_dir`
        // and `execute_in` without error. No test mutates cwd.
        use crate::slash::test_session_info;
        use crate::tui::components::chat::ChatView;
        use crate::tui::theme::Theme;

        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let result = DiffCmd.execute("", &mut SlashContext::new(&mut chat, &info));
        assert_eq!(result, Ok(()));
        assert_eq!(chat.entry_count(), 1);
        assert!(!chat.last_is_error());
    }

    // ── execute_in ──

    #[test]
    fn execute_in_clean_repo_pushes_no_changes_marker() {
        // Empty diff → friendly marker, not a blank message. Drives
        // `execute_in` end-to-end so the system-message dispatch lands
        // in test coverage, not just `collect_diff_in`.
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
        // Dirty tree → the diff body itself reaches the chat as a
        // SystemMessageBlock. Pin that the call doesn't error and that
        // exactly one block landed (no double-push between the
        // empty-trim guard and the body branch).
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
        // The trait boundary stringifies `anyhow::Error` — pin that
        // the actionable "not inside a git repository" wording reaches
        // the dispatcher's error wrapper rather than a Debug noise.
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
    fn collect_diff_in_returns_error_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let err = collect_diff_in(dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("not inside a git repository"),
            "{err:#}",
        );
    }

    #[test]
    fn collect_diff_in_fresh_repo_is_empty_when_nothing_staged() {
        // Pre-first-commit path: `has_head` is false, so we fall back
        // to `git diff --cached`, which is empty. No untracked files
        // either. Result must be the empty string so the execute path
        // renders "No uncommitted changes."
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
        // Both arms populated: tracked diff + untracked filenames.
        // The empty-line separator pinned in `format_diff_combined_*`
        // is tested at the unit level; here we verify the two pieces
        // co-exist in real `git` output, not just synthetic strings.
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

    // ── format_diff ──

    #[test]
    fn format_diff_tracked_only_renders_verbatim() {
        let body = format_diff("diff --git a/x b/x\n+foo\n", "");
        assert_eq!(body, "diff --git a/x b/x\n+foo");
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
        // The execute path treats empty as "No uncommitted changes" —
        // pin the contract here so a future change in trim semantics
        // doesn't accidentally start emitting "Untracked files:" alone.
        assert_eq!(format_diff("", ""), "");
        assert_eq!(format_diff("   \n", "  \n"), "");
    }

    // ── inside_git_repo ──

    #[test]
    fn inside_git_repo_returns_true_for_real_repo() {
        let (_dir, repo) = fresh_repo();
        assert!(inside_git_repo(&repo));
    }

    #[test]
    fn inside_git_repo_returns_false_outside_a_repo() {
        // A bare tempdir with no `.git` is not a repo.
        let dir = tempfile::tempdir().unwrap();
        assert!(!inside_git_repo(dir.path()));
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
        // `cat-file` of a non-existent SHA yields a stable, version-
        // independent error message ("Not a valid object name ...").
        // Avoid relying on flag-parser error wording — that drifts
        // across git versions.
        let (_dir, repo) = fresh_repo();
        let err = run_git_in(&repo, &["cat-file", "-t", "deadbeef"]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("deadbeef") || msg.to_ascii_lowercase().contains("not a valid"),
            "expected git error to surface, got: {msg}",
        );
    }

    // ── truncate ──

    #[test]
    fn truncate_short_input_unchanged() {
        let s = "abc".to_owned();
        assert_eq!(truncate(s), "abc");
    }

    #[test]
    fn truncate_oversized_input_appends_footer_with_human_size() {
        let s = "a".repeat(MAX_BYTES + 100);
        let got = truncate(s);
        // The kept prefix is exactly MAX_BYTES bytes; everything after
        // is the footer. Pin both halves so an off-by-one in either
        // direction (kept too few or too many) fails here.
        let footer = "\n\n(truncated: 100 B more)";
        assert_eq!(got.len(), MAX_BYTES + footer.len());
        assert_eq!(&got[..MAX_BYTES], &"a".repeat(MAX_BYTES));
        assert!(got.ends_with(footer), "{}", &got[got.len() - 40..]);
    }

    #[test]
    fn truncate_cuts_on_utf8_boundary_and_never_exceeds_max_bytes() {
        // The cap is a strict ≤ contract — the kept prefix must NOT
        // exceed MAX_BYTES even when the boundary lands inside a
        // multi-byte char. Build a string whose final char straddles
        // the cap; the cut must back up to the preceding boundary.
        let prefix = "a".repeat(MAX_BYTES - 1);
        let s = format!("{prefix}€trailing"); // '€' is 3 bytes
        let got = truncate(s);
        let footer_start = got.find("\n\n(truncated:").expect("footer present");
        // Kept prefix ends well before the cap (one byte short of the
        // straddling '€'), confirming no overshoot.
        assert_eq!(footer_start, MAX_BYTES - 1);
        assert!(got.is_char_boundary(footer_start));
    }

    // ── format_size ──

    #[test]
    fn format_size_truncates_to_one_decimal_for_kb() {
        // The fractional digit is integer truncation, not rounding —
        // pin both branches at the boundary (1023 B vs 1024 = 1.0 KB)
        // and at the rollover into MB (1023.9 KB vs 1.0 MB) so an
        // off-by-one in the `<` comparison would fail visibly.
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
