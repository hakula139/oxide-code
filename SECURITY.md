# Security Policy

## Supported Versions

Security fixes go into `main` and ship with the next release. Report vulnerabilities against the latest release or current `main`. Before 0.1.0, the latest release means the latest prerelease.

## Reporting a Vulnerability

Report vulnerabilities privately through a [GitHub security advisory](https://github.com/hakula139/oxide-code/security/advisories/new), not in public issues. Include:

- The affected version (`ox --version`) or commit, and your platform
- The permission mode and any relevant `allow` or `deny` rules
- Steps to reproduce or a proof of concept
- The impact and any mitigations you know of

The maintainer acknowledges a report within 7 days and shares an assessment within 14 days. For a confirmed vulnerability, the maintainer aims to release a fix within 30 days, coordinates disclosure with the reporter, and publishes the advisory once the fix is released. oxide-code has a single maintainer, so these are targets, and the advisory thread reports any delay.

## Trust Model

`ox` runs locally with the permissions of the user who starts it and has no OS-level sandbox. Its [permission gate](docs/guide/configuration.md#permission-tool-approval) determines which tool calls run, require approval, or are denied. In `manual` and `plan` modes, read, grep, and glob calls run without approval on any path you can read unless a `deny` rule matches. In `manual` mode, edits inside the working directory also run without approval.

Commands run by the `bash` tool inherit your environment, except for the credential variables `ox` uses to authenticate: `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, and `ANTHROPIC_CUSTOM_HEADERS`. Other secrets in your environment, such as cloud or GitHub tokens, are available to every command, including `echo`, which runs without approval.

In scope:

- Tool calls that bypass a check required by their permission mode, such as approval, a matching `deny` rule (including in `yolo` mode), a protected path, or the built-in deny list
- Approval prompts that misrepresent a call, or allow rules that match commands or paths excluded by their documented grammar
- Repository content, such as a project `.ox/config.toml`, that widens permissions, redirects credentials, extends the TLS trust store, or reads files outside the permission gate
- `ox` writing your API key or OAuth token to logs, session files, or files accessible to other users, passing it to a `bash` command, or sending it to any endpoint other than the configured endpoint or Anthropic's OAuth token endpoint
- Flaws in the release workflow, the Homebrew formula, the Nix flake, or the `cargo install --git` path that let someone other than the maintainer change what you install

Out of scope:

- Tool calls in `yolo` mode that no `deny` rule covers, calls you approved as shown in the prompt, and calls matched by your allow rules as documented
- Secrets that an allowed read, grep, or glob call returns, which the session transcript then records
- Prompt injection when the resulting tool calls still pass through the permission gate as configured
- Attacks that require prior write access outside the repository, such as access to your user config, environment, or shell startup files
- Behavior of a gateway, proxy, or CA bundle you configured
- Dependency advisories for code that `ox` never reaches
- Gate gaps already tracked in a public issue
