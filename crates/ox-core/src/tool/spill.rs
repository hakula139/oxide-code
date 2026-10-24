//! The dispatcher's output cap and the spill files that keep the full output a cut drops.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use tracing::warn;

use super::view::ToolResultView;
use super::{Prepared, Tool, ToolCtx, ToolOutput, run_blocking};
use crate::file_tracker::MAX_TRACKED_FILE_SIZE;
use crate::util::fs::create_private_dir_all;
use crate::util::text::format_size;

// ── Output Cap ──

pub(super) const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// Bytes within which a cut moves to a line boundary, so the lines beside an omission marker are
/// whole unless one line spans the window.
const LINE_SNAP_BYTES: usize = 1024;

/// Runs the prepared call on `tool`, applying [`cap_output`] after each tool's row caps, with the
/// call's `.txt` spill file keeping the full output.
pub(crate) async fn run_capped(tool: &dyn Tool, prepared: Prepared, ctx: &ToolCtx) -> ToolOutput {
    let mut output = tool.run(prepared, ctx).await;
    if output.content.len() <= MAX_OUTPUT_BYTES {
        return output;
    }

    let content = std::mem::take(&mut output.content);
    let spill = ctx.spills().file(".txt");
    match run_blocking(move || Ok(cap_output(&content, spill))).await {
        Ok(capped) => {
            output.metadata.result_view = Some(ToolResultView::Text {
                content: capped.clone(),
            });
            output.content = capped;
        }
        Err(e) => return ToolOutput::error(e),
    }
    output
}

/// Writes `content`, which must be longer than [`MAX_OUTPUT_BYTES`], whole to `spill`, and returns
/// up to half the cap left beside an omission marker from each end, all within the cap. Each part
/// meets the marker at a line boundary near its cut, or else at a UTF-8 boundary.
fn cap_output(content: &str, mut spill: Spill) -> String {
    spill.write(content.as_bytes());
    let note = spill.note("output");
    // The full length has at least as many digits as the omitted count the marker ends up with.
    let reserve = omission_marker(content.len(), &note).len() + "\n\n".len();
    let half = (MAX_OUTPUT_BYTES - reserve) / 2;
    let head_end = content.floor_char_boundary(half);
    let tail_start = content.ceil_char_boundary(content.len() - half);
    let bytes = content.as_bytes();
    let ends_line = bytes[head_end] == b'\n';
    let head_end = whole_line_head(&bytes[..head_end], ends_line).len();
    let starts_line = bytes[tail_start - 1] == b'\n';
    let tail_start = content.len() - whole_line_tail(&bytes[tail_start..], starts_line).len();

    let marker = omission_marker(tail_start - head_end, &note);
    let mut capped = String::with_capacity(MAX_OUTPUT_BYTES);
    capped.push_str(&content[..head_end]);
    _ = write!(capped, "\n{marker}\n");
    capped.push_str(&content[tail_start..]);
    capped
}

/// The line that stands in for the `omitted` bytes cut from a result, with `note` saying where the
/// full output is kept.
pub(super) fn omission_marker(omitted: usize, note: &str) -> String {
    format!("... [{omitted} bytes omitted, {note}] ...")
}

/// The part of `head` before its last line break within [`LINE_SNAP_BYTES`] of its end, or all of
/// `head` when it `ends_line` or no break lies that close.
pub(super) fn whole_line_head(head: &[u8], ends_line: bool) -> &[u8] {
    if ends_line {
        return head;
    }
    let window = head.len().saturating_sub(LINE_SNAP_BYTES);
    head[window..]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(head, |pos| &head[..window + pos])
}

/// The part of `tail` after its first line break within [`LINE_SNAP_BYTES`] of its start, or all of
/// `tail` when it `starts_line` or no break lies that close.
pub(super) fn whole_line_tail(tail: &[u8], starts_line: bool) -> &[u8] {
    if starts_line {
        return tail;
    }
    tail.iter()
        .take(LINE_SNAP_BYTES)
        .position(|&b| b == b'\n')
        .map_or(tail, |pos| &tail[pos + 1..])
}

// ── Output Spill ──

/// The directory in [`ToolCtx::artifact_dir`] that keeps the full output of results too large to
/// return whole.
const SPILL_DIR: &str = "tool-results";

