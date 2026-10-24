//! Session file I/O.

pub mod listing;
#[cfg(test)]
pub(crate) mod local_testing;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use tempfile::NamedTempFile;
use time::OffsetDateTime;
use tracing::{debug, warn};
use uuid::Uuid;

use super::chain::{Chain, ChainBuilder};
use super::entry::{
    CURRENT_VERSION, CompactInfo, Entry, Header, TitleInfo, UsageRecord, keep_newer,
};
use super::path::project_dir_name;
use super::post_compact::restore_post_compact_origin;
use super::state::extract_user_text;
use crate::file_tracker::FileSnapshot;
use crate::message::{ContentBlock, Message, TextOrigin};
use crate::prompt::SessionContext;
use crate::tool::ToolMetadata;
use crate::util::fs::create_private_dir_all;
use crate::util::path::{XdgBase, app_dir};

const SESSIONS_DIR: &str = "sessions";

// ── SessionStore ──

/// Low-level session file operations scoped to one project, the Git repository root of the launch
/// directory or the launch directory itself outside a repository.
#[derive(Clone)]
pub struct SessionStore {
    sessions_dir: PathBuf,
    /// Where new sessions go, named for the project root.
    project_dir: PathBuf,
    /// Where releases up to 0.1.0-alpha.5 put sessions started in the launch directory, named for
    /// it, when that differs from `project_dir`.
    cwd_dir: Option<PathBuf>,
    project_root: PathBuf,
    project: Project,
}

/// A launch directory and its project root, as session headers record them.
#[derive(Clone)]
pub(super) struct Project {
    pub(super) cwd: String,
    pub(super) root: String,
}

impl Project {
    /// Returns whether a session whose header records `cwd` and `project_root` belongs here: one
    /// recorded in this project, one started in this launch directory, or an older session that
    /// records no project and started at the project root.
    fn owns(&self, cwd: &str, project_root: Option<&str>) -> bool {
        cwd == self.cwd || project_root.map_or(cwd == self.root, |root| root == self.root)
    }
}

impl SessionStore {
    /// Opens the store for the project at `project_root`, launched in `cwd`. New session headers
    /// record both.
    pub fn open(cwd: &Path, project_root: &Path) -> Result<Self> {
        let sessions_dir = app_dir(XdgBase::Data)
            .map(|dir| dir.join(SESSIONS_DIR))
            .context("failed to determine the session storage directory")?;

        create_private_dir_all(&sessions_dir)?;

        let project_name = project_dir_name(project_root);
        let project_dir = sessions_dir.join(&project_name);
        create_private_dir_all(&project_dir)?;
        let cwd_dir =
            Some(sessions_dir.join(project_dir_name(cwd))).filter(|dir| *dir != project_dir);

        debug!(
            "session store at {} (project: {project_name})",
            sessions_dir.display()
        );
        Ok(Self {
            sessions_dir,
            project_dir,
            cwd_dir,
            project_root: project_root.to_path_buf(),
            project: Project {
                cwd: cwd.display().to_string(),
                root: project_root.display().to_string(),
            },
        })
    }

    pub(crate) fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub(super) fn project(&self) -> &Project {
        &self.project
    }

    /// Where [`Self::create`] puts the file of session `session_id` created at `created_at`.
    pub(super) fn session_file(&self, session_id: &str, created_at: OffsetDateTime) -> PathBuf {
        self.project_dir
            .join(session_filename(session_id, created_at))
    }

    /// Publishes the header and JSONL `lines` as a new file (`0600` on Unix), without replacing
    /// an existing session.
    pub(crate) fn create(&self, header: &Header, lines: &[u8]) -> Result<SessionWriter> {
        validate_session_id(&header.session_id)?;

        let mut buf = encode_lines([&Entry::Header(header.clone())])?;
        buf.extend_from_slice(lines);
        let path = self.session_file(&header.session_id, header.created_at);
        create_written(&path, |mut file| file.write_all(&buf))?;
        Ok(SessionWriter { path })
    }
}

// ── Path-Keyed Primitives ──

/// Opens an existing session file in append mode by explicit path.
pub(super) fn open_append_at(path: &Path) -> Result<SessionWriter> {
    open_append_file(path)?;
    Ok(SessionWriter {
        path: path.to_owned(),
    })
}

/// Loads session data from an explicit path with DAG-based chain resolution. The file must begin
/// with a header in a format version this build reads.
pub(super) fn load_session_data_from_path(path: &Path) -> Result<SessionData> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut lines = JsonlLines::new(BufReader::new(file));
    let header = read_header(&mut lines, path)?;
    if header.version > CURRENT_VERSION {
        bail!(
            "session format version {} is newer than supported ({CURRENT_VERSION}). \
             Upgrade oxide-code to open it.",
            header.version,
        );
    }

    let mut builder = SessionDataBuilder::default();
    while let Some((line_no, line)) = lines.next_line()? {
        match serde_json::from_str(line) {
            Ok(entry) => builder.apply_entry(entry, line_no),
            Err(e) => warn!("skipping malformed entry at line {line_no}: {e}"),
        }
    }
    Ok(builder.finish(header.session_id))
}

/// Reads just the `session_id` from a session file's header line.
pub(super) fn read_session_id_from_path(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let header = read_header(&mut JsonlLines::new(BufReader::new(file)), path)?;
    Ok(header.session_id)
}

/// The directory beside `session_file` that holds the artifacts of session `session_id`, such as
/// spilled tool output. It exists only once the session writes an artifact, and [`delete_at`]
/// removes it with the file.
pub(crate) fn artifact_dir(session_file: &Path, session_id: &str) -> PathBuf {
    session_file.with_file_name(session_id)
}

