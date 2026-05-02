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
            let title = glob_title(result.as_ref().ok().map(|r| r.content.as_str()));
            let truncated_total = result.as_ref().ok().and_then(|r| r.truncated_total);
            let mut output = ToolOutput::from_result(result.map(|r| r.content)).with_title(title);
            if let Some(total) = truncated_total {
                output = output.with_truncated_total(total);
            }
            output
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

/// `glob_files` return: model-facing prose plus the unbounded match
/// count when the result was capped at [`MAX_RESULTS`].
#[derive(Debug)]
struct GlobOutput {
    content: String,
    truncated_total: Option<usize>,
}

fn glob_files(pattern: &str, search_path: Option<&str>) -> Result<GlobOutput, String> {
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
        return Ok(GlobOutput {
            content: "No files found".into(),
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

    Ok(GlobOutput {
        content,
        truncated_total: truncated.then_some(total),
    })
}

// ── Result View ──

/// Builds a [`ToolResultView::GlobFiles`] from `glob_files` output.
/// Returns `None` for malformed shapes so they fall through to the
/// raw text body instead of silently dropping rows. The unbounded
/// match count prefers `metadata.truncated_total`; for sessions
/// whose JSONL predates that field, the count is recovered from the
/// `(Showing X of Y matches. ...)` prose still baked into `content`.
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

    let (body, footer_total) = match trimmed.rsplit_once("\n\n") {
        Some((body, footer)) if is_truncation_footer(footer) => {
            (body, parse_total_from_footer(footer))
        }
        // Unknown trailing prose: fall through rather than absorb it as a path.
        Some(_) => return None,
        None => (trimmed, None),
    };

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

/// Matches the `(Showing X of Y matches. ...)` footer emitted by
/// [`glob_files`] — gates trailing prose so unknown shapes fall
/// through instead of getting absorbed as a path.
fn is_truncation_footer(footer: &str) -> bool {
    footer.starts_with("(Showing ")
}

/// Extracts `Y` from `(Showing X of Y matches. ...)`. Caller must
/// have already gated the shape via [`is_truncation_footer`].
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
        assert!(output.metadata.truncated_total.is_none());
    }

    #[tokio::test]
    async fn run_missing_pattern() {
        let output = run(serde_json::json!({})).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_truncated_attaches_total_to_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let total = MAX_RESULTS + 10;
        for i in 0..total {
            std::fs::write(dir.path().join(format!("{i:04}.txt")), "").unwrap();
        }

        let output = run(serde_json::json!({
            "pattern": "*.txt",
            "path": dir.path().to_str().unwrap(),
        }))
        .await;

        assert!(!output.is_error);
        assert_eq!(output.metadata.truncated_total, Some(total));
    }

    // ── glob_files ──

    #[test]
    fn glob_files_basic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo.txt"), "").unwrap();
        std::fs::write(dir.path().join("bar.txt"), "").unwrap();
        std::fs::write(dir.path().join("baz.rs"), "").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.content.contains("foo.txt"));
        assert!(result.content.contains("bar.txt"));
        assert!(!result.content.contains("baz.rs"));
        assert!(result.truncated_total.is_none());
    }

    #[test]
    fn glob_files_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(dir.path().join("top.rs"), "").unwrap();
        std::fs::write(sub.join("nested.rs"), "").unwrap();

        let result = glob_files("**/*.rs", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.content.contains("top.rs"));
        assert!(result.content.contains("nested.rs"));
    }

    #[test]
    fn glob_files_sorted_by_mtime() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(dir.path().join("old.txt"), "old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(dir.path().join("new.txt"), "new").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        let lines: Vec<&str> = result.content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("new.txt"));
        assert!(lines[1].contains("old.txt"));
    }

    #[test]
    fn glob_files_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();

        let result = glob_files("*.rs", Some(dir.path().to_str().unwrap())).unwrap();
        assert_eq!(result.content, "No files found");
        assert!(result.truncated_total.is_none());
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
    fn glob_files_skips_hidden_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("secret.txt"), "").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.content.contains("visible.txt"));
        assert!(!result.content.contains(".hidden"));
    }

    #[test]
    fn glob_files_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.content.contains("tracked.txt"));
        assert!(!result.content.contains("ignored.txt"));
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
                pattern: "*.rs".to_owned(),
                files: vec!["src/main.rs".to_owned(), "src/lib.rs".to_owned()],
                total: 2,
            },
        );
    }

    #[test]
    fn result_view_pulls_total_from_metadata_and_drops_prose_footer() {
        // Trailing `(Showing ...)` prose is dropped from the file
        // list; metadata wins over the prose count when both are
        // present (the live path).
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
        let view = GlobTool
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
        // Sessions recorded before `truncated_total` landed in metadata
        // still carry the count in the prose footer. Recovering it
        // keeps the rendered "X of Y" honest after resume.
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
                pattern: "*.nope".to_owned(),
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
    fn result_view_falls_back_when_input_has_no_pattern() {
        // Defensive: missing pattern falls through to text rather
        // than rendering an empty header.
        let view = GlobTool.result_view(
            &serde_json::json!({}),
            "src/main.rs",
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
                pattern: "*.rs".to_owned(),
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
                pattern: "*.rs".to_owned(),
                files: vec!["a.rs".to_owned(), "b.rs".to_owned()],
                total: 2,
            },
        );
    }

    #[test]
    fn result_view_metadata_total_equal_to_file_count_succeeds() {
        // Boundary of the `total < files.len()` guard. Pinning equality
        // here keeps the comparator from drifting to `<=` or `==` —
        // mutants that would otherwise pass every other test.
        let metadata = ToolMetadata {
            truncated_total: Some(2),
            ..ToolMetadata::default()
        };
        let view = GlobTool
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
        // Unix paths can technically contain `\n`; back-to-back newlines
        // would let the parser mistake the rest of the body for a
        // truncation footer. The `is_truncation_footer` gate rejects
        // anything that doesn't start with `(Showing ` so we fall
        // through to text instead of dropping rows.
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
        // Inconsistent metadata — claims fewer total matches than the
        // visible body. Render-time math depends on `total >= files.len()`,
        // so reject up front.
        let metadata = ToolMetadata {
            truncated_total: Some(1),
            ..ToolMetadata::default()
        };
        let view = GlobTool.result_view(
            &serde_json::json!({"pattern": "*.rs"}),
            "a.rs\nb.rs",
            &metadata,
        );
        assert!(view.is_none());
    }

    #[test]
    fn result_view_falls_back_for_unrecognised_trailing_prose() {
        // `\n\n` separator present but the trailing chunk doesn't match
        // the truncation-footer shape — fall through to text rather
        // than absorb the rest as paths.
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
    fn parse_total_from_footer_returns_none_for_malformed_input() {
        // Drop the leading `(` → missing strip.
        assert_eq!(
            parse_total_from_footer("Showing 100 of 1234 matches."),
            None,
        );
        // Token at idx 3 is non-numeric.
        assert_eq!(
            parse_total_from_footer("(Showing 100 of many matches.)"),
            None
        );
        // Fewer than four whitespace-separated tokens.
        assert_eq!(parse_total_from_footer("(Showing 100 of)"), None);
    }
}
