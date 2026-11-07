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

Before 1.0, the maintainer fixes vulnerabilities found in-house in public pull requests and notes them in the release notes, by default without an advisory. The release notes link every published advisory, whether it covers a reported or an in-house vulnerability.

## Trust Model

User configuration and explicit environment variables select the endpoint, credentials, TLS trust, and gateway headers. A project `.ox/config.toml` cannot set those fields, the gateway request switches, or the approval classifier's model and instructions. Fixed credentials can come from a private inline user value or a secret file read at startup. Stored logins are sent only to the Anthropic API, while an explicitly configured fixed OAuth token uses the configured endpoint. `ox config check` validates credential declarations without opening secret files. Config snapshots, diagnostic errors, and Debug output omit credential values.

`ox` runs with the permissions of the user who starts it and has no OS sandbox. Its [permission gate](docs/guide/permissions.md) applies configured deny rules, the read-only restriction of `plan`, configured ask rules, and configured or session allow rules, in that order. Calls confined to session state, such as task updates and reading an already-started shell's output, run after deny and ask checks. Other unmatched calls use their mode: `auto` asks a separate model to allow the action or request approval, `manual` requests approval, `yolo` allows, and `plan` allows remaining read-only calls. There are no shipped protected-path rules or implicit workspace-edit approvals.

Automatic classification uses the configured API endpoint and credentials with a separate model. Its request includes user-authored session prompts, the complete validated pending tool input, any resolved native path, and working-directory and workspace-root information. It excludes assistant explanations and tool output, performs no investigation, and caches no verdicts. The classifier cannot override explicit deny or ask rules. A failed request, invalid response, or explicit refusal requires approval. A run without an approval frontend leaves that call unapproved. Model judgments can be mistaken, so classification provides no sandbox boundary.

Commands run by the `bash` tool, and the `git` and `gh` processes `ox` starts itself, inherit the user's environment except for the credential variables `ox` uses to authenticate, which [Authentication](docs/guide/authentication.md#commands-and-credentials) lists. Other environment secrets remain available to commands you allow. The `git` and `gh` processes used for the status line and `/diff` run outside the permission gate under git's trust model, including programs selected by repository configuration.

The gate uses tree-sitter-bash to match `exec` rules against literal simple-command words. A command-specific allow must cover every component, while the whole-class exec rule `*` permits any Bash call after deny and ask checks. Dynamic words, redirections, and unsupported syntax remain unmatched by command-specific allow rules. Wrappers and script arguments remain ordinary words. Bash does not use native `read` and `edit` path rules, and no shell filesystem effects are inferred. A permitted command can access any resource the operating system allows. [Permissions](docs/guide/permissions.md#rules) defines the supported matching grammar.

Native read, grep, glob, edit, and write tools resolve the path before the gate and use that resolved path when running. Configured path deny and ask rules match the requested and resolved spellings, equivalent directory aliases, and folded names. Search walks withhold matching files. A symlink swapped into a parent directory after the check remains a gap that `openat`-style traversal or an OS sandbox ([#133](https://github.com/hakula139/oxide-code/issues/133)) would close. Edit and Write recheck files before publication, but concurrent changes and crashes can still affect an in-place write. [File Edits](docs/design/file-edits.md) defines these integrity checks.

Run untrusted work in an external sandbox or container. Configured rules, classifier judgment, and user approval govern which calls start, while the operating system determines what a running program can access.

In scope:

- Tool calls that bypass explicit configured deny or ask rules within their documented matching scope, or execute after an unapproved or rejected request.
- Mutating calls that bypass `plan` mode.
- Approval prompts that misrepresent an action, fabricated user approvals, or allow rules that match outside their documented grammar.
- Repository content that widens permissions, changes classifier instructions, redirects credentials, extends TLS trust, or reads files outside the permission gate.
- `ox` disclosing its authentication credentials through logs, persisted output, child-process environments, or an endpoint outside the configured authentication contract.
- Flaws in the release workflow, Homebrew formula, Nix flake, or installation path that let another party change what users install.

Out of scope:

- Calls approved by the user, configured allow rules, the classifier, or `yolo` within the documented policy. Report classifier misjudgments as product bugs unless they bypass an explicit policy boundary.
- Filesystem or other effects of a Bash call permitted within the documented exec matching scope.
- Secrets returned by an allowed file read or search and then recorded in the transcript.
- Prompt injection whose resulting actions still pass through the configured permission gate.
- Attacks requiring prior write access to user configuration, the environment, or shell startup files.
- Behavior of a configured gateway, proxy, or CA bundle.
- Dependency advisories for unreachable code, and gate gaps already tracked in a public issue.
