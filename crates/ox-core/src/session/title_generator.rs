//! Background AI title generator, which asks Haiku to title a fresh session after its first prompt.

use anyhow::{Context, Result, bail};
use indoc::indoc;
use serde::Deserialize;
use tokio::sync::oneshot;
use tracing::warn;
use unicode_width::UnicodeWidthStr;

use super::display::fit_title;
use super::entry::UsageRecord;
use super::handle::SessionHandle;
use crate::client::anthropic::Client;
use crate::client::anthropic::wire::OutputFormat;
use crate::config::Effort;
use crate::model::ModelSpec;
use crate::protocol::{Event, EventSink};
use crate::util::text::{ELLIPSIS, ELLIPSIS_WIDTH, truncate_to_width};

// ── Constants ──

/// Model for title generation, cheap enough to run on every fresh session.
const HAIKU_MODEL: &str = "claude-haiku-5-5";

/// Output budget. Haiku 5.5 always runs adaptive thinking, so this leaves room for a short think
/// ahead of the 3-7 word JSON envelope.
const MAX_TOKENS: u32 = 1_024;

/// Chars of the first prompt sent for titling, so a long paste stays cheap.
const MAX_PROMPT_CHARS: usize = 1_000;

/// Title prompt, whose `{"title": ...}` shape [`title_output_format`] enforces.
const SYSTEM_PROMPT: &str = indoc! {r#"
    Generate a concise, sentence-case title (3-7 words) that captures the main topic or goal of this coding session. The title should be clear enough that the user recognizes the session in a list. Use sentence case: capitalize only the first word and proper nouns.

    Return JSON with a single "title" field.

    Good examples:
    {"title": "Fix login button on mobile"}
    {"title": "Add OAuth authentication"}
    {"title": "Debug failing CI tests"}
    {"title": "Refactor API client error handling"}

    Bad (too vague): {"title": "Code changes"}
    Bad (too long): {"title": "Investigate and fix why the mobile login button no longer responds"}
    Bad (wrong case): {"title": "Fix Login Button On Mobile"}"#
};

// ── Public API ──

/// Spawns a task that asks Haiku for a title, appends it to `session`, and emits
/// `SessionTitleUpdated` through `sink`. The returned receiver gets the usage the response reports,
/// even for a reply that holds no title. Finalizing `session` aborts the task. A generation failure
/// only warns, while a write failure shows as a session write error.
pub(crate) fn spawn<S>(
    client: Client,
    session: SessionHandle,
    sink: S,
    first_prompt: String,
) -> oneshot::Receiver<UsageRecord>
where
    S: EventSink + Clone + Send + 'static,
{
    let (billed, usage) = oneshot::channel();
    let owner = session.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = generate_and_record(&client, &session, &sink, &first_prompt, billed).await {
            warn!("AI title generation failed: {e:#}");
        }
    });
    owner.set_title_task(task);
    usage
}

// ── Generation ──

/// Asks Haiku for a title, then appends it and notifies `sink` unless a `/rename` won the race.
/// Sends the request's usage on `billed` and records it in `session` before parsing the reply,
/// since the API bills a reply that holds no title. The send goes first, so a `/rename` that
/// aborts the task during the write still leaves the cost counted.
async fn generate_and_record(
    client: &Client,
    session: &SessionHandle,
    sink: &impl EventSink,
    first_prompt: &str,
    billed: oneshot::Sender<UsageRecord>,
) -> Result<()> {
    let prompt = truncate_prompt(first_prompt, MAX_PROMPT_CHARS);
    let output_format = title_output_format();
    let (raw, usage) = client
        .complete(
            &ModelSpec::resolve(HAIKU_MODEL),
            SYSTEM_PROMPT,
            &prompt,
            MAX_TOKENS,
            Some(Effort::Low),
            Some(&output_format),
        )
        .await
        .context("Haiku completion failed")?;

    if let Some(tokens) = usage {
        let record = UsageRecord {
            model: HAIKU_MODEL.to_owned(),
            cache_ttl: client.prompt_cache_ttl(),
            tokens,
        };
        _ = billed.send(record.clone());
        let outcome = session.record_usage(vec![record]).await;
        sink.session_write_error(outcome.failure.as_deref());
    }

    let title = parse_title(&raw).context("Haiku returned a malformed title")?;

    if session.manual_title_set() {
        return Ok(());
    }
    let outcome = session.append_ai_title(title.clone()).await;
    sink.session_write_error(outcome.failure.as_deref());

    sink.emit(
        Event::SessionTitleUpdated {
            session_id: session.session_id().to_owned(),
            title,
        },
        "session-title-updated",
    );
    Ok(())
}

/// `{"title": string}` schema for structured outputs. Forces the envelope shape so Haiku
/// doesn't answer the prompt conversationally.
fn title_output_format() -> OutputFormat {
    OutputFormat::json_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "title": {"type": "string"},
        },
        "required": ["title"],
        "additionalProperties": false,
    }))
}