/// Removes the session file at `path` and the session's [`artifact_dir`], the directory first so a
/// failure leaves the session listed for another try.
pub(crate) fn delete_at(path: &Path) -> Result<()> {
    let session_id = read_session_id_from_path(path)?;
    let artifact_dir = artifact_dir(path, &session_id);
    if let Err(e) = fs::remove_dir_all(&artifact_dir)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(e).with_context(|| {
            format!(
                "failed to remove session directory {}",
                artifact_dir.display()
            )
        });
    }
    fs::remove_file(path)
        .with_context(|| format!("failed to remove session file {}", path.display()))
}

/// Sets the archive state of the session file at `path`. The file keeps its mtime, since listings
/// order sessions by activity and toggling the state is not activity.
pub(super) fn set_archived_at(path: &Path, archived: bool) -> Result<()> {
    let _lock = lock_writer(path)?;
    let file = open_append_file(path)?;
    let modified = file
        .metadata()
        .and_then(|meta| meta.modified())
        .with_context(|| format!("failed to stat {}", path.display()))?;
    let lines = archive_lines(archived)?;
    let result = append_whole(&file, &mut Appender(&file), &lines)
        .with_context(|| format!("failed to write {}", path.display()));
    if let Err(e) = file.set_modified(modified) {
        warn!(
            "archive toggle moved {} up the listing: {e}",
            path.display()
        );
    }
    result
}

const MAX_SESSION_ID_LEN: usize = 64;

fn validate_session_id(session_id: &str) -> Result<()> {
    let ok = !session_id.is_empty()
        && session_id.len() <= MAX_SESSION_ID_LEN
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        bail!("invalid session ID {session_id:?}");
    }
    Ok(())
}

fn session_filename(session_id: &str, created_at: OffsetDateTime) -> String {
    format!("{}-{session_id}.jsonl", created_at.unix_timestamp())
}

/// The session ID in a `{ts}-{id}.jsonl` file name.
fn file_session_id(path: &Path) -> Option<&str> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    stem.split_once('-').map(|(_, id)| id)
}

// ── SessionWriter ──

/// The path reopened for each append to one session file.
#[derive(Debug)]
pub(crate) struct SessionWriter {
    path: PathBuf,
}

impl SessionWriter {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Appends JSONL `lines` while owning the session directory's writer lock.
    pub(super) fn append_lines(&mut self, lines: &[u8]) -> Result<()> {
        let _lock = lock_writer(&self.path)?;
        let file = open_append_file(&self.path)?;
        append_whole(&file, &mut Appender(&file), lines)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    /// Appends an [`Entry::Archive`] setting the session's archive state.
    pub(super) fn set_archived(&mut self, archived: bool) -> Result<()> {
        self.append_lines(&archive_lines(archived)?)
    }
}

fn archive_lines(archived: bool) -> Result<Vec<u8>> {
    encode_lines([&Entry::Archive {
        archived,
        at: OffsetDateTime::now_utc(),
    }])
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
    /// The ID the file's header records.
    pub(crate) session_id: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) last_uuid: Option<Uuid>,
    /// Prompt state recorded on the resolved chain, `None` for a transcript that predates recording
    /// or whose last compaction no user message followed.
    pub(crate) context: Option<SessionContext>,
    pub(crate) compact: Option<CompactInfo>,
    pub(crate) title: Option<TitleInfo>,
    pub(crate) first_user_prompt_seen: bool,
    pub(crate) archived: bool,
    pub(crate) tool_result_metadata: HashMap<String, ToolMetadata>,
    /// Every billed request in the file, on any branch and across compactions.
    pub(crate) usage: Vec<UsageRecord>,
    pub(crate) file_snapshots: Vec<FileSnapshot>,
}

// ── Loader ──

/// The messages of a compaction's tail, the chain since its boundary.
struct CompactTail {
    uuids: HashSet<Uuid>,
    /// Whether the last message joined the tail, so the sidecars that follow it belong to it.
    accepts_sidecars: bool,
}

#[derive(Default)]
struct SessionDataBuilder {
    chain: ChainBuilder,
    /// The last context entry and its timestamp, awaiting the message entry it belongs to.
    pending_context: Option<(OffsetDateTime, Box<SessionContext>)>,
    compact: Option<CompactInfo>,
    /// A compact boundary that takes effect only once its post-compact root follows it.
    pending_compact: Option<CompactInfo>,
    latest_title: Option<TitleInfo>,
    first_user_prompt_seen: Option<bool>,
    archived: bool,
    /// Set once a compaction takes effect.
    compact_tail: Option<CompactTail>,
    tool_result_metadata: HashMap<String, ToolMetadata>,
    usage: Vec<UsageRecord>,
    usage_uuids: HashSet<Uuid>,
    file_snapshots: Vec<FileSnapshot>,
}

impl SessionDataBuilder {
    fn apply_entry(&mut self, entry: Entry, line_no: u32) {
        match entry {
            Entry::Context { context, timestamp } => {
                self.pending_context = Some((timestamp, context));
            }
            Entry::Message {
                uuid,
                parent_uuid,
                message,
                timestamp,
            } => self.insert_message(uuid, parent_uuid, message, timestamp, line_no),
            Entry::Rewind {
                tip,
                first_user_prompt_seen,
                ..
            } => {
                if self.chain.rewind(tip) {
                    self.first_user_prompt_seen = first_user_prompt_seen;
                }
            }
            Entry::Title(title) => {
                keep_newer(&mut self.latest_title, title);
                self.first_user_prompt_seen = Some(true);
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
    }

    fn insert_message(
        &mut self,
        uuid: Uuid,
        parent_uuid: Option<Uuid>,
        mut message: Message,
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
                restore_post_compact_origin(&mut message);
            } else {
                warn!("skipping compact boundary without its post-compact root at line {line_no}");
            }
        }
        let accepted = self.accepts_compact_tail_message(uuid, parent_uuid);
        if let Some(tail) = &mut self.compact_tail {
            tail.accepts_sidecars = accepted;
        }
        if !accepted {
            warn!("skipping pre-compact branch append at line {line_no}");
            return;
        }
        if parent_uuid.is_none() {
            self.clear_sidecars();
        }
        if extract_user_text(&message).is_some() {
            self.first_user_prompt_seen = Some(true);
        }
        self.chain
            .insert(uuid, parent_uuid, message, timestamp, context);
    }

