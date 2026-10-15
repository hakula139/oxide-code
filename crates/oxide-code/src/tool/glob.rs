//! File pattern matching tool (glob).

use std::fmt::Write as _;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolMetadata, ToolOutput, ToolResultView, extract_input_field};
use crate::permission::{PermissionClass, ReadDeny};

const MAX_RESULTS: usize = 100;

/// Withholds each path a `read` deny rule matches from the results.
#[cfg_attr(test, derive(Default))]
pub(crate) struct GlobTool {
    read_deny: ReadDeny,
}

impl GlobTool {
    pub(crate) fn new(read_deny: ReadDeny) -> Self {
        Self { read_deny }
    }
}

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files matching a glob pattern. Returns paths sorted by modification time (newest first)."
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Read
    }

    fn gate_target(
        &self,
        input: &serde_json::Value,
        cwd: &std::path::Path,
    ) -> crate::permission::GateTarget {
        // The gated target is the search root (the `path` arg, defaulting to cwd), so a path-scoped
        // deny can block listing under a protected directory.
        let path = extract_input_field(input, "path").unwrap_or(".");
        crate::permission::GateTarget::for_path(path, cwd)
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": r#"The glob pattern to match, relative to `path` (e.g. "**/*.rs", "src/**/*.ts"). "*" and "?" stay within one directory, and a pattern without "/" matches file names at any depth."#
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search in (default: current working directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn icon(&self) -> &'static str {
        "✱"
    }

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "pattern")
    }

    fn result_view(
        &self,
        input: &serde_json::Value,
        content: &str,
        metadata: &ToolMetadata,
    ) -> Option<ToolResultView> {
        build_files_view(input, content, metadata)
    }

    fn run(
        &self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(run(input, self.read_deny.clone()))
    }
}

// ── Input ──

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

// ── Execution ──

async fn run(raw: serde_json::Value, read_deny: ReadDeny) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let Input { pattern, path } = input;

    match tokio::task::spawn_blocking(move || {
        super::working_dir().and_then(|cwd| glob_files(&pattern, path.as_deref(), &cwd, &read_deny))
    })
    .await
    {
        Ok(Ok(GlobOutput {
            content,
            truncated_total,
        })) => {
            let title = glob_title(super::strip_withheld_note(&content));
            let output = ToolOutput::success(content).with_title(title);
            match truncated_total {
                Some(total) => output.with_truncated_total(total),
                None => output,
            }
        }
        Ok(Err(msg)) => ToolOutput::error(msg),
        Err(e) => ToolOutput::error(format!("Internal error: {e}")),
    }
}

fn glob_title(output: &str) -> String {
    if output == "No files found" {
        return output.to_owned();
    }
    let body = glob_output_body(output).unwrap_or(output.trim_end());
    let count = body.lines().filter(|l| !l.trim().is_empty()).count();
    let word = if count == 1 { "file" } else { "files" };
    format!("Found {count} {word}")
}

#[derive(Debug)]
struct GlobOutput {
    content: String,
    truncated_total: Option<usize>,
}

