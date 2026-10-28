//! Session listing: indexed page queries, the latest session, and lookup by ID or ID prefix.

use anyhow::Result;
use rusqlite::{Connection, Row, params_from_iter, types::Value};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::{CONTENT_VERSION, SessionStore, time_column, to_millis};

pub const DEFAULT_SESSION_LIST_LIMIT: usize = 30;

/// The `sessions` columns [`read_info`] decodes, in its order.
const INFO_COLUMNS: &str =
    "id, forked_from, cwd, project_root, git_branch, updated_at, title, message_count, archived_at";

// ── SessionInfo ──

/// A saved session as listings show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    /// The session this one forked from, which may since have been deleted.
    pub forked_from: Option<String>,
    pub cwd: String,
    pub project_root: String,
    /// When the session's head last moved, which orders listings.
    pub last_active_at: OffsetDateTime,
    pub title: Option<String>,
    /// The messages on the active branch since its last compaction.
    pub message_count: u32,
    /// Branch the session started on, or `None` outside a git repo.
    pub git_branch: Option<String>,
    pub archive_state: ArchiveState,
}

/// Whether a session is archived, which keeps it out of the default listings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveState {
    #[default]
    Active,
    Archived,
}

impl From<bool> for ArchiveState {
    fn from(archived: bool) -> Self {
        if archived {
            Self::Archived
        } else {
            Self::Active
        }
    }
}

impl ArchiveState {
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Active => Self::Archived,
            Self::Archived => Self::Active,
        }
    }
}

// ── ListScope ──

/// Which sessions a listing covers: the home project or every project, and active or archived ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListScope {
    pub projects: ProjectScope,
    pub archive: ArchiveState,
}

/// Which projects a listing or lookup covers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProjectScope {
    /// The project the store was opened for.
    #[default]
    Current,
    Every,
}

impl ProjectScope {
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Current => Self::Every,
            Self::Every => Self::Current,
        }
    }

    /// Where a listing in this scope looks, as error messages name it.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Current => "this project",
            Self::Every => "all projects",
        }
    }
}

// ── ListPage ──

/// One page of sessions, and whether more matching sessions follow it.
#[derive(Debug)]
pub struct ListPage {
    sessions: Vec<SessionInfo>,
    more: bool,
}

impl ListPage {
    pub fn sessions(&self) -> &[SessionInfo] {
        &self.sessions
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn into_sessions(self) -> Vec<SessionInfo> {
        self.sessions
    }

    pub fn has_more(&self) -> bool {
        self.more
    }
}

// ── SessionWalk ──

/// The sessions in one [`ListScope`], most recently active first, read a page at a time. Each page
/// continues after the last session the previous one returned, so a session that moves between
/// pages may show twice or not at all, while the pages themselves never overlap.
pub struct SessionWalk {
    store: SessionStore,
    scope: ListScope,
    /// The `updated_at` and ID of the last session returned, which the next page starts after.
    after: Option<(i64, String)>,
    exhausted: bool,
}

impl SessionWalk {
    /// Whether the walk returned every session in its scope.
    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// The next `limit` sessions, fewer once the walk is exhausted.
    pub fn next_page(&mut self, limit: usize) -> Result<Vec<SessionInfo>> {
        if self.exhausted {
            return Ok(Vec::new());
        }
        let mut sessions = page(
            &self.store.connect()?,
            &self.store,
            self.scope,
            self.after.as_ref(),
            limit.saturating_add(1),
        )?;
        self.exhausted = sessions.len() <= limit;
        sessions.truncate(limit);
        if let Some(last) = sessions.last() {
            self.after = Some((to_millis(last.last_active_at), last.session_id.clone()));
        }
        Ok(sessions)
    }
}

// ── Listing ──

impl SessionStore {
    /// Walks the sessions in `scope` from the most recently active.
    pub fn walk(&self, scope: ListScope) -> SessionWalk {
        SessionWalk {
            store: self.clone(),
            scope,
            after: None,
            exhausted: false,
        }
    }

    /// Up to `limit` sessions from the [`walk`](Self::walk) of `scope`, unbounded for `None`.
    pub fn list_paged(&self, limit: Option<usize>, scope: ListScope) -> Result<ListPage> {
        let mut walk = self.walk(scope);
        let sessions = walk.next_page(limit.unwrap_or(usize::MAX))?;
        Ok(ListPage {
            sessions,
            more: !walk.is_exhausted(),
        })
    }