    fn accepts_compact_tail_message(&mut self, uuid: Uuid, parent_uuid: Option<Uuid>) -> bool {
        let Some(CompactTail { uuids, .. }) = &mut self.compact_tail else {
            return true;
        };
        let accepted = match parent_uuid {
            None => uuids.is_empty(),
            Some(parent) => uuids.contains(&parent),
        };
        if accepted {
            uuids.insert(uuid);
        }
        accepted
    }

    fn accepts_sidecar(&self) -> bool {
        self.compact_tail
            .as_ref()
            .is_none_or(|tail| tail.accepts_sidecars)
    }

    fn reset_at_compact(&mut self, compact: CompactInfo) {
        self.chain = ChainBuilder::default();
        self.compact = Some(compact);
        self.first_user_prompt_seen = Some(true);
        self.compact_tail = Some(CompactTail {
            uuids: HashSet::new(),
            accepts_sidecars: false,
        });
        self.clear_sidecars();
    }

    fn clear_sidecars(&mut self) {
        self.tool_result_metadata.clear();
        self.file_snapshots.clear();
    }

    fn finish(self, session_id: String) -> SessionData {
        let Chain {
            messages,
            tip,
            context,
        } = self.chain.resolve();
        // A later writer can extend a user branch that an empty rewind abandoned.
        let first_user_prompt_seen = messages.iter().any(|m| extract_user_text(m).is_some())
            || messages.first().is_some_and(|message| {
                matches!(
                    message.content.first(),
                    Some(ContentBlock::Text {
                        origin: Some(TextOrigin::CompactionSummary),
                        ..
                    })
                )
            })
            || self
                .first_user_prompt_seen
                .unwrap_or(self.latest_title.is_some());
        SessionData {
            session_id,
            messages,
            last_uuid: tip,
            context: context.map(|context| *context),
            compact: self.compact,
            title: self.latest_title,
            first_user_prompt_seen,
            archived: self.archived,
            tool_result_metadata: self.tool_result_metadata,
            usage: self.usage,
            file_snapshots: self.file_snapshots,
        }
    }
}

// ── File Opening ──

/// Options for a readable handle that [`Appender`] writes through. On Windows, `append(true)`
/// strips the `FILE_WRITE_DATA` access that `set_len` needs to roll back a partial write, so the
/// handle opens for plain writing there and each write targets the end of the file.
fn append_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.write(true);
    #[cfg(not(windows))]
    options.append(true);
    options
}

/// Writes privately beside `path`, then publishes without replacing an existing file.
fn create_written(path: &Path, write: impl FnOnce(&File) -> io::Result<()>) -> Result<()> {
    let file = NamedTempFile::new_in(path.parent().context("session file has no parent")?)
        .with_context(|| format!("failed to create {}", path.display()))?;
    write(file.as_file()).with_context(|| format!("failed to write {}", path.display()))?;
    file.persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
}

fn open_append_file(path: &Path) -> Result<File> {
    append_options()
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))
}

