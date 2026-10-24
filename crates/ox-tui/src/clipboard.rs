//! Clipboard writes for chat selections. A local session writes the system clipboard directly and,
//! inside tmux, also loads a tmux paste buffer. A remote session loads the tmux buffer, which tmux
//! 3.2 and later forward to the outer terminal, or sends the terminal an OSC 52 sequence. Every
//! route runs off the event loop and reports back through [`Clipboard::recv`].

use std::fmt;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::debug;
use unicode_segmentation::UnicodeSegmentation as _;

use ox_core::util::env;
use ox_core::util::sync::lock_or_recover;
use ox_core::util::text::plural;

// ── Constants ──

/// Largest selection sent through tmux or OSC 52, in bytes, matching xterm's default cap.
const MAX_TERMINAL_COPY_BYTES: usize = 100_000;

/// Base64 bytes per DCS string inside GNU screen, which collects each string in a 768-byte buffer
/// and abandons one that overflows it.
const SCREEN_CHUNK_BYTES: usize = 76;

/// How long a copy may take before it is reported as failed.
const COPY_TIMEOUT: Duration = Duration::from_secs(2);

// ── Environment ──

/// The terminal running the session, as far as its OSC 52 support goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Terminal {
    /// Terminal.app, which ignores OSC 52.
    Apple,
    /// iTerm2, which ignores OSC 52 unless "Applications in terminal may access clipboard" is on.
    ITerm2,
    /// The Linux console, which has no clipboard.
    LinuxConsole,
    Other,
}

impl Terminal {
    /// iTerm2 also sets `LC_TERMINAL`, which SSH forwards by default.
    fn from_vars(
        term_program: Option<&str>,
        lc_terminal: Option<&str>,
        term: Option<&str>,
    ) -> Self {
        match (term_program, lc_terminal, term) {
            (Some("Apple_Terminal"), _, _) => Self::Apple,
            (Some("iTerm.app"), _, _) | (_, Some("iTerm2"), _) => Self::ITerm2,
            (_, _, Some("linux")) => Self::LinuxConsole,
            _ => Self::Other,
        }
    }
}

/// Session facts that pick the clipboard routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Environment {
    /// Whether the session runs on the machine whose clipboard the user pastes from.
    pub(crate) local: bool,
    pub(crate) tmux: bool,
    /// Whether GNU screen, outside tmux, hosts the session. Screen passes OSC 52 to the outer
    /// terminal only inside its DCS passthrough.
    pub(crate) screen: bool,
    pub(crate) terminal: Terminal,
}

impl Environment {
    pub(crate) fn detect() -> Self {
        let tmux = env::string("TMUX").is_some();
        Self {
            local: env::string("SSH_CONNECTION").is_none() && env::string("SSH_TTY").is_none(),
            tmux,
            screen: !tmux && env::string("STY").is_some(),
            terminal: Terminal::from_vars(
                env::string("TERM_PROGRAM").as_deref(),
                env::string("LC_TERMINAL").as_deref(),
                env::string("TERM").as_deref(),
            ),
        }
    }
}

// ── Copy Outcome ──

/// How a copy reached the clipboard.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// The system clipboard holds the text, written directly or forwarded by tmux.
    Copied,
    /// Only a tmux paste buffer holds the text, since tmux before 3.2 cannot forward it.
    TmuxBuffer,
    /// The terminal applies this OSC 52 sequence once it is written.
    Osc52 {
        sequence: String,
        /// Whether the terminal applies the sequence only once the user allows clipboard access, as
        /// iTerm2 does with "Applications in terminal may access clipboard".
        needs_permission: bool,
    },
}

/// Why a copy did not reach the clipboard.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CopyError {
    /// The selection is over [`MAX_TERMINAL_COPY_BYTES`], with its size in bytes, and only a
    /// terminal route was left.
    TooLarge(usize),
    /// Every route failed, and Terminal.app ignores OSC 52.
    AppleTerminal,
    /// Every route failed, and the Linux console has no clipboard.
    LinuxConsole,
    /// The routes took longer than [`COPY_TIMEOUT`].
    TimedOut,
}

