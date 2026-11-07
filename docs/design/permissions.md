# Tool Permissions and Approval

Every tool call passes through a permission gate. Rules place calls in one of three tiers: deny, ask, or allow. The mode decides the calls the rules leave open, and `plan` also denies every `edit`, `exec`, and `net` call. [Permissions](../guide/permissions.md) covers the modes, the rule syntax and settings for users, and this page covers how the gate reaches each decision.

Companion docs: [Modals](modals.md), [File Edits](file-edits.md).

## Rules and Classes

Rules live in three tables under `[permission]`, one for each tier, and each table holds a pattern list for each permission class: `read`, `edit`, `exec`, `net`, and `mcp`. Each tool declares its class in its `ToolSpec`, and `Tool::spec` has no default, so a new tool cannot inherit a permissive class. No built-in tool belongs to `net` or `mcp` yet, so those lists parse and validate but match no calls, and MCP tools join `mcp` with [#178](https://github.com/hakula139/oxide-code/issues/178).

A `Rule` pairs a class with a pattern, and the list that holds it sets its tier. Every pattern compiles when the config loads, so a malformed pattern fails startup and names its file.

`OX_PERMISSION_MODE` overrides `permission.mode`, treating an empty value as absent and failing on a typo as `effort` does, and `--mode` overrides both, which `/config` credits to the flag. Each config layer appends its `permission.<tier>.<class>` lists and `extra_roots`, and the merge records each entry's layer for `/config`. A project's `.ox/config.toml` may set only `deny` and `ask` in `[permission]`, which `reject_project_keys` enforces (**Project files tighten only**).

## Decision Pipeline

`Tool::prepare` validates and resolves input before permission evaluation. Invalid calls end without classification or approval. Input validation, Read-before-Edit, and checked file publication remain tool contracts.

`Policy::verdict` is synchronous and stops at the first applicable step:

1. A configured deny rule denies.
2. `plan` denies edit, exec, and net calls.
3. A configured ask rule asks.
4. A read-class operation with no external target allows because it acts only on session state.
5. A configured allow rule or session grant allows.
6. An unmatched call uses its mode: `auto` returns `Classify`, `manual` asks, `yolo` allows, and `plan` allows the remaining read-only call.

A command-specific allow rule must cover every extracted simple command. Native path rules apply to file tools and search walks. Bash uses exec rules and the selected mode without inferred filesystem effects.

A `Verdict` carries its `Basis`, which identifies the matching rule, session grant, mode, session-local capability, or classifier reason. Explicit ask bypasses classification. An unattended call that requires approval becomes `Unapproved` and never emits an unanswered approval request.

## Automatic Classification

`engine::classifier` sends a side query using the concrete Anthropic `Client::complete` path, existing endpoint, credentials, JSON output support, and transport cancellation. `permission.auto.model` defaults to Haiku, and `permission.auto.instructions` supplies user-configured natural-language policy. Project files cannot set either field.

The request contains user-authored text from the live transcript, the validated tool name and full input, any resolved native path, the working directory, and workspace roots. A logical user turn retains its original prompts across compaction and adds queued user input as it arrives. A resumed compacted session uses the visible user text and new follow-up, without recovering prompts replaced by its summary. Generated text, assistant explanations, and tool output are excluded. The pending action is never truncated, and the classifier performs no file reads or tool calls to investigate it.

The classifier returns `allow` or `ask` with a short reason. Dispatch emits an existing `Notice` when it allows and uses `Basis::Classifier` in the normal approval request when it asks. Failed requests, invalid JSON, empty reasons, refusals, and explicit non-final termination require approval. A gateway that omits the termination field still needs valid decision JSON.

Successful response usage is recorded before interpreting the decision, including malformed decisions and refusals. A `UsageRecord` names the classifier model and joins session cost without replacing the main model's displayed token usage or context estimate. There is no verdict cache, classifier-specific retry, background classifier task, or static-rule fallback for a failed request.

`Mailbox::race` handles interruption and shutdown. Since mode switches can arrive while the request runs, dispatch applies the latest mode and reevaluates the policy before using the classifier result.

## Mode Switches

`Op::SetMode` switches the engine's policy. The TUI sends it from the `/mode` picker, `/mode <mode>`, Tab, and Shift+Tab, and the REPL from `/mode <mode>`, while a headless run sends none, since `--mode` reaches the engine through `Config`. The engine confirms each switch with `Event::ModeChanged`, which the TUI records in its session snapshot and shows in the status line, and which the REPL prints. While idle, the engine applies a switch at once. During a turn, a compaction, or a sign-in, the switch waits in the operation's `Mailbox`. `Mailbox::apply_mode` hands it to the policy before the gate weighs each call, at each round boundary, and before the engine renders the user context for a prompt or a compaction, so the model reads the mode the policy enforces, as [Prompt](prompt.md#user-context) describes. `Engine::serve` applies a switch still waiting once the operation ends, however it ends (**Mode switches wait for the next gate, round boundary, or context render**).

Tab steps forward and Shift+Tab backward through `Mode`'s declared order, `auto`, `manual`, `plan`, then `yolo`, wrapping at both ends (**The keyboard cycle reaches `plan` before `yolo` and wraps at both ends**). The TUI steps from the last mode it requested, so a second press before the engine confirms the first moves on. The same order sets how the `/mode` picker, its argument completion, `--mode --help`, and the error messages list the modes, and the keys stay fixed until configurable key bindings land with [#278](https://github.com/hakula139/oxide-code/issues/278).

## Path Targets

A path tool resolves its target before any rule runs, in `Tool::prepare`, against the session's `Workspace` in its `ToolCtx`. `ToolCtx::path_access`, which every path tool and the `grep` and `glob` roots go through, expands a leading `~` with `tool_path`, and `Access::for_path` then joins the path to the cwd and walks it one component at a time as the kernel does, following every symlink, dangling ones included, and applying `..` from the current position, so a `missing/..` detour cannot hide a later symlink. Past the first missing component, the rest applies as `create_dir_all` builds it. A walk that cannot finish leaves the target outside the workspace under its joined spelling, and a resolved path that is not UTF-8 keeps its resolved spelling but falls outside the workspace.

An approval names the resolved path, in the title for `edit` and `write` and in the body for `read`, as do the tool's messages, and the file tools open `Access::resolved()`, the path the gate checked (**Resolve once, act on the result**). The file tools open with no-follow flags, so a symlink swapped into the final component fails the open, and they check size and type on the opened file. `grep` and `glob` keep the path as written, since read deny rules match that spelling, and walk from the resolved root.

Workspace roots anchor relative rules and inform classification. They do not implicitly allow edits.

On Windows, resolution expands existing 8.3 short names and drops NTFS stream suffixes, so `GIT~1` and `.git::$INDEX_ALLOCATION` resolve as `.git`.

## Command Targets

A `bash` call is parsed with tree-sitter-bash to extract simple commands joined by lists and pipelines. Each segment retains its words, including leading assignments, and the index where the actual program begins. Wrappers and literal script arguments remain ordinary words. The parser performs no filesystem resolution or effect inference.

Quote removal and literal-value tracking keep command patterns tied to words. Dynamic words, redirections, or unsupported constructs prevent command-specific allow matching. The explicit whole-class exec rule `*` can permit any call after deny and ask checks. Other unmatched calls reach the selected mode. Deny and ask still inspect extracted simple commands. [The user guide](../guide/permissions.md#rules) defines the supported syntax.

## Patterns

[Rules](../guide/permissions.md#rules) gives the user-facing grammar. Allow rules widen what can run, so they match conservatively, while deny and ask rules hold calls back, so they match across every spelling a call can have. `MatchDiscipline` carries the two approaches, and the ask tier uses the deny one.

An allow `exec` pattern matches a simple command's named words exactly, with a `*` before the last word matching one literal word and a trailing `*` matching the rest. Every segment needs coverage, so `cargo test *` alone cannot permit `cargo test && rm x`. A deny or ask pattern matches the named words, the program words after leading assignments, or the program words with the program's directory dropped. It compares folded words and needs later words only in order, with intervening arguments, so `rm -rf *` matches `/bin/rm -rf x`, `RM -RF x`, and `FOO=1 rm -rf x`.

`read` and `edit` patterns work like `.gitignore` entries, anchored at each anchor directory, the cwd and each workspace root, unless they start with `/` or `~/`. A leading `/**/` matches under any file-system root, including a drive or UNC share on Windows (**Gitignore-style paths**). An allow pattern matches the resolved path. A deny or ask pattern also matches the path as the call spells it and through directory aliases, compares names folded, and matches beneath each pattern's literal prefix as written, as it resolved when the policy loaded, and as it resolves when the call is checked, so `.env -> ../shared/.env` cannot route a read around `deny.read = [".env"]`, and `secrets -> ../vault` makes `secrets/**` cover `../vault/key` even when the link appears later (**Deny and ask rules are the enforced boundary for path tools**).

## Search Walks

Native search tools resolve and gate their root before walking. `grep` and `glob` gate on the search root, then check every file the walk finds against the `read` deny and ask rules, where it resolves and as spelled beneath the root, and withhold the matches, ending their output with a count of withheld files but no names. A walk does not follow symlinks below its resolved root, so the check needs no I/O.

## Approval Round-Trip

When the gate asks, the decision travels through the `ops` channel the turn loop polls (**Approval decisions on the op channel**). `run_tool_round` gates calls one at a time, so at most one approval is outstanding, and runs consecutive parallel-safe calls, the read-only `read`, `grep`, and `glob` tools, together once each passes the gate, so an approval never waits on a running call.

For a call that asks, the dispatcher emits `Event::ApprovalRequested` with the call's ID, the basis that made it ask, a session grant, and a preview: a diff against the file's current content for `edit` and `write`, falling back to a diff of the two strings when an edit cannot apply, or the command for `bash`. `await_approval` waits through `Mailbox::race_or` as the turn's other waits do, so an interrupt or shutdown ends the wait and a queued prompt or mode switch waits in the `Mailbox`, and it matches `Op::Resolve` by ID. Every frontend answers with an `ApprovalDecision`, where `DenyWithFeedback` returns the user's text to the model and `Abort` ends the turn as an interrupt does.

In the TUI, an `ApprovalModal` joins the `ModalStack`, and its `on_cancel` hook resolves the call to `Abort` whenever the modal leaves without an answer, so the blocked turn always gets one. Its keys wait for a 300 ms pause before they answer, so a keystroke meant for the input box or a pasted burst cannot approve a call, and the guard restarts when the preview reappears or an editor opens or closes. The preview shows everything it approves: every row wraps, a body over 16 rows scrolls, control and zero-width characters render as escapes, and whitespace runs in a command render as `·`.

A session grant lets the user allow the call and every later call it covers:

- **Grant**: `Policy::session_grant` derives the narrowest rule that names the call with `Rule::for_call` and offers it only when adding it would allow the call. A call an ask rule matches, an unrecognized command, a command with several simple commands, and a call with a word that expands, globs, or holds a character the editor cannot show offer none, so the user always sees the exact rule they grant.
- **Editor**: the user can widen or narrow the pattern before accepting it, and `SessionGrant::accept` requires it to still cover the call.
- **Engine check**: a frontend's check is advisory, so the gate admits the answered rule against its own grant with `SessionGrant::admit` and denies the call if the rule fails.
- **Lifetime**: an admitted rule joins the engine's policy as an allow rule and lives only in memory until `/clear`, `/archive`, or a successful `/resume` (**Approvals stay in process**). `Event::SessionRulesUpdated` carries the list to the frontend for `/status`.

## Stdio Frontends

The `--no-tui` REPL and `-p` headless runs share `run_op`, which passes each `Event::ApprovalRequested` to an `Approver` and sends the decision back as `Op::Resolve`. The request itself is never printed, so `stream-json` output carries only the call's result. `main` calls `Config::set_unattended` for a headless run and for a REPL whose stdin is not a terminal, so piped input never answers a prompt, and such a run passes no `Approver`, since its engine emits no request. The REPL's prompter writes the escaped preview and the rule to stderr and reads the answer from the next stdin line. `TextState::render` escapes control characters in model and tool text, so streamed text cannot redraw the terminal to disguise the preview.

## Design Decisions

1. **Explicit rules precede model judgment.** Deny and ask express a standing user boundary. Classification applies only to unmatched calls and cannot widen that boundary.
2. **The classifier can allow or ask.** Uncertainty leaves the decision with the user. Explicit deny, read-only mode, user rejection, and unavailable approval can prevent execution.
3. **User intent has explicit provenance.** Assistant explanations and tool output can contain untrusted instructions, so they cannot supply authorization.
4. **Classification is a side query.** Reusing the existing client keeps authentication, gateway behavior, cancellation, and billing in their owning layers without introducing another provider framework.
5. **Native path tools resolve once and act on the result.** This binds permission matching to the path the tool uses. File integrity checks remain independent of approval.
6. **Project files tighten only.** A checkout may add deny or ask rules while classifier settings and permission grants remain user-controlled.
7. **Approvals stay in process.** Session grants disappear when the session changes and are never restored from persisted tool output.
