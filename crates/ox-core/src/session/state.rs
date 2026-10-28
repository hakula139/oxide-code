//! In-memory session state owned by the actor, which turns absorbed commands into the records of
//! the next [`Commit`]. It uses no tokio, so lifecycle transitions test without a runtime.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use rusqlite::Connection;
use uuid::Uuid;

use super::display::fit_title;
use super::handle::{Checkpoint, CompactBoundary, Fork};
use super::store::commit::{
    self, Commit, Head, Landed, MessageRow, Record, Saved, SessionRow, UsageRow,
};
use super::store::load::SessionData;
use super::store::{CompactInfo, SessionStore, UsageRecord, now_millis};
use crate::file_tracker::FileSnapshot;
use crate::message::{ContentBlock, Message, Role};
use crate::prompt::SessionContext;
use crate::tool::ToolMetadata;
use crate::util::fs::FileLock;

/// What every batch after a failed commit reports, since nothing is written once one fails.
pub(super) const STOPPED: &str = "session persistence stopped after an earlier write failed";

// ── SessionState ──

/// Session lifecycle that [`super::actor::run`] drives. The session row exists once the first
/// message commits, and each commit writes the records absorbed since the last one.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool tracks an independent part of the session lifecycle"
)]
pub(super) struct SessionState {
    store: SessionStore,
    /// Opened by the first commit.
    conn: Option<Connection>,
    row: SessionRow,
    /// `None` until the row exists.
    saved: Option<SavedAt>,
    /// The message the next one chains from.
    tip: Option<Uuid>,
    message_count: u32,
    /// Whether an earlier prompt or manual title consumed the first-prompt title opportunity.
    first_user_prompt_seen: bool,
    /// Records absorbed since the last commit.
    records: Vec<Record>,
    /// Whether a record since the last commit moved the head.
    head_moved: bool,
    finished: bool,
    stopped: bool,
}

/// A saved session: the head this process last committed or loaded, and the liveness lock it holds
/// while the session is open.
struct SavedAt {
    base: Option<Uuid>,
    _lock: FileLock,
}

impl SessionState {
    /// A session on `git_branch` with no row, which its first message commit creates.
    pub(super) fn fresh(store: SessionStore, model: &str, git_branch: Option<String>) -> Self {
        let project = store.project();
        let row = SessionRow {
            id: Uuid::new_v4().to_string(),
            forked_from: None,
            cwd: project.cwd.clone(),
            project_root: project.root.clone(),
            git_branch,
            model: model.to_owned(),
            created_at: now_millis(),
            title: None,
            title_manual: false,
        };
        Self::new(store, row, None, None, 0, false)
    }

    /// The saved session `data` loaded, holding `lock`, whose branch keeps `message_count` messages
    /// after sanitizing. A fork records this process's directories and `model`.
    pub(super) fn resumed(
        store: SessionStore,
        data: &SessionData,
        message_count: u32,
        model: &str,
        lock: FileLock,
    ) -> Self {
        let project = store.project();
        let row = SessionRow {
            id: data.session_id.clone(),
            forked_from: data.forked_from.clone(),
            cwd: project.cwd.clone(),
            project_root: project.root.clone(),
            git_branch: data.git_branch.clone(),
            model: model.to_owned(),
            created_at: data.created_at,
            title: data.title.clone(),
            title_manual: data.title_manual,
        };
        let saved = SavedAt {
            base: data.head,
            _lock: lock,
        };
        Self::new(
            store,
            row,
            Some(saved),
            data.head,
            message_count,
            data.first_user_prompt_seen,
        )
    }

    fn new(
        store: SessionStore,
        row: SessionRow,
        saved: Option<SavedAt>,
        tip: Option<Uuid>,
        message_count: u32,
        first_user_prompt_seen: bool,
    ) -> Self {
        Self {
            store,
            conn: None,
            row,
            saved,
            tip,
            message_count,
            first_user_prompt_seen,
            records: Vec::new(),
            head_moved: false,
            finished: false,
            stopped: false,
        }
    }

    pub(super) fn session_id(&self) -> &str {
        &self.row.id
    }

    /// The directory the session's artifacts go to, which follows a fork.
    pub(super) fn artifact_dir(&self) -> PathBuf {
        self.store.artifact_dir(&self.row.id)
    }

