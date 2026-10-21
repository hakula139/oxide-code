//! The bare REPL and headless drivers, which run the [`Engine`](ox_core::engine::Engine) over
//! stdio, plus the exit status and shutdown signals the TUI shares.

pub(crate) mod headless;
pub(crate) mod repl;
mod stdio;

use std::process::ExitCode;

// ── Exit Status ──

/// How a driver ended, mapped to the process exit status by [`Exit::code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    Success,
    Failure,
    Signal(Shutdown),
}

impl Exit {
    pub(crate) fn code(self) -> ExitCode {
        match self {
            Self::Success => ExitCode::SUCCESS,
            Self::Failure => ExitCode::FAILURE,
            Self::Signal(signal) => ExitCode::from(signal.exit_status()),
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
    /// 128 plus the signal number, following the shell convention.
    const fn exit_status(self) -> u8 {
        match self {
            #[cfg(unix)]
            Self::Hangup => 129,
            Self::Interrupt => 130,
            #[cfg(unix)]
            Self::Terminate => 143,
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
                warn!("failed to listen for SIGTERM and SIGHUP, so only SIGINT stops ox: {e}");
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
mod testing {
    use std::io::{self, Write};

    /// A stdout whose reader went away.
    pub(super) struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
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

        mod exit_status {
            use super::*;

            #[test]
            fn is_128_plus_the_signal_number() {
                #[cfg(unix)]
                assert_eq!(Shutdown::Hangup.exit_status(), 129);
                assert_eq!(Shutdown::Interrupt.exit_status(), 130);
                #[cfg(unix)]
                assert_eq!(Shutdown::Terminate.exit_status(), 143);
            }
        }
    }
}
