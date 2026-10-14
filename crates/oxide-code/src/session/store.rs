//! Session file I/O.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use time::OffsetDateTime;
use tracing::{debug, warn};
use uuid::Uuid;

use super::chain::ChainBuilder;
use super::entry::{CURRENT_VERSION, CompactInfo, Entry, ExitInfo, SessionInfo, TitleInfo};
use super::path::{UNKNOWN_PROJECT_DIR, sanitize_cwd};
use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::prompt::SessionContext;
use crate::tool::ToolMetadata;
use crate::util::fs::create_private_dir_all;
use crate::util::path::xdg_dir;

const DATA_DIR: &str = "ox";
const SESSIONS_DIR: &str = "sessions";
pub(crate) const DEFAULT_SESSION_LIST_LIMIT: usize = 30;

// ── SessionStore ──

/// Low-level session file operations scoped to a project subdirectory.
#[derive(Clone)]
pub(crate) struct SessionStore {
    sessions_dir: PathBuf,
    project_dir: PathBuf,
}

/// A paginated slice of [`Self::list_paged`]'s output. `total` is the count before truncation,
/// so renderers can display `... and N more` footers without re-walking the directory.
#[derive(Debug, Default)]
pub(crate) struct ListPage {
    sessions: Vec<SessionInfo>,
    total: usize,
}

impl ListPage {
    pub(crate) fn new(sessions: Vec<SessionInfo>, total: usize) -> Self {
        debug_assert!(
            total >= sessions.len(),
            "ListPage total must include the materialised sessions",
        );
        Self { sessions, total }
    }

    pub(crate) fn sessions(&self) -> &[SessionInfo] {
        &self.sessions
    }

    pub(crate) fn into_sessions(self) -> Vec<SessionInfo> {
        self.sessions
    }

    pub(crate) fn total(&self) -> usize {
        self.total
    }
}

impl SessionStore {
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

    /// Creates the file (0o600, `O_EXCL`) and writes the header, then the JSONL `lines`, in one
    /// write that unlinks the file on failure.
    pub(crate) fn create(&self, header: &Entry, lines: &[u8]) -> Result<SessionWriter> {
        let Entry::Header {
            session_id,
            created_at,
            ..
        } = header
        else {
            bail!("expected Header entry");
        };
        validate_session_id(session_id)?;

        let mut buf = encode_lines([header])?;
        buf.extend_from_slice(lines);
        let path = self
            .project_dir
            .join(session_filename(session_id, *created_at));
        let file = create_written(&path, |mut file| file.write_all(&buf))?;
        Ok(SessionWriter {
            path,
            file: Some(file),
        })
    }

    /// Opens an existing session file in append mode, searching all projects.
    pub(crate) fn open_append(&self, session_id: &str) -> Result<SessionWriter> {
        let path = self.find_session_path(session_id)?;
        open_append_at(&path)
    }

    /// Loads a session's message chain via DAG resolution (newest-leaf-wins).
    pub(crate) fn load_session_data(&self, session_id: &str) -> Result<SessionData> {
        let path = self.find_session_path(session_id)?;
        load_session_data_from_path(&path)
    }

    /// List sessions for the current project, most recently active first. Wrapper around
    /// [`Self::list_paged`] for callers that don't need a cap.
    pub(crate) fn list(&self) -> Result<Vec<SessionInfo>> {
        Ok(self.list_paged(None, false)?.into_sessions())
    }

    /// List sessions across every project subdirectory. Wrapper around [`Self::list_paged`].
    pub(crate) fn list_all(&self) -> Result<Vec<SessionInfo>> {
        Ok(self.list_paged(None, true)?.into_sessions())
    }

    /// List the `limit` most-recently-active sessions and report the total available so callers
    /// can render a "and N more" footer. `None` is unbounded; `Some(n)` parses at most `n`
    /// files past the cheap stat-only pre-sort. `all` widens from the home project to every
    /// project subdir.
    pub(crate) fn list_paged(&self, limit: Option<usize>, all: bool) -> Result<ListPage> {
        let mut paths = if all {
            self.collect_paths_all_projects()?
        } else {
            collect_session_paths(&self.project_dir)?
        };
        sort_paths_recent_first(&mut paths);
        let total = paths.len();
        if let Some(cap) = limit {
            paths.truncate(cap);
        }
        let sessions = paths
            .into_iter()
            .filter_map(|(path, _)| match read_session_info(&path) {
                Ok(info) => Some(info),
                Err(e) => {
                    warn!("skipping unreadable session file {}: {e}", path.display());
                    None
                }
            })
            .collect();
        Ok(ListPage::new(sessions, total))
    }

