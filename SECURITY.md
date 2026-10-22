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

Before 1.0, the maintainer fixes vulnerabilities found in-house in public pull requests and notes them in the release notes, without an advisory.

## Trust Model

`ox` runs locally with the permissions of the user who starts it and has no OS-level sandbox. Its [permission gate](docs/guide/permissions.md) determines which tool calls run, require approval, or are denied. In `manual` and `plan` modes, read, grep, and glob calls run without approval on any path you can read unless a `deny` or `ask` rule matches. In `manual` mode, edits inside the working directory also run without approval.

Commands run by the `bash` tool, and the `git` and `gh` processes `ox` starts itself, such as for the status line and `/diff`, inherit your environment, except for the credential variables `ox` uses to authenticate: `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, and `ANTHROPIC_CUSTOM_HEADERS`. Other secrets in your environment, such as cloud or GitHub tokens, stay available because tools such as `gh` and `aws` need them, so the approval gate is the boundary. Any command you approve or allow can read them. In `manual` mode, a command that runs without approval never expands a variable, so `echo $GITHUB_TOKEN` asks first, and the built-in `credentials` rule denies reads of any process's environment under `/proc` on Linux.

The gate reads a `bash` command with tree-sitter-bash and matches rules against the words bash would run. Where the parser cannot read a command the way bash does, such as on a syntax error, a word boundary bash draws differently, or a reserved word the grammar reads as a command name, the command asks in every mode, including `yolo`, unless a `deny` rule matches first. A command that sets a variable bash reads to find or expand commands, such as `PATH` or `BASH_CMDS`, asks the same way, since it can change what a later word runs without changing the word.

In scope:

- Tool calls that bypass a check required by their permission mode, such as approval, a matching `deny` rule (including in `yolo` mode), a protected path, or the built-in deny list
- Approval prompts that misrepresent a call, or allow rules that match commands or paths excluded by their documented grammar
- Repository content, such as a project `.ox/config.toml`, that widens permissions, redirects credentials, extends the TLS trust store, or reads files outside the permission gate
- `ox` writing your API key or OAuth token to logs, session files, or files accessible to other users, passing it to a `bash` command or a `git` or `gh` process, or sending it to any endpoint other than the configured endpoint or Anthropic's OAuth token endpoint
- Flaws in the release workflow, the Homebrew formula, the Nix flake, or the `cargo install --git` path that let someone other than the maintainer change what you install

Out of scope:

- Calls you approved as shown in the prompt, calls matched by your allow rules as documented, and tool calls in `yolo` mode that no `deny` rule covers, other than a `bash` command the gate cannot read the way bash does or that hides the program it runs, which still asks
- Secrets that an allowed read, grep, or glob call returns, which the session transcript then records
- Prompt injection when the resulting tool calls still pass through the permission gate as configured
- Attacks that require prior write access outside the repository, such as access to your user config, environment, or shell startup files
- Behavior of a gateway, proxy, or CA bundle you configured
- Dependency advisories for code that `ox` never reaches
- Gate gaps already tracked in a public issue
