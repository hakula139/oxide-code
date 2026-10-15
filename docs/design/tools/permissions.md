# Tool Permissions and Approval

Every tool call passes through a tiered permission gate. Static rules settle the clear cases, and the user is asked only when no rule does. Planned extensions, including a classifier, session approvals, and an OS sandbox, are tracked in [#130](https://github.com/hakula139/oxide-code/issues/130).

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
8. **Allow match** → allow.
9. **Otherwise** → ask in interactive mode or deny in headless mode.

Every deny comes before every allow, so an allow rule cannot override an explicit deny. The shipped defaults form a separate deny set that holds in `auto` and `plan`.

## Path Targets

A path tool's target is resolved before any rules run. `GateTarget::for_path` joins the input to cwd and walks it one component at a time the way the kernel does. It follows every symlink it meets, a dangling one included, and steps `..` back from wherever the walk stands, so a `missing/..` detour cannot hide a link reached after it. Past the first missing component nothing exists yet, so the rest of the path applies as `create_dir_all` builds it. More than 40 symlink hops, or a component the walk cannot read, leave the target outside the workspace under its joined spelling, so the tool fails on it the same way the kernel would.

`check_permission` then pins the tool's `file_path` field to the resolved path before building the preview or running the tool, so the approval title shows the real destination and the tool opens what the gate checked. `read` and `edit` check the size and file type by path and again on the opened file, so a file swapped in between cannot skip the check. On Unix, `read`, `edit`, and `write` also open with `O_NOFOLLOW`, so a symlink swapped into the final component afterwards fails the open, `read` and `edit` open with `O_NONBLOCK` so a swapped-in fifo cannot hang them, and `write` creates a new file with `create_new`, which also refuses a dangling link. Windows has no `O_NOFOLLOW`, so a final-component swap there is not covered. On every platform, a symlink swapped into an intermediate directory between the gate and the open is not covered.

Each resolved target belongs to one zone:

- **Protected**: edit-class calls ask at step 6. A target is protected when the resolved path or the path as the call names it contains a protected name, or when it sits where a protected name directly inside cwd resolves, so `.vscode -> tools/vscode` protects `tools/vscode` as well. Names compare after dropping default-ignorable code points, Unicode compatibility case folding, and trimming trailing dots and spaces. A case-insensitive file system such as APFS opens `.VSCODE` and `.vſcode` as `.vscode`, HFS+ ignores the zero-width `\u{200c}` in `.g\u{200c}it`, and Windows opens `.git.` as `.git`. Windows 8.3 short names such as `GIT~1` are not recognized. Each protected path runs or configures code outside the task at hand:
  - `.git`, `.githooks`, `.husky`, `.vscode`, `.idea`, `.devcontainer`, and `.codex` as any path segment.
  - The file names `.gitconfig`, `.gitmodules`, `.envrc`, `ox.toml`, `rust-toolchain`, `rust-toolchain.toml`, and the bash / zsh / POSIX shell startup files anywhere.
  - `.cargo/config`, `.cargo/config.toml`, `.claude/settings.json`, `.claude/settings.local.json`, and everything under `.claude/hooks`. The rest of `.claude`, including `worktrees` and `plans`, stays editable.
  - Everything under oxide-code's own config, data, and state directories.
- **Workspace**: the target sits inside cwd, so edit-class calls allow at step 7. A cwd of `/`, `$HOME`, or any ancestor of `$HOME` is never a workspace, so launching there leaves every edit to the allow rules. On Unix, an existing regular file with more than one hard link is never a workspace target either, since another link can sit outside cwd. Rust's standard library exposes no hard-link count on Windows, so a hard-linked file there still counts as a workspace target.
- **Outside**: everything else, which needs an allow rule or an approval.

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

The `bash` command string is unparsed (`bash -c "..."`), so prefix and wildcard matching provides best-effort UX. Since `ls; rm -rf` and `$()` indirection defeat naive matching, allow rules match conservatively: a compound command (chained, substituted, or redirected with `>` / `<`) never matches an allow rule. Deny rules match the whole command or any chained segment.

## Approval Round-Trip

When the gate resolves to ask, the decision travels through the existing `user_rx` channel polled by the turn loop. Tool dispatch is sequential, so at most one approval can be outstanding and no id fan-out is needed.

`run_tool_round` passes the tool-use `id` and `sink` to `dispatch_tool_call`. The latter emits `AgentEvent::ApprovalRequested { id, preview }`, carrying the id and a small `Clone` preview: an edit diff from `edit::synthesize_chunk`, a diff against the file's current content for `write`, or the command string under the tool name for `bash`. The gate intercepts the call before `tools.run`, the same way the parse-error short-circuit returns a synthetic `ToolOutput`. It waits for a decision in a sibling of `await_unless_aborted`, whose select loop maps `Cancel` to `Cancelled` and `Quit` to `Quit`, buffers a `QueuePrompt` in `pending`, and matches `UserAction::ApprovalDecision { id, decision }`. Decisions with an id that does not match the outstanding call are ignored, and dropping the wait future is cancel-safe.

On the TUI side, an `ApprovalModal` joins the `ModalStack` and follows the `ConfirmDeleteSessionModal` template. The blocked agent must receive a decision on every dismissal path, but `ModalStack::handle_key` intercepts Esc and Ctrl+C before delegation, while `clear` drops modals outright. Both paths yield `ModalAction::Cancelled`. The stack therefore has a cancel hook (`Modal::on_cancel` returning an optional `ModalAction`) that resolves a pending approval to `ApprovalDecision::Deny` on universal cancel or session-swap `clear`. A denied call returns a synthetic error `ToolOutput`, so the model receives the refusal as a tool result and can choose another approach.

`Y` approves, and `N` or Esc denies. Enter does nothing. The modal answers key presses only, so a key release or repeat cannot approve. Apart from the stack-level Esc / Ctrl+C cancel, it ignores every key until 300 ms pass without one, and each key inside that window restarts it, so neither a keystroke typed for the prompt underneath nor a pasted burst can approve the call. `Y` also waits until a render has shown the first body row, so a modal clipped to a short terminal cannot approve a call the user never saw.

The preview must show everything it approves. Every row wraps. If the body exceeds 16 rows, it scrolls with Up / Down, PageUp / PageDown, and Home / End, with a `rows a-b of n` position line. Control and zero-width characters render as `\u{..}` escapes. In a command, each whitespace character beyond the indentation renders as `·` unless it is a single space, so padding that pushes a chained command off-screen appears as a dotted run.

## Configuration

```toml
[permission]
mode = "auto" # auto | plan | yolo
allow = ["bash(cargo test:*)", "edit(src/**)"]
deny = ["bash(rm -rf:*)", "read(**/.env)"]
```

`OX_PERMISSION_MODE` overrides the mode, following `effort`'s behavior: an empty environment value falls through, while a parse error propagates so a typo fails loudly. The `[permission]` table parses into a `PermissionFileConfig` on `FileConfig` with `deny_unknown_fields`, merges through `merge_section`, and resolves in `Config::load` after the compaction block, which is also where each deny rule's literal prefix resolves against the launch directory.

The shipped defaults cover catastrophic commands (`rm -rf`, disk writes, `mkfs`, fork bombs, `curl | sh`) and `edit(.git/**)`, preventing step 7's workspace allow from touching the project's own git metadata.

A checked-in `ox.toml` is untrusted, like the project credentials `reject_project_secrets` blocks, so a project file may set only `deny`. Setting `mode` or `allow` there is rejected with a message pointing to user config. The merge appends project `deny` onto the user deny set, so a repo can restrict itself but never widen what the user allowed.

## Headless Behavior

In `-p` / `--no-tui` runs there is no human to prompt, so a would-ask call denies directly: the gate carries an `interactive` flag the modal-less surfaces set to false, and `check_permission` turns an `Ask` verdict into a synthetic denial. With no human fallback, the deny rules, zones, and allow rules settle every call, so a headless run assumes an already-trusted invocation. The model sees the denial and can retry.

## Permission Classes

`Tool::permission_class` has no default, so every tool declares its class and a new tool cannot inherit a permissive class. Read-class tools are allowed at step 4, but they still extract a gate target (the file for `read`, the search root for `grep` / `glob`), allowing a path-scoped deny at step 1 or 3 to fire first. Rules target the class, so `read(**/.env)` also stops a `grep` rooted at a `.env` file. Because `grep` and `glob` gate on the search root, a deny on a file inside that root does not filter their matches ([#176](https://github.com/hakula139/oxide-code/issues/176)).

`edit` and `write` share a class but have different blast radii. `edit` requires the file to exist and to have been read, so it cannot create files. `write` can create new files and parent directories without a prior Read, while overwriting an existing file still goes through the tracker gate. Both declare `file_path` as their path field, so the resolution, zoning, and pinning described above treat them identically.

## Design Decisions

1. **Default `auto`.** Running tools unchecked is the greater hazard. `yolo` keeps unprompted execution available as an explicit opt-in.

2. **`yolo` keeps the user's denies.** A deny written by the user or project expresses intent that no mode should override, so it runs before the `yolo` short-circuit. The shipped defaults are generic guesses that can block legitimate work, so the opt-in skips them.

3. **Protected paths ask.** Editing `.git/hooks`, `.vscode/tasks.json`, or `.envrc` inside the workspace can escalate to code execution outside the task. These paths therefore require approval even under a broad `edit(**)` allow. Asking keeps legitimate edits possible, while a deny would force the user to edit config.

4. **Resolve once, act on the result.** Resolving in the gate and pinning the input makes the tool open the path the gate checked, and on Unix `O_NOFOLLOW` turns a symlink swapped into its final component into a failed open. A swap in an intermediate directory needs `openat`-style traversal or an OS sandbox ([#133](https://github.com/hakula139/oxide-code/issues/133)).

5. **Project files tighten only.** Honoring a project's `allow` or `mode` would let a teammate's repo broaden what the local user permitted, creating the same privilege-escalation vector as project-set credentials. Append-only `deny` lets a repo restrict itself without that risk.

6. **Deny rules are the enforced boundary for path tools.** The deny list matches both spellings of the path as the file system stands when the gate checks, each rule's literal prefix as it resolved when the policy loaded, and folded names. It does not cover a symlink swapped into an intermediate directory after the check, a Windows 8.3 short name, or whatever a `bash` command touches, so an untrusted model still needs an external OS sandbox.

7. **Decision on the existing channel.** Routing approvals through `user_rx` and the `ModalStack` reuses the cancel / quit / queue semantics that the turn loop already races, so no second channel or control-flow path is needed.

8. **Approvals stay in process.** A persisted "allow bash X" has no disk ground truth to revalidate on resume, unlike a `FileSnapshot` rehash, so restoring it would regress trust. Per the slash-command persistence stance ([decision 6](../slash/commands.md#design-decisions)), session commands remain reversible, and cross-session writes wait for an explicit confirmed action.

## Sources

- `crates/oxide-code/src/agent.rs`: `check_permission` (resolve, pin, decide), `dispatch_tool_call`, `await_unless_aborted`, `run_tool_round`.
- `crates/oxide-code/src/agent/event.rs`: `AgentEvent`, `UserAction`, `ApprovalPreview`.
- `crates/oxide-code/src/config.rs`: `Config::load`, `OX_PERMISSION_MODE`, `Effort` enum template.
- `crates/oxide-code/src/config/file.rs`: `FileConfig::merge`, `merge_section`, `reject_project_secrets`, `reject_project_permissions`.
- `crates/oxide-code/src/permission.rs`: `Mode`, `PermissionClass`, `GateTarget::for_path`, protected paths, `Policy::decide`.
- `crates/oxide-code/src/permission/rule.rs`: `class(specifier)` parsing and path-glob matching.
- `crates/oxide-code/src/slash/confirm.rs`: `ConfirmDeleteSessionModal`, the approval-modal template.
- `crates/oxide-code/src/tool.rs`: `Tool` trait (`permission_class`, `path_field`, `gate_target`, `pin_target`, `approval_preview`), `read_validated`, `write_no_follow`.
- `crates/oxide-code/src/tool/bash.rs`: unbounded execution surface.
- `crates/oxide-code/src/tool/edit.rs`: read-before-edit gate, `synthesize_chunk` preview.
- `crates/oxide-code/src/tool/write.rs`: new-file creation, symlink refusal, current-content preview.
- `crates/oxide-code/src/tui/modal.rs`: `ModalStack`, the cancel hook.
- `crates/oxide-code/src/tui/modal/approval.rs`: `ApprovalModal` wrapping, scrolling, and character reveal.
