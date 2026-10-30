//! Typed, request-keyed page workers shared by session and prompt pickers.

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use ox_core::session::store::listing::{SessionInfo, SessionWalk};
use ox_core::session::store::prompt_history::{PromptEntry, PromptWalk};

use crate::app_event::{AppEvent, AppEventSender};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);

pub(crate) enum ModalPage {
    Sessions(PageEnd<SessionWalk>),
    Prompts(PageEnd<PromptWalk>),
}

pub(crate) trait PageSource: Send + Sized + 'static {
    type Item: Send;

    fn is_exhausted(&self) -> bool;
    fn next_page(&mut self, limit: usize) -> anyhow::Result<Vec<Self::Item>>;
    fn wrap(page: PageEnd<Self>) -> ModalPage;
}

impl PageSource for SessionWalk {
    type Item = SessionInfo;

    fn is_exhausted(&self) -> bool {
        self.is_exhausted()
    }

    fn next_page(&mut self, limit: usize) -> anyhow::Result<Vec<Self::Item>> {
        self.next_page(limit)
    }

    fn wrap(page: PageEnd<Self>) -> ModalPage {
        ModalPage::Sessions(page)
    }
}

impl PageSource for PromptWalk {
    type Item = PromptEntry;

    fn is_exhausted(&self) -> bool {
        self.is_exhausted()
    }

    fn next_page(&mut self, limit: usize) -> anyhow::Result<Vec<Self::Item>> {
        self.next_page(limit)
    }

    fn wrap(page: PageEnd<Self>) -> ModalPage {
        ModalPage::Prompts(page)
    }
}

pub(crate) enum Pages<S> {
    Idle(S),
    Loading(u64),
    Reloading { request: u64, walk: S },
    Done,
}

/// A worker's result and its walk. Only the picker still awaiting `request` takes it.
pub(crate) struct PageEnd<S: PageSource> {
    request: u64,
    walk: S,
    page: Option<anyhow::Result<Vec<S::Item>>>,
}

pub(crate) enum Loaded<T> {
    Page(Vec<T>),
    Failed(anyhow::Error),
    Stopped,
}

impl<S: PageSource> Pages<S> {
    pub(crate) fn has_more(&self) -> bool {
        !matches!(self, Self::Done)
    }

    pub(crate) fn rest(walk: S) -> Self {
        if walk.is_exhausted() {
            Self::Done
        } else {
            Self::Idle(walk)
        }
    }

    /// Keeps only the latest replacement walk while a bounded page is still running, so rapid
    /// filter changes never create concurrent page workers for this picker.
    pub(crate) fn replace(&mut self, walk: S) {
        *self = match self {
            Self::Loading(request) | Self::Reloading { request, .. } => Self::Reloading {
                request: *request,
                walk,
            },
            _ => Self::rest(walk),
        };
    }

    /// Starts one bounded page when idle. A dropped picker may leave that page running, but its
    /// globally unique request ID prevents another picker from accepting the result.
    pub(crate) fn request(&mut self, events: &AppEventSender, limit: usize) {
        match std::mem::replace(self, Self::Done) {
            Self::Idle(mut walk) => {
                let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
                let events = events.clone();
                thread::spawn(move || {
                    let page = panic::catch_unwind(AssertUnwindSafe(|| walk.next_page(limit)));
                    let page = PageEnd {
                        request,
                        walk,
                        page: page.ok(),
                    };
                    _ = events.send(AppEvent::Page(S::wrap(page)));
                });
                *self = Self::Loading(request);
            }
            pages => *self = pages,
        }
    }

    /// Takes only the result of the request in flight, returning any other page unchanged.
    pub(crate) fn take(&mut self, page: PageEnd<S>) -> Result<Loaded<S::Item>, PageEnd<S>> {
        match std::mem::replace(self, Self::Done) {
            Self::Reloading { request, walk } if request == page.request => {
                *self = Self::rest(walk);
                return Err(page);
            }
            Self::Loading(request) if request == page.request => {}
            pages => {
                *self = pages;
                return Err(page);
            }
        }
        let PageEnd { walk, page, .. } = page;
        Ok(match page {
            Some(Ok(page)) => {
                *self = Self::rest(walk);
                Loaded::Page(page)
            }
            Some(Err(err)) => {
                *self = Self::Done;
                Loaded::Failed(err)
            }
            None => {
                *self = Self::Done;
                Loaded::Stopped
            }
        })
    }
}

#[cfg(test)]
impl<S: PageSource> PageEnd<S> {
    pub(crate) fn for_request(
        request: u64,
        walk: S,
        page: Option<anyhow::Result<Vec<S::Item>>>,
    ) -> Self {
        Self {
            request,
            walk,
            page,
        }
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;
    use tokio::sync::mpsc;

    use ox_core::session::store::listing::ProjectScope;
    use ox_core::session::store::testing::{SEEDED_PROMPT, isolated_store, seed_test_session};

    use super::*;

    #[test]
    fn serializes_replacements_and_loads_only_the_latest_desired_walk() {
        let (_dir, store) = isolated_store();
        seed_test_session(&store, "session", None, 1, OffsetDateTime::UNIX_EPOCH);
        let walk = || store.prompt_history(ProjectScope::Current, "");
        let mut pages = Pages::Loading(42);
        let (events, mut inbox) = mpsc::unbounded_channel();

        pages.replace(store.prompt_history(ProjectScope::Every, "absent"));
        pages.replace(store.prompt_history(ProjectScope::Current, "seeded"));
        pages.request(&events, 30);
        assert!(
            inbox.try_recv().is_err(),
            "replacement waits for the current worker"
        );
        assert!(pages.take(PageEnd::for_request(41, walk(), None)).is_err());
        assert!(matches!(pages, Pages::Reloading { request: 42, .. }));
        assert!(
            pages
                .take(PageEnd::for_request(42, walk(), Some(Ok(Vec::new()))))
                .is_err()
        );
        pages.request(&events, 30);
        let AppEvent::Page(ModalPage::Prompts(page)) = inbox.blocking_recv().unwrap() else {
            panic!("expected prompt page");
        };
        let Ok(Loaded::Page(entries)) = pages.take(page) else {
            panic!("expected latest result");
        };

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            [SEEDED_PROMPT]
        );
        assert!(!pages.has_more());
    }
}