    /// The most recently active unarchived session.
    pub(crate) fn latest(&self, projects: ProjectScope) -> Result<Option<SessionInfo>> {
        let scope = ListScope {
            projects,
            archive: ArchiveState::Active,
        };
        Ok(page(&self.connect()?, self, scope, None, 1)?.pop())
    }

    /// Sessions in either archive state whose ID starts with `prefix`, most recently active first.
    /// A prefix with a character no session ID holds matches nothing.
    pub(crate) fn list_matching(
        &self,
        prefix: &str,
        projects: ProjectScope,
    ) -> Result<Vec<SessionInfo>> {
        if !prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Ok(Vec::new());
        }
        let mut params = vec![Value::Text(format!("{prefix}*"))];
        let project = match projects {
            ProjectScope::Current => {
                params.push(Value::Text(self.project.root.clone()));
                " AND project_root = ?2"
            }
            ProjectScope::Every => "",
        };
        let sql = format!(
            "SELECT {INFO_COLUMNS} FROM sessions WHERE id GLOB ?1{project} \
             ORDER BY updated_at DESC, id DESC"
        );
        query_infos(&self.connect()?, &sql, params)
    }

    /// The session `session_id`, in any project and either archive state.
    pub(crate) fn find(&self, session_id: &str) -> Result<Option<SessionInfo>> {
        let sql = format!("SELECT {INFO_COLUMNS} FROM sessions WHERE id = ?1");
        let params = vec![Value::Text(session_id.to_owned())];
        Ok(query_infos(&self.connect()?, &sql, params)?.pop())
    }
}

/// Up to `limit` sessions in `scope` after the `after` keyset position, most recently active
/// first. Sessions a newer build wrote stay hidden, since this build cannot resume them.
fn page(
    conn: &Connection,
    store: &SessionStore,
    scope: ListScope,
    after: Option<&(i64, String)>,
    limit: usize,
) -> Result<Vec<SessionInfo>> {
    let mut filters = vec![
        "(archived_at IS NOT NULL) = ?".to_owned(),
        "format <= ?".to_owned(),
    ];
    let mut params = vec![
        Value::Integer((scope.archive == ArchiveState::Archived).into()),
        Value::Integer(CONTENT_VERSION.into()),
    ];
    if scope.projects == ProjectScope::Current {
        filters.push("project_root = ?".to_owned());
        params.push(Value::Text(store.project.root.clone()));
    }
    if let Some((updated_at, id)) = after {
        filters.push("(updated_at, id) < (?, ?)".to_owned());
        params.push(Value::Integer(*updated_at));
        params.push(Value::Text(id.clone()));
    }
    params.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
    let sql = format!(
        "SELECT {INFO_COLUMNS} FROM sessions WHERE {} ORDER BY updated_at DESC, id DESC LIMIT ?",
        filters.join(" AND "),
    );
    query_infos(conn, &sql, params)
}

fn query_infos(conn: &Connection, sql: &str, params: Vec<Value>) -> Result<Vec<SessionInfo>> {
    let mut stmt = conn.prepare(sql)?;
    let infos = stmt
        .query_map(params_from_iter(params), read_info)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(infos)
}