impl fmt::Display for CopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge(bytes) => write!(
                f,
                "Not copied: the selection is {} kB, over the {} kB terminal copy limit. \
                 Select less text.",
                bytes.div_ceil(1000),
                MAX_TERMINAL_COPY_BYTES / 1000,
            ),
            Self::AppleTerminal => f.write_str(
                "Not copied: Terminal.app ignores OSC 52. \
                 Fn+drag to select, or press Cmd+R to turn off mouse reporting.",
            ),
            Self::LinuxConsole => {
                f.write_str("Not copied: the Linux console has no clipboard. Shift+drag to select.")
            }
            Self::TimedOut => write!(
                f,
                "Not copied: the clipboard did not answer within {} s",
                COPY_TIMEOUT.as_secs(),
            ),
        }
    }
}

/// The outcome of one copy, for the notice over the chat.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CopyReport {
    /// The selection's length in grapheme clusters, the characters a reader counts.
    pub(crate) chars: usize,
    pub(crate) outcome: Result<Delivery, CopyError>,
}

impl CopyReport {
    /// The notice text, and whether it warns that the copy may not have landed.
    pub(crate) fn notice(&self) -> (String, bool) {
        let count = self.chars;
        let copied = format!(
            "Copied {count} {}",
            plural(count, "character", "characters"),
        );
        match &self.outcome {
            Ok(Delivery::Osc52 {
                needs_permission: true,
                ..
            }) => (
                format!(
                    "{copied} if iTerm2's \"Applications in terminal may access clipboard\" is on"
                ),
                true,
            ),
            Ok(Delivery::Copied | Delivery::Osc52 { .. }) => (copied, false),
            Ok(Delivery::TmuxBuffer) => (
                format!(
                    "{copied} to a tmux buffer. tmux 3.2 or later also copies to the clipboard."
                ),
                true,
            ),
            Err(error) => (error.to_string(), true),
        }
    }
}

// ── Clipboard ──

/// Copies text off the event loop. The native clipboard handle lives as long as this value, since
/// X11 and some Wayland compositors drop the contents once the process that wrote them lets go.
pub(crate) struct Clipboard {
    environment: Environment,
    native: Arc<Mutex<Option<arboard::Clipboard>>>,
    /// Runs tmux for the tmux routes. Tests replace it with a fake.
    tmux: Arc<dyn Runner>,
    tx: mpsc::UnboundedSender<CopyReport>,
    rx: mpsc::UnboundedReceiver<CopyReport>,
}

impl Clipboard {
    pub(crate) fn new(environment: Environment) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            environment,
            native: Arc::default(),
            tmux: Arc::new(Program("tmux")),
            tx,
            rx,
        }
    }

    /// Starts copying `text` and reports through [`Self::recv`]. Must run inside a tokio runtime.
    pub(crate) fn copy(&self, text: String) {
        let route = Route {
            environment: self.environment,
            native: Arc::clone(&self.native),
            tmux: Arc::clone(&self.tmux),
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let chars = text.graphemes(true).count();
            let outcome = tokio::time::timeout(COPY_TIMEOUT, route.deliver(text))
                .await
                .unwrap_or(Err(CopyError::TimedOut));
            _ = tx.send(CopyReport { chars, outcome });
        });
    }

    /// Waits for the next finished copy. Cancel-safe, so it can sit in a `tokio::select!` arm.
    pub(crate) async fn recv(&mut self) -> Option<CopyReport> {
        self.rx.recv().await
    }
}

// ── Routes ──

/// The routes one copy may take.
struct Route {
    environment: Environment,
    native: Arc<Mutex<Option<arboard::Clipboard>>>,
    tmux: Arc<dyn Runner>,
}

impl Route {
    /// Tries the native clipboard first, since a tmux forward can otherwise race the native write
    /// for X11 selection ownership.
    async fn deliver(self, text: String) -> Result<Delivery, CopyError> {
        let text: Arc<str> = text.into();
        let native = if self.environment.local {
            Some(write_native(self.native, Arc::clone(&text)).await)
        } else {
            None
        };
        let tmux = if self.environment.tmux && text.len() <= MAX_TERMINAL_COPY_BYTES {
            Some(load_tmux_buffer(&*self.tmux, &text).await)
        } else {
            None
        };
        settle(&text, self.environment, native.as_ref(), tmux.as_ref())
    }
}

/// Whether tmux also forwarded a loaded buffer to the outer terminal.
#[derive(Debug, PartialEq, Eq)]
enum TmuxLoad {
    Forwarded,
    BufferOnly,
}

