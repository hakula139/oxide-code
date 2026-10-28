//! The bare REPL and headless drivers, which run the [`Engine`](ox_core::engine::Engine) over
//! stdio, plus the exit status and shutdown signals the TUI shares.

pub(crate) mod headless;
pub(crate) mod repl;
mod stdio;

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

/// Resolves on the first SIGINT / SIGTERM / SIGHUP.
pub(crate) async fn shutdown_signal() -> Shutdown {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        use tracing::warn;

        let ctrl_c = tokio::signal::ctrl_c();
        let (mut sigterm, mut sighup) = match (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            (Ok(sigterm), Ok(sighup)) => (sigterm, sighup),
            (Err(e), _) | (_, Err(e)) => {
                warn!(
                    error = %e,
                    "failed to listen for SIGTERM and SIGHUP, so only SIGINT stops ox"
                );
                _ = ctrl_c.await;
                return Shutdown::Interrupt;
            }
        };
        tokio::select! {
            _ = ctrl_c => Shutdown::Interrupt,
            _ = sigterm.recv() => Shutdown::Terminate,
            _ = sighup.recv() => Shutdown::Hangup,
        }
    }
    #[cfg(not(unix))]
    {
        _ = tokio::signal::ctrl_c().await;
        Shutdown::Interrupt
    }
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::RefCell;
    use std::io::{self, Write};
    use std::rc::Rc;

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

    /// A terminal that stdout and stderr both write to, so a test sees the order of their lines.
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
