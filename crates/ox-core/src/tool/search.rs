//! Shared search roots, ordered file walking, bounded result rows, and visibility notes.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;

use super::spill::MAX_SPILL_BYTES;
use super::view::{GrepFileGroup, GrepMatchLine};
use super::{StoredView, ToolOutput, display_path, tool_path};
use crate::permission::{WalkFilter, Workspace};
use crate::util::text::{ELLIPSIS, format_size, plural};

// ── Search Root ──

/// The root a search call's `path` names, the cwd by default. The gate resolves it, and the walk
/// matches each hit's rules against its spelling.
pub(super) fn search_arg(path: Option<&str>) -> &str {
    path.unwrap_or(".")
}

/// Where a search tool's walk starts, which its prepared call resolved.
pub(super) struct SearchRoot {
    /// The resolved root, which exists.
    pub(super) base: PathBuf,
    /// The root as the call spells it with `~` expanded, for the read deny and ask rules each found
    /// file faces.
    pub(super) spelled: PathBuf,
    /// The resolved working directory, so hits under the resolved base render relative to it.
    pub(super) cwd: PathBuf,
    /// The session's workspace, whose anchor directories those rules apply below.
    pub(super) workspace: Arc<Workspace>,
}

impl SearchRoot {
    /// The resolved `base` of a search in `workspace` whose call spells its root as `path`, the cwd
    /// by default. The root must exist.
    pub(super) fn new(
        base: PathBuf,
        path: Option<&str>,
        workspace: Arc<Workspace>,
    ) -> Result<Self, String> {
        let spelled = tool_path(search_arg(path));
        if let Err(e) = std::fs::metadata(&base) {
            let cwd = workspace.cwd();
            return Err(format!(
                "Failed to access {}: {e}",
                display_path(&cwd.join(&spelled), cwd)
            ));
        }
        let cwd = workspace.resolved_cwd().to_path_buf();
        Ok(Self {
            base,
            spelled,
            cwd,
            workspace,
        })
    }
}

// ── File Walking ──

const MAX_WALK_DEPTH: usize = 64;

const VCS_DIRS: &[&str] = &[".bzr", ".git", ".hg", ".jj", ".sl", ".svn"];

/// Recursively walks `base`, yielding regular files only, until `cancel` fires. Each path the walk
/// cannot read goes to `unreadable`.
///
/// Honors `.gitignore` and global ignore files, includes hidden entries such as `.github/` so
/// dotfile config stays searchable, skips VCS metadata directories below `base`, and stays on the
/// starting filesystem, so a bind mount or container volume below `base` is searched only when the
/// caller roots the search there.
pub(super) fn walk_files<'a>(
    base: &Path,
    cancel: &'a CancellationToken,
    unreadable: &'a mut Vec<Unreadable>,
) -> impl Iterator<Item = ignore::DirEntry> + 'a {
    ignore::WalkBuilder::new(base)
        .hidden(false)
        .filter_entry(|e| e.depth() == 0 || !VCS_DIRS.iter().any(|d| e.file_name() == *d))
        .same_file_system(true)
        .max_depth(Some(MAX_WALK_DEPTH))
        .build()
        .take_while(|_| !cancel.is_cancelled())
        .filter_map(move |entry| match entry {
            Ok(entry) => Some(entry),
            Err(e) => {
                unreadable.extend(Unreadable::from_walk(&e));
                None
            }
        })
        .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
}

/// A path a search could not read, with the error that stopped it.
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(super) struct Unreadable {
    /// The path, unless the walk failed partway through a directory listing without naming it.
    pub(super) path: Option<PathBuf>,
    pub(super) error: String,
}

impl Unreadable {
    /// What walk error `e` failed to read, or `None` if `e` hides nothing from the walk. An ignore
    /// file that failed to load only loses its rules, and a path that vanished mid-walk holds
    /// nothing left to search.
    fn from_walk(e: &ignore::Error) -> Option<Self> {
        e.depth()?;
        let (path, cause) = match e {
            ignore::Error::WithPath { path, err } => (Some(path.clone()), err.as_ref()),
            _ => (None, e),
        };
        let error = match cause.io_error() {
            Some(io) if io.kind() == std::io::ErrorKind::NotFound => return None,
            Some(io) => innermost_error(io),
            None => cause.to_string(),
        };
        Some(Self { path, error })
    }

