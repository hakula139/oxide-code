//! Typed, request-keyed page workers shared by session and prompt pickers.

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use ox_core::session::store::listing::{SessionInfo, SessionWalk};
use ox_core::session::store::prompt_history::{PromptEntry, PromptWalk};

use super::searchable_list::{LoadState, SearchableItem, SearchableList};
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

/// Load-ahead threshold and page size for a paged picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PageBudget {
    pub(crate) threshold: usize,
    pub(crate) limit: usize,
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

    /// Restarts the listing from `walk`, clearing `list` and requesting the first page.
    pub(crate) fn reload<T: SearchableItem>(
        &mut self,
        walk: S,
        list: &mut SearchableList<T>,
        events: &AppEventSender,
        budget: PageBudget,
    ) {
        self.replace(walk);
        list.replace_items(Vec::new());
        list.set_load_state(LoadState::Loading);
        self.load_ahead(list, events, budget);
    }

    /// Applies a worker page for `subject`, appending mapped rows and requesting the next page when
    /// needed, or returning a user-facing error on failure. Unmatched pages are returned unchanged
    /// after checking whether a replacement walk is ready to start.
    pub(crate) fn apply_page<T: SearchableItem>(
        &mut self,
        page: PageEnd<S>,
        list: &mut SearchableList<T>,
        events: &AppEventSender,
        budget: PageBudget,
        subject: &str,
        map: impl FnOnce(Vec<S::Item>) -> Vec<T>,
    ) -> Result<Option<String>, PageEnd<S>> {
        let loaded = match self.take(page) {
            Ok(loaded) => loaded,
            Err(page) => {
                self.load_ahead(list, events, budget);
                return Err(page);
            }
        };
        Ok(self.apply_loaded(loaded, list, events, budget, subject, map))
    }

    /// Applies an already-unpacked [`Loaded`] result to `list`.
    pub(crate) fn apply_loaded<T: SearchableItem>(
        &mut self,
        loaded: Loaded<S::Item>,
        list: &mut SearchableList<T>,
        events: &AppEventSender,
        budget: PageBudget,
        subject: &str,
        map: impl FnOnce(Vec<S::Item>) -> Vec<T>,
    ) -> Option<String> {
        match loaded {
            Loaded::Page(items) => {
                list.append_items(map(items));
                self.load_ahead(list, events, budget);
                list.set_load_state(if self.has_more() {
                    LoadState::Loading
                } else {
                    LoadState::Complete
                });
                None
            }
            Loaded::Failed(err) => {
                tracing::warn!(
                    subject,
                    error = %format_args!("{err:#}"),
                    "picker failed to load page"
                );
                list.set_load_state(LoadState::Failed);
                Some(format!("Failed to load {subject}: {err:#}"))
            }
            Loaded::Stopped => {
                list.set_load_state(LoadState::Failed);
                Some(format!("Failed to load {subject}: the page loader stopped"))
            }
        }
    }

    /// Requests the next page while fewer than `budget.threshold` rows remain below the cursor.
    pub(crate) fn load_ahead<T: SearchableItem>(
        &mut self,
        list: &SearchableList<T>,
        events: &AppEventSender,
        budget: PageBudget,
    ) {
        if list.rows_below_cursor() < budget.threshold {
            self.request(events, budget.limit);
        }
    }

    #[cfg(test)]
    pub(crate) fn is_busy(&self) -> bool {
        matches!(self, Self::Loading(_) | Self::Reloading { .. })
    }
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    //! Shared page-worker test harness for pickers backed by [`Pages`].

    use std::ops::{Deref, DerefMut};
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;

    use ox_core::session::store::SessionStore;

    use super::ModalPage;
    use crate::app_event::AppEvent;
    use crate::modal::{Modal, ModalKey};
    use crate::session_info::LiveSessionInfo;
    use crate::session_info::testing::test_session_info;

    pub(crate) trait PagedModal: Modal {
        fn pages_busy(&self) -> bool;
    }

    /// A picker and the channel its page workers report to, standing in for the App.
    pub(crate) struct PageHarness<M> {
        pub(crate) picker: M,
        inbox: mpsc::UnboundedReceiver<AppEvent>,
    }

    impl<M> PageHarness<M> {
        pub(crate) fn new(
            store: SessionStore,
            live_session_id: Option<&str>,
            open: impl FnOnce(&LiveSessionInfo) -> M,
        ) -> Self {
            let (app_events, inbox) = mpsc::unbounded_channel();
            let mut info = LiveSessionInfo {
                store,
                app_events,
                ..test_session_info()
            };
            if let Some(id) = live_session_id {
                info.session_id = id.to_owned();
            }
            Self {
                picker: open(&info),
                inbox,
            }
        }

        /// The next page a worker sent, waiting for it.
        pub(crate) fn next_page(&mut self) -> ModalPage {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(event) = self.inbox.try_recv() {
                    let AppEvent::Page(page) = event else {
                        panic!("expected a modal page");
                    };
                    return page;
                }
                assert!(
                    Instant::now() < deadline,
                    "the requested page never arrived"
                );
                std::thread::yield_now();
            }
        }
    }

    impl<M: PagedModal> PageHarness<M> {
        /// Hands the picker every page it asks for, waiting on each worker.
        pub(crate) fn settle(&mut self) {
            while self.picker.pages_busy() {
                let page = self.next_page();
                _ = self.picker.on_page(page);
            }
        }

        pub(crate) fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> ModalKey {
            self.picker.handle_key(&KeyEvent::new(code, modifiers))
        }
    }

    impl<M> Deref for PageHarness<M> {
        type Target = M;

        fn deref(&self) -> &M {
            &self.picker
        }
    }

    impl<M> DerefMut for PageHarness<M> {
        fn deref_mut(&mut self) -> &mut M {
            &mut self.picker
        }
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