/// The directory's lock file stays in place across session deletion and recreation. Keeping the
/// lock off the transcript lets Windows readers open it during a batch.
fn open_writer_lock(path: &Path) -> Result<File> {
    let path = path
        .canonicalize()
        .with_context(|| format!("failed to open {}", path.display()))?;
    let path = path
        .parent()
        .context("session file has no parent")?
        .join(".writer.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))
}

fn lock_writer(path: &Path) -> Result<File> {
    let lock = open_writer_lock(path)?;
    lock.lock()
        .with_context(|| format!("failed to lock writer for {}", path.display()))?;
    Ok(lock)
}

/// Writes at the end of a file opened by [`append_options`].
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

/// Terminates a torn last line and writes `lines` through `out`, a handle on `file`. The caller
/// holds the directory's writer lock through this operation, including failed-write rollback.
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
        if let Err(e) = file.set_len(start) {
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

    /// Reads the next non-empty line with its 1-based line number, or `None` at EOF.
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

/// Parses the first non-empty line as the session header and validates its ID.
fn read_header<R: BufRead>(lines: &mut JsonlLines<R>, path: &Path) -> Result<Header> {
    let not_a_session = |reason| {
        format!(
            "{} is not an oxide-code session file: {reason}",
            path.display()
        )
    };
    let no_header = || not_a_session("its first line is not a session header");
    let Some((_, line)) = lines.next_line()? else {
        bail!(not_a_session("it is empty"));
    };
    let Entry::Header(header) = serde_json::from_str(line).with_context(no_header)? else {
        bail!(no_header());
    };
    validate_session_id(&header.session_id)?;
    Ok(header)
}

#[cfg(test)]
mod tests {
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    use indoc::indoc;
    use time::macros::datetime;

    use super::local_testing::{
        sample_header, sample_message_at, sample_message_entry, sample_summary_entry,
        sample_title_entry, test_session_file,
    };
    use super::testing::{TEST_PROJECT, WriterBarrier, isolated_store, test_store};
    use super::*;
    use crate::file_tracker::{FileState, LastView};
    use crate::message::{ContentBlock, Role};
    use crate::model::PromptCacheTtl;
    use crate::prompt::PromptParts;
    use crate::tool::tasks::testing::plan;
    use crate::util::path::testing::HOME_VAR;

    fn test_project_path(dir: &Path, filename: &str) -> PathBuf {
        dir.join(TEST_PROJECT).join(filename)
    }

    /// The error chain for a missing `path`, naming the I/O cause.
    fn open_failure(path: &Path) -> String {
        let cause = fs::metadata(path).unwrap_err();
        format!("failed to open {}: {cause}", path.display())
    }

    mod project {
        use super::*;

        mod owns {
            use super::*;

            #[test]
            fn keeps_the_recorded_project_the_launch_directory_or_an_older_root_session() {
                let project = Project {
                    cwd: "/repo/sub".to_owned(),
                    root: "/repo".to_owned(),
                };

                assert!(project.owns("/elsewhere", Some("/repo")));
                assert!(!project.owns("/repo", Some("/repo-b")));
                assert!(
                    project.owns("/repo/sub", Some("/repo/sub")),
                    "a session recorded before the directory joined a repository",
                );
                assert!(project.owns("/repo/sub", None));
                assert!(project.owns("/repo", None));
                assert!(!project.owns("/repo/other", None));
            }
        }
    }

    mod open {
        use super::*;

        fn open_in_isolated_env(xdg: &Path) -> SessionStore {
            let home = tempfile::tempdir().unwrap();
            temp_env::with_vars(
                [
                    ("XDG_DATA_HOME", Some(xdg.to_string_lossy().into_owned())),
                    (HOME_VAR, Some(home.path().to_string_lossy().into_owned())),
                ],
                || SessionStore::open(Path::new("/work/proj"), Path::new("/work/proj")),
            )
            .unwrap()
        }

        #[test]
        fn uses_xdg_data_home_when_set() {
            let xdg = tempfile::tempdir().unwrap();
            let store = open_in_isolated_env(xdg.path());
            let sessions = xdg.path().join("ox").join(SESSIONS_DIR);
            assert_eq!(store.sessions_dir, sessions);
            assert_eq!(
                store.project_dir,
                sessions.join(project_dir_name(Path::new("/work/proj")))
            );
            assert!(store.project_dir.is_dir(), "project dir created");
        }

        #[test]
        fn falls_back_to_home_local_share_when_xdg_unset() {
            let home = tempfile::tempdir().unwrap();
            let store = temp_env::with_vars(
                [
                    ("XDG_DATA_HOME", None),
                    (HOME_VAR, Some(home.path().to_string_lossy().into_owned())),
                ],
                || SessionStore::open(Path::new("/work/proj"), Path::new("/work/proj")),
            )
            .unwrap();
            let expected = home
                .path()
                .join(".local/share")
                .join("ox")
                .join(SESSIONS_DIR);
            assert_eq!(store.sessions_dir, expected);
        }

        #[cfg(unix)]
        #[test]
        fn creates_private_dirs_with_mode_0o700_on_unix() {
            use std::os::unix::fs::PermissionsExt as _;

            let xdg = tempfile::tempdir().unwrap();
            let store = open_in_isolated_env(xdg.path());
            for dir in [&store.sessions_dir, &store.project_dir] {
                let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "{dir:?} must be 0o700, got {mode:o}");
            }
        }
    }

    mod create {
        use super::*;

        #[test]
        fn writes_the_header_then_the_given_lines() {
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
                        Entry::Header(Header { session_id, version, .. }),
                        Entry::Message { uuid: written, .. },
                    ] if session_id == "test-id" && *version == CURRENT_VERSION && *written == uuid
                ),
                "got: {entries:?}"
            );
        }

        #[test]
        fn fails_when_file_already_exists() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let header = sample_header("existing");
            let created_at = header.created_at;
            let taken = test_project_path(dir.path(), &session_filename("existing", created_at));
            fs::write(&taken, "{}").unwrap();

            let err = store.create(&header, &[]).unwrap_err();

            assert_eq!(
                err.to_string(),
                format!("failed to create {}", taken.display())
            );
            assert_eq!(
                fs::read_to_string(&taken).unwrap(),
                "{}",
                "the file is left as it was"
            );
        }

        #[cfg(unix)]
        #[test]
        fn sets_user_only_file_permissions_on_unix() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let _writer = store.create(&sample_header("perm-test"), &[]).unwrap();

            let meta = fs::metadata(test_session_file(dir.path(), "perm-test")).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }

    mod open_append_at {
        use super::*;

        #[test]
        fn writes_to_existing_file() {
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
            assert!(matches!(
                &data.messages[0].content[0],
                ContentBlock::Text { text, .. } if text == "first"
            ));
            assert!(matches!(
                &data.messages[1].content[0],
                ContentBlock::Text { text, .. } if text == "second"
            ));
        }

        #[test]
        fn starts_an_empty_file_without_a_blank_line() {
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
        fn fails_for_a_missing_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ghost.jsonl");

            let err = open_append_at(&path).unwrap_err();

            assert_eq!(format!("{err:#}"), open_failure(&path));
        }

        #[test]
        fn opens_beside_a_live_writer_and_both_append() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut writer_a = store.create(&sample_header("concurrent"), &[]).unwrap();
            let path = test_session_file(dir.path(), "concurrent");
            let (from_a, from_b) = (Uuid::new_v4(), Uuid::new_v4());

            let mut writer_b = open_append_at(&path).unwrap();
            writer_a
                .append(&[sample_message_entry(from_a, "from a")])
                .unwrap();
            writer_b
                .append(&[sample_message_entry(from_b, "from b")])
                .unwrap();

            let content = fs::read_to_string(&path).unwrap();
            let message_uuids: Vec<Option<Uuid>> = content
                .lines()
                .map(|line| match serde_json::from_str(line).unwrap() {
                    Entry::Header(_) => None,
                    Entry::Message { uuid, .. } => Some(uuid),
                    entry => panic!("unexpected entry: {entry:?}"),
                })
                .collect();
            assert_eq!(message_uuids, [None, Some(from_a), Some(from_b)]);
        }
    }

    mod load_session_data_from_path {
        use super::*;

        #[test]
        fn only_the_committed_compact_root_gains_legacy_provenance() {
            let (_dir, store) = isolated_store();
            let now = datetime!(2026-01-01 00:00:00 UTC);
            let root = Uuid::new_v4();
            let prompt = Uuid::new_v4();
            let literal = "<system-reminder>Explain this tag.</system-reminder>";
            let legacy = Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("legacy summary wrapped\nacross lines"),
                    ContentBlock::text("<system-reminder>saved tasks</system-reminder>"),
                ],
            };
            let records = [
                Entry::Compact {
                    summary: "historical summary".to_owned(),
                    pre_message_count: 2,
                    instructions: None,
                    tasks: Vec::new(),
                    timestamp: now,
                },
                Entry::Message {
                    uuid: root,
                    parent_uuid: None,
                    message: legacy.clone(),
                    timestamp: now,
                },
                Entry::Message {
                    uuid: prompt,
                    parent_uuid: Some(root),
                    message: Message::user(literal),
                    timestamp: now,
                },
                Entry::Message {
                    uuid: Uuid::new_v4(),
                    parent_uuid: Some(prompt),
                    message: Message::user(crate::message::INTERRUPTION_MARKER),
                    timestamp: now,
                },
            ];
            store
                .create(
                    &sample_header("legacy-origins"),
                    &encode_lines(&records).unwrap(),
                )
                .unwrap();

            let loaded = store.load_session_data("legacy-origins").unwrap();
            let history = crate::session::history::interactions(
                &loaded.messages,
                loaded.compact.as_ref(),
                &loaded.tool_result_metadata,
                &crate::tool::ToolRegistry::new(Vec::new()),
                Path::new("/repo"),
            );

            assert_eq!(
                loaded.messages,
                [
                    Message {
                        role: Role::User,
                        content: vec![
                            ContentBlock::generated_text(
                                legacy.content[0].as_text().unwrap(),
                                TextOrigin::CompactionSummary
                            ),
                            ContentBlock::generated_text(
                                legacy.content[1].as_text().unwrap(),
                                TextOrigin::SystemReminder
                            ),
                        ]
                    },
                    Message::user(literal),
                    Message::user(crate::message::INTERRUPTION_MARKER),
                ]
            );
            assert_eq!(
                history,
                [
                    crate::protocol::Interaction::Compacted {
                        summary: "historical summary".to_owned(),
                        pre_count: 2,
                        instructions: None
                    },
                    crate::protocol::Interaction::UserText(literal.to_owned()),
                    crate::protocol::Interaction::UserText(
                        crate::message::INTERRUPTION_MARKER.to_owned()
                    ),
                ]
            );
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
                        tool_use_id: "toolu_1".to_owned(),
                    },
                },
            }
        }

        /// A boundary compacting one message, with no focus instructions or tasks.
        fn sample_compact(timestamp: OffsetDateTime) -> Entry {
            Entry::Compact {
                summary: "summary".to_owned(),
                pre_message_count: 1,
                instructions: None,
                tasks: Vec::new(),
                timestamp,
            }
        }

        fn sample_tool_metadata(
            tool_use_id: &str,
            title: &str,
            timestamp: OffsetDateTime,
        ) -> Entry {
            Entry::ToolResultMetadata {
                tool_use_id: tool_use_id.to_owned(),
                metadata: ToolMetadata {
                    title: Some(title.to_owned()),
                    ..ToolMetadata::default()
                },
                timestamp,
            }
        }

        #[test]
        fn includes_only_messages_with_last_uuid() {
            let (_dir, store) = isolated_store();
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
            assert!(matches!(
                &data.messages[0].content[0],
                ContentBlock::Text { text, .. } if text == "hello"
            ));
            assert_eq!(data.messages[1].role, crate::message::Role::Assistant);
            assert!(matches!(
                &data.messages[1].content[0],
                ContentBlock::Text { text, .. } if text == "hi there"
            ));
        }

        #[test]
        fn resets_sidecars_at_compact_boundary() {
            let (_dir, store) = isolated_store();
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
                .append(&[sample_tool_metadata(
                    "pre-tool",
                    "pre metadata",
                    datetime!(2026-04-16 12:00:02 UTC),
                )])
                .unwrap();
            writer.append(&[sample_snapshot("pre.txt")]).unwrap();
            writer
                .append(&[sample_compact(datetime!(2026-04-16 12:00:03 UTC))])
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
                .append(&[sample_tool_metadata(
                    "post-tool",
                    "post metadata",
                    datetime!(2026-04-16 12:00:05 UTC),
                )])
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
        fn takes_the_latest_context_since_the_last_compact_boundary() {
            let (_dir, store) = isolated_store();
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
                .append(&[sample_compact(datetime!(2026-04-17 12:00:00 UTC))])
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
        fn drops_a_context_whose_message_was_torn_off() {
            let (_dir, store) = isolated_store();
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
        fn drops_the_context_of_a_rejected_pre_compact_append() {
            let (_dir, store) = isolated_store();
            let mut writer = store
                .create(&sample_header("rejected-context"), &[])
                .unwrap();
            let pre = Uuid::new_v4();
            let synthetic = Uuid::new_v4();
            writer
                .append(&[
                    sample_message_at(pre, None, datetime!(2026-04-16 12:00:01 UTC), "pre"),
                    sample_compact(datetime!(2026-04-16 12:00:02 UTC)),
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
        fn replays_the_context_recorded_on_the_resolved_branch() {
            let (_dir, store) = isolated_store();
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
        fn ignores_newer_old_chain_messages_after_compact() {
            let (_dir, store) = isolated_store();
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
                ContentBlock::Text { text, .. } if text == "synthetic summary"
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
        fn ignores_a_compact_whose_root_never_landed() {
            let (_dir, store) = isolated_store();
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
            let mut torn =
                encode_lines(&[sample_compact(datetime!(2026-04-16 12:00:02 UTC))]).unwrap();
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
        fn skips_sidecars_after_rejected_compact_tail_message() {
            let (_dir, store) = isolated_store();
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
                .append(&[sample_compact(datetime!(2026-04-16 12:00:02 UTC))])
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
                .append(&[sample_tool_metadata(
                    "stale-tool",
                    "stale metadata",
                    datetime!(2026-04-16 12:00:05 UTC),
                )])
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
                .append(&[sample_tool_metadata(
                    "valid-tool",
                    "valid metadata",
                    datetime!(2026-04-16 12:00:08 UTC),
                )])
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
        fn collects_usage_once_on_every_branch_and_across_compactions() {
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
            assert_eq!(
                data.messages,
                [Message {
                    role: Role::User,
                    content: vec![ContentBlock::generated_text(
                        "summary",
                        TextOrigin::CompactionSummary
                    )],
                }]
            );
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
        fn skips_corrupt_empty_unknown_and_old_shape_lines() {
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
                    {"type":"message","uuid":"b2c3d4e5-f6a7-8901-bcde-234567890abc","parent_uuid":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","message":{"role":"assistant","content":[{"type":"thinking","thinking":"r","signature":"sig"},{"type":"text","text":"hi"}]},"timestamp":"2026-01-01T00:00:02Z"}
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
        fn names_the_file_and_the_cause_when_it_cannot_open() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ghost.jsonl");

            let err = load_session_data_from_path(&path).unwrap_err();

            assert_eq!(format!("{err:#}"), open_failure(&path));
        }

        #[test]
        fn resolves_transcript_through_newest_chain_leaf() {
            let (_dir, store) = isolated_store();
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
                matches!(&data.messages[0].content[0], ContentBlock::Text { text, .. } if text == "shared root"),
                "chain should start at the shared ancestor"
            );
            assert!(
                matches!(&data.messages[1].content[0], ContentBlock::Text { text, .. } if text == "branch B (newer leaf)"),
                "chain should end at the newest leaf"
            );
        }

        #[test]
        fn recovers_from_truncated_utf8_at_eof() {
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
        fn recovers_from_missing_trailing_newline() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let created_at = datetime!(2026-01-01 00:00:00 UTC);
            let path = test_project_path(dir.path(), &session_filename("nonewline", created_at));
            let content = indoc! {r#"
                {"type":"header","session_id":"nonewline","cwd":"/","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}
                {"type":"message","uuid":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","message":{"role":"user","content":[{"type":"text","text":"ok"}]},"timestamp":"2026-01-01T00:00:01Z"}"#
            };
            fs::write(&path, content).unwrap();

            let data = store.load_session_data("nonewline").unwrap();
            assert_eq!(data.messages.len(), 1);
            assert_eq!(
                data.last_uuid,
                Some(Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-1234567890ef").unwrap())
            );
        }

        #[test]
        fn rejects_future_format_version() {
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
            let err = store.load_session_data("future").unwrap_err();
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "session format version {future_version} \
                     is newer than supported ({CURRENT_VERSION}). \
                     Upgrade oxide-code to open it."
                ),
            );
        }

        #[test]
        fn returns_the_header_id_and_refuses_a_file_without_one() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            store.create(&sample_header("headed"), &[]).unwrap();
            let headed = test_session_file(dir.path(), "headed");
            let headless = dir.path().join("headless.jsonl");
            fs::write(
                &headless,
                encode_lines([&sample_title_entry("Orphan")]).unwrap(),
            )
            .unwrap();

            let data = load_session_data_from_path(&headed).unwrap();
            let err = load_session_data_from_path(&headless).unwrap_err();

            assert_eq!(data.session_id, "headed");
            assert_eq!(
                err.to_string(),
                format!(
                    "{} is not an oxide-code session file: its first line is not a session header",
                    headless.display(),
                ),
            );
        }
    }

    mod read_session_id_from_path {
        use super::*;

        #[test]
        fn produces_header_session_id() {
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
        fn rejects_non_header_first_line() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("headless.jsonl");
            fs::write(
                &path,
                r#"{"type":"message","uuid":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","message":{"role":"user","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-01-01T00:00:01Z"}
"#,
            )
            .unwrap();
            let err = read_session_id_from_path(&path).unwrap_err();

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "{} is not an oxide-code session file: its first line is not a session header",
                    path.display(),
                ),
            );
        }

        #[test]
        fn rejects_unparsable_first_line() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("notes.txt");
            fs::write(&path, "not json\n").unwrap();

            let err = read_session_id_from_path(&path).unwrap_err();

            let headline = format!(
                "{} is not an oxide-code session file: its first line is not a session header: ",
                path.display(),
            );
            assert!(format!("{err:#}").starts_with(&headline), "{err:#}");
        }

        #[test]
        fn rejects_an_empty_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("empty.jsonl");
            fs::write(&path, "").unwrap();

            let err = read_session_id_from_path(&path).unwrap_err();

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "{} is not an oxide-code session file: it is empty",
                    path.display()
                ),
            );
        }

        #[test]
        fn rejects_invalid_session_id() {
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
        fn errors_when_file_missing() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ghost.jsonl");

            let err = read_session_id_from_path(&path).unwrap_err();

            assert_eq!(format!("{err:#}"), open_failure(&path));
        }
    }

    mod delete_at {
        use super::*;

        #[test]
        fn removes_the_session_file_and_its_directory() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let _writer = store.create(&sample_header("doomed"), &[]).unwrap();
            let path = test_session_file(dir.path(), "doomed");
            let artifact_dir = test_project_path(dir.path(), "doomed");
            let spill = artifact_dir.join("tool-results/toolu_1.txt");
            fs::create_dir_all(spill.parent().unwrap()).unwrap();
            fs::write(&spill, "output").unwrap();

            delete_at(&path).unwrap();

            assert!(!path.exists(), "delete must unlink the file: {path:?}");
            assert!(
                !artifact_dir.exists(),
                "delete must remove the session directory"
            );
            assert!(
                store.list().unwrap().is_empty(),
                "list reflects the removal"
            );
        }

        #[test]
        fn errors_when_the_file_is_missing() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ghost.jsonl");

            let err = delete_at(&path).expect_err("missing session must error");

            assert_eq!(format!("{err:#}"), open_failure(&path));
        }

        #[test]
        fn keeps_the_session_file_when_its_directory_stays() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let _writer = store.create(&sample_header("stuck"), &[]).unwrap();
            let path = test_session_file(dir.path(), "stuck");
            fs::write(test_project_path(dir.path(), "stuck"), "not a directory").unwrap();

            let err = delete_at(&path).expect_err("the directory must go first");

            assert!(
                err.to_string()
                    .contains("failed to remove session directory"),
                "got: {err:#}"
            );
            assert!(path.exists(), "the session stays listed for another try");
        }
    }

    mod set_archived_at {
        use super::*;

        #[test]
        fn preserves_activity_committed_before_its_waiting_write() {
            let (_dir, store) = isolated_store();
            let mut writer = store.create(&sample_header("active"), &[]).unwrap();
            let path = writer.path().to_owned();
            let barrier = WriterBarrier::hold(&path);
            let (started, waiting) = mpsc::channel();
            let target = path.clone();
            let archive = std::thread::spawn(move || {
                started.send(()).unwrap();
                set_archived_at(&target, true).unwrap();
            });
            waiting.recv().unwrap();
            let active = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            let file = open_append_file(&path).unwrap();

            append_whole(
                &file,
                &mut Appender(&file),
                &encode_lines([&sample_message_entry(Uuid::new_v4(), "activity")]).unwrap(),
            )
            .unwrap();
            file.set_modified(active).unwrap();
            barrier.release();
            archive.join().unwrap();

            let data = load_session_data_from_path(&path).unwrap();
            assert!(data.archived);
            assert_eq!(data.messages, [Message::user("activity")]);
            assert_eq!(file.metadata().unwrap().modified().unwrap(), active);

            writer
                .append(&[sample_message_entry(Uuid::new_v4(), "later activity")])
                .unwrap();
            assert_ne!(file.metadata().unwrap().modified().unwrap(), active);
        }

        #[test]
        fn latest_entry_wins_keeping_the_transcript_and_mtime() {
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
        fn missing_file_errors_without_creating_it() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ghost.jsonl");
            let err = set_archived_at(&path, true).expect_err("missing session must error");

            assert_eq!(format!("{err:#}"), open_failure(&path));
            assert!(!path.exists());
        }
    }

    mod session_writer {
        use super::*;

        mod append_lines {
            use super::*;

            #[test]
            fn writes_multiple_entries() {
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
                            Entry::Header(_),
                            Entry::Message { uuid: first, .. },
                            Entry::Message { uuid: second, .. },
                        ] if *first == hello && *second == world
                    ),
                    "got: {entries:?}"
                );
            }

            #[test]
            fn terminates_a_line_torn_by_another_appender() {
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
        }
    }

    mod create_written {
        use super::*;

        #[test]
        fn leaves_no_public_file_when_the_write_fails() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("doomed.jsonl");

            let err =
                create_written(&path, |_| Err(io::ErrorKind::StorageFull.into())).unwrap_err();

            assert!(
                format!("{err:#}").contains("failed to write"),
                "got: {err:#}"
            );
            assert!(!path.exists(), "a failed create must leave no file behind");
            create_written(&path, |_| Ok(())).unwrap();
        }

        #[test]
        fn publishes_only_complete_content_and_preserves_a_competing_session() {
            for fail in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let header = sample_header("publication");
                let path = store.session_file(&header.session_id, header.created_at);
                let preparing = std::sync::Barrier::new(2);
                let release = std::sync::Barrier::new(2);

                std::thread::scope(|scope| {
                    let creator = scope.spawn(|| {
                        create_written(&path, |mut file| {
                            file.write_all(b"partial")?;
                            preparing.wait();
                            release.wait();
                            if fail {
                                Err(io::ErrorKind::StorageFull.into())
                            } else {
                                file.write_all(b" complete\n")
                            }
                        })
                    });
                    preparing.wait();
                    let visible = path.exists();
                    let listed = store.list().unwrap();
                    let lines =
                        encode_lines([&sample_message_entry(Uuid::new_v4(), "winner")]).unwrap();
                    let competing = store.create(&header, &lines);
                    release.wait();

                    assert!(!visible, "unpublished content must stay private");
                    assert_eq!(
                        listed
                            .into_iter()
                            .map(|info| info.session_id)
                            .collect::<Vec<_>>(),
                        [] as [String; 0],
                        "private content appeared in listings"
                    );
                    competing.unwrap();
                    let error = creator.join().unwrap().unwrap_err();
                    assert!(
                        error.to_string().contains(if fail {
                            "failed to write"
                        } else {
                            "failed to create"
                        }),
                        "{error:#}",
                    );
                    assert_eq!(
                        load_session_data_from_path(&path).unwrap().messages,
                        [Message::user("winner")],
                    );
                });
                assert_eq!(fs::read_dir(&store.project_dir).unwrap().count(), 1);
            }
        }
    }

    mod open_writer_lock {
        use super::*;

        #[test]
        fn keeps_the_same_lock_after_deletion_and_recreation() {
            let (_dir, store) = isolated_store();
            let header = sample_header("recreated");
            let writer = store.create(&header, &[]).unwrap();
            let path = writer.path().to_owned();
            let lock = lock_writer(&path).unwrap();

            delete_at(&path).unwrap();
            store.create(&header, &[]).unwrap();
            let competing = open_writer_lock(&path).unwrap();

            assert!(matches!(
                competing.try_lock(),
                Err(fs::TryLockError::WouldBlock)
            ));
            drop(lock);
            competing.try_lock().unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn creates_a_private_lock_shared_by_symlink_paths() {
            use std::os::unix::fs::{PermissionsExt as _, symlink};

            let (dir, store) = isolated_store();
            let writer = store.create(&sample_header("linked"), &[]).unwrap();
            let alias = dir.path().join("alias.jsonl");
            symlink(writer.path(), &alias).unwrap();
            let lock = lock_writer(writer.path()).unwrap();

            let competing = open_writer_lock(&alias).unwrap();

            assert_eq!(lock.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            assert!(matches!(
                competing.try_lock(),
                Err(fs::TryLockError::WouldBlock)
            ));
        }
    }

    mod appender {
        use super::*;

        mod write {
            use super::*;

            #[test]
            fn lands_after_lines_another_handle_appended() {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("shared.jsonl");
                create_written(&path, |mut file| file.write_all(b"1\n")).unwrap();
                let first = open_append_file(&path).unwrap();
                let second = open_append_file(&path).unwrap();

                Appender(&second).write_all(b"2\n").unwrap();
                let mut appender = Appender(&first);
                appender.write_all(b"3\n").unwrap();
                appender.flush().unwrap();

                assert_eq!(fs::read_to_string(&path).unwrap(), "1\n2\n3\n");
            }
        }
    }

    mod append_whole {
        use super::*;

        const CHILD_PATH: &str = "OX_TEST_SESSION_APPEND_PATH";

        struct CompetingWriter {
            child: Child,
            commands: ChildStdin,
            output: mpsc::Receiver<String>,
        }

        impl CompetingWriter {
            fn spawn(path: &Path) -> Self {
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "session::store::tests::append_whole::serializes_short_writes_and_recovery_across_processes",
                        "--nocapture",
                    ])
                    .env(CHILD_PATH, path)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let stdout = child.stdout.take().unwrap();
                let (tx, output) = mpsc::channel();
                std::thread::spawn(move || {
                    for line in BufReader::new(stdout).lines() {
                        if tx.send(line.unwrap()).is_err() {
                            break;
                        }
                    }
                });
                Self {
                    commands: child.stdin.take().unwrap(),
                    output,
                    child,
                }
            }

            fn check_lock(&mut self) {
                self.commands.write_all(b"check\n").unwrap();
                self.wait_for("OX_LOCK_HELD");
            }

            fn wait_for(&self, marker: &str) {
                loop {
                    let line = self.output.recv_timeout(Duration::from_secs(10)).unwrap();
                    if line.trim().ends_with(marker) {
                        break;
                    }
                }
            }

            fn append_and_wait(mut self) {
                self.commands.write_all(b"append\n").unwrap();
                self.wait_for("OX_APPEND_DONE");
                assert!(self.child.wait().unwrap().success());
            }
        }

        impl Drop for CompetingWriter {
            fn drop(&mut self) {
                _ = self.child.kill();
                _ = self.child.wait();
            }
        }

        fn append_in_child(path: &Path) {
            let mut command = String::new();
            io::stdin().read_line(&mut command).unwrap();
            let file = open_writer_lock(path).unwrap();
            assert!(matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock)));
            drop(file);
            assert_eq!(read_session_id_from_path(path).unwrap(), "partial");
            assert_eq!(
                load_session_data_from_path(path).unwrap().messages,
                [Message::user("root")]
            );
            let store = test_store(path.parent().unwrap().parent().unwrap());
            assert_eq!(
                store
                    .list()
                    .unwrap()
                    .into_iter()
                    .map(|info| info.session_id)
                    .collect::<Vec<_>>(),
                ["partial"]
            );
            println!("OX_LOCK_HELD");
            io::stdout().flush().unwrap();
            command.clear();
            io::stdin().read_line(&mut command).unwrap();
            let parent = load_session_data_from_path(path).unwrap().last_uuid;
            open_append_at(path)
                .unwrap()
                .append(&[sample_message_at(
                    Uuid::new_v4(),
                    parent,
                    datetime!(2026-04-16 12:00:04 UTC),
                    "other writer",
                )])
                .unwrap();
            println!("OX_APPEND_DONE");
            io::stdout().flush().unwrap();
        }

        /// Writes to `file` until `budget` bytes are spent, then returns what `exhausted` does.
        struct ShortWriter<'a, F> {
            file: &'a File,
            budget: usize,
            exhausted: F,
        }

        impl<F: FnMut(&File, &[u8]) -> io::Result<usize>> Write for ShortWriter<'_, F> {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.budget == 0 {
                    return (self.exhausted)(self.file, buf);
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
        fn truncates_a_partial_write_so_the_retry_adds_no_duplicates() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let (path, mut writer, batch) = session_with_pending_batch(&store, dir.path());
            let first_line = batch.iter().position(|&byte| byte == b'\n').unwrap() + 1;
            let file = open_append_file(&path).unwrap();
            let mut out = ShortWriter {
                file: &file,
                budget: first_line + 5,
                exhausted: |_: &File, _: &[u8]| Err(io::ErrorKind::StorageFull.into()),
            };

            let err = append_whole(&file, &mut out, &batch).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::StorageFull);
            drop(file);
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
        fn serializes_short_writes_and_recovery_across_processes() {
            if let Some(path) = std::env::var_os(CHILD_PATH) {
                append_in_child(Path::new(&path));
                return;
            }

            for failure in [
                None,
                Some(io::ErrorKind::StorageFull),
                Some(io::ErrorKind::WriteZero),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let (path, _writer, batch) = session_with_pending_batch(&store, dir.path());
                let mut child = CompetingWriter::spawn(&path);
                let lock = lock_writer(&path).unwrap();
                let file = open_append_file(&path).unwrap();
                let mut checked = false;
                let mut out = ShortWriter {
                    file: &file,
                    budget: 5,
                    exhausted: |file: &File, buf: &[u8]| {
                        if !checked {
                            child.check_lock();
                            checked = true;
                            return Err(io::ErrorKind::Interrupted.into());
                        }
                        match failure {
                            Some(io::ErrorKind::WriteZero) => Ok(0),
                            Some(kind) => Err(kind.into()),
                            None => Appender(file).write(&buf[..buf.len().min(17)]),
                        }
                    },
                };

                let result = append_whole(&file, &mut out, &batch);
                drop(file);
                drop(lock);
                child.append_and_wait();

                assert_eq!(
                    result.as_ref().err().map(io::Error::kind),
                    failure,
                    "{failure:?}"
                );
                let mut expected = vec![Message::user("root")];
                if failure.is_none() {
                    expected.extend([Message::user("second"), Message::user("third")]);
                }
                expected.push(Message::user("other writer"));
                assert_eq!(
                    store.load_session_data("partial").unwrap().messages,
                    expected,
                    "{failure:?}",
                );
            }
        }
    }
}
