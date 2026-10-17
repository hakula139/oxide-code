//! `/diff` shows uncommitted git changes inline. Runs `git diff HEAD`, or `git diff --cached` and
//! `git diff` before the first commit, then lists untracked files. Capped at [`MAX_BYTES`] so a
//! runaway diff can't stall rendering.

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Result, anyhow, bail};

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashOutcome};

// ── DiffCmd ──

const MAX_BYTES: usize = 64 * 1024;

pub(super) struct DiffCmd;

impl SlashCommand for DiffCmd {
    fn name(&self) -> &'static str {
        "diff"
    }

    fn description(&self) -> &'static str {
        "Show uncommitted working-tree changes"
    }

    fn execute(&self, _args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let cwd = ctx
            .info
            .working_dir
            .as_deref()
            .ok_or("No working directory for this session")?;
        let text = collect_diff_in(cwd)
            .map_err(|e| format!("Failed to collect working-tree changes: {e:#}"))?;
        if text.trim().is_empty() {
            Ok(SlashOutcome::SystemMessage("Working tree clean".to_owned()))
        } else {
            Ok(SlashOutcome::GitDiff(text))
        }
    }
}

// ── Execution ──

/// The tracked diff and the untracked file list at `cwd`, truncated for display.
fn collect_diff_in(cwd: &Path) -> Result<String> {
    if !inside_git_repo(cwd)? {
        bail!("not inside a git repository");
    }

    let has_head = has_head(cwd);
    let tracked = if has_head {
        run_git_diff_in(cwd, &["HEAD"])?
    } else {
        let cached = run_git_diff_in(cwd, &["--cached"])?;
        let unstaged = run_git_diff_in(cwd, &[])?;
        join_sections(&[cached.as_str(), unstaged.as_str()])
    };
    let untracked = run_git_in(cwd, &["ls-files", "--others", "--exclude-standard"])?;

    Ok(truncate(
        format_diff(&tracked, &untracked),
        full_diff_hint(has_head),
    ))
}

// ── Formatting ──

fn format_diff(tracked: &str, untracked: &str) -> String {
    let untracked = untracked.trim();
    let untracked = if untracked.is_empty() {
        String::new()
    } else {
        let lines = untracked
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Untracked files:\n{lines}")
    };
    join_sections(&[tracked, &untracked])
}

fn join_sections(sections: &[&str]) -> String {
    sections
        .iter()
        // Strip only git's trailing newline, since a diff line can end in meaningful whitespace.
        .map(|s| s.trim_end_matches('\n'))
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn full_diff_hint(has_head: bool) -> &'static str {
    if has_head {
        "`git diff HEAD`"
    } else {
        "`git diff --cached` and `git diff`"
    }
}

// ── Git Helpers ──

