//! Bare REPL driver for `--no-tui`: prompts and slash commands from stdin, replies on stdout.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Result, bail};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader, Lines};

use super::stdio::{Approver, StdioSink, Unattended, render_op, render_queued};
use super::{Exit, shutdown_signal};
use crate::command;
use crate::engine::{Engine, EngineHandle, TurnEnd};
use crate::permission::rule::Rule;
use crate::permission::{Basis, SessionGrant};
use crate::protocol::{Decision, Event, EventSink, Op};
use crate::slash::{self, Parsed};
use crate::tool::{ApprovalBody, ApprovalPreview};
use crate::util::text::escape_hidden;

/// Reads prompts and slash commands until EOF or a signal, then finalizes the session. EOF after
/// an operation failed or a command was rejected exits with failure. Approvals are asked only when
/// stdin is a terminal, so a piped line is never taken as an answer.
pub(crate) async fn run(engine: Engine, show_thinking: bool, drifted: &[PathBuf]) -> Result<Exit> {
    let sink = StdioSink::new(show_thinking);
    let attended = io::stdin().is_terminal();
    let stdin = BufReader::new(tokio::io::stdin());
    drive(engine.spawn(drifted), &sink, stdin, attended).await
}

async fn drive<R>(
    mut handle: EngineHandle,
    sink: &dyn EventSink,
    input: R,
    attended: bool,
) -> Result<Exit>
where
    R: AsyncBufRead + Unpin,
{
    let mut lines = input.lines();
    let mut signal = None;
    let mut failed = false;

    let result: Result<()> = async {
        loop {
            render_queued(&mut handle.events, sink);
            eprint!("> ");
            std::io::stderr().flush()?;

            let line = tokio::select! {
                line = lines.next_line() => line?,
                caught = shutdown_signal() => {
                    eprintln!();
                    signal = Some(caught);
                    None
                }
            };
            let Some(line) = line else {
                break;
            };

            let input = line.trim();
            if input.is_empty() {
                continue;
            }
            let op = match slash::parse_slash(input) {
                Some(parsed) => match command_op(&parsed) {
                    Ok(op) => op,
                    Err(message) => {
                        sink.emit(Event::Error(message), "slash-command-rejected");
                        failed = true;
                        continue;
                    }
                },
                None => Op::Prompt(slash::unescape_literal(input).to_owned()),
            };

            // A send fails only once the engine has stopped, which `render_op` then reports.
            _ = handle.ops.send(op.clone()).await;
            let mut prompter = Prompter {
                lines: &mut lines,
                out: io::stderr(),
                attended,
            };
            let rendered = render_op(&mut handle.events, sink, &handle.ops, &op, &mut prompter);
            tokio::select! {
                end = rendered => match end {
                    Some(TurnEnd::Failed) => {
                        if matches!(op, Op::Prompt(_)) {
                            println!();
                        }
                        failed = true;
                    }
                    Some(_) => {}
                    None => bail!("the engine stopped"),
                },
                caught = shutdown_signal() => {
                    eprintln!();
                    signal = Some(caught);
                    break;
                }
            }
        }
        Ok(())
    }
    .await;

    let failure = handle.shutdown().await;
    sink.session_write_error(failure.as_deref());
    result?;
    Ok(match signal {
        Some(signal) => Exit::Signal(signal),
        None if failed => Exit::Failure,
        None => Exit::Success,
    })
}

/// The op a slash command runs in the REPL, or the message saying why it cannot run here.
fn command_op(parsed: &Parsed) -> Result<Op, String> {
    slash::typed_op(parsed).unwrap_or_else(|| {
        let names: Vec<String> = command::NAMES
            .iter()
            .map(|name| format!("/{name}"))
            .collect();
        Err(format!(
            "/{name} is not a REPL command. \
             Available: {names}. \
             Use //{name} to send the literal text.",
            name = parsed.name,
            names = names.join(", "),
        ))
    })
}

// ── Prompter ──

