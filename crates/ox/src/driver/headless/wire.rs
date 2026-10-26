//! The `stream-json` wire format: a documented [`Event`] in the shape `RELEASING.md` keeps stable.
//!
//! The enum restates each documented variant, so renaming an internal variant or adding a TUI-only
//! one cannot change stdout. Payloads stay the protocol types, whose field names and result views
//! the saved session format already shares and whose drift the snapshot below catches.

use serde::Serialize;

use ox_core::client::RateLimits;
use ox_core::protocol::{CompactTrigger, Event, ToolCall, ToolResult, UsageSnapshot};
use ox_core::tool::bash::background::BackgroundShell;
use ox_core::tool::tasks::Task;

/// One `stream-json` line.
#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(super) enum HeadlessEvent<'a> {
    StreamToken(&'a str),
    ThinkingToken(&'a str),
    ProgressUpdate(&'a str),
    Retrying(&'a str),
    ToolCallStart(&'a ToolCall),
    ToolCallEnd(&'a ToolResult),
    UsageUpdated(&'a UsageSnapshot),
    RateLimitsUpdated(&'a RateLimits),
    TasksUpdated(&'a [Task]),
    BackgroundShellsUpdated(&'a [BackgroundShell]),
    AutoCompactionStarted,
    SummaryToken(&'a str),
    SummaryDiscarded,
    SessionCompacted {
        summary: &'a str,
        pre_count: u32,
        /// Owned because serde's derive would borrow an `Option<&str>` as `&Option<&str>`.
        instructions: Option<String>,
        trigger: &'a CompactTrigger,
    },
    Notice(&'a str),
    Error(&'a str),
    TurnComplete,
    PromptUnsent,
    TurnRolledBack,
    TurnFailed(&'a str),
}

/// The wire form of `event`, or `None` for a variant that `docs/guide/headless.md` does not
/// document, which keeps it off stdout.
pub(super) fn wire(event: &Event) -> Option<HeadlessEvent<'_>> {
    Some(match event {
        Event::StreamToken(text) => HeadlessEvent::StreamToken(text),
        Event::ThinkingToken(text) => HeadlessEvent::ThinkingToken(text),
        Event::ProgressUpdate(text) => HeadlessEvent::ProgressUpdate(text),
        Event::Retrying(text) => HeadlessEvent::Retrying(text),
        Event::ToolCallStart(call) => HeadlessEvent::ToolCallStart(call),
        Event::ToolCallEnd(result) => HeadlessEvent::ToolCallEnd(result),
        Event::UsageUpdated(usage) => HeadlessEvent::UsageUpdated(usage),
        Event::RateLimitsUpdated(limits) => HeadlessEvent::RateLimitsUpdated(limits),
        Event::TasksUpdated(tasks) => HeadlessEvent::TasksUpdated(tasks),
        Event::BackgroundShellsUpdated(shells) => HeadlessEvent::BackgroundShellsUpdated(shells),
        Event::AutoCompactionStarted => HeadlessEvent::AutoCompactionStarted,
        Event::SummaryToken(text) => HeadlessEvent::SummaryToken(text),
        Event::SummaryDiscarded => HeadlessEvent::SummaryDiscarded,
        Event::SessionCompacted {
            summary,
            pre_count,
            instructions,
            trigger,
        } => HeadlessEvent::SessionCompacted {
            summary,
            pre_count: *pre_count,
            instructions: instructions.clone(),
            trigger,
        },
        Event::Notice(text) => HeadlessEvent::Notice(text),
        Event::Error(text) => HeadlessEvent::Error(text),
        Event::TurnComplete => HeadlessEvent::TurnComplete,
        Event::PromptUnsent => HeadlessEvent::PromptUnsent,
        Event::TurnRolledBack => HeadlessEvent::TurnRolledBack,
        Event::TurnFailed(text) => HeadlessEvent::TurnFailed(text),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use ox_core::client::RateLimitWindow;
    use ox_core::permission::Mode;
    use ox_core::protocol::testing::ls_call;
    use ox_core::tool::bash::background::testing::shells_in_each_state;
    use ox_core::tool::tasks::testing::plan;
    use ox_core::tool::view::{
        DiffChunk, EditDiff, GrepFileGroup, GrepMatchLine, NumberedLine, ToolResultView,
    };

    use super::*;

    /// One instance of every documented event, covering each [`ToolResultView`] shape.
    fn documented_events() -> Vec<Event> {
        let results = [
            ToolResultView::Text {
                content: "ok".to_owned(),
            },
            ToolResultView::ReadExcerpt {
                path: "src/main.rs".to_owned(),
                lines: vec![NumberedLine {
                    number: 1,
                    text: "fn main() {}".to_owned(),
                }],
                total_lines: 1,
            },
            ToolResultView::Diff(EditDiff {
                chunk: DiffChunk {
                    old: vec![NumberedLine {
                        number: 3,
                        text: "old".to_owned(),
                    }],
                    new: vec![NumberedLine {
                        number: 3,
                        text: "new".to_owned(),
                    }],
                },
                locations: vec![3],
            }),
            ToolResultView::GrepMatches {
                groups: vec![GrepFileGroup {
                    path: "src/lib.rs".to_owned(),
                    lines: vec![GrepMatchLine {
                        number: 7,
                        text: "TODO".to_owned(),
                        is_match: true,
                    }],
                }],
                truncated: true,
            },
            ToolResultView::GlobFiles {
                pattern: "*.rs".to_owned(),
                files: vec!["src/main.rs".to_owned()],
                total: 1,
            },
            ToolResultView::Tasks { tasks: plan() },
        ]
        .map(|view| ToolResult {
            id: "call-1".to_owned(),
            content: "ok".to_owned(),
            is_error: false,
            title: Some("Ran".to_owned()),
            view,
        });

        let mut events = vec![
            Event::StreamToken("Done.".to_owned()),
            Event::ThinkingToken("Hmm.".to_owned()),
            Event::ProgressUpdate("Reading.".to_owned()),
            Event::Retrying("HTTP 529 overloaded_error. Retrying in 1 s.".to_owned()),
            Event::ToolCallStart(ls_call()),
            Event::UsageUpdated(UsageSnapshot {
                context_tokens: Some(1_000),
                context_window: Some(1_000_000),
                estimated_cost_usd: Some(0.018),
            }),
            Event::RateLimitsUpdated(RateLimits {
                five_hour: Some(RateLimitWindow {
                    utilization: 0.5,
                    resets_at: 1_760_000_000,
                }),
                seven_day: None,
            }),
            Event::TasksUpdated(plan()),
            Event::BackgroundShellsUpdated(shells_in_each_state()),
            Event::AutoCompactionStarted,
            Event::SummaryToken("Summary.".to_owned()),
            Event::SummaryDiscarded,
            Event::SessionCompacted {
                summary: "Summary.".to_owned(),
                pre_count: 4,
                instructions: Some("focus".to_owned()),
                trigger: CompactTrigger::MidTurn,
            },
            Event::Notice("Skipped instruction file /repo/CLAUDE.md.".to_owned()),
            Event::Error("Session write failed: disk full".to_owned()),
            Event::TurnComplete,
            Event::PromptUnsent,
            Event::TurnRolledBack,
            Event::TurnFailed("Anthropic API error (HTTP 400 Bad Request)".to_owned()),
        ];
        events.extend(results.map(Event::ToolCallEnd));
        events
    }

    #[test]
    fn pins_every_documented_payload() {
        let lines: Vec<String> = documented_events()
            .iter()
            .map(|event| {
                serde_json::to_string(&wire(event).expect("a documented event has a form")).unwrap()
            })
            .collect();

        insta::assert_snapshot!(lines.join("\n"));
    }

    #[test]
    fn keeps_undocumented_events_off_stdout() {
        for event in [
            Event::Cancelled,
            Event::AutoCompactionPaused { failures: 3 },
            Event::AutoCompactionResumed,
            Event::PromptDrained("next".to_owned()),
            Event::ModeChanged(Mode::Plan),
            Event::SessionReady,
        ] {
            assert!(wire(&event).is_none(), "{event:?}");
        }
    }
}
