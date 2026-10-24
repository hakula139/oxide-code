//! `/diff` shows uncommitted git changes inline. Runs `git diff HEAD`, or `git diff --cached` and
//! `git diff` before the first commit, then lists untracked files. Git runs off the event loop from
//! the repository root, and its output reaches chat as a [`WorkingTreeDiff`].

mod view;

#[cfg(test)]
pub(crate) use self::view::testing;
pub(crate) use self::view::{DiffRow, WorkingTreeDiff};

use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{Result, anyhow, bail};

use ox_core::util::git::output_async;

use super::context::SlashContext;
use super::registry::{ChatOutput, SlashCommand, SlashOutcome};

// ── DiffCmd ──

pub(super) struct DiffCmd;

impl SlashCommand for DiffCmd {
    fn name(&self) -> &'static str {
        "diff"
    }

    fn description(&self) -> &'static str {
        "Show uncommitted working-tree changes"
    }

    fn execute(&self, _args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let dir = ctx.info.project_root.clone();
        Ok(SlashOutcome::Background(Box::pin(async move {
            let diff = collect_diff(&dir)
                .await
                .map_err(|e| format!("Failed to collect working-tree changes: {e:#}"))?;
            Ok(if diff.is_empty() {
                ChatOutput::SystemMessage("Working tree clean".to_owned())
            } else {
                ChatOutput::GitDiff(diff)
            })
        })))
    }
}

// ── Execution ──

/// The tracked diff and the untracked paths of the work tree containing `dir`. Every call after the
/// first runs from the work tree's root, since `ls-files` lists only the paths under its cwd.
async fn collect_diff(dir: &Path) -> Result<WorkingTreeDiff> {
    let root = work_tree_root(dir).await?;
    let root = root.as_path();

    let untracked = run_git_in(
        root,
        &[
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--directory",
            "--no-empty-directory",
        ],
    );
    if has_head(root).await {
        let (head, untracked) = tokio::try_join!(run_git_diff_in(root, &["HEAD"]), untracked)?;
        Ok(WorkingTreeDiff::new(
            &[&head],
            &untracked,
            "`git diff HEAD`",
        ))
    } else {
        let (cached, unstaged, untracked) = tokio::try_join!(
            run_git_diff_in(root, &["--cached"]),
            run_git_diff_in(root, &[]),
            untracked,
        )?;
        Ok(WorkingTreeDiff::new(
            &[&cached, &unstaged],
            &untracked,
            "`git diff --cached` and `git diff`",
        ))
    }
}

// ── Git Helpers ──

/// The root of the work tree containing `dir`, resolved on each run so a repository git refused at
/// startup is found once the user applies the fix git names, as for dubious ownership. Fails
/// outside a work tree with git's own message.
async fn work_tree_root(dir: &Path) -> Result<PathBuf> {
    let out = run_git_in(dir, &["rev-parse", "--show-toplevel"]).await?;
    Ok(PathBuf::from(out.trim_end_matches(['\n', '\r'])))
}