const CHOICES: &str = "[y] approve  [n] deny  [f] feedback  [x] abort: ";
const GRANT_CHOICES: &str =
    "[y] approve  [a] allow for session  [n] deny  [f] feedback  [x] abort: ";

/// Answers approvals with lines read from the REPL's input, writing the call and its choices to
/// `out`. The input ending aborts the turn. Unless `attended`, it denies as [`Unattended`] does.
struct Prompter<'a, R, W> {
    lines: &'a mut Lines<R>,
    out: W,
    attended: bool,
}

impl<R: AsyncBufRead + Unpin, W: Write> Approver for Prompter<'_, R, W> {
    async fn decide(
        &mut self,
        preview: &ApprovalPreview,
        basis: &Basis,
        grant: Option<&SessionGrant>,
    ) -> Decision {
        if !self.attended {
            return Unattended.decide(preview, basis, grant).await;
        }
        _ = write_preview(&mut self.out, preview, basis);
        let choices = if grant.is_some() {
            GRANT_CHOICES
        } else {
            CHOICES
        };
        loop {
            let Some(answer) = self.ask(choices).await else {
                return Decision::Abort;
            };
            match (answer.to_ascii_lowercase().as_str(), grant) {
                ("y" | "yes", _) => return Decision::Approve,
                ("a", Some(grant)) => match self.rule(grant).await {
                    Some(Ok(rule)) => {
                        return Decision::AllowForSession {
                            rule: Box::new(rule),
                        };
                    }
                    Some(Err(e)) => _ = writeln!(self.out, "Rule rejected: {e:#}"),
                    None => return Decision::Abort,
                },
                ("n" | "no", _) => return Decision::Deny,
                ("f", _) => match self.ask("Feedback for the model: ").await {
                    Some(feedback) if !feedback.is_empty() => {
                        return Decision::DenyWithFeedback { feedback };
                    }
                    Some(_) => {}
                    None => return Decision::Abort,
                },
                ("x", _) => return Decision::Abort,
                _ => _ = writeln!(self.out, "Answer with one of the letters in brackets"),
            }
        }
    }
}

impl<R: AsyncBufRead + Unpin, W: Write> Prompter<'_, R, W> {
    /// Writes `prompt` and reads the trimmed answer, or `None` once the input ends or fails.
    async fn ask(&mut self, prompt: &str) -> Option<String> {
        _ = write!(self.out, "{prompt}");
        _ = self.out.flush();
        match self.lines.next_line().await {
            Ok(Some(line)) => Some(line.trim().to_owned()),
            Ok(None) => {
                _ = writeln!(self.out);
                None
            }
            Err(e) => {
                _ = writeln!(self.out, "\nCould not read the answer: {e}");
                None
            }
        }
    }

    /// Reads the pattern of the session rule to grant, where an empty answer takes the suggested
    /// one, or `None` once the input ends. An error says why the grant does not admit the rule.
    async fn rule(&mut self, grant: &SessionGrant) -> Option<Result<Rule>> {
        let suggested = grant.suggested().pattern().to_owned();
        let answer = self
            .ask(&format!("Pattern to allow [{suggested}]: "))
            .await?;
        let text = if answer.is_empty() {
            &suggested
        } else {
            &answer
        };
        Some(grant.accept(text))
    }
}

