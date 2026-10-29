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
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
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
    native: Arc<dyn NativeClipboard>,
    /// Runs tmux for the tmux routes. Tests replace it with a fake.
    tmux: Arc<dyn Runner>,
    pending: Option<watch::Sender<CopyRequest>>,
    generation: u64,
    tx: mpsc::UnboundedSender<(u64, CopyReport)>,
    rx: mpsc::UnboundedReceiver<(u64, CopyReport)>,
}

#[derive(Debug, Clone)]
struct CopyRequest {
    generation: u64,
    text: Arc<str>,
}

impl Clipboard {
    pub(crate) fn new(environment: Environment) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            environment,
            native: Arc::new(SystemClipboard::default()),
            tmux: Arc::new(Program("tmux")),
            pending: None,
            generation: 0,
            tx,
            rx,
        }
    }

    /// Queues `text`, replacing any pending copy. Must run inside a tokio runtime.
    pub(crate) fn copy(&mut self, text: String) {
        self.generation += 1;
        let request = CopyRequest {
            generation: self.generation,
            text: text.into(),
        };
        if let Some(pending) = &self.pending {
            _ = pending.send_replace(request);
            return;
        }

        let route = Route {
            environment: self.environment,
            native: Arc::clone(&self.native),
            tmux: Arc::clone(&self.tmux),
        };
        let (tx, rx) = watch::channel(request);
        self.pending = Some(tx);
        tokio::spawn(route.run(rx, self.tx.clone()));
    }

    /// Waits for the latest copy's report. Cancel-safe for a `tokio::select!` arm.
    pub(crate) async fn recv(&mut self) -> Option<CopyReport> {
        while let Some((generation, report)) = self.rx.recv().await {
            if generation == self.generation {
                return Some(report);
            }
        }
        None
    }
}

// ── Routes ──

/// The routes one copy may take.
struct Route {
    environment: Environment,
    native: Arc<dyn NativeClipboard>,
    tmux: Arc<dyn Runner>,
}

impl Route {
    async fn run(
        self,
        mut requests: watch::Receiver<CopyRequest>,
        reports: mpsc::UnboundedSender<(u64, CopyReport)>,
    ) {
        loop {
            let CopyRequest { generation, text } = requests.borrow_and_update().clone();
            let chars = text.graphemes(true).count();
            let deadline = Instant::now() + COPY_TIMEOUT;
            let delivery = self.deliver(text, deadline);
            tokio::pin!(delivery);

            if let Ok(outcome) = tokio::time::timeout_at(deadline, &mut delivery).await {
                _ = reports.send((generation, CopyReport { chars, outcome }));
            } else {
                _ = reports.send((
                    generation,
                    CopyReport {
                        chars,
                        outcome: Err(CopyError::TimedOut),
                    },
                ));
                // Native writes cannot be cancelled, and a killed child must be reaped.
                _ = delivery.await;
            }
            if requests.changed().await.is_err() {
                break;
            }
        }
    }

    /// Tries the native clipboard first, since a tmux forward can otherwise race the native write
    /// for X11 selection ownership.
    async fn deliver(&self, text: Arc<str>, deadline: Instant) -> Result<Delivery, CopyError> {
        let native = if self.environment.local {
            let native = Arc::clone(&self.native);
            let text = Arc::clone(&text);
            Some(
                tokio::task::spawn_blocking(move || native.write(&text))
                    .await
                    .context("failed to finish the clipboard write")
                    .flatten(),
            )
        } else {
            None
        };
        if Instant::now() >= deadline {
            return Err(CopyError::TimedOut);
        }
        let tmux = if self.environment.tmux && text.len() <= MAX_TERMINAL_COPY_BYTES {
            Some(load_tmux_buffer(&*self.tmux, &text, deadline).await)
        } else {
            None
        };
        if Instant::now() >= deadline {
            return Err(CopyError::TimedOut);
        }
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

trait NativeClipboard: Send + Sync {
    fn write(&self, text: &str) -> Result<()>;
}

#[derive(Default)]
struct SystemClipboard(Mutex<Option<arboard::Clipboard>>);

impl NativeClipboard for SystemClipboard {
    fn write(&self, text: &str) -> Result<()> {
        let mut native = lock_or_recover(&self.0, "native clipboard");
        let clipboard = match &mut *native {
            Some(clipboard) => clipboard,
            None => {
                native.insert(arboard::Clipboard::new().context("failed to open the clipboard")?)
            }
        };
        clipboard
            .set_text(text)
            .context("failed to write the clipboard")
    }
}

trait Runner: Send + Sync {
    /// Runs with `args` and `stdin` until `deadline`, awaiting termination on timeout. A failure
    /// reports trimmed stderr or the exit status.
    fn run<'a>(
        &'a self,
        args: &'a [&'a str],
        stdin: &'a str,
        deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

/// A program on `PATH` whose child exits before its run completes.
struct Program(&'static str);

impl Runner for Program {
    fn run<'a>(
        &'a self,
        args: &'a [&'a str],
        stdin: &'a str,
        deadline: Instant,
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
            self.run_child(&mut child, stdin, deadline).await
        })
    }
}