    /// The file at `path`, whose read failed with `e`.
    pub(super) fn from_read(path: &Path, e: &std::io::Error) -> Self {
        Self {
            path: Some(path.to_owned()),
            error: innermost_error(e),
        }
    }
}

/// The text of the error at the bottom of `e`'s source chain, without the context wrapping it.
fn innermost_error(e: &std::io::Error) -> String {
    let mut error: &dyn std::error::Error = e;
    while let Some(source) = error.source() {
        error = source;
    }
    error.to_string()
}

/// Drops each path in `unreadable` that `read_filter` withholds, so a note never names one, and
/// returns how many it dropped.
pub(super) fn withhold_unreadable(
    unreadable: &mut Vec<Unreadable>,
    read_filter: &WalkFilter,
) -> usize {
    let found = unreadable.len();
    unreadable.retain(|u| {
        u.path
            .as_deref()
            .is_none_or(|path| !read_filter.withholds(path))
    });
    found - unreadable.len()
}

/// The newest `limit` entries and the full count, in walk order among equal modification times.
pub(super) fn newest_first(
    entries: impl Iterator<Item = ignore::DirEntry>,
    limit: usize,
) -> (Vec<ignore::DirEntry>, usize) {
    let mut newest = BTreeMap::new();
    let mut total = 0;
    for entry in entries {
        let key = (Reverse(entry_mtime(&entry)), total);
        total += 1;
        newest.insert(key, entry);
        if newest.len() > limit {
            newest.pop_last();
        }
    }
    (newest.into_values().collect(), total)
}

/// The entry's modification time, or [`SystemTime::UNIX_EPOCH`] when stat fails (entry vanished,
/// permission denied, platform without mtime), which sorts unreadable entries oldest.
fn entry_mtime(entry: &ignore::DirEntry) -> SystemTime {
    entry
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

// ── Search Rows ──

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SearchRow {
    Path(String),
    Count { path: String, count: usize },
    Match { path: String, line: GrepMatchLine },
    Separator,
}

impl fmt::Display for SearchRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(path) => f.write_str(path),
            Self::Count { path, count } => write!(f, "{path}:{count}"),
            Self::Match { path, line } => {
                let separator = if line.is_match { ':' } else { '-' };
                write!(f, "{path}:{}{separator}{}", line.number, line.text)
            }
            Self::Separator => f.write_str("--"),
        }
    }
}

/// Retains an output prefix under a row cap and the spill budget. The first omitted row records
/// truncation, while callers that need full totals can continue counting without retaining rows.
pub(super) struct SearchRows {
    pub(super) kept: Vec<SearchRow>,
    bytes: u64,
    head_limit: usize,
    row_limit_reached: bool,
}

impl SearchRows {
    pub(super) fn new(head_limit: usize) -> Self {
        Self {
            kept: Vec::new(),
            bytes: 0,
            head_limit,
            row_limit_reached: false,
        }
    }

    pub(super) fn full(&self) -> bool {
        self.row_limit_reached || self.bytes >= MAX_SPILL_BYTES
    }

    pub(super) fn push(&mut self, row: SearchRow) {
        if self.kept.len() == self.head_limit {
            self.row_limit_reached = true;
        } else if !self.full() {
            self.bytes += row.to_string().len() as u64 + 1;
            self.kept.push(row);
        }
    }

    pub(super) fn limit_description(&self, noun: &str) -> Option<String> {
        if self.row_limit_reached {
            Some(format!("{} {noun}", self.head_limit))
        } else if self.bytes >= MAX_SPILL_BYTES {
            Some(format!("{} of output", format_size(MAX_SPILL_BYTES)))
        } else {
            None
        }
    }