    /// Stat-only walk across every project subdir; defers JSONL parsing to the caller's cap.
    fn collect_paths_all_projects(&self) -> Result<Vec<(PathBuf, OffsetDateTime)>> {
        let mut out = Vec::new();
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
                match collect_session_paths(&entry.path()) {
                    Ok(mut paths) => out.append(&mut paths),
                    Err(e) => warn!("skipping project dir {}: {e}", entry.path().display()),
                }
            }
        }
        Ok(out)
    }

    /// Remove a session's JSONL from disk. Refuses when `session_id == live_id` as the FS-boundary
    /// defense against unlinking the open append-writer's file. Upstream callers (resume picker
    /// filter, `/delete` resolver) also filter the live id, so this layer catches genuine bugs.
    pub(crate) fn delete(&self, session_id: &str, live_id: &str) -> Result<()> {
        validate_session_id(session_id)?;
        if session_id == live_id {
            bail!("refusing to delete the live session: {session_id}");
        }
        let path = self.find_session_path(session_id)?;
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove session file {}", path.display()))
    }

    /// Finds a session file by suffix match, checking the home project first.
    fn find_session_path(&self, session_id: &str) -> Result<PathBuf> {
        validate_session_id(session_id)?;
        if let Some(path) = find_session_in(&self.project_dir, session_id)? {
            return Ok(path);
        }
        for entry in fs::read_dir(&self.sessions_dir)
            .with_context(|| format!("cannot read {}", self.sessions_dir.display()))?
        {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!("skipping directory entry while resolving {session_id}: {e}");
                    continue;
                }
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

    #[cfg(test)]
    pub(crate) fn open_at(sessions_dir: PathBuf, project_name: &str) -> Result<Self> {
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

/// Opens an existing session file in append mode by explicit path.
pub(crate) fn open_append_at(path: &Path) -> Result<SessionWriter> {
    Ok(SessionWriter {
        path: path.to_owned(),
        file: Some(open_append_file(path)?),
    })
}

/// Loads session data from an explicit path with DAG-based chain resolution.
pub(crate) fn load_session_data_from_path(path: &Path) -> Result<SessionData> {
    let file =
        File::open(path).with_context(|| format!("session not found: {}", path.display()))?;
    let mut lines = JsonlLines::new(BufReader::new(file));
    let mut builder = SessionDataBuilder::new();

    while let Some((line_no, line)) = lines.next_line()? {
        let entry: Entry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(e) => {
                warn!("skipping malformed entry at line {line_no}: {e}");
                continue;
            }
        };
        builder.apply_entry(entry, line_no)?;
    }

    Ok(builder.finish())
}

struct SessionDataBuilder {
    chain: ChainBuilder,
    context: Option<SessionContext>,
    compact: Option<CompactInfo>,
    /// A compact boundary that takes effect only once its post-compact root follows it.
    pending_compact: Option<CompactInfo>,
    latest_title: Option<TitleInfo>,
    compact_tail_uuids: Option<HashSet<Uuid>>,
    compact_tail_accepts_sidecars: bool,
    tool_result_metadata: HashMap<String, ToolMetadata>,
    file_snapshots: Vec<FileSnapshot>,
}

impl SessionDataBuilder {
    fn new() -> Self {
        Self {
            chain: ChainBuilder::new(),
            context: None,
            compact: None,
            pending_compact: None,
            latest_title: None,
            compact_tail_uuids: None,
            compact_tail_accepts_sidecars: true,
            tool_result_metadata: HashMap::new(),
            file_snapshots: Vec::new(),
        }
    }

    fn apply_entry(&mut self, entry: Entry, line_no: u32) -> Result<()> {
        match entry {
            Entry::Header { version, .. } if version > CURRENT_VERSION => {
                bail!(
                    "session format version {version} is newer than supported ({CURRENT_VERSION}); please upgrade oxide-code"
                );
            }
            Entry::Context { context, .. } => self.context = Some(*context),
            Entry::Message {
                uuid,
                parent_uuid,
                message,
                timestamp,
            } => self.insert_message(uuid, parent_uuid, message, timestamp, line_no),
            Entry::Title {
                title, updated_at, ..
            } if self
                .latest_title
                .as_ref()
                .is_none_or(|cur| updated_at > cur.updated_at) =>
            {
                self.latest_title = Some(TitleInfo { title, updated_at });
            }
            Entry::ToolResultMetadata {
                tool_use_id,
                metadata,
                ..
            } if self.accepts_sidecar() => {
                self.tool_result_metadata.insert(tool_use_id, metadata);
            }
            Entry::FileSnapshot { snapshot } if self.accepts_sidecar() => {
                self.file_snapshots.push(snapshot);
            }
            Entry::Compact {
                summary,
                pre_message_count,
                instructions,
                ..
            } => {
                self.pending_compact = Some(CompactInfo {
                    summary,
                    pre_message_count,
                    instructions,
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn insert_message(
        &mut self,
        uuid: Uuid,
        parent_uuid: Option<Uuid>,
        message: Message,
        timestamp: OffsetDateTime,
        line_no: u32,
    ) {
        if let Some(compact) = self.pending_compact.take() {
            if parent_uuid.is_none() {
                self.reset_at_compact(compact);
            } else {
                warn!("skipping compact boundary without its post-compact root at line {line_no}");
            }
        }
        let accepted = self.accepts_compact_tail_message(uuid, parent_uuid);
        if self.compact_tail_uuids.is_some() {
            self.compact_tail_accepts_sidecars = accepted;
        }
        if !accepted {
            warn!("skipping pre-compact branch append at line {line_no}");
            return;
        }
        if parent_uuid.is_none() {
            self.clear_sidecars();
        }
        self.chain.insert(uuid, parent_uuid, message, timestamp);
    }

    fn accepts_compact_tail_message(&mut self, uuid: Uuid, parent_uuid: Option<Uuid>) -> bool {
        let Some(tail_uuids) = &mut self.compact_tail_uuids else {
            return true;
        };
        let accepted = match parent_uuid {
            None => tail_uuids.is_empty(),
            Some(parent) => tail_uuids.contains(&parent),
        };
        if accepted {
            tail_uuids.insert(uuid);
        }
        accepted
    }

    fn accepts_sidecar(&self) -> bool {
        self.compact_tail_uuids.is_none() || self.compact_tail_accepts_sidecars
    }

    fn reset_at_compact(&mut self, compact: CompactInfo) {
        self.chain = ChainBuilder::new();
        self.context = None;
        self.compact = Some(compact);
        self.compact_tail_uuids = Some(HashSet::new());
        self.compact_tail_accepts_sidecars = false;
        self.clear_sidecars();
    }

    fn clear_sidecars(&mut self) {
        self.tool_result_metadata.clear();
        self.file_snapshots.clear();
    }

    fn finish(self) -> SessionData {
        let (messages, last_uuid) = self.chain.resolve();
        SessionData {
            messages,
            last_uuid,
            context: self.context,
            compact: self.compact,
            title: self.latest_title,
            tool_result_metadata: self.tool_result_metadata,
            file_snapshots: self.file_snapshots,
        }
    }
}

/// Reads just the `session_id` from a session file's header line.
pub(crate) fn read_session_id_from_path(path: &Path) -> Result<String> {
    let file =
        File::open(path).with_context(|| format!("session not found: {}", path.display()))?;
    let header = read_header(&mut JsonlLines::new(BufReader::new(file)), path)?;
    Ok(header.session_id)
}

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

fn session_filename(session_id: &str, created_at: OffsetDateTime) -> String {
    format!("{}-{session_id}.jsonl", created_at.unix_timestamp())
}

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

/// Stat-only scan of one project subdirectory: returns `.jsonl` files paired with mtime so the
/// caller can sort and truncate before paying the per-file JSONL parse cost in
/// [`read_session_info`]. Stat failures warn-skip — bucketing them at `UNIX_EPOCH` would silently
/// sort them out of any cap window.
fn collect_session_paths(dir: &Path) -> Result<Vec<(PathBuf, OffsetDateTime)>> {
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
        .filter_map(|e| {
            let path = e.path();
            e.metadata()
                .and_then(|m| m.modified())
                .inspect_err(|err| {
                    warn!("skipping session {} (stat failed): {err}", path.display());
                })
                .ok()
                .map(|t| (path, OffsetDateTime::from(t)))
        })
        .collect())
}

fn sort_paths_recent_first(paths: &mut [(PathBuf, OffsetDateTime)]) {
    paths
        .sort_by(|(p1, m1), (p2, m2)| m2.cmp(m1).then_with(|| p2.file_name().cmp(&p1.file_name())));
}

// ── SessionWriter ──

/// Append handle for one session file that reopens its path after a failed write.
#[derive(Debug)]
pub(crate) struct SessionWriter {
    path: PathBuf,
    file: Option<File>,
}

impl SessionWriter {
    /// Appends JSONL `lines` in one write, so concurrent appenders interleave only between batches.
    pub(super) fn append_lines(&mut self, lines: &[u8]) -> Result<()> {
        let file = match self.file.take() {
            Some(file) => file,
            None => open_append_file(&self.path)?,
        };
        append_whole(&file, &mut &file, lines)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        self.file = Some(file);
        Ok(())
    }
}

/// Encodes `entries` as JSONL, one line each.
pub(super) fn encode_lines<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut buf, entry).context("failed to serialize entry")?;
        buf.push(b'\n');
    }
    Ok(buf)
}

// ── SessionData ──

/// Data loaded from a session file on resume.
#[derive(Debug)]
pub(crate) struct SessionData {
    pub(crate) messages: Vec<Message>,
    pub(crate) last_uuid: Option<Uuid>,
    /// Recorded prompt state, `None` for a transcript that predates recording or whose last
    /// compaction no user message followed.
    pub(crate) context: Option<SessionContext>,
    pub(crate) compact: Option<CompactInfo>,
    pub(crate) title: Option<TitleInfo>,
    pub(crate) tool_result_metadata: HashMap<String, ToolMetadata>,
    pub(crate) file_snapshots: Vec<FileSnapshot>,
}

// ── File Opening ──

fn open_create_exclusive(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).append(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Creates `path` and runs `write` on it, unlinking the file when `write` fails.
fn create_written(path: &Path, write: impl FnOnce(&File) -> io::Result<()>) -> Result<File> {
    let file = open_create_exclusive(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    if let Err(e) = write(&file) {
        _ = fs::remove_file(path);
        return Err(e).with_context(|| format!("failed to write {}", path.display()));
    }
    Ok(file)
}

fn open_append_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("session not found: {}", path.display()))
}

/// Writes `lines` through `out`, a handle on `file`, after terminating a torn last line. A failed
/// write is truncated back off unless another appender has written since.
fn append_whole(file: &File, out: &mut impl Write, lines: &[u8]) -> io::Result<()> {
    let mut start = file.metadata()?.len();
    if start > 0 && !ends_with_newline(file, start)? {
        out.write_all(b"\n")?;
        start += 1;
    }
    let mut written = 0;
    while written < lines.len() {
        let err = match out.write(&lines[written..]) {
            Ok(0) => io::ErrorKind::WriteZero.into(),
            Ok(n) => {
                written += n;
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => e,
        };
        let end = start + written as u64;
        if file.metadata().is_ok_and(|meta| meta.len() == end)
            && let Err(e) = file.set_len(start)
        {
            warn!("failed to truncate a partial session write: {e}");
        }
        return Err(err);
    }
    Ok(())
}

fn ends_with_newline(file: &File, len: u64) -> io::Result<bool> {
    let mut last = [0];
    #[cfg(unix)]
    std::os::unix::fs::FileExt::read_exact_at(file, &mut last, len - 1)?;
    #[cfg(not(unix))]
    {
        let mut file = file;
        io::Seek::seek(&mut file, io::SeekFrom::Start(len - 1))?;
        io::Read::read_exact(&mut file, &mut last)?;
    }
    Ok(last[0] == b'\n')
}

// ── Line Reading ──

/// JSONL line reader that warn-skips invalid UTF-8 lines, such as one torn mid-codepoint.
struct JsonlLines<R> {
    reader: R,
    line: String,
    line_no: u32,
}

impl<R: BufRead> JsonlLines<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line: String::new(),
            line_no: 0,
        }
    }

    /// Next non-empty line with its 1-based line number, or `None` at EOF.
    fn next_line(&mut self) -> Result<Option<(u32, &str)>> {
        let mut buf = std::mem::take(&mut self.line).into_bytes();
        loop {
            buf.clear();
            let read = self
                .reader
                .read_until(b'\n', &mut buf)
                .with_context(|| format!("read error at line {}", self.line_no + 1))?;
            if read == 0 {
                return Ok(None);
            }
            self.line_no += 1;
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            if buf.is_empty() {
                continue;
            }
            match String::from_utf8(buf) {
                Ok(line) => {
                    self.line = line;
                    return Ok(Some((self.line_no, &self.line)));
                }
                Err(e) => {
                    warn!("skipping non-utf8 entry at line {}: {e}", self.line_no);
                    buf = e.into_bytes();
                }
            }
        }
    }
}

