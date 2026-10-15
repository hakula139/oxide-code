//! Frontends that drive the [`Engine`](crate::engine::Engine) through its handle: the TUI, the
//! bare REPL, and headless mode.

pub(crate) mod headless;
pub(crate) mod repl;
pub(crate) mod tui;

use std::process::ExitCode;

use tokio::sync::mpsc;
use tracing::debug;

use crate::agent::event::{AgentEvent, AgentSink};
use crate::engine::TurnEnd;

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
async fn shutdown_signal() -> Shutdown {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let ctrl_c = tokio::signal::ctrl_c();
        let (Ok(mut sigterm), Ok(mut sighup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            _ = ctrl_c.await;
            return Shutdown::Interrupt;
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

// ── Stdio Rendering ──

/// Renders `events` on `sink` until the prompt in flight ends, returning how it ended, or `None`
/// once the engine has stopped.
async fn render_turn(
    events: &mut mpsc::UnboundedReceiver<AgentEvent>,
    sink: &dyn AgentSink,
) -> Option<TurnEnd> {
    while let Some(event) = events.recv().await {
        let end = TurnEnd::of_prompt(&event);
        render(sink, event);
        if end.is_some() {
            return end;
        }
    }
    None
}

/// Renders the events already queued, such as the engine's startup warnings.
fn render_queued(events: &mut mpsc::UnboundedReceiver<AgentEvent>, sink: &dyn AgentSink) {
    while let Ok(event) = events.try_recv() {
        render(sink, event);
    }
}

fn render(sink: &dyn AgentSink, event: AgentEvent) {
    if let Err(e) = sink.send(event) {
        debug!("stdio render failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::event::{CapturingSink, FailingSink};

    // ── exit_status ──

    #[test]
    fn exit_status_is_128_plus_the_signal_number() {
        #[cfg(unix)]
        assert_eq!(Shutdown::Hangup.exit_status(), 129);
        assert_eq!(Shutdown::Interrupt.exit_status(), 130);
        #[cfg(unix)]
        assert_eq!(Shutdown::Terminate.exit_status(), 143);
    }

    // ── render_turn ──

    #[tokio::test]
    async fn render_turn_stops_at_the_terminal_event_even_when_rendering_fails() {
        let (tx, mut events) = mpsc::unbounded_channel();
        for event in [
            AgentEvent::StreamToken("reply".to_owned()),
            AgentEvent::Cancelled,
            AgentEvent::TurnComplete,
        ] {
            tx.send(event).unwrap();
        }

        let end = render_turn(&mut events, &FailingSink).await;

        assert_eq!(end, Some(TurnEnd::Cancelled));
        assert!(matches!(events.try_recv(), Ok(AgentEvent::TurnComplete)));
    }

    #[tokio::test]
    async fn render_turn_reports_a_stopped_engine() {
        let (tx, mut events) = mpsc::unbounded_channel();
        tx.send(AgentEvent::StreamToken("partial".to_owned()))
            .unwrap();
        drop(tx);
        let sink = CapturingSink::new();

        let end = render_turn(&mut events, &sink).await;

        assert_eq!(end, None);
        assert!(matches!(
            sink.events().as_slice(),
            [AgentEvent::StreamToken(token)] if token == "partial"
        ));
    }
}
