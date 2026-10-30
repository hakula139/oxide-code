//! Results of background work, which reach the [`App`](crate::app::App)'s event loop together.

use tokio::sync::mpsc;

use crate::app::SlashEnd;
use crate::clipboard::CopyEnd;
use crate::git_probes::ProbeEnd;
use crate::modal::pages::ModalPage;

/// Sends [`AppEvent`]s to the event loop. A send fails only once the App is gone, so senders
/// discard the error.
pub(crate) type AppEventSender = mpsc::UnboundedSender<AppEvent>;

/// The end of a piece of background work, which the App hands to the component that started it.
pub(crate) enum AppEvent {
    /// A git or `gh` probe finished.
    Probe(ProbeEnd),
    /// A copy finished.
    Copy(CopyEnd),
    /// A background slash command resolved.
    Slash(SlashEnd),
    /// A picker's worker read a page.
    Page(ModalPage),
}
