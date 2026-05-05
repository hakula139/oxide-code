//! Channel-based [`AgentSink`] transport for the TUI.

use tokio::sync::mpsc;

use crate::agent::event::{AGENT_EVENT_CHANNEL_CAP, AgentEvent, AgentSink};

// ── Channel Sink (TUI) ──

/// Sends agent events through an `mpsc` channel for TUI consumption.
///
/// Non-blocking — uses `try_send` so the agent task never stalls when the TUI is slow to drain.
/// A full channel surfaces as an error to the agent loop rather than backpressuring the model
/// stream.
#[derive(Clone)]
pub(crate) struct ChannelSink {
    tx: mpsc::Sender<AgentEvent>,
}

impl ChannelSink {
    pub(crate) fn new(tx: mpsc::Sender<AgentEvent>) -> Self {
        Self { tx }
    }
}

impl AgentSink for ChannelSink {
    fn send(&self, event: AgentEvent) -> anyhow::Result<()> {
        use mpsc::error::TrySendError;
        match self.tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                anyhow::bail!(
                    "agent event channel is full (capacity {AGENT_EVENT_CHANNEL_CAP}); \
                     TUI is not draining events fast enough"
                )
            }
            Err(TrySendError::Closed(_)) => anyhow::bail!("TUI channel closed"),
        }
    }
}

/// Creates a linked `ChannelSink` + bounded `Receiver` pair.
pub(crate) fn channel() -> (ChannelSink, mpsc::Receiver<AgentEvent>) {
    let (tx, rx) = mpsc::channel(AGENT_EVENT_CHANNEL_CAP);
    (ChannelSink::new(tx), rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── channel ──

    #[tokio::test]
    async fn channel_sink_delivers_events() {
        let (sink, mut rx) = channel();
        sink.send(AgentEvent::StreamToken("hello".to_owned()))
            .unwrap();
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, AgentEvent::StreamToken(s) if s == "hello"));
    }

    #[test]
    fn channel_sink_send_after_receiver_dropped() {
        let (sink, rx) = channel();
        drop(rx);
        assert!(sink.send(AgentEvent::TurnComplete).is_err());
    }
}