// ── Parsing ──

/// Parses the `{"title": "..."}` envelope from Haiku's response, fenced or not. Plain text, such as
/// a refusal, fails.
fn parse_title(response: &str) -> Result<String> {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        bail!("empty response");
    }

    let unwrapped = strip_code_fence(trimmed);
    let TitleEnvelope { title } = serde_json::from_str(unwrapped).with_context(|| {
        format!(
            "response is not a title envelope: {}",
            truncate_for_log(unwrapped)
        )
    })?;
    let title = fit_title(&title);
    if title.is_empty() {
        bail!("title envelope had an empty title field");
    }
    Ok(title)
}

/// Caps `s` for log and error messages, so a long Haiku refusal stays readable.
fn truncate_for_log(s: &str) -> String {
    const LOG_CAP: usize = 120;
    let prefix: String = s.chars().take(LOG_CAP).collect();
    if prefix.len() == s.len() && s.width() <= LOG_CAP {
        return prefix;
    }
    truncate_to_width(&format!("{prefix}{ELLIPSIS}"), LOG_CAP + ELLIPSIS_WIDTH)
}

/// Strips a surrounding triple-backtick code fence and its optional language tag, leaving other
/// input untouched.
fn strip_code_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    let body_start = rest.find('\n').map_or(0, |i| i + 1);
    let body = &rest[body_start..];
    body.trim_end().strip_suffix("```").unwrap_or(body).trim()
}

/// Truncates `text` to `max_chars`, keeping the tail, where the request usually is.
fn truncate_prompt(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let skip = text.chars().count() - max_chars;
    text.chars().skip(skip).collect()
}