    pub(super) fn render(&self) -> String {
        self.kept
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn into_groups(self) -> Vec<GrepFileGroup> {
        let mut groups: Vec<GrepFileGroup> = Vec::new();
        for row in self.kept {
            if let SearchRow::Match { path, line } = row {
                match groups.last_mut() {
                    Some(group) if group.path == path => group.lines.push(line),
                    _ => groups.push(GrepFileGroup {
                        path,
                        lines: vec![line],
                    }),
                }
            }
        }
        groups
    }
}

// ── Search Output ──

/// A finished search's result.
#[derive(Debug)]
pub(super) struct SearchOutput {
    pub(super) content: String,
    /// A one-line summary of the call, which the UI shows on its result.
    pub(super) title: String,
    /// The rows to store, or [`StoredView::Text`] when notes follow them so the result renders as
    /// text. `None` stores no view.
    pub(super) view: Option<StoredView>,
}

impl SearchOutput {
    /// A result that stores no view.
    pub(super) fn plain(content: String, title: String) -> Self {
        Self {
            content,
            title,
            view: None,
        }
    }
}

impl From<SearchOutput> for ToolOutput {
    fn from(output: SearchOutput) -> Self {
        let SearchOutput {
            content,
            title,
            view,
        } = output;
        let output = Self::success(content).with_title(title);
        match view {
            Some(view) => output.with_stored_view(view),
            None => output,
        }
    }
}

// ── Search Notes ──

pub(super) const NO_FILES_FOUND: &str = "No files found";

/// Most paths a search note names before it counts the rest.
const MAX_NOTE_PATHS: usize = 10;

/// Appends a search note listing `lines` under `header`. It names at most [`MAX_NOTE_PATHS`] and
/// counts the rest, so a tree full of failures cannot flood the result.
pub(super) fn push_path_note(content: &mut String, header: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    _ = write!(content, "\n\n{header}");
    for line in lines.iter().take(MAX_NOTE_PATHS) {
        _ = write!(content, "\n  {line}");
    }
    if lines.len() > MAX_NOTE_PATHS {
        let more = lines.len() - MAX_NOTE_PATHS;
        _ = write!(content, "\n  {ELLIPSIS} and {more} more");
    }
}

/// Appends the note a search tool adds for the paths in `unreadable`, so a search that missed them
/// does not pass for a complete one. Paths render relative to `cwd`.
pub(super) fn push_unreadable_note(content: &mut String, unreadable: &[Unreadable], cwd: &Path) {
    let lines: Vec<String> = unreadable
        .iter()
        .map(|Unreadable { path, error }| match path {
            Some(path) => format!("{}: {error}", display_path(path, cwd)),
            None => error.clone(),
        })
        .collect();
    push_path_note(content, "Could not read:", &lines);
}

/// Appends the note a search tool adds when read deny or ask rules withheld `count` of the files it
/// found. The note counts them without naming any.
pub(super) fn push_withheld_note(content: &mut String, count: usize) {
    if count == 0 {
        return;
    }
    let word = plural(count, "file", "files");
    _ = write!(
        content,
        "\n\n({count} {word} withheld by read deny or ask rules)"
    );
}

#[cfg(test)]
mod tests {
    use std::fs;

    use indoc::{formatdoc, indoc};

    use super::*;
    use crate::tool::local_testing::{self, create_modified_at, deny_reads};
    use crate::util::fs::testing::deny_read_access;
    use crate::util::path::local_testing::canonical_tempdir;
    use crate::util::path::testing::HOME_VAR;

    mod search_root {
        use super::*;

        mod new {
            use super::*;

            #[test]
            fn resolves_relative_and_absolute_paths_and_keeps_their_spelling() {
                let dir = canonical_tempdir();
                fs::create_dir(dir.path().join("src")).unwrap();
                fs::create_dir(dir.path().join("lib")).unwrap();
                let lib = dir.path().join("lib");

                let relative =
                    local_testing::search_root(dir.path(), Some("./src/../lib")).unwrap();
                let absolute =
                    local_testing::search_root(Path::new("/"), Some(lib.to_str().unwrap()))
                        .unwrap();

                assert_eq!(
                    (relative.base, relative.spelled, relative.cwd),
                    (
                        lib.clone(),
                        PathBuf::from("./src/../lib"),
                        dir.path().to_owned()
                    ),
                );
                assert_eq!(absolute.base, lib);
            }

            #[test]
            fn reads_a_leading_tilde_as_the_home_directory() {
                let dir = canonical_tempdir();
                let home = dir.path().join("home");
                fs::create_dir_all(home.join("notes")).unwrap();

                let root = temp_env::with_var(HOME_VAR, Some(&home), || {
                    local_testing::search_root(dir.path(), Some("~/notes")).unwrap()
                });

                assert_eq!(
                    (root.base, root.spelled),
                    (home.join("notes"), home.join("notes"))
                );
            }

