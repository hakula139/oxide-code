use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use tracing::{debug, warn};
use uuid::Uuid;

use super::entry::{CURRENT_VERSION, Entry, ExitInfo, SessionInfo, TitleInfo};
use crate::message::Message;

const DATA_DIR: &str = "ox";
const SESSIONS_DIR: &str = "sessions";

/// Tail buffer size for extracting the latest [`Entry::Title`] /
/// [`Entry::Summary`] without reading the entire file. 4 KB is generous
/// for a single JSON line.
const TAIL_BUF_SIZE: u64 = 4096;

// ── SessionStore ──

/// Low-level session file operations.
///
/// Each session is a JSONL file in `$XDG_DATA_HOME/ox/sessions/`. The store
/// handles path resolution, file creation, reading, and listing.
pub(crate) struct SessionStore {
    sessions_dir: PathBuf,
}

impl SessionStore {
    /// Create a store rooted at the XDG data directory.
    ///
    /// Creates `$XDG_DATA_HOME/ox/sessions/` if it does not exist.
    pub(crate) fn open() -> Result<Self> {
        let sessions_dir = resolve_sessions_dir(
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            dirs::home_dir(),
        )
        .context("cannot determine session storage directory")?;

        fs::create_dir_all(&sessions_dir)
            .with_context(|| format!("failed to create {}", sessions_dir.display()))?;

        debug!("session store at {}", sessions_dir.display());
        Ok(Self { sessions_dir })
    }

    /// Create a new session file and write the header entry.
    ///
    /// Takes an exclusive advisory lock on the file to prevent concurrent
    /// access. The lock is held for the lifetime of the returned writer.
    /// On Unix, the file is created with mode `0o600` so session contents
    /// (verbatim tool output, assistant responses) are not world-readable.
    pub(crate) fn create(&self, header: &Entry) -> Result<SessionWriter> {
        let Entry::Header { session_id, .. } = header else {
            bail!("expected Header entry");
        };
        let path = self.session_path(session_id)?;
        let file = open_create_exclusive(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.try_lock_exclusive()
            .with_context(|| format!("session {session_id} is already in use"))?;
        let mut writer = SessionWriter { file };
        writer.append(header)?;
        Ok(writer)
    }

    /// Open an existing session file in append mode.
    ///
    /// Takes an exclusive advisory lock on the file to prevent concurrent
    /// access. The lock is held for the lifetime of the returned writer.
    pub(crate) fn open_append(&self, session_id: &str) -> Result<SessionWriter> {
        let path = self.session_path(session_id)?;
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("session not found: {}", path.display()))?;
        file.try_lock_exclusive()
            .with_context(|| format!("session {session_id} is already in use"))?;
        Ok(SessionWriter { file })
    }

    /// Load all messages from a session file along with the UUID of the
    /// last message (for parent-chain continuity on resume).
    ///
    /// Skips non-[`Entry::Message`] lines (headers, titles, summaries,
    /// unknown). Warns and skips malformed lines.
    pub(crate) fn load_session_data(&self, session_id: &str) -> Result<SessionData> {
        let path = self.session_path(session_id)?;
        let file =
            File::open(&path).with_context(|| format!("session not found: {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut messages = Vec::new();
        let mut last_uuid = None;

        for (i, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("read error at line {}", i + 1))?;
            if line.is_empty() {
                continue;
            }
            let entry: Entry = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(e) => {
                    warn!("skipping malformed entry at line {}: {e}", i + 1);
                    continue;
                }
            };
            match entry {
                Entry::Header { version, .. } if version > CURRENT_VERSION => {
                    bail!(
                        "session format version {version} is newer than supported ({CURRENT_VERSION}); please upgrade oxide-code"
                    );
                }
                Entry::Message { uuid, message, .. } => {
                    last_uuid = Some(uuid);
                    messages.push(message);
                }
                _ => {}
            }
        }

        Ok(SessionData {
            messages,
            last_uuid,
        })
    }

