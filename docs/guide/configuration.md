# Configuration

## Authentication

oxide-code checks two credential sources in order:

### API key

Set the `ANTHROPIC_API_KEY` environment variable:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

This is the simplest method. The key is sent directly in the `x-api-key` header.

### Claude Code OAuth

If no API key is set, oxide-code reads OAuth credentials created by [Claude Code](https://code.claude.com/docs):

1. **macOS Keychain** — the `"Claude Code-credentials"` service entry, accessed via the `security-framework` crate.
2. **Credentials file** — `~/.claude/.credentials.json`.

When both sources exist, the credential with the later expiry is used. Expired tokens are refreshed automatically. On Linux, only the file source is available (no Keychain support).

You do not need to configure anything — if Claude Code is installed and authenticated, oxide-code picks up its credentials automatically.

## Environment variables

| Variable               | Default                     | Description             |
| ---------------------- | --------------------------- | ----------------------- |
| `ANTHROPIC_API_KEY`    | —                           | Anthropic API key       |
| `ANTHROPIC_MODEL`      | `claude-opus-4-6`           | Model to use            |
| `ANTHROPIC_BASE_URL`   | `https://api.anthropic.com` | API base URL            |
| `ANTHROPIC_MAX_TOKENS` | `16384`                     | Max tokens per response |
| `OX_SHOW_THINKING`     | `false`                     | Show extended thinking  |

Set `OX_SHOW_THINKING=1` to display the model's thinking process (dimmed text) when extended thinking is enabled for the model.
