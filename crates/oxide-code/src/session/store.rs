use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use time::OffsetDateTime;
use tracing::{debug, warn};
use uuid::Uuid;

use super::entry::{CURRENT_VERSION, Entry, ExitInfo, SessionInfo, TitleInfo};
use super::path::{UNKNOWN_PROJECT_DIR, sanitize_cwd};
use crate::message::Message;

const DATA_DIR: &str = "ox";
const SESSIONS_DIR: &str = "sessions";

/// Tail buffer size for extracting the latest [`Entry::Title`] /
/// [`Entry::Summary`] without reading the entire file. 4 KB is generous
/// for a single JSON line.
const TAIL_BUF_SIZE: u64 = 4096;

/// Retry budget for acquiring the advisory write lock on a session
/// file. Matches the credentials lock in `config/oauth.rs` so the two
/// retry paths behave uniformly.
const LOCK_MAX_RETRIES: u32 = 5;

/// Sleep duration between lock-acquisition attempts. Shortened under
/// `cfg(test)` so the contention test does not block CI for seconds.
#[cfg(not(test))]
const LOCK_RETRY_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

// ── SessionStore ──

/// Low-level session file operations.
///
/// Sessions are stored under `$XDG_DATA_HOME/ox/sessions/{project}/`,
/// where `{project}` is a sanitized fingerprint of the working
/// directory at session creation time. The store exposes one "home"
/// project (the current CWD) that listing, creation, and default
/// resume operate on, and provides explicit cross-project variants
/// for `--all` callers.
pub(crate) struct SessionStore {
    /// Root directory holding every project subdirectory.
    sessions_dir: PathBuf,
    /// Subdirectory for the current working directory.
    project_dir: PathBuf,
}

impl SessionStore {
    /// Create a store rooted at the XDG data directory, scoped to the
    /// current working directory. Creates both the root and the
    /// project subdirectory if needed, and runs a one-time migration
    /// of any legacy flat-layout sessions into their project subdirs.
    pub(crate) fn open() -> Result<Self> {
        let sessions_dir = resolve_sessions_dir(
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            dirs::home_dir(),
        )
        .context("cannot determine session storage directory")?;

        fs::create_dir_all(&sessions_dir)
            .with_context(|| format!("failed to create {}", sessions_dir.display()))?;

        // Migrations run oldest-first so each pass operates on the
        // layout the previous one produced.
        migrate_flat_layout(&sessions_dir);
        migrate_add_timestamp_prefix(&sessions_dir);

        let project_name = match std::env::current_dir() {
            Ok(cwd) => sanitize_cwd(&cwd),
            Err(e) => {
                warn!("cannot resolve current directory for project scoping: {e}");
                UNKNOWN_PROJECT_DIR.to_owned()
            }
        };
        let project_dir = sessions_dir.join(&project_name);
        fs::create_dir_all(&project_dir)
            .with_context(|| format!("failed to create {}", project_dir.display()))?;

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
    /// Takes an exclusive advisory lock on the file to prevent concurrent
    /// access. The lock is held for the lifetime of the returned writer.
    /// On Unix, the file is created with mode `0o600` so session contents
    /// (verbatim tool output, assistant responses) are not world-readable.
    ///
    /// Filenames are `{created_at_epoch}-{session_id}.jsonl`. The epoch
    /// prefix makes `ls` on a project subdirectory return sessions in
    /// chronological order, which is convenient when inspecting the
    /// store outside of `ox --list`.
    pub(crate) fn create(&self, header: &Entry) -> Result<SessionWriter> {
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
        lock_with_retry(&file, session_id)?;
        let mut writer = SessionWriter { file };
        writer.append(header)?;
        Ok(writer)
    }

    /// Open an existing session file in append mode.
    ///
    /// Takes an exclusive advisory lock on the file to prevent concurrent
    /// access. The lock is held for the lifetime of the returned writer.
    /// Contended locks are retried up to [`LOCK_MAX_RETRIES`] times with
    /// a [`LOCK_RETRY_INTERVAL`] delay, so accidental back-to-back
    /// `ox -c <id>` invocations do not fail abruptly.
    ///
    /// Searches every project subdirectory, not just the current one,
    /// so `ox -c <id>` resumes a session regardless of which project
    /// it originally belonged to.
    pub(crate) fn open_append(&self, session_id: &str) -> Result<SessionWriter> {
        let path = self.find_session_path(session_id)?;
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("session not found: {}", path.display()))?;
        lock_with_retry(&file, session_id)?;
        Ok(SessionWriter { file })
    }