#[derive(Deserialize)]
struct TitleEnvelope {
    title: String,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use wiremock::matchers::{body_partial_json, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::anthropic::local_testing::{COMPLETION_USAGE, completion_body, test_client};
    use crate::client::anthropic::testing::api_key;
    use crate::message::Message;
    use crate::protocol::testing::CapturingSink;
    use crate::session::handle::testing::dead;
    use crate::session::store::testing::test_store;

    fn title_client(base_url: String) -> Client {
        test_client(base_url, api_key(), HAIKU_MODEL)
    }

    /// The usage record of one title request that `client` sent.
    fn title_record(client: &Client) -> UsageRecord {
        UsageRecord {
            model: HAIKU_MODEL.to_owned(),
            cache_ttl: client.prompt_cache_ttl(),
            tokens: COMPLETION_USAGE,
        }
    }

    /// Session handle with one recorded message, so `append_ai_title` writes to an existing file.
    async fn prepared_session(dir: &Path) -> SessionHandle {
        let store = test_store(dir);
        let handle = crate::session::handle::start(&store, HAIKU_MODEL, None);
        handle.record_message(Message::user("first prompt")).await;
        handle
    }

    mod spawn {
        use super::*;

        #[tokio::test]
        async fn success_notifies_sink_and_delivers_the_request_usage() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(completion_body(r#"{"title":"Fix auth"}"#)),
                )
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let session = prepared_session(dir.path()).await;
            let session_id = session.session_id().to_owned();
            let client = title_client(server.uri());
            let billed = title_record(&client);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

            let usage = spawn(client, session, tx, "first prompt".to_owned());

            assert_eq!(
                rx.recv().await,
                Some(Event::SessionTitleUpdated {
                    session_id,
                    title: "Fix auth".to_owned(),
                }),
            );
            assert_eq!(rx.recv().await, None);
            assert_eq!(usage.await.unwrap(), billed);
        }

        #[tokio::test]
        async fn error_emits_no_title_event_and_bills_nothing() {
            // A 400 is fatal, so the task fails without waiting out the retry backoff.
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
                .expect(1)
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let session = prepared_session(dir.path()).await;
            let client = title_client(server.uri());
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

            let usage = spawn(client, session, tx, "first prompt".to_owned());

            assert_eq!(rx.recv().await, None);
            assert!(usage.await.is_err(), "a failed request bills nothing");
        }
    }

    mod generate_and_record {
        use super::*;

        #[tokio::test]
        async fn happy_path_appends_title_and_notifies_sink() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .and(body_partial_json(serde_json::json!({
                    "model": "claude-haiku-5-5",
                    "max_tokens": 1_024,
                    "output_config": {"effort": "low"},
                })))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(completion_body(r#"{"title":"Fix login"}"#)),
                )
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let session = prepared_session(dir.path()).await;
            let client = title_client(server.uri());
            let sink = CapturingSink::default();

            generate_and_record(
                &client,
                &session,
                &sink,
                "first prompt",
                oneshot::channel().0,
            )
            .await
            .unwrap();

            let events = sink.events();
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    Event::SessionTitleUpdated { title, .. } if title == "Fix login"
                )),
                "sink got SessionTitleUpdated: {events:?}",
            );
        }

        #[tokio::test]
        async fn bills_but_skips_emit_when_renamed_during_haiku_call() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(completion_body(r#"{"title":"AI loses race"}"#)),
                )
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let session = prepared_session(dir.path()).await;
            session.set_manual_title("User wins".to_owned()).await;

            let client = title_client(server.uri());
            let sink = CapturingSink::default();
            let (billed, usage) = oneshot::channel();

            generate_and_record(&client, &session, &sink, "first prompt", billed)
                .await
                .unwrap();

            let events = sink.events();
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, Event::SessionTitleUpdated { .. })),
                "AI title must not emit when manual title is already set: {events:?}",
            );
            assert_eq!(usage.await.unwrap(), title_record(&client));
            let data = test_store(dir.path())
                .load_session_data(session.session_id())
                .unwrap();
            assert_eq!(data.usage, [title_record(&client)]);
        }

        #[tokio::test]
        async fn unwraps_code_fenced_json_envelope() {
            let server = MockServer::start().await;
            let raw = indoc! {r#"
                ```json
                {"title":"Add OAuth auth"}
                ```
            "#};
            Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_string(completion_body(raw)))
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let session = prepared_session(dir.path()).await;
            let client = title_client(server.uri());
            let sink = CapturingSink::default();

            generate_and_record(&client, &session, &sink, "prompt", oneshot::channel().0)
                .await
                .unwrap();

            let events = sink.events();
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, Event::SessionTitleUpdated { title, .. } if title == "Add OAuth auth")),
                "sink got SessionTitleUpdated: {events:?}",
            );
        }

        #[tokio::test]
        async fn titles_a_reply_without_usage_and_bills_nothing() {
            let server = MockServer::start().await;
            let body = serde_json::json!({
                "content": [{"type": "text", "text": r#"{"title":"Fix auth"}"#}],
            });
            Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let session = prepared_session(dir.path()).await;
            let client = title_client(server.uri());
            let sink = CapturingSink::default();
            let (billed, usage) = oneshot::channel();

            generate_and_record(&client, &session, &sink, "first prompt", billed)
                .await
                .unwrap();

            assert_eq!(
                sink.events(),
                [Event::SessionTitleUpdated {
                    session_id: session.session_id().to_owned(),
                    title: "Fix auth".to_owned(),
                }],
            );
            assert!(usage.await.is_err(), "no usage, no record");
            let data = test_store(dir.path())
                .load_session_data(session.session_id())
                .unwrap();
            assert_eq!(data.usage, []);
        }

        #[tokio::test]
        async fn conversational_reply_bails_without_a_title_but_bills_it() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_string(completion_body(
                    "I'd be happy to help! However, I need more details.",
                )))
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let session = prepared_session(dir.path()).await;
            let client = title_client(server.uri());
            let sink = CapturingSink::default();
            let (billed, usage) = oneshot::channel();

            let err = generate_and_record(&client, &session, &sink, "hi", billed)
                .await
                .expect_err("plain prose must fail parsing");

            assert!(format!("{err:#}").contains("title envelope"));
            assert!(
                !sink
                    .events()
                    .iter()
                    .any(|e| matches!(e, Event::SessionTitleUpdated { .. })),
                "no title event on parse failure",
            );
            assert_eq!(usage.await.unwrap(), title_record(&client));
            let data = test_store(dir.path())
                .load_session_data(session.session_id())
                .unwrap();
            assert_eq!(data.usage, [title_record(&client)]);
        }

        #[tokio::test]
        async fn surfaces_a_write_failure_before_the_title_event() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(completion_body(r#"{"title":"Fix auth"}"#)),
                )
                .mount(&server)
                .await;

            let session = dead("dead-session");
            let client = title_client(server.uri());
            let sink = CapturingSink::default();

            generate_and_record(
                &client,
                &session,
                &sink,
                "first prompt",
                oneshot::channel().0,
            )
            .await
            .unwrap();

            assert_eq!(
                sink.events(),
                [
                    Event::Error(
                        "Session write failed: session writer task has stopped. \
                         Conversation history may be incomplete."
                            .to_owned(),
                    ),
                    Event::SessionTitleUpdated {
                        session_id: "dead-session".to_owned(),
                        title: "Fix auth".to_owned(),
                    },
                ],
            );
        }

        #[tokio::test]
        async fn http_error_bails_with_context() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let session = prepared_session(dir.path()).await;
            let client = title_client(server.uri());
            let sink = CapturingSink::default();

            let err = generate_and_record(&client, &session, &sink, "hi", oneshot::channel().0)
                .await
                .expect_err("HTTP error must propagate");
            assert_eq!(
                format!("{err:#}"),
                "Haiku completion failed: Anthropic API error (HTTP 400 Bad Request): bad request"
            );
        }
    }

    mod title_output_format {
        use super::*;

        #[test]
        fn matches_title_envelope_shape() {
            let fmt = title_output_format();
            let v = serde_json::to_value(&fmt).unwrap();
            assert_eq!(v["type"], "json_schema");
            assert_eq!(v["schema"]["properties"]["title"]["type"], "string");
            assert_eq!(v["schema"]["required"], serde_json::json!(["title"]));
            assert_eq!(v["schema"]["additionalProperties"], false);
        }
    }

    mod parse_title {
        use super::*;

        #[test]
        fn extracts_json_title_field() {
            let out = parse_title(r#"{"title": "Fix auth bug"}"#).unwrap();
            assert_eq!(out, "Fix auth bug");
        }

        #[test]
        fn trims_whitespace_inside_json_envelope() {
            let out = parse_title(r#"{"title": "  padded  "}"#).unwrap();
            assert_eq!(out, "padded");
        }

        #[test]
        fn keeps_one_line_of_at_most_max_title_width() {
            let envelope = |title: String| serde_json::json!({ "title": title }).to_string();

            let short = parse_title(&envelope("\nFix auth flow\nDetails below".to_owned()));
            let long = parse_title(&envelope("word ".repeat(30)));

            assert_eq!(short.unwrap(), "Fix auth flow");
            assert_eq!(long.unwrap(), format!("{}wo...", "word ".repeat(15)));
        }

        #[test]
        fn errors_on_empty_title_field() {
            let err = parse_title(r#"{"title": ""}"#).unwrap_err().to_string();
            assert!(err.contains("empty title"), "got: {err}");
        }

        #[test]
        fn errors_on_plain_text_response() {
            let err = parse_title("I'd be happy to help! However, I need more details.")
                .unwrap_err()
                .to_string();
            assert!(err.contains("not a title envelope"), "got: {err}");
        }

        #[test]
        fn errors_on_empty_response() {
            assert!(parse_title("   ").is_err());
        }

        #[test]
        fn unwraps_json_code_fence() {
            // Haiku on some gateways wraps the JSON envelope in a fenced block.
            let raw = indoc! {r#"
                ```json
                {
                  "title": "Fix the login flow"
                }
                ```
            "#};
            assert_eq!(parse_title(raw).unwrap(), "Fix the login flow");
        }

        #[test]
        fn unwraps_bare_code_fence() {
            let raw = indoc! {r#"
                ```
                {"title":"Add OAuth auth"}
                ```
            "#};
            assert_eq!(parse_title(raw).unwrap(), "Add OAuth auth");
        }
    }

    mod truncate_for_log {
        use super::*;

        #[test]
        fn passes_short_strings_through() {
            assert_eq!(truncate_for_log("short"), "short");
        }

        #[test]
        fn caps_long_strings_with_ellipsis() {
            let cap = "a".repeat(120);
            assert_eq!(truncate_for_log(&cap), cap);
            for len in [121, 123, 500] {
                assert_eq!(truncate_for_log(&"a".repeat(len)), format!("{cap}..."));
            }
        }

        #[test]
        fn caps_multiline_and_zero_width_responses() {
            for text in [format!("x{}y", "\n".repeat(500)), "\u{200b}".repeat(500)] {
                let expected = format!("{}{ELLIPSIS}", text.chars().take(120).collect::<String>());
                assert_eq!(truncate_for_log(&text), expected);
            }
        }
    }

    mod strip_code_fence {
        use super::*;

        #[test]
        fn leaves_unwrapped_text_alone() {
            assert_eq!(strip_code_fence("hello"), "hello");
            assert_eq!(strip_code_fence(r#"{"title":"x"}"#), r#"{"title":"x"}"#);
        }

        #[test]
        fn handles_language_tag() {
            let raw = indoc! {"
                ```json
                body
                ```
            "};
            assert_eq!(strip_code_fence(raw), "body");
        }

        #[test]
        fn handles_no_opening_newline() {
            assert_eq!(strip_code_fence("```body```"), "body");
        }
    }

    mod truncate_prompt {
        use super::*;

        #[test]
        fn passes_short_text_through() {
            assert_eq!(truncate_prompt("short", 100), "short");
        }

        #[test]
        fn keeps_the_tail_of_long_text() {
            let long = "a".repeat(50) + "TAIL";
            let out = truncate_prompt(&long, 10);
            assert_eq!(out.chars().count(), 10);
            assert!(out.ends_with("TAIL"), "should retain tail: {out:?}");
        }

        #[test]
        fn respects_char_boundaries_for_multibyte() {
            let s: String = "\u{00e9}".repeat(1_000) + "tail";
            let out = truncate_prompt(&s, 4);
            assert_eq!(out, "tail");
        }
    }
}
