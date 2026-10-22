//! What the search tools share: the resolved root, the file walk, and the notes on what a search
//! could not read or withheld.

use std::cmp::Reverse;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;

use super::{Prepared, ToolCtx, display_path, extract_input_field, tool_path};
use crate::permission::{Access, WalkFilter, Workspace};
use crate::util::text::{ELLIPSIS, plural};

// ── Search Root ──

/// Prepares a search tool's call, resolving its `path` argument, the cwd by default, as the root
/// the gate checks and the walk covers.
pub(super) fn prepare_search_root(input: serde_json::Value, ctx: &ToolCtx) -> Prepared {
    let root = search_path(extract_input_field(&input, "path"));
    let access = Access::for_path(root, &ctx.workspace);
    Prepared { input, access }
}

/// The root a search call's `path` argument names, the cwd by default, read as [`tool_path`]
/// reads it.
/// The gate and the walk both take the root from here, so they see one spelling.
pub(super) fn search_path(path: Option<&str>) -> PathBuf {
    tool_path(path.unwrap_or("."))
}

/// Where a search tool's walk starts, which its prepared call resolved.
pub(super) struct SearchRoot {
    /// The resolved root, which exists.
    pub(super) base: PathBuf,
    /// The root as the call spells it with `~` expanded, for the read deny and ask rules each found
    /// file faces.
    pub(super) spelled: PathBuf,
    /// The canonical working directory, so hits under the canonical base render relative to it.
    pub(super) cwd: PathBuf,
    /// The session's workspace, whose anchor directories those rules apply below.
    pub(super) workspace: Arc<Workspace>,
}

impl SearchRoot {
    /// The root `access` resolved for a search in `workspace` whose call spells it as `path`, the
    /// cwd by default. The root must exist.
    pub(super) fn new(
        access: &Access,
        path: Option<&str>,
        workspace: Arc<Workspace>,
    ) -> Result<Self, String> {
        let cwd = workspace.cwd();
        let spelled = search_path(path);
        let Some(base) = access.resolved() else {
            return Err(format!(
                "Failed to access {}: the search root was not resolved.",
                display_path(&spelled, cwd)
            ));
        };
        if let Err(e) = std::fs::metadata(&base) {
            return Err(format!(
                "Failed to access {}: {e}",
                display_path(&cwd.join(&spelled), cwd)
            ));
        }
        let cwd = dunce::canonicalize(cwd)
            .map_err(|e| format!("Failed to get working directory: {e}"))?;
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
/// dotfile config stays searchable, skips VCS metadata directories below `base`, and pins the walk
/// to the starting filesystem so symlinked mounts don't escape.
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
    read_filter: &WalkFilter<'_>,
) -> usize {
    let found = unreadable.len();
    unreadable.retain(|u| {
        u.path
            .as_deref()
            .is_none_or(|path| !read_filter.withholds(path))
    });
    found - unreadable.len()
}

/// `entries` sorted newest first by modification time, keeping walk order among equal times.
pub(super) fn newest_first(
    entries: impl Iterator<Item = ignore::DirEntry>,
) -> Vec<ignore::DirEntry> {
    let mut entries: Vec<_> = entries.collect();
    entries.sort_by_cached_key(|entry| Reverse(entry_mtime(entry)));
    entries
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

/// Appends the note a search tool adds when read deny or ask rules withheld `count` of the files
/// it found. The note counts them without naming any.
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
    use crate::tool::local_testing::{
        create_modified_at, deny_read_access, deny_reads, search_root,
    };
    use crate::util::path::local_testing::canonical_tempdir;
    use crate::util::path::testing::HOME_VAR;

    mod new {
        use super::*;

        #[test]
        fn resolves_relative_and_absolute_paths_and_keeps_their_spelling() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join("src")).unwrap();
            fs::create_dir(dir.path().join("lib")).unwrap();
            let lib = dir.path().join("lib");

            let relative = search_root(dir.path(), Some("./src/../lib")).unwrap();
            let absolute = search_root(Path::new("/"), Some(lib.to_str().unwrap())).unwrap();

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
                search_root(dir.path(), Some("~/notes")).unwrap()
            });

            assert_eq!(
                (root.base, root.spelled),
                (home.join("notes"), home.join("notes"))
            );
        }

        #[test]
        fn defaults_to_the_cwd_without_a_path() {
            let dir = canonical_tempdir();

            let root = search_root(dir.path(), None).unwrap();

            assert_eq!(
                (root.base, root.spelled),
                (dir.path().to_owned(), PathBuf::from(".")),
            );
        }

        #[test]
        fn missing_path_is_named_relative_to_cwd() {
            let dir = canonical_tempdir();

            let err = search_root(dir.path(), Some("missing")).err().unwrap();

            let cause = fs::metadata(dir.path().join("missing")).unwrap_err();
            assert_eq!(err, format!("Failed to access missing: {cause}"));
        }

        #[test]
        fn refuses_an_access_that_is_not_a_path() {
            let workspace = Arc::new(Workspace::new(Path::new("/repo")));
            let err = SearchRoot::new(&Access::None, Some("src"), workspace)
                .err()
                .unwrap();
            assert_eq!(
                err,
                "Failed to access src: the search root was not resolved."
            );
        }

        #[cfg(unix)]
        #[test]
        fn resolves_dot_dot_after_a_symlink_physically() {
            let dir = canonical_tempdir();
            fs::create_dir_all(dir.path().join("real/target")).unwrap();
            let cwd = dir.path().join("cwd");
            fs::create_dir(&cwd).unwrap();
            std::os::unix::fs::symlink(dir.path().join("real/target"), cwd.join("link")).unwrap();

            let root = search_root(&cwd, Some("link/..")).unwrap();

            assert_eq!(root.base, dir.path().join("real"));
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn walks_from_a_root_whose_name_is_not_utf8() {
            use std::os::unix::ffi::OsStrExt as _;
            let dir = canonical_tempdir();
            let cwd = dir.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
            fs::create_dir(&cwd).unwrap();

            let root = search_root(&cwd, None).unwrap();

            assert_eq!(root.base, cwd);
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
            let root = search_root(dir.path(), None).unwrap();
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

            let sorted = newest_first(entries.into_iter());

            let names: Vec<_> = sorted
                .iter()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["b.txt", "c.txt", "a.txt"]);
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
