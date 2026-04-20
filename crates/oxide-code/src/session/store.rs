use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use time::OffsetDateTime;
use tracing::{debug, warn};
use uuid::Uuid;

use super::entry::{CURRENT_VERSION, Entry, ExitInfo, SessionInfo, TitleInfo};
use super::path::{UNKNOWN_PROJECT_DIR, sanitize_cwd};
use crate::message::Message;
use crate::util::path::xdg_dir;

const DATA_DIR: &str = "ox";
const SESSIONS_DIR: &str = "sessions";

/// Tail buffer size for extracting the latest [`Entry::Title`] /
/// [`Entry::Summary`] without reading the entire file. 4 KB is generous
/// for a single JSON line.
const TAIL_BUF_SIZE: u64 = 4096;

// ── SessionStore ──

/// Low-level session file operations.
///
/// Sessions are stored under `$XDG_DATA_HOME/ox/sessions/{project}/`,
/// where `{project}` is a filesystem-safe subdirectory name derived
/// from the working directory at session creation time (see
/// [`super::path::sanitize_cwd`]). The store exposes one "home"
/// project (the current CWD) that listing, creation, and default
/// resume operate on, and provides explicit cross-project variants
/// for `--all` callers.
#[derive(Clone)]
pub(crate) struct SessionStore {
    /// Root directory holding every project subdirectory.
    sessions_dir: PathBuf,
    /// Subdirectory for the current working directory.
    project_dir: PathBuf,
}

impl SessionStore {
    /// Create a store rooted at the XDG data directory, scoped to the
    /// current working directory. Creates both the root and the
    /// project subdirectory if needed.
    pub(crate) fn open() -> Result<Self> {
        let sessions_dir = xdg_dir(
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            dirs::home_dir(),
            Path::new(".local/share"),
            &Path::new(DATA_DIR).join(SESSIONS_DIR),
        )
        .context("cannot determine session storage directory")?;

        create_private_dir_all(&sessions_dir)?;

        let project_name = match std::env::current_dir() {
            Ok(cwd) => sanitize_cwd(&cwd),
            Err(e) => {
                warn!("cannot resolve current directory for project scoping: {e}");
                UNKNOWN_PROJECT_DIR.to_owned()
            }
        };
        let project_dir = sessions_dir.join(&project_name);
        create_private_dir_all(&project_dir)?;

        debug!(
            "session store at {} (project: {project_name})",
            sessions_dir.display()
        );
        Ok(Self {
            sessions_dir,
            project_dir,
        })
    }

