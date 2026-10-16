# Tool Permissions and Approval

Every tool call passes through a tiered permission gate. Static rules settle the clear cases, and the user is asked only when no rule does. Planned extensions, including a classifier and an OS sandbox, are tracked in [#130](https://github.com/hakula139/oxide-code/issues/130).

Companion docs: [research/tools/permissions.md](../../research/tools/permissions.md), [slash/modals.md](../slash/modals.md), [session/file-tracking.md](../session/file-tracking.md).

## Modes

A mode sets the standing posture, shaped like the `Effort` enum (`ALL` / `as_str` / `Display` / `FromStr`) so it threads through config the same way `effort` does.

- **`auto`** (default): uses the tiered pipeline below.
- **`plan`**: performs read-only analysis. Read-class tools are allowed, while edit-class tools and `bash` calls are denied because shell commands cannot be statically proven side-effect-free.
- **`yolo`**: allows everything not blocked by a user or project deny rule. It skips all approval prompts and the shipped dangerous-pattern defaults, for use in trusted or externally sandboxed environments.

## Decision Pipeline

The gate evaluates each call in a fixed order and stops at the first match.

1. **User or project deny match** → deny in every mode.
2. **`yolo` mode** → allow.
3. **Shipped dangerous-pattern default match** → deny.
4. **Read-class call** (`read`, `glob`, `grep`) → allow.
5. **`plan` mode** → deny.
6. **Edit-class call on a protected path** → ask, regardless of what the allow rules say.
7. **Edit-class call inside the workspace** → allow.
8. **Allow or session allow match** → allow.
9. **Otherwise** → ask the frontend, as the [approval round-trip](#approval-round-trip) describes.

Every deny comes before every allow, so an allow rule cannot override an explicit deny. The shipped defaults form a separate deny set that holds in `auto` and `plan`.

## Path Targets

A path tool's target is resolved before any rules run. `GateTarget::for_path` joins the input to cwd and walks it one component at a time the way the kernel does. It follows every symlink it meets, a dangling one included, and steps `..` back from wherever the walk stands, so a `missing/..` detour cannot hide a link reached after it. Past the first missing component nothing exists yet, so the rest of the path applies as `create_dir_all` builds it. More than 40 symlink hops, or a component the walk cannot read, leave the target outside the workspace under its joined spelling, so the tool fails on it the same way the kernel would.

`check_permission` then pins the tool's `file_path` field to the resolved path before building the preview or running the tool, so the approval title shows the real destination and the tool opens what the gate checked. `read` and `edit` check the size and file type by path and again on the opened file, so a file swapped in between cannot skip the check. `read`, `edit`, and `write` also refuse a symlink swapped into the final component afterwards. Unix opens with `O_NOFOLLOW`, plus `O_NONBLOCK` so a swapped-in fifo cannot hang the open. Windows opens with `FILE_FLAG_OPEN_REPARSE_POINT` and fails when the handle is a symlink or junction. Any other reparse point, such as a deduplicated file, opens again through its file system filter, and the tool keeps that handle only when it reaches the same file. `write` creates a new file with `create_new`, which also refuses a dangling link, and truncates an existing one only after the open succeeds. On every platform, a symlink swapped into an intermediate directory between the gate and the open is not covered.

Each resolved target belongs to one zone:

- **Protected**: edit-class calls ask at step 6. A target is protected when the resolved path or the path as the call names it contains a protected name, or when it sits where a protected name directly inside cwd resolves, so `.vscode -> tools/vscode` protects `tools/vscode` as well. Names compare after dropping default-ignorable code points, Unicode compatibility case folding, and trimming trailing dots and spaces. A case-insensitive file system such as APFS opens `.VSCODE` and `.vſcode` as `.vscode`, HFS+ ignores the zero-width `\u{200c}` in `.g\u{200c}it`, and Windows opens `.git.` as `.git`. On Windows the existing part of the resolved path is spelled as the file system opens it, with each 8.3 short name in full and NTFS stream suffixes dropped, so `GIT~1` and `.git::$INDEX_ALLOCATION` count as `.git`. Each protected path runs or configures code outside the task at hand:
  - `.git`, `.githooks`, `.husky`, `.vscode`, `.idea`, `.devcontainer`, `.codex`, and `.ox` as any path segment.
  - The file names `.gitconfig`, `.gitmodules`, `.envrc`, `ox.toml`, `rust-toolchain`, `rust-toolchain.toml`, and the bash / zsh / POSIX shell startup files anywhere.
  - `.cargo/config`, `.cargo/config.toml`, `.claude/settings.json`, `.claude/settings.local.json`, and everything under `.claude/hooks`. The rest of `.claude`, including `worktrees` and `plans`, stays editable.
  - Everything under oxide-code's own config, data, and state directories.
- **Workspace**: the target sits inside cwd, so edit-class calls allow at step 7. A cwd of `/`, `$HOME`, or any ancestor of `$HOME` is never a workspace, so launching there leaves every edit to the allow rules. An existing regular file with more than one hard link is never a workspace target either, since another link can sit outside cwd.
- **Outside**: everything else, which needs an allow rule or an approval.

## Command Targets

A `bash` call's target is its command as tree-sitter-bash parses it. `GateTarget::for_command` splits the command into simple commands across `;`, `&&`, `||`, `&`, pipes, line breaks, subshells, and compound statements. Each simple command drops the transparent wrappers `builtin`, `command`, `env`, `exec`, `nice`, `nohup`, `time`, and `timeout` with their options, so `nice -n 5 cargo test 2>&1` runs the simple command `cargo test`. Its words are unquoted, and a leading unquoted `~` or `~/` expands to the home directory. Each output redirection to a file (`>`, `>>`, `>|`, `>&`, `&>`, `&>>`) becomes an edit-class path target resolved and zoned like a path tool's, except for `/dev/null` and the other standard streams.

A leading assignment, either before the command or as an `env` argument, drops out of the text a rule matches only when it sets one of a short list of variables that change output and nothing else, such as `RUST_LOG`, `LANG`, `TZ`, or `NO_COLOR`. Any other variable can change which program runs or what it loads, as `PATH`, `LD_PRELOAD`, and `GIT_PAGER` do, so the text keeps it. `GIT_PAGER=sh git log` then needs a rule that names the assignment, and it is never a known-safe read. Deny rules still see the program that runs.

A command is opaque when the parse cannot show everything it runs or writes:

- It fails to parse, or it holds a command or process substitution, an arithmetic expansion, or a `[[` or `((` test, which can evaluate a substitution inside an array subscript.
- A command name is not a literal word, as in `$CMD -rf x`.
- It runs a shell such as `sh`, `bash`, or `zsh`, or a builtin that runs code (`eval`, `source`, `.`, `trap`, `let`), so both `sh -c '...'` and `curl ... | sh` are opaque.
- A wrapper takes an option the parser does not know, such as `env -S`.
- A redirection target is not a literal path, holds a glob, or names more than one file.
- It assigns a variable the shell itself reads, such as `PATH`, `HOME`, `IFS`, or `BASH_ENV`, by a plain assignment or a `for` loop, or it declares or exports a variable outside the short list above, since later commands see it.
- A `cd`, `pushd`, `popd`, or `env -C` runs alongside a redirection or a known-safe read of a relative path, which may then name another file.

The literal script of `sh -c` and the joined arguments of `eval` are parsed as well, so a deny rule still sees the simple commands inside them.

A few read-only commands run without an allow rule. `echo`, `false`, `pwd`, and `true` are always safe. `cat`, `head`, `ls`, `tail`, `wc`, `find`, `grep`, and `sed` are safe when every argument is a literal word without globs and the command runs by its bare name, with guards for the ones that can do more:

- `find` without an action that runs a command, deletes, or writes a file (`-exec`, `-execdir`, `-ok`, `-okdir`, `-delete`, `-fls`, `-fprint`, `-fprint0`, `-fprintf`).
- `grep` without a recursive option or `-d`.
- `sed` without `-i`, `-I`, or a script file, and with scripts that only print, delete, substitute, or move text between buffers.

Long options count by any prefix GNU tools accept for them, so `sed --in` is refused as `--in-place`.

Every argument that may name a file, such as each word past the options and a value attached to an option, counts as a read of that path. A read that a `read` deny rule matches makes the command ask, since the arguments over-approximate the files it reads.

## Rule Grammar

Rules use Claude Code's `name(specifier)` string form, where the name identifies a permission class: `read` (the `read`, `grep`, and `glob` tools), `edit` (`edit` and `write`), or `bash`. Class names are case-insensitive. An unknown name causes a load error, so `write(...)` fails loudly. `bash`, `bash()`, and `bash(*)` all become a class-wide rule.

Bash specifiers can be exact, prefix (`cargo test:*`), or wildcard (`git *`). Read and edit specifiers are path globs:

- `*` and `?` match within one path segment, while `**` spans multiple segments. `[...]` and `{a,b}` work as in `globset`.
- A glob without a leading `/` or `~/` matches a cwd-relative path and applies only inside the working directory. `*.json` matches `package.json` but not `src/a.json`, while `**/.env` matches at any depth.
- A glob starting with `/` matches the resolved absolute path. A glob starting with `~/` expands to the home directory first.
- On Windows, `\` separates names like `/`, an absolute glob needs a drive (`C:/...`), and a rooted glob without one fails to load.
- Deny rules also match the path as the call spells it, lexically normalized, so `.env -> ../shared/.env` cannot route a read around `read(**/.env)`. Allow rules match only the resolved path.
- When the policy loads, a deny rule whose literal prefix, the components before the first glob character, runs through a symlink also matches where that prefix resolves, with relative rules resolved against the launch directory. With `secrets -> ../vault`, `read(secrets/**)` also covers `../vault/key`.
- Allow rules match names exactly as written. Deny rules also compare names the way protected paths do, so `read(**/.env)` covers `.ENV`, `.env.`, and a path whose cwd prefix differs only in case.
- A leading `./` is dropped.

A `bash` specifier matches one simple command of the [command target](#command-targets). An allow rule matches a simple command as written after its assignments and wrappers are stripped, so `bash(cargo test:*)` covers `RUST_LOG=debug cargo test 2>&1`. A deny rule matches the whole command or any simple command, compared as written, unquoted, and with its command name reduced to the bare name, so `bash(rm -rf:*)` also stops `"rm" -rf x`, `/bin/rm -rf x`, and `cd src && rm -rf x`.

A command is allowed at step 8 only when it is not opaque, each of its simple commands is allowed by a rule or is a known-safe read, and each file it writes sits in the workspace or is allowed by an `edit` rule. A write is also checked against the `edit` deny rules at steps 1 and 3 and asks at step 6 when its target is protected. An opaque command can still be denied, so `bash(rm -rf:*)` stops `rm -rf x; echo $(date)`, but no rule allows it.

## Approval Round-Trip

When the gate resolves to ask, the decision travels through the existing `ops` channel polled by the turn loop. Tool dispatch is sequential, so at most one approval can be outstanding and no id fan-out is needed.

`run_tool_round` passes the tool-use `id` and `sink` to `dispatch_tool_call`. The latter emits `Event::ApprovalRequested { id, preview, grant }`, carrying the id, the session grant described below, and a small `Clone` preview: an edit diff from `edit::synthesize_chunk`, a diff against the file's current content for `write`, or the command string under the tool name for `bash`. The gate intercepts the call before `tools.run`, the same way the parse-error short-circuit returns a synthetic `ToolOutput`. It waits for a decision in a sibling of `await_unless_aborted`, whose select loop maps `Op::Interrupt` to `Cancelled` and `Op::Shutdown` to `Shutdown`, buffers an `Op::Queue` in `pending`, and matches `Op::Resolve { id, decision }`. Decisions with an id that does not match the outstanding call are ignored, and dropping the wait future is cancel-safe.

Every frontend answers with one of the `Decision` variants:

- **`Approve`** runs the call.
- **`AllowForSession`** runs it and adds a session rule, described below.
- **`Deny`** returns a synthetic error `ToolOutput` saying the policy denied the call, so the model receives the refusal as a tool result and can choose another approach.
- **`DenyWithFeedback`** returns the user's text in that error result, so the model learns why and what to do next.
- **`Abort`** ends the turn as `Op::Interrupt` does, and the call's result records that it never ran.

On the TUI side, an `ApprovalModal` joins the `ModalStack` and follows the `ConfirmDeleteSessionModal` template. The blocked agent must receive a decision on every dismissal path, but `ModalStack::handle_key` intercepts Esc and Ctrl+C before delegation, while `clear` drops modals outright. Both paths yield `ModalAction::Cancelled`. The stack therefore has a cancel hook (`Modal::on_cancel` returning an optional `ModalAction`) that resolves a pending approval to `Decision::Abort` on universal cancel or session-swap `clear`, matching what Esc does outside the modal.

`Y` approves, `N` denies, and Esc aborts. `F` opens a one-line feedback editor, where Enter denies with the typed text, an empty answer submits nothing, and Esc returns to the choice. Enter does nothing at the choice. The modal answers key presses only, so a key release or repeat cannot approve. Apart from the stack-level Esc / Ctrl+C cancel, it ignores every key until 300 ms pass without one, and each key inside that window restarts it, so neither a keystroke typed for the prompt underneath nor a pasted burst can approve the call. Opening an editor restarts the window, so an Enter typed right behind `A` or `F` cannot submit it. `Y` also waits until a render has shown the first body row, so a modal clipped to a short terminal cannot approve a call the user never saw.

`A` allows the call and every later call a session rule covers:

- **Grant:** when the gate asks, `Policy::session_grant` derives the narrowest rule that names the call with `Rule::for_call`. That is the command as written for `bash`, or the escaped path for an edit, relative inside the launch directory and canonical outside it. The grant rides on `Event::ApprovalRequested` and is offered only if adding its rule would let the call through, so a protected path, an opaque command, or one that runs more than one simple command offers none. For a single simple command, the rule names it without its assignments, wrappers, and redirections, so `RUST_LOG=debug cargo test 2>&1` offers `bash(cargo test)`. A command containing `*`, which would parse as a wildcard, and a call with a character the editor cannot show as itself offer none either, so the user always sees the exact rule they grant.
- **Editor:** `A` opens a one-line editor prefilled with the rule, where the user can widen or narrow it, such as `bash(cargo test:*)`. Enter checks the edit with `SessionGrant::accept`, which keeps the class and requires the rule to still cover the call, and shows the reason inline when it does not. Esc returns to the choice through `Modal::captures_esc`, while Ctrl+C still aborts.
- **Engine check:** the accepted rule answers `Decision::AllowForSession`. A frontend's check is advisory, so the gate keeps its own grant for the pending call, admits the answered rule against it with `SessionGrant::admit`, and denies the call with an error when the rule fails.
- **Precedence and lifetime:** an admitted rule joins the engine's policy, where step 8 weighs it like a configured allow rule, so deny rules and protected paths still take precedence. The list lives only in the engine's memory and empties on `/clear`, `/archive`, and a successful `/resume`. `Event::SessionRulesUpdated` carries it to the frontend, and `/status` lists it under Session Rules.

The preview must show everything it approves. Every row wraps. If the body exceeds 16 rows, it scrolls with Up / Down, PageUp / PageDown, and Home / End, with a `rows a-b of n` position line. Control and zero-width characters render as `\u{..}` escapes. In a command, each whitespace character beyond the indentation renders as `·` unless it is a single space, so padding that pushes a chained command off-screen appears as a dotted run.

## Configuration

```toml
[permission]
mode = "auto" # auto | plan | yolo
allow = ["bash(cargo test:*)", "edit(src/**)"]
deny = ["bash(rm -rf:*)", "read(**/.env)"]
```

`OX_PERMISSION_MODE` overrides the mode, following `effort`'s behavior: an empty environment value falls through, while a parse error propagates so a typo fails loudly. The `[permission]` table parses into a `PermissionTable` on `FileConfig` with `deny_unknown_fields`, concatenates its rule lists across config layers, and resolves in `Config::load` after the compaction block, which is also where each deny rule's literal prefix resolves against the launch directory.

The shipped defaults cover catastrophic commands (`rm -rf`, disk writes, `mkfs`, fork bombs, `curl | sh`) and `edit(.git/**)`, preventing step 7's workspace allow from touching the project's own git metadata.

A checked-in `.ox/config.toml` is untrusted, like the project credentials `reject_project_secrets` blocks, so a project file may set only `deny`. Setting `mode` or `allow` there is rejected with a message pointing to user config. The merge appends project `deny` onto the user deny set, so a repo can restrict itself but never widen what the user allowed.

## Stdio Frontends

The `--no-tui` REPL and `-p` headless runs share `render_op`, which hands each `Event::ApprovalRequested` to an `Approver` and sends its decision back as `Op::Resolve`. The request itself never reaches the event sink, so the `stream-json` output carries only the call's result.

- **REPL:** when stdin is a terminal, the prompter writes the preview to stderr, escaping hidden characters, and reads the answer from the next stdin line: `y`, `a` with the rule to allow, `n`, `f` with the feedback line, or `x` to abort. An unknown answer, empty feedback, or a rule the grant does not admit returns to the choices, and the end of input aborts. Piped input answers as a headless run does, so a scripted prompt line is never read as an answer.
- **Headless:** with no one to ask, the deny rules, zones, and allow rules settle every call, so a headless run assumes an already-trusted invocation. `Unattended` denies a would-ask call with feedback that names the rule to add to the `allow` list, quoted as a TOML string from the call's grant. A call that offers no grant, such as a protected path, a command that runs more than one simple command, or a command containing `*`, is denied with feedback saying that no allow rule names it exactly, since a broader rule may still cover it.

The stdio sink escapes control characters other than tabs and line breaks in model and tool text, so streamed text or a tool label cannot redraw the terminal to disguise the REPL's preview.

## Permission Classes

`Tool::permission_class` has no default, so every tool declares its class and a new tool cannot inherit a permissive class. Read-class tools are allowed at step 4, but they still extract a gate target (the file for `read`, the search root for `grep` / `glob`), allowing a path-scoped deny at step 1 or 3 to fire first. Rules target the class, so `read(**/.env)` also stops a `grep` rooted at a `.env` file. Because `grep` and `glob` gate on the search root, each also checks every file its walk finds against the `read` deny rules and withholds the matches, ending its output with a count of withheld files that names none. A file is matched where it resolves and as the call spells it below the search root, as the gate matches a read of it, so `read(**/.env)` also withholds `.env` under a root that is a symlink out of the workspace. The walk follows no symlink below the canonical root, so the check needs no I/O.

`edit` and `write` share a class but have different blast radii. `edit` requires the file to exist and to have been read, so it cannot create files. `write` can create new files and parent directories without a prior Read, while overwriting an existing file still goes through the tracker gate. Both declare `file_path` as their path field, so the resolution, zoning, and pinning described above treat them identically.

## Design Decisions

1. **Default `auto`.** Running tools unchecked is the greater hazard. `yolo` keeps unprompted execution available as an explicit opt-in.

2. **`yolo` keeps the user's denies.** A deny written by the user or project expresses intent that no mode should override, so it runs before the `yolo` short-circuit. The shipped defaults are generic guesses that can block legitimate work, so the opt-in skips them.

3. **Protected paths ask.** Editing `.git/hooks`, `.vscode/tasks.json`, or `.envrc` inside the workspace can escalate to code execution outside the task. These paths therefore require approval even under a broad `edit(**)` allow. Asking keeps legitimate edits possible, while a deny would force the user to edit config.

4. **Resolve once, act on the result.** Resolving in the gate and pinning the input makes the tool open the path the gate checked, and the no-follow open turns a symlink swapped into its final component into a failed open. A swap in an intermediate directory needs `openat`-style traversal or an OS sandbox ([#133](https://github.com/hakula139/oxide-code/issues/133)).

5. **Project files tighten only.** Honoring a project's `allow` or `mode` would let a teammate's repo broaden what the local user permitted, creating the same privilege-escalation vector as project-set credentials. Append-only `deny` lets a repo restrict itself without that risk.

6. **Deny rules are the enforced boundary for path tools.** The deny list matches both spellings of the path as the file system stands when the gate checks, each rule's literal prefix as it resolved when the policy loaded, and folded names. It does not cover a symlink swapped into an intermediate directory after the check or whatever a `bash` command touches, so an untrusted model still needs an external OS sandbox.

7. **Decision on the existing channel.** Routing approvals through `ops` and the `ModalStack` reuses the cancel / quit / queue semantics that the turn loop already races, so no second channel or control-flow path is needed.

8. **Approvals stay in process.** A persisted "allow bash X" has no disk ground truth to revalidate on resume, unlike a `FileSnapshot` rehash, so restoring it would regress trust. Per the slash-command persistence stance ([decision 6](../slash/commands.md#design-decisions)), session commands remain reversible, and cross-session writes wait for an explicit confirmed action.

## Sources

- `crates/oxide-code/src/config.rs`: `Config::load`, `OX_PERMISSION_MODE`, `Effort` enum template.
- `crates/oxide-code/src/config/load.rs`: `reject_project_secrets`, `reject_project_permissions`.
- `crates/oxide-code/src/config/merge.rs`: `APPENDED` rule lists, `merge`.
- `crates/oxide-code/src/driver/repl.rs`: `Prompter`, the stdin approval prompt.
- `crates/oxide-code/src/driver/stdio.rs`: `render_op`, the `Approver` trait, `Unattended`, the escaped `StdioSink` output.
- `crates/oxide-code/src/engine.rs`: the live policy, whose session allow rules reset when the session changes.
- `crates/oxide-code/src/engine/abort.rs`: `await_unless_aborted`, which races an allowed call against cancel and quit.
- `crates/oxide-code/src/engine/dispatch.rs`: `check_permission` (resolve, pin, decide), `allow_for_session` (admit against the call's grant), `await_approval`, `dispatch_tool_call`, `run_tool_round`.
- `crates/oxide-code/src/permission.rs`: `Mode`, `PermissionClass`, `GateTarget::for_path`, `GateTarget::for_command`, protected paths, `Policy::decide`, `Policy::session_grant`, `SessionGrant`, `ReadDeny`.
- `crates/oxide-code/src/permission/rule.rs`: `class(specifier)` parsing, path-glob matching, segment matching for `bash`, and `Rule::for_call`.
- `crates/oxide-code/src/permission/shell.rs`: `CommandTarget::parse`, wrapper stripping, opaque forms, redirection writes, and the known-safe commands.
- `crates/oxide-code/src/protocol.rs`: `Event::ApprovalRequested`, `Event::SessionRulesUpdated`, `Op::Resolve`, `Decision`.
- `crates/oxide-code/src/slash/confirm.rs`: `ConfirmDeleteSessionModal`, the approval-modal template.
- `crates/oxide-code/src/slash/status.rs`: the Session Rules listing.
- `crates/oxide-code/src/tool.rs`: `Tool` trait (`permission_class`, `path_field`, `gate_target`, `pin_target`, `approval_preview`), `ApprovalPreview`, `read_validated`, `write_no_follow`, `open_no_follow`, `push_withheld_note`.
- `crates/oxide-code/src/tool/bash.rs`: unbounded execution surface.
- `crates/oxide-code/src/tool/edit.rs`: read-before-edit gate, `synthesize_chunk` preview.
- `crates/oxide-code/src/tool/glob.rs`: read-deny filter on each match.
- `crates/oxide-code/src/tool/grep.rs`: read-deny filter in `collect_files`.
- `crates/oxide-code/src/tool/write.rs`: new-file creation, symlink refusal, current-content preview.
- `crates/oxide-code/src/tui/modal.rs`: `ModalStack`, the cancel hook, `captures_esc`.
- `crates/oxide-code/src/tui/modal/approval.rs`: `ApprovalModal` wrapping, scrolling, character reveal, and the rule and feedback editors.
- `crates/oxide-code/src/util/text.rs`: `is_hidden` and `escape_controls`, shared by both frontends.