async fn has_head(root: &Path) -> bool {
    run_git_in(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .await
        .is_ok()
}

/// Runs `git diff` with the output shape [`WorkingTreeDiff`] parses, whatever the user config.
async fn run_git_diff_in(root: &Path, extra: &[&str]) -> Result<String> {
    let mut args = vec![
        "-c",
        "core.quotePath=false",
        "-c",
        "diff.renames=true",
        "-c",
        "diff.suppressBlankEmpty=false",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--submodule=short",
    ];
    args.extend_from_slice(extra);
    run_git_in(root, &args).await
}

async fn run_git_in(root: &Path, args: &[&str]) -> Result<String> {
    let out = git_output(root, args).await?;
    if !out.status.success() {
        bail!("{}", git_failure_message(args, &out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs git through [`output_async`] without its optional locks, so a `/diff` beside the user's own
/// git commands never writes the index. Dropping the future kills the child.
async fn git_output(root: &Path, args: &[&str]) -> Result<Output> {
    let args_without_locks = [&["--no-optional-locks"], args].concat();
    output_async("git", &args_without_locks, root)
        .await
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indoc::indoc;
    use tempfile::TempDir;

    use ox_core::util::git::testing::{isolated_git, with_isolated_git_env_async};

    use super::*;
    use crate::session_info::testing::test_session_info;

    fn git_setup(cwd: &Path, args: &[&str]) {
        let out = isolated_git(cwd)
            .args(args)
            .output()
            .expect("git available on PATH");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "git {args:?} failed: {stderr}");
    }

    /// What git itself prints to stderr for `args` in `cwd`, trimmed.
    fn git_stderr(cwd: &Path, args: &[&str]) -> String {
        let stderr = isolated_git(cwd).args(args).output().unwrap().stderr;
        String::from_utf8_lossy(&stderr).trim().to_owned()
    }

    fn fresh_repo() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        git_setup(&path, &["init", "--quiet", "--initial-branch=main"]);
        git_setup(&path, &["config", "user.email", "test@example.invalid"]);
        git_setup(&path, &["config", "user.name", "Test"]);
        (dir, path)
    }

    fn commit_file(repo: &Path, name: &str, content: &str) {
        std::fs::write(repo.join(name), content).unwrap();
        git_setup(repo, &["add", name]);
        git_setup(repo, &["commit", "--quiet", "-m", "init"]);
    }

    mod execute {
        use super::*;

        async fn execute_at(project_root: &Path) -> Result<ChatOutput, String> {
            let mut info = test_session_info();
            info.project_root = project_root.to_owned();
            let task = DiffCmd
                .execute("", &SlashContext::new(&info, None))
                .expect("/diff defers its failures to the task")
                .into_background();
            with_isolated_git_env_async(task).await
        }

        #[tokio::test]
        async fn clean_repo_reports_a_clean_tree() {
            let (_dir, repo) = fresh_repo();

            let output = execute_at(&repo).await.expect("clean repo is Ok");

            assert!(
                matches!(&output, ChatOutput::SystemMessage(text) if text == "Working tree clean"),
                "{output:?}",
            );
        }

        #[tokio::test]
        async fn lists_the_whole_repository_when_the_project_root_is_a_subdirectory() {
            // The startup probe falls back to the working directory when git refused the repository
            // then, so the project root can be a subdirectory.
            let (_dir, repo) = fresh_repo();
            std::fs::create_dir(repo.join("sub")).unwrap();
            commit_file(&repo, "sub/kept.txt", "k\n");
            commit_file(&repo, "top.txt", "old\n");
            std::fs::write(repo.join("top.txt"), "new\n").unwrap();
            std::fs::write(repo.join("sub/here.txt"), "h\n").unwrap();
            std::fs::write(repo.join("outside.txt"), "o\n").unwrap();

            let output = execute_at(&repo.join("sub"))
                .await
                .expect("dirty repo is Ok");

            assert!(
                matches!(&output, ChatOutput::GitDiff(diff)
                    if diff.rows[0] == DiffRow::File("top.txt".to_owned())
                        && diff.untracked == ["outside.txt", "sub/here.txt"]),
                "{output:?}",
            );
        }

        #[tokio::test]
        async fn outside_a_repo_reports_what_git_says() {
            let dir = tempfile::tempdir().unwrap();
            let expected = git_stderr(dir.path(), &["rev-parse", "--is-inside-work-tree"]);

            let err = execute_at(dir.path()).await.unwrap_err();

            assert_eq!(
                err,
                format!("Failed to collect working-tree changes: {expected}")
            );
        }
    }

    mod collect_diff {
        use super::*;

        async fn collect(repo: &Path) -> Result<WorkingTreeDiff> {
            with_isolated_git_env_async(collect_diff(repo)).await
        }

        #[tokio::test]
        async fn fresh_repo_is_empty_when_nothing_staged() {
            let (_dir, repo) = fresh_repo();
            assert!(collect(&repo).await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn fresh_repo_combines_staged_and_unstaged_diffs() {
            let (_dir, repo) = fresh_repo();
            std::fs::write(repo.join("a.txt"), "staged\n").unwrap();
            git_setup(&repo, &["add", "a.txt"]);
            std::fs::write(repo.join("a.txt"), "staged\nunstaged\n").unwrap();
            std::fs::write(repo.join("new.txt"), "u\n").unwrap();
            std::fs::create_dir_all(repo.join("empty")).unwrap();
            std::fs::create_dir_all(repo.join("vendor/pkg")).unwrap();
            std::fs::write(repo.join("vendor/pkg/lib.js"), "v\n").unwrap();

            let diff = collect(&repo).await.unwrap();

            let added: Vec<_> = diff
                .rows
                .iter()
                .filter_map(|row| match row {
                    DiffRow::Added { number, text } => Some((*number, text.as_str())),
                    _ => None,
                })
                .collect();
            assert_eq!(added, [(1, "staged"), (2, "unstaged")]);
            assert_eq!(diff.untracked, ["new.txt", "vendor/"]);
            assert_eq!(diff.truncation, None);
        }

        #[tokio::test]
        async fn after_commit_diffs_against_head_and_lists_untracked_files() {
            let (_dir, repo) = fresh_repo();
            commit_file(&repo, "a.txt", "first\n");
            std::fs::write(repo.join("a.txt"), "first\nsecond\n").unwrap();
            git_setup(&repo, &["add", "a.txt"]);
            std::fs::write(repo.join("a.txt"), "first\nsecond\nthird\n").unwrap();
            std::fs::write(repo.join("new file.txt"), "u\n").unwrap();

            let diff = collect(&repo).await.unwrap();

            assert_eq!(
                diff.rows,
                [
                    DiffRow::File("a.txt".to_owned()),
                    DiffRow::Hunk("@@ -1 +1,3 @@".to_owned()),
                    DiffRow::Context {
                        number: 1,
                        text: "first".to_owned()
                    },
                    DiffRow::Added {
                        number: 2,
                        text: "second".to_owned()
                    },
                    DiffRow::Added {
                        number: 3,
                        text: "third".to_owned()
                    },
                ]
            );
            assert_eq!(diff.untracked, ["new file.txt"]);
        }

        #[tokio::test]
        async fn surfaces_why_git_refuses_a_repository_it_found() {
            // A refusal such as dubious ownership fails the same probe, but its switch is a
            // process-wide variable that would leak into concurrent tests.
            let (_dir, repo) = fresh_repo();
            git_setup(&repo, &["config", "core.repositoryformatversion", "99"]);
            let expected = git_stderr(&repo, &["rev-parse", "--is-inside-work-tree"]);

            let err = collect(&repo).await.unwrap_err();

            assert!(expected.contains("99"), "{expected}");
            assert_eq!(format!("{err:#}"), expected);
        }
    }

    mod work_tree_root {
        use super::*;

        #[tokio::test]
        async fn finds_the_root_from_a_subdirectory() {
            let (_dir, repo) = fresh_repo();
            std::fs::create_dir(repo.join("sub")).unwrap();
            let expected = isolated_git(&repo)
                .args(["rev-parse", "--show-toplevel"])
                .output()
                .unwrap()
                .stdout;

            let root = with_isolated_git_env_async(work_tree_root(&repo.join("sub")))
                .await
                .unwrap();

            assert_eq!(
                root,
                PathBuf::from(String::from_utf8(expected).unwrap().trim_end())
            );
        }

        #[tokio::test]
        async fn refuses_the_git_directory_with_what_git_says() {
            let (_dir, repo) = fresh_repo();
            let git_dir = repo.join(".git");
            let expected = git_stderr(&git_dir, &["rev-parse", "--show-toplevel"]);

            let err = with_isolated_git_env_async(work_tree_root(&git_dir))
                .await
                .unwrap_err();

            assert_eq!(format!("{err:#}"), expected);
        }
    }

    mod has_head {
        use super::*;

        #[tokio::test]
        async fn finds_no_head_in_a_fresh_repo() {
            let (_dir, repo) = fresh_repo();
            assert!(!with_isolated_git_env_async(has_head(&repo)).await);
        }

        #[tokio::test]
        async fn finds_the_head_after_the_first_commit() {
            let (_dir, repo) = fresh_repo();
            commit_file(&repo, "a.txt", "hello\n");
            assert!(with_isolated_git_env_async(has_head(&repo)).await);
        }
    }

    mod run_git_diff_in {
        use super::*;

        #[tokio::test]
        async fn pins_the_output_shape_against_user_config() {
            let (_dir, repo) = fresh_repo();
            commit_file(&repo, "é.txt", "first\n\nthird\n");
            std::fs::write(repo.join("é.txt"), "first\n\nTHIRD\n").unwrap();
            git_setup(&repo, &["config", "core.quotePath", "true"]);
            git_setup(&repo, &["config", "diff.noprefix", "true"]);
            git_setup(&repo, &["config", "diff.suppressBlankEmpty", "true"]);
            git_setup(&repo, &["config", "color.ui", "always"]);

            let body = with_isolated_git_env_async(run_git_diff_in(&repo, &["HEAD"]))
                .await
                .unwrap();

            assert!(body.starts_with("diff --git a/é.txt b/é.txt\n"), "{body}");
            assert!(body.contains(" first\n \n-third\n"), "{body:?}");
            assert!(
                body.contains(indoc! {"

                    --- a/é.txt
                    +++ b/é.txt
                "}),
                "{body}"
            );
            assert!(!body.contains('\x1b'), "{body:?}");
        }

        #[tokio::test]
        async fn detects_renames_but_not_copies_whatever_the_user_config() {
            let (_dir, repo) = fresh_repo();
            let content = "one\ntwo\nthree\nfour\nfive\n";
            commit_file(&repo, "a.txt", content);
            commit_file(&repo, "old.txt", "moved\nlines\nhere\n");
            std::fs::write(repo.join("a.txt"), format!("{content}six\n")).unwrap();
            std::fs::write(repo.join("b.txt"), content).unwrap();
            git_setup(&repo, &["add", "b.txt"]);
            git_setup(&repo, &["mv", "old.txt", "new.txt"]);
            git_setup(&repo, &["config", "diff.renames", "copies"]);

            let body = with_isolated_git_env_async(run_git_diff_in(&repo, &["HEAD"]))
                .await
                .unwrap();

            assert!(
                body.contains("diff --git a/b.txt b/b.txt\nnew file"),
                "{body}"
            );
            assert!(!body.contains("copy from"), "{body}");
            assert!(
                body.contains("rename from old.txt\nrename to new.txt"),
                "{body}"
            );
        }
    }

    mod run_git_in {
        use super::*;

        #[tokio::test]
        async fn propagates_stderr_on_failure() {
            let (_dir, repo) = fresh_repo();
            let args = ["cat-file", "-t", "deadbeef"];
            let expected = git_stderr(&repo, &args);

            let err = with_isolated_git_env_async(run_git_in(&repo, &args))
                .await
                .unwrap_err();

            assert_eq!(format!("{err:#}"), expected);
        }
    }

    mod git_output {
        use super::*;

        #[tokio::test]
        async fn reports_a_spawn_failure_with_a_recovery_step() {
            let dir = tempfile::tempdir().unwrap();
            let missing = dir.path().join("missing");
            let cause = std::process::Command::new("git")
                .current_dir(&missing)
                .output()
                .unwrap_err();

            let err = git_output(&missing, &["status"]).await.unwrap_err();

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "failed to spawn git status: {cause}. Check that git is installed and on PATH."
                ),
            );
        }
    }

    mod git_failure_message {
        use super::*;

        #[test]
        fn passes_through_trimmed_stderr() {
            assert_eq!(
                git_failure_message(&["status"], b"  fatal: not a git repo\n"),
                "fatal: not a git repo",
            );
        }

        #[test]
        fn falls_back_to_synthetic_when_stderr_blank() {
            assert_eq!(git_failure_message(&["status"], b""), "git status failed");
            assert_eq!(
                git_failure_message(&["diff", "HEAD"], b"  \n\t\n  "),
                "git diff HEAD failed",
            );
        }
    }
}