    /// Create a new session file and write the header entry.
    ///
    /// On Unix, the file is created with mode `0o600` so session contents
    /// (verbatim tool output, assistant responses) are not world-readable.
    /// Creation is atomic via `O_CREAT | O_EXCL`, so the rare case of two
    /// processes minting the same session ID fails cleanly.
    ///
    /// Filenames are `{created_at_epoch}-{session_id}.jsonl`. The epoch
    /// prefix makes `ls` on a project subdirectory return sessions in
    /// chronological order, which is convenient when inspecting the
    /// store outside of `ox --list`.
    ///
    /// Session files carry no file-level lock: concurrent resumes are
    /// explicitly allowed and form forks in the recorded UUID chain.
    /// See [`Self::load_session_data`] for the fork-aware loader.
    #[expect(
        clippy::unused_async,
        reason = "async preserves the call-site shape for a planned tokio::fs migration; synchronous fs calls are short-lived (open + header write)"
    )]
    pub(crate) async fn create(&self, header: &Entry) -> Result<SessionWriter> {
        let Entry::Header {
            session_id,
            created_at,
            ..
        } = header
        else {
            bail!("expected Header entry");
        };
        validate_session_id(session_id)?;

        let path = self
            .project_dir
            .join(session_filename(session_id, *created_at));
        let file = open_create_exclusive(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;

        let mut writer = SessionWriter { file };
        writer.append(header)?;
        Ok(writer)
    }

    /// Open an existing session file in append mode.
    ///
    /// Searches every project subdirectory, not just the current one,
    /// so `ox -c <id>` resumes a session regardless of which project
    /// it originally belonged to.
    ///
    /// No file-level lock is acquired: two processes resuming the same
    /// session both append to the same file, forming a fork in the
    /// UUID chain. [`Self::load_session_data`] reconstructs the newest
    /// non-sidechain branch on the next resume. Individual writes rely
    /// on POSIX `O_APPEND` for line positioning; writes larger than
    /// `PIPE_BUF` (typically 4 KiB) may interleave, but the loader
    /// warn-skips any malformed UTF-8 / JSON fragments that result.
    #[expect(
        clippy::unused_async,
        reason = "async preserves the call-site shape for a planned tokio::fs migration; synchronous fs calls are short-lived"
    )]
    pub(crate) async fn open_append(&self, session_id: &str) -> Result<SessionWriter> {
        let path = self.find_session_path(session_id)?;
        open_append_at(&path)
    }

    /// Load a session's message chain and return the UUID of its tip
    /// (for parent-chain continuity on resume). Like
    /// [`Self::open_append`], searches every project subdirectory.
    ///
    /// Walks the recorded UUID DAG rather than the raw file order.
    /// Two processes resuming the same session concurrently both
    /// append with `parent_uuid` pointing at what each saw as the
    /// tip, forming a fork. The loader:
    ///
    /// 1. Builds a map of every valid `Entry::Message` by UUID.
    /// 2. Computes the set of leaves — UUIDs not referenced as
    ///    `parent_uuid` by any other message.
    /// 3. Picks the leaf with the newest timestamp as the tip (ties
    ///    break by UUID byte order for determinism).
    /// 4. Walks back via `parent_uuid` to the root, reverses → linear
    ///    chain from root to tip.
    ///
    /// The "newest-leaf wins" policy matches claude-code's
    /// `loadMessagesFromJsonlPath` and means the losing branch on a
    /// concurrent-resume fork stays in the file but is invisible to
    /// later resumes. The trade-off is documented in
    /// [`Self::open_append`].
    ///
    /// Non-`Message` entries (headers, titles, summaries, unknown)
    /// are skipped. Malformed lines — including interleaved-write
    /// fragments from concurrent large writes, truncated UTF-8 from
    /// a crash during `writeln!`, or unknown future entry types —
    /// are warn-skipped and do not fail the load.
    pub(crate) fn load_session_data(&self, session_id: &str) -> Result<SessionData> {
        let path = self.find_session_path(session_id)?;
        load_session_data_from_path(&path)
    }

    /// List sessions for the current project, sorted by file mtime
    /// (most recently active first) so resumed sessions bubble to the
    /// top.
    pub(crate) fn list(&self) -> Result<Vec<SessionInfo>> {
        let mut sessions = read_sessions_in_dir(&self.project_dir)?;
        sort_sessions_recent_first(&mut sessions);
        Ok(sessions)
    }

    /// List sessions across every project subdirectory. Used by the
    /// `--all` flag for cross-project views.
    pub(crate) fn list_all(&self) -> Result<Vec<SessionInfo>> {
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.sessions_dir)
            .with_context(|| format!("cannot read {}", self.sessions_dir.display()))?
        {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!("skipping directory entry: {e}");
                    continue;
                }
            };
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                match read_sessions_in_dir(&entry.path()) {
                    Ok(mut s) => sessions.append(&mut s),
                    Err(e) => warn!("skipping project dir {}: {e}", entry.path().display()),
                }
            }
        }
        sort_sessions_recent_first(&mut sessions);
        Ok(sessions)
    }

    /// Locate an existing session file by session ID. Filenames are
    /// prefixed with the creation epoch, so we match by the
    /// `-{session_id}.jsonl` suffix rather than a direct path build.
    ///
    /// Checks the current project first (the fast path), then falls
    /// back to walking sibling project subdirectories so cross-project
    /// resume by session ID also works.
    fn find_session_path(&self, session_id: &str) -> Result<PathBuf> {
        validate_session_id(session_id)?;
        if let Some(path) = find_session_in(&self.project_dir, session_id)? {
            return Ok(path);
        }
        for entry in fs::read_dir(&self.sessions_dir)
            .with_context(|| format!("cannot read {}", self.sessions_dir.display()))?
        {
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if let Some(path) = find_session_in(&entry.path(), session_id)? {
                return Ok(path);
            }
        }
        bail!("session not found: {session_id}")
    }

    /// Create a store at an explicit directory. Used by tests to bypass
    /// XDG resolution. `project_name` selects the subdirectory inside
    /// `sessions_dir` that acts as the current project.
    #[cfg(test)]
    pub(super) fn open_at(sessions_dir: PathBuf, project_name: &str) -> Result<Self> {
        fs::create_dir_all(&sessions_dir)?;
        let project_dir = sessions_dir.join(project_name);
        fs::create_dir_all(&project_dir)?;
        Ok(Self {
            sessions_dir,
            project_dir,
        })
    }
}

// ── Path-keyed primitives ──

/// Open an existing session file in append mode by path. Underlies
/// [`SessionStore::open_append`] (which resolves the path first) and
/// `SessionManager::resume_from_path` (which bypasses the store entirely
/// for sessions living outside the XDG project subdirectories).
pub(crate) fn open_append_at(path: &Path) -> Result<SessionWriter> {
    let file = OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("session not found: {}", path.display()))?;
    Ok(SessionWriter { file })
}

