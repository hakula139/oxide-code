//! Background AI session title generator.
//!
//! Once a fresh session has recorded its first user prompt, spawn a
//! detached task that asks Haiku for a concise 3-7 word sentence-case
//! title, append it to the session file as a new
//! [`Entry::Title`][crate::session::entry::Entry::Title] with source
//! [`AiGenerated`][crate::session::entry::TitleSource::AiGenerated], and
//! push an [`AgentEvent::SessionTitleUpdated`] so the TUI status bar
//! updates live.
//!
//! Failure modes (Haiku timeout / malformed response / write error) all
//! warn-log only — the first-prompt title stays on disk and in the UI.
//! Callers wire this on fresh sessions exactly once; resumed sessions skip
//! regeneration (the original title, if any, is already on disk).

use anyhow::{Context, Result, bail};
use indoc::indoc;
use serde::Deserialize;
use tracing::warn;

use crate::agent::event::{AgentEvent, AgentSink};
use crate::client::anthropic::Client;
use crate::client::anthropic::wire::OutputFormat;
use crate::session::handle::SessionHandle;

// ── Constants ──

/// Haiku model used for title generation. Small and fast, OAuth-compatible,
/// and cheap enough to fire on every fresh session without thought.
const HAIKU_MODEL: &str = "claude-haiku-4-5";

/// Output budget for the title response. 40 tokens comfortably fits the
/// 3-7 word JSON envelope the prompt demands; anything longer is a
/// Haiku misstep we'd rather cut off than bill.
const MAX_TOKENS: u32 = 40;

/// Clamp on the prompt we feed Haiku. Long first messages occasionally
/// contain pasted code or logs; truncating to 1 000 chars keeps the title
/// request small, predictable, and cheap regardless of input size.
const MAX_PROMPT_CHARS: usize = 1_000;

/// Title prompt. Instructs the model to return JSON with a single
/// `title` field; the paired JSON-schema output format (see
/// [`title_output_format`]) enforces that shape regardless of whether
/// the model would otherwise try to answer the user's prompt
/// conversationally.
const SYSTEM_PROMPT: &str = indoc! {r#"
    Generate a concise, sentence-case title (3-7 words) that captures the main topic or goal of this coding session. The title should be clear enough that the user recognizes the session in a list. Use sentence case: capitalize only the first word and proper nouns.

    Return JSON with a single "title" field.

    Good examples:
    {"title": "Fix login button on mobile"}
    {"title": "Add OAuth authentication"}
    {"title": "Debug failing CI tests"}
    {"title": "Refactor API client error handling"}

    Bad (too vague): {"title": "Code changes"}
    Bad (too long): {"title": "Investigate and fix the issue where the login button does not respond on mobile devices"}
    Bad (wrong case): {"title": "Fix Login Button On Mobile"}
"#};

// ── Public API ──

/// Spawns a detached task that asks Haiku for a title, records it on
/// `session`, and notifies `sink`.
///
/// `first_prompt` should be the user's first message text — truncated here
/// to [`MAX_PROMPT_CHARS`] to keep the Haiku request small.
pub(crate) fn spawn<S>(client: Client, session: SessionHandle, sink: S, first_prompt: String)
where
    S: AgentSink + Clone + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = generate_and_record(&client, &session, &sink, &first_prompt).await {
            // Expected failure: network hiccup, rate-limit, or non-JSON
            // Haiku reply. The first-prompt title stays in the file and
            // status bar; the warning routes per
            // `crate::util::log::init_tracing`.
            warn!("AI title generation failed: {e}");
        }
    });
}

// ── Generation ──

