//! Rough token estimates for a transcript or a whole request, at four bytes of JSON a token.

use crate::message::Message;
use crate::prompt::PromptParts;
use crate::tool::ToolDefinition;

/// Estimates the tokens of a whole request from its parts, for a transcript no request has reported
/// usage on.
pub(super) fn request_tokens(
    prompt: &PromptParts,
    tools: &[&ToolDefinition],
    transcript: &[Message],
) -> u32 {
    let system = &prompt.system;
    let text = system.stable.len() + system.dynamic.len() + prompt.user_context.len();
    let tools = serde_json::to_string(tools).map_or(0, |json| json.len());
    byte_tokens(text + tools).saturating_add(transcript_tokens(transcript))
}

/// Estimates the tokens `transcript` adds to a request, at four bytes of JSON each.
pub(super) fn transcript_tokens(transcript: &[Message]) -> u32 {
    transcript
        .iter()
        .map(rough_tokens)
        .fold(0, u32::saturating_add)
}

/// Estimates a message's tokens.
pub(super) fn rough_tokens(message: &Message) -> u32 {
    byte_tokens(serde_json::to_string(message).map_or(0, |json| json.len()))
}

fn byte_tokens(bytes: usize) -> u32 {
    u32::try_from(bytes / 4).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    mod request_tokens {
        use super::*;

        #[test]
        fn counts_the_prompt_the_tools_and_the_transcript_at_four_bytes_a_token() {
            let mut prompt = PromptParts::default();
            prompt.system.stable = "s".repeat(4_000);
            prompt.system.dynamic = "d".repeat(400);
            prompt.user_context = "u".repeat(4_000);
            let tools = [&ToolDefinition {
                name: "read".to_owned(),
                description: "r".repeat(3_948),
                input_schema: json!({}),
            }];
            let transcript = [Message::user("hi")];

            let tokens = request_tokens(&prompt, &tools, &transcript);

            assert_eq!(tokens, 2_100 + 1_000 + transcript_tokens(&transcript));
        }
    }
}