/// `Ok(false)` outside a worktree, and `Err` when git can't spawn.
fn inside_git_repo(cwd: &Path) -> Result<bool> {
    let out = git_output(cwd, &["rev-parse", "--is-inside-work-tree"])?;
    Ok(out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true")
}

fn has_head(cwd: &Path) -> bool {
    run_git_in(cwd, &["rev-parse", "--verify", "HEAD"]).is_ok()
}

/// Runs `git diff` with the output shape the diff block parses, whatever the user's git config.
fn run_git_diff_in(cwd: &Path, extra: &[&str]) -> Result<String> {
    let mut args = vec![
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    args.extend_from_slice(extra);
    run_git_in(cwd, &args)
}

fn run_git_in(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = git_output(cwd, args)?;
    if !out.status.success() {
        bail!("{}", git_failure_message(args, &out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            anyhow!(
                "failed to spawn git {}: {e}. Check that git is installed and on PATH.",
                args.join(" "),
            )
        })
}

/// Trimmed stderr, or `git <args> failed` when stderr is blank.
fn git_failure_message(args: &[&str], stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    let msg = s.trim();
    if msg.is_empty() {
        format!("git {} failed", args.join(" "))
    } else {
        msg.to_owned()
    }
}

// ── Truncation ──

/// Cuts `s` on a UTF-8 boundary within [`MAX_BYTES`], with a footer naming the dropped size.
fn truncate(mut s: String, full_hint: &str) -> String {
    if s.len() <= MAX_BYTES {
        return s;
    }
    let cut = s.floor_char_boundary(MAX_BYTES);
    let dropped = s.len() - cut;
    s.truncate(cut);
    _ = write!(
        s,
        "\n\n(Truncated {}. Run {full_hint} for the full output.)",
        format_size(dropped),
    );
    s
}

/// `"N B"`, `"N.N KB"`, or `"N.N MB"` (integer-truncated decimal).
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
    use crate::slash::test_session_info;
    use crate::util::git::testing::{isolated_git, with_isolated_git_env};

    fn git_setup(cwd: &Path, args: &[&str]) {
        let out = isolated_git(cwd)
            .args(args)
            .output()
            .expect("git available on PATH");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "git {args:?} failed: {stderr}");
    }

    fn fresh_repo() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        git_setup(&path, &["init", "--quiet", "--initial-branch=main"]);
        git_setup(&path, &["config", "user.email", "test@example.invalid"]);
        git_setup(&path, &["config", "user.name", "Test"]);
        (dir, path)
    }

    // ── DiffCmd::name ──

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(DiffCmd.name(), "diff");
        assert_eq!(DiffCmd.aliases(), [] as [&str; 0]);
        assert_ne!(DiffCmd.description(), "");
    }

    // ── DiffCmd::execute ──

    fn execute_at(cwd: Option<&Path>) -> Result<SlashOutcome, String> {
        let mut info = test_session_info();
        info.working_dir = cwd.map(Path::to_owned);
        with_isolated_git_env(|| DiffCmd.execute("", &SlashContext::new(&info, None)))
    }

    #[test]
    fn execute_clean_repo_reports_a_clean_tree() {
        let (_dir, repo) = fresh_repo();
        let outcome = execute_at(Some(&repo)).expect("clean repo execute is Ok");
        assert!(
            matches!(&outcome, SlashOutcome::SystemMessage(text) if text == "Working tree clean"),
            "{outcome:?}",
        );
    }

    #[test]
    fn execute_dirty_repo_returns_the_diff() {
        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("note.txt"), "hello\n").unwrap();
        let outcome = execute_at(Some(&repo)).expect("dirty repo execute is Ok");
        assert!(
            matches!(&outcome, SlashOutcome::GitDiff(text) if text == "Untracked files:\n  note.txt"),
            "{outcome:?}",
        );
    }

    #[test]
    fn execute_outside_a_repo_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = execute_at(Some(dir.path())).expect_err("execute must error outside a repo");
        assert_eq!(
            err,
            "Failed to collect working-tree changes: not inside a git repository"
        );
    }

    #[test]
    fn execute_without_a_session_cwd_errors() {
        let err = execute_at(None).expect_err("execute needs the session cwd");
        assert_eq!(err, "No working directory for this session");
    }

    // ── collect_diff_in ──

    #[test]
    fn collect_diff_in_fresh_repo_is_empty_when_nothing_staged() {
        let (_dir, repo) = fresh_repo();
        assert_eq!(
            with_isolated_git_env(|| collect_diff_in(&repo)).unwrap(),
            ""
        );
    }

    #[test]
    fn collect_diff_in_fresh_repo_lists_untracked_files() {
        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("new.txt"), "hi\n").unwrap();
        let body = with_isolated_git_env(|| collect_diff_in(&repo)).unwrap();
        assert_eq!(body, "Untracked files:\n  new.txt");
    }

    #[test]
    fn collect_diff_in_fresh_repo_combines_staged_and_unstaged_diffs() {
        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("a.txt"), "staged\n").unwrap();
        git_setup(&repo, &["add", "a.txt"]);
        std::fs::write(repo.join("a.txt"), "staged\nunstaged\n").unwrap();

        let body = with_isolated_git_env(|| collect_diff_in(&repo)).unwrap();

        assert!(body.contains("+staged"), "staged add missing: {body}");
        assert!(body.contains("+unstaged"), "unstaged edit missing: {body}");
    }

    #[test]
    fn collect_diff_in_after_commit_shows_unstaged_changes() {
        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("a.txt"), "first\n").unwrap();
        git_setup(&repo, &["add", "a.txt"]);
        git_setup(&repo, &["commit", "--quiet", "-m", "init"]);
        std::fs::write(repo.join("a.txt"), "first\nsecond\n").unwrap();
        let body = with_isolated_git_env(|| collect_diff_in(&repo)).unwrap();
        assert!(body.contains("a.txt"), "diff body missing path: {body}");
        assert!(body.contains("+second"), "diff body missing add: {body}");
    }

    #[test]
    fn collect_diff_in_separates_tracked_changes_from_untracked_list() {
        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("a.txt"), "first\n").unwrap();
        git_setup(&repo, &["add", "a.txt"]);
        git_setup(&repo, &["commit", "--quiet", "-m", "init"]);
        std::fs::write(repo.join("a.txt"), "first\nedit\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "u\n").unwrap();
        let body = with_isolated_git_env(|| collect_diff_in(&repo)).unwrap();
        assert!(body.contains("+edit"), "{body}");
        assert!(body.contains("Untracked files:"), "{body}");
        assert!(body.contains("untracked.txt"), "{body}");
    }

    // ── format_diff ──

    #[test]
    fn format_diff_tracked_only_renders_verbatim() {
        let body = format_diff(
            indoc! {"
                diff --git a/x b/x
                +foo
            "},
            "",
        );
        assert_eq!(
            body,
            indoc! {"
                diff --git a/x b/x
                +foo"
            }
        );
    }

    #[test]
    fn format_diff_keeps_trailing_whitespace_on_diff_lines() {
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
        let body = format_diff(
            indoc! {"
                diff --git a/x b/x
                +foo
            "},
            "new.rs\n",
        );
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
        assert_eq!(format_diff("", ""), "");
        assert_eq!(format_diff("   \n", "  \n"), "");
    }

    // ── join_sections ──

    #[test]
    fn join_sections_keeps_non_empty_sections_with_blank_separator() {
        assert_eq!(join_sections(&["one\n", " \n", "two\n"]), "one\n\ntwo");
    }

    // ── inside_git_repo ──

    #[test]
    fn inside_git_repo_is_true_for_real_repo() {
        let (_dir, repo) = fresh_repo();
        assert!(with_isolated_git_env(|| inside_git_repo(&repo)).unwrap());
    }

    #[test]
    fn inside_git_repo_is_false_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!with_isolated_git_env(|| inside_git_repo(dir.path())).unwrap());
    }

    // ── has_head ──

    #[test]
    fn has_head_is_false_in_fresh_repo_with_no_commits() {
        let (_dir, repo) = fresh_repo();
        assert!(!with_isolated_git_env(|| has_head(&repo)));
    }

    #[test]
    fn has_head_is_true_after_first_commit() {
        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        git_setup(&repo, &["add", "a.txt"]);
        git_setup(&repo, &["commit", "--quiet", "-m", "init"]);
        assert!(with_isolated_git_env(|| has_head(&repo)));
    }

    // ── run_git_diff_in ──

    #[test]
    fn run_git_diff_in_pins_prefixes_and_color_against_user_config() {
        let (_dir, repo) = fresh_repo();
        std::fs::write(repo.join("a.txt"), "first\n").unwrap();
        git_setup(&repo, &["add", "a.txt"]);
        git_setup(&repo, &["commit", "--quiet", "-m", "init"]);
        std::fs::write(repo.join("a.txt"), "second\n").unwrap();
        git_setup(&repo, &["config", "diff.noprefix", "true"]);
        git_setup(&repo, &["config", "color.ui", "always"]);

        let body = run_git_diff_in(&repo, &["HEAD"]).unwrap();
        assert!(body.starts_with("diff --git a/a.txt b/a.txt\n"), "{body}");
        assert!(
            body.contains(indoc! {"

                --- a/a.txt
                +++ b/a.txt
            "}),
            "{body}"
        );
        assert!(!body.contains('\x1b'), "{body:?}");
    }

    // ── run_git_in ──

    #[test]
    fn run_git_in_propagates_stderr_on_failure() {
        let (_dir, repo) = fresh_repo();
        let err = with_isolated_git_env(|| run_git_in(&repo, &["cat-file", "-t", "deadbeef"]))
            .unwrap_err();
        let msg = format!("{err:#}");
        let has_sha = msg.contains("deadbeef");
        let has_not_valid = msg.to_ascii_lowercase().contains("not a valid");
        assert!(
            has_sha || has_not_valid,
            "expected git error to surface, got: {msg}",
        );
    }

    // ── git_output ──

    #[test]
    fn git_output_reports_a_spawn_failure_with_a_recovery_step() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let cause = Command::new("git")
            .current_dir(&missing)
            .output()
            .unwrap_err();

        let err = git_output(&missing, &["status"]).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            format!(
                "failed to spawn git status: {cause}. Check that git is installed and on PATH."
            ),
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
        assert_eq!(git_failure_message(&["status"], b""), "git status failed",);
        assert_eq!(
            git_failure_message(&["diff", "HEAD"], b"  \n\t\n  "),
            "git diff HEAD failed",
        );
    }

    // ── truncate ──

    #[test]
    fn truncate_under_or_at_cap_is_unchanged() {
        assert_eq!(truncate("abc".to_owned(), "`git diff HEAD`"), "abc");
        let exact = "a".repeat(MAX_BYTES);
        assert_eq!(truncate(exact.clone(), "`git diff HEAD`"), exact);
    }

    #[test]
    fn truncate_oversized_input_appends_footer_with_human_size() {
        let s = "a".repeat(MAX_BYTES + 100);
        let got = truncate(s, "`git diff HEAD`");
        let footer = "\n\n(Truncated 100 B. Run `git diff HEAD` for the full output.)";
        assert_eq!(got.len(), MAX_BYTES + footer.len());
        assert_eq!(&got[..MAX_BYTES], &"a".repeat(MAX_BYTES));
        let tail = &got[got.len() - 80..];
        assert!(got.ends_with(footer), "footer drift, tail: {tail}");
    }

    #[test]
    fn truncate_cuts_on_utf8_boundary_and_never_exceeds_max_bytes() {
        // The final char straddles the cap, so the cut backs up to the preceding boundary.
        let prefix = "a".repeat(MAX_BYTES - 1);
        let s = format!("{prefix}€trailing"); // '€' is 3 bytes
        let got = truncate(s, "`git diff --cached` and `git diff`");
        let footer_start = got.find("\n\n(Truncated ").expect("footer present");
        assert_eq!(footer_start, MAX_BYTES - 1);
        assert!(got.is_char_boundary(footer_start));
        assert!(got.contains("`git diff --cached` and `git diff`"));
    }

    // ── format_size ──

    #[test]
    fn format_size_picks_unit_and_truncates_to_one_decimal() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024 - 1), "1023.9 KB");
        let mb = 1024 * 1024;
        assert_eq!(format_size(mb), "1.0 MB");
        assert_eq!(format_size(mb * 3 / 2), "1.5 MB");
    }
}