            #[test]
            fn defaults_to_the_cwd_without_a_path() {
                let dir = canonical_tempdir();

                let root = local_testing::search_root(dir.path(), None).unwrap();

                assert_eq!(
                    (root.base, root.spelled),
                    (dir.path().to_owned(), PathBuf::from(".")),
                );
            }

            #[test]
            fn missing_path_is_named_relative_to_cwd() {
                let dir = canonical_tempdir();

                let err = local_testing::search_root(dir.path(), Some("missing"))
                    .err()
                    .unwrap();

                let cause = fs::metadata(dir.path().join("missing")).unwrap_err();
                assert_eq!(err, format!("Failed to access missing: {cause}"));
            }

            #[cfg(unix)]
            #[test]
            fn resolves_dot_dot_after_a_symlink_physically() {
                let dir = canonical_tempdir();
                fs::create_dir_all(dir.path().join("real/target")).unwrap();
                let cwd = dir.path().join("cwd");
                fs::create_dir(&cwd).unwrap();
                std::os::unix::fs::symlink(dir.path().join("real/target"), cwd.join("link"))
                    .unwrap();

                let root = local_testing::search_root(&cwd, Some("link/..")).unwrap();

                assert_eq!(root.base, dir.path().join("real"));
            }

            #[cfg(target_os = "linux")]
            #[test]
            fn walks_from_a_root_whose_name_is_not_utf8() {
                use std::os::unix::ffi::OsStrExt as _;
                let dir = canonical_tempdir();
                let cwd = dir.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
                fs::create_dir(&cwd).unwrap();

                let root = local_testing::search_root(&cwd, None).unwrap();

                assert_eq!(root.base, cwd);
            }
        }
    }

    mod walk_files {
        use super::*;

        #[test]
        fn stops_once_cancelled() {
            let dir = canonical_tempdir();
            for name in ["a.txt", "b.txt", "c.txt"] {
                fs::write(dir.path().join(name), "").unwrap();
            }
            let cancel = CancellationToken::new();
            let mut unreadable = Vec::new();
            let mut walk = walk_files(dir.path(), &cancel, &mut unreadable);

            let first = walk.next();
            cancel.cancel();

            assert!(first.is_some(), "an uncancelled walk yields files");
            assert_eq!(walk.count(), 0, "the walk stops at the cancel");
        }

        #[test]
        fn records_a_directory_it_cannot_read_and_walks_on() {
            let dir = canonical_tempdir();
            let readable = dir.path().join("a.txt");
            fs::write(&readable, "").unwrap();
            let locked = dir.path().join("locked");
            fs::create_dir(&locked).unwrap();
            fs::write(locked.join("b.txt"), "").unwrap();
            let _denied = deny_read_access(&locked);
            let mut unreadable = Vec::new();

            let files: Vec<_> = walk_files(dir.path(), &CancellationToken::new(), &mut unreadable)
                .map(ignore::DirEntry::into_path)
                .collect();

            let cause = fs::read_dir(&locked).unwrap_err().to_string();
            assert_eq!(files, [readable]);
            assert_eq!(
                unreadable,
                [Unreadable {
                    path: Some(locked),
                    error: cause,
                }]
            );
        }
    }

    mod unreadable {
        use super::*;

        mod from_walk {
            use super::*;

            #[test]
            fn keeps_only_errors_that_hide_files() {
                let denied = || ignore::Error::Io(std::io::Error::other("denied"));
                let at_depth = |err| ignore::Error::WithDepth {
                    depth: 1,
                    err: Box::new(err),
                };
                let cases = [
                    (
                        ignore::Error::WithPath {
                            path: "locked".into(),
                            err: Box::new(at_depth(denied())),
                        },
                        Some((Some("locked"), "denied")),
                    ),
                    (at_depth(denied()), Some((None, "denied"))),
                    (
                        ignore::Error::WithPath {
                            path: "gone".into(),
                            err: Box::new(at_depth(ignore::Error::Io(
                                std::io::ErrorKind::NotFound.into(),
                            ))),
                        },
                        None,
                    ),
                    (
                        ignore::Error::WithPath {
                            path: ".gitignore".into(),
                            err: Box::new(denied()),
                        },
                        None,
                    ),
                    (ignore::Error::Partial(vec![denied(), denied()]), None),
                ];

                for (e, expected) in cases {
                    let expected = expected.map(|(path, error): (Option<&str>, &str)| Unreadable {
                        path: path.map(PathBuf::from),
                        error: error.to_owned(),
                    });
                    assert_eq!(Unreadable::from_walk(&e), expected, "{e}");
                }
            }
        }
    }