/// Single-shot title generator: call Haiku, parse, append, notify.
async fn generate_and_record(
    client: &Client,
    session: &SessionHandle,
    sink: &impl AgentSink,
    first_prompt: &str,
) -> Result<()> {
    let prompt = truncate_prompt(first_prompt, MAX_PROMPT_CHARS);
    let output_format = title_output_format();
    let raw = client
        .complete(
            HAIKU_MODEL,
            SYSTEM_PROMPT,
            &prompt,
            MAX_TOKENS,
            Some(&output_format),
        )
        .await
        .context("Haiku completion failed")?;
    let title = parse_title(&raw).context("Haiku returned a malformed title")?;

    let outcome = session.append_ai_title(title.clone()).await;
    sink.session_write_error(outcome.failure.as_deref());

    _ = sink.send(AgentEvent::SessionTitleUpdated {
        session_id: session.session_id().to_owned(),
        title,
    });
    Ok(())
}

/// `{"title": string}` schema for [`Client::complete`]'s structured
/// outputs. Built once per call — the schema JSON itself is small and
/// constructing a `serde_json::Value` is cheap compared to the HTTP
/// round-trip, so a `LazyLock` optimization would be theatre.
///
/// Without this, a first prompt phrased as a direct request (e.g.
/// `"see what's next to do in this repo"`) would frequently drive Haiku
/// to answer the task instead of titling it, and [`parse_title`] would
/// then bail on the conversational reply. The schema forces Haiku onto
/// the envelope shape regardless of how the prompt scans.
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

/// Parses Haiku's response as the `{"title": "..."}` JSON envelope, or
/// bail with enough context for the caller's warn-log.
///
/// The envelope is mandatory. A bare plain-text response is almost
/// always Haiku's conversational refusal to the title task ("I'd be
/// happy to help! However, I need more details..." for short prompts
/// like `hi`), and using that prose as the title is worse than keeping
/// the first-prompt title we already wrote to disk.
///
/// Triple-backtick code fences (`` ```json ... ``` ``) are stripped
/// first — Haiku wraps the envelope that way on some gateways.
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
    let cleaned = title.trim();
    if cleaned.is_empty() {
        bail!("title envelope had an empty title field");
    }
    Ok(cleaned.to_owned())
}

/// Cap a string for inclusion in a log / error message. Haiku refusals
/// run long; truncate so the warn-log stays readable.
fn truncate_for_log(s: &str) -> String {
    const LOG_CAP: usize = 120;
    if s.chars().count() <= LOG_CAP {
        return s.to_owned();
    }
    let head: String = s.chars().take(LOG_CAP).collect();
    format!("{head}...")
}

/// Strips a surrounding triple-backtick markdown code fence (with an
/// optional `json` / `text` / ... language tag) from `s`, returning the
/// inner body trimmed of whitespace. Leaves any input that isn't wrapped
/// in a fence untouched.
fn strip_code_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    let body_start = rest.find('\n').map_or(0, |i| i + 1);
    let body = &rest[body_start..];
    body.trim_end().strip_suffix("```").unwrap_or(body).trim()
}