/// Picks the delivery from the routes that ran, falling back to OSC 52 when none confirmed a copy.
fn settle(
    text: &str,
    environment: Environment,
    native: Option<&Result<()>>,
    tmux: Option<&Result<TmuxLoad>>,
) -> Result<Delivery, CopyError> {
    if let Some(Err(error)) = native {
        debug!(error = %format_args!("{error:#}"), "native clipboard write failed");
    }
    if let Some(Err(error)) = tmux {
        debug!(error = %format_args!("{error:#}"), "tmux load-buffer failed");
    }
    if matches!(native, Some(Ok(()))) || matches!(tmux, Some(Ok(TmuxLoad::Forwarded))) {
        return Ok(Delivery::Copied);
    }
    if matches!(tmux, Some(Ok(TmuxLoad::BufferOnly))) {
        return Ok(Delivery::TmuxBuffer);
    }
    let needs_permission = match environment.terminal {
        Terminal::Apple => return Err(CopyError::AppleTerminal),
        Terminal::LinuxConsole => return Err(CopyError::LinuxConsole),
        Terminal::ITerm2 => true,
        Terminal::Other => false,
    };
    if text.len() > MAX_TERMINAL_COPY_BYTES {
        return Err(CopyError::TooLarge(text.len()));
    }
    Ok(Delivery::Osc52 {
        sequence: osc52_sequence(text, environment.screen),
        needs_permission,
    })
}

/// Writes the system clipboard, keeping the handle in `native` for later copies.
async fn write_native(
    native: Arc<Mutex<Option<arboard::Clipboard>>>,
    text: Arc<str>,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let mut native = lock_or_recover(&native, "native clipboard");
        let clipboard = match &mut *native {
            Some(clipboard) => clipboard,
            None => {
                native.insert(arboard::Clipboard::new().context("failed to open the clipboard")?)
            }
        };
        clipboard
            .set_text(&*text)
            .context("failed to write the clipboard")
    })
    .await
    .context("failed to finish the clipboard write")?
}

trait Runner: Send + Sync {
    /// Runs the program with `args` and `stdin`, returning its trimmed stderr, or else its exit
    /// status, when it fails.
    fn run<'a>(
        &'a self,
        args: &'a [&'a str],
        stdin: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

/// A program on `PATH`. Dropping a run's future kills the child, and tokio reaps it.
struct Program(&'static str);

impl Runner for Program {
    fn run<'a>(
        &'a self,
        args: &'a [&'a str],
        stdin: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let mut child = Command::new(self.0)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("failed to run {}", self.0))?;
            let mut pipe = child.stdin.take().expect("stdin is piped");
            let written = pipe.write_all(stdin.as_bytes()).await;
            drop(pipe);
            let output = child
                .wait_with_output()
                .await
                .with_context(|| format!("failed to wait for {}", self.0))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                bail!(if stderr.is_empty() {
                    output.status.to_string()
                } else {
                    stderr
                });
            }
            written.with_context(|| format!("failed to write to {}", self.0))
        })
    }
}

/// Stores `text` in a tmux paste buffer. `-w` also forwards it to the outer terminal's clipboard
/// unless `set-clipboard` is off, and tmux before 3.2 rejects the flag, so a failure retries
/// without it.
async fn load_tmux_buffer(tmux: &dyn Runner, text: &str) -> Result<TmuxLoad> {
    match tmux.run(&["load-buffer", "-w", "-"], text).await {
        Ok(()) => Ok(TmuxLoad::Forwarded),
        Err(error) => tmux
            .run(&["load-buffer", "-"], text)
            .await
            .map(|()| TmuxLoad::BufferOnly)
            .map_err(|_| error),
    }
}