    mod withhold_unreadable {
        use super::*;

        #[test]
        fn drops_and_counts_only_the_paths_the_filter_withholds() {
            let dir = canonical_tempdir();
            let root = local_testing::search_root(dir.path(), None).unwrap();
            let read_filter = deny_reads(&["secrets"]);
            let read_filter = read_filter.walk(&root.base, &root.spelled, &root.workspace);
            let at = |path: Option<&str>| Unreadable {
                path: path.map(|path| dir.path().join(path)),
                error: "denied".to_owned(),
            };
            let mut unreadable = vec![at(Some("secrets")), at(Some("src")), at(None)];

            let withheld = withhold_unreadable(&mut unreadable, &read_filter);

            assert_eq!(withheld, 1);
            assert_eq!(unreadable, [at(Some("src")), at(None)]);
        }
    }

    mod newest_first {
        use super::*;

        #[test]
        fn keeps_later_newest_entries_while_counting_the_whole_walk() {
            let dir = canonical_tempdir();
            for index in 0..300 {
                create_modified_at(
                    &dir.path().join(format!("{index:03}.rs")),
                    1_000_000 + index,
                );
            }
            let mut unreadable = Vec::new();
            let mut entries: Vec<_> =
                walk_files(dir.path(), &CancellationToken::new(), &mut unreadable).collect();
            entries.sort_by_key(|entry| entry.file_name().to_owned());
            let visited = std::cell::Cell::new(0);
            let entries = entries
                .into_iter()
                .inspect(|_| visited.set(visited.get() + 1));

            let (kept, total) = newest_first(entries, 3);

            let names: Vec<_> = kept
                .iter()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["299.rs", "298.rs", "297.rs"]);
            assert_eq!(total, 300);
            assert_eq!(visited.get(), 300);
            assert_eq!(unreadable, []);
        }

        #[test]
        fn orders_by_mtime_and_keeps_input_order_among_ties() {
            let dir = canonical_tempdir();
            for (name, secs) in [
                ("a.txt", 1_000_000),
                ("b.txt", 1_000_060),
                ("c.txt", 1_000_000),
            ] {
                create_modified_at(&dir.path().join(name), secs);
            }
            let mut unreadable = Vec::new();
            let mut entries: Vec<_> =
                walk_files(dir.path(), &CancellationToken::new(), &mut unreadable).collect();
            entries.sort_by_key(|entry| Reverse(entry.file_name().to_owned()));

            for (limit, expected) in [
                (usize::MAX, vec!["b.txt", "c.txt", "a.txt"]),
                (2, vec!["b.txt", "c.txt"]),
                (1, vec!["b.txt"]),
                (0, vec![]),
            ] {
                let (sorted, total) = newest_first(entries.clone().into_iter(), limit);

                let names: Vec<_> = sorted
                    .iter()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect();
                assert_eq!(names, expected, "limit {limit}");
                assert_eq!(total, 3, "limit {limit}");
            }
        }
    }

    mod search_rows {
        use super::*;

        mod push {
            use super::*;

            #[test]
            fn retains_only_the_requested_prefix_when_later_matches_are_counted() {
                let mut rows = SearchRows::new(2);

                for index in 0..10_000 {
                    rows.push(SearchRow::Count {
                        path: format!("file-{index}"),
                        count: index + 1,
                    });
                }

                assert_eq!(
                    rows.kept,
                    [
                        SearchRow::Count {
                            path: "file-0".to_owned(),
                            count: 1
                        },
                        SearchRow::Count {
                            path: "file-1".to_owned(),
                            count: 2
                        },
                    ]
                );
                assert_eq!(rows.render(), "file-0:1\nfile-1:2");
                assert_eq!(rows.limit_description("files").as_deref(), Some("2 files"));
                assert!(rows.full());
            }

