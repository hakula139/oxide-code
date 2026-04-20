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

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use indoc::indoc;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::warn;

use crate::agent::event::{AgentEvent, AgentSink};
use crate::client::anthropic::Client;
use crate::session::manager::SessionManager;
use crate::session::writer::log_session_err;

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

/// Title prompt. Constrains the output to sentence-case, 3-7 words, wrapped
/// in a JSON envelope so [`parse_title`] can extract the title reliably.
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

/// Spawn a detached task that asks Haiku for a title, records it on
/// `session`, and notifies `sink`.
///
/// `first_prompt` should be the user's first message text — truncated here
/// to [`MAX_PROMPT_CHARS`] to keep the Haiku request small.
pub(crate) fn spawn<S>(
    client: Client,
    session: Arc<Mutex<SessionManager>>,
    sink: S,
    first_prompt: String,
) where
    S: AgentSink + Clone + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = generate_and_record(&client, &session, &sink, &first_prompt).await {
            // Expected failure mode: transient network hiccup, rate-limit,
            // or Haiku returning non-JSON. The first-prompt title stays in
            // the file and in the status bar; the user never sees this.
            warn!("AI title generation failed: {e}");
        }
    });
}

/// Single-shot title generator: call Haiku, parse, append, notify.
async fn generate_and_record(
    client: &Client,
    session: &Mutex<SessionManager>,
    sink: &impl AgentSink,
    first_prompt: &str,
) -> Result<()> {
    let prompt = truncate_prompt(first_prompt, MAX_PROMPT_CHARS);
    let raw = client
        .complete(HAIKU_MODEL, SYSTEM_PROMPT, &prompt, MAX_TOKENS)
        .await
        .context("Haiku completion failed")?;
    let title = parse_title(&raw).context("Haiku returned a malformed title")?;

    // Hold the session lock only for the append. `append_ai_title` does
    // one small write + flush; holding longer would block new user
    // messages from being recorded.
    {
        let mut s = session.lock().await;
        let r = s.append_ai_title(&title);
        log_session_err(r, &mut s, Some(sink));
    }

    _ = sink.send(AgentEvent::SessionTitleUpdated(title));
    Ok(())
}

/// Parse Haiku's response as the `{"title": "..."}` JSON envelope, or
/// bail with enough context for the caller's warn-log.
///
/// The envelope is mandatory. A bare plain-text response is almost
/// always Haiku's conversational refusal to the title task ("I'd be
/// happy to help! However, I need more details…" for short prompts
/// like `hi`), and using that prose as the title is worse than keeping
/// the first-prompt title we already wrote to disk.
///
/// Triple-backtick code fences (`` ```json … ``` ``) are stripped
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
    format!("{head}…")
}

/// Strip a surrounding triple-backtick markdown code fence (with an
/// optional `json` / `text` / … language tag) from `s`, returning the
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

/// Truncate `text` to at most `max_chars` characters, preferring the tail
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
    use super::*;

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
        // However, I need more details…") would otherwise land on the
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

    // ── truncate_for_log ──

    #[test]
    fn truncate_for_log_passes_short_strings_through() {
        assert_eq!(truncate_for_log("short"), "short");
    }

    #[test]
    fn truncate_for_log_caps_long_strings_with_ellipsis() {
        let long = "a".repeat(500);
        let out = truncate_for_log(&long);
        assert!(out.ends_with('…'), "got: {out:?}");
        assert!(
            out.chars().count() <= 121,
            "got {} chars",
            out.chars().count()
        );
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
