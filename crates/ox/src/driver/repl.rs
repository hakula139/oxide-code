//! Bare REPL driver for `--no-tui`: prompts and slash commands from stdin, replies on stdout.

use std::io::{self, BufRead, Write};
use std::mem;
use std::path::Path;
use std::pin::Pin;
use std::sync::mpsc as std_mpsc;
use std::task::{Context, Poll, ready};
use std::thread;

use anyhow::{Context as _, Result, bail};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncRead, ReadBuf};
use tokio::sync::mpsc;

use ox_core::command::{self, Parsed};
use ox_core::engine::{Engine, EngineHandle};
use ox_core::permission::rule::Rule;
use ox_core::permission::{Basis, SessionGrant};
use ox_core::protocol::notice::ENGINE_STOPPED;
use ox_core::protocol::{ApprovalDecision, Event, Op, OpEnd};
use ox_core::tool::view::{ApprovalBody, ApprovalPreview, DiffRow};
use ox_core::util::text::escape_hidden;
use ox_tui::glyphs::{HUNK_GAP, diff_cap_row, diff_sign};

use super::stdio::{Approver, Output, OutputFormat, RunEnd, run_op};
use super::{Exit, Shutdown, Signals, decode_input};

/// The command `/help`, which the REPL answers itself.
const HELP: &str = "help";

/// Reads prompts and slash commands until EOF, `/exit`, a signal, or stdout's reader going away,
/// then finalizes the session. EOF after an operation failed, a command was rejected, or output
/// failed exits with failure, as a lost stdout always does. An attended `engine` asks its approvals
/// on stdin, while an unattended one asks none, so a piped line is never taken as an answer. In an
/// attended run, SIGINT interrupts the running operation or discards the line at the prompt, as
/// shells do. Every other signal ends the run. `first`, when given, runs as the first input line.
pub(crate) async fn run(
    engine: Engine,
    show_thinking: bool,
    first: Option<String>,
) -> Result<Exit> {
    let attended = engine.attended();
    let project_root = engine.project_root().to_owned();
    let handle = engine.spawn();
    let mut output = Output::new(
        OutputFormat::Text,
        show_thinking,
        io::stdout(),
        io::stderr(),
    );
    let input =
        LineReader::spawn(|| io::stdin().lock()).context("failed to start reading stdin")?;
    let prompter = Prompter::new(input, io::stderr());
    let mut signals = Signals::new();
    drive(
        handle,
        &project_root,
        &mut output,
        first,
        prompter,
        attended,
        &mut async || signals.recv().await,
    )
    .await
}

/// Runs the REPL over `first`, then `prompter`'s input. Each `signal` call awaits the next signal.
async fn drive<R, W, O, E>(
    mut handle: EngineHandle,
    project_root: &Path,
    output: &mut Output<O, E>,
    mut first: Option<String>,
    mut prompter: Prompter<R, W>,
    attended: bool,
    signal: &mut impl AsyncFnMut() -> Shutdown,
) -> Result<Exit>
where
    R: AsyncBufRead + Unpin,
    W: Write,
    O: Write,
    E: Write,
{
    let mut stopped = None;
    let mut failed = false;

    let result: Result<()> = async {
        loop {
            output.render_queued(&mut handle.events);
            let line = if let Some(line) = first.take() {
                line
            } else {
                output.record(prompter.prompt("> ").map_err(Into::into));
                let line = tokio::select! {
                    line = prompter.next_line() => line?,
                    caught = signal() => {
                        output.break_line();
                        if attended && caught == Shutdown::Interrupt {
                            continue;
                        }
                        stopped = Some(caught);
                        break;
                    }
                };
                let Some(line) = line else {
                    break;
                };
                line
            };

            let input = line.trim();
            if input.is_empty() {
                continue;
            }
            let op = match command::parse_slash(input) {
                Some(parsed) if parsed.name == HELP => {
                    output.render(Event::Notice(repl_help()));
                    continue;
                }
                Some(parsed) => match command_op(&parsed, project_root) {
                    Ok(op) => op,
                    Err(message) => {
                        output.render(Event::Error(message));
                        failed = true;
                        continue;
                    }
                },
                None => Op::prompt(command::unescape_literal(input)),
            };
            if op == Op::Shutdown {
                break;
            }

            let approver = attended.then_some(&mut prompter);
            match run_op(
                &mut handle.events,
                &handle.ops,
                output,
                signal,
                op,
                approver,
            )
            .await
            {
                RunEnd::Ended(OpEnd::Failed) => failed = true,
                RunEnd::Ended(_) => {}
                RunEnd::EngineStopped => bail!(ENGINE_STOPPED),
                RunEnd::Signal(caught) => {
                    stopped = Some(caught);
                    break;
                }
            }
            if output.stdout_lost() {
                break;
            }
        }
        Ok(())
    }
    .await;

    let failure = handle.shutdown().await?;
    if let Some(failure) = &failure {
        output.session_write_error(failure);
    }
    failed |= failure.is_some();
    result?;
    if let Some(signal) = stopped {
        return Ok(Exit::Signal(signal));
    }
    output.finish().context("failed to print output")?;
    Ok(if failed { Exit::Failure } else { Exit::Success })
}

