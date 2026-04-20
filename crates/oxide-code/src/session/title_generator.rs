//! Background AI session title generator.
//!
//! Mirrors Claude Code's `generateSessionTitle` (see `../claude-code/src/
//! utils/sessionTitle.ts`): once a fresh session has recorded its first
//! user prompt, spawn a detached task that asks Haiku for a concise
//! 3-7 word sentence-case title, append it to the session file as a new
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

/// Haiku model used for title generation. Matches claude-code's
/// `getSmallFastModel()` default; small and fast, OAuth-compatible.
const HAIKU_MODEL: &str = "claude-haiku-4-5";

/// Output budget for the title response. 40 tokens comfortably fits the
/// 3-7 word JSON envelope the prompt demands; anything longer is a
/// Haiku misstep we'd rather cut off than bill.
const MAX_TOKENS: u32 = 40;

/// Clamp on the prompt we feed Haiku. Long first messages occasionally
/// contain pasted code or logs; truncating to 1 000 chars matches
/// claude-code's `MAX_CONVERSATION_TEXT` and keeps the title request
/// small and predictable.
const MAX_PROMPT_CHARS: usize = 1_000;

/// Title prompt. Kept close to claude-code's (sentence-case, 3-7 words,
/// JSON envelope) so we get the same output shape.
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
    let title = parse_title(&raw).context("Haiku returned an unparseable title")?;

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

/// Parse Haiku's response as the `{"title": "..."}` JSON envelope, with a
/// whitespace-trimmed plain-text fallback for the case where Haiku skips
/// the JSON wrapper entirely (rare, but cheap to support).
fn parse_title(response: &str) -> Result<String> {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        bail!("empty response");
    }

    if let Ok(TitleEnvelope { title }) = serde_json::from_str::<TitleEnvelope>(trimmed) {
        let cleaned = title.trim();
        if !cleaned.is_empty() {
            return Ok(cleaned.to_owned());
        }
    }
    Ok(trimmed.to_owned())
}

/// Truncate `text` to at most `max_chars` characters, preferring the tail
/// when the input is long (recent context wins). Matches claude-code's
/// `extractConversationText` behaviour for long-prompt sessions.
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
    fn parse_title_falls_back_to_raw_on_empty_title_field() {
        // Haiku returned the envelope but an empty title — fall back to
        // the raw response so we don't silently end up with nothing.
        let raw = r#"{"title": ""}"#;
        let out = parse_title(raw).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn parse_title_falls_back_to_trimmed_plain_text() {
        let out = parse_title("  Refactor API client  ").unwrap();
        assert_eq!(out, "Refactor API client");
    }

    #[test]
    fn parse_title_errors_on_empty_response() {
        assert!(parse_title("   ").is_err());
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