/// Load session data from an explicit path. See
/// [`SessionStore::load_session_data`] for the description of DAG-based
/// chain resolution and fault-tolerant parsing — this is the underlying
/// primitive, used both by the store lookup and by the external-path
/// resume flow.
pub(crate) fn load_session_data_from_path(path: &Path) -> Result<SessionData> {
    let file =
        File::open(path).with_context(|| format!("session not found: {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut nodes: HashMap<Uuid, ChainNode> = HashMap::new();
    let mut referenced: HashSet<Uuid> = HashSet::new();
    let mut latest_title: Option<TitleInfo> = None;
    let mut buf = Vec::new();
    let mut line_no: u32 = 0;

    // Read byte-by-line instead of `BufReader::lines()`. A crash during
    // `writeln!` can leave the last record truncated — mid-byte of a
    // multibyte UTF-8 codepoint in the worst case — and `lines()`
    // propagates `InvalidData` there, failing the entire resume. Doing the
    // decode ourselves lets us warn-skip bad lines with the same
    // resilience we already apply to malformed JSON, and tolerate a
    // missing trailing newline.
    loop {
        buf.clear();
        let read = reader
            .read_until(b'\n', &mut buf)
            .with_context(|| format!("read error at line {}", line_no + 1))?;
        if read == 0 {
            break;
        }
        line_no += 1;
        let without_newline = buf.strip_suffix(b"\n").unwrap_or(&buf);
        if without_newline.is_empty() {
            continue;
        }
        let line = match std::str::from_utf8(without_newline) {
            Ok(s) => s,
            Err(e) => {
                warn!("skipping non-utf8 entry at line {line_no}: {e}");
                continue;
            }
        };
        let entry: Entry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(e) => {
                warn!("skipping malformed entry at line {line_no}: {e}");
                continue;
            }
        };
        match entry {
            Entry::Header { version, .. } if version > CURRENT_VERSION => {
                bail!(
                    "session format version {version} is newer than supported ({CURRENT_VERSION}); please upgrade oxide-code"
                );
            }
            Entry::Message {
                uuid,
                parent_uuid,
                message,
                timestamp,
            } => {
                if let Some(p) = parent_uuid {
                    referenced.insert(p);
                }
                // Last-append-wins on duplicate UUIDs — a retry or
                // partial-write recovery could replay an entry, and we
                // prefer the most recent representation.
                nodes.insert(
                    uuid,
                    ChainNode {
                        parent_uuid,
                        message,
                        timestamp,
                    },
                );
            }
            // Track the newest title so the TUI's status bar and any
            // future surface can display it on resume without a second
            // pass over the file. AI-generated titles appended later beat
            // the first-prompt title by `updated_at`.
            Entry::Title {
                title, updated_at, ..
            } if latest_title
                .as_ref()
                .is_none_or(|cur| updated_at > cur.updated_at) =>
            {
                latest_title = Some(TitleInfo { title, updated_at });
            }
            _ => {}
        }
    }

    let (messages, last_uuid) = resolve_chain(nodes, &referenced);
    Ok(SessionData {
        messages,
        last_uuid,
        title: latest_title,
    })
}

/// Read just the `session_id` from a session file's header (line 1).
/// Used by external-path resume so callers can key the resumed
/// [`SessionManager`] on the file's declared identity rather than its path.
pub(crate) fn read_session_id_from_path(path: &Path) -> Result<String> {
    let file =
        File::open(path).with_context(|| format!("session not found: {}", path.display()))?;
    let mut first_line = String::new();
    BufReader::new(file).read_line(&mut first_line)?;
    let Entry::Header { session_id, .. } = serde_json::from_str(first_line.trim())
        .with_context(|| format!("first line of {} is not a valid header", path.display()))?
    else {
        bail!("{} does not begin with a header", path.display());
    };
    Ok(session_id)
}

/// Create `path` (and parents) with owner-only (`0o700`) perms on Unix.
///
/// Session files are already `0o600`, but lax parent-dir perms would leak
/// session IDs, project names, and mtimes via `ls`. Passing the mode to
/// `DirBuilder` applies it in the create syscall, closing the TOCTOU gap
/// a post-create `chmod` would leave. Already-existing directories then
/// get a best-effort tighten (no-op on POSIX-less mounts).
fn create_private_dir_all(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
            debug!("failed to tighten {} to 0o700: {e}", path.display());
        }
    }
    Ok(())
}

