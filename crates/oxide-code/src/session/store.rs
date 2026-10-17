//! Session file I/O.

pub(crate) mod listing;
#[cfg(test)]
pub(crate) mod testing;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use time::OffsetDateTime;
use tracing::{debug, warn};
use uuid::Uuid;

use super::chain::{Chain, ChainBuilder};
use super::entry::{CURRENT_VERSION, CompactInfo, Entry, TitleInfo, UsageRecord};
use super::path::sanitize_cwd;
use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::prompt::SessionContext;
use crate::tool::ToolMetadata;
use crate::util::fs::create_private_dir_all;
use crate::util::path::{XdgBase, app_dir};

const SESSIONS_DIR: &str = "sessions";

// ── SessionStore ──

/// Low-level session file operations scoped to a project subdirectory.
#[derive(Clone)]
pub(crate) struct SessionStore {
    sessions_dir: PathBuf,
    project_dir: PathBuf,
    cwd: PathBuf,
}

impl SessionStore {
    /// Opens the store scoped to the project at `cwd`, which new session headers record.
    pub(crate) fn open(cwd: &Path) -> Result<Self> {
        let sessions_dir = app_dir(XdgBase::Data)
            .map(|dir| dir.join(SESSIONS_DIR))
            .context("cannot determine session storage directory")?;

        create_private_dir_all(&sessions_dir)?;

        let project_name = sanitize_cwd(cwd);
        let project_dir = sessions_dir.join(&project_name);
        create_private_dir_all(&project_dir)?;

        debug!(
            "session store at {} (project: {project_name})",
            sessions_dir.display()
        );
        Ok(Self {
            sessions_dir,
            project_dir,
            cwd: cwd.to_path_buf(),
        })
    }

    pub(super) fn cwd(&self) -> &Path {
        &self.cwd
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
        let file = create_written(&path, |file| Appender(file).write_all(&buf))?;
        Ok(SessionWriter {
            path,
            file: Some(file),
        })
    }
}

// ── Path-Keyed Primitives ──

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
    /// The last context entry and its timestamp, awaiting the message entry it belongs to.
    pending_context: Option<(OffsetDateTime, Box<SessionContext>)>,
    compact: Option<CompactInfo>,
    /// A compact boundary that takes effect only once its post-compact root follows it.
    pending_compact: Option<CompactInfo>,
    latest_title: Option<TitleInfo>,
    archived: bool,
    compact_tail_uuids: Option<HashSet<Uuid>>,
    compact_tail_accepts_sidecars: bool,
    tool_result_metadata: HashMap<String, ToolMetadata>,
    usage: Vec<UsageRecord>,
    usage_uuids: HashSet<Uuid>,
    file_snapshots: Vec<FileSnapshot>,
}

impl SessionDataBuilder {
    fn new() -> Self {
        Self {
            chain: ChainBuilder::new(),
            pending_context: None,
            compact: None,
            pending_compact: None,
            latest_title: None,
            archived: false,
            compact_tail_uuids: None,
            compact_tail_accepts_sidecars: true,
            tool_result_metadata: HashMap::new(),
            usage: Vec::new(),
            usage_uuids: HashSet::new(),
            file_snapshots: Vec::new(),
        }
    }