impl Program {
    async fn run_child(&self, child: &mut Child, stdin: &str, deadline: Instant) -> Result<()> {
        let mut pipe = child.stdin.take().expect("stdin is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let completion = async {
            let write = async {
                let result = pipe.write_all(stdin.as_bytes()).await;
                drop(pipe);
                result
            };
            let read = async {
                let mut bytes = Vec::new();
                stderr.read_to_end(&mut bytes).await.map(|_| bytes)
            };
            let (written, status, stderr) = tokio::join!(write, child.wait(), read);
            let status = status.with_context(|| format!("failed to wait for {}", self.0))?;
            let stderr = stderr.with_context(|| format!("failed to read from {}", self.0))?;
            if !status.success() {
                let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
                bail!(if stderr.is_empty() {
                    status.to_string()
                } else {
                    stderr
                });
            }
            written.with_context(|| format!("failed to write to {}", self.0))
        };
        let output = tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline) => None,
            output = completion => Some(output),
        };
        if let Some(output) = output {
            return output;
        }

        child
            .kill()
            .await
            .with_context(|| format!("failed to stop {}", self.0))?;
        bail!("{} did not answer before the clipboard deadline", self.0)
    }
}

/// Stores `text` in a tmux paste buffer. `-w` also forwards it to the outer terminal's clipboard
/// unless `set-clipboard` is off, and tmux before 3.2 rejects the flag, so a failure retries
/// without it.
async fn load_tmux_buffer(tmux: &dyn Runner, text: &str, deadline: Instant) -> Result<TmuxLoad> {
    match tmux.run(&["load-buffer", "-w", "-"], text, deadline).await {
        Ok(()) => Ok(TmuxLoad::Forwarded),
        Err(error) if Instant::now() < deadline => tmux
            .run(&["load-buffer", "-"], text, deadline)
            .await
            .map(|()| TmuxLoad::BufferOnly)
            .map_err(|_| error),
        Err(error) => Err(error),
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

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use super::{Environment, Terminal};

    /// A remote session outside any multiplexer, which copies only through OSC 52 and leaves the
    /// system clipboard and tmux alone.
    pub(crate) const REMOTE: Environment = Environment {
        local: false,
        tmux: false,
        screen: false,
        terminal: Terminal::Other,
    };
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};

    use anyhow::anyhow;
    use futures::poll;
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::sync::{Semaphore, oneshot};

    use super::testing::REMOTE;
    use super::*;

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
            _deadline: Instant,
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
            deadline: Instant,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                tokio::time::sleep_until(deadline).await;
                bail!("tmux timed out")
            })
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

    mod environment {
        use super::*;

        mod detect {
            use super::*;

            const LOCAL: Environment = Environment {
                local: true,
                ..REMOTE
            };

            #[test]
            fn reads_the_session_from_the_environment() {
                for (case, set, expected) in [
                    ("a plain local session", &[][..], LOCAL),
                    (
                        "SSH_CONNECTION",
                        &[("SSH_CONNECTION", "10.0.0.1 50000 10.0.0.2 22")],
                        REMOTE,
                    ),
                    ("SSH_TTY", &[("SSH_TTY", "/dev/pts/0")], REMOTE),
                    (
                        "tmux hides an outer screen",
                        &[("TMUX", "/tmp/tmux-1/default,1,0"), ("STY", "1.pts-0.host")],
                        Environment {
                            tmux: true,
                            ..LOCAL
                        },
                    ),
                    (
                        "screen outside tmux",
                        &[("STY", "1.pts-0.host")],
                        Environment {
                            screen: true,
                            ..LOCAL
                        },
                    ),
                    (
                        "the terminal variables",
                        &[
                            ("TERM_PROGRAM", "tmux"),
                            ("LC_TERMINAL", "iTerm2"),
                            ("TERM", "screen-256color"),
                        ],
                        Environment {
                            terminal: Terminal::ITerm2,
                            ..LOCAL
                        },
                    ),
                ] {
                    let vars = [
                        "SSH_CONNECTION",
                        "SSH_TTY",
                        "TMUX",
                        "STY",
                        "TERM_PROGRAM",
                        "LC_TERMINAL",
                        "TERM",
                    ]
                    .map(|name| {
                        let value = set.iter().find(|(set, _)| *set == name).map(|(_, v)| *v);
                        (name, value)
                    });

                    let detected = temp_env::with_vars(vars, Environment::detect);

                    assert_eq!(detected, expected, "{case}");
                }
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

        struct ControlledTmux {
            started: mpsc::UnboundedSender<String>,
            written: mpsc::UnboundedSender<String>,
            releases: HashMap<&'static str, Semaphore>,
            contents: Mutex<String>,
        }

        impl Runner for ControlledTmux {
            fn run<'a>(
                &'a self,
                args: &'a [&'a str],
                stdin: &'a str,
                _deadline: Instant,
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
                assert_eq!(args, ["load-buffer", "-w", "-"]);
                Box::pin(async move {
                    self.started.send(stdin.to_owned()).unwrap();
                    self.releases[stdin].acquire().await.unwrap().forget();
                    *self.contents.lock().unwrap() = stdin.to_owned();
                    self.written.send(stdin.to_owned()).unwrap();
                    Ok(())
                })
            }
        }

        struct ControlledNative {
            started: mpsc::UnboundedSender<String>,
            written: mpsc::UnboundedSender<String>,
            release: Mutex<Option<oneshot::Receiver<()>>>,
            contents: Mutex<String>,
        }

        impl NativeClipboard for ControlledNative {
            fn write(&self, text: &str) -> Result<()> {
                self.started.send(text.to_owned()).unwrap();
                let release = self.release.lock().unwrap().take();
                if let Some(release) = release {
                    _ = release.blocking_recv();
                }
                *self.contents.lock().unwrap() = text.to_owned();
                self.written.send(text.to_owned()).unwrap();
                Ok(())
            }
        }

        struct StoppingTmux {
            stopped: Semaphore,
            events: Mutex<Vec<&'static str>>,
            contents: Mutex<String>,
        }

        impl Runner for StoppingTmux {
            fn run<'a>(
                &'a self,
                _args: &'a [&'a str],
                stdin: &'a str,
                deadline: Instant,
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
                Box::pin(async move {
                    if stdin == "first" {
                        tokio::time::sleep_until(deadline).await;
                        self.events.lock().unwrap().push("first stopping");
                        self.stopped.acquire().await.unwrap().forget();
                        self.events.lock().unwrap().push("first stopped");
                        bail!("tmux timed out");
                    }
                    *self.contents.lock().unwrap() = stdin.to_owned();
                    self.events.lock().unwrap().push("latest written");
                    Ok(())
                })
            }
        }

        fn worker(clipboard: &mut Clipboard) -> impl Future<Output = ()> + use<> {
            let (tx, rx) = watch::channel(CopyRequest {
                generation: 0,
                text: Arc::from(""),
            });
            clipboard.pending = Some(tx);
            Route {
                environment: clipboard.environment,
                native: Arc::clone(&clipboard.native),
                tmux: Arc::clone(&clipboard.tmux),
            }
            .run(rx, clipboard.tx.clone())
        }

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
        async fn completes_the_current_write_before_the_latest_pending_selection() {
            for pending in [vec!["latest"], vec!["obsolete", "latest"]] {
                let (started_tx, mut started) = mpsc::unbounded_channel();
                let (written_tx, mut written) = mpsc::unbounded_channel();
                let tmux = Arc::new(ControlledTmux {
                    started: started_tx,
                    written: written_tx,
                    releases: ["first", "obsolete", "latest"]
                        .map(|text| (text, Semaphore::new(0)))
                        .into(),
                    contents: Mutex::default(),
                });
                let mut clipboard = Clipboard::new(Environment {
                    tmux: true,
                    ..REMOTE
                });
                clipboard.tmux = tmux.clone();
                let mut worker = Box::pin(worker(&mut clipboard));

                clipboard.copy("first".to_owned());
                assert!(poll!(&mut worker).is_pending(), "{pending:?}");
                assert_eq!(
                    started.recv().await.as_deref(),
                    Some("first"),
                    "{pending:?}"
                );
                for &text in &pending {
                    clipboard.copy(text.to_owned());
                    tmux.releases[text].add_permits(1);
                }

                assert!(poll!(&mut worker).is_pending(), "{pending:?}");
                assert_eq!(written.try_recv(), Err(TryRecvError::Empty), "{pending:?}");

                tmux.releases["first"].add_permits(1);
                assert!(poll!(&mut worker).is_pending(), "{pending:?}");
                let report = clipboard.recv().await.unwrap();

                assert_eq!(
                    written.recv().await.as_deref(),
                    Some("first"),
                    "{pending:?}"
                );
                assert_eq!(
                    written.recv().await.as_deref(),
                    Some("latest"),
                    "{pending:?}"
                );
                assert_eq!(written.try_recv(), Err(TryRecvError::Empty), "{pending:?}");
                assert_eq!(*tmux.contents.lock().unwrap(), "latest", "{pending:?}");
                assert_eq!(report.outcome, Ok(Delivery::Copied), "{pending:?}");
                assert_eq!(
                    report.notice(),
                    ("Copied 6 characters".to_owned(), false),
                    "{pending:?}"
                );
            }
        }

        #[tokio::test(start_paused = true)]
        async fn waits_for_a_timed_out_native_write_before_starting_the_next_copy() {
            let (started_tx, mut started) = mpsc::unbounded_channel();
            let (written_tx, mut written) = mpsc::unbounded_channel();
            let (release, wait) = oneshot::channel();
            let native = Arc::new(ControlledNative {
                started: started_tx,
                written: written_tx,
                release: Mutex::new(Some(wait)),
                contents: Mutex::default(),
            });
            let mut clipboard = Clipboard::new(Environment {
                local: true,
                ..REMOTE
            });
            clipboard.native = native.clone();
            let mut worker = Box::pin(worker(&mut clipboard));

            clipboard.copy("first".to_owned());
            assert!(poll!(&mut worker).is_pending());
            assert_eq!(started.recv().await.as_deref(), Some("first"));
            tokio::time::advance(COPY_TIMEOUT).await;
            assert!(poll!(&mut worker).is_pending());
            let timed_out = clipboard.recv().await.unwrap();

            assert_eq!(timed_out.outcome, Err(CopyError::TimedOut));
            assert_eq!(*native.contents.lock().unwrap(), "");

            clipboard.copy("latest".to_owned());
            assert!(poll!(&mut worker).is_pending());

            assert_eq!(started.try_recv(), Err(TryRecvError::Empty));

            release.send(()).unwrap();
            tokio::spawn(worker);
            let report = clipboard.recv().await.unwrap();

            assert_eq!(written.recv().await.as_deref(), Some("first"));
            assert_eq!(written.recv().await.as_deref(), Some("latest"));
            assert_eq!(*native.contents.lock().unwrap(), "latest");
            assert_eq!(report.outcome, Ok(Delivery::Copied));
            assert_eq!(report.notice(), ("Copied 6 characters".to_owned(), false));
        }

        #[tokio::test(start_paused = true)]
        async fn waits_for_a_timed_out_tmux_child_to_stop_before_the_next_copy() {
            let tmux = Arc::new(StoppingTmux {
                stopped: Semaphore::new(0),
                events: Mutex::default(),
                contents: Mutex::default(),
            });
            let mut clipboard = Clipboard::new(Environment {
                tmux: true,
                ..REMOTE
            });
            clipboard.tmux = tmux.clone();
            let mut worker = Box::pin(worker(&mut clipboard));

            clipboard.copy("first".to_owned());
            assert!(poll!(&mut worker).is_pending());
            tokio::time::advance(COPY_TIMEOUT).await;
            assert!(poll!(&mut worker).is_pending());
            let timed_out = clipboard.recv().await.unwrap();

            assert_eq!(timed_out.outcome, Err(CopyError::TimedOut));

            clipboard.copy("latest".to_owned());
            assert!(poll!(&mut worker).is_pending());

            assert_eq!(*tmux.events.lock().unwrap(), ["first stopping"]);

            tmux.stopped.add_permits(1);
            assert!(poll!(&mut worker).is_pending());
            let report = clipboard.recv().await.unwrap();

            assert_eq!(
                *tmux.events.lock().unwrap(),
                ["first stopping", "first stopped", "latest written"],
            );
            assert_eq!(*tmux.contents.lock().unwrap(), "latest");
            assert_eq!(report.outcome, Ok(Delivery::Copied));
        }

        #[tokio::test]
        async fn drains_the_latest_pending_selection_when_the_clipboard_drops() {
            let (started_tx, mut started) = mpsc::unbounded_channel();
            let (written_tx, mut written) = mpsc::unbounded_channel();
            let tmux = Arc::new(ControlledTmux {
                started: started_tx,
                written: written_tx,
                releases: ["first", "latest"]
                    .map(|text| (text, Semaphore::new(0)))
                    .into(),
                contents: Mutex::default(),
            });
            let mut clipboard = Clipboard::new(Environment {
                tmux: true,
                ..REMOTE
            });
            clipboard.tmux = tmux.clone();

            clipboard.copy("first".to_owned());
            assert_eq!(started.recv().await.as_deref(), Some("first"));
            clipboard.copy("latest".to_owned());
            drop(clipboard);
            tmux.releases["latest"].add_permits(1);
            tmux.releases["first"].add_permits(1);

            assert_eq!(written.recv().await.as_deref(), Some("first"));
            assert_eq!(written.recv().await.as_deref(), Some("latest"));
            assert_eq!(*tmux.contents.lock().unwrap(), "latest");
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

    mod recv {
        use super::*;

        #[tokio::test]
        async fn discards_superseded_success_fallback_and_failure_reports() {
            for (name, outcome) in [
                ("native copy", Ok(Delivery::Copied)),
                (
                    "terminal copy",
                    Ok(Delivery::Osc52 {
                        sequence: osc52_sequence("old", false),
                        needs_permission: false,
                    }),
                ),
                ("timeout", Err(CopyError::TimedOut)),
            ] {
                let mut clipboard = Clipboard::new(REMOTE);
                clipboard.generation = 2;
                clipboard
                    .tx
                    .send((1, CopyReport { chars: 3, outcome }))
                    .unwrap();
                let latest = CopyReport {
                    chars: 6,
                    outcome: Err(CopyError::AppleTerminal),
                };
                clipboard
                    .tx
                    .send((
                        2,
                        CopyReport {
                            chars: latest.chars,
                            outcome: Err(CopyError::AppleTerminal),
                        },
                    ))
                    .unwrap();

                let report = clipboard.recv().await.unwrap();

                assert_eq!(report, latest, "{name}");
                assert_eq!(
                    report.notice(),
                    (CopyError::AppleTerminal.to_string(), true),
                    "{name}",
                );
            }
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

                git.run(
                    &["config", "--file", "-", "--get", "a.b"],
                    config,
                    Instant::now() + COPY_TIMEOUT,
                )
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
                        Instant::now() + COPY_TIMEOUT,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(stderr.to_string(), "fixture error");
                let status = git
                    .run(
                        &["config", "--file", "-", "--get", "a.c"],
                        config,
                        Instant::now() + COPY_TIMEOUT,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(status.to_string(), exit_1);
            }
        }

        mod run_child {
            use super::*;

            #[tokio::test]
            async fn kills_and_reaps_the_child_when_its_deadline_expires() {
                const CHILD: &str = "OX_TEST_CLIPBOARD_CHILD";
                if std::env::var_os(CHILD).is_some() {
                    std::io::Write::write_all(&mut std::io::stderr().lock(), b"ready").unwrap();
                    std::future::pending::<()>().await;
                    return;
                }

                let thread = std::thread::current();
                let test_name = thread.name().unwrap();
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", test_name, "--nocapture"])
                    .env(CHILD, "1")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap();
                let mut ready = [0; 5];

                let result = tokio::time::timeout(Duration::from_secs(10), async {
                    child
                        .stderr
                        .as_mut()
                        .unwrap()
                        .read_exact(&mut ready)
                        .await
                        .context("the child did not announce readiness")?;
                    Program("fixture")
                        .run_child(&mut child, "", Instant::now())
                        .await
                })
                .await;
                let reaped = child.id().is_none();
                let status = child.try_wait();
                child.kill().await.unwrap();

                assert_eq!(&ready, b"ready");
                let error = result.expect("the child was not stopped").unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "fixture did not answer before the clipboard deadline"
                );
                assert!(reaped, "the child was awaited before returning");
                let status = status.unwrap().expect("the child has exited");
                assert!(!status.success(), "the child required forced termination");
                #[cfg(unix)]
                assert_eq!(
                    std::os::unix::process::ExitStatusExt::signal(&status),
                    Some(9),
                    "the child was killed with SIGKILL"
                );
            }
        }
    }

    mod load_tmux_buffer {
        use super::*;

        struct DelayedTmux {
            delay: Duration,
            deadlines: Mutex<Vec<Instant>>,
        }

        impl Runner for DelayedTmux {
            fn run<'a>(
                &'a self,
                _args: &'a [&'a str],
                _stdin: &'a str,
                deadline: Instant,
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
                let first = {
                    let mut deadlines = self.deadlines.lock().unwrap();
                    let first = deadlines.is_empty();
                    deadlines.push(deadline);
                    first
                };
                Box::pin(async move {
                    if first {
                        tokio::time::sleep(self.delay).await;
                        bail!("first attempt failed");
                    }
                    Ok(())
                })
            }
        }

        #[tokio::test]
        async fn retries_without_w_for_tmux_before_3_2() {
            let old = FakeTmux::new([Err("usage: load-buffer [-b buffer-name] path"), Ok(())]);
            let down = FakeTmux::new([Err("no server running"), Err("usage: load-buffer")]);

            assert_eq!(
                load_tmux_buffer(&*old, "text", Instant::now() + COPY_TIMEOUT)
                    .await
                    .unwrap(),
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
                load_tmux_buffer(&*down, "text", Instant::now() + COPY_TIMEOUT)
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

        #[tokio::test(start_paused = true)]
        async fn shares_the_deadline_with_fallback_and_skips_expired_retries() {
            for (name, delay, attempts, expected) in [
                (
                    "time remains",
                    COPY_TIMEOUT / 2,
                    2,
                    Ok(TmuxLoad::BufferOnly),
                ),
                (
                    "deadline reached",
                    COPY_TIMEOUT,
                    1,
                    Err("first attempt failed"),
                ),
            ] {
                let tmux = DelayedTmux {
                    delay,
                    deadlines: Mutex::default(),
                };
                let deadline = Instant::now() + COPY_TIMEOUT;

                let result = load_tmux_buffer(&tmux, "text", deadline).await;

                assert_eq!(
                    result.map_err(|error| error.to_string()),
                    expected.map_err(str::to_owned),
                    "{name}",
                );
                assert_eq!(
                    *tmux.deadlines.lock().unwrap(),
                    vec![deadline; attempts],
                    "{name}"
                );
            }
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
