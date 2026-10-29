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

User configuration and explicit environment variables select the endpoint, credentials, TLS trust, and gateway headers. A project `.ox/config.toml` cannot set those fields or the gateway request switches. Fixed credentials can come from a private inline user value or a secret file read at startup. Stored logins are sent only to the Anthropic API, while an explicitly configured fixed OAuth token uses the configured endpoint. `ox config check` validates credential declarations without opening secret files. Config snapshots, diagnostic errors, and Debug output omit credential values.

`ox` runs locally with the permissions of the user who starts it and has no OS-level sandbox. Its [permission gate](docs/guide/permissions.md) determines which tool calls run, require approval, or are denied. In `manual` and `plan` modes, read, grep, and glob calls run without approval on any path you can read unless a `deny` or `ask` rule matches. In `manual` mode, edits inside the working directory also run without approval.

Commands run by the `bash` tool, and the `git` and `gh` processes `ox` starts itself, such as for the status line and `/diff`, inherit your environment, except for the credential variables `ox` uses to authenticate, which [Authentication](docs/guide/authentication.md#commands-and-credentials) lists. Other secrets in your environment, such as cloud or GitHub tokens, stay available because tools such as `gh` and `aws` need them, so the approval gate is the boundary. Any command you approve or allow can read them. In `manual` mode, a command that runs without approval never expands a variable, so `echo $GITHUB_TOKEN` asks first, and the built-in `credentials` rule denies reads of any process's environment under `/proc` on Linux.

The `git` and `gh` processes for the status line and `/diff` run outside the permission gate under git's own trust model, so in a repository you do not trust, `/diff` runs the programs its `.git/config` or `.gitattributes` name, such as a `filter.<name>.clean` driver or a `core.fsmonitor` hook, as typing `git diff` there would. The model cannot start these processes itself, since a `git` command it runs through the `bash` tool asks in `manual` mode unless an allow rule covers it, and `plan` mode denies it.

The gate reads a `bash` command with tree-sitter-bash and matches rules against the words bash would run. It recognizes the command's syntax by an allowlist, and a command holding anything outside it asks in `manual` unless a `deny` rule matches first, while `yolo` runs it unless a `deny` rule or one of your `ask` rules matches what the gate can read of it. That covers syntax the parser cannot read the way bash does, such as a syntax error, a word boundary bash draws differently, or a reserved word the grammar reads as a command name, and syntax that can change what a later word runs without changing the word, such as arithmetic, a function definition, or a statement that sets `PATH` or `BASH_CMDS`.

An `exec` deny or ask rule, including the built-in `rm` rule, is a guardrail against mistakes wherever it applies, and no rule of that kind is a security boundary. It matches every simple command the gate finds in a `bash` command, including those that a wrapper such as `sudo` or `xargs`, a `find -exec` action, a substitution, or a literal `sh -c` or `eval` script runs. A rule that names a program cannot match a copy or link of it under another name, as in `cp /bin/rm ./x && ./x -rf ~`, and no analysis of a command line can see what a program runs once it starts, such as `python3 -c`, a build script that `make` or `cargo build` runs, or a git alias. A `read` or `edit` rule or a protected path applied to a `bash` command is a guardrail for the same reason, since a program such as `tee`, `cp`, or `python3` can open or write any file.

A `read` pattern also reaches a `bash` command that reads files. A file the command reads for certain faces it like a read tool's file in every mode: one opened through `<`, one a wrapper option names, such as `xargs -a list`, and each literal file argument of a reader the gate recognizes (`cat`, `find`, `grep`, `head`, `ls`, `sed`, `tail`, and `wc`), including each file a glob there matches and the root of a tree the reader searches recursively, where a leading `~`, `$HOME`, or `${HOME}` names the home directory. A rule that matches a directory covers everything below it, so the built-in `ssh` rule denies `cat ~/.ssh/id_rsa`, `cat ~/.ss*/id_rsa`, and `grep -r key ~/.ssh` even under an allow rule for `cat` or `grep`. A `sed` script's `r` and `R` files are reads of this kind, its `w` and `W` files and the files `sed -i` rewrites are writes that `edit` rules weigh, and a script that may run a command, comes from a file, or does not parse is syntax the gate cannot analyze. A relative file argument or `<` target after a `cd` or `pushd` to a literal directory counts under that directory as well as the working directory, so `cd ~/.ssh && cat id_rsa` is denied too. A reader may also read files its arguments do not pin down: a file below a tree it searches recursively, or a file that an expanded argument, an `xargs` input, a `find -exec` path, a file list such as `wc --files0-from`, a glob `ox` cannot list, or the matches of a glob that can change which arguments are options, auxiliary files, or search roots may name. A literal file argument after such a glob stays a certain read, weighed as if the glob were one word, as a variable is, so `cat * ~/.ssh/id_rsa` is denied whatever `*` matches. Outside `yolo`, such a call asks when a deny or ask `read` pattern may match one of those files, or, for an argument that may name any file, while any such pattern exists, and the built-in `ssh` rule counts, so `cat $F` and `grep -r PRIVATE ~` ask by default. A recursive reader without a search root, such as `grep -r key`, reads the working directory for certain, as `grep -r key .` does. `yolo` runs such a call, as it runs syntax the gate cannot analyze. Recursive readers compare their search roots with each rule's leading path, while auxiliary files such as `grep -f patterns.txt` stay exact reads. The comparison does not follow symlinks below a search root ([#658](https://github.com/hakula139/oxide-code/issues/658)). A program the gate does not recognize as a reader, including one an `exec` allow rule names, can still open any file it may read.

A gap in these properties is a vulnerability:

- In `manual` mode, a `bash` command runs without approval only if the gate recognizes all of its syntax, every simple command in it, including those inside a substitution or a literal `sh -c` or `eval` script, is allowed by a rule or is a known-safe read, and every file it redirects output to is inside the working directory or an extra root, or allowed by an `edit` rule.
- A path `deny` rule, built-in or configured, and a configured path `ask` rule hold for the read, grep, glob, edit, and write tools in every mode, and a built-in path `ask` rule and a protected path hold for them in every mode except `yolo`, since the gate resolves the path and the tool opens the path it checked. The exception is a symlink swapped into a parent directory after the check, which `openat`-style traversal or an OS-level sandbox ([#133](https://github.com/hakula139/oxide-code/issues/133)) would close.

Edit and Write recheck the file before publishing, but concurrent writers can still modify the file during those checks or before publication, and the in-place write every platform uses does not promise crash atomicity. The [file edits design](docs/design/file-edits.md) defines metadata preservation, publication, and the precommit refusals.

To run untrusted work without watching it, run `ox` inside a sandbox or container, since `yolo` mode runs whatever the guardrails miss and every call that only a built-in `ask` rule or protected path holds.

In scope:

- Tool calls that bypass a check required by their permission mode, such as approval, a path `deny` rule or configured path `ask` rule on a file tool in any mode, or a built-in path `ask` rule or protected path on a file tool outside `yolo` mode
- A `bash` command that runs without approval in `manual` mode although it breaks the `manual` property above
- Approval prompts that misrepresent a call, or allow rules that match commands or paths excluded by their documented grammar
- Repository content, such as a project `.ox/config.toml`, that widens permissions, redirects credentials, extends the TLS trust store, or reads files outside the permission gate
- `ox` writing your API key or OAuth token to logs, the session store or its exports, or files accessible to other users, passing it to a `bash` command or a `git` or `gh` process, or sending it to any endpoint other than the configured endpoint or Anthropic's OAuth token endpoint
- Flaws in the release workflow, the Homebrew formula, the Nix flake, or the `cargo install --git` path that let someone other than the maintainer change what you install

Out of scope:

- Calls you approved as shown in the prompt, calls matched by your allow rules as documented, and tool calls in `yolo` mode that no path `deny` rule or configured path `ask` rule on a file tool covers
- A `bash` command that gets past an `exec`, `read`, or `edit` rule or a protected path without matching it, such as through a copied binary, an interpreter, a build script, a git alias, or shell syntax the gate misreads, when it does not also break the `manual` property above. These checks are a guardrail for `bash`, so report such a gap in a public issue.
- Secrets that an allowed read, grep, or glob call returns, which the session transcript then records
- Prompt injection when the resulting tool calls still pass through the permission gate as configured
- Attacks that require prior write access outside the repository, such as access to your user config, environment, or shell startup files
- Behavior of a gateway, proxy, or CA bundle you configured
- Dependency advisories for code that `ox` never reaches
- Gate gaps already tracked in a public issue