/// Restrict session IDs to ASCII alphanumerics + `-_`, max 64 chars.
/// Covers our 36-char UUID v4 while rejecting path separators, NUL,
/// control chars, and Windows-reserved chars.
fn validate_session_id(session_id: &str) -> Result<()> {
    let ok = !session_id.is_empty()
        && session_id.len() <= 64
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        bail!("invalid session ID: {session_id}");
    }
    Ok(())
}

/// Format a new session filename as `{epoch}-{session_id}.jsonl`. The
/// epoch prefix gives chronological directory-order listings and stays
/// fixed-width (10 ASCII digits) through the year 2286.
fn session_filename(session_id: &str, created_at: OffsetDateTime) -> String {
    format!("{}-{session_id}.jsonl", created_at.unix_timestamp())
}

/// Return the path of the first `.jsonl` file in `dir` whose name ends
/// with `-{session_id}.jsonl`. `None` means "no match in this dir", not
/// a hard error — the caller can continue searching other locations.
fn find_session_in(dir: &Path, session_id: &str) -> Result<Option<PathBuf>> {
    let suffix = format!("-{session_id}.jsonl");
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("cannot read {}", dir.display())));
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.ends_with(&suffix) {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

/// Read every `.jsonl` file in `dir` and return the successfully
/// parsed [`SessionInfo`] entries, warning and skipping on errors.
fn read_sessions_in_dir(dir: &Path) -> Result<Vec<SessionInfo>> {
    let entries = fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?;
    Ok(entries
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
        .collect())
}

fn sort_sessions_recent_first(sessions: &mut [SessionInfo]) {
    // Matches the `sort_by_key(|e| Reverse(...))` idiom used by the
    // mtime sort in `tool/{glob,grep}.rs`. `session_id` breaks ties
    // in reverse-alphabetical order so the resulting order is
    // stable across `list` calls even when two sessions share a
    // single-second mtime.
    sessions.sort_by_key(|s| {
        (
            std::cmp::Reverse(s.last_active_at),
            std::cmp::Reverse(s.session_id.clone()),
        )
    });
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
    /// Latest [`Entry::Title`] in the file (max `updated_at`). `None` when
    /// no title was ever recorded (e.g., the session exited before the
    /// first user prompt).
    pub(crate) title: Option<TitleInfo>,
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

/// Internal node used by [`resolve_chain`] to walk the UUID DAG.
struct ChainNode {
    parent_uuid: Option<Uuid>,
    message: Message,
    timestamp: OffsetDateTime,
}

/// Turn a UUID-indexed message map into a linear chain ending at the
/// newest leaf. `referenced` is the set of UUIDs mentioned by some
/// message as its `parent_uuid`; the leaves are `nodes.keys() - referenced`.
///
/// Returns `(chain, Some(tip))` on success, or `(vec![], None)` when
/// the file contains no messages. A cycle (e.g., from on-disk
/// corruption where a UUID points at one of its descendants) is
/// treated as a terminated chain: the walker detects the repeat and
/// stops, preserving the prefix it has already collected rather than
/// looping forever. A `parent_uuid` missing from `nodes` (orphan) is
/// also treated as a chain terminator.
fn resolve_chain(
    mut nodes: HashMap<Uuid, ChainNode>,
    referenced: &HashSet<Uuid>,
) -> (Vec<Message>, Option<Uuid>) {
    let tip = nodes
        .iter()
        .filter(|(uuid, _)| !referenced.contains(uuid))
        .max_by(|(a_uuid, a), (b_uuid, b)| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a_uuid.cmp(b_uuid))
        })
        .map(|(uuid, _)| *uuid);
    let Some(tip_uuid) = tip else {
        return (Vec::new(), None);
    };

    let mut chain: Vec<Message> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut cursor = Some(tip_uuid);
    while let Some(uuid) = cursor {
        if !seen.insert(uuid) {
            // Cycle or repeated visit — bail out with what we have.
            warn!(
                "session chain walk hit a cycle at {uuid}; truncating to the prefix collected so far"
            );
            break;
        }
        let Some(node) = nodes.remove(&uuid) else {
            // Missing ancestor — chain reaches an orphan. Stop here;
            // everything we collected so far stays in `chain`.
            break;
        };
        chain.push(node.message);
        cursor = node.parent_uuid;
    }
    chain.reverse();
    (chain, Some(tip_uuid))
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
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot stat {}", path.display()))?;
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

    let last_active_at = metadata
        .modified()
        .ok()
        .map_or(created_at, OffsetDateTime::from);

    Ok(SessionInfo {
        session_id,
        cwd,
        model,
        created_at,
        last_active_at,
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
pub(super) const TEST_PROJECT: &str = "test-project";

/// Open a [`SessionStore`] rooted at `dir` under [`TEST_PROJECT`].
/// Shared between the `session::store` and `session::manager` test
/// modules so both exercise the same project-scoping path.
#[cfg(test)]
pub(super) fn test_store(dir: &Path) -> SessionStore {
    SessionStore::open_at(dir.to_path_buf(), TEST_PROJECT).unwrap()
}

/// Resolve a session file inside [`TEST_PROJECT`] by its session ID.
/// Filenames are prefixed with the creation epoch (see
/// [`session_filename`]), so tests cannot build the path directly
/// from a session ID alone; this helper scans the project dir for
/// the matching suffix. Panics on miss.
#[cfg(test)]
pub(super) fn test_session_file(dir: &Path, session_id: &str) -> PathBuf {
    let project_dir = test_project_dir(dir);
    find_session_in(&project_dir, session_id)
        .unwrap()
        .unwrap_or_else(|| panic!("no session file for id {session_id} in {project_dir:?}"))
}

/// Project subdirectory used by [`test_store`]. Exposed so tests
/// can assert on the directory's contents (e.g., that lazy file
/// creation has not materialized anything yet) without panicking
/// on a missing session like [`test_session_file`].
#[cfg(test)]
pub(super) fn test_project_dir(dir: &Path) -> PathBuf {
    dir.join(TEST_PROJECT)
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use time::macros::datetime;

    use super::super::entry::TitleSource;
    use super::*;
    use crate::message::ContentBlock;

    /// Direct path inside [`TEST_PROJECT`] for a given filename. Used
    /// by tests that hand-roll a file before opening the store, or
    /// that seed invalid content the production loader should skip.
    fn test_project_path(dir: &Path, filename: &str) -> PathBuf {
        dir.join(TEST_PROJECT).join(filename)
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
        sample_message_at(uuid, None, datetime!(2026-04-16 12:00:01 UTC), text)
    }

    /// Variant of [`sample_message_entry`] that accepts a `parent_uuid`
    /// and explicit timestamp so tests can build multi-message chains
    /// (and forks) exercised by the DAG-walking loader.
    fn sample_message_at(
        uuid: Uuid,
        parent_uuid: Option<Uuid>,
        timestamp: OffsetDateTime,
        text: &str,
    ) -> Entry {
        Entry::Message {
            uuid,
            parent_uuid,
            message: Message::user(text),
            timestamp,
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

    #[tokio::test]
    async fn create_writes_header_to_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let header = sample_header("test-id");

        let _writer = store.create(&header).await.unwrap();

        let content = fs::read_to_string(test_session_file(dir.path(), "test-id")).unwrap();
        let parsed: Entry = serde_json::from_str(content.trim()).unwrap();
        assert!(
            matches!(parsed, Entry::Header { session_id, version, .. } if session_id == "test-id" && version == CURRENT_VERSION)
        );
    }

    #[tokio::test]
    async fn create_rejects_non_header_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let entry = sample_message_entry(Uuid::new_v4(), "hi");

        assert!(store.create(&entry).await.is_err());
    }

    #[tokio::test]
    async fn create_fails_when_file_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let header = sample_header("existing");
        let Entry::Header { created_at, .. } = header else {
            unreachable!()
        };
        let taken = test_project_path(dir.path(), &session_filename("existing", created_at));
        fs::write(taken, "{}").unwrap();
        assert!(store.create(&header).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_sets_user_only_file_permissions_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("perm-test")).await.unwrap();

        let meta = fs::metadata(test_session_file(dir.path(), "perm-test")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    // ── open_append ──

    #[tokio::test]
    async fn open_append_writes_to_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("append-test")).await.unwrap();
        let u1 = Uuid::new_v4();
        writer
            .append(&sample_message_at(
                u1,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "first",
            ))
            .unwrap();
        drop(writer);

        let mut writer = store.open_append("append-test").await.unwrap();
        writer
            .append(&sample_message_at(
                Uuid::new_v4(),
                Some(u1),
                datetime!(2026-04-16 12:00:02 UTC),
                "second",
            ))
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

    #[tokio::test]
    async fn open_append_fails_for_nonexistent_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.open_append("no-such-session").await.is_err());
    }

    #[tokio::test]
    async fn open_append_allows_concurrent_resumes_without_blocking() {
        // Two processes resuming the same session is a first-class
        // case: both acquire append handles immediately, and the
        // resulting UUID fork is resolved at load time (see
        // `load_session_data_picks_newest_leaf_on_fork`).
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer_a = store.create(&sample_header("concurrent")).await.unwrap();

        let start = std::time::Instant::now();
        let writer_b = store.open_append("concurrent").await;
        let elapsed = start.elapsed();

        assert!(
            writer_b.is_ok(),
            "second resume should succeed immediately, got {writer_b:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "open_append should not block on a concurrent writer: {elapsed:?}"
        );
    }

    // ── load_session_data ──

    #[tokio::test]
    async fn load_session_data_returns_only_messages_with_last_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("load-test")).await.unwrap();

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
        let store = test_store(dir.path());
        let created_at = datetime!(2026-01-01 00:00:00 UTC);
        let path = test_project_path(dir.path(), &session_filename("messy", created_at));
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
        assert!(store.load_session_data("").is_err());
        assert!(store.load_session_data("../etc/passwd").is_err());
        assert!(store.load_session_data(r"..\..\etc\passwd").is_err());
        assert!(store.load_session_data("session\0evil").is_err());
        // Windows-reserved and control chars are also rejected.
        assert!(store.load_session_data("foo:bar").is_err());
        assert!(store.load_session_data("foo|bar").is_err());
        assert!(store.load_session_data("foo*").is_err());
        // Bounded length: a very long ID is rejected.
        assert!(store.load_session_data(&"a".repeat(65)).is_err());
    }

    #[tokio::test]
    async fn load_session_data_picks_newest_leaf_on_fork() {
        // Two processes resumed and each appended — forming a fork
        // in the UUID DAG. `load_session_data` must pick the newest
        // leaf as the tip and walk back to the shared ancestor,
        // matching claude-code's `loadMessagesFromJsonlPath`.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("fork")).await.unwrap();

        let root = Uuid::new_v4();
        writer
            .append(&sample_message_at(
                root,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "shared root",
            ))
            .unwrap();

        // Branch A: recorded first, at t+2s.
        let branch_a = Uuid::new_v4();
        writer
            .append(&sample_message_at(
                branch_a,
                Some(root),
                datetime!(2026-04-16 12:00:02 UTC),
                "branch A (older leaf)",
            ))
            .unwrap();

        // Branch B: recorded later, at t+3s. Should win as the tip.
        let branch_b = Uuid::new_v4();
        writer
            .append(&sample_message_at(
                branch_b,
                Some(root),
                datetime!(2026-04-16 12:00:03 UTC),
                "branch B (newer leaf)",
            ))
            .unwrap();
        drop(writer);

        let data = store.load_session_data("fork").unwrap();
        assert_eq!(
            data.last_uuid,
            Some(branch_b),
            "tip should be the newer leaf"
        );
        assert_eq!(data.messages.len(), 2);
        assert!(
            matches!(&data.messages[0].content[0], ContentBlock::Text { text } if text == "shared root"),
            "chain should start at the shared ancestor"
        );
        assert!(
            matches!(&data.messages[1].content[0], ContentBlock::Text { text } if text == "branch B (newer leaf)"),
            "chain should end at the newest leaf"
        );
    }

    #[tokio::test]
    async fn load_session_data_terminates_at_orphan_parent_reference() {
        // parent_uuid points at a UUID not present in the file — e.g.,
        // because the parent line was lost to an interleaved write or
        // a truncation. The walker should stop at the orphan instead
        // of looping or erroring; everything collected so far is
        // returned.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("orphan")).await.unwrap();

        let only = Uuid::new_v4();
        writer
            .append(&sample_message_at(
                only,
                Some(Uuid::new_v4()), // points at a missing ancestor
                datetime!(2026-04-16 12:00:01 UTC),
                "orphan tip",
            ))
            .unwrap();
        drop(writer);

        let data = store.load_session_data("orphan").unwrap();
        assert_eq!(data.last_uuid, Some(only));
        assert_eq!(data.messages.len(), 1);
        assert!(
            matches!(&data.messages[0].content[0], ContentBlock::Text { text } if text == "orphan tip")
        );
    }

    #[tokio::test]
    async fn load_session_data_breaks_chain_walk_on_cycle() {
        // Corrupted file where two messages point at each other.
        // Defensive: the walker must terminate rather than looping.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("cycle")).await.unwrap();

        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        writer
            .append(&sample_message_at(
                a,
                Some(b), // a points at b — part of the cycle
                datetime!(2026-04-16 12:00:01 UTC),
                "A",
            ))
            .unwrap();
        writer
            .append(&sample_message_at(
                b,
                Some(a), // b points back at a — completes the cycle
                datetime!(2026-04-16 12:00:02 UTC),
                "B",
            ))
            .unwrap();
        drop(writer);

        // Every message is referenced by another, so there's no leaf.
        // `resolve_chain` returns an empty chain; we only assert the
        // load doesn't hang and succeeds.
        let data = store.load_session_data("cycle").unwrap();
        assert!(
            data.messages.is_empty(),
            "cycle with no leaf should yield an empty chain, got: {:?}",
            data.messages,
        );
    }

    #[tokio::test]
    async fn load_session_data_prefers_later_duplicate_uuid() {
        // A replayed append (e.g., after a partial-write retry) can
        // produce two records with the same UUID. Keep the later one
        // — newest-wins matches the API-replay semantics the UUID is
        // supposed to dedupe on.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("dup")).await.unwrap();

        let u = Uuid::new_v4();
        writer
            .append(&sample_message_at(
                u,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "first copy",
            ))
            .unwrap();
        writer
            .append(&sample_message_at(
                u,
                None,
                datetime!(2026-04-16 12:00:05 UTC),
                "second copy",
            ))
            .unwrap();
        drop(writer);

        let data = store.load_session_data("dup").unwrap();
        assert_eq!(data.last_uuid, Some(u));
        assert_eq!(data.messages.len(), 1);
        assert!(
            matches!(&data.messages[0].content[0], ContentBlock::Text { text } if text == "second copy"),
            "latest duplicate should win"
        );
    }

    #[test]
    fn load_session_data_recovers_from_truncated_utf8_at_eof() {
        // Simulate a SIGKILL mid-`writeln!` that left the final record
        // broken in the middle of a multibyte UTF-8 sequence. The
        // pre-fix code used `BufReader::lines()`, which propagates an
        // `InvalidData` error there and fails the whole resume.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let created_at = datetime!(2026-01-01 00:00:00 UTC);
        let path = test_project_path(dir.path(), &session_filename("chopped", created_at));

        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(
            br#"{"type":"header","session_id":"chopped","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}
"#,
        );
        bytes.extend_from_slice(
            br#"{"type":"message","uuid":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","message":{"role":"user","content":[{"type":"text","text":"ok"}]},"timestamp":"2026-01-01T00:00:01Z"}
"#,
        );
        // Start writing the next message, then crash inside the emoji.
        bytes.extend_from_slice(br#"{"type":"message","uuid":"b2c3d4e5-f6a7-8901-bcde-234567890abc","message":{"role":"assistant","content":[{"type":"text","text":"crab "#);
        // First two bytes of 🦀 (U+1F980, UTF-8: F0 9F A6 80) so we
        // are mid-character at EOF.
        bytes.extend_from_slice(&[0xF0, 0x9F]);
        fs::write(&path, &bytes).unwrap();

        let data = store.load_session_data("chopped").unwrap();
        assert_eq!(data.messages.len(), 1, "first full message survives");
        assert_eq!(
            data.last_uuid,
            Some(Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-1234567890ef").unwrap())
        );
    }

    #[test]
    fn load_session_data_recovers_from_missing_trailing_newline() {
        // A crash between the JSON body and the final '\n' leaves a
        // complete record without a newline. The loader should still
        // parse it rather than dropping the last turn.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let created_at = datetime!(2026-01-01 00:00:00 UTC);
        let path = test_project_path(dir.path(), &session_filename("nonewline", created_at));
        let content = concat!(
            r#"{"type":"header","session_id":"nonewline","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}"#,
            "\n",
            r#"{"type":"message","uuid":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","message":{"role":"user","content":[{"type":"text","text":"ok"}]},"timestamp":"2026-01-01T00:00:01Z"}"#,
        );
        fs::write(&path, content).unwrap();

        let data = store.load_session_data("nonewline").unwrap();
        assert_eq!(data.messages.len(), 1);
        assert_eq!(
            data.last_uuid,
            Some(Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-1234567890ef").unwrap())
        );
    }

    #[test]
    fn load_session_data_rejects_future_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let created_at = datetime!(2026-01-01 00:00:00 UTC);
        let path = test_project_path(dir.path(), &session_filename("future", created_at));
        let future_version = CURRENT_VERSION + 1;
        fs::write(
            &path,
            format!(
                r#"{{"type":"header","session_id":"future","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z","version":{future_version}}}
"#
            ),
        )
        .unwrap();
        let err = store.load_session_data("future").unwrap_err().to_string();
        assert!(err.contains("newer than supported"), "got: {err}");
    }

    // ── list ──

    #[tokio::test]
    async fn list_returns_sessions_in_mtime_order_newest_first() {
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
            .await
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
            .await
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

    #[tokio::test]
    async fn list_mtime_overrides_header_created_at_order() {
        // Sort is by file mtime, not header created_at: a resumed session
        // (fresh mtime, older header) should bubble above a brand-new one.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());

        let mut w_old = store
            .create(&Entry::Header {
                session_id: "aaa-old-header".to_owned(),
                cwd: "/a".to_owned(),
                model: "m".to_owned(),
                created_at: datetime!(2026-01-01 10:00:00 UTC),
                version: CURRENT_VERSION,
            })
            .await
            .unwrap();
        w_old.append(&sample_title_entry("Old")).unwrap();
        drop(w_old);

        let mut w_new = store
            .create(&Entry::Header {
                session_id: "zzz-new-header".to_owned(),
                cwd: "/z".to_owned(),
                model: "m".to_owned(),
                created_at: datetime!(2026-04-17 10:00:00 UTC),
                version: CURRENT_VERSION,
            })
            .await
            .unwrap();
        w_new.append(&sample_title_entry("New")).unwrap();
        drop(w_new);

        // Backdate zzz's mtime so its file is older than aaa's.
        let new_path = test_session_file(dir.path(), "zzz-new-header");
        let far_past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let times = std::fs::FileTimes::new().set_modified(far_past);
        File::options()
            .write(true)
            .open(&new_path)
            .unwrap()
            .set_times(times)
            .unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions[0].session_id, "aaa-old-header",
            "freshly-touched file with older header should come first"
        );
        assert!(
            sessions[0].last_active_at > sessions[1].last_active_at,
            "mtime drives ordering"
        );
        assert_eq!(sessions[1].session_id, "zzz-new-header");
    }

    #[tokio::test]
    async fn list_picks_latest_title_when_re_appended() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("retitled")).await.unwrap();
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

    #[tokio::test]
    async fn list_finds_first_prompt_title_beyond_tail_window() {
        // The first-prompt title is written at line 2 and never re-appended.
        // A pure tail scan misses it once the file exceeds TAIL_BUF_SIZE.
        // Verify the head scan catches it.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("long")).await.unwrap();
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

        let path = test_session_file(dir.path(), "long");
        assert!(
            path.metadata().unwrap().len() > TAIL_BUF_SIZE,
            "test file should exceed the tail window"
        );
        let sessions = store.list().unwrap();
        let title = sessions[0].title.as_ref().unwrap();
        assert_eq!(title.title, "first prompt");
    }

    #[tokio::test]
    async fn list_works_without_title_or_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("bare")).await.unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "bare");
        assert!(sessions[0].title.is_none());
        assert!(sessions[0].exit.is_none());
    }

    #[test]
    fn list_skips_non_session_files_and_malformed_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        fs::write(test_project_path(dir.path(), "notes.txt"), "not a session").unwrap();
        fs::write(
            test_project_path(dir.path(), "bad.jsonl"),
            r#"{"type":"message","uuid":"00000000-0000-0000-0000-000000000000","message":{"role":"user","content":[]},"timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_is_scoped_to_current_project() {
        // Only sessions in the current project dir are visible; siblings
        // in other project subdirectories stay hidden.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _own = store.create(&sample_header("own")).await.unwrap();

        // Drop a session into a sibling project subdir.
        let sibling_store =
            SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let _foreign = sibling_store
            .create(&sample_header("foreign"))
            .await
            .unwrap();
        drop(sibling_store);

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "own");
    }

    // ── list_all ──

    #[tokio::test]
    async fn list_all_spans_every_project_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _own = store.create(&sample_header("own")).await.unwrap();

        let foreign_store =
            SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let _foreign = foreign_store
            .create(&sample_header("foreign"))
            .await
            .unwrap();
        drop(foreign_store);

        let all = store.list_all().unwrap();
        let mut ids: Vec<_> = all.iter().map(|s| s.session_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["foreign", "own"]);
    }

    // ── find_session_path ──

    #[tokio::test]
    async fn find_session_path_falls_back_to_other_projects() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());

        // Session lives in a different project subdirectory.
        let foreign_store =
            SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let _w = foreign_store
            .create(&sample_header("foreign"))
            .await
            .unwrap();
        drop(foreign_store);

        let found = store.find_session_path("foreign").unwrap();
        assert_eq!(
            found.parent(),
            Some(dir.path().join("other-project").as_path())
        );
        let name = found.file_name().unwrap().to_string_lossy();
        assert!(
            name.ends_with("-foreign.jsonl"),
            "unexpected filename: {name}"
        );
    }

    #[test]
    fn find_session_path_errors_for_unknown_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let err = store.find_session_path("ghost").unwrap_err().to_string();
        assert!(err.contains("session not found"), "got: {err}");
    }

    // ── append ──

    #[tokio::test]
    async fn append_writes_multiple_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("multi")).await.unwrap();

        writer
            .append(&sample_message_entry(Uuid::new_v4(), "hello"))
            .unwrap();
        writer
            .append(&sample_message_entry(Uuid::new_v4(), "world"))
            .unwrap();

        let content = fs::read_to_string(test_session_file(dir.path(), "multi")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 messages
    }
}
