//! Frontends that drive the [`Engine`](crate::engine::Engine) through its handle: the TUI, the
//! bare REPL, and headless mode.

pub(crate) mod headless;
pub(crate) mod repl;
mod stdio;
pub(crate) mod tui;

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
                warn!("cannot listen for SIGTERM and SIGHUP, falling back to SIGINT alone: {e}");
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