/// Walks `search_path` (resolved against `cwd`, defaulting to it), filters entries against
/// `pattern`, and returns up to `MAX_RESULTS` matches sorted newest-first by mtime, rendered
/// relative to the canonical `cwd`. Matches `read_deny` withholds are counted in a trailing note.
///
/// `walk_files` skips files matched by `.gitignore` or a global ignore file, so only non-ignored
/// files reach the matcher. Truncation past `MAX_RESULTS` is announced inline and surfaced
/// separately on `truncated_total` for the structured view.
fn glob_files(
    pattern: &str,
    search_path: Option<&str>,
    cwd: &Path,
    read_deny: &ReadDeny,
) -> Result<GlobOutput, String> {
    let base = super::resolve_base_dir(search_path, cwd)?;
    if !base.is_dir() {
        return Err(format!(
            "{} is not a directory",
            super::display_path(&base, cwd)
        ));
    }

    let glob = compile_glob(pattern)?;

    let read_deny = read_deny.walk(&base, Path::new(search_path.unwrap_or(".")), cwd);
    let mut withheld = 0;
    let mut matches: Vec<(String, std::time::SystemTime)> = super::walk_files(&base)
        .filter(|entry| {
            let rel = entry.path().strip_prefix(&base).unwrap_or(entry.path());
            glob.is_match(rel)
        })
        .filter(|entry| {
            let denied = read_deny.denies(entry.path());
            withheld += usize::from(denied);
            !denied
        })
        .map(|entry| {
            let mtime = super::entry_mtime(&entry);
            (super::display_path(entry.path(), cwd), mtime)
        })
        .collect();

    matches.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    if matches.is_empty() {
        let mut content = "No files found".to_owned();
        super::push_withheld_note(&mut content, withheld);
        return Ok(GlobOutput {
            content,
            truncated_total: None,
        });
    }

    let total = matches.len();
    let truncated = total > MAX_RESULTS;
    matches.truncate(MAX_RESULTS);

    let mut content = String::new();
    for (i, (p, _)) in matches.iter().enumerate() {
        if i > 0 {
            content.push('\n');
        }
        content.push_str(p);
    }

    if truncated {
        _ = write!(
            content,
            "\n\n(Showing {MAX_RESULTS} of {total} matches. Use a more specific pattern.)"
        );
    }
    super::push_withheld_note(&mut content, withheld);

    Ok(GlobOutput {
        content,
        truncated_total: truncated.then_some(total),
    })
}

/// Compiles `pattern` relative to the search root, so `*` and `?` never cross `/` and a leading
/// `./` anchors at the root. A pattern without `/` gains a `**/` prefix and matches file names at
/// any depth, the way gitignore-style globs behave. Absolute patterns are rejected because the
/// permission gate only sees `path`.
fn compile_glob(pattern: &str) -> Result<globset::GlobMatcher, String> {
    if Path::new(pattern).has_root() {
        return Err(format!(
            "Pattern {pattern} is absolute. Set `path` to its directory and pass a pattern relative to it."
        ));
    }
    let pattern = if pattern.contains('/') {
        pattern.trim_start_matches("./").to_owned()
    } else {
        format!("**/{pattern}")
    };
    globset::GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|e| format!("Invalid glob pattern: {e}"))
}

// ── Result View ──

fn build_files_view(
    input: &serde_json::Value,
    content: &str,
    metadata: &ToolMetadata,
) -> Option<ToolResultView> {
    let pattern = extract_input_field(input, "pattern")?.to_owned();
    let trimmed = content.trim_end();
    if trimmed == "No files found" {
        return Some(ToolResultView::GlobFiles {
            pattern,
            files: Vec::new(),
            total: 0,
        });
    }

    let body = glob_output_body(trimmed)?;
    let footer_total = glob_footer_total(trimmed);

    let files: Vec<String> = body.lines().map(str::to_owned).collect();
    if files.is_empty() {
        return None;
    }
    let total = metadata
        .truncated_total
        .or(footer_total)
        .unwrap_or(files.len());
    if total < files.len() {
        return None;
    }
    Some(ToolResultView::GlobFiles {
        pattern,
        files,
        total,
    })
}

fn is_truncation_footer(footer: &str) -> bool {
    footer.starts_with("(Showing ")
}

fn glob_output_body(content: &str) -> Option<&str> {
    let trimmed = content.trim_end();
    match trimmed.rsplit_once("\n\n") {
        Some((body, footer)) if is_truncation_footer(footer) => Some(body),
        Some(_) => None,
        None => Some(trimmed),
    }
}

fn glob_footer_total(content: &str) -> Option<usize> {
    let (_, footer) = content.trim_end().rsplit_once("\n\n")?;
    parse_total_from_footer(footer)
}

