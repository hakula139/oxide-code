use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolMetadata, ToolOutput, ToolResultView, extract_input_field};

const MAX_RESULTS: usize = 100;

pub(crate) struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files matching a glob pattern. Returns paths sorted by modification time (newest first)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": r#"The glob pattern to match (e.g. "**/*.rs", "src/**/*.ts")"#
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
        _input: &serde_json::Value,
        content: &str,
        _metadata: &ToolMetadata,
    ) -> Option<ToolResultView> {
        parse_files_view(content)
    }

    fn run(
        &self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(run(input))
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

async fn run(raw: serde_json::Value) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let Input { pattern, path } = input;

    match tokio::task::spawn_blocking(move || glob_files(&pattern, path.as_deref())).await {
        Ok(result) => {
            let title = glob_title(result.as_deref().ok());
            ToolOutput::from_result(result).with_title(title)
        }
        Err(e) => ToolOutput {
            content: format!("Internal error: {e}"),
            is_error: true,
            metadata: super::ToolMetadata::default(),
        },
    }
}

fn glob_title(output: Option<&str>) -> String {
    match output {
        Some("No files found") | None => "No files found".into(),
        Some(text) => {
            let count = text.lines().filter(|l| !l.starts_with('(')).count();
            let word = if count == 1 { "file" } else { "files" };
            format!("Found {count} {word}")
        }
    }
}

fn glob_files(pattern: &str, search_path: Option<&str>) -> Result<String, String> {
    let base = super::resolve_base_dir(search_path)?;
    if !base.is_dir() {
        return Err(format!("Directory does not exist: {}", base.display()));
    }

    let glob = globset::Glob::new(pattern)
        .map_err(|e| format!("Invalid glob pattern: {e}"))?
        .compile_matcher();

    let mut matches: Vec<(String, std::time::SystemTime)> = super::walk_files(&base)
        .filter(|entry| {
            let rel = entry.path().strip_prefix(&base).unwrap_or(entry.path());
            glob.is_match(rel)
        })
        .map(|entry| {
            let mtime = super::entry_mtime(&entry);
            (super::display_path(entry.path(), &base), mtime)
        })
        .collect();

    matches.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    if matches.is_empty() {
        return Ok("No files found".into());
    }

    let truncated = matches.len() > MAX_RESULTS;
    let total = matches.len();
    matches.truncate(MAX_RESULTS);

    let mut output = String::new();
    for (i, (p, _)) in matches.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        output.push_str(p);
    }

    if truncated {
        _ = write!(
            output,
            "\n\n(Showing {MAX_RESULTS} of {total} matches. Use a more specific pattern.)"
        );
    }

    Ok(output)
}

// ── Result View ──

/// Parses glob output into a [`ToolResultView::GlobFiles`]. Output
/// shape: a `\n`-joined list of paths optionally followed by
/// `\n\n(Showing 100 of N matches. Use a more specific pattern.)`.
/// Returns `None` for any line that can't be classified so malformed
/// output falls through to the raw text body instead of silently
/// dropping rows.
fn parse_files_view(content: &str) -> Option<ToolResultView> {
    let trimmed = content.trim_end();
    if trimmed == "No files found" {
        return Some(ToolResultView::GlobFiles {
            files: Vec::new(),
            total: 0,
        });
    }

    let (body, total) = match trimmed.rsplit_once("\n\n") {
        Some((body, footer)) => (body, Some(parse_truncation_footer(footer)?)),
        None => (trimmed, None),
    };

    let files: Vec<String> = body.lines().map(str::to_owned).collect();
    // Empty body protects against shapes like `\n\n(Showing 0 of 0 matches.)`
    // where everything is footer — fall through to text rather than render
    // a structured view with zero rows but a non-zero total.
    if files.is_empty() {
        return None;
    }
    let total = total.unwrap_or(files.len());
    if total < files.len() {
        return None;
    }
    Some(ToolResultView::GlobFiles { files, total })
}