    fn apply_entry(&mut self, entry: Entry, line_no: u32) -> Result<()> {
        match entry {
            Entry::Header { version, .. } if version > CURRENT_VERSION => {
                bail!(
                    "session format version {version} is newer than supported ({CURRENT_VERSION}). \
                     Upgrade oxide-code to open it."
                );
            }
            Entry::Context { context, timestamp } => {
                self.pending_context = Some((timestamp, context));
            }
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
            Entry::Archive { archived, .. } => self.archived = archived,
            Entry::ToolResultMetadata {
                tool_use_id,
                metadata,
                ..
            } if self.accepts_sidecar() => {
                self.tool_result_metadata.insert(tool_use_id, metadata);
            }
            Entry::Usage { uuid, usage, .. } if self.usage_uuids.insert(uuid) => {
                self.usage.push(usage);
            }
            Entry::FileSnapshot { snapshot } if self.accepts_sidecar() => {
                self.file_snapshots.push(snapshot);
            }
            Entry::Compact {
                summary,
                pre_message_count,
                instructions,
                tasks,
                ..
            } => {
                self.pending_compact = Some(CompactInfo {
                    summary,
                    pre_message_count,
                    instructions,
                    tasks,
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
        // A context shares its message's timestamp, so a context stranded by a torn batch never
        // binds to the next writer's message.
        let context = self
            .pending_context
            .take()
            .and_then(|(at, context)| (at == timestamp).then_some(context));
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
        self.chain
            .insert(uuid, parent_uuid, message, timestamp, context);
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
        let Chain {
            messages,
            tip,
            context,
        } = self.chain.resolve();
        SessionData {
            messages,
            last_uuid: tip,
            context: context.map(|context| *context),
            compact: self.compact,
            title: self.latest_title,
            archived: self.archived,
            tool_result_metadata: self.tool_result_metadata,
            usage: self.usage,
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

/// Removes the session file at `path`. Refuses the live session's file, which callers already
/// filter out, so a bug upstream cannot unlink the file under its open writer.
pub(crate) fn delete_at(path: &Path, live_id: &str) -> Result<()> {
    let session_id = read_session_id_from_path(path)?;
    if session_id == live_id {
        bail!("refusing to delete the live session: {session_id}");
    }
    fs::remove_file(path)
        .with_context(|| format!("failed to remove session file {}", path.display()))
}

/// Sets the archive state of the session file at `path`. The file keeps its mtime, since listings
/// order sessions by activity and toggling the state is not activity.
pub(crate) fn set_archived_at(path: &Path, archived: bool) -> Result<()> {
    let mut writer = open_append_at(path)?;
    let modified = fs::metadata(path)
        .and_then(|meta| meta.modified())
        .with_context(|| format!("cannot stat {}", path.display()))?;
    writer.set_archived(archived)?;
    if let Some(file) = &writer.file
        && let Err(e) = file.set_modified(modified)
    {
        warn!(
            "archive toggle moved {} up the listing: {e}",
            path.display()
        );
    }
    Ok(())
}

const MAX_SESSION_ID_LEN: usize = 64;

fn validate_session_id(session_id: &str) -> Result<()> {
    let ok = !session_id.is_empty()
        && session_id.len() <= MAX_SESSION_ID_LEN
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

/// The session id in a `{ts}-{id}.jsonl` file name.
fn file_session_id(path: &Path) -> Option<&str> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    stem.split_once('-').map(|(_, id)| id)
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
        append_whole(&file, &mut Appender(&file), lines)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        self.file = Some(file);
        Ok(())
    }

    /// Appends an [`Entry::Archive`] setting the session's archive state.
    pub(super) fn set_archived(&mut self, archived: bool) -> Result<()> {
        let entry = Entry::Archive {
            archived,
            at: OffsetDateTime::now_utc(),
        };
        self.append_lines(&encode_lines([&entry])?)
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
    /// Prompt state recorded on the resolved chain, `None` for a transcript that predates
    /// recording or whose last compaction no user message followed.
    pub(crate) context: Option<SessionContext>,
    pub(crate) compact: Option<CompactInfo>,
    pub(crate) title: Option<TitleInfo>,
    pub(crate) archived: bool,
    pub(crate) tool_result_metadata: HashMap<String, ToolMetadata>,
    /// Every billed request in the file, on any branch and across compactions.
    pub(crate) usage: Vec<UsageRecord>,
    pub(crate) file_snapshots: Vec<FileSnapshot>,
}

// ── File Opening ──

/// Options for a readable handle that [`Appender`] writes through. On Windows, `append(true)`
/// strips the `FILE_WRITE_DATA` access that `set_len` needs to roll back a partial write, so
/// the handle opens for plain writing there and each write targets the end of the file.
fn append_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.write(true);
    #[cfg(not(windows))]
    options.append(true);
    options
}

fn open_create_exclusive(path: &Path) -> io::Result<File> {
    let mut options = append_options();
    options.create_new(true);
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
    append_options()
        .open(path)
        .with_context(|| format!("session not found: {}", path.display()))
}

/// Writes at the end of a file opened by [`append_options`], atomically with other appenders.
struct Appender<'a>(&'a File);

impl Write for Appender<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // An offset of `u64::MAX` is `FILE_WRITE_TO_END_OF_FILE`, the per-write form of append.
        #[cfg(windows)]
        {
            std::os::windows::fs::FileExt::seek_write(self.0, buf, u64::MAX)
        }
        #[cfg(not(windows))]
        {
            let mut file = self.0;
            file.write(buf)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
        git_branch,
    })
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use time::macros::datetime;

    use super::testing::{
        TEST_PROJECT, sample_header, sample_message_at, sample_message_entry, sample_summary_entry,
        sample_title_entry, test_session_file, test_store,
    };
    use super::*;
    use crate::config::PromptCacheTtl;
    use crate::file_tracker::{FileState, LastView};
    use crate::message::ContentBlock;
    use crate::prompt::PromptParts;
    use crate::tool::tasks::testing::plan;
    use crate::util::path::testing::HOME_VAR;

    fn test_project_path(dir: &Path, filename: &str) -> PathBuf {
        dir.join(TEST_PROJECT).join(filename)
    }

    fn session_context(date: &str) -> SessionContext {
        let mut prompt = PromptParts::default();
        date.clone_into(&mut prompt.seen.date);
        prompt.session_context()
    }

    fn sample_context(date: &str, timestamp: OffsetDateTime) -> Entry {
        Entry::Context {
            context: Box::new(session_context(date)),
            timestamp,
        }
    }

    /// A user message recorded with the context dated `date`, as the actor writes one.
    fn message_with_context(
        date: &str,
        uuid: Uuid,
        parent_uuid: Option<Uuid>,
        timestamp: OffsetDateTime,
        text: &str,
    ) -> [Entry; 2] {
        [
            sample_context(date, timestamp),
            sample_message_at(uuid, parent_uuid, timestamp, text),
        ]
    }

    fn sample_snapshot(path: &str) -> Entry {
        Entry::FileSnapshot {
            snapshot: FileSnapshot {
                path: PathBuf::from(path),
                state: FileState {
                    content_hash: 42,
                    last_view: LastView::Full,
                    recorded_at: datetime!(2026-04-16 12:00:01 UTC),
                    tool_use_id: None,
                },
            },
        }
    }

    // ── SessionStore::open ──

    async fn open_in_isolated_env(xdg: &Path) -> SessionStore {
        let home = tempfile::tempdir().unwrap();
        temp_env::async_with_vars(
            [
                ("XDG_DATA_HOME", Some(xdg.to_string_lossy().into_owned())),
                (HOME_VAR, Some(home.path().to_string_lossy().into_owned())),
            ],
            async { SessionStore::open(Path::new("/work/proj")) },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn open_uses_xdg_data_home_when_set() {
        let xdg = tempfile::tempdir().unwrap();
        let store = open_in_isolated_env(xdg.path()).await;
        let sessions = xdg.path().join("ox").join(SESSIONS_DIR);
        assert_eq!(store.sessions_dir, sessions);
        assert_eq!(
            store.project_dir,
            sessions.join(sanitize_cwd(Path::new("/work/proj")))
        );
        assert!(store.project_dir.is_dir(), "project dir created");
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
                (HOME_VAR, Some(home.path().to_string_lossy().into_owned())),
            ],
            async { SessionStore::open(Path::new("/work/proj")) },
        )
        .await
        .unwrap();
        let expected = home
            .path()
            .join(".local/share")
            .join("ox")
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

    // ── open_append_at ──

    #[tokio::test]
    async fn open_append_at_writes_to_existing_file() {
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

        let mut writer = open_append_at(&test_session_file(dir.path(), "append-test")).unwrap();
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
    fn open_append_at_starts_an_empty_file_without_a_blank_line() {
        let dir = tempfile::tempdir().unwrap();
        let _store = test_store(dir.path());
        let path = test_project_path(
            dir.path(),
            &session_filename("empty", datetime!(2026-04-16 12:00:00 UTC)),
        );
        fs::write(&path, "").unwrap();

        open_append_at(&path)
            .unwrap()
            .append(&[sample_summary_entry(0)])
            .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.starts_with(r#"{"type":"summary""#),
            "got: {content}"
        );
    }

    #[test]
    fn open_append_at_fails_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_append_at(&dir.path().join("ghost.jsonl"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("session not found"), "got: {err}");
    }

    #[tokio::test]
    async fn open_append_at_allows_concurrent_resumes_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer_a = store.create(&sample_header("concurrent"), &[]).unwrap();

        let start = std::time::Instant::now();
        let writer_b = open_append_at(&test_session_file(dir.path(), "concurrent"));
        let elapsed = start.elapsed();

        assert!(
            writer_b.is_ok(),
            "second resume should succeed immediately, got {writer_b:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "open_append_at should not block on a concurrent writer: {elapsed:?}"
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
                tasks: Vec::new(),
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

        let first = Uuid::new_v4();
        writer
            .append(&message_with_context(
                "2026-04-16",
                first,
                None,
                datetime!(2026-04-16 12:00:01 UTC),
                "first",
            ))
            .unwrap();
        writer
            .append(&message_with_context(
                "2026-04-17",
                Uuid::new_v4(),
                Some(first),
                datetime!(2026-04-17 12:00:00 UTC),
                "second",
            ))
            .unwrap();
        assert_eq!(load(), Some(session_context("2026-04-17")));

        writer
            .append(&[Entry::Compact {
                summary: "summary".to_owned(),
                pre_message_count: 1,
                instructions: None,
                tasks: Vec::new(),
                timestamp: datetime!(2026-04-17 12:00:00 UTC),
            }])
            .unwrap();
        assert_eq!(
            load(),
            Some(session_context("2026-04-17")),
            "a compact without its post-compact root keeps the old context",
        );

        let summary = Uuid::new_v4();
        writer
            .append(&[sample_message_at(
                summary,
                None,
                datetime!(2026-04-17 12:00:01 UTC),
                "summary",
            )])
            .unwrap();
        assert_eq!(load(), None, "a compacted transcript no longer carries it");

        writer
            .append(&message_with_context(
                "2026-04-18",
                Uuid::new_v4(),
                Some(summary),
                datetime!(2026-04-18 12:00:00 UTC),
                "after compact",
            ))
            .unwrap();
        assert_eq!(load(), Some(session_context("2026-04-18")));
    }

    #[test]
    fn load_session_data_drops_a_context_whose_message_was_torn_off() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("torn-context"), &[]).unwrap();
        let root = Uuid::new_v4();
        writer
            .append(&message_with_context(
                "2026-04-16",
                root,
                None,
                datetime!(2026-04-16 12:00:00 UTC),
                "root",
            ))
            .unwrap();
        let mut torn = encode_lines(&[sample_context(
            "2026-04-17",
            datetime!(2026-04-17 12:00:00 UTC),
        )])
        .unwrap();
        torn.extend_from_slice(br#"{"type":"message","uuid":"#);
        writer.append_lines(&torn).unwrap();

        writer
            .append(&[sample_message_at(
                Uuid::new_v4(),
                Some(root),
                datetime!(2026-04-17 12:00:01 UTC),
                "another writer's message",
            )])
            .unwrap();

        let data = store.load_session_data("torn-context").unwrap();
        assert_eq!(data.messages.len(), 2);
        assert_eq!(data.context, Some(session_context("2026-04-16")));
    }

    #[test]
    fn load_session_data_drops_the_context_of_a_rejected_pre_compact_append() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store
            .create(&sample_header("rejected-context"), &[])
            .unwrap();
        let pre = Uuid::new_v4();
        let synthetic = Uuid::new_v4();
        writer
            .append(&[
                sample_message_at(pre, None, datetime!(2026-04-16 12:00:01 UTC), "pre"),
                Entry::Compact {
                    summary: "summary".to_owned(),
                    pre_message_count: 1,
                    instructions: None,
                    tasks: Vec::new(),
                    timestamp: datetime!(2026-04-16 12:00:02 UTC),
                },
                sample_message_at(
                    synthetic,
                    None,
                    datetime!(2026-04-16 12:00:02 UTC),
                    "summary",
                ),
            ])
            .unwrap();

        let at = datetime!(2026-04-16 12:00:05 UTC);
        writer
            .append(&message_with_context(
                "2026-04-16",
                Uuid::new_v4(),
                Some(pre),
                at,
                "stale old-chain append",
            ))
            .unwrap();
        writer
            .append(&[sample_message_at(
                Uuid::new_v4(),
                Some(synthetic),
                at,
                "post-compact",
            )])
            .unwrap();

        let data = store.load_session_data("rejected-context").unwrap();
        assert_eq!(data.messages.len(), 2);
        assert_eq!(
            data.context, None,
            "the stale append's context must not bind to the next message",
        );
    }

    #[test]
    fn load_session_data_replays_the_context_recorded_on_the_resolved_branch() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("fork-context"), &[]).unwrap();
        let load = || store.load_session_data("fork-context").unwrap().context;

        let root = Uuid::new_v4();
        let branch_a = Uuid::new_v4();
        writer
            .append(
                &[
                    message_with_context(
                        "2026-04-16",
                        root,
                        None,
                        datetime!(2026-04-16 12:00:01 UTC),
                        "root",
                    ),
                    message_with_context(
                        "2026-04-17",
                        branch_a,
                        Some(root),
                        datetime!(2026-04-17 12:00:00 UTC),
                        "branch A",
                    ),
                    message_with_context(
                        "2026-04-18",
                        Uuid::new_v4(),
                        Some(root),
                        datetime!(2026-04-18 12:00:00 UTC),
                        "branch B",
                    ),
                ]
                .concat(),
            )
            .unwrap();
        assert_eq!(load(), Some(session_context("2026-04-18")));

        writer
            .append(&[sample_message_at(
                Uuid::new_v4(),
                Some(branch_a),
                datetime!(2026-04-19 12:00:00 UTC),
                "branch A follow-up",
            )])
            .unwrap();
        assert_eq!(
            load(),
            Some(session_context("2026-04-17")),
            "the tip on branch A replays the context branch A recorded",
        );
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
                tasks: plan(),
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
                tasks: plan(),
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
            tasks: Vec::new(),
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
                tasks: Vec::new(),
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
    fn load_session_data_collects_usage_once_on_every_branch_and_across_compactions() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let created_at = datetime!(2026-01-01 00:00:00 UTC);
        let path = test_project_path(dir.path(), &session_filename("billed", created_at));
        fs::write(
            &path,
            indoc! {r#"
                {"type":"header","session_id":"billed","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}
                {"type":"message","uuid":"00000000-0000-4000-8000-000000000001","message":{"role":"user","content":[{"type":"text","text":"prompt"}]},"timestamp":"2026-01-01T00:00:01Z"}
                {"type":"message","uuid":"00000000-0000-4000-8000-000000000002","parent_uuid":"00000000-0000-4000-8000-000000000001","message":{"role":"assistant","content":[{"type":"text","text":"abandoned"}]},"timestamp":"2026-01-01T00:00:02Z"}
                {"type":"usage","uuid":"00000000-0000-4000-8000-0000000000a1","model":"claude-opus-5-5","cache_ttl":"5m","input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":10,"timestamp":"2026-01-01T00:00:02Z"}
                {"type":"message","uuid":"00000000-0000-4000-8000-000000000003","parent_uuid":"00000000-0000-4000-8000-000000000001","message":{"role":"assistant","content":[{"type":"text","text":"kept"}]},"timestamp":"2026-01-01T00:00:03Z"}
                {"type":"usage","uuid":"00000000-0000-4000-8000-0000000000a2","model":"claude-sonnet-5-5","cache_ttl":"1h","input_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":20,"timestamp":"2026-01-01T00:00:03Z"}
                {"type":"compact","summary":"summary","pre_message_count":2,"timestamp":"2026-01-01T00:00:04Z"}
                {"type":"message","uuid":"00000000-0000-4000-8000-000000000004","message":{"role":"user","content":[{"type":"text","text":"summary"}]},"timestamp":"2026-01-01T00:00:04Z"}
                {"type":"usage","uuid":"00000000-0000-4000-8000-0000000000a3","model":"claude-opus-5-5","cache_ttl":"5m","input_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":30,"timestamp":"2026-01-01T00:00:05Z"}
                {"type":"usage","uuid":"00000000-0000-4000-8000-0000000000a3","model":"claude-opus-5-5","cache_ttl":"5m","input_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":30,"timestamp":"2026-01-01T00:00:05Z"}
            "#},
        )
        .unwrap();

        let data = store.load_session_data("billed").unwrap();
        assert_eq!(data.messages, [Message::user("summary")]);
        let usage: Vec<_> = data
            .usage
            .iter()
            .map(|record| {
                (
                    record.model.as_str(),
                    record.cache_ttl,
                    record.tokens.input_tokens,
                )
            })
            .collect();
        assert_eq!(
            usage,
            [
                ("claude-opus-5-5", PromptCacheTtl::FiveMin, 1),
                ("claude-sonnet-5-5", PromptCacheTtl::OneHour, 2),
                ("claude-opus-5-5", PromptCacheTtl::FiveMin, 3),
            ]
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
    fn load_session_data_nonexistent_session_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_session_data_from_path(&dir.path().join("ghost.jsonl"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("session not found"), "got: {err}");
    }

    #[tokio::test]
    async fn load_session_data_resolves_transcript_through_newest_chain_leaf() {
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
        let content = indoc! {r#"
            {"type":"header","session_id":"nonewline","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}
            {"type":"message","uuid":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","message":{"role":"user","content":[{"type":"text","text":"ok"}]},"timestamp":"2026-01-01T00:00:01Z"}"#};
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

    // ── delete_at ──

    #[test]
    fn delete_at_removes_the_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("doomed"), &[]).unwrap();
        let path = test_session_file(dir.path(), "doomed");

        delete_at(&path, "live-session-id").unwrap();

        assert!(!path.exists(), "delete must unlink the file: {path:?}");
        assert!(
            store.list().unwrap().is_empty(),
            "list reflects the removal"
        );
    }

    #[test]
    fn delete_at_refuses_the_live_session_file() {
        // Defense in depth: the resume picker filters the live session from its rows, but a bug
        // upstream would otherwise unlink the open writer's file.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("live-id"), &[]).unwrap();
        let path = test_session_file(dir.path(), "live-id");

        let err = delete_at(&path, "live-id").expect_err("self-delete must be rejected");

        assert!(
            err.to_string()
                .contains("refusing to delete the live session"),
            "error must explain the refusal: {err:#}",
        );
        assert!(path.exists(), "file untouched");
    }

    #[test]
    fn delete_at_errors_when_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let err = delete_at(&dir.path().join("ghost.jsonl"), "live-id")
            .expect_err("missing session must error");
        assert!(
            err.to_string().contains("session not found"),
            "got: {err:#}"
        );
    }

    // ── set_archived_at ──

    #[test]
    fn set_archived_at_latest_entry_wins_keeping_the_transcript_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("shelved"), &[]).unwrap();
        writer
            .append(&[sample_message_entry(Uuid::new_v4(), "hello")])
            .unwrap();
        drop(writer);
        let path = test_session_file(dir.path(), "shelved");
        let last_active = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        open_append_file(&path)
            .unwrap()
            .set_modified(last_active)
            .unwrap();
        let modified = || fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!load_session_data_from_path(&path).unwrap().archived);

        set_archived_at(&path, true).unwrap();
        let data = load_session_data_from_path(&path).unwrap();
        assert!(data.archived);
        assert_eq!(data.messages, [Message::user("hello")]);
        assert_eq!(modified(), last_active, "archiving is not activity");

        set_archived_at(&path, false).unwrap();
        assert!(!load_session_data_from_path(&path).unwrap().archived);
        assert_eq!(modified(), last_active, "unarchiving is not activity");
    }

    #[test]
    fn set_archived_at_missing_file_errors_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghost.jsonl");
        let err = set_archived_at(&path, true).expect_err("missing session must error");
        assert!(
            err.to_string().contains("session not found"),
            "got: {err:#}"
        );
        assert!(!path.exists());
    }

    // ── append ──

    #[tokio::test]
    async fn append_writes_multiple_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut writer = store.create(&sample_header("multi"), &[]).unwrap();
        let (hello, world) = (Uuid::new_v4(), Uuid::new_v4());

        writer
            .append(&[
                sample_message_entry(hello, "hello"),
                sample_message_entry(world, "world"),
            ])
            .unwrap();

        let content = fs::read_to_string(test_session_file(dir.path(), "multi")).unwrap();
        let entries: Vec<Entry> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            matches!(
                entries.as_slice(),
                [
                    Entry::Header { .. },
                    Entry::Message { uuid: first, .. },
                    Entry::Message { uuid: second, .. },
                ] if *first == hello && *second == world
            ),
            "got: {entries:?}"
        );
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

    // ── Appender ──

    #[test]
    fn appender_writes_after_lines_another_handle_appended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.jsonl");
        let first = create_written(&path, |file| Appender(file).write_all(b"1\n")).unwrap();
        let second = open_append_file(&path).unwrap();

        Appender(&second).write_all(b"2\n").unwrap();
        let mut appender = Appender(&first);
        appender.write_all(b"3\n").unwrap();
        appender.flush().unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "1\n2\n3\n");
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
            Appender(self.file).write(&buf[..n])
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
            exhausted: |file| {
                Appender(file).write_all(b"\nother appender\n").unwrap();
                Ok(0)
            },
        };

        let err = append_whole(&file, &mut out, &batch).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::WriteZero);
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.ends_with("\nother appender\n"), "got: {content}");
    }
}