fn parse_total_from_footer(footer: &str) -> Option<usize> {
    footer
        .strip_prefix('(')?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use indoc::{formatdoc, indoc};

    use super::*;
    use crate::permission::{Mode, Policy};
    use crate::tool::testing::canonical_tempdir;
    use crate::util::path::slash_separated;

    // ── gate_target ──

    #[test]
    fn gate_target_defaults_the_search_root_to_cwd() {
        // With no `path`, the search root is cwd, so a deny scoped to the project root still applies.
        let dir = tempfile::tempdir().unwrap();
        let cwd = dunce::canonicalize(dir.path()).unwrap();

        let target = GlobTool::default().gate_target(&serde_json::json!({"pattern": "*.rs"}), &cwd);
        let crate::permission::GateTarget::Path { relative, .. } = target else {
            panic!("expected a path target");
        };
        assert_eq!(relative.as_deref(), Some(""));
    }

    // ── run ──

    #[tokio::test]
    async fn run_finds_files() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();

        let output = GlobTool::default()
            .run(serde_json::json!({
                "pattern": "*.txt",
                "path": dir.path().to_str().unwrap()
            }))
            .await;

        assert!(!output.is_error);
        assert!(output.content.contains("a.txt"));
        assert!(!output.content.contains("b.rs"));
        assert!(output.metadata.truncated_total.is_none());
    }

    #[tokio::test]
    async fn run_missing_pattern() {
        let output = GlobTool::default().run(serde_json::json!({})).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_invalid_pattern_omits_title() {
        let output = GlobTool::default()
            .run(serde_json::json!({"pattern": "[unclosed"}))
            .await;
        assert!(output.is_error);
        assert!(
            output.content.contains("Invalid glob pattern"),
            "{}",
            output.content
        );
        assert_eq!(output.metadata.title, None, "errors are not a file count");
    }

    #[tokio::test]
    async fn run_truncated_attaches_total_to_metadata() {
        let dir = canonical_tempdir();
        let total = MAX_RESULTS + 10;
        for i in 0..total {
            std::fs::write(dir.path().join(format!("{i:04}.txt")), "").unwrap();
        }

        let output = GlobTool::default()
            .run(serde_json::json!({
                "pattern": "*.txt",
                "path": dir.path().to_str().unwrap(),
            }))
            .await;

        assert!(!output.is_error);
        assert_eq!(output.metadata.truncated_total, Some(total));
    }

    // ── glob_title ──

    #[test]
    fn glob_title_counts_paths_without_truncation_footer_separator() {
        let body = indoc! {"
            a.txt
            b.txt

            (Showing 2 of 3 matches. Use a more specific pattern.)"
        };

        assert_eq!(glob_title(body), "Found 2 files");
    }

    #[test]
    fn glob_title_no_files_found_is_not_a_count() {
        assert_eq!(glob_title("No files found"), "No files found");
    }

    // ── glob_files ──

    #[test]
    fn glob_files_basic() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("foo.txt"), "").unwrap();
        std::fs::write(dir.path().join("bar.txt"), "").unwrap();
        std::fs::write(dir.path().join("baz.rs"), "").unwrap();

        let result = glob_files("*.txt", None, dir.path(), &ReadDeny::default()).unwrap();
        let mut files: Vec<&str> = result.content.lines().collect();
        files.sort_unstable();
        assert_eq!(files, ["bar.txt", "foo.txt"]);
        assert!(result.truncated_total.is_none());
    }

    #[test]
    fn glob_files_recursive() {
        let dir = canonical_tempdir();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(dir.path().join("top.rs"), "").unwrap();
        std::fs::write(sub.join("nested.rs"), "").unwrap();

        let result = glob_files("**/*.rs", None, dir.path(), &ReadDeny::default()).unwrap();
        assert!(result.content.lines().any(|l| l == "top.rs"));
        assert!(result.content.lines().any(|l| l == "sub/nested.rs"));
    }

    #[test]
    fn glob_files_star_stays_within_one_directory() {
        let dir = canonical_tempdir();
        let nested = dir.path().join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
        std::fs::write(nested.join("b.rs"), "").unwrap();

        let scoped = glob_files("src/*.rs", None, dir.path(), &ReadDeny::default()).unwrap();
        assert_eq!(scoped.content, "src/a.rs");

        // A pattern without `/` matches the file name at any depth.
        let bare = glob_files("b.rs", None, dir.path(), &ReadDeny::default()).unwrap();
        assert_eq!(bare.content, "src/nested/b.rs");
    }

    #[test]
    fn glob_files_dot_slash_pattern_anchors_at_the_search_root() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/b.rs"), "").unwrap();

        assert_eq!(
            glob_files("./*.rs", None, dir.path(), &ReadDeny::default())
                .unwrap()
                .content,
            "a.rs"
        );
        assert_eq!(
            glob_files("././src/*.rs", None, dir.path(), &ReadDeny::default())
                .unwrap()
                .content,
            "src/b.rs"
        );
    }

    #[test]
    fn glob_files_renders_paths_relative_to_cwd() {
        let dir = canonical_tempdir();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::write(cwd.join("src/lib.rs"), "").unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("x.rs"), "").unwrap();

        let inside = glob_files("*.rs", Some("src"), &cwd, &ReadDeny::default()).unwrap();
        assert_eq!(inside.content, "src/lib.rs");

        let outside_hit =
            glob_files("*.rs", Some("../outside"), &cwd, &ReadDeny::default()).unwrap();
        assert_eq!(
            outside_hit.content,
            slash_separated(&outside.join("x.rs")),
            "hits outside cwd fall back to an absolute path",
        );
    }

    #[test]
    fn glob_files_sorted_by_mtime() {
        let dir = canonical_tempdir();

        std::fs::write(dir.path().join("old.txt"), "old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(dir.path().join("new.txt"), "new").unwrap();

        let result = glob_files("*.txt", None, dir.path(), &ReadDeny::default()).unwrap();
        assert_eq!(result.content, "new.txt\nold.txt");
    }

    #[test]
    fn glob_files_no_matches() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();

        let result = glob_files("*.rs", None, dir.path(), &ReadDeny::default()).unwrap();
        assert_eq!(result.content, "No files found");
        assert!(result.truncated_total.is_none());
    }

    #[test]
    fn glob_files_invalid_pattern() {
        let dir = canonical_tempdir();
        let err = glob_files("[invalid", None, dir.path(), &ReadDeny::default()).unwrap_err();
        assert!(err.contains("Invalid glob pattern"));
    }

    #[test]
    fn glob_files_absolute_pattern_points_at_path() {
        let dir = canonical_tempdir();
        let pattern = format!("{}/*.rs", dir.path().display());
        let err = glob_files(&pattern, None, dir.path(), &ReadDeny::default()).unwrap_err();
        assert_eq!(
            err,
            format!(
                "Pattern {pattern} is absolute. Set `path` to its directory and pass a pattern relative to it."
            )
        );
    }

    #[test]
    fn glob_files_invalid_directory() {
        let dir = canonical_tempdir();
        let err =
            glob_files("*.txt", Some("missing"), dir.path(), &ReadDeny::default()).unwrap_err();
        assert!(err.starts_with("Cannot access missing: "), "{err}");

        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let err = glob_files("*.txt", Some("a.txt"), dir.path(), &ReadDeny::default()).unwrap_err();
        assert_eq!(err, "a.txt is not a directory");
    }

    #[test]
    fn glob_files_truncated_at_max_results() {
        let dir = canonical_tempdir();
        for i in 0..MAX_RESULTS + 10 {
            std::fs::write(dir.path().join(format!("{i:04}.txt")), "").unwrap();
        }

        let result = glob_files("*.txt", None, dir.path(), &ReadDeny::default()).unwrap();
        let file_count = result
            .content
            .lines()
            .filter(|l| l.contains(".txt"))
            .count();
        assert_eq!(file_count, MAX_RESULTS);
        assert!(
            result
                .content
                .contains(&format!("Showing {MAX_RESULTS} of {}", MAX_RESULTS + 10))
        );
        assert_eq!(result.truncated_total, Some(MAX_RESULTS + 10));
    }

    #[test]
    fn glob_files_withholds_paths_a_read_deny_rule_matches() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        std::fs::create_dir(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/key.toml"), "").unwrap();
        let deny = ["read(secrets/**)".to_owned()];
        let read_deny = Policy::resolve(Mode::Auto, &[], &deny, dir.path())
            .unwrap()
            .read_deny();

        let result = glob_files("*.toml", None, dir.path(), &read_deny).unwrap();
        assert_eq!(
            result.content,
            "config.toml\n\n(1 file withheld by read deny rules)"
        );
        assert_eq!(result.truncated_total, None);
        assert_eq!(
            glob_title(crate::tool::strip_withheld_note(&result.content)),
            "Found 1 file"
        );
        // The structured view would drop the note, so the text view shows it.
        let view = GlobTool::default().result_view(
            &serde_json::json!({"pattern": "*.toml"}),
            &result.content,
            &ToolMetadata::default(),
        );
        assert_eq!(view, None);

        let result = glob_files("secrets/*", None, dir.path(), &read_deny).unwrap();
        assert_eq!(
            result.content,
            "No files found\n\n(1 file withheld by read deny rules)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn glob_files_withholds_a_denied_file_below_a_symlinked_root() {
        let dir = canonical_tempdir();
        let (cwd, outside) = (dir.path().join("cwd"), dir.path().join("outside"));
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join(".env"), "").unwrap();
        std::fs::write(outside.join("app.toml"), "").unwrap();
        std::os::unix::fs::symlink(&outside, cwd.join("config")).unwrap();
        let deny = ["read(**/.env)".to_owned()];
        let read_deny = Policy::resolve(Mode::Auto, &[], &deny, &cwd)
            .unwrap()
            .read_deny();

        let result = glob_files("*", Some("config"), &cwd, &read_deny).unwrap();
        assert_eq!(
            result.content,
            format!(
                "{}\n\n(1 file withheld by read deny rules)",
                outside.join("app.toml").display()
            )
        );
    }

    #[test]
    fn glob_files_includes_hidden_paths_but_skips_vcs_dirs() {
        let dir = canonical_tempdir();
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("ci.yml"), "").unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::write(git.join("hook.yml"), "").unwrap();

        let result = glob_files(
            ".github/workflows/*.yml",
            None,
            dir.path(),
            &ReadDeny::default(),
        )
        .unwrap();
        assert_eq!(result.content, ".github/workflows/ci.yml");

        let all = glob_files("*.yml", None, dir.path(), &ReadDeny::default()).unwrap();
        assert_eq!(all.content, ".github/workflows/ci.yml");
    }

    #[test]
    fn glob_files_respects_gitignore() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "").unwrap();

        let result = glob_files("*.txt", None, dir.path(), &ReadDeny::default()).unwrap();
        assert_eq!(result.content, "tracked.txt");
    }

    // ── result_view ──

    #[test]
    fn result_view_builds_glob_files() {
        let view = GlobTool::default()
            .result_view(
                &serde_json::json!({"pattern": "*.rs"}),
                indoc! {"
                    src/main.rs
                    src/lib.rs"
                },
                &ToolMetadata::default(),
            )
            .unwrap();

        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                pattern: "*.rs".to_owned(),
                files: vec!["src/main.rs".to_owned(), "src/lib.rs".to_owned()],
                total: 2,
            },
        );
    }

    #[test]
    fn result_view_pulls_total_from_metadata_and_drops_prose_footer() {
        let files: Vec<String> = (0..MAX_RESULTS).map(|i| format!("f{i:03}.rs")).collect();
        let body = files.join("\n");
        let content = formatdoc! {"
            {body}

            (Showing {MAX_RESULTS} of 1234 matches. Use a more specific pattern.)"
        };
        let metadata = ToolMetadata {
            truncated_total: Some(1234),
            ..ToolMetadata::default()
        };
        let view = GlobTool::default()
            .result_view(
                &serde_json::json!({"pattern": "**/*.rs"}),
                &content,
                &metadata,
            )
            .unwrap();

        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                pattern: "**/*.rs".to_owned(),
                files,
                total: 1234,
            },
        );
    }

    #[test]
    fn result_view_recovers_total_from_prose_footer_for_legacy_sessions() {
        let files: Vec<String> = (0..MAX_RESULTS).map(|i| format!("f{i:03}.rs")).collect();
        let body = files.join("\n");
        let content = formatdoc! {"
            {body}

            (Showing {MAX_RESULTS} of 1234 matches. Use a more specific pattern.)"
        };
        let view = GlobTool::default()
            .result_view(
                &serde_json::json!({"pattern": "**/*.rs"}),
                &content,
                &ToolMetadata::default(),
            )
            .unwrap();

        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                pattern: "**/*.rs".to_owned(),
                files,
                total: 1234,
            },
        );
    }

    #[test]
    fn result_view_handles_no_files_found() {
        let view = GlobTool::default()
            .result_view(
                &serde_json::json!({"pattern": "*.nope"}),
                "No files found",
                &ToolMetadata::default(),
            )
            .unwrap();

        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                pattern: "*.nope".to_owned(),
                files: Vec::new(),
                total: 0,
            },
        );
    }

    #[test]
    fn result_view_falls_back_for_empty_content() {
        let view = GlobTool::default().result_view(
            &serde_json::json!({"pattern": "*.rs"}),
            "",
            &ToolMetadata::default(),
        );
        assert!(view.is_none());
    }

    #[test]
    fn result_view_falls_back_when_input_has_no_pattern() {
        let view = GlobTool::default().result_view(
            &serde_json::json!({}),
            "src/main.rs",
            &ToolMetadata::default(),
        );
        assert!(view.is_none());
    }

    #[test]
    fn result_view_single_file_no_footer() {
        let view = GlobTool::default()
            .result_view(
                &serde_json::json!({"pattern": "*.rs"}),
                "src/only.rs",
                &ToolMetadata::default(),
            )
            .unwrap();
        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                pattern: "*.rs".to_owned(),
                files: vec!["src/only.rs".to_owned()],
                total: 1,
            },
        );
    }

    #[test]
    fn result_view_normalises_trailing_newline() {
        let view = GlobTool::default()
            .result_view(
                &serde_json::json!({"pattern": "*.rs"}),
                "a.rs\nb.rs\n",
                &ToolMetadata::default(),
            )
            .unwrap();
        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                pattern: "*.rs".to_owned(),
                files: vec!["a.rs".to_owned(), "b.rs".to_owned()],
                total: 2,
            },
        );
    }

    #[test]
    fn result_view_metadata_total_equal_to_file_count_succeeds() {
        let metadata = ToolMetadata {
            truncated_total: Some(2),
            ..ToolMetadata::default()
        };
        let view = GlobTool::default()
            .result_view(
                &serde_json::json!({"pattern": "*.rs"}),
                "a.rs\nb.rs",
                &metadata,
            )
            .unwrap();
        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                pattern: "*.rs".to_owned(),
                files: vec!["a.rs".to_owned(), "b.rs".to_owned()],
                total: 2,
            },
        );
    }

    #[test]
    fn result_view_path_with_embedded_blank_line_falls_back() {
        let view = build_files_view(
            &serde_json::json!({"pattern": "*.rs"}),
            indoc! {"
                weird

                name.rs
                next.rs"
            },
            &ToolMetadata::default(),
        );
        assert!(view.is_none());
    }

    #[test]
    fn result_view_falls_back_when_metadata_total_under_visible_files() {
        let metadata = ToolMetadata {
            truncated_total: Some(1),
            ..ToolMetadata::default()
        };
        let view = GlobTool::default().result_view(
            &serde_json::json!({"pattern": "*.rs"}),
            "a.rs\nb.rs",
            &metadata,
        );
        assert!(view.is_none());
    }

    #[test]
    fn result_view_falls_back_for_unrecognised_trailing_prose() {
        let view = GlobTool::default().result_view(
            &serde_json::json!({"pattern": "*.rs"}),
            indoc! {"
                src/main.rs

                (Some other footer we don't recognise)"
            },
            &ToolMetadata::default(),
        );
        assert!(view.is_none());
    }

    // ── is_truncation_footer ──

    #[test]
    fn is_truncation_footer_accepts_glob_files_shape() {
        assert!(is_truncation_footer(
            "(Showing 100 of 250 matches. Use a more specific pattern.)"
        ));
    }

    #[test]
    fn is_truncation_footer_rejects_unknown_prose() {
        assert!(!is_truncation_footer("(Some footer)"));
        assert!(!is_truncation_footer("Showing 100 of 250"));
        assert!(!is_truncation_footer(""));
    }

    // ── parse_total_from_footer ──

    #[test]
    fn parse_total_from_footer_extracts_y_from_glob_files_shape() {
        assert_eq!(
            parse_total_from_footer("(Showing 100 of 1234 matches. Use a more specific pattern.)"),
            Some(1234),
        );
    }

    #[test]
    fn parse_total_from_footer_is_none_for_malformed_input() {
        assert_eq!(
            parse_total_from_footer("Showing 100 of 1234 matches."),
            None,
        );
        assert_eq!(
            parse_total_from_footer("(Showing 100 of many matches.)"),
            None
        );
        assert_eq!(parse_total_from_footer("(Showing 100 of)"), None);
    }
}