struct HeaderInfo {
    session_id: String,
    cwd: String,
    created_at: OffsetDateTime,
    git_branch: Option<String>,
}

/// Parses the first non-empty line as the session header and validates its id.
fn read_header<R: BufRead>(lines: &mut JsonlLines<R>, path: &Path) -> Result<HeaderInfo> {
    let Some((_, line)) = lines.next_line()? else {
        bail!("{} is empty (no header)", path.display());
    };
    let Entry::Header {
        session_id,
        cwd,
        created_at,
        git_branch,
        ..
    } = serde_json::from_str(line)
        .with_context(|| format!("first line of {} is not a valid header", path.display()))?
    else {
        bail!("{} does not begin with a header", path.display());
    };
    validate_session_id(&session_id)?;
    Ok(HeaderInfo {
        session_id,
        cwd,
        created_at,
        git_branch,
    })
}

// ── Session Info Extraction ──

// Cheap pre-filter: `serde(tag = "type")` always emits `type` first.
const TITLE_LINE_PREFIX: &str = r#"{"type":"title""#;
const SUMMARY_LINE_PREFIX: &str = r#"{"type":"summary""#;

fn read_session_info(path: &Path) -> Result<SessionInfo> {
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot stat {}", path.display()))?;
    let mut lines = JsonlLines::new(BufReader::new(&file));
    let HeaderInfo {
        session_id,
        cwd,
        created_at,
        git_branch,
    } = read_header(&mut lines, path)?;

    let mut title: Option<TitleInfo> = None;
    let mut exit: Option<ExitInfo> = None;
    while let Some((_, line)) = lines.next_line()? {
        if line.starts_with(TITLE_LINE_PREFIX) {
            if let Some(t) = parse_title(line)
                && title
                    .as_ref()
                    .is_none_or(|cur| t.updated_at > cur.updated_at)
            {
                title = Some(t);
            }
        } else if line.starts_with(SUMMARY_LINE_PREFIX)
            && let Ok(Entry::Summary {
                message_count,
                updated_at,
            }) = serde_json::from_str(line)
            && exit.as_ref().is_none_or(|cur| updated_at > cur.updated_at)
        {
            exit = Some(ExitInfo {
                message_count,
                updated_at,
            });
        }
    }

    let last_active_at = match metadata.modified() {
        Ok(t) => OffsetDateTime::from(t),
        Err(e) => {
            warn!(
                "cannot read mtime of {} (falling back to created_at): {e}",
                path.display()
            );
            created_at
        }
    };

    Ok(SessionInfo {
        session_id,
        cwd,
        last_active_at,
        title,
        exit,
        git_branch,
    })
}