    /// Queues `message` with the prompt state it is the first to carry and the metadata of its tool
    /// results, and returns the AI-title seed on the first user text. The first-prompt title is
    /// `title`, or else that text. `manual_title_set` skips both the first-prompt title and the
    /// AI-title seed.
    pub(super) fn queue_message(
        &mut self,
        message: Message,
        context: Option<SessionContext>,
        tool_metadata: BTreeMap<String, ToolMetadata>,
        title: Option<&str>,
        manual_title_set: bool,
    ) -> Option<String> {
        let mut ai_title_seed = None;
        if !self.first_user_prompt_seen
            && let Some(text) = extract_user_text(&message)
        {
            self.first_user_prompt_seen = true;
            if !manual_title_set {
                ai_title_seed = Some(text.to_owned());
                let title = fit_title(title.unwrap_or(text));
                self.set_title(title, false);
            }
        }
        let parent_id = self.tip;
        self.push_message(MessageRow {
            id: Uuid::new_v4(),
            parent_id,
            message,
            context,
            compact: None,
            tool_metadata,
            created_at: now_millis(),
        });
        self.message_count = self.message_count.saturating_add(1);
        ai_title_seed
    }

    /// The branch point the next message chains from.
    pub(super) fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            tip: self.tip,
            message_count: self.message_count,
            first_user_prompt_seen: self.first_user_prompt_seen,
        }
    }

    /// Moves the head back to `checkpoint`. A rewind to before the first user prompt lets the next
    /// one title the session again.
    pub(super) fn rewind(&mut self, checkpoint: Checkpoint, manual_title_set: bool) {
        self.tip = checkpoint.tip;
        self.message_count = checkpoint.message_count;
        self.first_user_prompt_seen = checkpoint.first_user_prompt_seen || manual_title_set;
        self.head_moved = true;
    }

    /// Queues the compaction root `synthetic_message`, which opens a new chain carrying `boundary`,
    /// and returns the message count it replaces. The compaction takes effect in memory whether or
    /// not its commit lands.
    pub(super) fn compact(&mut self, boundary: CompactBoundary, synthetic_message: Message) -> u32 {
        let CompactBoundary { record, context } = boundary;
        let pre_message_count = self.message_count;
        self.push_message(MessageRow {
            id: Uuid::new_v4(),
            parent_id: None,
            message: synthetic_message,
            context: Some(context),
            compact: Some(CompactInfo {
                record,
                pre_message_count,
            }),
            tool_metadata: BTreeMap::new(),
            created_at: now_millis(),
        });
        self.message_count = 1;
        // The synthetic root is user-role but must not seed a duplicate first-prompt title.
        self.first_user_prompt_seen = true;
        pre_message_count
    }

    fn push_message(&mut self, row: MessageRow) {
        self.tip = Some(row.id);
        self.head_moved = true;
        self.records.push(Record::Message(Box::new(row)));
    }

    pub(super) fn queue_usage(&mut self, records: Vec<UsageRecord>) {
        let created_at = now_millis();
        self.records.extend(records.into_iter().map(|record| {
            Record::Usage(UsageRow {
                id: Uuid::new_v4(),
                record,
                created_at,
            })
        }));
    }

    /// Titles the session. A fresh session's row takes the title when its first message commits.
    pub(super) fn set_title(&mut self, title: String, manual: bool) {
        if self.saved.is_some() {
            self.records.push(Record::Title {
                title: title.clone(),
                manual,
            });
        }
        self.row.title = Some(title);
        self.row.title_manual = manual;
    }

    pub(super) fn queue_archive(&mut self) {
        self.records.push(Record::Archive);
    }

    /// Queues the file tracker `snapshots` that close the session, once.
    pub(super) fn finish(&mut self, snapshots: Vec<FileSnapshot>) {
        if !std::mem::replace(&mut self.finished, true) {
            self.records.push(Record::FileSnapshots(snapshots));
        }
    }

    /// Commits the records absorbed since the last commit, returning the fork it made. A fresh
    /// session keeps its records until a message creates its row. The first failure stops
    /// persistence for good, and every later batch with records fails with [`STOPPED`].
    pub(super) fn flush(&mut self) -> Result<Option<Fork>> {
        if self.stopped {
            if self.records.is_empty() && !self.head_moved {
                return Ok(None);
            }
            self.records.clear();
            self.head_moved = false;
            return Err(anyhow!(STOPPED));
        }
        let materializes = self
            .records
            .iter()
            .any(|record| matches!(record, Record::Message(_)));
        if self.saved.is_none() && !materializes
            || self.saved.is_some() && self.records.is_empty() && !self.head_moved
        {
            return Ok(None);
        }

        let result = self.commit();
        self.records.clear();
        self.head_moved = false;
        match result {
            Ok(forked) => Ok(forked),
            Err(e) => {
                self.stopped = true;
                Err(e)
            }
        }
    }

    fn commit(&mut self) -> Result<Option<Fork>> {
        let conn = match &mut self.conn {
            Some(conn) => conn,
            conn => conn.insert(self.store.connect()?),
        };
        let head = self.head_moved.then_some(Head {
            id: self.tip,
            message_count: self.message_count,
        });
        let saved = match &self.saved {
            Some(saved) => Saved::At { base: saved.base },
            None => Saved::Fresh,
        };
        let landed = commit::commit(
            conn,
            &self.store,
            &Commit {
                session: &self.row,
                saved,
                records: &self.records,
                head,
            },
        )?;
        let base = match &self.saved {
            Some(saved) if head.is_none() => saved.base,
            _ => self.tip,
        };
        let (lock, forked) = match landed {
            Landed::InPlace => {
                if let Some(saved) = &mut self.saved {
                    saved.base = base;
                }
                return Ok(None);
            }
            Landed::Created(lock) => (lock, None),
            Landed::Forked {
                id,
                lock,
                title,
                title_manual,
            } => {
                let from = std::mem::replace(&mut self.row.id, id);
                self.row.forked_from = Some(from.clone());
                self.row.title = title;
                self.row.title_manual = title_manual;
                let to = self.row.id.clone();
                (lock, Some(Fork { from, to }))
            }
        };
        self.saved = Some(SavedAt { base, _lock: lock });
        Ok(forked)
    }
}