/// Writes the call `preview` shows, one line per command line or diff row, then why `basis` made
/// it ask when a rule holds it.
fn write_preview(out: &mut impl Write, preview: &ApprovalPreview, basis: &Basis) -> io::Result<()> {
    writeln!(out, "{} needs approval:", escape_hidden(&preview.title))?;
    match &preview.body {
        ApprovalBody::Command(command) => {
            for line in command.lines() {
                writeln!(out, "  {}", escape_hidden(line))?;
            }
        }
        ApprovalBody::Diff(chunks) => {
            for chunk in chunks {
                for line in &chunk.old {
                    writeln!(out, "  - {}", escape_hidden(&line.text))?;
                }
                for line in &chunk.new {
                    writeln!(out, "  + {}", escape_hidden(&line.text))?;
                }
            }
        }
    }
    if let Some(reason) = basis.reason() {
        writeln!(out, "{}", escape_hidden(&reason))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use indoc::{formatdoc, indoc};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::engine::testing::{
        engine, mount_reply, mount_tool_call_once, request_bodies, stopped_engine, with_pinned_home,
    };
    use crate::permission::{Access, Mode, PermissionClass, Policy, Tier, Workspace};
    use crate::protocol::CapturingSink;
    use crate::session::store::testing::test_store;
    use crate::tool::bash::BashTool;
    use crate::tool::{DiffChunk, DiffLine, ToolRegistry};

    fn bash_preview(command: &str) -> ApprovalPreview {
        ApprovalPreview {
            title: "Bash".to_owned(),
            body: ApprovalBody::Command(command.to_owned()),
        }
    }

    fn bash_grant(command: &str) -> SessionGrant {
        Policy::default()
            .session_grant(
                PermissionClass::Exec,
                &Access::for_command(command, &Workspace::new(Path::new("/repo"))),
            )
            .unwrap()
    }

    /// Runs one `decide` over `input`, returning the decision and what the prompter wrote.
    async fn decide(input: &str, grant: Option<&SessionGrant>) -> (Decision, String) {
        let mut lines = input.as_bytes().lines();
        let mut prompter = Prompter {
            lines: &mut lines,
            out: Vec::new(),
            attended: true,
        };
        let decision = prompter
            .decide(
                &bash_preview("cargo test"),
                &Basis::Mode(Mode::Manual),
                grant,
            )
            .await;
        (decision, String::from_utf8(prompter.out).unwrap())
    }

    // ── drive ──

    #[tokio::test]
    async fn drive_skips_blank_lines_and_exits_cleanly_at_eof() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();
            let input: &[u8] = b"\n   \nhello\n";

            let exit = Box::pin(drive(engine.spawn(&[]), &sink, input, true))
                .await
                .unwrap();

            assert_eq!(exit, Exit::Success);
            let requests = request_bodies(&server).await;
            assert_eq!(requests.len(), 1, "blank lines never reach the model");
            let last = requests[0]["messages"].as_array().unwrap().last().unwrap();
            assert_eq!(last["content"][0]["text"], "hello");
        })
        .await;
    }

    #[tokio::test]
    async fn drive_keeps_reading_after_a_failed_turn_and_exits_with_failure() {
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
            let store = test_store(dir.path());
            let engine = engine(server.uri(), &store, dir.path()).await;
            let session_id = engine.session_id().to_owned();
            let sink = CapturingSink::new();
            let input: &[u8] = indoc! {b"
                first
                /resume zzzz
                //literal
            "};

            let exit = Box::pin(drive(
                engine.spawn(&[PathBuf::from("drifted.rs")]),
                &sink,
                input,
                true,
            ))
            .await
            .unwrap();

            assert_eq!(exit, Exit::Failure);
            let requests = request_bodies(&server).await;
            assert_eq!(requests.len(), 2, "/resume must not reach the model");
            let last = requests[1]["messages"].as_array().unwrap().last().unwrap();
            assert_eq!(last["content"][0]["text"], "/literal");
            let reports: Vec<String> = sink
                .events()
                .into_iter()
                .filter_map(|event| match event {
                    Event::Error(msg) | Event::TurnFailed(msg) => Some(msg),
                    _ => None,
                })
                .collect();
            assert_eq!(reports.len(), 3, "{reports:?}");
            assert!(reports[0].ends_with(": drifted.rs"), "{reports:?}");
            assert!(reports[1].contains("HTTP 400"), "{reports:?}");
            assert_eq!(
                reports[2],
                format!(
                    "Resume failed (still on session {session_id}): no session matching `zzzz`"
                ),
            );
            assert!(matches!(sink.events().last(), Some(Event::TurnComplete)));
            let data = store.load_session_data(&session_id).unwrap();
            assert_eq!(
                data.messages.len(),
                3,
                "the failed prompt, the literal, and its reply are saved",
            );
        })
        .await;
    }

    #[tokio::test]
    async fn drive_runs_each_typed_command_as_its_op() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let first_id = engine.session_id().to_owned();
            let sink = CapturingSink::new();
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

            let exit = Box::pin(drive(engine.spawn(&[]), &sink, input.as_bytes(), true))
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
            let confirmations: Vec<String> = sink
                .events()
                .into_iter()
                .filter_map(|event| match event {
                    Event::SessionCompacted { instructions, .. } => {
                        Some(format!("compacted with {instructions:?}"))
                    }
                    Event::SessionTitleUpdated { title, .. } => Some(format!("renamed to {title}")),
                    Event::ConfigChanged { model, effort, .. } => {
                        Some(format!("using {} at {effort:?}", model.id()))
                    }
                    Event::SessionRolled { .. } => Some("rolled".to_owned()),
                    Event::SessionResumed { id, title, .. } => {
                        Some(format!("resumed {id} titled {title:?}"))
                    }
                    _ => None,
                })
                .collect();
            let resumed = format!("resumed {first_id} titled Some(\"Fix login\")");
            assert_eq!(
                confirmations,
                [
                    "compacted with Some(\"focus on login\")",
                    "renamed to Fix login",
                    "using claude-sonnet-5-5 at Some(High)",
                    "using claude-sonnet-5-5 at Some(Low)",
                    "rolled",
                    resumed.as_str(),
                ],
            );
        })
        .await;
    }

    #[tokio::test]
    async fn drive_rejects_a_command_it_cannot_run_and_exits_with_failure() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();
            let input: &[u8] = indoc! {b"
                /model gpt-4
                /model
                /help
            "};

            let exit = Box::pin(drive(engine.spawn(&[]), &sink, input, true))
                .await
                .unwrap();

            assert_eq!(exit, Exit::Failure);
            assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
            let reports: Vec<String> = sink
                .events()
                .into_iter()
                .filter_map(|event| match event {
                    Event::Error(msg) => Some(msg),
                    _ => None,
                })
                .collect();
            assert_eq!(reports.len(), 3, "{reports:?}");
            assert!(
                reports[0].starts_with("/model: Unknown model: `gpt-4`."),
                "{reports:?}"
            );
            assert_eq!(
                reports[1],
                "/model: Missing model id. Usage: `/model <id>`."
            );
            assert_eq!(
                reports[2],
                "/help is not a REPL command. \
                 Available: /clear, /compact, /effort, /mode, /model, /rename, /resume. \
                 Use //help to send the literal text.",
            );
        })
        .await;
    }

    #[tokio::test]
    async fn drive_answers_an_approval_from_the_next_input_line() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_tool_call_once(&server, "call-1", "bash", r#"{"command":"git push"}"#).await;
            mount_reply(&server, "Done.").await;
            let dir = tempfile::tempdir().unwrap();
            let engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                .await
                .with_tools(ToolRegistry::new(vec![Box::new(BashTool::default())]));
            let input = indoc! {"
                list files
                f
                Use git ls-files.
            "};

            let exit = Box::pin(drive(
                engine.spawn(&[]),
                &CapturingSink::new(),
                input.as_bytes(),
                true,
            ))
            .await
            .unwrap();

            assert_eq!(exit, Exit::Success);
            let requests = request_bodies(&server).await;
            let result = &requests[1]["messages"].as_array().unwrap().last().unwrap()["content"][0];
            assert_eq!(result["type"], "tool_result");
            assert_eq!(
                result["content"],
                "The bash call was denied with this feedback: Use git ls-files.",
            );
        })
        .await;
    }

    #[tokio::test]
    async fn drive_without_a_terminal_denies_approvals_and_keeps_every_line_a_prompt() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_tool_call_once(&server, "call-1", "bash", r#"{"command":"git push"}"#).await;
            mount_reply(&server, "Done.").await;
            let dir = tempfile::tempdir().unwrap();
            let engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                .await
                .with_tools(ToolRegistry::new(vec![Box::new(BashTool::default())]));
            let input = indoc! {"
                list files
                y
            "};

            let exit = Box::pin(drive(
                engine.spawn(&[]),
                &CapturingSink::new(),
                input.as_bytes(),
                false,
            ))
            .await
            .unwrap();

            assert_eq!(exit, Exit::Success);
            let requests = request_bodies(&server).await;
            assert_eq!(requests.len(), 3, "the `y` line is the second prompt");
            let result = &requests[1]["messages"].as_array().unwrap().last().unwrap()["content"][0];
            let content = result["content"].as_str().unwrap();
            assert!(
                content.contains("This run cannot ask for approval."),
                "{content}"
            );
            let last = requests[2]["messages"].as_array().unwrap().last().unwrap();
            assert_eq!(last["content"][0]["text"], "y");
        })
        .await;
    }

    #[tokio::test]
    async fn drive_fails_once_the_engine_stops() {
        let input: &[u8] = b"hello\n";

        let result = Box::pin(drive(stopped_engine(), &CapturingSink::new(), input, true)).await;

        assert_eq!(result.unwrap_err().to_string(), "the engine stopped");
    }

    // ── Prompter::decide ──

    #[tokio::test]
    async fn decide_maps_each_answer_to_its_decision() {
        for (input, decision) in [
            ("y\n", Decision::Approve),
            (" YES \n", Decision::Approve),
            ("n\n", Decision::Deny),
            (
                "f\nRun the unit tests only.\n",
                Decision::DenyWithFeedback {
                    feedback: "Run the unit tests only.".to_owned(),
                },
            ),
            ("x\n", Decision::Abort),
            ("f\n", Decision::Abort),
            ("", Decision::Abort),
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
    async fn decide_asks_again_after_an_unknown_answer_or_empty_feedback() {
        let input = indoc! {"
            a
            f

            maybe
            run the tests
            n
        "};
        let (decision, out) = decide(input, None).await;

        assert_eq!(decision, Decision::Deny);
        assert_eq!(out.matches(CHOICES).count(), 5, "{out}");
        assert_eq!(
            out.matches("Answer with one of the letters").count(),
            3,
            "a prompt typed ahead re-asks: {out}"
        );
    }

    #[tokio::test]
    async fn decide_allows_the_suggested_or_an_edited_rule_for_the_session() {
        let grant = bash_grant("cargo test");
        let allowed = |pattern: &str| Decision::AllowForSession {
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
            Decision::Abort,
            "the input ending mid-rule aborts"
        );
    }

    #[tokio::test]
    async fn decide_aborts_saying_why_when_the_input_cannot_be_read() {
        let mut lines = (&b"\xff\n"[..]).lines();
        let mut prompter = Prompter {
            lines: &mut lines,
            out: Vec::new(),
            attended: true,
        };

        let decision = prompter
            .decide(&bash_preview("ls"), &Basis::Mode(Mode::Manual), None)
            .await;

        assert_eq!(decision, Decision::Abort);
        let out = String::from_utf8(prompter.out).unwrap();
        assert!(out.contains("Could not read the answer: "), "{out}");
    }

    // ── write_preview ──

    #[test]
    fn write_preview_lists_diff_rows_then_the_rule_and_escapes_hidden_characters() {
        let basis = Basis::Rule {
            tier: Tier::Ask,
            rule: Rule::parse(PermissionClass::Edit, "src/\u{200b}*.rs").unwrap(),
        };
        let mut out = Vec::new();

        write_preview(
            &mut out,
            &ApprovalPreview {
                title: "Edit(src/a.rs)".to_owned(),
                body: ApprovalBody::Diff(vec![DiffChunk {
                    old: vec![DiffLine {
                        number: 3,
                        text: "let a = 1;".to_owned(),
                    }],
                    new: vec![DiffLine {
                        number: 3,
                        text: "let a = 2;\u{1b}[2K\u{200b}".to_owned(),
                    }],
                }]),
            },
            &basis,
        )
        .unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            indoc! {r"
                Edit(src/a.rs) needs approval:
                  - let a = 1;
                  + let a = 2;\u{1b}[2K\u{200b}
                Held by the `ask.edit` pattern `src/\u{200b}*.rs`
            "},
        );
    }
}