fn parse_title(line: &str) -> Option<TitleInfo> {
    match serde_json::from_str(line).ok()? {
        Entry::Title {
            title, updated_at, ..
        } => Some(TitleInfo { title, updated_at }),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) const TEST_PROJECT: &str = "test-project";

#[cfg(test)]
pub(crate) fn test_store(dir: &Path) -> SessionStore {
    SessionStore::open_at(dir.to_path_buf(), TEST_PROJECT).unwrap()
}

/// Seed a session for cross-module tests. `last_active_at` derives from `created_at`. Title and
/// summary are written only when their args are `Some`; with `message_count: None` the session
/// is left unfinished. Title source is hard-coded to `UserProvided`.
#[cfg(test)]
pub(crate) fn seed_test_session(
    store: &SessionStore,
    session_id: &str,
    title: Option<&str>,
    message_count: Option<u32>,
    created_at: time::OffsetDateTime,
) {
    let mut writer = store
        .create(
            &Entry::Header {
                session_id: session_id.to_owned(),
                cwd: "/work/proj".to_owned(),
                model: "claude-opus-4-7".to_owned(),
                created_at,
                version: CURRENT_VERSION,
                git_branch: None,
            },
            &[],
        )
        .unwrap();
    if let Some(t) = title {
        writer
            .append(&[Entry::Title {
                title: t.to_owned(),
                source: super::entry::TitleSource::UserProvided,
                updated_at: created_at,
            }])
            .unwrap();
    }
    if let Some(count) = message_count {
        writer
            .append(&[Entry::Summary {
                message_count: count,
                updated_at: created_at,
            }])
            .unwrap();
    }
}

#[cfg(test)]
pub(super) fn test_session_file(dir: &Path, session_id: &str) -> PathBuf {
    let project_dir = test_project_dir(dir);
    find_session_in(&project_dir, session_id)
        .unwrap()
        .unwrap_or_else(|| panic!("no session file for id {session_id} in {project_dir:?}"))
}

#[cfg(test)]
pub(crate) fn test_project_dir(dir: &Path) -> PathBuf {
    dir.join(TEST_PROJECT)
}

#[cfg(test)]
impl SessionWriter {
    /// Writer whose first append fails on a read-only handle and whose next one reopens `path`.
    pub(super) fn failing_once(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            file: Some(File::open(path).unwrap()),
        }
    }

    pub(super) fn append(&mut self, entries: &[Entry]) -> Result<()> {
        self.append_lines(&encode_lines(entries)?)
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use time::macros::datetime;

    use super::super::entry::TitleSource;
    use super::*;
    use crate::message::ContentBlock;
    use crate::prompt::PromptParts;

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
            git_branch: None,
        }
    }

    fn sample_message_entry(uuid: Uuid, text: &str) -> Entry {
        sample_message_at(uuid, None, datetime!(2026-04-16 12:00:01 UTC), text)
    }

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

    fn session_context(date: &str) -> SessionContext {
        let mut prompt = PromptParts::default();
        date.clone_into(&mut prompt.seen.date);
        prompt.session_context()
    }

    fn sample_context(date: &str) -> Entry {
        Entry::Context {
            context: Box::new(session_context(date)),
            timestamp: datetime!(2026-04-16 12:00:00 UTC),
        }
    }

    fn sample_snapshot(path: &str) -> Entry {
        Entry::FileSnapshot {
            snapshot: FileSnapshot {
                path: PathBuf::from(path),
                content_hash: 42,
                mtime: datetime!(2026-04-16 12:00:00 UTC),
                size: 10,
                last_view: crate::file_tracker::LastView::Full,
                recorded_at: datetime!(2026-04-16 12:00:01 UTC),
            },
        }
    }

    // ── SessionStore::open ──

    async fn open_in_isolated_env(xdg: &Path) -> SessionStore {
        let home = tempfile::tempdir().unwrap();
        temp_env::async_with_vars(
            [
                ("XDG_DATA_HOME", Some(xdg.to_string_lossy().into_owned())),
                ("HOME", Some(home.path().to_string_lossy().into_owned())),
            ],
            async { SessionStore::open() },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn open_uses_xdg_data_home_when_set() {
        let xdg = tempfile::tempdir().unwrap();
        let store = open_in_isolated_env(xdg.path()).await;
        let sessions = xdg.path().join(DATA_DIR).join(SESSIONS_DIR);
        assert_eq!(store.sessions_dir, sessions);
        assert!(sessions.is_dir(), "sessions root created: {sessions:?}");
        assert!(store.project_dir.is_dir(), "project dir created");
        assert!(
            store.project_dir.starts_with(&sessions),
            "project dir lives under sessions root",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_creates_private_dirs_with_mode_0o700_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let xdg = tempfile::tempdir().unwrap();
        let store = open_in_isolated_env(xdg.path()).await;
        for dir in [&store.sessions_dir, &store.project_dir] {
            let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{dir:?} must be 0o700, got {mode:o}");
        }
    }

    #[tokio::test]
    async fn open_falls_back_to_home_local_share_when_xdg_unset() {
        let home = tempfile::tempdir().unwrap();
        let store = temp_env::async_with_vars(
            [
                ("XDG_DATA_HOME", None),
                ("HOME", Some(home.path().to_string_lossy().into_owned())),
            ],
            async { SessionStore::open() },
        )
        .await
        .unwrap();
        let expected = home
            .path()
            .join(".local/share")
            .join(DATA_DIR)
            .join(SESSIONS_DIR);
        assert_eq!(store.sessions_dir, expected);
    }

    // ── create ──

    #[tokio::test]
    async fn create_writes_the_header_then_the_given_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let uuid = Uuid::new_v4();
        let lines = encode_lines(&[sample_message_entry(uuid, "hi")]).unwrap();

        let _writer = store.create(&sample_header("test-id"), &lines).unwrap();

        let content = fs::read_to_string(test_session_file(dir.path(), "test-id")).unwrap();
        let entries: Vec<Entry> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            matches!(
                entries.as_slice(),
                [
                    Entry::Header { session_id, version, .. },
                    Entry::Message { uuid: written, .. },
                ] if session_id == "test-id" && *version == CURRENT_VERSION && *written == uuid
            ),
            "got: {entries:?}"
        );
    }

    #[tokio::test]
    async fn create_rejects_non_header_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let entry = sample_message_entry(Uuid::new_v4(), "hi");

        assert!(store.create(&entry, &[]).is_err());
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
        assert!(store.create(&header, &[]).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_sets_user_only_file_permissions_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("perm-test"), &[]).unwrap();

        let meta = fs::metadata(test_session_file(dir.path(), "perm-test")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    // ── open_append ──

    #[tokio::test]
    async fn open_append_writes_to_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("append-test"), &[]).unwrap();
        let u1 = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                u1,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "first",
            )])
            .unwrap();
        drop(writer);

        let mut writer = store.open_append("append-test").unwrap();
        writer
            .append(&[sample_message_at(
                Uuid::new_v4(),
                Some(u1),
                datetime!(2026-04-16 12:00:02 UTC),
                "second",
            )])
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
    fn open_append_starts_an_empty_file_without_a_blank_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let path = test_project_path(
            dir.path(),
            &session_filename("empty", datetime!(2026-04-16 12:00:00 UTC)),
        );
        fs::write(&path, "").unwrap();

        store
            .open_append("empty")
            .unwrap()
            .append(&[sample_summary_entry(0)])
            .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.starts_with(r#"{"type":"summary""#),
            "got: {content}"
        );
    }

    #[tokio::test]
    async fn open_append_fails_for_nonexistent_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.open_append("no-such-session").is_err());
    }

    #[tokio::test]
    async fn open_append_allows_concurrent_resumes_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer_a = store.create(&sample_header("concurrent"), &[]).unwrap();

        let start = std::time::Instant::now();
        let writer_b = store.open_append("concurrent");
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
    async fn load_session_data_includes_only_messages_with_last_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("load-test"), &[]).unwrap();

        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        writer
            .append(&[Entry::Message {
                uuid: u1,
                parent_uuid: None,
                message: Message::user("hello"),
                timestamp: datetime!(2026-04-16 12:00:01 UTC),
            }])
            .unwrap();
        writer
            .append(&[Entry::Message {
                uuid: u2,
                parent_uuid: Some(u1),
                message: Message::assistant("hi there"),
                timestamp: datetime!(2026-04-16 12:00:02 UTC),
            }])
            .unwrap();
        writer.append(&[sample_title_entry("greeting")]).unwrap();
        writer.append(&[sample_summary_entry(2)]).unwrap();
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
    fn load_session_data_resets_sidecars_at_compact_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store
            .create(&sample_header("compact-sidecars"), &[])
            .unwrap();

        let pre = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                pre,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "pre-compact",
            )])
            .unwrap();
        writer
            .append(&[Entry::ToolResultMetadata {
                tool_use_id: "pre-tool".to_owned(),
                metadata: ToolMetadata {
                    title: Some("pre metadata".to_owned()),
                    ..ToolMetadata::default()
                },
                timestamp: datetime!(2026-04-16 12:00:02 UTC),
            }])
            .unwrap();
        writer.append(&[sample_snapshot("pre.txt")]).unwrap();
        writer
            .append(&[Entry::Compact {
                summary: "summary".to_owned(),
                pre_message_count: 1,
                instructions: None,
                timestamp: datetime!(2026-04-16 12:00:03 UTC),
            }])
            .unwrap();

        let post = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                post,
                None,
                datetime!(2026-04-16 12:00:04 UTC),
                "post-compact summary",
            )])
            .unwrap();
        writer
            .append(&[Entry::ToolResultMetadata {
                tool_use_id: "post-tool".to_owned(),
                metadata: ToolMetadata {
                    title: Some("post metadata".to_owned()),
                    ..ToolMetadata::default()
                },
                timestamp: datetime!(2026-04-16 12:00:05 UTC),
            }])
            .unwrap();
        writer.append(&[sample_snapshot("post.txt")]).unwrap();
        drop(writer);

        let data = store.load_session_data("compact-sidecars").unwrap();
        assert_eq!(data.last_uuid, Some(post));
        assert_eq!(data.messages.len(), 1);
        assert!(
            data.tool_result_metadata.contains_key("post-tool"),
            "post-compact metadata should survive",
        );
        assert!(
            !data.tool_result_metadata.contains_key("pre-tool"),
            "pre-compact metadata must not leak into replay",
        );
        assert_eq!(data.file_snapshots.len(), 1);
        assert!(data.file_snapshots[0].path.ends_with("post.txt"));
    }

    #[test]
    fn load_session_data_takes_the_latest_context_since_the_last_compact_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store
            .create(&sample_header("compact-context"), &[])
            .unwrap();
        let load = || store.load_session_data("compact-context").unwrap().context;

        writer.append(&[sample_context("2026-04-16")]).unwrap();
        writer
            .append(&[sample_message_entry(Uuid::new_v4(), "first")])
            .unwrap();
        writer.append(&[sample_context("2026-04-17")]).unwrap();
        assert_eq!(load(), Some(session_context("2026-04-17")));

        writer
            .append(&[Entry::Compact {
                summary: "summary".to_owned(),
                pre_message_count: 1,
                instructions: None,
                timestamp: datetime!(2026-04-17 12:00:00 UTC),
            }])
            .unwrap();
        assert_eq!(
            load(),
            Some(session_context("2026-04-17")),
            "a compact without its post-compact root keeps the old context",
        );

        writer
            .append(&[sample_message_at(
                Uuid::new_v4(),
                None,
                datetime!(2026-04-17 12:00:01 UTC),
                "summary",
            )])
            .unwrap();
        assert_eq!(load(), None, "a compacted transcript no longer carries it");

        writer.append(&[sample_context("2026-04-18")]).unwrap();
        assert_eq!(load(), Some(session_context("2026-04-18")));
    }

    #[test]
    fn load_session_data_ignores_newer_old_chain_messages_after_compact() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store
            .create(&sample_header("compact-generation"), &[])
            .unwrap();

        let pre = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                pre,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "pre-compact",
            )])
            .unwrap();
        writer
            .append(&[Entry::Compact {
                summary: "summary".to_owned(),
                pre_message_count: 1,
                instructions: Some("focus".to_owned()),
                timestamp: datetime!(2026-04-16 12:00:02 UTC),
            }])
            .unwrap();

        let synthetic = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                synthetic,
                None,
                datetime!(2026-04-16 12:00:03 UTC),
                "synthetic summary",
            )])
            .unwrap();
        writer
            .append(&[sample_message_at(
                Uuid::new_v4(),
                Some(pre),
                datetime!(2026-04-16 12:00:05 UTC),
                "stale old-chain append",
            )])
            .unwrap();
        drop(writer);

        let data = store.load_session_data("compact-generation").unwrap();
        assert_eq!(data.last_uuid, Some(synthetic));
        assert_eq!(data.messages.len(), 1);
        assert!(matches!(
            &data.messages[0].content[0],
            ContentBlock::Text { text } if text == "synthetic summary"
        ));
        assert_eq!(
            data.compact,
            Some(CompactInfo {
                summary: "summary".to_owned(),
                pre_message_count: 1,
                instructions: Some("focus".to_owned()),
            })
        );
    }

    #[test]
    fn load_session_data_ignores_a_compact_whose_root_never_landed() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("torn-compact"), &[]).unwrap();
        let pre = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                pre,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "pre-compact",
            )])
            .unwrap();
        let mut torn = encode_lines(&[Entry::Compact {
            summary: "summary".to_owned(),
            pre_message_count: 1,
            instructions: None,
            timestamp: datetime!(2026-04-16 12:00:02 UTC),
        }])
        .unwrap();
        torn.extend_from_slice(br#"{"type":"message","uuid":"#);
        writer.append_lines(&torn).unwrap();

        let kept = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                kept,
                Some(pre),
                datetime!(2026-04-16 12:00:03 UTC),
                "kept",
            )])
            .unwrap();

        let data = store.load_session_data("torn-compact").unwrap();
        assert!(data.compact.is_none(), "got: {:?}", data.compact);
        assert_eq!(data.last_uuid, Some(kept));
        assert_eq!(data.messages.len(), 2);
    }

    #[test]
    fn load_session_data_skips_sidecars_after_rejected_compact_tail_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store
            .create(&sample_header("compact-stale-sidecars"), &[])
            .unwrap();

        let pre = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                pre,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "pre-compact",
            )])
            .unwrap();
        writer
            .append(&[Entry::Compact {
                summary: "summary".to_owned(),
                pre_message_count: 1,
                instructions: None,
                timestamp: datetime!(2026-04-16 12:00:02 UTC),
            }])
            .unwrap();

        let synthetic = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                synthetic,
                None,
                datetime!(2026-04-16 12:00:03 UTC),
                "synthetic summary",
            )])
            .unwrap();
        writer
            .append(&[sample_message_at(
                Uuid::new_v4(),
                Some(pre),
                datetime!(2026-04-16 12:00:04 UTC),
                "stale old-chain append",
            )])
            .unwrap();
        writer
            .append(&[Entry::ToolResultMetadata {
                tool_use_id: "stale-tool".to_owned(),
                metadata: ToolMetadata {
                    title: Some("stale metadata".to_owned()),
                    ..ToolMetadata::default()
                },
                timestamp: datetime!(2026-04-16 12:00:05 UTC),
            }])
            .unwrap();
        writer.append(&[sample_snapshot("stale.txt")]).unwrap();

        let valid = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                valid,
                Some(synthetic),
                datetime!(2026-04-16 12:00:07 UTC),
                "valid post-compact append",
            )])
            .unwrap();
        writer
            .append(&[Entry::ToolResultMetadata {
                tool_use_id: "valid-tool".to_owned(),
                metadata: ToolMetadata {
                    title: Some("valid metadata".to_owned()),
                    ..ToolMetadata::default()
                },
                timestamp: datetime!(2026-04-16 12:00:08 UTC),
            }])
            .unwrap();
        writer.append(&[sample_snapshot("valid.txt")]).unwrap();
        drop(writer);

        let data = store.load_session_data("compact-stale-sidecars").unwrap();
        assert_eq!(data.last_uuid, Some(valid));
        assert!(data.tool_result_metadata.contains_key("valid-tool"));
        assert!(
            !data.tool_result_metadata.contains_key("stale-tool"),
            "metadata following a rejected compact-tail message must not survive",
        );
        assert_eq!(data.file_snapshots.len(), 1);
        assert!(data.file_snapshots[0].path.ends_with("valid.txt"));
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
    fn load_session_data_nonexistent_session_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(store.load_session_data("nonexistent").is_err());
    }

    #[test]
    fn load_session_data_rejects_malicious_session_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let too_long = "a".repeat(65);
        for bogus in [
            "",
            "../etc/passwd",
            r"..\..\etc\passwd",
            "session\0evil",
            "foo:bar",
            "foo|bar",
            "foo*",
            &too_long,
        ] {
            let err = store.load_session_data(bogus).unwrap_err().to_string();
            assert!(err.contains("invalid session ID"), "{bogus:?}: {err}");
        }
    }

    #[tokio::test]
    async fn load_session_data_picks_newest_leaf_on_fork() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("fork"), &[]).unwrap();

        let root = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                root,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "shared root",
            )])
            .unwrap();

        let branch_a = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                branch_a,
                Some(root),
                datetime!(2026-04-16 12:00:02 UTC),
                "branch A (older leaf)",
            )])
            .unwrap();

        let branch_b = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                branch_b,
                Some(root),
                datetime!(2026-04-16 12:00:03 UTC),
                "branch B (newer leaf)",
            )])
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
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("orphan"), &[]).unwrap();

        let only = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                only,
                Some(Uuid::new_v4()), // points at a missing ancestor
                datetime!(2026-04-16 12:00:01 UTC),
                "orphan tip",
            )])
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
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("cycle"), &[]).unwrap();

        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                a,
                Some(b), // a points at b — part of the cycle
                datetime!(2026-04-16 12:00:01 UTC),
                "A",
            )])
            .unwrap();
        writer
            .append(&[sample_message_at(
                b,
                Some(a), // b points back at a — completes the cycle
                datetime!(2026-04-16 12:00:02 UTC),
                "B",
            )])
            .unwrap();
        drop(writer);

        let data = store.load_session_data("cycle").unwrap();
        assert!(
            data.messages.is_empty(),
            "cycle with no leaf should yield an empty chain, got: {:?}",
            data.messages,
        );
    }

    #[tokio::test]
    async fn load_session_data_prefers_later_duplicate_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("dup"), &[]).unwrap();

        let u = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                u,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "first copy",
            )])
            .unwrap();
        writer
            .append(&[sample_message_at(
                u,
                None,
                datetime!(2026-04-16 12:00:05 UTC),
                "second copy",
            )])
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
        bytes.extend_from_slice(br#"{"type":"message","uuid":"b2c3d4e5-f6a7-8901-bcde-234567890abc","message":{"role":"assistant","content":[{"type":"text","text":"crab "#);
        // Truncated mid-codepoint (first two bytes of U+1F980).
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

    // ── read_session_id_from_path ──

    #[test]
    fn read_session_id_from_path_produces_header_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("standalone.jsonl");
        fs::write(
            &path,
            indoc! {r#"
                {"type":"header","session_id":"abc-123","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}
                {"type":"message","uuid":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","message":{"role":"user","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-01-01T00:00:01Z"}
            "#},
        )
        .unwrap();
        assert_eq!(read_session_id_from_path(&path).unwrap(), "abc-123");
    }

    #[test]
    fn read_session_id_from_path_rejects_non_header_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("headless.jsonl");
        fs::write(
            &path,
            r#"{"type":"message","uuid":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","message":{"role":"user","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-01-01T00:00:01Z"}
"#,
        )
        .unwrap();
        let err = read_session_id_from_path(&path).unwrap_err().to_string();
        assert!(err.contains("does not begin with a header"), "got: {err}");
    }

    #[test]
    fn read_session_id_from_path_rejects_unparsable_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.jsonl");
        fs::write(&path, "not json\n").unwrap();
        let err = read_session_id_from_path(&path).unwrap_err().to_string();
        assert!(err.contains("not a valid header"), "got: {err}");
    }

    #[test]
    fn read_session_id_from_path_rejects_invalid_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("traversal.jsonl");
        fs::write(
            &path,
            r#"{"type":"header","session_id":"../escape","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}
"#,
        )
        .unwrap();
        let err = read_session_id_from_path(&path).unwrap_err().to_string();
        assert!(err.contains("invalid session ID"), "got: {err}");
    }

    #[test]
    fn read_session_id_from_path_rejects_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        fs::write(&path, "").unwrap();
        let err = read_session_id_from_path(&path).unwrap_err().to_string();
        assert!(err.contains("is empty"), "got: {err}");
    }

    #[test]
    fn read_session_id_from_path_errors_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_session_id_from_path(&dir.path().join("ghost.jsonl"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("session not found"), "got: {err}");
    }

    // ── list ──

    #[tokio::test]
    async fn list_orders_sessions_by_mtime_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());

        let mut wa = store
            .create(
                &Entry::Header {
                    session_id: "aaa".to_owned(),
                    cwd: "/a".to_owned(),
                    model: "m".to_owned(),
                    created_at: datetime!(2026-04-15 10:00:00 UTC),
                    version: CURRENT_VERSION,
                    git_branch: None,
                },
                &[],
            )
            .unwrap();
        wa.append(&[sample_title_entry("Older")]).unwrap();
        wa.append(&[sample_summary_entry(3)]).unwrap();

        let mut wb = store
            .create(
                &Entry::Header {
                    session_id: "bbb".to_owned(),
                    cwd: "/b".to_owned(),
                    model: "m".to_owned(),
                    created_at: datetime!(2026-04-16 12:00:00 UTC),
                    version: CURRENT_VERSION,
                    git_branch: None,
                },
                &[],
            )
            .unwrap();
        wb.append(&[sample_title_entry("Newer")]).unwrap();
        wb.append(&[sample_summary_entry(5)]).unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "bbb");
        assert_eq!(sessions[0].title.as_ref().unwrap().title, "Newer");
        assert_eq!(sessions[0].exit.as_ref().unwrap().message_count, 5);
        assert_eq!(sessions[1].session_id, "aaa");
    }

    #[tokio::test]
    async fn list_mtime_overrides_header_created_at_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());

        let mut w_old = store
            .create(
                &Entry::Header {
                    session_id: "aaa-old-header".to_owned(),
                    cwd: "/a".to_owned(),
                    model: "m".to_owned(),
                    created_at: datetime!(2026-01-01 10:00:00 UTC),
                    version: CURRENT_VERSION,
                    git_branch: None,
                },
                &[],
            )
            .unwrap();
        w_old.append(&[sample_title_entry("Old")]).unwrap();
        drop(w_old);

        let mut w_new = store
            .create(
                &Entry::Header {
                    session_id: "zzz-new-header".to_owned(),
                    cwd: "/z".to_owned(),
                    model: "m".to_owned(),
                    created_at: datetime!(2026-04-17 10:00:00 UTC),
                    version: CURRENT_VERSION,
                    git_branch: None,
                },
                &[],
            )
            .unwrap();
        w_new.append(&[sample_title_entry("New")]).unwrap();
        drop(w_new);

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
        let mut writer = store.create(&sample_header("retitled"), &[]).unwrap();
        writer
            .append(&[Entry::Title {
                title: "original prompt".to_owned(),
                source: TitleSource::FirstPrompt,
                updated_at: datetime!(2026-04-16 12:00:00 UTC),
            }])
            .unwrap();
        writer
            .append(&[Entry::Title {
                title: "AI generated".to_owned(),
                source: TitleSource::AiGenerated,
                updated_at: datetime!(2026-04-16 12:01:00 UTC),
            }])
            .unwrap();

        let sessions = store.list().unwrap();
        let title = sessions[0].title.as_ref().unwrap();
        assert_eq!(title.title, "AI generated");
    }

    #[tokio::test]
    async fn list_finds_ai_title_buried_between_head_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("buried"), &[]).unwrap();
        writer
            .append(&[Entry::Title {
                title: "first prompt".to_owned(),
                source: TitleSource::FirstPrompt,
                updated_at: datetime!(2026-04-16 12:00:00 UTC),
            }])
            .unwrap();
        writer
            .append(&[sample_message_entry(Uuid::new_v4(), "short user text")])
            .unwrap();
        writer
            .append(&[Entry::Title {
                title: "AI picked".to_owned(),
                source: TitleSource::AiGenerated,
                updated_at: datetime!(2026-04-16 12:00:05 UTC),
            }])
            .unwrap();
        let bulky_body = "x".repeat(16_000);
        writer
            .append(&[sample_message_entry(Uuid::new_v4(), &bulky_body)])
            .unwrap();
        writer.append(&[sample_summary_entry(2)]).unwrap();

        let sessions = store.list().unwrap();
        let title = sessions[0].title.as_ref().unwrap();
        assert_eq!(title.title, "AI picked");
        let exit = sessions[0].exit.as_ref().unwrap();
        assert_eq!(exit.message_count, 2);
    }

    #[tokio::test]
    async fn list_absent_title_and_exit_fields_are_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("bare"), &[]).unwrap();

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
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _own = store.create(&sample_header("own"), &[]).unwrap();

        let sibling_store =
            SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let _foreign = sibling_store
            .create(&sample_header("foreign"), &[])
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
        let _own = store.create(&sample_header("own"), &[]).unwrap();

        let foreign_store =
            SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let _foreign = foreign_store
            .create(&sample_header("foreign"), &[])
            .unwrap();
        drop(foreign_store);

        let all = store.list_all().unwrap();
        let mut ids: Vec<_> = all.iter().map(|s| s.session_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["foreign", "own"]);
    }

    // ── list_paged ──

    /// Builds `count` finished sessions in monotonically increasing mtime order.
    fn seed_n_sessions(store: &SessionStore, count: usize) {
        for i in 0..count {
            let id = format!("session-{i:04}");
            let offset = i64::try_from(i).expect("test count fits i64");
            let mut writer = store
                .create(
                    &Entry::Header {
                        session_id: id.clone(),
                        cwd: "/work/project".to_owned(),
                        model: "m".to_owned(),
                        created_at: datetime!(2026-04-15 10:00:00 UTC)
                            + time::Duration::seconds(offset),
                        version: CURRENT_VERSION,
                        git_branch: None,
                    },
                    &[],
                )
                .unwrap();
            writer
                .append(&[sample_title_entry(&format!("Title {i}"))])
                .unwrap();
        }
    }

    #[tokio::test]
    async fn list_paged_caps_at_limit_keeping_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        seed_n_sessions(&store, 10);

        let page = store.list_paged(Some(3), false).unwrap();
        assert_eq!(page.sessions().len(), 3, "cap honored");
        assert_eq!(page.total(), 10, "total reflects pre-cap count");
        // mtime descends in seed order; newest three are the last three created.
        assert_eq!(page.sessions()[0].session_id, "session-0009");
        assert_eq!(page.sessions()[2].session_id, "session-0007");
    }

    #[tokio::test]
    async fn list_paged_unbounded_returns_total_equal_to_len() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        seed_n_sessions(&store, 4);

        let page = store.list_paged(None, false).unwrap();
        assert_eq!(page.sessions().len(), 4);
        assert_eq!(page.total(), 4, "no cap → total equals returned len");
    }

    #[tokio::test]
    async fn list_paged_zero_limit_yields_no_rows_but_full_total() {
        // `Some(0)` means "parse no rows"; the CLI's `--limit 0` unbounded sentinel translates
        // to `None` upstream. Pin that the store layer faithfully applies the cap.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        seed_n_sessions(&store, 3);

        let page = store.list_paged(Some(0), false).unwrap();
        assert!(page.sessions().is_empty());
        assert_eq!(page.total(), 3, "total still reflects what's on disk");
    }

    // ── delete ──

    #[tokio::test]
    async fn delete_removes_the_session_file_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("doomed"), &[]).unwrap();
        let path = test_session_file(dir.path(), "doomed");
        assert!(path.exists(), "fixture path materialized: {path:?}");

        store.delete("doomed", "live-session-id").unwrap();
        assert!(!path.exists(), "delete must unlink the file: {path:?}");
        assert!(
            store.list().unwrap().is_empty(),
            "list reflects the removal"
        );
    }

    #[tokio::test]
    async fn delete_falls_through_to_other_project_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let foreign_store =
            SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let _w = foreign_store
            .create(&sample_header("foreign-doomed"), &[])
            .unwrap();
        drop(foreign_store);

        store.delete("foreign-doomed", "live-session-id").unwrap();
        let foreign_dir = dir.path().join("other-project");
        let remaining: Vec<_> = fs::read_dir(&foreign_dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            remaining.is_empty(),
            "foreign project dir empty: {remaining:?}"
        );
    }

    #[test]
    fn delete_refuses_when_id_matches_live_session() {
        // Defense in depth: the resume picker filters the live session id from rows, but a typed
        // `/delete <live-id>` would otherwise reach this method and unlink the open writer's file.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("live-id"), &[]).unwrap();
        let err = store
            .delete("live-id", "live-id")
            .expect_err("self-delete must be rejected");
        assert!(
            err.to_string()
                .contains("refusing to delete the live session"),
            "error must explain the refusal: {err:#}",
        );
        assert!(
            test_session_file(dir.path(), "live-id").exists(),
            "file untouched"
        );
    }

    #[test]
    fn delete_errors_when_session_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let err = store
            .delete("ghost", "live-id")
            .expect_err("missing session must error");
        assert!(
            err.to_string().contains("session not found"),
            "got: {err:#}"
        );
    }

    #[test]
    fn delete_rejects_invalid_session_ids() {
        // Path-traversal / NUL / oversize ids must hit `validate_session_id` before any disk op.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        for bogus in ["", "../etc/passwd", "session\0evil", &"a".repeat(65)] {
            let err = store
                .delete(bogus, "live-id")
                .expect_err("must reject `{bogus}`");
            assert!(
                err.to_string().contains("invalid session ID"),
                "id `{bogus}` must surface invalid-id error: {err:#}",
            );
        }
    }

    // ── find_session_path ──

    #[tokio::test]
    async fn find_session_path_falls_back_to_other_projects() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());

        let foreign_store =
            SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let _w = foreign_store
            .create(&sample_header("foreign"), &[])
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
        let mut writer = store.create(&sample_header("multi"), &[]).unwrap();

        writer
            .append(&[sample_message_entry(Uuid::new_v4(), "hello")])
            .unwrap();
        writer
            .append(&[sample_message_entry(Uuid::new_v4(), "world")])
            .unwrap();

        let content = fs::read_to_string(test_session_file(dir.path(), "multi")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 messages
    }

    #[test]
    fn append_terminates_a_line_torn_by_another_appender() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("torn"), &[]).unwrap();
        let first = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                first,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "first",
            )])
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(test_session_file(dir.path(), "torn"))
            .unwrap()
            .write_all(br#"{"type":"message","uuid":"#)
            .unwrap();

        let second = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                second,
                Some(first),
                datetime!(2026-04-16 12:00:02 UTC),
                "second",
            )])
            .unwrap();

        let data = store.load_session_data("torn").unwrap();
        assert_eq!(data.last_uuid, Some(second), "appended entry must parse");
        assert_eq!(data.messages.len(), 2);
    }

    // ── create_written ──

    #[test]
    fn create_written_unlinks_the_file_when_the_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doomed.jsonl");

        let err = create_written(&path, |_| Err(io::ErrorKind::StorageFull.into())).unwrap_err();

        assert!(
            format!("{err:#}").contains("failed to write"),
            "got: {err:#}"
        );
        assert!(!path.exists(), "a failed create must leave no file behind");
        create_written(&path, |_| Ok(())).unwrap();
    }

    // ── append_whole ──

    /// Writes through to `file` until `budget` bytes are spent, then returns what `exhausted` does.
    struct ShortWriter<'a> {
        file: &'a File,
        budget: usize,
        exhausted: fn(&File) -> io::Result<usize>,
    }

    impl Write for ShortWriter<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.budget == 0 {
                return (self.exhausted)(self.file);
            }
            let n = buf.len().min(self.budget);
            self.budget -= n;
            let mut file = self.file;
            file.write(&buf[..n])
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Creates a session holding one root message, returning its path, writer, and a two-line
    /// batch continuing the chain.
    fn session_with_pending_batch(
        store: &SessionStore,
        dir: &Path,
    ) -> (PathBuf, SessionWriter, Vec<u8>) {
        let root = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut writer = store.create(&sample_header("partial"), &[]).unwrap();
        writer
            .append(&[sample_message_at(
                root,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "root",
            )])
            .unwrap();
        let batch = encode_lines(&[
            sample_message_at(
                second,
                Some(root),
                datetime!(2026-04-16 12:00:02 UTC),
                "second",
            ),
            sample_message_at(
                Uuid::new_v4(),
                Some(second),
                datetime!(2026-04-16 12:00:03 UTC),
                "third",
            ),
        ])
        .unwrap();
        (test_session_file(dir, "partial"), writer, batch)
    }

    #[test]
    fn append_whole_truncates_a_partial_write_so_the_retry_adds_no_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let (path, mut writer, batch) = session_with_pending_batch(&store, dir.path());
        let first_line = batch.iter().position(|&byte| byte == b'\n').unwrap() + 1;
        let file = open_append_file(&path).unwrap();
        let mut out = ShortWriter {
            file: &file,
            budget: first_line + 5,
            exhausted: |_| Err(io::ErrorKind::StorageFull.into()),
        };

        let err = append_whole(&file, &mut out, &batch).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        writer.append_lines(&batch).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let unique: HashSet<&str> = content.lines().collect();
        assert_eq!(
            unique.len(),
            content.lines().count(),
            "duplicated line in: {content}"
        );
        let data = store.load_session_data("partial").unwrap();
        assert_eq!(
            data.messages,
            [
                Message::user("root"),
                Message::user("second"),
                Message::user("third"),
            ]
        );
    }

    #[test]
    fn append_whole_keeps_a_line_another_appender_wrote_after_the_partial_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let (path, _writer, batch) = session_with_pending_batch(&store, dir.path());
        let file = open_append_file(&path).unwrap();
        let mut out = ShortWriter {
            file: &file,
            budget: 5,
            exhausted: |mut file| {
                file.write_all(b"\nother appender\n").unwrap();
                Ok(0)
            },
        };

        let err = append_whole(&file, &mut out, &batch).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::WriteZero);
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.ends_with("\nother appender\n"), "got: {content}");
    }
}
