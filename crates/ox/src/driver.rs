//! The bare REPL and headless drivers, which run the [`Engine`](ox_core::engine::Engine) over
//! stdio, plus the exit status and shutdown signals the TUI shares.

pub(crate) mod headless;
pub(crate) mod repl;
mod stdio;

pub(crate) use self::stdio::OutputFormat;

use std::process::ExitCode;

// ── Input ──

/// Decodes stdin bytes, replacing each invalid sequence with U+FFFD, so one stray byte does not
/// end the run, as Claude Code does.
fn decode_input(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

// ── Exit Status ──

/// How a driver ended, mapped to the process exit status by [`Exit::finish`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    Success,
    Failure,
    Signal(Shutdown),
}

impl Exit {
    /// Ends the process by its signal when one stopped the run, as [`Shutdown::end_process`]
    /// describes, and otherwise returns the status to exit with.
    pub(crate) fn finish(self) -> ExitCode {
        match self {
            Self::Success => ExitCode::SUCCESS,
            Self::Failure => ExitCode::FAILURE,
            Self::Signal(signal) => signal.end_process(),
        }
    }
}

// ── Shutdown ──

/// The signal that ended a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shutdown {
    #[cfg(unix)]
    Hangup,
    Interrupt,
    #[cfg(unix)]
    Terminate,
}

impl Shutdown {
    /// Ends the process by the signal on Unix, so a shell, `xargs`, or make sees the child die as
    /// git does and stops its loop. Windows cannot die by a signal, so it reports 130.
    fn end_process(self) -> ExitCode {
        #[cfg(unix)]
        {
            _ = signal_hook::low_level::emulate_default_handler(self.number());
            unreachable!("emulate_default_handler aborts when it cannot raise the signal")
        }

        #[cfg(not(unix))]
        ExitCode::from(self.exit_status())
    }

    #[cfg(unix)]
    const fn number(self) -> i32 {
        match self {
            Self::Hangup => signal_hook::consts::SIGHUP,
            Self::Interrupt => signal_hook::consts::SIGINT,
            Self::Terminate => signal_hook::consts::SIGTERM,
        }
    }

    /// 128 plus the signal number, the shell convention for the one signal Windows reports.
    #[cfg(not(unix))]
    const fn exit_status(self) -> u8 {
        match self {
            Self::Interrupt => 130,
        }
    }
}

/// The SIGINT / SIGTERM / SIGHUP listeners of one run, registered once so a signal arriving
/// between two waits is still delivered. A signal ox cannot listen for is logged and ignored.
pub(crate) struct Signals {
    #[cfg(unix)]
    interrupt: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    hangup: Option<tokio::signal::unix::Signal>,
    #[cfg(not(unix))]
    interrupt: Option<tokio::signal::windows::CtrlC>,
}

impl Signals {
    /// Starts listening. Must run inside the tokio runtime.
    pub(crate) fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            Self {
                interrupt: listen("SIGINT", signal(SignalKind::interrupt())),
                terminate: listen("SIGTERM", signal(SignalKind::terminate())),
                hangup: listen("SIGHUP", signal(SignalKind::hangup())),
            }
        }
        #[cfg(not(unix))]
        Self {
            interrupt: listen("Ctrl+C", tokio::signal::windows::ctrl_c()),
        }
    }

    /// Waits for the next signal. Cancel safe, so a caller may race it against other work and call
    /// it again without losing a signal.
    pub(crate) async fn recv(&mut self) -> Shutdown {
        #[cfg(unix)]
        {
            tokio::select! {
                () = next(self.interrupt.as_mut()) => Shutdown::Interrupt,
                () = next(self.terminate.as_mut()) => Shutdown::Terminate,
                () = next(self.hangup.as_mut()) => Shutdown::Hangup,
            }
        }
        #[cfg(not(unix))]
        {
            if let Some(interrupt) = &mut self.interrupt {
                _ = interrupt.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
            Shutdown::Interrupt
        }
    }
}

/// The listener `registered` made, or `None` after logging why `name` cannot be heard.
fn listen<T>(name: &str, registered: std::io::Result<T>) -> Option<T> {
    registered
        .inspect_err(|e| tracing::warn!(error = %e, "failed to listen for {name}"))
        .ok()
}

/// Waits for the next signal on `listener`, or forever without one.
#[cfg(unix)]
async fn next(listener: Option<&mut tokio::signal::unix::Signal>) {
    match listener {
        Some(listener) => _ = listener.recv().await,
        None => std::future::pending().await,
    }
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::RefCell;
    use std::io::{self, Write};
    use std::rc::Rc;

    use super::Shutdown;
    use super::stdio::{Output, OutputFormat};

    /// An output in `format` with the screens its stdout and stderr show.
    pub(super) fn output(format: OutputFormat) -> (Output<Screen, Screen>, Screen, Screen) {
        let (stdout, stderr) = (Screen::default(), Screen::default());
        let output = Output::new(format, false, stdout.clone(), stderr.clone());
        (output, stdout, stderr)
    }

    /// The `data` of the result line that ends `stdout`.
    pub(super) fn result_data(stdout: &str) -> serde_json::Value {
        let line = stdout.lines().last().expect("a result line");
        let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(record["type"], "result");
        record["data"].take()
    }

    /// A signal that never arrives.
    pub(super) async fn no_signal() -> Shutdown {
        std::future::pending().await
    }

    /// A stdout whose reader went away.
    pub(crate) struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// How a [`FailingWriter`] fails, once.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Failure {
        /// The first write that contains the text.
        Write(&'static str),
        /// The first flush.
        Flush,
    }

    impl Failure {
        pub(super) fn message(self) -> &'static str {
            match self {
                Self::Write(_) => "write failed",
                Self::Flush => "flush failed",
            }
        }
    }

    /// A writer that fails once as its [`Failure`] says and shows every other write on `screen`.
    pub(super) struct FailingWriter {
        pub(super) screen: Screen,
        failure: Option<Failure>,
    }

    impl FailingWriter {
        pub(super) fn new(failure: Failure) -> Self {
            Self {
                screen: Screen::default(),
                failure: Some(failure),
            }
        }
    }

    impl Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(failure @ Failure::Write(text)) = self.failure
                && buf.windows(text.len()).any(|part| part == text.as_bytes())
            {
                self.failure = None;
                return Err(io::Error::other(failure.message()));
            }
            self.screen.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            if let Some(failure @ Failure::Flush) = self.failure {
                self.failure = None;
                return Err(io::Error::other(failure.message()));
            }
            Ok(())
        }
    }

    /// A terminal that stdout and stderr both write to, so a test sees the order of their lines.
    /// Clones show the same screen, so a test keeps one to read what the code under test wrote.
    #[derive(Clone, Default)]
    pub(super) struct Screen(Rc<RefCell<Vec<u8>>>);

    impl Screen {
        pub(super) fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).expect("the screen is UTF-8")
        }
    }

    impl Write for Screen {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod shutdown {
        use super::*;

        #[cfg(unix)]
        mod number {
            use super::*;

            #[test]
            fn maps_each_variant_to_its_signal_number() {
                assert_eq!(
                    (
                        Shutdown::Hangup.number(),
                        Shutdown::Interrupt.number(),
                        Shutdown::Terminate.number(),
                    ),
                    (1, 2, 15),
                );
            }
        }

        #[cfg(not(unix))]
        mod exit_status {
            use super::*;

            #[test]
            fn reports_128_plus_the_signal_number() {
                assert_eq!(Shutdown::Interrupt.exit_status(), 130);
            }
        }
    }
}
