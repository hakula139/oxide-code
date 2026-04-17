use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{debug, warn};

use super::entry::{Entry, SessionInfo};
use crate::message::Message;

const DATA_DIR: &str = "ox";
const SESSIONS_DIR: &str = "sessions";

/// Tail buffer size for extracting the summary entry without reading the
/// entire file. 4 KB is generous for a single JSON line.
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
    pub(crate) fn create(&self, header: &Entry) -> Result<SessionWriter> {
        let Entry::Header { session_id, .. } = header else {
            bail!("expected Header entry");
        };
        let path = self.session_path(session_id)?;
        let file = File::create_new(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        let mut writer = SessionWriter { file };
        writer.append(header)?;
        Ok(writer)
    }

    /// Load all messages from a session file, skipping non-message entries.
    pub(crate) fn load_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let path = self.session_path(session_id)?;
        let file =
            File::open(&path).with_context(|| format!("session not found: {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut messages = Vec::new();

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
            if let Entry::Message { message, .. } = entry {
                messages.push(message);
            }
        }

        Ok(messages)
    }

    /// List sessions by reading the header (first line) and summary (tail)
    /// of each `.jsonl` file. Returned in reverse chronological order.
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
        if session_id.contains(['/', '\\']) || session_id.contains("..") {
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

/// Read session info from a JSONL file by parsing the first line (header)
/// and scanning the tail for a summary entry.
fn read_session_info(path: &Path) -> Result<SessionInfo> {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;

    let mut first_line = String::new();
    BufReader::new(&file).read_line(&mut first_line)?;
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

    // Scan the tail for a summary entry. The explicit seek in
    // `read_tail_summary` is required because `BufReader` above may have
    // read ahead past the first line, advancing the file position.
    let summary = read_tail_summary(&mut file)?;

    Ok(SessionInfo {
        session_id,
        cwd,
        model,
        created_at,
        title: summary.as_ref().map(|(t, _, _)| t.clone()),
        updated_at: summary.as_ref().map(|(_, u, _)| *u),
        message_count: summary.as_ref().map(|(_, _, c)| *c),
    })
}

/// Read the last `TAIL_BUF_SIZE` bytes of a file and scan for the final
/// summary entry.
fn read_tail_summary(file: &mut File) -> Result<Option<(String, time::OffsetDateTime, u32)>> {
    let len = file.metadata()?.len();
    let offset = len.saturating_sub(TAIL_BUF_SIZE);
    file.seek(SeekFrom::Start(offset))?;

    let mut buf = String::new();
    file.read_to_string(&mut buf)?;

    for line in buf.lines().rev() {
        if let Ok(Entry::Summary {
            title,
            updated_at,
            message_count,
        }) = serde_json::from_str(line)
        {
            return Ok(Some((title, updated_at, message_count)));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use time::macros::datetime;

    use super::*;
    use crate::message::ContentBlock;

    fn test_store(dir: &Path) -> SessionStore {
        SessionStore {
            sessions_dir: dir.to_path_buf(),
        }
    }

    fn sample_header(session_id: &str) -> Entry {
        Entry::Header {
            session_id: session_id.to_owned(),
            parent_id: None,
            cwd: "/tmp/project".to_owned(),
            model: "claude-opus-4-6".to_owned(),
            created_at: datetime!(2026-04-16 12:00:00 UTC),
        }
    }

    fn sample_message_entry(text: &str) -> Entry {
        Entry::Message {
            message: Message::user(text),
            timestamp: datetime!(2026-04-16 12:00:01 UTC),
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
        assert!(matches!(parsed, Entry::Header { session_id, .. } if session_id == "test-id"));
    }

    #[test]
    fn create_rejects_non_header_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let entry = sample_message_entry("hi");

        assert!(store.create(&entry).is_err());
    }

    #[test]
    fn create_fails_when_file_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        fs::write(dir.path().join("existing.jsonl"), "{}").unwrap();
        let header = Entry::Header {
            session_id: "existing".to_owned(),
            parent_id: None,
            cwd: "/".to_owned(),
            model: "m".to_owned(),
            created_at: datetime!(2026-01-01 0:00 UTC),
        };
        assert!(store.create(&header).is_err());
    }

    // ── load_messages ──

    #[test]
    fn load_messages_returns_only_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("load-test")).unwrap();
        writer.append(&sample_message_entry("hello")).unwrap();
        writer
            .append(&Entry::Message {
                message: Message::assistant("hi there"),
                timestamp: datetime!(2026-04-16 12:00:02 UTC),
            })
            .unwrap();
        writer
            .append(&Entry::Summary {
                title: "Test".to_owned(),
                updated_at: datetime!(2026-04-16 12:00:02 UTC),
                message_count: 2,
            })
            .unwrap();
        drop(writer);

        let messages = store.load_messages("load-test").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, crate::message::Role::User);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "hello"));
        assert_eq!(messages[1].role, crate::message::Role::Assistant);
        assert!(
            matches!(&messages[1].content[0], ContentBlock::Text { text } if text == "hi there")
        );
    }

    #[test]
    fn load_messages_skips_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.jsonl");
        fs::write(
            &path,
            indoc! {r#"
                {"type":"header","session_id":"c","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z"}
                not valid json
                {"type":"message","message":{"role":"user","content":[{"type":"text","text":"ok"}]},"timestamp":"2026-01-01T00:00:01Z"}
            "#},
        )
        .unwrap();
        let store = test_store(dir.path());

        let messages = store.load_messages("corrupt").unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn load_messages_nonexistent_session_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.load_messages("nonexistent").is_err());
    }

    #[test]
    fn load_messages_skips_empty_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blanks.jsonl");
        fs::write(
            &path,
            indoc! {r#"
                {"type":"header","session_id":"blanks","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z"}

                {"type":"message","message":{"role":"user","content":[{"type":"text","text":"ok"}]},"timestamp":"2026-01-01T00:00:01Z"}

            "#},
        )
        .unwrap();
        let store = test_store(dir.path());

        let messages = store.load_messages("blanks").unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn load_messages_skips_unknown_entry_types() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.jsonl");
        fs::write(
            &path,
            indoc! {r#"
                {"type":"header","session_id":"future","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z"}
                {"type":"new_fancy_type","data":"something"}
                {"type":"message","message":{"role":"user","content":[{"type":"text","text":"ok"}]},"timestamp":"2026-01-01T00:00:01Z"}
            "#},
        )
        .unwrap();
        let store = test_store(dir.path());

        let messages = store.load_messages("future").unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn load_messages_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.load_messages("../etc/passwd").is_err());
    }

    #[test]
    fn load_messages_rejects_backslash_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.load_messages(r"..\..\etc\passwd").is_err());
    }

    // ── list ──

    #[test]
    fn list_returns_sessions_in_reverse_chronological_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());

        // Session A: older.
        let mut wa = store
            .create(&Entry::Header {
                session_id: "aaa".to_owned(),
                parent_id: None,
                cwd: "/a".to_owned(),
                model: "m".to_owned(),
                created_at: datetime!(2026-04-15 10:00:00 UTC),
            })
            .unwrap();
        wa.append(&Entry::Summary {
            title: "Older".to_owned(),
            updated_at: datetime!(2026-04-15 10:05:00 UTC),
            message_count: 3,
        })
        .unwrap();

        // Session B: newer.
        let mut wb = store
            .create(&Entry::Header {
                session_id: "bbb".to_owned(),
                parent_id: None,
                cwd: "/b".to_owned(),
                model: "m".to_owned(),
                created_at: datetime!(2026-04-16 12:00:00 UTC),
            })
            .unwrap();
        wb.append(&Entry::Summary {
            title: "Newer".to_owned(),
            updated_at: datetime!(2026-04-16 12:05:00 UTC),
            message_count: 5,
        })
        .unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "bbb");
        assert_eq!(sessions[0].title.as_deref(), Some("Newer"));
        assert_eq!(sessions[0].message_count, Some(5));
        assert_eq!(sessions[1].session_id, "aaa");
    }

    #[test]
    fn list_works_without_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("no-summary")).unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "no-summary");
        assert!(sessions[0].title.is_none());
        assert!(sessions[0].message_count.is_none());
    }

    #[test]
    fn list_empty_directory_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_ignores_non_jsonl_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "not a session").unwrap();
        let store = test_store(dir.path());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_skips_jsonl_with_non_header_first_line() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("bad.jsonl"),
            r#"{"type":"message","message":{"role":"user","content":[]},"timestamp":"2026-01-01T00:00:00Z"}"#,
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
                parent_id: None,
                cwd: "/".to_owned(),
                model: "m".to_owned(),
                created_at: datetime!(2026-04-15 10:00:00 UTC),
            })
            .unwrap();
        let _wb = store
            .create(&Entry::Header {
                session_id: "new".to_owned(),
                parent_id: None,
                cwd: "/".to_owned(),
                model: "m".to_owned(),
                created_at: datetime!(2026-04-16 12:00:00 UTC),
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
        let header = sample_header("multi");
        let mut writer = store.create(&header).unwrap();

        writer.append(&sample_message_entry("hello")).unwrap();
        writer.append(&sample_message_entry("world")).unwrap();

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
