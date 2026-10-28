# Research Notes

API references and surveys behind oxide-code's design. The API references summarize Anthropic's documentation and the request shapes captured from Claude Code. A survey compares how [Claude Code](https://github.com/hakula139/claude-code), [OpenAI Codex](https://github.com/openai/codex), and other coding agents handle a design question that is still open, and names the date and versions it was verified against. No survey is open today. [Research Notes](../conventions/writing.md#research-notes) covers how a survey tracks its open questions and when it is deleted.

[`docs/design/`](../design/) holds the decisions that shipped.

## API References

| Document                                      | Description                                                                                 |
| --------------------------------------------- | ------------------------------------------------------------------------------------------- |
| [Anthropic API](api/anthropic.md)             | Credentials, headers, caching, request fields, gateway restrictions, token refresh, pricing |
| [Extended Thinking](api/extended-thinking.md) | Thinking configuration, streamed blocks, signatures, replay constraints                     |
| [System Prompt](api/system-prompt.md)         | Prompt assembly, instruction discovery, context injection, caching, request metadata        |