    /// Load all messages from a session file along with the UUID of the
    /// last message (for parent-chain continuity on resume). Like
    /// [`Self::open_append`], searches every project subdirectory.
    ///
    /// Skips non-[`Entry::Message`] lines (headers, titles, summaries,
    /// unknown). Warns and skips malformed lines.
    pub(crate) fn load_session_data(&self, session_id: &str) -> Result<SessionData> {
        let path = self.find_session_path(session_id)?;
        let file =
            File::open(&path).with_context(|| format!("session not found: {}", path.display()))?;
        let mut reader = BufReader::new(file);
        let mut messages = Vec::new();
        let mut last_uuid = None;
        let mut buf = Vec::new();
        let mut line_no: u32 = 0;

        // Read byte-by-line instead of `BufReader::lines()`. A crash
        // during `writeln!` can leave the last record truncated —
        // mid-byte of a multibyte UTF-8 codepoint in the worst case —
        // and `lines()` propagates `InvalidData` there, failing the
        // entire resume. Doing the decode ourselves lets us warn-skip
        // bad lines with the same resilience we already apply to
        // malformed JSON, and tolerate a missing trailing newline.
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

/// Reject session IDs that could escape the project subdirectory.
fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.contains(['/', '\\', '\0']) || session_id.contains("..") {
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
    sessions.sort_by(|a, b| {
        b.last_active_at
            .cmp(&a.last_active_at)
            .then_with(|| b.session_id.cmp(&a.session_id))
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

/// Try to acquire an exclusive advisory lock, retrying on contention
/// with a fixed interval between attempts.
///
/// `flock` is released automatically when a process exits, so a stuck
/// lock always implies a live peer. Retrying lets accidental
/// back-to-back invocations succeed once the first has finished a
/// short-lived action (e.g. listing, quick query), while still erroring
/// out on a genuinely long-held lock after [`LOCK_MAX_RETRIES`] attempts.
fn lock_with_retry(file: &File, session_id: &str) -> Result<()> {
    for attempt in 0..=LOCK_MAX_RETRIES {
        match file.try_lock_exclusive() {
            Ok(true) => return Ok(()),
            Ok(false) if attempt < LOCK_MAX_RETRIES => {
                std::thread::sleep(LOCK_RETRY_INTERVAL);
            }
            Ok(false) => {
                bail!(
                    "session {session_id} is in use by another process \
                     (retried {LOCK_MAX_RETRIES} times)"
                );
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "failed to acquire lock on session {session_id}"
                )));
            }
        }
    }
    unreachable!()
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

// ── Migration ──

/// Move any legacy flat-layout session files (`{uuid}.jsonl` directly
/// inside [`sessions_dir`]) into their project subdirectory based on
/// each file's header `cwd`, adding the timestamp prefix in the
/// process. Idempotent and fast when no flat files exist, so it can
/// run unconditionally at store open time.
///
/// Errors on individual files are logged and skipped — a partial
/// migration is preferable to refusing to open the store entirely.
fn migrate_flat_layout(sessions_dir: &Path) {
    let entries = match fs::read_dir(sessions_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                "cannot scan {} for flat-layout migration: {e}",
                sessions_dir.display()
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl")
            || entry.file_type().is_ok_and(|t| t.is_dir())
        {
            continue;
        }
        if let Err(e) = relocate_flat_session(sessions_dir, &path) {
            warn!(
                "skipping flat-layout migration for {}: {e}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
}

/// Read the header of a legacy flat-layout session file and move it
/// into the appropriate project subdirectory with a timestamped name.
fn relocate_flat_session(sessions_dir: &Path, path: &Path) -> Result<()> {
    let (session_id, cwd, created_at) =
        read_session_header_parts(path).context("cannot read header for migration")?;
    let project = sessions_dir.join(sanitize_cwd(Path::new(&cwd)));
    fs::create_dir_all(&project).with_context(|| format!("cannot create {}", project.display()))?;
    let target = project.join(session_filename(&session_id, created_at));
    if target.exists() {
        bail!("destination already exists: {}", target.display());
    }
    fs::rename(path, &target)
        .with_context(|| format!("rename {} -> {}", path.display(), target.display()))?;
    debug!("migrated {} to {}", path.display(), target.display());
    Ok(())
}

/// Rename `{session_id}.jsonl` files inside each project subdirectory
/// to `{epoch}-{session_id}.jsonl`. A no-op when every file is already
/// prefixed.
///
/// Called after [`migrate_flat_layout`], so every direct entry in
/// `sessions_dir` is a project subdirectory.
fn migrate_add_timestamp_prefix(sessions_dir: &Path) {
    let entries = match fs::read_dir(sessions_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                "cannot scan {} for timestamp-prefix migration: {e}",
                sessions_dir.display()
            );
            return;
        }
    };
    for project in entries.flatten() {
        if !project.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        prefix_sessions_in(&project.path());
    }
}

fn prefix_sessions_in(dir: &Path) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!("cannot scan {} for timestamp prefix: {e}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Already prefixed when the stem starts with `{digits}-`.
        if stem.split_once('-').is_some_and(|(prefix, _)| {
            !prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_digit())
        }) {
            continue;
        }
        if let Err(e) = prefix_one_session(&path) {
            warn!(
                "skipping timestamp prefix for {}: {e}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
}

fn prefix_one_session(path: &Path) -> Result<()> {
    let (session_id, _cwd, created_at) =
        read_session_header_parts(path).context("cannot read header for migration")?;
    let parent = path.parent().context("session path missing parent")?;
    let target = parent.join(session_filename(&session_id, created_at));
    if target.exists() {
        bail!("destination already exists: {}", target.display());
    }
    fs::rename(path, &target)
        .with_context(|| format!("rename {} -> {}", path.display(), target.display()))?;
    debug!("renamed {} to {}", path.display(), target.display());
    Ok(())
}

/// Read the first line of a session file and return the fields
/// migrations need (`session_id`, `cwd`, `created_at`).
fn read_session_header_parts(path: &Path) -> Result<(String, String, OffsetDateTime)> {
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut first_line = String::new();
    BufReader::new(file).read_line(&mut first_line)?;
    let entry: Entry = serde_json::from_str(first_line.trim()).context("invalid header line")?;
    match entry {
        Entry::Header {
            session_id,
            cwd,
            created_at,
            ..
        } => Ok((session_id, cwd, created_at)),
        _ => bail!("first line is not a header"),
    }
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
mod tests {
    use indoc::indoc;
    use time::macros::datetime;

    use super::super::entry::TitleSource;
    use super::*;
    use crate::message::ContentBlock;

    const TEST_PROJECT: &str = "test-project";

    fn test_store(dir: &Path) -> SessionStore {
        SessionStore::open_at(dir.to_path_buf(), TEST_PROJECT).unwrap()
    }

    /// Direct path inside [`TEST_PROJECT`] for a given filename. Used
    /// by tests that hand-roll a file before opening the store, or
    /// that seed invalid content the production loader should skip.
    fn test_project_path(dir: &Path, filename: &str) -> PathBuf {
        dir.join(TEST_PROJECT).join(filename)
    }

    /// Locate a session file inside [`TEST_PROJECT`] by its session
    /// ID. Filenames are prefixed with the creation epoch (see
    /// [`session_filename`]), so tests cannot build the path directly
    /// from a session ID alone; this helper scans the project dir
    /// for the matching suffix.
    fn test_session_file(dir: &Path, session_id: &str) -> PathBuf {
        let project_dir = dir.join(TEST_PROJECT);
        find_session_in(&project_dir, session_id)
            .unwrap()
            .unwrap_or_else(|| panic!("no session file for id {session_id} in {project_dir:?}"))
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

        let content = fs::read_to_string(test_session_file(dir.path(), "test-id")).unwrap();
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
        let header = sample_header("existing");
        let Entry::Header { created_at, .. } = header else {
            unreachable!()
        };
        let taken = test_project_path(dir.path(), &session_filename("existing", created_at));
        fs::write(taken, "{}").unwrap();
        assert!(store.create(&header).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn create_sets_user_only_file_permissions_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("perm-test")).unwrap();

        let meta = fs::metadata(test_session_file(dir.path(), "perm-test")).unwrap();
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
    fn open_append_rejects_concurrent_access_after_retries_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _writer = store.create(&sample_header("locked")).unwrap();

        let start = std::time::Instant::now();
        let result = store.open_append("locked");
        let elapsed = start.elapsed();

        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("retried"), "unexpected error: {err}");
        assert!(
            err.contains("in use by another process"),
            "unexpected error: {err}"
        );
        // Retry loop must wait LOCK_MAX_RETRIES × LOCK_RETRY_INTERVAL before
        // giving up — confirms we actually retried rather than failing fast.
        let expected = LOCK_RETRY_INTERVAL * LOCK_MAX_RETRIES;
        assert!(
            elapsed >= expected,
            "lock gave up too early: {elapsed:?} < {expected:?}"
        );
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
        assert!(store.load_session_data("../etc/passwd").is_err());
        assert!(store.load_session_data(r"..\..\etc\passwd").is_err());
        assert!(store.load_session_data("session\0evil").is_err());
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

    #[test]
    fn list_returns_sessions_in_mtime_order_newest_first() {
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
    fn list_mtime_overrides_header_created_at_order() {
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

        let path = test_session_file(dir.path(), "long");
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
        let store = test_store(dir.path());
        fs::write(test_project_path(dir.path(), "notes.txt"), "not a session").unwrap();
        fs::write(
            test_project_path(dir.path(), "bad.jsonl"),
            r#"{"type":"message","uuid":"00000000-0000-0000-0000-000000000000","message":{"role":"user","content":[]},"timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_is_scoped_to_current_project() {
        // Only sessions in the current project dir are visible; siblings
        // in other project subdirectories stay hidden.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _own = store.create(&sample_header("own")).unwrap();

        // Drop a session into a sibling project subdir.
        let sibling_store =
            SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let _foreign = sibling_store.create(&sample_header("foreign")).unwrap();
        drop(sibling_store);

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "own");
    }

    // ── list_all ──

    #[test]
    fn list_all_spans_every_project_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let _own = store.create(&sample_header("own")).unwrap();

        let foreign_store =
            SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let _foreign = foreign_store.create(&sample_header("foreign")).unwrap();
        drop(foreign_store);

        let all = store.list_all().unwrap();
        let mut ids: Vec<_> = all.iter().map(|s| s.session_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["foreign", "own"]);
    }

    // ── find_session_path ──

    #[test]
    fn find_session_path_falls_back_to_other_projects() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());

        // Session lives in a different project subdirectory.
        let foreign_store =
            SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let _w = foreign_store.create(&sample_header("foreign")).unwrap();
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

        let content = fs::read_to_string(test_session_file(dir.path(), "multi")).unwrap();
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

    // ── migrate_flat_layout ──

    #[test]
    fn migrate_flat_layout_moves_files_into_project_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().to_path_buf();
        fs::create_dir_all(&sessions_dir).unwrap();

        // Seed a legacy flat-layout session whose header points at
        // "/foo/project".
        let legacy_path = sessions_dir.join("legacy.jsonl");
        fs::write(
            &legacy_path,
            r#"{"type":"header","session_id":"legacy","cwd":"/foo/project","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}
"#,
        )
        .unwrap();

        migrate_flat_layout(&sessions_dir);

        assert!(!legacy_path.exists(), "legacy file should have moved");
        let project_dir = sessions_dir.join(sanitize_cwd(Path::new("/foo/project")));
        let moved = find_session_in(&project_dir, "legacy").unwrap();
        let name = moved
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());
        assert_eq!(
            name.as_deref(),
            Some("1767225600-legacy.jsonl"),
            "unexpected migrated filename: {name:?}"
        );
    }

    #[test]
    fn migrate_flat_layout_is_idempotent_and_skips_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().to_path_buf();

        // Pre-existing project subdir with a session already in place.
        let project = sessions_dir.join("already-scoped");
        fs::create_dir_all(&project).unwrap();
        let settled = project.join("kept.jsonl");
        fs::write(
            &settled,
            r#"{"type":"header","session_id":"kept","cwd":"/x","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}
"#,
        )
        .unwrap();

        // Calling the migration twice must leave the file exactly where
        // it is and not recurse into the subdirectory.
        migrate_flat_layout(&sessions_dir);
        migrate_flat_layout(&sessions_dir);

        assert!(settled.exists());
    }

    // ── migrate_add_timestamp_prefix ──

    #[test]
    fn migrate_add_timestamp_prefix_renames_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().to_path_buf();
        let project = sessions_dir.join("proj");
        fs::create_dir_all(&project).unwrap();

        let legacy = project.join("sess.jsonl");
        fs::write(
            &legacy,
            r#"{"type":"header","session_id":"sess","cwd":"/x","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}
"#,
        )
        .unwrap();

        migrate_add_timestamp_prefix(&sessions_dir);

        assert!(!legacy.exists(), "legacy unprefixed file should have moved");
        assert!(project.join("1767225600-sess.jsonl").exists());
    }

    #[test]
    fn migrate_add_timestamp_prefix_leaves_already_prefixed_files() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().to_path_buf();
        let project = sessions_dir.join("proj");
        fs::create_dir_all(&project).unwrap();

        let existing = project.join("1700000000-ok.jsonl");
        fs::write(
            &existing,
            r#"{"type":"header","session_id":"ok","cwd":"/x","model":"m","created_at":"2026-01-01T00:00:00Z","version":1}
"#,
        )
        .unwrap();

        migrate_add_timestamp_prefix(&sessions_dir);

        assert!(existing.exists(), "already-prefixed file must stay put");
    }
}