/// `/name (aliases)` for every command the REPL runs, comma-separated.
fn repl_command_list() -> String {
    let mut names: Vec<&str> = command::names().collect();
    names.push(HELP);
    names.sort_unstable();
    names
        .into_iter()
        .map(command::label)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The command list `/help` prints in the REPL.
fn repl_help() -> String {
    format!(
        "Commands: {}. Use `//text` to send a literal slash.",
        repl_command_list()
    )
}

/// The op a slash command runs in the REPL at `project_root`, or the message saying why it cannot
/// run here.
fn command_op(parsed: &Parsed, project_root: &Path) -> Result<Op, String> {
    command::typed_op(parsed, project_root).unwrap_or_else(|| {
        Err(format!(
            "`/{name}` is not a REPL command. \
             Available: {names}. \
             Use `//{name}` to send the literal text.",
            name = parsed.name,
            names = repl_command_list(),
        ))
    })
}

// ── Prompter ──

const CHOICES: &str = "[y] approve  [n] deny  [f] feedback  [x] abort: ";
const GRANT_CHOICES: &str =
    "[y] approve  [a] allow for session  [n] deny  [f] feedback  [x] abort: ";

/// Answers approvals with lines read from the REPL's input, writing the call and its choices to
/// `out`. The input ending aborts the turn.
struct Prompter<R, W> {
    input: R,
    out: W,
}

/// What the session-rule prompt answered.
enum RuleAnswer {
    /// A rule the grant accepts.
    Accepted(Rule),
    /// Why the grant rejects the typed pattern.
    Rejected(anyhow::Error),
    /// The input ended, or could not be read, before an answer.
    Ended,
}

impl<R: AsyncBufRead + Unpin, W: Write> Approver for Prompter<R, W> {
    async fn decide(
        &mut self,
        preview: &ApprovalPreview,
        basis: &Basis,
        grant: Option<&SessionGrant>,
    ) -> Result<ApprovalDecision> {
        write_preview(&mut self.out, preview, basis)?;
        let choices = if grant.is_some() {
            GRANT_CHOICES
        } else {
            CHOICES
        };
        loop {
            let Some(answer) = self.ask(choices).await? else {
                return Ok(ApprovalDecision::Abort);
            };
            match (answer.to_ascii_lowercase().as_str(), grant) {
                ("y" | "yes", _) => return Ok(ApprovalDecision::Approve),
                ("a", Some(grant)) => match self.rule(grant).await? {
                    RuleAnswer::Accepted(rule) => {
                        return Ok(ApprovalDecision::AllowForSession {
                            rule: Box::new(rule),
                        });
                    }
                    RuleAnswer::Rejected(error) => {
                        writeln!(self.out, "Rule rejected: {error:#}")?;
                    }
                    RuleAnswer::Ended => return Ok(ApprovalDecision::Abort),
                },
                ("n" | "no", _) => return Ok(ApprovalDecision::Deny),
                ("f", _) => match self.ask("Feedback for the model: ").await? {
                    Some(feedback) if !feedback.is_empty() => {
                        return Ok(ApprovalDecision::DenyWithFeedback { feedback });
                    }
                    Some(_) => {}
                    None => return Ok(ApprovalDecision::Abort),
                },
                ("x", _) => return Ok(ApprovalDecision::Abort),
                _ => writeln!(self.out, "Answer with one of the letters in brackets")?,
            }
        }
    }
}

impl<R: AsyncBufRead + Unpin, W: Write> Prompter<R, W> {
    fn new(input: R, out: W) -> Self {
        Self { input, out }
    }

    fn prompt(&mut self, prompt: &str) -> io::Result<()> {
        write!(self.out, "{prompt}")?;
        self.out.flush()
    }

    /// Reads one line without its terminator, decoded lossily, or `None` at the input's end.
    async fn next_line(&mut self) -> io::Result<Option<String>> {
        let mut line = Vec::new();
        if self.input.read_until(b'\n', &mut line).await? == 0 {
            return Ok(None);
        }
        let text = decode_input(line);
        Ok(Some(text.trim_end_matches(['\r', '\n']).to_owned()))
    }

    /// Writes `prompt` before reading the trimmed answer, or `None` once the input ends or fails.
    /// An output error leaves the answer unread.
    async fn ask(&mut self, prompt: &str) -> Result<Option<String>> {
        self.prompt(prompt)?;
        match self.next_line().await {
            Ok(Some(line)) => Ok(Some(line.trim().to_owned())),
            Ok(None) => {
                writeln!(self.out)?;
                Ok(None)
            }
            Err(e) => {
                writeln!(self.out, "\nCould not read the answer: {e}")?;
                Ok(None)
            }
        }
    }

    /// Reads and validates a session-rule pattern, using the suggestion for empty input.
    async fn rule(&mut self, grant: &SessionGrant) -> Result<RuleAnswer> {
        let suggested = grant.suggested().pattern().to_owned();
        let Some(answer) = self
            .ask(&format!("Pattern to allow [{suggested}]: "))
            .await?
        else {
            return Ok(RuleAnswer::Ended);
        };
        let text = if answer.is_empty() {
            &suggested
        } else {
            &answer
        };
        Ok(match grant.accept(text) {
            Ok(rule) => RuleAnswer::Accepted(rule),
            Err(error) => RuleAnswer::Rejected(error),
        })
    }
}

/// Writes the call `preview` shows, one line per command line or diff row, then why `basis` made it
/// ask when a rule holds it.
fn write_preview(out: &mut impl Write, preview: &ApprovalPreview, basis: &Basis) -> io::Result<()> {
    writeln!(out, "{} needs approval:", escape_hidden(&preview.title))?;
    match &preview.body {
        ApprovalBody::Command(command) => {
            for line in command.lines() {
                writeln!(out, "  {}", escape_hidden(line))?;
            }
        }
        ApprovalBody::Diff(diff) => {
            let width = diff.max_number().to_string().len();
            for row in diff.rows() {
                match row {
                    DiffRow::Line(line) => {
                        let sign = diff_sign(line.kind);
                        let text = escape_hidden(&line.text);
                        writeln!(out, "  {:>width$}{sign}{text}", line.number)?;
                    }
                    DiffRow::Gap => writeln!(out, "  {HUNK_GAP:>width$}")?,
                    DiffRow::Omitted(lines) => writeln!(out, "  {}", diff_cap_row(lines))?,
                }
            }
        }
    }
    if let Some(reason) = basis.reason() {
        writeln!(out, "{}", escape_hidden(&reason))?;
    }
    Ok(())
}

// ── Input ──

/// Lines that a dedicated thread reads from a blocking reader, one per request. A read on tokio's
/// blocking pool, as `tokio::io::stdin` makes, cannot be cancelled and holds the runtime's shutdown
/// until a line arrives, so tokio recommends a thread for interactive input. The thread reads only
/// while a caller waits for a line, so a line typed during a turn stays with the terminal, which
/// discards it on Ctrl+C. An end of input ends only the read that meets it, as a read of the file
/// descriptor does. A read pending at exit does not delay it.
struct LineReader {
    requests: std_mpsc::Sender<()>,
    lines: mpsc::UnboundedReceiver<io::Result<Vec<u8>>>,
    /// Whether the thread owes a line, which a cancelled read leaves to the next one.
    requested: bool,
    line: Vec<u8>,
    consumed: usize,
    /// Whether `line` stopped at the input's end, which the next read then reports.
    ended: bool,
}

impl LineReader {
    /// Starts the thread, which reads from the input `open` returns.
    fn spawn<R: BufRead>(open: impl FnOnce() -> R + Send + 'static) -> io::Result<Self> {
        let (requests, requested) = std_mpsc::channel();
        let (sender, lines) = mpsc::unbounded_channel();
        thread::Builder::new()
            .name("ox-input".to_owned())
            .spawn(move || {
                let mut input = open();
                while requested.recv().is_ok() {
                    let mut line = Vec::new();
                    let read = input.read_until(b'\n', &mut line).map(|_| line);
                    if sender.send(read).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests,
            lines,
            requested: false,
            line: Vec::new(),
            consumed: 0,
            ended: false,
        })
    }
}

impl AsyncRead for LineReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let available = ready!(self.as_mut().poll_fill_buf(cx))?;
        let len = available.len().min(buf.remaining());
        buf.put_slice(&available[..len]);
        self.consume(len);
        Poll::Ready(Ok(()))
    }
}

