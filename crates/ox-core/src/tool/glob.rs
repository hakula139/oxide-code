//! File pattern matching tool (glob).

use std::fmt::Write as _;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::search::{NO_FILES_FOUND, SearchRoot, SearchRow, SearchRows};
use super::view::ToolResultView;
use super::{Prepared, Tool, ToolCtx, ToolMetadata, ToolOutput, ToolSpec, extract_input_field};
use crate::permission::{PermissionClass, ReadFilter};
use crate::util::text::plural;

const SHOWING_PREFIX: &str = "(Showing ";

const MAX_RESULTS: usize = 100;

/// Withholds each path a `read` deny or ask rule matches from the results.
pub(crate) struct GlobTool {
    read_filter: ReadFilter,
}

impl GlobTool {
    pub(crate) fn new(read_filter: ReadFilter) -> Self {
        Self { read_filter }
    }
}

impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".to_owned(),
            description: "Find files matching a glob pattern. \
                          Returns paths sorted by modification time (newest first)."
                .to_owned(),
            input_schema: input_schema(),
            class: PermissionClass::Read,
            parallel_safe: true,
        }
    }

    fn prepare(&self, input: serde_json::Value, ctx: &ToolCtx) -> Prepared {
        super::search::prepare_search_root(input, ctx)
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
        _cwd: &Path,
    ) -> Option<ToolResultView> {
        parse_files_view(input, content, metadata)
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(run(prepared, ctx, self.read_filter.clone()))
    }
}

// ── Input ──

fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "The glob pattern to match, relative to `path` \
                                (e.g. \"**/*.rs\", \"src/**/*.ts\"). \
                                \"*\" and \"?\" stay within one directory, \
                                and a pattern without \"/\" matches file names at any depth."
            },
            "path": {
                "type": "string",
                "description": "The directory to search in (default: current working directory)"
            }
        },
        "required": ["pattern"]
    })
}

#[derive(Deserialize)]
struct Input {
    pattern: String,
    path: Option<String>,
}

// ── Execution ──

async fn run(prepared: Prepared, ctx: &ToolCtx, read_filter: ReadFilter) -> ToolOutput {
    let Prepared { input: raw, access } = prepared;
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(msg) => return ToolOutput::error(msg),
    };

    let Input { pattern, path } = input;

    let workspace = Arc::clone(&ctx.workspace);
    let cancel = ctx.cancel.clone();
    match super::run_blocking(move || {
        let root = SearchRoot::new(&access, path.as_deref(), workspace)?;
        glob_files(&pattern, &root, &read_filter, &cancel)
    })
    .await
    {
        Ok(GlobOutput {
            content,
            title,
            truncated_total,
            view,
        }) => {
            let output = ToolOutput::success(content)
                .with_title(title)
                .with_result_view(view);
            match truncated_total {
                Some(total) => output.with_truncated_total(total),
                None => output,
            }
        }
        Err(msg) => ToolOutput::error(msg),
    }
}

#[derive(Debug)]
struct GlobOutput {
    content: String,
    /// A one-line summary of the call, which the UI shows on its result.
    title: String,
    truncated_total: Option<usize>,
    view: ToolResultView,
}