/// Decodes a row of [`INFO_COLUMNS`].
fn read_info(row: &Row<'_>) -> rusqlite::Result<SessionInfo> {
    Ok(SessionInfo {
        session_id: row.get(0)?,
        forked_from: row.get(1)?,
        cwd: row.get(2)?,
        project_root: row.get(3)?,
        git_branch: row.get(4)?,
        last_active_at: time_column(row, 5)?,
        title: row.get(6)?,
        message_count: row.get(7)?,
        archive_state: row.get::<_, Option<i64>>(8)?.is_some().into(),
    })
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::session::store::testing::{archive_test_session, isolated_store, seed_test_session};

    fn ids(sessions: &[SessionInfo]) -> Vec<&str> {
        sessions
            .iter()
            .map(|info| info.session_id.as_str())
            .collect()
    }

    /// Seeds session `id` in `store`'s project, last active `minute` minutes into the day.
    fn seed(store: &SessionStore, id: &str, minute: u8) {
        let at = datetime!(2026-04-18 09:00:00 UTC)
            .replace_minute(minute)
            .unwrap();
        seed_test_session(store, id, None, 1, at);
    }

    mod session_walk {
        use super::*;

        mod next_page {
            use super::*;

            #[test]
            fn continues_after_the_last_session_without_gaps_or_repeats() {
                let (_dir, store) = isolated_store();
                for (id, minute) in [("a", 1), ("b", 2), ("c", 2), ("d", 3), ("e", 4)] {
                    seed(&store, id, minute);
                }
                let mut walk = store.walk(ListScope::default());

                let pages: Vec<(Vec<String>, bool)> = (0..4)
                    .map(|_| {
                        let page = walk.next_page(2).unwrap();
                        let ids = page.into_iter().map(|info| info.session_id).collect();
                        (ids, walk.is_exhausted())
                    })
                    .collect();

                let page = |ids: &[&str], exhausted| {
                    (ids.iter().map(|&id| id.to_owned()).collect(), exhausted)
                };
                assert_eq!(
                    pages,
                    [
                        page(&["e", "d"], false),
                        page(&["c", "b"], false),
                        page(&["a"], true),
                        page(&[], true),
                    ],
                    "ties on the last-active time order by ID",
                );
            }
        }
    }

    mod list_paged {
        use super::*;

        #[test]
        fn lists_the_scope_it_names_and_hides_sessions_a_newer_build_wrote() {
            let (dir, store) = isolated_store();
            let other = SessionStore::open_at(dir.path().to_path_buf(), "other").unwrap();
            seed(&store, "here", 1);
            seed(&store, "here-archived", 2);
            archive_test_session(&store, "here-archived");
            seed(&other, "there", 3);
            seed(&store, "newer", 4);
            store
                .connect()
                .unwrap()
                .execute(
                    "UPDATE sessions SET format = ?1 WHERE id = 'newer'",
                    [CONTENT_VERSION + 1],
                )
                .unwrap();

            for (projects, archive, expected) in [
                (ProjectScope::Current, ArchiveState::Active, vec!["here"]),
                (
                    ProjectScope::Current,
                    ArchiveState::Archived,
                    vec!["here-archived"],
                ),
                (
                    ProjectScope::Every,
                    ArchiveState::Active,
                    vec!["there", "here"],
                ),
                (
                    ProjectScope::Every,
                    ArchiveState::Archived,
                    vec!["here-archived"],
                ),
            ] {
                let scope = ListScope { projects, archive };

                let listed = store.list_paged(None, scope).unwrap();

                assert_eq!(ids(listed.sessions()), expected, "{scope:?}");
                assert!(!listed.has_more(), "{scope:?}");
            }
        }

        #[test]
        fn reports_more_past_the_limit() {
            let (_dir, store) = isolated_store();
            seed(&store, "older", 1);
            seed(&store, "newer", 2);

            let listed = store.list_paged(Some(1), ListScope::default()).unwrap();

            assert_eq!(ids(listed.sessions()), ["newer"]);
            assert!(listed.has_more());
        }
    }

    mod list_matching {
        use super::*;

        #[test]
        fn matches_the_prefix_in_the_scope_it_names_newest_first() {
            let (dir, store) = isolated_store();
            let other = SessionStore::open_at(dir.path().to_path_buf(), "other").unwrap();
            seed(&store, "abc", 1);
            seed(&other, "abd", 2);
            seed(&store, "xyz", 3);
            archive_test_session(&store, "abc");

            for (projects, expected) in [
                (ProjectScope::Current, vec!["abc"]),
                (ProjectScope::Every, vec!["abd", "abc"]),
            ] {
                let matched = store.list_matching("ab", projects).unwrap();

                assert_eq!(ids(&matched), expected, "{projects:?}");
            }
        }

        #[test]
        fn matches_nothing_for_a_prefix_with_a_glob_metacharacter() {
            let (_dir, store) = isolated_store();
            seed(&store, "abc", 1);

            for prefix in ["*", "?bc", "[a]", "a*"] {
                let matched = store.list_matching(prefix, ProjectScope::Every).unwrap();

                assert_eq!(matched, [], "{prefix}");
            }
        }
    }
}