    /// List sessions by reading the header (first line) and scanning the
    /// tail for the latest [`Entry::Title`] and [`Entry::Summary`] of
    /// each `.jsonl` file. Returned in reverse chronological order.
    pub(crate) fn list(&self) -> Result<Vec<SessionInfo>> {
        let entries = fs::read_dir(&self.sessions_dir)
            .with_context(|| format!("cannot read {}", self.sessions_dir.display()))?;

        let mut sessions: Vec<SessionInfo> = entries
            .filter_map(|entry| match entry {
                Ok(e) => Some(e),
                Err(e) => {
                    warn!("skipping directory entry: {e}");
                    None
                }
            })
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .filter_map(|e| match read_session_info(&e.path()) {
                Ok(info) => Some(info),
                Err(e) => {
                    warn!("skipping unreadable session file: {e}");
                    None
                }
            })
            .collect();

        sessions.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.session_id.cmp(&a.session_id))
        });
        Ok(sessions)
    }

    /// Return the most recent session ID, if any sessions exist.
    pub(crate) fn latest_session_id(&self) -> Result<Option<String>> {
        let sessions = self.list()?;
        Ok(sessions.into_iter().next().map(|s| s.session_id))
    }

    fn session_path(&self, session_id: &str) -> Result<PathBuf> {
        if session_id.contains(['/', '\\', '\0']) || session_id.contains("..") {
            bail!("invalid session ID: {session_id}");
        }
        Ok(self.sessions_dir.join(format!("{session_id}.jsonl")))
    }

    /// Create a store at an explicit directory. Used by tests to bypass
    /// XDG resolution.
    #[cfg(test)]
    pub(super) fn open_at(sessions_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&sessions_dir)?;
        Ok(Self { sessions_dir })
    }
}

// ── SessionWriter ──

/// Handle for appending entries to an open session file.
#[derive(Debug)]
pub(crate) struct SessionWriter {
    file: File,
}

impl SessionWriter {
    /// Serialize an entry as a single JSON line and flush immediately.
    pub(crate) fn append(&mut self, entry: &Entry) -> Result<()> {
        let json = serde_json::to_string(entry).context("failed to serialize entry")?;
        writeln!(self.file, "{json}").context("failed to write entry")?;
        self.file.flush().context("failed to flush entry")?;
        Ok(())
    }
}

// ── SessionData ──

/// Data loaded from a session file on resume.
#[derive(Debug)]
pub(crate) struct SessionData {
    /// Conversation messages in file order.
    pub(crate) messages: Vec<Message>,
    /// UUID of the last [`Entry::Message`] in the file, used as
    /// `parent_uuid` for the first newly-recorded message. `None` if the
    /// file contains no messages.
    pub(crate) last_uuid: Option<Uuid>,
}

// ── File Opening ──

/// Create a new file exclusively (fails if it exists). On Unix, applies
/// `0o600` permissions so session contents aren't world-readable.
fn open_create_exclusive(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

// ── Path Resolution ──

/// Resolve `$XDG_DATA_HOME/ox/sessions/`, falling back to
/// `~/.local/share/ox/sessions/`.
fn resolve_sessions_dir(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let base = xdg
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".local").join("share")))?;
    Some(base.join(DATA_DIR).join(SESSIONS_DIR))
}

// ── Session Info Extraction ──

/// Read session info from a JSONL file.
///
/// Combines a head scan (line 1 header + line 2 optional
/// [`Entry::Title`] with source [`TitleSource::FirstPrompt`]) with a
/// tail scan for the latest re-appended [`Entry::Title`] and
/// [`Entry::Summary`]. The first-prompt title lives at line 2 of the
/// file and can sit beyond [`TAIL_BUF_SIZE`] once the session grows,
/// so a pure tail scan would miss it.
///
/// When both a head and a tail title are present, the one with the
/// newer `updated_at` wins — this lets AI-generated titles (appended
/// later, landing in the tail window) supersede the first-prompt title.
fn read_session_info(path: &Path) -> Result<SessionInfo> {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = BufReader::new(&file);

    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;
    let header: Entry = serde_json::from_str(first_line.trim()).context("invalid header line")?;
    let Entry::Header {
        session_id,
        cwd,
        model,
        created_at,
        ..
    } = header
    else {
        bail!("first line is not a header");
    };

    let mut second_line = String::new();
    let head_title = if reader.read_line(&mut second_line)? > 0 {
        parse_title(second_line.trim())
    } else {
        None
    };
    drop(reader);

    // BufReader's internal buffering may have advanced the underlying file
    // position past line 2. read_tail_info seeks explicitly to the tail
    // region, so the post-drop position does not matter.
    let (tail_title, exit) = read_tail_info(&mut file)?;

    let title = [head_title, tail_title]
        .into_iter()
        .flatten()
        .max_by_key(|t| t.updated_at);

    Ok(SessionInfo {
        session_id,
        cwd,
        model,
        created_at,
        title,
        exit,
    })
}