/// Lists up to [`MAX_RESULTS`] files under `root` that match `pattern`, newest first and relative
/// to the root's canonical cwd, until `cancel` fires. Trailing notes name the paths the walk could
/// not read and count the matches and unreadable paths `read_filter` withholds. A truncated list
/// says so inline and reports its total on `truncated_total`.
fn glob_files(
    pattern: &str,
    root: &SearchRoot,
    read_filter: &ReadFilter,
    cancel: &CancellationToken,
) -> Result<GlobOutput, String> {
    let SearchRoot {
        base,
        spelled,
        cwd,
        workspace,
    } = root;
    if !base.is_dir() {
        return Err(format!(
            "{} is not a directory",
            super::display_path(base, cwd)
        ));
    }

    let glob = compile_glob(pattern)?;

    let read_filter = read_filter.walk(base, spelled, workspace);
    let mut unreadable = Vec::new();
    let mut withheld = 0;
    let entries = super::search::walk_files(base, cancel, &mut unreadable)
        .filter(|entry| {
            let rel = entry.path().strip_prefix(base).unwrap_or(entry.path());
            glob.is_match(rel)
        })
        .filter(|entry| {
            let held = read_filter.withholds(entry.path());
            withheld += usize::from(held);
            !held
        });
    let (entries, total) = super::search::newest_first(entries, MAX_RESULTS);
    let mut rows = SearchRows::new(MAX_RESULTS);
    for entry in entries {
        rows.push(SearchRow::Path(super::display_path(entry.path(), cwd)));
    }
    withheld += super::search::withhold_unreadable(&mut unreadable, &read_filter);

    let truncated = total > MAX_RESULTS;

    let (mut content, title) = if rows.kept.is_empty() {
        (NO_FILES_FOUND.to_owned(), NO_FILES_FOUND.to_owned())
    } else {
        let count = rows.kept.len();
        let word = plural(count, "file", "files");
        (rows.render(), format!("Found {count} {word}"))
    };
    if truncated {
        _ = write!(
            content,
            "\n\n{SHOWING_PREFIX}{MAX_RESULTS} of {total} matches. Use a more specific pattern.)"
        );
    }
    super::search::push_unreadable_note(&mut content, &unreadable, cwd);
    super::search::push_withheld_note(&mut content, withheld);
    let view = if unreadable.is_empty() && withheld == 0 {
        ToolResultView::GlobFiles {
            pattern: pattern.to_owned(),
            files: rows.kept.iter().map(ToString::to_string).collect(),
            total,
        }
    } else {
        ToolResultView::Text {
            content: content.clone(),
        }
    };

    Ok(GlobOutput {
        content,
        title,
        truncated_total: truncated.then_some(total),
        view,
    })
}

