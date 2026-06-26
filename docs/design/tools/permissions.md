# Tool Permissions and Approval

Tiered permission gate in front of every mutating tool call: instant static rules settle the obvious cases, a cheap classifier judges the ambiguous middle, and the user is asked only when real risk remains.

Companion docs: [research/tools/permissions.md](../../research/tools/permissions.md), [slash/modals.md](../slash/modals.md), [session/file-tracking.md](../session/file-tracking.md).

## Modes

A mode sets the standing posture, shaped like the `Effort` enum (`ALL` / `as_str` / `Display` / `FromStr`) so it threads through config and a future `/permission` control the same way `effort` does.

- **`auto`** (default): the tiered pipeline below. The gate is on out of the box, flipping today's unchecked behavior.
- **`plan`**: read-only analysis. Read-only tools allow; every mutating tool denies, including `bash`, which cannot be statically proven side-effect-free.
- **`yolo`**: allow everything, skipping the pipeline. The opt-in "dangerously skip" posture for trusted or externally sandboxed environments. `yolo` bypasses deny rules too, so it is the one mode with no floor.

## Decision Pipeline

In `auto` mode the gate evaluates a call in fixed order and stops at the first match. This is the shipped Phase 1 pipeline; the classifier (step 6's fallback) and session allow-always are later phases noted inline.

1. **Deny match** (user ∪ project deny rules, including the shipped dangerous-pattern defaults) → deny.
2. **Read-only tool** (`read`, `glob`, `grep`) → allow.
3. **`plan` mode** → deny every mutating tool.
4. **Edit-class call inside the working directory** (`edit` / `write`, target path canonicalized first) → allow.
5. **Allow match** (user allow rules) → allow. Session allow-always (Phase 3) will also allow here.
6. **Otherwise** → ask (interactive) or deny (headless). The classifier (Phase 2) will slot in here, allowing a `safe` verdict before the fallback.

Deny precedes every allow, so an explicit deny is never downgraded by an allow rule or by the classifier. The shipped dangerous-pattern defaults seed the deny set, so a classifier outage cannot let a command matching them through. They hold in `auto`, and `yolo` bypasses every deny rule, including these. A per-rule opt-out within `auto` is deferred.

## Rule Grammar

Rules reuse Claude Code's `tool(specifier)` string form for transferable muscle memory, with tool-name matching case-insensitive so `Bash(...)` and `bash(...)` are equivalent. `bash` / `bash()` / `bash(*)` collapse to a tool-wide rule. Bash specifiers come in exact, prefix (`cargo test:*`), and wildcard (`git *`) shapes; `edit` / `write` specifiers are gitignore-style path globs resolved against the working directory.

The `bash` command string is unparsed (`bash -c "..."`), so prefix and wildcard matching is best-effort UX, not a boundary: `ls; rm -rf` and `$()` indirection defeat naive matching. Allow rules therefore match conservatively, and a compound command (chained, substituted, or redirected with `>` / `<`) never matches a prefix allow. Deny rules match the whole command or any chained segment of it. Path keys are canonicalized before any inside-cwd test, since `edit` / `write` resolve neither `..` nor symlinks today, so a raw-string check is bypassable.

## Classifier

The classifier mirrors the background title generator: a cheap Haiku model, a JSON-schema `OutputFormat` forcing a `{ "safe": bool, "reason": string }` envelope, prompt clamping, and warn-log-and-fall-back on any HTTP, parse, or timeout failure. It is consulted only at step 6, never for the static cases.

A verdict caches per session, keyed by tool name plus the verbatim `bash` command string or the canonical `edit` / `write` path, and scoped to the session's resolved policy so a later mode or rule change starts fresh. The cache is process-local and never persisted. On failure the call has already cleared the deny list at step 1, so it falls through to ask interactively or deny in headless mode.

## Approval Round-Trip

When step 6 resolves to ask, the decision rides the existing `user_rx` channel rather than a second channel the turn loop does not poll. Tool dispatch is sequential, so at most one approval is ever outstanding and no id fan-out is needed.

`run_tool_round` threads the tool-use `id` and `sink` into `dispatch_tool_call`, which emits a new `AgentEvent::ApprovalRequested { id, preview }` carrying the id and a small `Clone` preview: an edit diff via `edit::synthesize_chunk`, an all-add diff for `write`, the command string for `bash`. The gate intercepts before `tools.run`, the same place the parse-error short-circuit already returns a synthetic `ToolOutput`. It awaits a decision in a sibling of `await_unless_aborted`: the select-loop still maps `Cancel` → `Cancelled` and `Quit` → `Quit`, still buffers a queued `SubmitPrompt` into `pending`, and matches a new `UserAction::ApprovalDecision { id, decision }`. A decision whose id does not match the outstanding call is ignored, and the wait future is cancel-safe by drop.

On the TUI side an `ApprovalModal` joins the `ModalStack`, built from the `ConfirmDeleteSessionModal` template. The blocked agent must receive a decision on every dismissal path, but `ModalStack::handle_key` intercepts Esc and Ctrl+C before delegation and `clear` drops modals outright, both yielding `ModalAction::None`. The stack therefore gains a cancel hook (a `Modal::on_cancel` returning an optional `ModalAction`) so universal-cancel and session-swap `clear` resolve a pending approval to `ApprovalDecision::Deny` instead of stranding the agent. A denied call returns a synthetic error `ToolOutput`, so the model sees the refusal as a tool result and can choose another approach.

## Configuration

```toml
[permission]
mode = "auto"                              # auto | plan | yolo
allow = ["bash(cargo test:*)", "edit(src/**)"]
deny  = ["bash(rm -rf:*)", "write(.git/**)"]
```

`OX_PERMISSION_MODE` overrides the mode with the same empty-env-falls-through and parse-error-propagates behavior `effort` uses, so a typo fails loudly rather than defaulting permissive. The block adds a `PermissionFileConfig` to `FileConfig` with `deny_unknown_fields`, merged through `merge_section`, and resolved in `Config::load` after the compaction block.

The shipped deny defaults cover catastrophic commands (`rm -rf` of broad roots, disk writes, fork bombs, `curl | sh`) and metadata paths (`write(.git/**)`, `write(.ox/**)`), so step 4's in-cwd allow cannot create a new file under those paths without first clearing step 1.

A checked-in `ox.toml` is untrusted, exactly like the credentials `reject_project_secrets` already blocks, so a project file may set only `deny`. Setting `mode` or `allow` there is rejected with a message pointing to user config. The merge appends project `deny` onto the user deny set, so a repo can restrict itself but never widen what the user allowed.

## Headless Behavior

In `-p` / `--no-tui` runs there is no human to prompt, so a would-ask call denies directly: the gate carries an `interactive` flag the modal-less surfaces set to false, and `check_permission` turns an `Ask` verdict into a synthetic denial. The deny list is the whole boundary here, with no human fallback, so a headless run assumes an already-trusted invocation. The model sees the denial and can retry. Phase 2 will insert the classifier ahead of this fallback, so a `safe` verdict allows before the deny.

## Tool Risk Classes

Risk is a new method on the `Tool` trait, so each tool declares its own class. The three classes are read-only (`read`, `glob`, `grep`), edit-class (`edit`, `write`), and execute (`bash`).

`edit` and `write` share a class but differ in blast radius. `edit` requires the file to exist and to have been read, so it cannot create files. `write` can create brand-new files and parent directories without a prior Read, while overwriting an existing file still goes through the tracker gate. The step-4 cwd check operates on each call's target path, canonicalized (or resolved through its nearest existing parent when the file is new), so the two share one risk class.

## Phasing

Each phase ships independently.

1. **Static tiers, modal, and modes.** The deny / read-only / cwd / allow pipeline, the `ApprovalModal` plus the `ModalStack` cancel hook, the mode enum, and config wiring. Fully deterministic and offline, with step 6 resolving straight to ask.
2. **Classifier.** Insert the Haiku verdict and the per-session cache at step 6.
3. **Session allow-always.** The in-memory "don't ask again this session" map at step 5, mirroring `FileTracker`.

## Design Decisions

1. **Classifier runs last.** Static checks are instant and free, so the model round-trip runs only when neither an allow nor a deny rule settles the call. A pure rule engine would prompt on everything unmatched, breaking the non-stop goal, and a pure classifier would add a round-trip to every `bash` call. The tiered order spends neither.

2. **Default `auto`, flipping today's behavior.** Running tools unchecked is the larger hazard. `yolo` preserves the old behavior for anyone who wants it, as an explicit opt-in.

3. **No immutable danger floor.** The dangerous-pattern defaults are ordinary deny rules rather than a separate immune tier. Evaluating deny before the classifier keeps them effective in `auto`, and `yolo` bypasses every deny rule rather than carving out an exception. A granular per-rule opt-out is deferred.

4. **Project files tighten only.** Honoring a project `allow` or `mode` would let a teammate's repo widen what the local user permitted, the same privilege-escalation vector as project-set credentials. Append-only `deny` lets a repo restrict itself without that risk.

5. **The classifier is a UX layer.** Because `bash` is unparsed, the classifier cannot be trusted against an adversarial model. The deny list is the dependable boundary, and an unreachable classifier degrades to asking rather than to running.

6. **Decision on the existing channel.** Routing the approval through `user_rx` and the `ModalStack` reuses the cancel / quit / queue semantics the turn loop already races, so no second channel or control-flow path is introduced.

7. **Session memory in process only.** A persisted "allow bash X" has no disk ground-truth to re-validate on resume, unlike a `FileSnapshot` rehash, so re-admitting it would be a trust regression. Per the roadmap, session commands stay reversible and cross-session writes wait for an explicit confirmed action.

8. **Classifier reuses the title-gen path.** The cheap-Haiku, JSON-schema, warn-and-fall-back shape already exists and already handles auth, betas, and gateway constraints. A bespoke classifier client would duplicate it.

## Deferred

- Per-rule opt-out of a shipped dangerous-pattern deny default within `auto`; today only `yolo` bypasses them.
- Persisted project allowlists via an explicit confirmed writer (`util/fs.rs::atomic_write_private`).
- Editable bash-prefix widening in the approval modal ("don't ask again for `cargo *`").
- Read confidentiality scoping: `read` can open any absolute path, gated nowhere today.
- Rule env vars beyond `OX_PERMISSION_MODE`.
- Resume survival of session allow-always through the session actor.

## Sources

- `crates/oxide-code/src/agent.rs`: `dispatch_tool_call`, `await_unless_aborted`, `run_tool_round`.
- `crates/oxide-code/src/agent/event.rs`: `AgentEvent`, `UserAction`.
- `crates/oxide-code/src/client/anthropic/wire.rs`: `OutputFormat` JSON-schema envelope.
- `crates/oxide-code/src/config.rs`: `Config::load`, `Effort` enum template.
- `crates/oxide-code/src/config/file.rs`: `FileConfig::merge`, `merge_section`, `reject_project_secrets`.
- `crates/oxide-code/src/session/title_generator.rs`: classifier template (Haiku, structured output).
- `crates/oxide-code/src/slash/confirm.rs`: `ConfirmDeleteSessionModal`, the approval-modal template.
- `crates/oxide-code/src/tool.rs`: `Tool` trait, the new risk-class method.
- `crates/oxide-code/src/tool/bash.rs`: unbounded execute surface.
- `crates/oxide-code/src/tool/edit.rs`: read-before-edit gate, `synthesize_chunk` preview.
- `crates/oxide-code/src/tool/write.rs`: new-file creation and the cwd-scope seam.
- `crates/oxide-code/src/tui/modal.rs`: `ModalStack`, the cancel-hook seam for approval.