/// The OSC 52 write for `text`. Inside GNU screen it goes out as a run of DCS strings of at most
/// [`SCREEN_CHUNK_BYTES`] payload bytes each, which screen forwards in order.
fn osc52_sequence(text: &str, screen: bool) -> String {
    let encoded = STANDARD.encode(text);
    if !screen {
        return format!("\x1b]52;c;{encoded}\x07");
    }
    let pieces: Vec<&str> = encoded
        .as_bytes()
        .chunks(SCREEN_CHUNK_BYTES)
        .map(|piece| std::str::from_utf8(piece).expect("base64 is ASCII"))
        .collect();
    format!("\x1bP\x1b]52;c;{}\x07\x1b\\", pieces.join("\x1b\\\x1bP"))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use anyhow::anyhow;

    use super::*;

    const REMOTE: Environment = Environment {
        local: false,
        tmux: false,
        screen: false,
        terminal: Terminal::Other,
    };

    /// A tmux that records each run's arguments and stdin and answers from `replies` in order,
    /// failing any unexpected call.
    struct FakeTmux {
        runs: Mutex<Vec<(Vec<String>, String)>>,
        replies: Mutex<VecDeque<Result<(), &'static str>>>,
    }

    impl FakeTmux {
        fn new<const N: usize>(replies: [Result<(), &'static str>; N]) -> Arc<Self> {
            Arc::new(Self {
                runs: Mutex::default(),
                replies: Mutex::new(replies.into()),
            })
        }

        fn runs(&self) -> Vec<(Vec<String>, String)> {
            self.runs.lock().unwrap().clone()
        }
    }

    impl Runner for FakeTmux {
        fn run<'a>(
            &'a self,
            args: &'a [&'a str],
            stdin: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            let args = args.iter().map(|&arg| arg.to_owned()).collect();
            self.runs.lock().unwrap().push((args, stdin.to_owned()));
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err("unexpected tmux invocation"));
            Box::pin(async move { reply.map_err(|error| anyhow!(error)) })
        }
    }

    struct PendingTmux;

    impl Runner for PendingTmux {
        fn run<'a>(
            &'a self,
            _args: &'a [&'a str],
            _stdin: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(std::future::pending())
        }
    }

    fn load_buffer(args: &[&str], stdin: &str) -> (Vec<String>, String) {
        let args = ["load-buffer"]
            .iter()
            .chain(args)
            .map(|&arg| arg.to_owned());
        (args.collect(), stdin.to_owned())
    }

    mod terminal {
        use super::*;

        mod from_vars {
            use super::*;

            #[test]
            fn names_the_terminals_without_default_osc52() {
                assert_eq!(
                    Terminal::from_vars(Some("Apple_Terminal"), None, Some("xterm-256color")),
                    Terminal::Apple,
                );
                assert_eq!(
                    Terminal::from_vars(Some("iTerm.app"), None, None),
                    Terminal::ITerm2
                );
                assert_eq!(
                    Terminal::from_vars(Some("tmux"), Some("iTerm2"), None),
                    Terminal::ITerm2,
                    "inside tmux or over SSH only LC_TERMINAL names iTerm2",
                );
                assert_eq!(
                    Terminal::from_vars(None, None, Some("linux")),
                    Terminal::LinuxConsole
                );
                assert_eq!(
                    Terminal::from_vars(Some("ghostty"), None, Some("xterm-ghostty")),
                    Terminal::Other
                );
            }
        }
    }

    mod copy_error {
        use super::*;

        #[test]
        fn messages_name_the_cause() {
            assert_eq!(
                CopyError::TooLarge(MAX_TERMINAL_COPY_BYTES + 1).to_string(),
                "Not copied: the selection is 101 kB, over the 100 kB terminal copy limit. \
                 Select less text.",
            );
            assert_eq!(
                CopyError::LinuxConsole.to_string(),
                "Not copied: the Linux console has no clipboard. Shift+drag to select.",
            );
            assert_eq!(
                CopyError::TimedOut.to_string(),
                "Not copied: the clipboard did not answer within 2 s",
            );
        }
    }

    mod copy_report {
        use super::*;

        mod notice {
            use super::*;

            #[test]
            fn counts_characters_and_names_the_fix_for_a_failed_copy() {
                let notice = |chars, outcome| CopyReport { chars, outcome }.notice();
                let osc52 = |needs_permission| {
                    Ok(Delivery::Osc52 {
                        sequence: String::new(),
                        needs_permission,
                    })
                };

                assert_eq!(
                    notice(1, Ok(Delivery::Copied)),
                    ("Copied 1 character".to_owned(), false)
                );
                assert_eq!(
                    notice(9, osc52(false)),
                    ("Copied 9 characters".to_owned(), false)
                );
                assert_eq!(
                    notice(9, osc52(true)),
                    (
                        "Copied 9 characters \
                         if iTerm2's \"Applications in terminal may access clipboard\" is on"
                            .to_owned(),
                        true,
                    ),
                );
                assert_eq!(
                    notice(9, Ok(Delivery::TmuxBuffer)),
                    (
                        "Copied 9 characters to a tmux buffer. \
                         tmux 3.2 or later also copies to the clipboard."
                            .to_owned(),
                        true,
                    ),
                );
                assert_eq!(
                    notice(9, Err(CopyError::AppleTerminal)),
                    (
                        "Not copied: Terminal.app ignores OSC 52. \
                         Fn+drag to select, or press Cmd+R to turn off mouse reporting."
                            .to_owned(),
                        true,
                    ),
                );
            }
        }
    }

    mod copy {
        use super::*;

        async fn copy_with(clipboard: &mut Clipboard, text: &str) -> CopyReport {
            clipboard.copy(text.to_owned());
            clipboard.recv().await.expect("the copy reports back")
        }

        #[tokio::test]
        async fn in_a_remote_session_sends_osc52() {
            let mut clipboard = Clipboard::new(REMOTE);

            let report = copy_with(&mut clipboard, "cafe\u{301}").await;

            assert_eq!(
                report,
                CopyReport {
                    chars: 4,
                    outcome: Ok(Delivery::Osc52 {
                        sequence: "\x1b]52;c;Y2FmZcyB\x07".to_owned(),
                        needs_permission: false,
                    }),
                },
                "the combining accent counts with its letter",
            );
        }

        #[tokio::test]
        async fn inside_tmux_hands_the_text_to_tmux() {
            let tmux = FakeTmux::new([Ok(())]);
            let mut clipboard = Clipboard::new(Environment {
                tmux: true,
                ..REMOTE
            });
            clipboard.tmux = tmux.clone();

            let report = copy_with(&mut clipboard, "text").await;

            assert_eq!(report.outcome, Ok(Delivery::Copied));
            assert_eq!(tmux.runs(), [load_buffer(&["-w", "-"], "text")]);
        }

        #[tokio::test]
        async fn inside_tmux_keeps_a_selection_over_the_limit_from_tmux() {
            let tmux = FakeTmux::new([Ok(())]);
            let mut clipboard = Clipboard::new(Environment {
                tmux: true,
                ..REMOTE
            });
            clipboard.tmux = tmux.clone();
            let at_limit = "x".repeat(MAX_TERMINAL_COPY_BYTES);
            let over_limit = "x".repeat(MAX_TERMINAL_COPY_BYTES + 1);

            let fits = copy_with(&mut clipboard, &at_limit).await;
            let too_large = copy_with(&mut clipboard, &over_limit).await;

            assert_eq!(fits.outcome, Ok(Delivery::Copied));
            assert_eq!(
                too_large.outcome,
                Err(CopyError::TooLarge(MAX_TERMINAL_COPY_BYTES + 1))
            );
            assert_eq!(tmux.runs(), [load_buffer(&["-w", "-"], &at_limit)]);
        }

        #[tokio::test(start_paused = true)]
        async fn reports_a_route_that_outlasts_the_timeout() {
            let mut clipboard = Clipboard::new(Environment {
                tmux: true,
                ..REMOTE
            });
            clipboard.tmux = Arc::new(PendingTmux);

            let report = copy_with(&mut clipboard, "text").await;

            assert_eq!(report.outcome, Err(CopyError::TimedOut));
        }
    }

    mod settle {
        use super::*;

        #[test]
        fn takes_a_confirmed_route_before_osc52() {
            let settle_with = |native: Option<Result<()>>, tmux: Option<Result<TmuxLoad>>| {
                settle("text", REMOTE, native.as_ref(), tmux.as_ref())
            };

            assert_eq!(settle_with(Some(Ok(())), None), Ok(Delivery::Copied));
            assert_eq!(
                settle_with(Some(Ok(())), Some(Err(anyhow!("no server running")))),
                Ok(Delivery::Copied),
                "a native write needs no tmux",
            );
            assert_eq!(
                settle_with(
                    Some(Err(anyhow!("no display"))),
                    Some(Ok(TmuxLoad::Forwarded))
                ),
                Ok(Delivery::Copied),
            );
            assert_eq!(
                settle_with(None, Some(Ok(TmuxLoad::BufferOnly))),
                Ok(Delivery::TmuxBuffer)
            );
            assert!(matches!(
                settle_with(Some(Err(anyhow!("no display"))), None),
                Ok(Delivery::Osc52 { .. })
            ));
            let screen = Environment {
                screen: true,
                ..REMOTE
            };
            assert!(matches!(
                settle("text", screen, None, None),
                Ok(Delivery::Osc52 { sequence, .. }) if sequence.starts_with("\x1bP")
            ));
        }

        #[test]
        fn refuses_oversized_text_and_terminals_without_osc52() {
            let at_limit = "x".repeat(MAX_TERMINAL_COPY_BYTES);
            let over_limit = "x".repeat(MAX_TERMINAL_COPY_BYTES + 1);

            assert!(settle(&at_limit, REMOTE, None, None).is_ok());
            assert_eq!(
                settle(&over_limit, REMOTE, None, None),
                Err(CopyError::TooLarge(MAX_TERMINAL_COPY_BYTES + 1)),
            );
            assert_eq!(
                settle(&over_limit, REMOTE, Some(&Ok(())), None),
                Ok(Delivery::Copied),
                "the native clipboard has no limit",
            );
            assert_eq!(
                settle(
                    "text",
                    Environment {
                        terminal: Terminal::Apple,
                        ..REMOTE
                    },
                    Some(&Err(anyhow!("denied"))),
                    None
                ),
                Err(CopyError::AppleTerminal),
            );
            assert_eq!(
                settle(
                    &over_limit,
                    Environment {
                        terminal: Terminal::LinuxConsole,
                        ..REMOTE
                    },
                    None,
                    None
                ),
                Err(CopyError::LinuxConsole),
                "no selection size would copy without OSC 52",
            );
            assert_eq!(
                settle(
                    "text",
                    Environment {
                        terminal: Terminal::LinuxConsole,
                        ..REMOTE
                    },
                    None,
                    None
                ),
                Err(CopyError::LinuxConsole),
            );
        }
    }

    mod program {
        use super::*;

        mod run {
            use super::*;

            #[tokio::test]
            async fn feeds_stdin_and_reports_stderr_or_the_exit_status_on_failure() {
                // `git config --file -` parses stdin, and every platform the tests run on has git.
                let git = Program("git");
                let config = "[a]\n\tb = c\n";
                let exit_1 = if cfg!(windows) {
                    "exit code: 1"
                } else {
                    "exit status: 1"
                };

                git.run(&["config", "--file", "-", "--get", "a.b"], config)
                    .await
                    .unwrap();
                let stderr = git
                    .run(
                        &[
                            "-c",
                            "alias.fixture=!echo '  fixture error  ' >&2; exit 3",
                            "fixture",
                        ],
                        "",
                    )
                    .await
                    .unwrap_err();
                assert_eq!(stderr.to_string(), "fixture error");
                let status = git
                    .run(&["config", "--file", "-", "--get", "a.c"], config)
                    .await
                    .unwrap_err();
                assert_eq!(status.to_string(), exit_1);
            }
        }
    }

    mod load_tmux_buffer {
        use super::*;

        #[tokio::test]
        async fn retries_without_w_for_tmux_before_3_2() {
            let old = FakeTmux::new([Err("usage: load-buffer [-b buffer-name] path"), Ok(())]);
            let down = FakeTmux::new([Err("no server running"), Err("usage: load-buffer")]);

            assert_eq!(
                load_tmux_buffer(&*old, "text").await.unwrap(),
                TmuxLoad::BufferOnly
            );
            assert_eq!(
                old.runs(),
                [
                    load_buffer(&["-w", "-"], "text"),
                    load_buffer(&["-"], "text")
                ]
            );
            assert_eq!(
                load_tmux_buffer(&*down, "text")
                    .await
                    .unwrap_err()
                    .to_string(),
                "no server running",
                "the first error explains the failure",
            );
            assert_eq!(
                down.runs(),
                [
                    load_buffer(&["-w", "-"], "text"),
                    load_buffer(&["-"], "text")
                ]
            );
        }
    }

    mod osc52_sequence {
        use super::*;

        #[test]
        fn inside_screen_splits_into_dcs_passthrough_strings() {
            let text = "x".repeat(60);
            let encoded = STANDARD.encode(&text);
            assert_eq!(encoded.len(), 80);

            let sequence = osc52_sequence(&text, true);

            assert_eq!(
                sequence,
                format!(
                    "\x1bP\x1b]52;c;{}\x1b\\\x1bP{}\x07\x1b\\",
                    &encoded[..76],
                    &encoded[76..],
                ),
            );
            assert_eq!(osc52_sequence("x", false), "\x1b]52;c;eA==\x07");
        }
    }
}
