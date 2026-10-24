//! Scoped workspace tracing capture for assertions on event messages and fields.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Dispatch, Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::{Context, SubscriberExt as _};

#[derive(Clone)]
pub struct LogCapture {
    // A second registration avoids tracing-core's single-dispatch cache using the emitting
    // thread's default subscriber, which may be disabled outside the capture scope.
    _disabled_dispatch: Dispatch,
    events: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
}

impl Default for LogCapture {
    fn default() -> Self {
        Self {
            _disabled_dispatch: Dispatch::new(tracing::subscriber::NoSubscriber::default()),
            events: Arc::default(),
        }
    }
}

impl LogCapture {
    /// Captures synchronous code with `dispatcher::with_default`, or each poll of a future with
    /// `WithSubscriber::with_subscriber`.
    pub fn dispatch(&self) -> Dispatch {
        let filter = Targets::new().with_targets([
            ("ox", LevelFilter::TRACE),
            ("ox_core", LevelFilter::TRACE),
            ("ox_tui", LevelFilter::TRACE),
        ]);
        Dispatch::new(
            tracing_subscriber::registry()
                .with(filter)
                .with(self.clone()),
        )
    }

    pub fn events(&self) -> Vec<BTreeMap<String, String>> {
        self.events
            .lock()
            .expect("captured log mutex must not be poisoned")
            .clone()
    }
}

impl<S: Subscriber> Layer<S> for LogCapture {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.events
            .lock()
            .expect("captured log mutex must not be poisoned")
            .push(fields.0);
    }
}

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    mod log_capture {
        use super::*;

        mod dispatch {
            use super::*;

            fn emit() {
                tracing::warn!("scoped capture event");
            }

            #[test]
            fn captures_a_callsite_first_seen_outside_the_scope() {
                const CHILD: &str = "OX_TEST_LOG_CAPTURE_CHILD";
                if std::env::var_os(CHILD).is_none() {
                    let test = concat!(
                        module_path!(),
                        "::captures_a_callsite_first_seen_outside_the_scope"
                    );
                    let output = Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", test.split_once("::").unwrap().1])
                        .env(CHILD, "1")
                        .output()
                        .unwrap();
                    assert!(
                        output.status.success(),
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                    return;
                }
                let logs = LogCapture::default();
                let dispatch = logs.dispatch();
                let cloned_dispatch = dispatch.clone();
                let events = Arc::clone(&logs.events);
                drop(dispatch);
                drop(logs);
                std::thread::spawn(emit).join().unwrap();

                tracing::dispatcher::with_default(&cloned_dispatch, emit);

                assert_eq!(
                    *events.lock().unwrap(),
                    [BTreeMap::from([(
                        "message".to_owned(),
                        "scoped capture event".to_owned(),
                    )])]
                );
            }
        }
    }
}