// ── Helpers ──

/// Extracts the first nonempty ordinary text from a user message.
pub(super) fn extract_user_text(message: &Message) -> Option<&str> {
    if message.role != Role::User {
        return None;
    }
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, origin: None } => Some(text.as_str()),
            _ => None,
        })
        .find(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::file_tracker::{FileSource, FileState, LastView};
    use crate::session::handle::testing::boundary;
    use crate::session::store::local_testing::{fail_writes, message_row, save};
    use crate::session::store::testing::isolated_store;
    use crate::tool::StoredView;

    /// The first text of each message in `messages`.
    fn texts(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .map(|message| match message.content.first() {
                Some(ContentBlock::Text { text, .. }) => text.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// The texts of the chain session `session_id` resumes.
    fn chain(store: &SessionStore, session_id: &str) -> Vec<String> {
        texts(&store.load_session_data(session_id).unwrap().messages)
    }

    /// Every message ID session `session_id` holds, in insertion order.
    fn row_ids(store: &SessionStore, session_id: &str) -> Vec<Uuid> {
        let conn = store.connect().unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM messages WHERE session_id = ?1 ORDER BY seq")
            .unwrap();
        stmt.query_map([session_id], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|id| Uuid::parse_str(&id.unwrap()).unwrap())
            .collect()
    }

    fn usage_rows(store: &SessionStore, session_id: &str) -> i64 {
        store
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM usage WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn usage() -> UsageRecord {
        serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-5-5",
            "cache_ttl": "5m",
            "input_tokens": 1,
            "output_tokens": 2,
        }))
        .unwrap()
    }

    fn queue_user(state: &mut SessionState, text: &str) -> Option<String> {
        state.queue_message(Message::user(text), None, BTreeMap::new(), None, false)
    }

    fn queue_assistant(state: &mut SessionState, text: &str) {
        state.queue_message(Message::assistant(text), None, BTreeMap::new(), None, false);
    }

    /// Resumes the saved session `session_id` as another process would, recording `model`.
    fn open(store: &SessionStore, session_id: &str, model: &str) -> SessionState {
        let lock = store.hold(session_id).unwrap();
        let data = store.load_session_data(session_id).unwrap();
        let count = u32::try_from(data.messages.len()).unwrap();
        SessionState::resumed(store.clone(), &data, count, model, lock)
    }

    /// A saved session holding the user prompt `text`.
    fn saved_session(store: &SessionStore, text: &str) -> String {
        let mut state = SessionState::fresh(store.clone(), "m", None);
        queue_user(&mut state, text);
        state.flush().unwrap();
        state.session_id().to_owned()
    }

    mod fresh {
        use super::*;

        #[test]
        fn names_the_session_with_a_uuid_v4() {
            let (_dir, store) = isolated_store();

            let state = SessionState::fresh(store, "m", None);

            let id = Uuid::parse_str(state.session_id()).unwrap();
            assert_eq!(id.get_version_num(), 4, "{id}");
        }
    }

    mod queue_message {
        use super::*;

        #[test]
        fn titles_and_seeds_only_on_the_first_user_text() {
            for (name, title, manual_title_set, expected_title, expected_seed) in [
                (
                    "plain",
                    None,
                    false,
                    Some("Fix the auth bug"),
                    Some("Fix the auth bug"),
                ),
                (
                    "given title",
                    Some("/init AGENTS.md"),
                    false,
                    Some("/init AGENTS.md"),
                    Some("Fix the auth bug"),
                ),
                ("manual title", None, true, None, None),
            ] {
                let (_dir, store) = isolated_store();
                let mut state = SessionState::fresh(store, "m", None);
                state.queue_message(
                    Message {
                        role: Role::User,
                        content: vec![ContentBlock::tool_result("t1", "output", false)],
                    },
                    None,
                    BTreeMap::new(),
                    None,
                    manual_title_set,
                );
                queue_assistant(&mut state, "reply");

                let seed = state.queue_message(
                    Message::user("Fix the auth bug"),
                    None,
                    BTreeMap::new(),
                    title,
                    manual_title_set,
                );
                let again = queue_user(&mut state, "follow up");

                assert_eq!(seed.as_deref(), expected_seed, "{name}");
                assert_eq!(again, None, "{name}");
                assert_eq!(state.row.title.as_deref(), expected_title, "{name}");
                assert_eq!(state.message_count, 4, "{name}");
            }
        }
    }

    mod flush {
        use super::*;

        #[test]
        fn creates_the_row_with_the_first_message_and_what_preceded_it() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store.clone(), "m", Some("main".to_owned()));
            let id = state.session_id().to_owned();
            state.set_title("Named".to_owned(), true);
            state.queue_usage(vec![usage()]);

            assert_eq!(state.flush().unwrap(), None);
            assert_eq!(store.find(&id).unwrap(), None, "no message, no row");
            state.queue_message(Message::user("hello"), None, BTreeMap::new(), None, true);
            assert_eq!(state.flush().unwrap(), None);

            let info = store.find(&id).unwrap().unwrap();
            assert_eq!(info.title.as_deref(), Some("Named"));
            assert_eq!(info.message_count, 1);
            assert_eq!(info.git_branch.as_deref(), Some("main"));
            let data = store.load_session_data(&id).unwrap();
            assert!(data.title_manual);
            assert_eq!(data.head, Some(row_ids(&store, &id)[0]));
            assert_eq!(texts(&data.messages), ["hello"]);
            assert_eq!(usage_rows(&store, &id), 1);
        }

        #[test]
        fn writes_a_round_in_order_with_the_results_metadata() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store.clone(), "m", None);
            let id = state.session_id().to_owned();
            let metadata = ToolMetadata {
                title: Some("Read f.rs".to_owned()),
                result_view: Some(StoredView::Text),
            };
            queue_user(&mut state, "read it");
            queue_assistant(&mut state, "reading");
            state.queue_message(
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::tool_result("t1", "fn main() {}", false)],
                },
                None,
                BTreeMap::from([("t1".to_owned(), metadata.clone())]),
                None,
                false,
            );

            state.flush().unwrap();

            let data = store.load_session_data(&id).unwrap();
            assert_eq!(data.messages.len(), 3);
            assert_eq!(data.messages[1], Message::assistant("reading"));
            assert_eq!(
                data.tool_metadata,
                HashMap::from([("t1".to_owned(), metadata)])
            );
        }

        #[test]
        fn compaction_saves_a_root_with_its_boundary_and_moves_the_head() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store.clone(), "m", None);
            let id = state.session_id().to_owned();
            queue_user(&mut state, "a");
            queue_assistant(&mut state, "b");
            state.flush().unwrap();

            let pre_count = state.compact(boundary("summary"), Message::user("synthetic"));
            state.flush().unwrap();

            assert_eq!(pre_count, 2);
            let data = store.load_session_data(&id).unwrap();
            assert_eq!(texts(&data.messages), ["synthetic"]);
            let compact = data.compact.unwrap();
            assert_eq!(compact.pre_message_count, 2);
            assert_eq!(compact.record.summary, "summary");
            assert_eq!(data.context, Some(SessionContext::default()));
            assert_eq!(data.head, row_ids(&store, &id).last().copied());
            assert_eq!(store.find(&id).unwrap().unwrap().message_count, 1);
            assert_eq!(row_ids(&store, &id).len(), 3, "pre-compact rows stay");
        }

        #[test]
        fn rewind_moves_the_head_and_keeps_every_row() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store.clone(), "m", None);
            let id = state.session_id().to_owned();
            queue_user(&mut state, "kept");
            let checkpoint = state.checkpoint();
            queue_user(&mut state, "refused");
            state.flush().unwrap();

            state.rewind(checkpoint, false);
            state.flush().unwrap();
            queue_user(&mut state, "next");
            state.flush().unwrap();

            assert_eq!(chain(&store, &id), ["kept", "next"]);
            assert_eq!(row_ids(&store, &id).len(), 3);
            assert_eq!(store.find(&id).unwrap().unwrap().message_count, 2);
        }

        #[test]
        fn a_failed_commit_stops_persistence_for_good() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store.clone(), "m", None);
            let id = state.session_id().to_owned();
            fail_writes(&store);
            queue_user(&mut state, "lost");

            let first = state.flush().unwrap_err();
            queue_user(&mut state, "dropped");
            let later = state.flush().unwrap_err();
            let empty = state.flush();

            assert!(format!("{first:#}").contains("disk full"), "{first:#}");
            assert_eq!(later.to_string(), STOPPED);
            assert_eq!(
                empty.unwrap(),
                None,
                "a batch with no records reports nothing"
            );
            assert_eq!(store.find(&id).unwrap(), None);
        }

        #[test]
        fn saves_the_finish_snapshots_once() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store.clone(), "m", None);
            let id = state.session_id().to_owned();
            queue_user(&mut state, "read f.rs");
            state.flush().unwrap();

            state.finish(vec![snapshot("/repo/f.rs")]);
            state.finish(vec![snapshot("/repo/g.rs")]);
            state.flush().unwrap();

            let paths: Vec<_> = store
                .load_session_data(&id)
                .unwrap()
                .file_snapshots
                .into_iter()
                .map(|snapshot| snapshot.path)
                .collect();
            assert_eq!(paths, [PathBuf::from("/repo/f.rs")]);
        }

        #[test]
        fn leaves_no_row_for_a_session_without_messages() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store.clone(), "m", None);
            state.queue_usage(vec![usage()]);
            state.finish(Vec::new());

            assert_eq!(state.flush().unwrap(), None);

            assert_eq!(store.list().unwrap(), []);
        }
    }

    mod lazy_fork {
        use super::*;

        /// What a writer in the scenario table does on the saved session.
        #[derive(Debug, Clone, Copy)]
        enum Step {
            Open(usize),
            Write(usize, &'static str),
            /// Moves the head back to before the writer's last write.
            Rewind(usize),
            Rename(usize),
            Archive(usize),
            Close(usize),
        }

        use Step::{Archive, Close, Open, Rename, Rewind, Write};

        #[test]
        fn forks_only_a_writer_whose_view_another_writer_moved() {
            for (name, steps, a_forks, source_chain, a_chain) in [
                (
                    "A and B open S, only B writes",
                    vec![Open(0), Open(1), Write(1, "b")],
                    false,
                    vec!["start", "b"],
                    vec!["start", "b"],
                ),
                (
                    "A and B open S, B writes, then A writes",
                    vec![Open(0), Open(1), Write(1, "b"), Write(0, "a")],
                    true,
                    vec!["start", "b"],
                    vec!["start", "a"],
                ),
                (
                    "A writes, B opens S and writes, then A writes again",
                    vec![
                        Open(0),
                        Write(0, "a1"),
                        Open(1),
                        Write(1, "b"),
                        Write(0, "a2"),
                    ],
                    true,
                    vec!["start", "a1", "b"],
                    vec!["start", "a1", "a2"],
                ),
                (
                    "B writes and exits, then A writes from its older view",
                    vec![Open(0), Open(1), Write(1, "b"), Close(1), Write(0, "a")],
                    true,
                    vec!["start", "b"],
                    vec!["start", "a"],
                ),
                (
                    "B renames and archives S, then A writes",
                    vec![Open(0), Open(1), Rename(1), Archive(1), Write(0, "a")],
                    false,
                    vec!["start", "a"],
                    vec!["start", "a"],
                ),
                (
                    "B rewinds S to exactly the head A last saw, then A writes",
                    vec![Open(0), Open(1), Write(1, "b"), Rewind(1), Write(0, "a")],
                    false,
                    vec!["start", "a"],
                    vec!["start", "a"],
                ),
            ] {
                let (_dir, store) = isolated_store();
                let source = saved_session(&store, "start");
                let mut writers: [Option<SessionState>; 2] = [None, None];
                let mut checkpoints: [Option<Checkpoint>; 2] = [None, None];
                let mut forks = Vec::new();

                for step in steps {
                    match step {
                        Open(who) => writers[who] = Some(open(&store, &source, "m")),
                        Write(who, text) => {
                            let state = writers[who].as_mut().unwrap();
                            checkpoints[who] = Some(state.checkpoint());
                            queue_user(state, text);
                            forks.extend(state.flush().unwrap().map(|fork| (who, fork)));
                        }
                        Rewind(who) => {
                            let state = writers[who].as_mut().unwrap();
                            state.rewind(checkpoints[who].unwrap(), false);
                            state.flush().unwrap();
                        }
                        Rename(who) => {
                            let state = writers[who].as_mut().unwrap();
                            state.set_title("Renamed".to_owned(), true);
                            state.flush().unwrap();
                        }
                        Archive(who) => {
                            let state = writers[who].as_mut().unwrap();
                            state.queue_archive();
                            state.flush().unwrap();
                        }
                        Close(who) => writers[who] = None,
                    }
                }

                let a_id = writers[0].as_ref().unwrap().session_id().to_owned();
                assert_eq!(a_id != source, a_forks, "{name}");
                let expected_forks: Vec<_> = a_forks
                    .then(|| {
                        (
                            0,
                            Fork {
                                from: source.clone(),
                                to: a_id.clone(),
                            },
                        )
                    })
                    .into_iter()
                    .collect();
                assert_eq!(forks, expected_forks, "{name}");
                assert_eq!(chain(&store, &source), source_chain, "{name}");
                assert_eq!(chain(&store, &a_id), a_chain, "{name}");
            }
        }

        #[test]
        fn copies_the_base_chain_since_the_last_compaction_into_a_fork_of_its_own() {
            let (dir, store) = isolated_store();
            let mut original = SessionState::fresh(store.clone(), "m", None);
            let source = original.session_id().to_owned();
            queue_user(&mut original, "before compaction");
            original.compact(boundary("summary"), Message::user("root"));
            queue_user(&mut original, "kept");
            let checkpoint = original.checkpoint();
            queue_user(&mut original, "abandoned");
            original.rewind(checkpoint, false);
            original.queue_usage(vec![usage()]);
            original.set_title("Manual".to_owned(), true);
            original.flush().unwrap();
            drop(original);
            let other = SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
            let mut stale = open(&other, &source, "stale-model");
            let mut writer = open(&store, &source, "m");
            queue_user(&mut writer, "other process");
            writer.flush().unwrap();
            drop(writer);
            let source_rows = row_ids(&store, &source);
            let before_fork = now_millis();

            queue_user(&mut stale, "forked");
            let fork = stale.flush().unwrap().expect("the stale write forks");
            queue_user(&mut stale, "after the fork");
            let later = stale.flush().unwrap();

            let fork_id = stale.session_id().to_owned();
            assert_eq!(fork.from, source);
            assert_eq!(fork.to, fork_id);
            assert_eq!(later, None, "the fork reports once");
            assert_eq!(
                chain(&store, &fork_id),
                ["root", "kept", "forked", "after the fork"]
            );
            assert_eq!(
                row_ids(&store, &fork_id)[..2],
                source_rows[1..3],
                "the copied rows keep their IDs"
            );
            assert_eq!(usage_rows(&store, &fork_id), 0);
            assert_eq!(
                row_ids(&store, &source),
                source_rows,
                "the source is untouched"
            );
            assert_eq!(chain(&store, &source), ["root", "kept", "other process"]);
            let info = store.find(&fork_id).unwrap().unwrap();
            assert_eq!(info.forked_from.as_deref(), Some(source.as_str()));
            assert_eq!(info.title.as_deref(), Some("Manual"));
            assert_eq!(info.project_root, "/work/other-project");
            assert_eq!(info.message_count, 4);
            let data = store.load_session_data(&fork_id).unwrap();
            assert!(data.title_manual);
            let (model, created_at): (String, i64) = store
                .connect()
                .unwrap()
                .query_row(
                    "SELECT model, created_at FROM sessions WHERE id = ?1",
                    [&fork_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(model, "stale-model");
            assert!(created_at >= before_fork, "the fork starts when it forks");
            store
                .delete(&source)
                .expect("the forking process released the source's lock");
            assert!(store.delete(&fork_id).is_err(), "the fork's lock is held");
        }

        #[test]
        fn forks_without_copying_a_chain_when_the_writer_left_its_branch_empty() {
            let (_dir, store) = isolated_store();
            let mut stale = SessionState::fresh(store.clone(), "m", None);
            let source = stale.session_id().to_owned();
            let checkpoint = stale.checkpoint();
            queue_user(&mut stale, "refused");
            stale.flush().unwrap();
            stale.rewind(checkpoint, false);
            stale.flush().unwrap();
            let elsewhere = message_row(9, None, Message::user("elsewhere"));
            save(&store, &source, vec![elsewhere], Vec::new(), Some(9));

            queue_user(&mut stale, "next");
            let fork = stale.flush().unwrap().expect("the stale write forks");

            assert_eq!(fork.from, source);
            assert_eq!(chain(&store, &fork.to), ["next"]);
            assert_eq!(chain(&store, &source), ["elsewhere"]);
        }

        #[test]
        fn a_stale_finish_leaves_the_source_snapshots_alone() {
            let (_dir, store) = isolated_store();
            let source = saved_session(&store, "start");
            let mut stale = open(&store, &source, "m");
            let mut writer = open(&store, &source, "m");
            queue_user(&mut writer, "moved");
            writer.finish(vec![snapshot("/repo/current.rs")]);
            writer.flush().unwrap();

            stale.finish(vec![snapshot("/repo/stale.rs")]);
            assert_eq!(stale.flush().unwrap(), None, "a finish moves no head");

            let paths: Vec<_> = store
                .load_session_data(&source)
                .unwrap()
                .file_snapshots
                .into_iter()
                .map(|snapshot| snapshot.path)
                .collect();
            assert_eq!(paths, [PathBuf::from("/repo/current.rs")]);
            assert_eq!(stale.session_id(), source);
        }
    }

    mod extract_user_text {
        use super::*;

        #[test]
        fn generated_text_cannot_seed_a_title() {
            use crate::message::TextOrigin;

            for origin in [
                TextOrigin::SystemReminder,
                TextOrigin::Interruption,
                TextOrigin::CompactionSummary,
                TextOrigin::CompactionTruncation,
            ] {
                let mut message = Message {
                    role: Role::User,
                    content: vec![ContentBlock::generated_text("generated", origin)],
                };
                assert_eq!(extract_user_text(&message), None, "{origin:?}");

                message.content.push(ContentBlock::text("literal"));

                assert_eq!(extract_user_text(&message), Some("literal"), "{origin:?}");
            }
        }

        #[test]
        fn selects_first_nonempty_user_text() {
            let tool_result = ContentBlock::tool_result("t1", "output", false);
            let user_with_only_blank = Message {
                role: Role::User,
                content: vec![ContentBlock::text("  ")],
            };
            let user_with_only_tool_result = Message {
                role: Role::User,
                content: vec![tool_result.clone()],
            };
            let user_with_text_after_tool_result = Message {
                role: Role::User,
                content: vec![tool_result, ContentBlock::text("follow-up")],
            };

            assert_eq!(extract_user_text(&Message::user("hello")), Some("hello"));
            assert_eq!(extract_user_text(&Message::assistant("hello")), None);
            assert_eq!(extract_user_text(&user_with_only_blank), None);
            assert_eq!(extract_user_text(&user_with_only_tool_result), None);
            assert_eq!(
                extract_user_text(&user_with_text_after_tool_result),
                Some("follow-up"),
            );
        }
    }

    fn snapshot(path: &str) -> FileSnapshot {
        FileSnapshot {
            path: PathBuf::from(path),
            state: FileState {
                content_hash: 42,
                last_view: LastView::Full,
                source: FileSource::Read,
                recorded_at: time::macros::datetime!(2026-01-01 00:00:01 UTC),
                tool_use_id: "t1".to_owned(),
            },
        }
    }
}