/// Compiles `pattern` relative to the search root, so `*` and `?` never cross `/` and a leading
/// `./` anchors at the root. A pattern without `/` gains a `**/` prefix and matches file names at
/// any depth, the way gitignore-style globs behave. Absolute patterns are rejected because the
/// permission gate only sees `path`.
fn compile_glob(pattern: &str) -> Result<globset::GlobMatcher, String> {
    if Path::new(pattern).has_root() {
        return Err(format!(
            "Pattern {pattern} is absolute. \
             Set `path` to its directory and pass a pattern relative to it."
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

/// Reconstructs a legacy result without stored paths, retaining unrecognized output as text.
fn parse_files_view(
    input: &serde_json::Value,
    content: &str,
    metadata: &ToolMetadata,
) -> Option<ToolResultView> {
    let pattern = extract_input_field(input, "pattern")?.to_owned();
    let trimmed = content.trim_end();
    if trimmed == NO_FILES_FOUND {
        return Some(ToolResultView::GlobFiles {
            pattern,
            files: Vec::new(),
            total: 0,
        });
    }

    let body = glob_output_body(trimmed)?;

    let files: Vec<String> = body.lines().map(str::to_owned).collect();
    if files.is_empty() {
        return None;
    }
    let total = metadata.truncated_total.unwrap_or(files.len());
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
    footer.starts_with(SHOWING_PREFIX)
}

fn glob_output_body(content: &str) -> Option<&str> {
    let trimmed = content.trim_end();
    match trimmed.rsplit_once("\n\n") {
        Some((body, footer)) if is_truncation_footer(footer) => Some(body),
        Some(_) => None,
        None => Some(trimmed),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;

    use indoc::{formatdoc, indoc};

    use super::*;
    use crate::permission::Access;
    use crate::tool::ToolRegistry;
    use crate::tool::local_testing::{self, ctx, deny_reads, search_root, temp_ctx};
    use crate::util::fs::testing::deny_read_access;
    use crate::util::path::local_testing::canonical_tempdir;
    use crate::util::path::slash_separated;

    /// A `glob` tool without read rules.
    fn glob_tool() -> GlobTool {
        GlobTool::new(ReadFilter::default())
    }

    mod prepare {
        use super::*;

        #[test]
        fn defaults_the_search_root_to_cwd() {
            // A deny scoped to the project root must apply when `path` is omitted.
            let dir = canonical_tempdir();

            let prepared =
                glob_tool().prepare(serde_json::json!({"pattern": "*.rs"}), &ctx(dir.path()));

            let Access::Path {
                canonical,
                relative,
                ..
            } = prepared.access
            else {
                panic!("expected a path target");
            };
            assert_eq!(canonical, dir.path().to_str().unwrap());
            assert_eq!(relative.as_deref(), Some(""));
        }
    }

    mod result_view {
        use super::*;

        #[test]
        fn builds_glob_files() {
            let view = glob_tool()
                .result_view(
                    &serde_json::json!({"pattern": "*.rs"}),
                    indoc! {"
                        src/main.rs
                        src/lib.rs"
                    },
                    &ToolMetadata::default(),
                    Path::new("/repo"),
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
        fn pulls_total_from_metadata_and_drops_prose_footer() {
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
            let view = glob_tool()
                .result_view(
                    &serde_json::json!({"pattern": "**/*.rs"}),
                    &content,
                    &metadata,
                    Path::new("/repo"),
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
        fn handles_no_files_found() {
            let view = glob_tool()
                .result_view(
                    &serde_json::json!({"pattern": "*.nope"}),
                    "No files found",
                    &ToolMetadata::default(),
                    Path::new("/repo"),
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
        fn falls_back_for_empty_content() {
            let view = glob_tool().result_view(
                &serde_json::json!({"pattern": "*.rs"}),
                "",
                &ToolMetadata::default(),
                Path::new("/repo"),
            );
            assert!(view.is_none());
        }

        #[test]
        fn falls_back_when_input_has_no_pattern() {
            let view = glob_tool().result_view(
                &serde_json::json!({}),
                "src/main.rs",
                &ToolMetadata::default(),
                Path::new("/repo"),
            );
            assert!(view.is_none());
        }

        #[test]
        fn lists_a_single_file_with_a_total_of_one() {
            let view = glob_tool()
                .result_view(
                    &serde_json::json!({"pattern": "*.rs"}),
                    "src/only.rs",
                    &ToolMetadata::default(),
                    Path::new("/repo"),
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
        fn normalizes_trailing_newline() {
            let view = glob_tool()
                .result_view(
                    &serde_json::json!({"pattern": "*.rs"}),
                    "a.rs\nb.rs\n",
                    &ToolMetadata::default(),
                    Path::new("/repo"),
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
        fn metadata_total_equal_to_file_count_succeeds() {
            let metadata = ToolMetadata {
                truncated_total: Some(2),
                ..ToolMetadata::default()
            };
            let view = glob_tool()
                .result_view(
                    &serde_json::json!({"pattern": "*.rs"}),
                    "a.rs\nb.rs",
                    &metadata,
                    Path::new("/repo"),
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
        fn path_with_embedded_blank_line_falls_back() {
            let view = parse_files_view(
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
        fn falls_back_when_metadata_total_under_visible_files() {
            let metadata = ToolMetadata {
                truncated_total: Some(1),
                ..ToolMetadata::default()
            };
            let view = glob_tool().result_view(
                &serde_json::json!({"pattern": "*.rs"}),
                "a.rs\nb.rs",
                &metadata,
                Path::new("/repo"),
            );
            assert!(view.is_none());
        }

        #[test]
        fn falls_back_for_unrecognized_trailing_prose() {
            let view = glob_tool().result_view(
                &serde_json::json!({"pattern": "*.rs"}),
                indoc! {"
                    src/main.rs

                    (Some other footer we don't recognize)"
                },
                &ToolMetadata::default(),
                Path::new("/repo"),
            );
            assert!(view.is_none());
        }
    }

    mod run {
        use super::*;

        async fn assert_filename_round_trip(name: &str) {
            let dir = canonical_tempdir();
            fs::write(dir.path().join(name), "").unwrap();
            let input = serde_json::json!({"pattern": "*"});
            let registry = ToolRegistry::new(vec![Box::new(glob_tool())]);

            let output = registry
                .run_input("glob", input.clone(), &ctx(dir.path()))
                .await;
            assert!(!output.is_error, "{name:?}: {}", output.content);
            let metadata: ToolMetadata =
                serde_json::from_value(serde_json::to_value(&output.metadata).unwrap()).unwrap();

            assert_eq!(output.content, name);
            assert_eq!(output.metadata.title.as_deref(), Some("Found 1 file"));
            for cwd in [dir.path(), Path::new("/another/workspace")] {
                assert_eq!(
                    registry.result_view(
                        "glob",
                        &input,
                        &output.content,
                        &metadata,
                        output.is_error,
                        cwd
                    ),
                    ToolResultView::GlobFiles {
                        pattern: "*".to_owned(),
                        files: vec![name.to_owned()],
                        total: 1,
                    },
                    "{name:?} at {cwd:?}",
                );
            }
        }

        #[tokio::test]
        async fn persists_the_text_view_when_search_notes_are_present() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("public.rs"), "").unwrap();
            fs::write(dir.path().join("secret.rs"), "").unwrap();
            let registry =
                ToolRegistry::new(vec![Box::new(GlobTool::new(deny_reads(&["secret.rs"])))]);
            let input = serde_json::json!({"pattern": "*.rs"});

            let output = registry
                .run_input("glob", input.clone(), &ctx(dir.path()))
                .await;
            assert!(!output.is_error, "{}", output.content);

            assert_eq!(
                output.content,
                "public.rs\n\n(1 file withheld by read deny or ask rules)"
            );
            let expected = ToolResultView::Text {
                content: output.content.clone(),
            };
            assert_eq!(output.metadata.result_view, Some(expected.clone()));
            assert_eq!(
                registry.result_view(
                    "glob",
                    &input,
                    &output.content,
                    &output.metadata,
                    output.is_error,
                    dir.path()
                ),
                expected
            );
        }

        #[tokio::test]
        async fn preserves_a_filename_that_equals_the_empty_result_message() {
            assert_filename_round_trip("No files found").await;
        }

        #[tokio::test]
        async fn matching_pattern_reports_files() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.txt"), "").unwrap();
            fs::write(dir.path().join("b.rs"), "").unwrap();

            let input = serde_json::json!({
                "pattern": "*.txt",
                "path": dir.path().to_str().unwrap()
            });

            let output = local_testing::run(&glob_tool(), input, &temp_ctx()).await;

            let name = dir.path().file_name().unwrap().to_str().unwrap();
            assert!(!output.is_error);
            assert_eq!(output.content, format!("{name}/a.txt"));
            assert!(output.metadata.truncated_total.is_none());
        }

        #[tokio::test]
        async fn missing_pattern_reports_input_error() {
            let output = local_testing::run(&glob_tool(), serde_json::json!({}), &temp_ctx()).await;

            assert!(output.is_error);
            assert_eq!(output.content, "Invalid input: missing field `pattern`");
        }

        #[tokio::test]
        async fn invalid_pattern_omits_title() {
            let input = serde_json::json!({"pattern": "[unclosed"});

            let output = local_testing::run(&glob_tool(), input, &temp_ctx()).await;

            let cause = globset::Glob::new("**/[unclosed").unwrap_err();
            assert!(output.is_error);
            assert_eq!(output.content, format!("Invalid glob pattern: {cause}"));
            assert_eq!(output.metadata.title, None, "errors are not a file count");
        }

        #[tokio::test]
        async fn truncated_attaches_total_to_metadata() {
            let dir = canonical_tempdir();
            let total = MAX_RESULTS + 10;
            for i in 0..total {
                fs::write(dir.path().join(format!("{i:04}.txt")), "").unwrap();
            }

            let input = serde_json::json!({"pattern": "*.txt"});

            let output = local_testing::run(&glob_tool(), input, &ctx(dir.path())).await;

            assert!(!output.is_error);
            assert_eq!(output.metadata.truncated_total, Some(total));
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn preserves_newlines_in_native_filenames_after_metadata_round_trip() {
            for name in ["line\nname.rs", "blank\n\nname.rs", "trailing.rs\n"] {
                assert_filename_round_trip(name).await;
            }
        }
    }

    mod glob_files {
        use super::*;

        fn glob_in(
            pattern: &str,
            path: Option<&str>,
            cwd: &Path,
            read_filter: &ReadFilter,
        ) -> Result<GlobOutput, String> {
            let root = search_root(cwd, path)?;
            glob_files(pattern, &root, read_filter, &CancellationToken::new())
        }

        #[test]
        fn plain_pattern_finds_matching_paths() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("foo.txt"), "").unwrap();
            fs::write(dir.path().join("bar.txt"), "").unwrap();
            fs::write(dir.path().join("baz.rs"), "").unwrap();

            let result = glob_in("*.txt", None, dir.path(), &ReadFilter::default()).unwrap();
            let mut files: Vec<&str> = result.content.lines().collect();
            files.sort_unstable();
            assert_eq!(files, ["bar.txt", "foo.txt"]);
            assert!(result.truncated_total.is_none());
        }

        #[test]
        fn recursive_pattern_finds_nested_paths() {
            let dir = canonical_tempdir();
            let sub = dir.path().join("sub");
            fs::create_dir(&sub).unwrap();
            fs::write(dir.path().join("top.rs"), "").unwrap();
            fs::write(sub.join("nested.rs"), "").unwrap();

            let result = glob_in("**/*.rs", None, dir.path(), &ReadFilter::default()).unwrap();
            assert!(result.content.lines().any(|l| l == "top.rs"));
            assert!(result.content.lines().any(|l| l == "sub/nested.rs"));
        }

        #[test]
        fn star_stays_within_one_directory() {
            let dir = canonical_tempdir();
            let nested = dir.path().join("src/nested");
            fs::create_dir_all(&nested).unwrap();
            fs::write(dir.path().join("src/a.rs"), "").unwrap();
            fs::write(nested.join("b.rs"), "").unwrap();

            let scoped = glob_in("src/*.rs", None, dir.path(), &ReadFilter::default()).unwrap();
            assert_eq!(scoped.content, "src/a.rs");

            // A pattern without `/` matches the file name at any depth.
            let bare = glob_in("b.rs", None, dir.path(), &ReadFilter::default()).unwrap();
            assert_eq!(bare.content, "src/nested/b.rs");
        }

        #[test]
        fn dot_slash_pattern_anchors_at_the_search_root() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join("src")).unwrap();
            fs::write(dir.path().join("a.rs"), "").unwrap();
            fs::write(dir.path().join("src/b.rs"), "").unwrap();

            assert_eq!(
                glob_in("./*.rs", None, dir.path(), &ReadFilter::default())
                    .unwrap()
                    .content,
                "a.rs"
            );
            assert_eq!(
                glob_in("././src/*.rs", None, dir.path(), &ReadFilter::default())
                    .unwrap()
                    .content,
                "src/b.rs"
            );
        }

        #[test]
        fn renders_paths_relative_to_cwd() {
            let dir = canonical_tempdir();
            let cwd = dir.path().join("project");
            fs::create_dir_all(cwd.join("src")).unwrap();
            fs::write(cwd.join("src/lib.rs"), "").unwrap();
            let outside = dir.path().join("outside");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("x.rs"), "").unwrap();

            let inside = glob_in("*.rs", Some("src"), &cwd, &ReadFilter::default()).unwrap();
            assert_eq!(inside.content, "src/lib.rs");

            let outside_hit =
                glob_in("*.rs", Some("../outside"), &cwd, &ReadFilter::default()).unwrap();
            assert_eq!(
                outside_hit.content,
                slash_separated(&outside.join("x.rs")),
                "hits outside cwd fall back to an absolute path",
            );
        }

        #[test]
        fn modification_time_orders_newest_first() {
            let dir = canonical_tempdir();
            local_testing::create_modified_at(&dir.path().join("a.txt"), 1_000_000);
            local_testing::create_modified_at(&dir.path().join("b.txt"), 1_000_060);

            let result = glob_in("*.txt", None, dir.path(), &ReadFilter::default()).unwrap();

            assert_eq!(result.content, "b.txt\na.txt");
        }

        #[test]
        fn unmatched_pattern_reports_no_files() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.txt"), "").unwrap();

            let result = glob_in("*.rs", None, dir.path(), &ReadFilter::default()).unwrap();
            assert_eq!(result.content, "No files found");
            assert_eq!(result.title, "No files found");
            assert!(result.truncated_total.is_none());
        }

        #[test]
        fn invalid_pattern_reports_error() {
            let dir = canonical_tempdir();
            let err = glob_in("[invalid", None, dir.path(), &ReadFilter::default()).unwrap_err();

            let cause = globset::Glob::new("**/[invalid").unwrap_err();
            assert_eq!(err, format!("Invalid glob pattern: {cause}"));
        }

        #[test]
        fn absolute_pattern_points_at_path() {
            let dir = canonical_tempdir();
            let pattern = format!("{}/*.rs", dir.path().display());
            let err = glob_in(&pattern, None, dir.path(), &ReadFilter::default()).unwrap_err();
            assert_eq!(
                err,
                format!(
                    "Pattern {pattern} is absolute. \
                     Set `path` to its directory and pass a pattern relative to it."
                )
            );
        }

        #[test]
        fn invalid_directory_reports_error() {
            let dir = canonical_tempdir();
            let err =
                glob_in("*.txt", Some("missing"), dir.path(), &ReadFilter::default()).unwrap_err();
            let cause = fs::metadata(dir.path().join("missing")).unwrap_err();
            assert_eq!(err, format!("Failed to access missing: {cause}"));

            fs::write(dir.path().join("a.txt"), "").unwrap();
            let err =
                glob_in("*.txt", Some("a.txt"), dir.path(), &ReadFilter::default()).unwrap_err();
            assert_eq!(err, "a.txt is not a directory");
        }

        #[test]
        fn lists_at_most_max_results_and_names_the_total() {
            let dir = canonical_tempdir();
            for i in 0..MAX_RESULTS + 10 {
                fs::write(dir.path().join(format!("{i:04}.txt")), "").unwrap();
            }

            let result = glob_in("*.txt", None, dir.path(), &ReadFilter::default()).unwrap();

            let (files, footer) = result.content.split_once("\n\n").unwrap();
            let files: HashSet<_> = files.lines().map(str::to_owned).collect();
            let all: HashSet<_> = (0..MAX_RESULTS + 10)
                .map(|i| format!("{i:04}.txt"))
                .collect();
            assert_eq!(files.len(), MAX_RESULTS);
            assert!(files.is_subset(&all), "{files:?}");
            assert_eq!(
                footer,
                format!(
                    "(Showing {MAX_RESULTS} of {} matches. Use a more specific pattern.)",
                    MAX_RESULTS + 10,
                ),
            );
            assert_eq!(result.truncated_total, Some(MAX_RESULTS + 10));
            assert_eq!(result.title, format!("Found {MAX_RESULTS} files"));
        }

        #[test]
        fn withholds_paths_a_read_deny_rule_matches() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("config.toml"), "").unwrap();
            fs::create_dir(dir.path().join("secrets")).unwrap();
            fs::write(dir.path().join("secrets/key.toml"), "").unwrap();
            let read_filter = deny_reads(&["secrets/**"]);

            let result = glob_in("*.toml", None, dir.path(), &read_filter).unwrap();
            assert_eq!(
                result.content,
                indoc! {"
                    config.toml

                    (1 file withheld by read deny or ask rules)"
                }
            );
            assert_eq!(result.truncated_total, None);
            assert_eq!(result.title, "Found 1 file");
            // The structured view would drop the note, so the text view shows it.
            let view = glob_tool().result_view(
                &serde_json::json!({"pattern": "*.toml"}),
                &result.content,
                &ToolMetadata::default(),
                Path::new("/repo"),
            );
            assert_eq!(view, None);

            let result = glob_in("secrets/*", None, dir.path(), &read_filter).unwrap();
            assert_eq!(
                result.content,
                indoc! {"
                    No files found

                    (1 file withheld by read deny or ask rules)"
                }
            );
        }

        #[test]
        fn lists_a_directory_it_could_not_read_after_the_matches() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.rs"), "").unwrap();
            let locked = dir.path().join("locked");
            fs::create_dir(&locked).unwrap();
            fs::write(locked.join("b.rs"), "").unwrap();
            let _denied = deny_read_access(&locked);

            let result = glob_in("*.rs", None, dir.path(), &ReadFilter::default()).unwrap();

            let cause = fs::read_dir(&locked).unwrap_err();
            assert_eq!(
                result.content,
                formatdoc! {"
                    a.rs

                    Could not read:
                      locked: {cause}"
                }
            );
            assert_eq!(result.title, "Found 1 file");
            let view = glob_tool().result_view(
                &serde_json::json!({"pattern": "*.rs"}),
                &result.content,
                &ToolMetadata::default(),
                Path::new("/repo"),
            );
            assert_eq!(view, None);
        }

        #[test]
        fn withholds_an_unreadable_directory_a_read_deny_rule_matches() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join("a.rs"), "").unwrap();
            let secrets = dir.path().join("secrets");
            fs::create_dir(&secrets).unwrap();
            let _denied = deny_read_access(&secrets);

            let result = glob_in("*.rs", None, dir.path(), &deny_reads(&["secrets"])).unwrap();

            assert_eq!(
                result.content,
                indoc! {"
                    a.rs

                    (1 file withheld by read deny or ask rules)"
                }
            );
        }

        #[test]
        fn includes_hidden_paths_but_skips_vcs_dirs() {
            let dir = canonical_tempdir();
            let workflows = dir.path().join(".github/workflows");
            fs::create_dir_all(&workflows).unwrap();
            fs::write(workflows.join("ci.yml"), "").unwrap();
            let git = dir.path().join(".git");
            fs::create_dir(&git).unwrap();
            fs::write(git.join("hook.yml"), "").unwrap();

            let result = glob_in(
                ".github/workflows/*.yml",
                None,
                dir.path(),
                &ReadFilter::default(),
            )
            .unwrap();
            assert_eq!(result.content, ".github/workflows/ci.yml");

            let all = glob_in("*.yml", None, dir.path(), &ReadFilter::default()).unwrap();
            assert_eq!(all.content, ".github/workflows/ci.yml");
        }

        #[test]
        fn respects_gitignore() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join(".git")).unwrap();
            fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
            fs::write(dir.path().join("ignored.txt"), "").unwrap();
            fs::write(dir.path().join("tracked.txt"), "").unwrap();

            let result = glob_in("*.txt", None, dir.path(), &ReadFilter::default()).unwrap();
            assert_eq!(result.content, "tracked.txt");
        }

        #[cfg(unix)]
        #[test]
        fn withholds_a_denied_file_below_a_symlinked_root() {
            let dir = canonical_tempdir();
            let (cwd, outside) = (dir.path().join("cwd"), dir.path().join("outside"));
            fs::create_dir(&cwd).unwrap();
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join(".env"), "").unwrap();
            fs::write(outside.join("app.toml"), "").unwrap();
            std::os::unix::fs::symlink(&outside, cwd.join("config")).unwrap();
            let read_filter = deny_reads(&[".env"]);

            let result = glob_in("*", Some("config"), &cwd, &read_filter).unwrap();
            let app = outside.join("app.toml").display().to_string();
            assert_eq!(
                result.content,
                formatdoc! {"
                    {app}

                    (1 file withheld by read deny or ask rules)"
                }
            );
        }
    }

    mod is_truncation_footer {
        use super::*;

        #[test]
        fn accepts_glob_files_shape() {
            assert!(is_truncation_footer(
                "(Showing 100 of 250 matches. Use a more specific pattern.)"
            ));
        }

        #[test]
        fn rejects_unknown_prose() {
            assert!(!is_truncation_footer("(Some footer)"));
            assert!(!is_truncation_footer("Showing 100 of 250"));
            assert!(!is_truncation_footer(""));
        }
    }
}