/// Parse a JSONL line into a [`TitleInfo`], returning `None` for any
/// line that is not a well-formed [`Entry::Title`].
fn parse_title(line: &str) -> Option<TitleInfo> {
    match serde_json::from_str(line).ok()? {
        Entry::Title {
            title, updated_at, ..
        } => Some(TitleInfo { title, updated_at }),
        _ => None,
    }
}

/// Read the last [`TAIL_BUF_SIZE`] bytes of a file and scan backward
/// for the latest [`Entry::Title`] and [`Entry::Summary`] entries in
/// that window. Title may be re-appended (e.g., by an AI-title
/// feature); the latest supersedes earlier ones.
fn read_tail_info(file: &mut File) -> Result<(Option<TitleInfo>, Option<ExitInfo>)> {
    let len = file.metadata()?.len();
    let offset = len.saturating_sub(TAIL_BUF_SIZE);
    file.seek(SeekFrom::Start(offset))?;

    let mut buf = String::new();
    file.read_to_string(&mut buf)?;

    let mut title: Option<TitleInfo> = None;
    let mut exit: Option<ExitInfo> = None;
    for line in buf.lines().rev() {
        if title.is_some() && exit.is_some() {
            break;
        }
        if title.is_none()
            && let Some(t) = parse_title(line)
        {
            title = Some(t);
            continue;
        }
        if exit.is_none()
            && let Ok(Entry::Summary {
                message_count,
                updated_at,
            }) = serde_json::from_str(line)
        {
            exit = Some(ExitInfo {
                message_count,
                updated_at,
            });
        }
    }

    Ok((title, exit))
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use time::macros::datetime;

    use super::*;
    use crate::message::ContentBlock;
    use crate::session::entry::TitleSource;

    fn test_store(dir: &Path) -> SessionStore {
        SessionStore::open_at(dir.to_path_buf()).unwrap()
    }

    fn sample_header(session_id: &str) -> Entry {
        Entry::Header {
            session_id: session_id.to_owned(),
            cwd: "/tmp/project".to_owned(),
            model: "claude-opus-4-6".to_owned(),
            created_at: datetime!(2026-04-16 12:00:00 UTC),
            version: CURRENT_VERSION,
        }
    }

    fn sample_message_entry(uuid: Uuid, text: &str) -> Entry {
        Entry::Message {
            uuid,
            parent_uuid: None,
            message: Message::user(text),
            timestamp: datetime!(2026-04-16 12:00:01 UTC),
        }
    }

    fn sample_title_entry(title: &str) -> Entry {
        Entry::Title {
            title: title.to_owned(),
            source: TitleSource::FirstPrompt,
            updated_at: datetime!(2026-04-16 12:00:01 UTC),
        }
    }

    fn sample_summary_entry(message_count: u32) -> Entry {
        Entry::Summary {
            message_count,
            updated_at: datetime!(2026-04-16 12:05:00 UTC),
        }
    }

    // ── create ──

    #[test]
    fn create_writes_header_to_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let header = sample_header("test-id");

        let _writer = store.create(&header).unwrap();

        let content = fs::read_to_string(dir.path().join("test-id.jsonl")).unwrap();
        let parsed: Entry = serde_json::from_str(content.trim()).unwrap();
        assert!(
            matches!(parsed, Entry::Header { session_id, version, .. } if session_id == "test-id" && version == CURRENT_VERSION)
        );
    }

    #[test]
    fn create_rejects_non_header_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let entry = sample_message_entry(Uuid::new_v4(), "hi");

        assert!(store.create(&entry).is_err());
    }

    #[test]
    fn create_fails_when_file_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        fs::write(dir.path().join("existing.jsonl"), "{}").unwrap();
        assert!(store.create(&sample_header("existing")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn create_sets_user_only_file_permissions_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("perm-test")).unwrap();

        let meta = fs::metadata(dir.path().join("perm-test.jsonl")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    // ── open_append ──

    #[test]
    fn open_append_writes_to_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("append-test")).unwrap();
        writer
            .append(&sample_message_entry(Uuid::new_v4(), "first"))
            .unwrap();
        drop(writer); // release lock

        let mut writer = store.open_append("append-test").unwrap();
        writer
            .append(&sample_message_entry(Uuid::new_v4(), "second"))
            .unwrap();
        drop(writer);

        let data = store.load_session_data("append-test").unwrap();
        assert_eq!(data.messages.len(), 2);
        assert!(
            matches!(&data.messages[0].content[0], ContentBlock::Text { text } if text == "first")
        );
        assert!(
            matches!(&data.messages[1].content[0], ContentBlock::Text { text } if text == "second")
        );
    }

    #[test]
    fn open_append_fails_for_nonexistent_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.open_append("no-such-session").is_err());
    }

    #[test]
    fn open_append_rejects_concurrent_access() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("locked")).unwrap();

        let result = store.open_append("locked");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("already in use"), "unexpected error: {err}");
    }

    // ── load_session_data ──

    #[test]
    fn load_session_data_returns_only_messages_with_last_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("load-test")).unwrap();

        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        writer
            .append(&Entry::Message {
                uuid: u1,
                parent_uuid: None,
                message: Message::user("hello"),
                timestamp: datetime!(2026-04-16 12:00:01 UTC),
            })
            .unwrap();
        writer
            .append(&Entry::Message {
                uuid: u2,
                parent_uuid: Some(u1),
                message: Message::assistant("hi there"),
                timestamp: datetime!(2026-04-16 12:00:02 UTC),
            })
            .unwrap();
        writer.append(&sample_title_entry("greeting")).unwrap();
        writer.append(&sample_summary_entry(2)).unwrap();
        drop(writer);

        let data = store.load_session_data("load-test").unwrap();
        assert_eq!(data.messages.len(), 2);
        assert_eq!(data.last_uuid, Some(u2));
        assert_eq!(data.messages[0].role, crate::message::Role::User);
        assert!(
            matches!(&data.messages[0].content[0], ContentBlock::Text { text } if text == "hello")
        );
        assert_eq!(data.messages[1].role, crate::message::Role::Assistant);
        assert!(
            matches!(&data.messages[1].content[0], ContentBlock::Text { text } if text == "hi there")
        );
    }

    #[test]
    fn load_session_data_skips_corrupt_empty_and_unknown_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("messy.jsonl");
        fs::write(
            &path,
            indoc! {r#"
                {"type":"header","session_id":"messy","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}
                not valid json
                {"type":"new_fancy_type","data":"something"}

                {"type":"message","uuid":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","message":{"role":"user","content":[{"type":"text","text":"ok"}]},"timestamp":"2026-01-01T00:00:01Z"}
            "#},
        )
        .unwrap();
        let store = test_store(dir.path());

        let data = store.load_session_data("messy").unwrap();
        assert_eq!(data.messages.len(), 1);
        assert_eq!(
            data.last_uuid,
            Some(Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-1234567890ef").unwrap())
        );
    }

    #[test]
    fn load_session_data_nonexistent_session_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.load_session_data("nonexistent").is_err());
    }

    #[test]
    fn load_session_data_rejects_malicious_session_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.load_session_data("../etc/passwd").is_err());
        assert!(store.load_session_data(r"..\..\etc\passwd").is_err());
        assert!(store.load_session_data("session\0evil").is_err());
    }

    #[test]
    fn load_session_data_rejects_future_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.jsonl");
        let future_version = CURRENT_VERSION + 1;
        fs::write(
            &path,
            format!(
                r#"{{"type":"header","session_id":"future","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z","version":{future_version}}}
"#
            ),
        )
        .unwrap();
        let store = test_store(dir.path());
        let err = store.load_session_data("future").unwrap_err().to_string();
        assert!(err.contains("newer than supported"), "got: {err}");
    }

    // ── list ──

    #[test]
    fn list_returns_sessions_in_reverse_chronological_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());

        let mut wa = store
            .create(&Entry::Header {
                session_id: "aaa".to_owned(),
                cwd: "/a".to_owned(),
                model: "m".to_owned(),
                created_at: datetime!(2026-04-15 10:00:00 UTC),
                version: CURRENT_VERSION,
            })
            .unwrap();
        wa.append(&sample_title_entry("Older")).unwrap();
        wa.append(&sample_summary_entry(3)).unwrap();

        let mut wb = store
            .create(&Entry::Header {
                session_id: "bbb".to_owned(),
                cwd: "/b".to_owned(),
                model: "m".to_owned(),
                created_at: datetime!(2026-04-16 12:00:00 UTC),
                version: CURRENT_VERSION,
            })
            .unwrap();
        wb.append(&sample_title_entry("Newer")).unwrap();
        wb.append(&sample_summary_entry(5)).unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "bbb");
        assert_eq!(sessions[0].title.as_ref().unwrap().title, "Newer");
        assert_eq!(sessions[0].exit.as_ref().unwrap().message_count, 5);
        assert_eq!(sessions[1].session_id, "aaa");
    }

    #[test]
    fn list_picks_latest_title_when_re_appended() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("retitled")).unwrap();
        writer
            .append(&Entry::Title {
                title: "original prompt".to_owned(),
                source: TitleSource::FirstPrompt,
                updated_at: datetime!(2026-04-16 12:00:00 UTC),
            })
            .unwrap();
        writer
            .append(&Entry::Title {
                title: "AI generated".to_owned(),
                source: TitleSource::AiGenerated,
                updated_at: datetime!(2026-04-16 12:01:00 UTC),
            })
            .unwrap();

        let sessions = store.list().unwrap();
        let title = sessions[0].title.as_ref().unwrap();
        assert_eq!(title.title, "AI generated");
    }

    #[test]
    fn list_finds_first_prompt_title_beyond_tail_window() {
        // The first-prompt title is written at line 2 and never re-appended.
        // A pure tail scan misses it once the file exceeds TAIL_BUF_SIZE.
        // Verify the head scan catches it.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("long")).unwrap();
        writer
            .append(&Entry::Title {
                title: "first prompt".to_owned(),
                source: TitleSource::FirstPrompt,
                updated_at: datetime!(2026-04-16 12:00:00 UTC),
            })
            .unwrap();
        let padding = "x".repeat(200);
        for _ in 0..30 {
            writer
                .append(&sample_message_entry(Uuid::new_v4(), &padding))
                .unwrap();
        }

        let path = dir.path().join("long.jsonl");
        assert!(
            path.metadata().unwrap().len() > TAIL_BUF_SIZE,
            "test file should exceed the tail window"
        );
        let sessions = store.list().unwrap();
        let title = sessions[0].title.as_ref().unwrap();
        assert_eq!(title.title, "first prompt");
    }

    #[test]
    fn list_works_without_title_or_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("bare")).unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "bare");
        assert!(sessions[0].title.is_none());
        assert!(sessions[0].exit.is_none());
    }

    #[test]
    fn list_skips_non_session_files_and_malformed_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "not a session").unwrap();
        fs::write(
            dir.path().join("bad.jsonl"),
            r#"{"type":"message","uuid":"00000000-0000-0000-0000-000000000000","message":{"role":"user","content":[]},"timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let store = test_store(dir.path());
        assert!(store.list().unwrap().is_empty());
    }

    // ── latest_session_id ──

    #[test]
    fn latest_session_id_returns_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());

        let _wa = store
            .create(&Entry::Header {
                session_id: "old".to_owned(),
                cwd: "/".to_owned(),
                model: "m".to_owned(),
                created_at: datetime!(2026-04-15 10:00:00 UTC),
                version: CURRENT_VERSION,
            })
            .unwrap();
        let _wb = store
            .create(&Entry::Header {
                session_id: "new".to_owned(),
                cwd: "/".to_owned(),
                model: "m".to_owned(),
                created_at: datetime!(2026-04-16 12:00:00 UTC),
                version: CURRENT_VERSION,
            })
            .unwrap();

        assert_eq!(store.latest_session_id().unwrap().as_deref(), Some("new"));
    }

    #[test]
    fn latest_session_id_returns_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.latest_session_id().unwrap().is_none());
    }

    // ── append ──

    #[test]
    fn append_writes_multiple_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("multi")).unwrap();

        writer
            .append(&sample_message_entry(Uuid::new_v4(), "hello"))
            .unwrap();
        writer
            .append(&sample_message_entry(Uuid::new_v4(), "world"))
            .unwrap();

        let content = fs::read_to_string(dir.path().join("multi.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 messages
    }

    // ── resolve_sessions_dir ──

    #[test]
    fn resolve_sessions_dir_prefers_xdg() {
        let xdg = PathBuf::from("/custom/data");
        let result = resolve_sessions_dir(Some(xdg), Some(PathBuf::from("/home/u")));
        assert_eq!(result, Some(PathBuf::from("/custom/data/ox/sessions")));
    }

    #[test]
    fn resolve_sessions_dir_falls_back_to_home() {
        let result = resolve_sessions_dir(None, Some(PathBuf::from("/home/u")));
        assert_eq!(
            result,
            Some(PathBuf::from("/home/u/.local/share/ox/sessions"))
        );
    }

    #[test]
    fn resolve_sessions_dir_ignores_relative_xdg() {
        let result = resolve_sessions_dir(
            Some(PathBuf::from("relative")),
            Some(PathBuf::from("/home/u")),
        );
        assert_eq!(
            result,
            Some(PathBuf::from("/home/u/.local/share/ox/sessions"))
        );
    }

    #[test]
    fn resolve_sessions_dir_returns_none_without_home_or_xdg() {
        assert!(resolve_sessions_dir(None, None).is_none());
    }
}
