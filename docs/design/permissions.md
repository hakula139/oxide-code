# Tool Permissions and Approval

Every tool call passes through a permission gate. Rules place calls in one of three tiers: deny, ask, or allow. The mode decides the calls the rules leave open, and `plan` also denies every `edit`, `exec`, and `net` call. [Permissions](../guide/permissions.md) covers the modes, rule syntax, and settings for users, and this page covers how the gate reaches each decision.

Companion docs: [Modals](modals.md), [File Edits](file-edits.md).

## Rules and Classes

Rules live in three tables under `[permission]`, one per tier, and each table holds a pattern list for each permission class. Each tool declares its class in its `ToolSpec`, and `Tool::spec` has no default, so a new tool cannot inherit a permissive class. No built-in tool belongs to `net` or `mcp` yet, so those lists parse and validate but match no calls, and MCP tools join `mcp` with [#178](https://github.com/hakula139/oxide-code/issues/178).

A `Rule` pairs a class with a pattern, and the list that holds it sets its tier. Every pattern compiles when the config loads, so a malformed pattern fails startup and names its file.

`OX_PERMISSION_MODE` overrides `permission.mode`, treating an empty value as absent and failing on a typo as `effort` does, and `--mode` overrides both, which `/config` credits to the flag. Each config layer appends its `permission.<tier>.<class>` lists and `extra_roots`, and the merge records each entry's layer for `/config`. A project's `.ox/config.toml` may set only `deny` and `ask` in `[permission]`, which `reject_project_keys` enforces (**Project files tighten only**).

## Decision Pipeline

`Tool::prepare` validates and resolves input before the gate, so an invalid call ends without classification or approval. Input validation, Read-before-Edit, and checked file publication stay tool contracts.

`Policy::verdict` is synchronous and stops at the first applicable step:

1. A configured deny rule denies.
2. `plan` denies edit, exec, and net calls.
3. A configured ask rule asks.
4. A read-class operation with no external target allows, since it acts only on session state.
5. A configured allow rule or session grant allows.
6. An unmatched call uses its mode: `auto` returns `Classify`, `manual` asks, `yolo` allows, and `plan` allows the remaining read-only call.

A command-specific allow rule must cover every extracted simple command. Native path rules apply to file tools and search walks, while Bash uses exec rules and the selected mode without inferring filesystem effects.

A `Verdict` carries its `Basis`, which names the matching rule, session grant, mode, session-local capability, or classifier reason. An explicit ask bypasses classification. An unattended call that needs approval becomes `Unapproved` and never emits an approval request nobody would answer.

## Automatic Classification

`engine::classifier` sends a side query through the Anthropic `Client::complete` path, reusing the endpoint, credentials, JSON output support, and transport cancellation. `permission.auto.model` defaults to Haiku, and `permission.auto.instructions` adds the user's natural-language policy. Project files cannot set either.

The request holds user-authored text from the live transcript, the validated tool name and full input, any resolved native path, the working directory, and the workspace roots. A logical user turn keeps its original prompts across compaction and adds queued user input as it arrives, while a resumed compacted session has only the visible user text and new follow-ups, since its summary replaced the earlier prompts. Generated text, assistant explanations, and tool output stay out. The pending action is never truncated, and the classifier reads no files and calls no tools to investigate it.

The classifier returns `allow` or `ask` with a short reason. On allow, dispatch emits a `Notice`, and on ask it sends the normal approval request with `Basis::Classifier`. A failed request, invalid JSON, an empty reason, a refusal, or an explicit non-final stop requires approval, and a gateway that omits the stop field still needs valid decision JSON.

Response usage is recorded before the decision is read, malformed decisions and refusals included. Its `UsageRecord` names the classifier model and joins the session cost without replacing the main model's displayed token usage or context estimate. The classifier has no verdict cache, retry of its own, background task, or static-rule fallback for a failed request.

`Mailbox::race` handles interruption and shutdown. A mode switch can arrive while the request runs, so dispatch applies the latest mode and reevaluates the policy before using the classifier's result.

## Mode Switches

`Op::SetMode` switches the engine's policy. The TUI sends it from the `/mode` picker, `/mode <mode>`, Tab, and Shift+Tab, while a headless run sends none, since `--mode` reaches the engine through `Config`. The engine confirms each switch with `Event::ModeChanged` on receipt, which the TUI records in its session snapshot and shows in the status line.

While idle, the engine applies a switch at once. During a turn, a compaction, or a sign-in, the switch waits in the operation's `Mailbox`, and `Mailbox::apply_mode` hands it to the policy before the gate weighs each call, at each round boundary, and before the engine renders the user context for a prompt or a compaction, so the model reads the mode the policy enforces, as [Prompt](prompt.md#user-context) describes. `Engine::serve` applies a switch still waiting once the operation ends, however it ends (**Mode switches wait for the next gate, round boundary, or context render**).

Tab steps forward and Shift+Tab backward through `Mode`'s declared order, `auto`, `manual`, `plan`, then `yolo`, wrapping at both ends (**The keyboard cycle reaches `plan` before `yolo` and wraps at both ends**). The TUI steps from the last mode it requested, so a second press before the engine confirms the first moves on. The same order lists the modes in the `/mode` picker, its argument completion, `--mode --help`, and error messages, and the keys stay fixed until configurable key bindings land with [#278](https://github.com/hakula139/oxide-code/issues/278).

## Path Targets

A path tool resolves its target in `Tool::prepare`, before any rule runs, against the session's `Workspace` in its `ToolCtx`. `ToolCtx::path_access`, which every path tool and the `grep` and `glob` roots go through, expands a leading `~`, and `Access::for_path` joins the path to the working directory and walks it one component at a time as the kernel does:

- It follows every symlink, dangling ones included, and applies `..` from the current position, so a `missing/..` detour cannot hide a later symlink.
- Past the first missing component, the rest applies as `create_dir_all` would build it, and a walk that cannot finish uses the joined spelling.
- A non-UTF-8 target keeps its exact `PathBuf` for the tool beside the lossy spelling that rules match.
- On Windows, it expands existing 8.3 short names and drops NTFS stream suffixes, so `GIT~1` and `.git::$INDEX_ALLOCATION` resolve as `.git`.

An approval and the tool's messages name the resolved path, in the title for `edit` and `write` and in the body for `read`, and the file tools open `Access::resolved()`, the path the gate checked (**Native path tools resolve once and act on the result**). They open with no-follow flags, so a symlink swapped into the final component fails the open, and they check size and type on the opened file. `grep` and `glob` keep the path as written, since read deny rules match that spelling, and walk from the resolved root.

Workspace roots anchor relative rules and inform classification, but they allow no edits by themselves.

## Command Targets

A `bash` call is parsed with tree-sitter-bash into the simple commands its lists and pipelines join. Each segment keeps its words, leading assignments included, and the index where the program begins, while wrappers and literal script arguments stay ordinary words. The parser resolves no paths and infers no effects.

Quote removal and literal-value tracking tie command patterns to words. A dynamic word, a redirection, or an unsupported construct prevents command-specific allow matching, though the whole-class exec rule `*` can still allow any call that passes the deny and ask checks. Other unmatched calls reach the selected mode, and deny and ask rules still inspect the extracted simple commands. [The user guide](../guide/permissions.md#rules) defines the supported syntax.

## Patterns

[Rules](../guide/permissions.md#rules) gives the user-facing grammar. Allow rules widen what can run, so they match conservatively, while deny and ask rules hold calls back, so they match every spelling a call can have. `MatchDiscipline` carries the two approaches, and the ask tier uses the deny one.

An allow `exec` pattern matches a simple command's words exactly, where a `*` before the last word matches one literal word and a trailing `*` matches the rest. Every segment needs coverage, so `cargo test *` alone cannot allow `cargo test && rm x`. A deny or ask pattern matches the named words, the program words after leading assignments, or the program words without the program's directory. It compares folded words and needs later words only in order, with arguments between them, so `rm -rf *` matches `/bin/rm -rf x`, `RM -RF x`, and `FOO=1 rm -rf x`.

`read` and `edit` patterns work like `.gitignore` entries anchored at each anchor directory, the working directory and each workspace root, unless they start with `/` or `~/`. A leading `/**/` matches under any filesystem root, including a drive or UNC share on Windows (**Gitignore-style paths**). An allow pattern matches the resolved path. A deny or ask pattern also matches the path as the call spells it and through directory aliases, compares folded names, and matches beneath each pattern's literal prefix as written, as resolved when the policy loaded, and as resolved at the check (**Deny and ask rules are the enforced boundary for path tools**). So `.env -> ../shared/.env` cannot route a read around `deny.read = [".env"]`, and `secrets -> ../vault` makes `secrets/**` cover `../vault/key` even when the link appears later.

## Search Walks

`grep` and `glob` gate their resolved root before walking, then check every file the walk finds against the `read` deny and ask rules, both where it resolves and as spelled beneath the root. They withhold matching files and end their output with a count of withheld files but no names. A walk does not follow symlinks below its resolved root, so the check needs no I/O.

## Approval Round-Trip

When the gate asks, the decision travels through the `ops` channel the turn loop polls (**Approval decisions on the op channel**). `run_tool_round` gates calls one at a time, so at most one approval is outstanding, and runs consecutive parallel-safe calls, the read-only `read`, `grep`, and `glob`, together once each passes the gate, so an approval never waits on a running call.

For a call that asks, the dispatcher emits `Event::ApprovalRequested` with the call's ID, the basis that made it ask, a session grant, and a preview. The preview is a diff against the file's current content for `edit` and `write`, or of the two strings when an edit cannot apply, and the command for `bash`. `await_approval` waits through `Mailbox::race_or` as the turn's other waits do, so an interrupt or shutdown ends the wait while a queued prompt or mode switch waits in the `Mailbox`, and it matches `Op::Resolve` by ID. Every frontend answers with an `ApprovalDecision`, where `DenyWithFeedback` returns the user's text to the model and `Abort` ends the turn as an interrupt does.

In the TUI, an `ApprovalModal` joins the `ModalStack`, and its `on_cancel` hook resolves the call to `Abort` whenever the modal leaves without an answer, so the blocked turn always gets one. Its keys answer only after a 300 ms pause, so a keystroke meant for the input box or a pasted burst cannot approve a call, and the guard restarts when the preview reappears or an editor opens or closes. The preview shows everything it approves: every row wraps, a body over 16 rows scrolls, control and zero-width characters render as escapes, and whitespace runs in a command render as `·`.

A session grant lets the user allow the call and every later call it covers:

- **Grant.** `Policy::session_grant` derives the narrowest rule naming the call with `Rule::for_call` and offers it only when adding it would allow the call. A call an ask rule matches, an unrecognized command, a command with several simple commands, and a call with a word that expands, globs, or holds a character the editor cannot show offer none, so the user always sees the exact rule they grant.
- **Editor.** The user can widen or narrow the pattern before accepting it, and `SessionGrant::accept` requires it to still cover the call.
- **Engine check.** A frontend's check is advisory, so the gate admits the answered rule against its own grant with `SessionGrant::admit` and denies the call if the rule fails.
- **Lifetime.** An admitted rule joins the engine's policy as an allow rule and lives only in memory until `/clear`, `/archive`, or a successful `/resume` (**Approvals stay in process**). `Event::SessionRulesUpdated` carries the list to the frontend for `/status`.

## Headless Runs

Headless `-p` runs render engine events through `run_op`. `main` calls `Config::set_unattended`, so the policy settles calls that would need approval before they reach the frontend, and stream JSON carries the call's result. `TextState::render` escapes control characters in model and tool text.

## Design Decisions

1. **Explicit rules precede model judgment.** Deny and ask express a standing user boundary. Classification applies only to unmatched calls and cannot widen that boundary.
2. **The classifier can allow or ask.** Uncertainty leaves the decision with the user. Explicit deny, read-only mode, user rejection, and unavailable approval can prevent execution.
3. **User intent has explicit provenance.** Assistant explanations and tool output can contain untrusted instructions, so they cannot supply authorization.
4. **Classification is a side query.** Reusing the existing client keeps authentication, gateway behavior, cancellation, and billing in their owning layers without introducing another provider framework.
5. **Native path tools resolve once and act on the result.** This binds permission matching to the path the tool uses, and a no-follow open makes a symlink swapped into the final component fail. A swap in an intermediate directory needs `openat`-style traversal or an OS sandbox ([#133](https://github.com/hakula139/oxide-code/issues/133)). File integrity checks stay independent of approval.
6. **Project files tighten only.** A checkout may add deny or ask rules while classifier settings and permission grants stay user-controlled.
7. **Approvals stay in process.** Session grants disappear when the session changes and are never restored from persisted tool output. A persisted grant has no disk ground truth to revalidate on resume, unlike a `FileSnapshot` rehash, so restoring it would regress trust.
8. **Gitignore-style paths.** Patterns work like `.gitignore` entries, so `.env` and `target` need no `**/` prefix and cover what is beneath them. Anchoring at every anchor directory applies a project rule to each workspace root, while `/` and `~/` keep exact absolute rules available.
9. **Deny and ask rules are the enforced boundary for path tools.** Matching every spelling and alias of a path, at load and at the check, keeps a symlink from routing a path tool around a rule. The rules do not cover a symlink swapped into an intermediate directory after the check or files a `bash` command touches, so an untrusted model still needs an OS sandbox.
10. **Approval decisions on the op channel.** Routing approvals through `ops` and the `ModalStack` reuses the turn loop's cancel, quit, and queue semantics, so approvals need no second channel or control-flow path.
11. **Mode switches wait for the next gate, round boundary, or context render.** A switch mid-turn changes how the remaining calls are weighed without revisiting calls that already passed the gate, which would mean cancelling a running command or a pending approval. Applying it at a round boundary or before a context render as well means the model's next request names the mode the policy enforces, even when no call reaches the gate first. Confirming the switch on receipt keeps the status line honest about the mode the next call meets.
12. **The keyboard cycle reaches `plan` before `yolo` and wraps at both ends.** Stepping forward from the default `auto` passes the stricter modes before the most permissive one, and the footer names the mode Tab selects next, so `yolo` is announced before a forward press reaches it. Shift+Tab steps backward through the same order, so either key undoes an overshoot, and wrapping keeps every mode reachable from any start. That wrap lets one Shift+Tab press from `auto` reach `yolo` although the footer names only Tab's target, a cost the cycle accepts to keep an overshoot reversible from either end.

## Out of Scope

- **An OS sandbox** ([#133](https://github.com/hakula139/oxide-code/issues/133)). It would enforce a filesystem boundary that the rules cannot reach, covering files a `bash` command touches and intermediate-directory swaps.

## Sources

- `crates/ox-core/src/config.rs`: `OX_PERMISSION_MODE`, `Config::apply_mode_flag`, and `Config::set_unattended`.
- `crates/ox-core/src/config/load.rs`: `reject_project_keys`, the keys a project file may set.
- `crates/ox-core/src/engine.rs`: the live policy, `Engine::set_mode`, and `Engine::serve`.
- `crates/ox-core/src/engine/abort.rs`: `Mailbox`, which holds the mode switches and queued prompts that arrive mid-operation.
- `crates/ox-core/src/engine/classifier.rs`: the classifier request, the user prompts it carries, and its decision.
- `crates/ox-core/src/engine/dispatch.rs`: `run_tool_round`, the gate, and `await_approval`.
- `crates/ox-core/src/permission.rs`: `Mode`, `Workspace`, `Access::for_path`, `Verdict`, `Basis`, `Policy::verdict`, `Policy::session_grant`, `SessionGrant`, and `ReadFilter`.
- `crates/ox-core/src/permission/rule.rs`: `Rule`, the patterns of each class, `MatchDiscipline`, and `Rule::for_call`.
- `crates/ox-core/src/permission/shell.rs`: `CommandTarget` and its segments and words.
- `crates/ox-core/src/permission/shell/pattern.rs`: the `exec` pattern grammar.
- `crates/ox-core/src/permission/shell/walk.rs`: the simple-command extraction that leaves unsupported syntax unmatched.
- `crates/ox-core/src/permission/shell/word.rs`: quote removal, expansions, and literal words.
- `crates/ox-core/src/protocol.rs`: `Event::ApprovalRequested`, `Event::SessionRulesUpdated`, `Event::ModeChanged`, `Op::Resolve`, `Op::SetMode`, and `ApprovalDecision`.
- `crates/ox-core/src/tool.rs`: the `Tool` trait, `ToolSpec`, and `ToolCtx::path_access`.
- `crates/ox-core/src/tool/file.rs`: `read_validated` and `open_gated`, the size and type checks on the opened file.
- `crates/ox-core/src/tool/search.rs`: `SearchRoot` and the withheld-file note.
- `crates/ox-core/src/util/fs.rs`: `open_no_follow`, the open that refuses a swapped symlink.
- `crates/ox-tui/src/action.rs`: `next_mode`, the Tab and Shift+Tab cycle.
- `crates/ox-tui/src/app/dispatch.rs`: `App::cycle_mode`, which steps from the last requested mode.
- `crates/ox-tui/src/modal/acceptance.rs`: `Acceptance`, the type-ahead guard.
- `crates/ox-tui/src/modal/approval.rs`: `ApprovalModal`, its preview, and its rule and feedback editors.
- `crates/ox/src/driver/stdio.rs`: `run_op` and `TextState`.
- `crates/ox/src/main.rs`: the `--mode` flag and the attendance setting.