            #[test]
            fn stops_at_the_spill_limit_without_a_row_limit() {
                let row = "x".repeat(1 << 20);
                let mut rows = SearchRows::new(usize::MAX);
                let mut pushed = 0;

                while !rows.full() {
                    rows.push(SearchRow::Path(row.clone()));
                    pushed += 1;
                }
                rows.push(SearchRow::Path("omitted".to_owned()));

                assert_eq!(pushed, 10);
                assert_eq!(rows.render(), vec![row; 10].join("\n"));
                assert_eq!(
                    rows.limit_description("files").as_deref(),
                    Some("10 MiB of output")
                );
            }
        }

        mod into_groups {
            use super::*;

            #[test]
            fn keeps_native_paths_and_context_across_separators() {
                let first = GrepMatchLine {
                    number: 5,
                    text: "hit".to_owned(),
                    is_match: true,
                };
                let context = GrepMatchLine {
                    number: 10,
                    text: "context".to_owned(),
                    is_match: false,
                };
                let mut rows = SearchRows::new(10);
                rows.push(SearchRow::Match {
                    path: "name:12:part.rs".to_owned(),
                    line: first.clone(),
                });
                rows.push(SearchRow::Separator);
                rows.push(SearchRow::Match {
                    path: "name:12:part.rs".to_owned(),
                    line: context.clone(),
                });
                rows.push(SearchRow::Match {
                    path: "line\nname.rs".to_owned(),
                    line: first.clone(),
                });

                let groups = rows.into_groups();

                assert_eq!(
                    groups,
                    [
                        GrepFileGroup {
                            path: "name:12:part.rs".to_owned(),
                            lines: vec![first.clone(), context]
                        },
                        GrepFileGroup {
                            path: "line\nname.rs".to_owned(),
                            lines: vec![first]
                        },
                    ]
                );
            }
        }
    }

    mod push_path_note {
        use super::*;

        #[test]
        fn names_up_to_the_cap_and_counts_the_rest() {
            let lines: Vec<String> = (1..=13).map(|i| format!("{i}.rs")).collect();
            let listed = indoc! {"
                a.rs

                Header:
                  1.rs
                  2.rs
                  3.rs
                  4.rs
                  5.rs
                  6.rs
                  7.rs
                  8.rs
                  9.rs
                  10.rs"
            };
            for (count, expected) in [
                (10, listed.to_owned()),
                (13, format!("{listed}\n  ... and 3 more")),
            ] {
                let mut content = "a.rs".to_owned();

                push_path_note(&mut content, "Header:", &lines[..count]);

                assert_eq!(content, expected);
            }

            let mut content = "a.rs".to_owned();
            push_path_note(&mut content, "Header:", &[]);
            assert_eq!(content, "a.rs");
        }
    }

    mod push_unreadable_note {
        use super::*;

        #[test]
        fn names_each_path_relative_to_the_cwd() {
            let cwd = Path::new("/repo");
            let unreadable = [
                Unreadable {
                    path: Some(cwd.join("locked")),
                    error: "Permission denied (os error 13)".to_owned(),
                },
                Unreadable {
                    path: None,
                    error: "Input/output error (os error 5)".to_owned(),
                },
            ];
            let mut content = "a.rs".to_owned();

            push_unreadable_note(&mut content, &unreadable, cwd);

            assert_eq!(
                content,
                indoc! {"
                    a.rs

                    Could not read:
                      locked: Permission denied (os error 13)
                      Input/output error (os error 5)"
                }
            );
        }
    }

    mod push_withheld_note {
        use super::*;

        #[test]
        fn counts_the_withheld_files() {
            for (count, note) in [
                (1, "(1 file withheld by read deny or ask rules)"),
                (3, "(3 files withheld by read deny or ask rules)"),
            ] {
                let mut content = "a.rs".to_owned();
                push_withheld_note(&mut content, count);
                assert_eq!(
                    content,
                    formatdoc! {"
                        a.rs

                        {note}"
                    }
                );
            }

            let mut content = "a.rs".to_owned();
            push_withheld_note(&mut content, 0);
            assert_eq!(content, "a.rs");
        }
    }
}