/// Most bytes a spill file keeps, so a runaway command cannot fill the disk. It matches the largest
/// file `read` opens, so every spill file reads back.
pub(super) const MAX_SPILL_BYTES: u64 = MAX_TRACKED_FILE_SIZE;

/// Where one call's spill files go, each named by the call's ID and a suffix.
pub(super) struct Spills {
    dir: PathBuf,
    stem: String,
}

impl Spills {
    /// The spills of call `tool_use_id`: [`SPILL_DIR`] in `artifact_dir`, under names that start
    /// with the call's ID.
    pub(super) fn new(artifact_dir: &Path, tool_use_id: &str) -> Self {
        Self {
            dir: artifact_dir.join(SPILL_DIR),
            stem: spill_stem(tool_use_id),
        }
    }

    /// The spill into the file named by the call's ID followed by `suffix`, such as `.stdout.txt`.
    pub(super) fn file(&self, suffix: &str) -> Spill {
        Spill {
            path: self.dir.join(format!("{}{suffix}", self.stem)),
            state: SpillState::Pending,
        }
    }
}

/// `id` with each character other than an ASCII letter, digit, `-`, or `_` replaced by `_`, so a
/// tool call ID from the API cannot name a path outside the spill directory.
fn spill_stem(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A file that keeps the full output of a result too large to return whole. The first write creates
/// it with `0600` perms in a `0700` directory, so a result that fits leaves nothing on disk.
pub(super) struct Spill {
    path: PathBuf,
    state: SpillState,
}

enum SpillState {
    Pending,
    /// `full` once the file reached [`MAX_SPILL_BYTES`] and dropped bytes.
    Open {
        file: File,
        written: u64,
        full: bool,
    },
    /// Why creating or writing the file failed.
    Failed(String),
}

impl Spill {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Returns whether a write has started the spill, whether or not it succeeded.
    pub(super) fn started(&self) -> bool {
        !matches!(self.state, SpillState::Pending)
    }

    /// Appends `bytes`, creating the file on the first write. A failure ends the spill, and
    /// [`Self::note`] reports it.
    pub(super) fn write(&mut self, bytes: &[u8]) {
        if let SpillState::Pending = self.state {
            self.state = match create_spill_file(&self.path) {
                Ok(file) => SpillState::Open {
                    file,
                    written: 0,
                    full: false,
                },
                Err(e) => self.fail(&e),
            };
        }
        let SpillState::Open {
            file,
            written,
            full,
        } = &mut self.state
        else {
            return;
        };

        let room = usize::try_from(MAX_SPILL_BYTES - *written).unwrap_or(usize::MAX);
        let kept = &bytes[..bytes.len().min(room)];
        *full |= kept.len() < bytes.len();
        if let Err(e) = file.write_all(kept) {
            self.state = self.fail(&e.to_string());
        } else {
            *written += kept.len() as u64;
        }
    }

    /// Logs `error` and returns the state that ends the spill.
    fn fail(&self, error: &str) -> SpillState {
        warn!(
            "failed to spill tool output to {}: {error}",
            self.path.display()
        );
        SpillState::Failed(error.to_owned())
    }

    /// Where the full `what`, such as `stdout`, is kept, as the omission marker names it. Call it
    /// only once a write has started the spill.
    pub(super) fn note(&self, what: &str) -> String {
        let path = self.path.display();
        match &self.state {
            SpillState::Pending => unreachable!("a result omits bytes only after they spill"),
            SpillState::Open { full: false, .. } => format!("full {what} saved to {path}"),
            SpillState::Open { full: true, .. } => {
                let size = format_size(MAX_SPILL_BYTES);
                format!("first {size} of {what} saved to {path}")
            }
            SpillState::Failed(error) => {
                format!("saving the full {what} to {path} failed: {error}")
            }
        }
    }
}

/// Creates the new file at `path` with `0600` perms on Unix, and its directory with `0700` perms.
fn create_spill_file(path: &Path) -> Result<File, String> {
    let dir = path
        .parent()
        .expect("a spill file sits in the spill directory");
    create_private_dir_all(dir).map_err(|e| e.root_cause().to_string())?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    options.open(path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::pin::Pin;

    use super::*;
    use crate::permission::{PermissionClass, ReadFilter};
    use crate::tool::grep::GrepTool;
    use crate::tool::local_testing::{ctx, spill_path};
    use crate::tool::testing::bash_registry;
    use crate::tool::{ToolMetadata, ToolRegistry, ToolSpec};

    /// Splits output capped by [`cap_output`] into its head, the text inside its omission marker,
    /// and its tail.
    fn split_capped(out: &str) -> (&str, &str, &str) {
        let (head, rest) = out.split_once("\n... [").unwrap();
        let (marker, tail) = rest.split_once("] ...\n").unwrap();
        (head, marker, tail)
    }

    mod run_capped {
        use super::*;

        /// A tool whose every call returns its text.
        struct Fixed(String);

        impl Tool for Fixed {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "fixed".to_owned(),
                    description: "return fixed text".to_owned(),
                    input_schema: serde_json::json!({"type": "object"}),
                    class: PermissionClass::Read,
                    parallel_safe: true,
                }
            }

            fn run<'a>(
                &'a self,
                _prepared: Prepared,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
                Box::pin(std::future::ready(ToolOutput::success(self.0.clone())))
            }
        }

        async fn run_fixed(text: &str, ctx: &ToolCtx) -> ToolOutput {
            let registry = ToolRegistry::new(vec![Box::new(Fixed(text.to_owned()))]);
            registry
                .run_input("fixed", serde_json::json!({}), ctx)
                .await
        }

        fn oversize_grep_input() -> (tempfile::TempDir, serde_json::Value) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("big.txt");
            let line = "x".repeat(400);
            fs::write(&path, format!("{line}\n").repeat(500)).unwrap();
            let input = serde_json::json!({"pattern": "x", "path": path, "head_limit": 0});
            (dir, input)
        }

        #[tokio::test]
        async fn spills_an_oversized_result_and_returns_its_head_tail_and_path() {
            let dir = tempfile::tempdir().unwrap();
            let full = (0..25_600).fold(String::new(), |mut full, i| {
                _ = writeln!(full, "line {i:06}");
                full
            });
            assert_eq!(full.len(), 300 << 10);

            let output = run_fixed(&full, &ctx(dir.path())).await;

            let (head, marker, tail) = split_capped(&output.content);
            assert!(head.len().abs_diff(tail.len()) < "line 000000\n".len());
            assert!(full.starts_with(head) && full.ends_with(tail));
            let omitted = full.len() - head.len() - tail.len();
            let path = spill_path(dir.path(), ".txt");
            assert_eq!(
                marker,
                format!(
                    "{omitted} bytes omitted, full output saved to {}",
                    path.display()
                ),
            );
            // Each end gives up at most one line to reach a line boundary.
            let floor = MAX_OUTPUT_BYTES - 1 - 2 * "line 000000\n".len();
            assert!((floor..=MAX_OUTPUT_BYTES).contains(&output.content.len()));
            assert_eq!(fs::read_to_string(&path).unwrap(), full);
        }

        #[tokio::test]
        async fn within_cap_leaves_content_untouched_and_writes_nothing() {
            let dir = tempfile::tempdir().unwrap();
            let text = "x".repeat(MAX_OUTPUT_BYTES);

            let output = run_fixed(&text, &ctx(dir.path())).await;

            assert_eq!(output.content, text);
            assert!(!dir.path().join("session").exists());
        }

        #[tokio::test]
        async fn byte_cap_on_grep_spills_and_falls_through_to_text_view() {
            let (dir, input) = oversize_grep_input();
            let registry = ToolRegistry::new(vec![Box::new(GrepTool::new(ReadFilter::default()))]);

            let output = registry
                .run_input("grep", input.clone(), &ctx(dir.path()))
                .await;

            assert!(spill_path(dir.path(), ".txt").exists());
            let view = registry.result_view(
                "grep",
                &input,
                &output.content,
                &output.metadata,
                output.is_error,
                Path::new("/repo"),
            );
            assert_eq!(
                view,
                ToolResultView::Text {
                    content: output.content.clone()
                },
            );
            let metadata: ToolMetadata =
                serde_json::from_value(serde_json::to_value(&output.metadata).unwrap()).unwrap();
            assert_eq!(metadata.result_view, Some(view));
            assert!(
                output
                    .content
                    .contains("bytes omitted, full output saved to")
            );
        }

        #[tokio::test]
        async fn binary_bash_output_fits_the_cap_with_only_its_pipe_spill() {
            // Each byte decodes as the three bytes of U+FFFD.
            let dir = tempfile::tempdir().unwrap();
            let bytes = vec![0xFF; 300 << 10];
            fs::write(dir.path().join("image.bin"), &bytes).unwrap();
            let registry = bash_registry(Vec::new());

            let output = registry
                .run_input(
                    "bash",
                    serde_json::json!({"command": "cat image.bin"}),
                    &ctx(dir.path()),
                )
                .await;

            assert!(!output.is_error, "{}", output.content);
            let stdout = spill_path(dir.path(), ".stdout.txt");
            let note = format!("full stdout saved to {}] ...", stdout.display());
            assert!(output.content.contains(&note));
            assert!(
                fs::read(&stdout).unwrap() == bytes,
                "the spill holds the raw bytes"
            );
            let spills: Vec<_> = fs::read_dir(stdout.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(spills, [stdout]);
        }
    }

    mod cap_output {
        use super::*;

        #[test]
        fn cuts_both_ends_at_line_boundaries() {
            let dir = tempfile::tempdir().unwrap();
            let content = (0..20_000).fold(String::new(), |mut content, i| {
                _ = writeln!(content, "line number {i}");
                content
            });

            let out = cap_output(&content, ctx(dir.path()).spills().file(".txt"));

            let (head, marker, tail) = split_capped(&out);
            assert!(
                content.starts_with(&format!("{head}\n")),
                "{:?}",
                head.lines().last()
            );
            let tail_start = content.len() - tail.len();
            assert!(content[..tail_start].ends_with('\n') && content.ends_with(tail));
            let omitted = tail_start - head.len();
            assert!(marker.starts_with(&format!("{omitted} bytes omitted, ")));
            assert!(out.len() <= MAX_OUTPUT_BYTES, "{}", out.len());
        }

        #[test]
        fn keeps_whole_lines_that_meet_the_cuts() {
            let dir = tempfile::tempdir().unwrap();
            let content = "\n".repeat(MAX_OUTPUT_BYTES + 1);

            let out = cap_output(&content, ctx(dir.path()).spills().file(".txt"));

            let (head, marker, tail) = split_capped(&out);
            assert_eq!(head.len(), tail.len());
            let omitted = content.len() - head.len() - tail.len();
            assert!(marker.starts_with(&format!("{omitted} bytes omitted, ")));
        }

        #[test]
        fn cuts_both_ends_at_char_boundaries() {
            // Wherever the cuts fall, three of the four offsets put each one inside a 4-byte char.
            for offset in 0..4 {
                let dir = tempfile::tempdir().unwrap();
                let pad = "a".repeat(offset);
                let content = format!("{pad}{}{pad}", "🦀".repeat(MAX_OUTPUT_BYTES / 2));

                let out = cap_output(&content, ctx(dir.path()).spills().file(".txt"));

                let (head, marker, tail) = split_capped(&out);
                assert!(content.starts_with(head) && content.ends_with(tail));
                let omitted = content.len() - head.len() - tail.len();
                assert!(marker.starts_with(&format!("{omitted} bytes omitted, ")));
                assert!(
                    out.len() <= MAX_OUTPUT_BYTES,
                    "offset {offset}: {}",
                    out.len()
                );
            }
        }

        #[test]
        fn fits_the_cap_beside_a_long_spill_path() {
            let dir = tempfile::tempdir().unwrap();
            let cwd = (0..3).fold(dir.path().to_owned(), |cwd, _| cwd.join("d".repeat(100)));
            let content = "x".repeat(MAX_OUTPUT_BYTES + 1);

            let out = cap_output(&content, ctx(&cwd).spills().file(".txt"));

            let path = spill_path(&cwd, ".txt");
            assert!(path.as_os_str().len() > 300);
            assert!(out.len() <= MAX_OUTPUT_BYTES, "{}", out.len());
            let (_, marker, _) = split_capped(&out);
            assert!(marker.ends_with(&format!("full output saved to {}", path.display())));
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
        }

        #[test]
        fn names_why_the_spill_failed_in_place_of_its_path() {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("session"), "a file").unwrap();
            let content = "x".repeat(MAX_OUTPUT_BYTES * 2);

            let out = cap_output(&content, ctx(dir.path()).spills().file(".txt"));

            let (_, marker, _) = split_capped(&out);
            let path = spill_path(dir.path(), ".txt");
            let failed = format!("saving the full output to {} failed: ", path.display());
            assert!(marker.contains(&failed), "{marker}");
            assert!(out.len() <= MAX_OUTPUT_BYTES, "{}", out.len());
        }
    }

    mod whole_line_head {
        use super::*;

        #[test]
        fn ends_before_the_last_line_break_near_the_end() {
            assert_eq!(whole_line_head(b"one\ntwo\n333", false), b"one\ntwo");
            assert_eq!(whole_line_head(b"one\ntwo\n", false), b"one\ntwo");
        }

        #[test]
        fn keeps_a_head_that_ends_a_line() {
            assert_eq!(whole_line_head(b"one\ntwo", true), b"one\ntwo");
        }

        #[test]
        fn moves_only_to_a_line_break_within_the_window() {
            let head = [b"\n".as_slice(), &[b'a'; LINE_SNAP_BYTES]].concat();
            assert_eq!(whole_line_head(&head, false), head);
            let near = [b"\n".as_slice(), &[b'a'; LINE_SNAP_BYTES - 1]].concat();
            assert_eq!(whole_line_head(&near, false), b"");
        }
    }

    mod whole_line_tail {
        use super::*;

        #[test]
        fn starts_after_the_first_line_break_near_the_start() {
            assert_eq!(whole_line_tail(b"33\nfour\nfive\n", false), b"four\nfive\n");
        }

        #[test]
        fn keeps_a_tail_that_starts_a_line() {
            assert_eq!(whole_line_tail(b"four\nfive\n", true), b"four\nfive\n");
        }

        #[test]
        fn moves_only_to_a_line_break_within_the_window() {
            let tail = [&[b'b'; LINE_SNAP_BYTES], b"\n".as_slice()].concat();
            assert_eq!(whole_line_tail(&tail, false), tail);
            let near = [&[b'b'; LINE_SNAP_BYTES - 1], b"\nc".as_slice()].concat();
            assert_eq!(whole_line_tail(&near, false), b"c");
        }
    }

    mod spill_stem {
        use super::*;

        #[test]
        fn replaces_characters_that_could_leave_the_spill_directory() {
            assert_eq!(spill_stem("toolu_01Ab-9"), "toolu_01Ab-9");
            assert_eq!(spill_stem("../a/b\\c.d"), "___a_b_c_d");
        }
    }

    mod write {
        use super::*;

        #[test]
        fn keeps_at_most_the_size_cap_and_says_so() {
            let dir = tempfile::tempdir().unwrap();
            let mut spill = ctx(dir.path()).spills().file(".stdout.txt");
            let chunk = vec![b'a'; 1 << 20];

            for _ in 0..=MAX_SPILL_BYTES >> 20 {
                spill.write(&chunk);
            }

            let path = spill_path(dir.path(), ".stdout.txt");
            let spilled = fs::metadata(&path).unwrap().len();
            assert_eq!(spilled, MAX_SPILL_BYTES);
            assert_eq!(
                spill.note("stdout"),
                format!("first 10 MiB of stdout saved to {}", path.display()),
            );
        }

        #[test]
        fn names_the_error_of_a_write_the_file_refuses() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("toolu_test.stdout.txt");
            fs::write(&path, "").unwrap();
            let read_only = || File::open(&path).unwrap();
            let mut spill = Spill {
                path: path.clone(),
                state: SpillState::Open {
                    file: read_only(),
                    written: 0,
                    full: false,
                },
            };

            spill.write(b"x");

            let error = read_only().write_all(b"x").unwrap_err();
            assert_eq!(
                spill.note("stdout"),
                format!(
                    "saving the full stdout to {} failed: {error}",
                    path.display()
                ),
            );
        }
    }

    mod create_spill_file {
        use super::*;

        #[test]
        fn refuses_to_replace_an_existing_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("toolu_test.txt");
            fs::write(&path, "earlier").unwrap();

            assert!(create_spill_file(&path).is_err());

            assert_eq!(fs::read_to_string(&path).unwrap(), "earlier");
        }

        #[cfg(unix)]
        #[test]
        fn creates_a_private_file_in_a_private_directory() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let path = spill_path(dir.path(), ".txt");

            create_spill_file(&path).unwrap();

            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
    }
}