/// Parses glob's `(Showing X of Y matches. ...)` footer and returns the
/// total. `None` flags a footer that we couldn't classify so the caller
/// falls back to the raw text body instead of dropping the line.
fn parse_truncation_footer(footer: &str) -> Option<usize> {
    let footer = footer.trim();
    let inner = footer.strip_prefix("(Showing ")?.strip_suffix(')')?;
    let (_, rest) = inner.split_once(" of ")?;
    rest.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use indoc::{formatdoc, indoc};

    use super::*;

    // ── run ──

    #[tokio::test]
    async fn run_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();

        let output = run(serde_json::json!({
            "pattern": "*.txt",
            "path": dir.path().to_str().unwrap()
        }))
        .await;

        assert!(!output.is_error);
        assert!(output.content.contains("a.txt"));
        assert!(!output.content.contains("b.rs"));
    }

    #[tokio::test]
    async fn run_missing_pattern() {
        let output = run(serde_json::json!({})).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    // ── glob_files ──

    #[test]
    fn glob_files_basic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo.txt"), "").unwrap();
        std::fs::write(dir.path().join("bar.txt"), "").unwrap();
        std::fs::write(dir.path().join("baz.rs"), "").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.contains("foo.txt"));
        assert!(result.contains("bar.txt"));
        assert!(!result.contains("baz.rs"));
    }

    #[test]
    fn glob_files_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(dir.path().join("top.rs"), "").unwrap();
        std::fs::write(sub.join("nested.rs"), "").unwrap();

        let result = glob_files("**/*.rs", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.contains("top.rs"));
        assert!(result.contains("nested.rs"));
    }

    #[test]
    fn glob_files_sorted_by_mtime() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(dir.path().join("old.txt"), "old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(dir.path().join("new.txt"), "new").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("new.txt"));
        assert!(lines[1].contains("old.txt"));
    }

    #[test]
    fn glob_files_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();

        let result = glob_files("*.rs", Some(dir.path().to_str().unwrap())).unwrap();
        assert_eq!(result, "No files found");
    }

    #[test]
    fn glob_files_invalid_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let err = glob_files("[invalid", Some(dir.path().to_str().unwrap())).unwrap_err();
        assert!(err.contains("Invalid glob pattern"));
    }

    #[test]
    fn glob_files_invalid_directory() {
        let err = glob_files("*.txt", Some("/nonexistent/dir")).unwrap_err();
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn glob_files_truncated_at_max_results() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_RESULTS + 10 {
            std::fs::write(dir.path().join(format!("{i:04}.txt")), "").unwrap();
        }

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        let file_count = result.lines().filter(|l| l.contains(".txt")).count();
        assert_eq!(file_count, MAX_RESULTS);
        assert!(result.contains(&format!("Showing {MAX_RESULTS} of {}", MAX_RESULTS + 10)));
    }

    #[test]
    fn glob_files_skips_hidden_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("secret.txt"), "").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.contains("visible.txt"));
        assert!(!result.contains(".hidden"));
    }

    #[test]
    fn glob_files_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.contains("tracked.txt"));
        assert!(!result.contains("ignored.txt"));
    }

    // ── result_view ──

    #[test]
    fn result_view_builds_glob_files() {
        let view = GlobTool
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
                files: vec!["src/main.rs".to_owned(), "src/lib.rs".to_owned()],
                total: 2,
            },
        );
    }

    #[test]
    fn result_view_preserves_total_from_truncation_footer() {
        // Files vector reflects the tool's MAX_RESULTS cap; total
        // preserves the unbounded count so the renderer can surface it.
        let files: Vec<String> = (0..MAX_RESULTS).map(|i| format!("f{i:03}.rs")).collect();
        let body = files.join("\n");
        let content = formatdoc! {"
            {body}

            (Showing {MAX_RESULTS} of 1234 matches. Use a more specific pattern.)"
        };
        let view = GlobTool
            .result_view(
                &serde_json::json!({"pattern": "**/*.rs"}),
                &content,
                &ToolMetadata::default(),
            )
            .unwrap();

        assert_eq!(view, ToolResultView::GlobFiles { files, total: 1234 });
    }

    #[test]
    fn result_view_handles_no_files_found() {
        let view = GlobTool
            .result_view(
                &serde_json::json!({"pattern": "*.nope"}),
                "No files found",
                &ToolMetadata::default(),
            )
            .unwrap();

        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                files: Vec::new(),
                total: 0,
            },
        );
    }

    #[test]
    fn result_view_falls_back_for_empty_content() {
        // `body.lines()` yields nothing — falling through to text shows
        // the user the raw output instead of a misleading empty list.
        let view = GlobTool.result_view(
            &serde_json::json!({"pattern": "*.rs"}),
            "",
            &ToolMetadata::default(),
        );
        assert!(view.is_none());
    }

    #[test]
    fn result_view_single_file_no_footer() {
        // Off-by-one guard for the `files.is_empty()` boundary. Also
        // pins `total` to derived `files.len()` when no footer present.
        let view = GlobTool
            .result_view(
                &serde_json::json!({"pattern": "*.rs"}),
                "src/only.rs",
                &ToolMetadata::default(),
            )
            .unwrap();
        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                files: vec!["src/only.rs".to_owned()],
                total: 1,
            },
        );
    }

    #[test]
    fn result_view_normalises_trailing_newline() {
        // glob_files never emits a trailing newline today, but `trim_end`
        // means we tolerate one — pin the contract so a future producer
        // change doesn't shift this silently.
        let view = GlobTool
            .result_view(
                &serde_json::json!({"pattern": "*.rs"}),
                "a.rs\nb.rs\n",
                &ToolMetadata::default(),
            )
            .unwrap();
        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                files: vec!["a.rs".to_owned(), "b.rs".to_owned()],
                total: 2,
            },
        );
    }

    #[test]
    fn result_view_footer_total_equal_to_file_count_succeeds() {
        // Boundary of the `total < files.len()` guard. Pinning equality
        // here keeps the comparator from drifting to `<=` or `==` —
        // mutants that would otherwise pass every other test.
        let view = GlobTool
            .result_view(
                &serde_json::json!({"pattern": "*.rs"}),
                indoc! {"
                    a.rs
                    b.rs

                    (Showing 2 of 2 matches. Use a more specific pattern.)"
                },
                &ToolMetadata::default(),
            )
            .unwrap();
        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                files: vec!["a.rs".to_owned(), "b.rs".to_owned()],
                total: 2,
            },
        );
    }

    #[test]
    fn result_view_path_with_embedded_blank_line_falls_back() {
        // Unix paths can technically contain `\n`; back-to-back newlines
        // would let the parser mistake the rest of the body for a
        // truncation footer. `rsplit_once` anchors the footer at the end,
        // and a body section that doesn't parse as a footer triggers
        // text-fallback instead of dropping rows.
        let view = parse_files_view("weird\n\nname.rs\nnext.rs");
        assert!(view.is_none());
    }

    #[test]
    fn result_view_falls_back_when_footer_total_under_visible_files() {
        // Inconsistent footer — claims fewer total matches than the
        // visible body. Render-time math depends on `total >= files.len()`,
        // so reject up front.
        let view = GlobTool.result_view(
            &serde_json::json!({"pattern": "*.rs"}),
            indoc! {"
                a.rs
                b.rs

                (Showing 100 of 1 matches. Use a more specific pattern.)"
            },
            &ToolMetadata::default(),
        );
        assert!(view.is_none());
    }

    #[test]
    fn result_view_falls_back_for_malformed_footer() {
        // Footer present but unparsable — fall through to the text body
        // rather than absorb the line as a "path" and render misleading
        // structure.
        let view = GlobTool.result_view(
            &serde_json::json!({"pattern": "*.rs"}),
            indoc! {"
                src/main.rs

                (Some other footer we don't recognise)"
            },
            &ToolMetadata::default(),
        );
        assert!(view.is_none());
    }

    // ── parse_truncation_footer ──

    #[test]
    fn parse_truncation_footer_extracts_total() {
        assert_eq!(
            parse_truncation_footer("(Showing 100 of 250 matches. Use a more specific pattern.)"),
            Some(250),
        );
    }

    #[test]
    fn parse_truncation_footer_rejects_unrecognised_input() {
        assert!(parse_truncation_footer("(Some footer)").is_none());
        assert!(parse_truncation_footer("(Showing 100 of NaN matches.)").is_none());
    }

    #[test]
    fn parse_truncation_footer_ignores_trailing_prose() {
        // The prose after the count can change without breaking the
        // parser — only the leading `(Showing X of Y` shape carries the
        // contract.
        assert_eq!(
            parse_truncation_footer("(Showing 100 of 250 matches. Try a tighter glob.)"),
            Some(250),
        );
    }
}
