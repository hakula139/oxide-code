# Tool Permissions and Approval

Every tool call passes through a permission gate with three rule tiers (deny, ask, allow) and a mode deciding unmatched calls (`plan` also denies every `edit`, `exec`, and `net` call). [Permissions](../guide/permissions.md) covers user-facing modes, syntax, and settings, [Modals](modals.md) and [File Edits](file-edits.md) cover the approval overlay and file-integrity checks, and this page covers gate evaluation.

## Rules and Classes

`[permission]` holds three tier tables, each mapping permission classes to pattern lists. Every tool must declare its class in `Tool::spec` (no default, preventing accidental permissive defaults). `net` and `mcp` parse and validate now but match no calls yet (MCP tools join `mcp` in [#178](https://github.com/hakulab/oxide-code/issues/178)). Patterns compile at config load, failing startup with the offending file on syntax errors.

`OX_PERMISSION_MODE` overrides `permission.mode` (empty treated as absent, typos fail like `effort`), and `--mode` overrides both (attributed to the flag in `/config`). Config layers append `permission.<tier>.<class>` lists and `extra_roots`, recording source layers for `/config`. `reject_project_keys` restricts `[permission]` in project `.ox/config.toml` to `deny` and `ask` only (**Project files tighten only**).

## Decision Pipeline

`Tool::prepare` validates and resolves inputs before gating so invalid calls end without classification or prompts. Input validation, Read-before-Edit, and checked file publication remain tool contracts.

`Policy::verdict` is synchronous and returns at the first matching step:

1. Configured deny rule → deny.
2. `plan` mode on `edit`, `exec`, or `net` → deny.
3. Configured ask rule → ask.
4. `read`-class call with no external target (session-local state only) → allow.
5. Configured allow rule or session grant → allow.
6. Mode fallback: `auto` → `Classify`, `manual` → ask, `yolo` → allow, `plan` → allow (remaining reads).

A command-specific allow rule must cover every extracted simple command. Native path rules apply to file tools and search walks, while Bash uses exec rules and the active mode without inferring filesystem effects. Every `Verdict` carries a `Basis` (rule, session grant, mode, session-local capability, or classifier reason). Explicit ask rules bypass classification, and unattended calls needing approval resolve directly to `Unapproved`.

## Automatic Classification

`engine::classifier` runs a side query via `Client::complete`, reusing endpoint, credentials, JSON output support, and cancellation. `permission.auto.model` defaults to Haiku and `permission.auto.instructions` appends user policy (neither allowed in project config).

The prompt sends only user-authored transcript text, the validated tool name and full untruncated input, any resolved native path, working directory, and workspace roots. A logical user turn retains its original prompts across compaction plus queued user prompts as they arrive (a resumed compacted session has only visible user text and new follow-ups). Generated text, assistant explanations, and tool output stay excluded, and the classifier reads no files and calls no tools.

The classifier returns `allow` or `ask` with a short reason. `allow` emits a `Notice`, while `ask` triggers the normal approval flow with `Basis::Classifier`. Errors, invalid JSON, empty reasons, refusals, or non-final stops fail closed to approval (gateways omitting the stop field still require valid decision JSON). Usage is recorded before decision parsing (including malformed replies and refusals) under the classifier model, adding to session cost without altering displayed token usage or context estimates. There is no verdict cache, retry, background task, or static-rule fallback.

`Mailbox::race` handles interruption and shutdown. If a mode switch arrives mid-request, dispatch applies it and reevaluates the policy before acting on the classifier result.

## Mode Switches

`Op::SetMode` updates the engine's policy (sent by the TUI from `/mode`, Tab, and Shift+Tab, while headless runs pass `--mode` via `Config`) and confirms immediately with `Event::ModeChanged` for the TUI snapshot and status line.

Idle switches apply immediately. During a turn, compaction, or sign-in, switches queue in the operation's `Mailbox` and apply via `Mailbox::apply_mode` before the next gated call, round boundary, or prompt / compaction user-context render ([Prompt](prompt.md#user-context)), with `Engine::serve` applying any remaining switch when the operation ends (**Mode switches wait for the next gate, round boundary, or context render**).

Tab and Shift+Tab cycle forward and backward with wrap through `auto` → `manual` → `plan` → `yolo` (**The keyboard cycle reaches `plan` before `yolo` and wraps at both ends**), stepping from the TUI's last requested mode so rapid presses advance before confirmation. The `/mode` picker, argument completion, `--mode --help`, and error messages use the same order, and keys stay fixed until configurable bindings land ([#278](https://github.com/hakulab/oxide-code/issues/278)).

## Path Targets

Path tools resolve targets in `Tool::prepare` via `ToolCtx::path_access` against `Workspace`, expanding leading `~`, joining to the working directory, and walking components via `Access::for_path` as the kernel does:

- Follows every symlink (including dangling ones) and resolves `..` at each step so `missing/..` cannot hide a symlink.
- Applies remaining components past the first missing entry as `create_dir_all` would, falling back to the joined spelling if the walk cannot finish.
- Keeps the exact non-UTF-8 `PathBuf` alongside the lossy string used for rule matching.
- On Windows, expands existing 8.3 short names and strips NTFS stream suffixes (`GIT~1`, `.git::$INDEX_ALLOCATION` → `.git`).

Approvals and tool messages display the resolved path (`edit` / `write` in the title, `read` in the body), and file tools open `Access::resolved()` with no-follow flags and check size and type on the opened descriptor (**Native path tools resolve once and act on the result**). `grep` and `glob` preserve the written path for deny matching while walking from the resolved root. Workspace roots anchor relative rules and inform classification without auto-allowing edits.

## Command Targets

`bash` commands parse via tree-sitter-bash into simple-command segments retaining leading assignments and the program word index, treating wrappers and literal script arguments as ordinary words without path resolution or effect inference. Quote removal and literal tracking tie patterns to words. Dynamic words, redirections, and unsupported constructs block command-specific allow rules (while whole-class `*` still applies after deny and ask checks), falling through to the mode while deny and ask still inspect extracted simple commands ([Rules](../guide/permissions.md#rules)).

## Patterns

`MatchDiscipline` separates conservative allow matching from broad deny / ask matching (ask uses the deny discipline, grammar in [Rules](../guide/permissions.md#rules)):

- **`exec` allow.** Matches simple-command words exactly (`*` before the last word matches one literal word, trailing `*` matches the rest), requiring every segment to match (`cargo test *` will not allow `cargo test && rm x`).
- **`exec` deny / ask.** Matches full words, words after leading assignments, or basename-stripped program words, case-folded and ordered with intervening arguments allowed (`rm -rf *` matches `/bin/rm -rf x`, `RM -RF x`, and `FOO=1 rm -rf x`).
- **`read` / `edit`.** Gitignore-style patterns anchored at the working directory and each workspace root unless starting with `/` or `~/`, with leading `/**/` matching under any root including Windows drives and UNC shares (**Gitignore-style paths**). Allow matches resolved paths. Deny / ask also matches written spellings, directory aliases, folded names, and beneath each pattern's literal prefix as written, at policy load, and at check time (**Deny and ask rules are the enforced boundary for path tools**), blocking symlink detours like `.env -> ../shared/.env` or `secrets -> ../vault`.

## Search Walks

`grep` and `glob` gate their resolved root before walking, do not follow symlinks below it, and filter every discovered file (resolved and written spelling) against `read` deny and ask rules without extra I/O, withholding matches and appending only the withheld count (no names).

## Approval Round-Trip

`run_tool_round` gates calls sequentially so at most one approval is active, then runs consecutive parallel-safe read tools (`read`, `grep`, `glob`) concurrently once gated so approvals never wait on running tools.

For an asking call, dispatch emits `Event::ApprovalRequested` with call ID, basis, optional session grant, and preview (a diff against current content for `edit` / `write` or between strings when an edit cannot apply, and the command for `bash`). `await_approval` races `Op::Resolve` (matched by ID) against interrupts and shutdown via `Mailbox::race_or` while queuing prompts and mode switches. Frontends reply with `ApprovalDecision` (`DenyWithFeedback` passes text to the model, and `Abort` interrupts the turn).

The TUI pushes `ApprovalModal` onto `ModalStack`, whose `on_cancel` resolves `Abort` on dismissal so the turn never hangs. A 300 ms acceptance guard (restarting when the preview reappears or an editor opens / closes) blocks accidental keystrokes or pastes. Previews wrap every row, scroll past 16 rows, escape control and zero-width characters, and render command whitespace runs as `·`.

Session grants allow the call and matching future calls:

- **Grant.** `Policy::session_grant` derives the narrowest rule via `Rule::for_call` and offers it only when adding it would allow the call (omitted for ask-rule hits, unrecognized or multi-command shells, or words with expansions, globs, or non-displayable characters).
- **Editor.** Users may widen or narrow the pattern, provided `SessionGrant::accept` confirms it still covers the call.
- **Engine check.** Frontend validation is advisory, so dispatch verifies the returned rule with `SessionGrant::admit` and denies on failure.
- **Lifetime.** Admitted rules join the in-memory policy until `/clear`, `/archive`, or successful `/resume` (**Approvals stay in process**), synced to frontends via `Event::SessionRulesUpdated` for `/status`.

## Headless Runs

`-p` runs call `Config::set_unattended` in `main` so calls needing approval settle before reaching the frontend, `run_op` renders events (`stream-json` carries results), and `TextState::render` escapes control characters in model and tool output.

## Design Decisions

1. **Explicit rules precede model judgment.** Deny and ask are standing user boundaries that classification cannot widen.
2. **The classifier can allow or ask.** Uncertainty stays with the user, while deny rules, `plan`, user rejection, and unattended mode block execution.
3. **User intent has explicit provenance.** Excluding assistant text and tool output stops untrusted instructions from authorizing calls.
4. **Classification is a side query.** Reusing `Client::complete` shares auth, gateways, cancellation, and billing without another provider stack.
5. **Native path tools resolve once and act on the result.** Binds matching to the opened path and blocks final-component symlink swaps. Intermediate-directory swaps require `openat` traversal or an OS sandbox ([#133](https://github.com/hakulab/oxide-code/issues/133)). File-integrity checks stay separate from approval.
6. **Project files tighten only.** Untrusted checkouts may add `deny` or `ask` rules while allow rules and classifier settings remain user-owned.
7. **Approvals stay in process.** Persisted grants lack disk ground truth to revalidate on resume (unlike `FileSnapshot` rehashing), so restoring them would regress trust.
8. **Gitignore-style paths.** Lets `.env` or `target` match subtrees without `**/`, anchors project rules at every workspace root, and keeps exact `/` and `~/` rules available.
9. **Deny and ask rules are the enforced boundary for path tools.** Matching written spellings, aliases, and load- and check-time resolutions blocks symlink bypasses for path tools, while intermediate-directory races and `bash` filesystem access still require an OS sandbox.
10. **Approval decisions on the op channel.** Routing through `ops` and `ModalStack` reuses turn cancellation, shutdown, and queuing with no second control path.
11. **Mode switches wait for the next gate, round boundary, or context render.** Avoids cancelling already-approved calls or pending approvals while ensuring the model's next request reflects the enforced mode and immediate confirmation keeps the status line accurate.
12. **The keyboard cycle reaches `plan` before `yolo` and wraps at both ends.** Stepping forward from `auto` passes stricter modes before `yolo` (announced in the footer beforehand), and Shift+Tab reverses any overshoot with wrap (accepting that Shift+Tab from `auto` reaches `yolo` in one press).

## Out of Scope

- **An OS sandbox.** Needed to enforce filesystem boundaries across `bash` commands and intermediate-directory symlink races ([#133](https://github.com/hakulab/oxide-code/issues/133)).

## Sources

- `crates/ox-core/src/engine/classifier.rs`: the classifier request, the user prompts it carries, and its decision.
- `crates/ox-core/src/engine/dispatch.rs`: `run_tool_round`, the gate, and `await_approval`.
- `crates/ox-core/src/permission.rs`: `Mode`, `Workspace`, `Access::for_path`, `Verdict`, `Basis`, `Policy::verdict`, `Policy::session_grant`, `SessionGrant`, and `ReadFilter`.
- `crates/ox-core/src/permission/rule.rs`: `Rule`, the patterns of each class, `MatchDiscipline`, and `Rule::for_call`.
- `crates/ox-core/src/permission/shell.rs`: `CommandTarget` and its segments and words.
- `crates/ox-core/src/permission/shell/pattern.rs`: the `exec` pattern grammar.
- `crates/ox-core/src/permission/shell/walk.rs`: the simple-command extraction that leaves unsupported syntax unmatched.
- `crates/ox-core/src/permission/shell/word.rs`: quote removal, expansions, and literal words.
- `crates/ox-tui/src/modal/approval.rs`: `ApprovalModal`, its preview, and its rule and feedback editors.
