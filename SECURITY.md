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

Commands run by the `bash` tool, and the `git` and `gh` processes `ox` starts itself, such as for the status line and `/diff`, inherit your environment, except for the credential variables `ox` uses to authenticate: `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, and `ANTHROPIC_CUSTOM_HEADERS`. Other secrets in your environment, such as cloud or GitHub tokens, stay available because tools such as `gh` and `aws` need them, so the approval gate is the boundary. Any command you approve or allow can read them. In `manual` mode, a command that runs without approval never expands a variable, so `echo $GITHUB_TOKEN` asks first, and the built-in `credentials` rule denies reads of any process's environment under `/proc` on Linux. The `bash` tool also drops `CDPATH`, so a `cd` lands where the permission gate resolved it.

The `git` and `gh` processes for the status line and `/diff` run outside the permission gate under git's own trust model, so in a repository you do not trust, `/diff` runs the programs its `.git/config` or `.gitattributes` name, such as a `filter.<name>.clean` driver or a `core.fsmonitor` hook, as typing `git diff` there would. The model cannot start these processes itself, since a `git` command it runs through the `bash` tool asks in `manual` mode unless an allow rule covers it, and `plan` mode denies it.

The gate reads a `bash` command with tree-sitter-bash and matches rules against the words bash would run. Where the parser cannot read a command the way bash does, such as on a syntax error, a word boundary bash draws differently, or a reserved word the grammar reads as a command name, the command asks in every mode, including `yolo`, unless a `deny` rule matches first. A command that sets a variable bash reads to find or expand commands, such as `PATH` or `BASH_CMDS`, asks the same way, since it can change what a later word runs without changing the word.

An `exec` deny or ask rule, including the built-in `rm` rule, is a guardrail against mistakes in every mode, and no rule of that kind is a security boundary. It matches every simple command the gate finds in a `bash` command, including those that a wrapper such as `sudo` or `xargs`, a `find -exec` action, or a literal `sh -c` or `eval` script runs. A rule that names a program cannot match a copy or link of it under another name, as in `cp /bin/rm ./x && ./x -rf ~`, and no analysis of a command line can see what a program runs once it starts, such as `python3 -c`, a build script that `make` or `cargo build` runs, or a git alias. A `read` or `edit` rule or a protected path applied to a `bash` command is a guardrail for the same reason, since a program such as `tee`, `cp`, or `python3` can open or write any file.

A gap in either of these properties is a vulnerability:

- In `manual` mode, a `bash` command runs without approval only if the gate parses all of it and nothing in it hides what it runs (as `$(...)`, `eval`, or `sh -c` does), every simple command in it is allowed by a rule or is a known-safe read, and every file it redirects output to is inside the working directory or an extra root, or allowed by an `edit` rule.
- A path `deny` or `ask` rule, built-in or configured, and a protected path hold for the read, grep, glob, edit, and write tools in every mode, since the gate resolves the path and the tool opens the path it checked. The exception is a symlink swapped into a parent directory after the check, which `openat`-style traversal or an OS-level sandbox ([#133](https://github.com/hakula139/oxide-code/issues/133)) would close.

To run untrusted work without watching it, run `ox` inside a sandbox or container, since `yolo` mode runs whatever the guardrails miss.

In scope:

- Tool calls that bypass a check required by their permission mode, such as approval, or a built-in or configured path `deny` or `ask` rule or protected path on a file tool (including in `yolo` mode)
- A `bash` command that runs without approval in `manual` mode although it breaks the `manual` property above
- Approval prompts that misrepresent a call, or allow rules that match commands or paths excluded by their documented grammar
- Repository content, such as a project `.ox/config.toml`, that widens permissions, redirects credentials, extends the TLS trust store, or reads files outside the permission gate
- `ox` writing your API key or OAuth token to logs, session files, or files accessible to other users, passing it to a `bash` command or a `git` or `gh` process, or sending it to any endpoint other than the configured endpoint or Anthropic's OAuth token endpoint
- Flaws in the release workflow, the Homebrew formula, the Nix flake, or the `cargo install --git` path that let someone other than the maintainer change what you install

Out of scope:

- Calls you approved as shown in the prompt, calls matched by your allow rules as documented, and tool calls in `yolo` mode that no path `deny` or `ask` rule or protected path on a file tool covers
- A `bash` command that gets past an `exec`, `read`, or `edit` rule or a protected path without matching it, such as through a copied binary, an interpreter, a build script, a git alias, or shell syntax the gate misreads, when it does not also break the `manual` property above. These checks are a guardrail for `bash`, so report such a gap in a public issue.
- Secrets that an allowed read, grep, or glob call returns, which the session transcript then records
- Prompt injection when the resulting tool calls still pass through the permission gate as configured
- Attacks that require prior write access outside the repository, such as access to your user config, environment, or shell startup files
- Behavior of a gateway, proxy, or CA bundle you configured
- Dependency advisories for code that `ox` never reaches
- Gate gaps already tracked in a public issue