impl AsyncBufRead for LineReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        if this.consumed < this.line.len() {
            return Poll::Ready(Ok(&this.line[this.consumed..]));
        }
        if mem::take(&mut this.ended) {
            return Poll::Ready(Ok(&[]));
        }
        if !this.requested {
            // A thread that is gone has also closed `lines`, which reads as the input's end.
            _ = this.requests.send(());
            this.requested = true;
        }
        let read = ready!(this.lines.poll_recv(cx));
        this.requested = false;
        match read {
            Some(Ok(line)) if !line.is_empty() => {
                this.ended = !line.ends_with(b"\n");
                this.line = line;
                this.consumed = 0;
                Poll::Ready(Ok(&this.line))
            }
            Some(Ok(_)) | None => Poll::Ready(Ok(&[])),
            Some(Err(e)) => Poll::Ready(Err(e)),
        }
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.get_mut().consumed += amt;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use indoc::{formatdoc, indoc};
    use tokio::io::BufReader;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use ox_core::engine::EnginePanic;
    use ox_core::engine::testing::{
        engine, mount_reply, mount_stalled_reply_once, mount_tool_call_once, panic_message,
        panicked_engine, request_bodies, unsaved_session_handle, until_requested, with_pinned_home,
    };
    use ox_core::permission::testing::exec_grant;
    use ox_core::permission::{Mode, PermissionClass, Tier};
    use ox_core::session::store::testing::{saved_message_count, saved_messages, test_store};
    use ox_core::tool::testing::{bash_preview, bash_registry, diff_line};
    use ox_core::tool::view::{Diff, DiffHunk, DiffLineKind};
    use ox_tui::glyphs::INTERRUPTED_MARKER;

    use super::*;
    use crate::driver::testing::{ClosedPipe, FailingWriter, Failure, Screen, no_signal, output};

    /// Serves one chunk per read, as a terminal serves one line. An empty chunk ends the input for
    /// one read, as Ctrl+D at a terminal does, and the input stays ended after the last chunk.
    struct Chunks(std::vec::IntoIter<&'static str>);

    impl Chunks {
        fn new<const N: usize>(chunks: [&'static str; N]) -> Self {
            Self(Vec::from(chunks).into_iter())
        }
    }

    impl io::Read for Chunks {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let chunk = self.0.next().unwrap_or_default();
            io::Read::read(&mut chunk.as_bytes(), buf)
        }
    }

    mod drive {
        use super::*;

        #[tokio::test]
        async fn skips_blank_lines_and_exits_cleanly_at_eof() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let (mut output, stdout, _) = output(OutputFormat::Text);
                let input: &[u8] = b"\n   \nhello\n";

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output,
                    None,
                    Prompter::new(input, io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                assert_eq!(stdout.text(), "reply\n");
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 1, "blank lines never reach the model");
                let last = requests[0]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], "hello");
            })
            .await;
        }

        #[tokio::test]
        async fn exits_with_failure_when_the_session_cannot_be_saved() {
            let failure = "session writer task has stopped";
            let (mut output, _, stderr) = output(OutputFormat::Text);
            let input: &[u8] = b"hello\n";
            let handle = unsaved_session_handle(failure);

            let exit = Box::pin(drive(
                handle,
                Path::new("/repo"),
                &mut output,
                None,
                Prompter::new(input, io::sink()),
                true,
                &mut no_signal,
            ))
            .await
            .unwrap();

            assert_eq!(exit, Exit::Failure);
            assert_eq!(
                stderr.text(),
                format!("Error: Session write failed: {failure}\n")
            );
        }

        #[tokio::test]
        async fn runs_the_first_prompt_before_reading_input() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let first = Some("Explain\nthis".to_owned());
                let input: &[u8] = b"second\n";

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output(OutputFormat::Text).0,
                    first,
                    Prompter::new(input, io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let prompts: Vec<serde_json::Value> = request_bodies(&server)
                    .await
                    .iter()
                    .map(|request| {
                        let last = request["messages"].as_array().unwrap().last().unwrap();
                        last["content"][0]["text"].clone()
                    })
                    .collect();
                assert_eq!(prompts, ["Explain\nthis", "second"]);
            })
            .await;
        }

        #[tokio::test]
        async fn retains_prompt_write_and_flush_failures_after_a_reply_is_saved() {
            with_pinned_home(async {
                for failure in [Failure::Write("> "), Failure::Flush] {
                    let server = MockServer::start().await;
                    mount_reply(&server, "reply").await;
                    let dir = tempfile::tempdir().unwrap();
                    let store = test_store(dir.path());
                    let engine = engine(server.uri(), &store, dir.path()).await;
                    let session_id = engine.session_id();
                    let (mut output, stdout, _) = output(OutputFormat::Text);
                    let mut out = FailingWriter::new(failure);
                    let input: &[u8] = b"hello\n/exit\n";

                    let result = Box::pin(drive(
                        engine.spawn(),
                        dir.path(),
                        &mut output,
                        None,
                        Prompter::new(input, &mut out),
                        true,
                        &mut no_signal,
                    ))
                    .await;
                    assert_eq!(
                        format!("{:#}", result.unwrap_err()),
                        format!("failed to print output: {}", failure.message()),
                        "{failure:?}",
                    );

                    assert!(out.screen.text().ends_with("> "), "{failure:?}");
                    assert_eq!(stdout.text(), "reply\n", "{failure:?}");
                    let text: Vec<serde_json::Value> = saved_messages(&store, &session_id)
                        .iter()
                        .map(|message| message["content"][0]["text"].clone())
                        .collect();
                    assert_eq!(text, ["hello", "reply"], "{failure:?}");
                }
            })
            .await;
        }

        #[tokio::test]
        async fn keeps_reading_after_a_failed_turn_and_exits_with_failure() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(ResponseTemplate::new(400))
                    .up_to_n_times(1)
                    .mount(&server)
                    .await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let instructions = dir.path().join("CLAUDE.md");
                fs::write(&instructions, b"\xff\xfe").unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let session_id = engine.session_id();
                let (mut output, stdout, stderr) = output(OutputFormat::Text);
                let input: &[u8] = indoc! {b"
                    first
                    /resume zzzz
                    //literal
                "};

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output,
                    None,
                    Prompter::new(input, io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Failure);
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 2, "/resume must not reach the model");
                let last = requests[1]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], "/literal");
                assert_eq!(
                    stderr.text(),
                    formatdoc! {"
                        Skipped instruction file {}: stream did not contain valid UTF-8
                        Error: Anthropic API error (HTTP 400 Bad Request)
                        Error: Resume failed (still on session {}): no session matching `zzzz`
                        ",
                        instructions.display(),
                        &session_id[..8],
                    },
                );
                assert_eq!(stdout.text(), "reply\n");
                assert_eq!(
                    saved_message_count(&store, &session_id),
                    3,
                    "the failed prompt, the literal, and its reply are saved",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn reports_a_lost_reply_after_later_output_succeeds_and_the_session_is_saved() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let session_id = engine.session_id();
                let failure = Failure::Write("reply");
                let stdout = FailingWriter::new(failure);
                let screen = stdout.screen.clone();
                let mut output = Output::new(OutputFormat::Text, false, stdout, io::sink());
                let input: &[u8] = b"first\nsecond\n";

                let result = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output,
                    None,
                    Prompter::new(input, io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await;
                assert_eq!(
                    format!("{:#}", result.unwrap_err()),
                    format!("failed to print output: {}", failure.message()),
                );

                assert_eq!(screen.text(), "reply\n", "only the second reply shows");
                let expected: Vec<serde_json::Value> = [
                    ("user", "first"),
                    ("assistant", "reply"),
                    ("user", "second"),
                    ("assistant", "reply"),
                ]
                .into_iter()
                .map(|(role, text)| {
                    serde_json::json!({
                        "role": role,
                        "content": [{"type": "text", "text": text}],
                    })
                })
                .collect();
                assert_eq!(saved_messages(&store, &session_id), expected);
            })
            .await;
        }

        #[tokio::test]
        async fn runs_each_typed_command_as_its_op() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let first_id = engine.session_id();
                let (mut output, _, stderr) = output(OutputFormat::Text);
                let log = (1..=150)
                    .map(|n| format!("request {n} served in 12 ms."))
                    .collect::<Vec<_>>()
                    .join(" ");
                let prefix = &first_id[..8];
                let input = formatdoc! {"
                    Why does the login service log stop here? {log}
                    second
                    /compact focus on login
                    /rename Fix login
                    /model sonnet
                    /effort low
                    /clear
                    /resume {prefix}
                "};

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output,
                    None,
                    Prompter::new(input.as_bytes(), io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let requests = request_bodies(&server).await;
                assert_eq!(
                    requests.len(),
                    3,
                    "only the prompts and the summary reach the model"
                );
                assert!(
                    requests[2].to_string().contains("focus on login"),
                    "{}",
                    requests[2]
                );
                let stderr = stderr.text();
                let confirmations: Vec<&str> = stderr.lines().collect();
                let [compacted, renamed, model, effort, rolled, resumed] = confirmations[..] else {
                    panic!("one confirmation per command: {stderr}");
                };
                assert!(compacted.contains("focus on login"), "{compacted}");
                assert_eq!(renamed, "Renamed the session to Fix login");
                assert_eq!(
                    model,
                    "Switched to Claude Sonnet 5.5 (claude-sonnet-5-5) · effort medium",
                );
                assert_eq!(effort, "Effort set to low");
                assert!(rolled.starts_with("Started session "), "{rolled}");
                assert_eq!(resumed, format!("Resumed session {prefix}: Fix login"));
            })
            .await;
        }

        #[tokio::test]
        async fn runs_init_as_the_update_prompt_when_instruction_files_exist() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                fs::write(dir.path().join("AGENTS.md"), "# AGENTS.md\n").unwrap();
                let input: &[u8] = b"/init\n";

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output(OutputFormat::Text).0,
                    None,
                    Prompter::new(input, io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let Ok(Some(prompt)) = command::init::resolve("", dir.path()) else {
                    panic!("bare /init must resolve once AGENTS.md exists");
                };
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 1);
                let last = requests[0]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], prompt);
            })
            .await;
        }

        #[tokio::test]
        async fn init_without_an_instruction_file_runs_only_with_a_file_name() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let (mut output, _, stderr) = output(OutputFormat::Text);
                let input: &[u8] = indoc! {b"
                    /init
                    /init CLAUDE.md
                "};

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output,
                    None,
                    Prompter::new(input, io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Failure);
                assert_eq!(
                    stderr.text(),
                    "Error: /init: This project has no AGENTS.md or CLAUDE.md. \
                     Run `/init AGENTS.md` or `/init CLAUDE.md` to create one.\n",
                );
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 1, "the bare /init must not reach the model");
                let last = requests[0]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(
                    last["content"][0]["text"],
                    command::init::create_prompt(dir.path(), "CLAUDE.md"),
                );
                let titles: Vec<String> = store
                    .list()
                    .unwrap()
                    .into_iter()
                    .filter_map(|info| info.title)
                    .collect();
                assert_eq!(titles, ["/init CLAUDE.md"]);
            })
            .await;
        }

        #[tokio::test]
        async fn exit_stops_reading_and_exits_cleanly() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let input: &[u8] = indoc! {b"
                    /quit
                    hello
                "};

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output(OutputFormat::Text).0,
                    None,
                    Prompter::new(input, io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
            })
            .await;
        }

        #[tokio::test]
        async fn rejects_a_command_it_cannot_run_and_exits_with_failure() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let (mut output, _, stderr) = output(OutputFormat::Text);
                let input: &[u8] = indoc! {b"
                    /effort turbo
                    /model
                    /theme
                "};

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output,
                    None,
                    Prompter::new(input, io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Failure);
                assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
                let stderr = stderr.text();
                let reports: Vec<&str> = stderr.lines().collect();
                assert_eq!(reports.len(), 3, "{reports:?}");
                assert!(
                    reports[0].starts_with("Error: /effort: Unknown effort: `turbo`."),
                    "{reports:?}"
                );
                assert_eq!(
                    reports[1],
                    "Error: /model: Missing model ID. Usage: `/model <id>`."
                );
                assert_eq!(
                    reports[2],
                    "Error: `/theme` is not a REPL command. \
                     Available: /clear (new, reset), /compact, /effort, /exit (quit), /help, \
                     /init, /mode, /model, /rename, /resume (continue). \
                     Use `//theme` to send the literal text.",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn help_prints_the_command_list_and_keeps_reading() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let (mut output, stdout, stderr) = output(OutputFormat::Text);

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output,
                    None,
                    Prompter::new(&b"/help\n"[..], io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
                assert_eq!(stdout.text(), "");
                assert_eq!(
                    stderr.text(),
                    "Commands: /clear (new, reset), /compact, /effort, /exit (quit), /help, \
                     /init, /mode, /model, /rename, /resume (continue). \
                     Use `//text` to send a literal slash.\n",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn answers_an_approval_from_the_next_input_line() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_tool_call_once(&server, "call-1", "bash", r#"{"command":"git push"}"#).await;
                mount_reply(&server, "Done.").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                    .await
                    .with_tools(bash_registry(Vec::new()));
                let input = indoc! {"
                    list files
                    f
                    Use git ls-files.
                "};

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output(OutputFormat::Text).0,
                    None,
                    Prompter::new(input.as_bytes(), io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let requests = request_bodies(&server).await;
                let result =
                    &requests[1]["messages"].as_array().unwrap().last().unwrap()["content"][0];
                assert_eq!(result["type"], "tool_result");
                assert_eq!(
                    result["content"],
                    "The user denied the bash call and wrote: Use git ls-files.",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn an_input_end_at_an_approval_aborts_only_that_turn() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_tool_call_once(&server, "call-1", "bash", r#"{"command":"git push"}"#).await;
                mount_reply(&server, "Done.").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                    .await
                    .with_tools(bash_registry(Vec::new()));
                let (mut output, stdout, _) = output(OutputFormat::Text);
                let input = LineReader::spawn(|| {
                    io::BufReader::new(Chunks::new(["push it\n", "", "and then?\n"]))
                })
                .unwrap();

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output,
                    None,
                    Prompter::new(input, io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                assert_eq!(stdout.text(), "Done.\n");
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 2, "the aborted call never runs");
                let last = requests[1]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], "and then?");
            })
            .await;
        }

        #[tokio::test]
        async fn retains_approval_output_failures_after_aborting_and_saving_the_turn() {
            with_pinned_home(async {
                for failure in [
                    Failure::Write("needs approval"),
                    Failure::Write("[y]"),
                    Failure::Flush,
                ] {
                    let server = MockServer::start().await;
                    mount_tool_call_once(&server, "call-1", "bash", r#"{"command":"git push"}"#)
                        .await;
                    let dir = tempfile::tempdir().unwrap();
                    let store = test_store(dir.path());
                    let engine = engine(server.uri(), &store, dir.path())
                        .await
                        .with_tools(bash_registry(Vec::new()));
                    let session_id = engine.session_id();
                    let (mut output, _, stderr) = output(OutputFormat::Text);
                    let mut out = FailingWriter::new(failure);
                    let input: &[u8] = b"/exit\n";

                    let result = Box::pin(drive(
                        engine.spawn(),
                        dir.path(),
                        &mut output,
                        Some("push it".to_owned()),
                        Prompter::new(input, &mut out),
                        true,
                        &mut no_signal,
                    ))
                    .await;
                    assert_eq!(
                        format!("{:#}", result.unwrap_err()),
                        format!("failed to print output: {}", failure.message()),
                        "{failure:?}",
                    );

                    assert!(out.screen.text().ends_with("> "), "{failure:?}");
                    assert!(
                        stderr.text().ends_with(&format!("{INTERRUPTED_MARKER}\n")),
                        "{failure:?}: {}",
                        stderr.text(),
                    );
                    assert_eq!(request_bodies(&server).await.len(), 1, "{failure:?}");
                    let messages = saved_messages(&store, &session_id);
                    assert_eq!(messages[0]["content"][0]["text"], "push it", "{failure:?}");
                    assert_eq!(messages[1]["content"][0]["id"], "call-1", "{failure:?}");
                    let results: Vec<&serde_json::Value> = messages
                        .iter()
                        .flat_map(|message| message["content"].as_array().unwrap())
                        .filter(|block| block["type"] == "tool_result")
                        .collect();
                    assert!(
                        results.iter().all(|result| result["is_error"] == true),
                        "{failure:?}: the call never ran: {results:?}",
                    );
                    assert_eq!(
                        messages.last().unwrap()["content"][0]["text"],
                        "[Request interrupted by user]",
                        "{failure:?}",
                    );
                }
            })
            .await;
        }

        #[tokio::test]
        async fn retains_signal_output_failure_while_interrupting_and_reading_later_turns() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_stalled_reply_once(&server).await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let session_id = engine.session_id();
                let stdout = Screen::default();
                let failure = Failure::Write("\n");
                let stderr = FailingWriter::new(failure);
                let screen = stderr.screen.clone();
                let mut output = Output::new(OutputFormat::Text, false, stdout.clone(), stderr);
                let input: &[u8] = indoc! {b"
                    first
                    second
                "};
                let mut interrupted = false;
                let mut signal = async || {
                    if !interrupted {
                        until_requested(&server, 1).await;
                        interrupted = true;
                        return Shutdown::Interrupt;
                    }
                    std::future::pending().await
                };

                let result = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output,
                    None,
                    Prompter::new(input, io::sink()),
                    true,
                    &mut signal,
                ))
                .await;
                assert_eq!(
                    format!("{:#}", result.unwrap_err()),
                    format!("failed to print output: {}", failure.message()),
                );
                assert_eq!(screen.text(), format!("{INTERRUPTED_MARKER}\n"));
                assert_eq!(stdout.text(), "reply\n");
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 2, "{requests:?}");
                let last = requests[1]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], "second");
                assert_eq!(saved_message_count(&store, &session_id), 4);
            })
            .await;
        }

        #[tokio::test]
        async fn sigint_at_a_terminal_prompt_keeps_reading() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let (writer, reader) = tokio::io::duplex(64);
                let mut writer = Some(writer);
                let mut signal = async || {
                    let Some(mut writer) = writer.take() else {
                        return std::future::pending().await;
                    };
                    tokio::io::AsyncWriteExt::write_all(&mut writer, b"hello\n")
                        .await
                        .unwrap();
                    Shutdown::Interrupt
                };

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output(OutputFormat::Text).0,
                    None,
                    Prompter::new(BufReader::new(reader), io::sink()),
                    true,
                    &mut signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 1, "{requests:?}");
                let last = requests[0]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], "hello");
            })
            .await;
        }

        #[tokio::test]
        async fn sigint_with_piped_input_ends_the_run() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_stalled_reply_once(&server).await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let input: &[u8] = b"first\n";
                let mut signal = async || {
                    until_requested(&server, 1).await;
                    Shutdown::Interrupt
                };

                let exit = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output(OutputFormat::Text).0,
                    None,
                    Prompter::new(input, io::sink()),
                    false,
                    &mut signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Signal(Shutdown::Interrupt));
                assert_eq!(request_bodies(&server).await.len(), 1);
            })
            .await;
        }

        #[tokio::test]
        async fn hands_back_the_panic_of_an_engine_that_stops() {
            let input: &[u8] = b"hello\n";

            let result = Box::pin(drive(
                panicked_engine(),
                Path::new("/repo"),
                &mut output(OutputFormat::Text).0,
                None,
                Prompter::new(input, FailingWriter::new(Failure::Write("> "))),
                true,
                &mut no_signal,
            ))
            .await;

            let panic = result.unwrap_err().downcast::<EnginePanic>().unwrap();
            assert_eq!(panic_message(panic), "engine bug");
        }

        #[tokio::test]
        async fn stops_reading_once_stdout_is_gone() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let mut output = Output::new(OutputFormat::Text, false, ClosedPipe, io::sink());
                let input: &[u8] = b"first\nsecond\n";

                let result = Box::pin(drive(
                    engine.spawn(),
                    dir.path(),
                    &mut output,
                    None,
                    Prompter::new(input, io::sink()),
                    true,
                    &mut no_signal,
                ))
                .await;

                let error = result.unwrap_err();
                assert_eq!(
                    error
                        .root_cause()
                        .downcast_ref::<io::Error>()
                        .map(io::Error::kind),
                    Some(io::ErrorKind::BrokenPipe),
                    "{error:#}",
                );
                assert_eq!(request_bodies(&server).await.len(), 1, "second never runs");
            })
            .await;
        }
    }

    mod prompter {
        use super::*;

        /// Input whose reads always fail.
        struct FailingInput;

        impl tokio::io::AsyncRead for FailingInput {
            fn poll_read(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                _: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                Poll::Ready(Err(io::Error::other("input failed")))
            }
        }

        mod decide {
            use super::*;

            /// Runs one `decide` over `input`, returning the decision and what the prompter wrote.
            async fn decide(
                input: &str,
                grant: Option<&SessionGrant>,
            ) -> (ApprovalDecision, String) {
                let mut prompter = Prompter::new(input.as_bytes(), Vec::new());
                let decision = prompter
                    .decide(
                        &bash_preview("cargo test"),
                        &Basis::Mode(Mode::Manual),
                        grant,
                    )
                    .await
                    .unwrap();
                (decision, String::from_utf8(prompter.out).unwrap())
            }

            #[tokio::test]
            async fn maps_each_answer_to_its_decision() {
                for (input, decision) in [
                    ("y\n", ApprovalDecision::Approve),
                    (" YES \n", ApprovalDecision::Approve),
                    ("n\n", ApprovalDecision::Deny),
                    (
                        "f\nRun the unit tests only.\n",
                        ApprovalDecision::DenyWithFeedback {
                            feedback: "Run the unit tests only.".to_owned(),
                        },
                    ),
                    ("x\n", ApprovalDecision::Abort),
                    ("f\n", ApprovalDecision::Abort),
                    ("", ApprovalDecision::Abort),
                ] {
                    let (got, out) = decide(input, None).await;
                    assert_eq!(got, decision, "{input:?}");
                    assert!(
                        out.starts_with("Bash needs approval:\n  cargo test\n"),
                        "{out}"
                    );
                    assert!(out.contains(CHOICES), "{out}");
                }
            }

            #[tokio::test]
            async fn asks_again_after_an_unknown_answer_or_empty_feedback() {
                let input = indoc! {"
                    a
                    f

                    maybe
                    run the tests
                    n
                "};
                let (decision, out) = decide(input, None).await;

                assert_eq!(decision, ApprovalDecision::Deny);
                assert_eq!(out.matches(CHOICES).count(), 5, "{out}");
                assert_eq!(
                    out.matches("Answer with one of the letters").count(),
                    3,
                    "a prompt typed ahead re-asks: {out}"
                );
            }

            #[tokio::test]
            async fn allows_the_suggested_or_an_edited_rule_for_the_session() {
                let grant = exec_grant("cargo test");
                let allowed = |pattern: &str| ApprovalDecision::AllowForSession {
                    rule: Box::new(Rule::parse(PermissionClass::Exec, pattern).unwrap()),
                };

                let (decision, out) = decide("a\n\n", Some(&grant)).await;
                assert_eq!(decision, allowed("cargo test"));
                assert!(out.contains(GRANT_CHOICES), "{out}");
                assert!(out.contains("Pattern to allow [cargo test]: "), "{out}");

                let input = indoc! {"
                    a
                    cargo build
                    a
                    cargo test *
                "};
                let (decision, out) = decide(input, Some(&grant)).await;
                assert_eq!(decision, allowed("cargo test *"));
                let (rejected, retried) = out
                    .split_once("the pattern no longer covers this call")
                    .unwrap();
                assert_eq!(rejected.matches(GRANT_CHOICES).count(), 1, "{out}");
                assert_eq!(
                    retried.matches(GRANT_CHOICES).count(),
                    1,
                    "a rejected rule returns to the choices: {out}"
                );

                let (decision, _) = decide("a\n", Some(&grant)).await;
                assert_eq!(
                    decision,
                    ApprovalDecision::Abort,
                    "the input ending mid-rule aborts"
                );
            }

            #[tokio::test]
            async fn aborts_saying_why_when_the_input_cannot_be_read() {
                let mut prompter = Prompter::new(BufReader::new(FailingInput), Vec::new());

                let decision = prompter
                    .decide(&bash_preview("ls"), &Basis::Mode(Mode::Manual), None)
                    .await
                    .unwrap();

                assert_eq!(decision, ApprovalDecision::Abort);
                let out = String::from_utf8(prompter.out).unwrap();
                assert!(
                    out.contains("Could not read the answer: input failed"),
                    "{out}"
                );
            }

            #[tokio::test]
            async fn aborts_before_reading_an_undisplayed_approval() {
                let grant = exec_grant("cargo test");
                for failure in [
                    Failure::Write("needs approval"),
                    Failure::Write("[y]"),
                    Failure::Flush,
                ] {
                    for input in ["y\n", "a\n\n"] {
                        let mut prompter =
                            Prompter::new(input.as_bytes(), FailingWriter::new(failure));

                        let decision = prompter
                            .decide(
                                &bash_preview("cargo test"),
                                &Basis::Mode(Mode::Manual),
                                Some(&grant),
                            )
                            .await;

                        assert_eq!(
                            decision.unwrap_err().to_string(),
                            failure.message(),
                            "{failure:?}, {input:?}"
                        );
                        assert_eq!(
                            prompter.next_line().await.unwrap(),
                            Some(input.trim().to_owned())
                        );
                    }
                }
            }

            #[tokio::test]
            async fn stops_before_reading_again_when_followup_output_fails() {
                let grant = exec_grant("cargo test");
                for (failure, input, unread) in [
                    (Failure::Write("Feedback"), "f\ny\n", "y"),
                    (Failure::Write("Pattern"), "a\n\n", ""),
                    (Failure::Write("Rule rejected"), "a\ncargo build\ny\n", "y"),
                    (Failure::Write("Answer with"), "maybe\ny\n", "y"),
                ] {
                    let mut prompter = Prompter::new(input.as_bytes(), FailingWriter::new(failure));

                    let result = prompter
                        .decide(
                            &bash_preview("cargo test"),
                            &Basis::Mode(Mode::Manual),
                            Some(&grant),
                        )
                        .await;
                    assert_eq!(
                        result.unwrap_err().to_string(),
                        failure.message(),
                        "{failure:?}"
                    );

                    assert_eq!(
                        prompter.next_line().await.unwrap().as_deref(),
                        Some(unread),
                        "{failure:?}",
                    );
                }
            }
        }

        mod next_line {
            use super::*;

            #[tokio::test]
            async fn replaces_an_invalid_byte_and_trims_crlf() {
                let mut prompter = Prompter::new(&b"hello\xff\r\n"[..], io::sink());

                assert_eq!(
                    prompter.next_line().await.unwrap().as_deref(),
                    Some("hello\u{FFFD}"),
                );
                assert_eq!(prompter.next_line().await.unwrap(), None);
            }
        }

        mod ask {
            use super::*;

            #[tokio::test]
            async fn reports_output_failure_when_an_input_diagnostic_cannot_be_written() {
                let cases: [(Box<dyn AsyncBufRead + Unpin>, Failure); 2] = [
                    (Box::new(&b""[..]), Failure::Write("\n")),
                    (
                        Box::new(BufReader::new(FailingInput)),
                        Failure::Write("Could not read"),
                    ),
                ];

                for (input, failure) in cases {
                    let mut prompter = Prompter::new(input, FailingWriter::new(failure));

                    let result = prompter.ask("Answer: ").await;

                    assert_eq!(
                        result.unwrap_err().to_string(),
                        failure.message(),
                        "{failure:?}"
                    );
                }
            }
        }
    }

    mod write_preview {
        use super::*;

        #[test]
        fn lists_diff_rows_then_the_rule_and_escapes_hidden_characters() {
            let basis = Basis::Rule {
                tier: Tier::Ask,
                rule: Rule::parse(PermissionClass::Edit, "src/\u{200b}*.rs").unwrap(),
            };
            let mut out = Vec::new();

            write_preview(
                &mut out,
                &ApprovalPreview {
                    title: "Edit(src/a.rs)".to_owned(),
                    body: ApprovalBody::Diff(Diff {
                        hunks: vec![
                            DiffHunk {
                                lines: vec![
                                    diff_line(DiffLineKind::Context, 2, "fn f() {"),
                                    diff_line(DiffLineKind::Removed, 3, "let a = 1;"),
                                    diff_line(
                                        DiffLineKind::Added,
                                        3,
                                        "let a = 2;\u{1b}[2K\u{200b}",
                                    ),
                                ],
                            },
                            DiffHunk {
                                lines: vec![diff_line(DiffLineKind::Added, 12, "g();")],
                            },
                        ],
                        omitted_lines: 7,
                    }),
                },
                &basis,
            )
            .unwrap();

            assert_eq!(
                String::from_utf8(out).unwrap(),
                indoc! {r"
                    Edit(src/a.rs) needs approval:
                       2 │ fn f() {
                       3 - let a = 1;
                       3 + let a = 2;\u{1b}[2K\u{200b}
                       ⋮
                      12 + g();
                      ... +7 lines past the diff limit
                    Asks because of the `ask.edit` pattern `src/\u{200b}*.rs`
                "},
            );
        }
    }

    mod line_reader {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        use tokio::io::AsyncReadExt as _;

        use super::*;

        /// Serves one line per read and counts the reads.
        struct Counted(Arc<AtomicUsize>);

        impl io::Read for Counted {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.0.fetch_add(1, Ordering::SeqCst);
                io::Read::read(&mut &b"line\n"[..], buf)
            }
        }

        #[tokio::test]
        async fn yields_each_line_then_ends_with_the_input() {
            let reader = LineReader::spawn(|| &b"one\r\ntwo\nthree"[..]).unwrap();
            let mut prompter = Prompter::new(reader, io::sink());

            let mut lines = Vec::new();
            while let Some(line) = prompter.next_line().await.unwrap() {
                lines.push(line);
            }

            assert_eq!(lines, ["one", "two", "three"]);
        }

        #[tokio::test]
        async fn an_input_end_ends_only_the_read_that_meets_it() {
            let reader =
                LineReader::spawn(|| io::BufReader::new(Chunks::new(["a\n", "", "b", "", "c\n"])))
                    .unwrap();
            let mut prompter = Prompter::new(reader, io::sink());

            let mut lines = Vec::new();
            for _ in 0..5 {
                lines.push(prompter.next_line().await.unwrap());
            }

            assert_eq!(
                lines,
                [
                    Some("a".to_owned()),
                    None,
                    Some("b".to_owned()),
                    Some("c".to_owned()),
                    None,
                ],
                "a line the input's end cuts short never runs into the next one",
            );
        }

        #[tokio::test]
        async fn reads_from_the_input_only_while_a_line_is_awaited() {
            let reads = Arc::new(AtomicUsize::new(0));
            let counted = Counted(Arc::clone(&reads));
            let reader = LineReader::spawn(|| io::BufReader::new(counted)).unwrap();
            let mut prompter = Prompter::new(reader, io::sink());
            let settle = || tokio::time::sleep(Duration::from_millis(50));

            settle().await;
            let reads_before = reads.load(Ordering::SeqCst);
            let line = prompter.next_line().await.unwrap();
            settle().await;

            assert_eq!(reads_before, 0, "nothing reads before a line is awaited");
            assert_eq!(line.as_deref(), Some("line"));
            assert_eq!(reads.load(Ordering::SeqCst), 1, "one read per awaited line");
        }

        #[tokio::test]
        async fn keeps_the_rest_of_a_line_a_short_read_leaves() {
            let mut reader = LineReader::spawn(|| &b"one\ntwo\n"[..]).unwrap();

            let mut head = [0; 2];
            reader.read_exact(&mut head).await.unwrap();
            let mut rest = Vec::new();
            reader.read_to_end(&mut rest).await.unwrap();

            assert_eq!(&head, b"on");
            assert_eq!(rest, b"e\ntwo\n");
        }

        #[tokio::test]
        async fn passes_a_read_error_on_and_keeps_reading() {
            /// Fails its first read, then reads `rest`.
            struct FailsOnce {
                failed: bool,
                rest: &'static [u8],
            }

            impl io::Read for FailsOnce {
                fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                    if !self.failed {
                        self.failed = true;
                        return Err(io::Error::other("read failed"));
                    }
                    io::Read::read(&mut self.rest, buf)
                }
            }

            let reader = LineReader::spawn(|| {
                io::BufReader::new(FailsOnce {
                    failed: false,
                    rest: b"after\n",
                })
            })
            .unwrap();
            let mut prompter = Prompter::new(reader, io::sink());

            let error = prompter.next_line().await.unwrap_err();
            let line = prompter.next_line().await.unwrap();

            assert_eq!(error.to_string(), "read failed");
            assert_eq!(line.as_deref(), Some("after"));
        }

        #[test]
        fn a_pending_read_never_holds_the_runtime_shutdown() {
            let (input, _writer) = io::pipe().unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_time()
                .build()
                .unwrap();
            runtime.block_on(async {
                let reader = LineReader::spawn(|| io::BufReader::new(input)).unwrap();
                let mut prompter = Prompter::new(reader, io::sink());
                let read = tokio::time::timeout(Duration::from_millis(50), prompter.next_line());
                assert!(read.await.is_err(), "no line arrives");
            });

            let (dropped, done) = std_mpsc::channel();
            thread::spawn(move || {
                drop(runtime);
                dropped.send(()).unwrap();
            });

            done.recv_timeout(Duration::from_secs(10))
                .expect("the runtime shuts down while the read waits for a line");
        }
    }
}
