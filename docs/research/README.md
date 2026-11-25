# Research Notes

API references and surveys behind oxide-code's design. The API references summarize Anthropic's documentation and Claude Code's request shapes. A survey compares how Claude Code, [OpenAI Codex](https://github.com/openai/codex), and other coding agents handle an open design question, noting the verification date and versions. No survey is open today ([Research Notes](../conventions/writing.md#research-notes) covers their lifecycle). Shipped decisions live in [`docs/design/`](../design/).

## API References

| Document                                      | Description                                                                                 |
| --------------------------------------------- | ------------------------------------------------------------------------------------------- |
| [Anthropic API](api/anthropic.md)             | Credentials, headers, caching, request fields, gateway restrictions, token refresh, pricing |
| [Extended Thinking](api/extended-thinking.md) | Thinking configuration, streamed blocks, signatures, replay constraints                     |
| [System Prompt](api/system-prompt.md)         | Prompt assembly, instruction discovery, context injection, caching, request metadata        |