/// Truncates `text` to at most `max_chars` characters, preferring the tail
/// when the input is long. The tail of a long first message is usually the
/// actual request (setup, pasted logs, or context appear earlier), so the
/// title signal lives there.
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

    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::agent::event::CapturingSink;
    use crate::client::anthropic::testing::{completion_body, test_client};
    use crate::config::Auth;
    use crate::message::Message;
    use crate::session::store::test_store;

    // ── Fixtures ──

    fn title_client(base_url: String) -> Client {
        test_client(base_url, Auth::ApiKey("sk".to_owned()), HAIKU_MODEL)
    }

    /// Sink backed by an mpsc channel so tests can wait on `recv()`
    /// with a timeout instead of polling `CapturingSink`.
    #[derive(Clone)]
    struct ChannelSink(tokio::sync::mpsc::UnboundedSender<AgentEvent>);

    impl crate::agent::event::AgentSink for ChannelSink {
        fn send(&self, event: AgentEvent) -> anyhow::Result<()> {
            _ = self.0.send(event);
            Ok(())
        }
    }

    /// Session handle with one recorded message so the file is
    /// materialized when `append_ai_title` runs.
    async fn prepared_session(dir: &Path) -> SessionHandle {
        let store = test_store(dir);
        let handle = crate::session::handle::start(&store, HAIKU_MODEL);
        handle.record_message(Message::user("first prompt")).await;
        handle
    }

    // ── spawn ──

    #[tokio::test]
    async fn spawn_success_notifies_sink_with_session_title_event() {
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
        let client = title_client(server.uri());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ChannelSink(tx);

        spawn(client, session, sink, "first prompt".to_owned());

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for title event")
            .expect("channel closed before event");

        assert!(
            matches!(&event, AgentEvent::SessionTitleUpdated { title, .. } if title == "Fix auth"),
            "unexpected event: {event:?}",
        );
    }

    #[tokio::test]
    async fn spawn_error_does_not_emit_session_title_event() {
        // 503 → generate_and_record fails → warn-log only, no event.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string("bad gateway"))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let session = prepared_session(dir.path()).await;
        let client = title_client(server.uri());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ChannelSink(tx);

        spawn(client, session, sink, "first prompt".to_owned());

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            rx.try_recv().is_err(),
            "no event should be emitted on generation failure",
        );
    }

    // ── generate_and_record ──

    #[tokio::test]
    async fn generate_and_record_happy_path_appends_title_and_notifies_sink() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(completion_body(r#"{"title":"Fix login"}"#)),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let session = prepared_session(dir.path()).await;
        let client = title_client(server.uri());
        let sink = CapturingSink::new();

        generate_and_record(&client, &session, &sink, "first prompt")
            .await
            .unwrap();

        let events = sink.events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::SessionTitleUpdated { title, .. } if title == "Fix login")),
            "sink got SessionTitleUpdated: {events:?}",
        );
    }

    #[tokio::test]
    async fn generate_and_record_unwraps_code_fenced_json_envelope() {
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
        let sink = CapturingSink::new();

        generate_and_record(&client, &session, &sink, "prompt")
            .await
            .unwrap();

        let events = sink.events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::SessionTitleUpdated { title, .. } if title == "Add OAuth auth")),
            "sink got SessionTitleUpdated: {events:?}",
        );
    }

    #[tokio::test]
    async fn generate_and_record_conversational_reply_bails_without_updating_title() {
        // Haiku sometimes answers the prompt instead of titling it. The
        // bail keeps the first-prompt title on disk and out of the UI.
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
        let sink = CapturingSink::new();

        let err = generate_and_record(&client, &session, &sink, "hi")
            .await
            .expect_err("plain prose must fail parsing");
        assert!(format!("{err:#}").contains("title envelope"));
        assert!(
            !sink
                .events()
                .iter()
                .any(|e| matches!(e, AgentEvent::SessionTitleUpdated { .. })),
            "no title event on parse failure",
        );
    }

    #[tokio::test]
    async fn generate_and_record_write_failure_emits_error_and_title_events() {
        // A write failure surfaces an Error to the sink but must not
        // skip the SessionTitleUpdated event — UI keeps updating.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(completion_body(r#"{"title":"Fix auth"}"#)),
            )
            .mount(&server)
            .await;

        let session = super::super::handle::testing::dead("dead-session");
        let client = title_client(server.uri());
        let sink = CapturingSink::new();

        generate_and_record(&client, &session, &sink, "first prompt")
            .await
            .unwrap();

        let events = sink.events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error(m) if m.contains("Session write failed"))),
            "Error event expected for write failure: {events:?}",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::SessionTitleUpdated { title, .. } if title == "Fix auth")),
            "SessionTitleUpdated expected even after write failure: {events:?}",
        );
    }

    #[tokio::test]
    async fn generate_and_record_http_error_bails_with_context() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string("bad gateway"))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let session = prepared_session(dir.path()).await;
        let client = title_client(server.uri());
        let sink = CapturingSink::new();

        let err = generate_and_record(&client, &session, &sink, "hi")
            .await
            .expect_err("HTTP error must propagate");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Haiku completion failed"),
            "outer context: {msg}"
        );
        assert!(msg.contains("503"), "status surfaced: {msg}");
    }

    // ── title_output_format ──

    #[test]
    fn title_output_format_matches_title_envelope_shape() {
        // The schema must line up with [`TitleEnvelope`] so a
        // schema-conforming response parses via `parse_title`.
        let fmt = title_output_format();
        let v = serde_json::to_value(&fmt).unwrap();
        assert_eq!(v["type"], "json_schema");
        assert_eq!(v["schema"]["properties"]["title"]["type"], "string");
        assert_eq!(v["schema"]["required"], serde_json::json!(["title"]));
        assert_eq!(v["schema"]["additionalProperties"], false);
    }

    // ── parse_title ──

    #[test]
    fn parse_title_extracts_json_title_field() {
        let out = parse_title(r#"{"title": "Fix auth bug"}"#).unwrap();
        assert_eq!(out, "Fix auth bug");
    }

    #[test]
    fn parse_title_trims_whitespace_inside_json_envelope() {
        let out = parse_title(r#"{"title": "  padded  "}"#).unwrap();
        assert_eq!(out, "padded");
    }

    #[test]
    fn parse_title_errors_on_empty_title_field() {
        // An envelope with an empty title is as useless as no title at
        // all. Bail so the first-prompt fallback stays in place.
        let err = parse_title(r#"{"title": ""}"#).unwrap_err().to_string();
        assert!(err.contains("empty title"), "got: {err}");
    }

    #[test]
    fn parse_title_errors_on_plain_text_response() {
        // Haiku's conversational refusal ("I'd be happy to help!
        // However, I need more details...") would otherwise land on the
        // status bar as a multi-sentence "title". Require the JSON
        // envelope so the first-prompt title survives instead.
        let err = parse_title("I'd be happy to help! However, I need more details.")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a title envelope"), "got: {err}");
    }

    #[test]
    fn parse_title_errors_on_empty_response() {
        assert!(parse_title("   ").is_err());
    }

    #[test]
    fn parse_title_unwraps_json_code_fence() {
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
    fn parse_title_unwraps_bare_code_fence() {
        let raw = indoc! {r#"
            ```
            {"title":"Add OAuth auth"}
            ```
        "#};
        assert_eq!(parse_title(raw).unwrap(), "Add OAuth auth");
    }

    // ── truncate_for_log ──

    #[test]
    fn truncate_for_log_passes_short_strings_through() {
        assert_eq!(truncate_for_log("short"), "short");
    }

    #[test]
    fn truncate_for_log_caps_long_strings_with_ellipsis() {
        let long = "a".repeat(500);
        let out = truncate_for_log(&long);
        assert!(out.ends_with("..."), "got: {out:?}");
        assert_eq!(out.chars().count(), 123, "got: {out:?}");
    }

    // ── strip_code_fence ──

    #[test]
    fn strip_code_fence_leaves_unwrapped_text_alone() {
        assert_eq!(strip_code_fence("hello"), "hello");
        assert_eq!(strip_code_fence(r#"{"title":"x"}"#), r#"{"title":"x"}"#);
    }

    #[test]
    fn strip_code_fence_handles_language_tag() {
        let raw = indoc! {"
            ```json
            body
            ```
        "};
        assert_eq!(strip_code_fence(raw), "body");
    }

    #[test]
    fn strip_code_fence_handles_no_opening_newline() {
        // Single-line fenced block — no language tag, no newline.
        assert_eq!(strip_code_fence("```body```"), "body");
    }

    // ── truncate_prompt ──

    #[test]
    fn truncate_prompt_passes_short_text_through() {
        assert_eq!(truncate_prompt("short", 100), "short");
    }

    #[test]
    fn truncate_prompt_keeps_the_tail_of_long_text() {
        let long = "a".repeat(50) + "TAIL";
        let out = truncate_prompt(&long, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with("TAIL"), "should retain tail: {out:?}");
    }

    #[test]
    fn truncate_prompt_respects_char_boundaries_for_multibyte() {
        // 1000 é + "tail": truncate to 4 chars should give just "tail",
        // not broken bytes from an é midpoint.
        let s: String = "\u{00e9}".repeat(1_000) + "tail";
        let out = truncate_prompt(&s, 4);
        assert_eq!(out, "tail");
    }
}
