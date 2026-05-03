# CLAUDE.md — oxide-code

## Project Overview

oxide-code is a terminal-based AI coding assistant written in Rust, inspired by [Claude Code](https://code.claude.com/docs). It communicates with LLM APIs to help developers with software engineering tasks directly from the terminal.

### CLI

```bash
ox                                          # Start an interactive session
```

### Project Layout

```text
.
├── crates/oxide-code/                      # Main binary crate
├── docs/                                   # Roadmap and research notes
└── target/                                 # Build output
```

### Crate Structure (`crates/oxide-code/src/`)

```text
.
├── agent.rs                                # Agent turn loop, stream accumulation, tool dispatch
├── agent/
│   ├── event.rs                            # AgentEvent, UserAction, AgentSink trait, StdioSink
│   └── pending_calls.rs                    # PendingCall / PendingCalls correlation state shared by live streaming and transcript resume
├── client.rs                               # Client module root
├── client/
│   ├── anthropic.rs                        # Anthropic Messages API client (Client struct + streaming)
│   └── anthropic/
│       ├── betas.rs                        # Per-request `anthropic-beta` header computation, [1m] gating
│       ├── billing.rs                      # Anthropic billing attestation (fingerprint, cch hash, x-anthropic-billing-header)
│       ├── completion.rs                   # Non-streaming `Client::complete` + body builder for one-shots
│       ├── identity.rs                     # Per-machine `device_id` for `metadata.user_id` — lazy mint + persist at $XDG_DATA_HOME/ox/user-id
│       ├── sse.rs                          # SSE pump, frame parsing, API-error formatting
│       ├── testing.rs                      # Cfg-test fixtures shared by client, agent, and title_generator tests
│       └── wire.rs                         # Request / response wire types (CreateMessageRequest, StreamEvent, etc.)
├── config.rs                               # Configuration loading and layered merging
├── config/
│   ├── file.rs                             # TOML config file discovery, parsing, and merge (user + project)
│   └── oauth.rs                            # Claude Code OAuth credentials (macOS Keychain + file), token refresh, directory-based advisory lock
├── file_tracker.rs                         # Per-session FileTracker: Read-before-Edit gate, mtime+xxh64 staleness check, persist-on-finish + verify-on-resume
├── main.rs                                 # CLI entry point, mode dispatch (TUI / REPL / headless), signal handling
├── message.rs                              # Conversation message types
├── model.rs                                # Ground-truth table: marketing name, cutoff, capabilities; `marketing_or_id` unknown-id fallback
├── prompt.rs                               # System prompt builder (section assembly)
├── prompt/
│   ├── environment.rs                      # Runtime environment detection (platform, git, date, knowledge cutoff)
│   ├── instructions.rs                     # Instruction file discovery and loading (CLAUDE.md, AGENTS.md)
│   └── sections.rs                         # Static prompt section constants (intro, guidance, style)
├── session.rs                              # Session module root
├── session/
│   ├── actor.rs                            # Session actor task body + SessionCmd protocol + receive-and-drain batching loop
│   ├── chain.rs                            # ChainBuilder: UUID-DAG message-chain reconstruction (fork-aware tip pick + parent walk)
│   ├── entry.rs                            # JSONL entry types (Header, Message, Title, Summary) and metadata structs
│   ├── handle.rs                           # SessionHandle (cheap-to-clone async API), SharedState, start / resume / roll lifecycle
│   ├── handle/
│   │   └── testing.rs                      # Cfg-test SessionHandle constructors for sibling test modules (dead, acks_then_drops)
│   ├── history.rs                          # Transcript → display interaction stream (pair ToolUse with ToolResult inline)
│   ├── list_view.rs                        # `ox --list` table rendering (writes to any `impl Write`)
│   ├── path.rs                             # Filesystem-safe project subdirectory derivation (sanitize_cwd)
│   ├── resolver.rs                         # CLI `--continue` argument resolution (ResumeMode, resolve_session)
│   ├── sanitize.rs                         # Resume-time transcript repair (drop unresolved / orphan tool blocks, collapse roles, sentinels)
│   ├── snapshots/                          # `cargo insta` baseline JSONL byte-shape snapshots for `actor` round-trip tests
│   ├── state.rs                            # SessionState: pure-data lifecycle struct owned by the actor (uuid chain, counts, finish gating)
│   ├── store.rs                            # SessionStore / SessionWriter (BufWriter-backed): file I/O, XDG path, listing
│   └── title_generator.rs                  # Background AI title generation (Haiku) with detached task
├── slash.rs                                # Slash-command surface root: re-exports + dispatch
├── slash/
│   ├── clear.rs                            # /clear (new, reset) — forwards UserAction::Clear, resets ChatView, drops the AI title
│   ├── config.rs                           # /config — read-only resolved config + layered file paths
│   ├── context.rs                          # SlashContext (borrowed ChatView + SessionInfo) handed to each command's execute
│   ├── diff.rs                             # /diff — `git diff HEAD` + untracked, 64 KB cap on UTF-8 boundary
│   ├── effort.rs                           # /effort — list / swap effort tier; `auto` clears the user pick
│   ├── format.rs                           # Shared kv-section / kv-table renderer
│   ├── help.rs                             # /help — registry-driven command listing
│   ├── init.rs                             # /init — synthesize an AGENTS.md / CLAUDE.md author-or-update prompt
│   ├── matcher.rs                          # filter_and_rank: tier-ranked popup matches
│   ├── model.rs                            # /model — list / swap; resolver alias → lookup → unique suffix → unique substring; `[1m]` first-class
│   ├── parser.rs                           # parse_slash + popup_query — detect `/cmd args`; allows `:` and `.`
│   ├── registry.rs                         # SlashCommand trait + SlashOutcome + BUILT_INS slice + alias-aware lookup
│   └── status.rs                           # /status — model, effort, cwd, version, auth, session id
├── tool.rs                                 # Tool trait, registry, definitions
├── tool/
│   ├── bash.rs                             # Shell command execution with timeout
│   ├── edit.rs                             # Exact string replacement in files
│   ├── glob.rs                             # File pattern matching (glob)
│   ├── grep.rs                             # Content search via regex
│   ├── read.rs                             # File reading with line numbers and pagination
│   └── write.rs                            # File writing with directory creation
├── tui.rs                                  # TUI module root
├── tui/
│   ├── app.rs                              # Root App struct, tokio::select! event loop, render dispatch
│   ├── component.rs                        # Component trait (components report UserAction back to the agent loop)
│   ├── components.rs                       # Components module root
│   ├── theme.rs                            # Theme palette (Slot{fg,bg,modifiers} per role) + style helpers + LazyLock-cached Mocha default
│   ├── theme/
│   │   ├── builtin.rs                      # Built-in TOML catalogue (Mocha / Macchiato / Frappe / Latte / Material via include_str!) + lookup
│   │   ├── color.rs                        # Color string parsing (hex, ANSI named, indexed, reset)
│   │   └── loader.rs                       # Theme TOML deserialization + base+overrides resolution (resolve_theme + SlotPatch)
│   ├── components/
│   │   ├── chat.rs                         # ChatView container (scroll, event dispatch, block stacking, load_history)
│   │   ├── chat/
│   │   │   ├── blocks.rs                   # ChatBlock trait + RenderCtx + icon-prefix helpers
│   │   │   └── blocks/
│   │   │       ├── assistant.rs            # AssistantText + AssistantThinking
│   │   │       ├── error.rs                # ErrorBlock
│   │   │       ├── git_diff.rs             # GitDiffBlock — unified-diff render reusing the Edit-tool `+` / `-` row-bg + line-number gutter
│   │   │       ├── interrupted.rs          # InterruptedMarker — dim italic `(interrupted)` line on cancel
│   │   │       ├── streaming.rs            # StreamingAssistant (in-flight buffer + render cache)
│   │   │       ├── system.rs               # SystemMessageBlock — left-bar accent + body text for slash-command output
│   │   │       ├── tool.rs                 # ToolCallBlock + ToolResultBlock (left-bar border machinery + per-variant dispatch)
│   │   │       ├── tool/
│   │   │       │   ├── bordered_row.rs     # Shared `[bar] [text]` row renderer for unnumbered body / header / footer rows
│   │   │       │   ├── diff.rs             # Edit-tool unified diff body — boundary trim + per-side budget + line-number gutter
│   │   │       │   ├── glob.rs             # Glob-tool body — header + flat path list + truncation footer
│   │   │       │   ├── grep.rs             # Grep-tool per-file groups of line-numbered matches (content mode)
│   │   │       │   ├── numbered_row.rs     # Shared `[bar] [number] [sep] [text]` row renderer — pipe sep for read / grep, sign sep for diff
│   │   │       │   ├── read_excerpt.rs     # Read-tool line-numbered excerpt body + path / range header
│   │   │       │   └── text.rs             # Default truncated-text body (fallback for tools without a richer view)
│   │   │       └── user.rs                 # UserMessage
│   │   ├── input.rs                        # Multi-line input area (ratatui-textarea) + slash-popup wiring
│   │   ├── input/
│   │   │   ├── popup.rs                    # Slash-command autocomplete overlay — dim non-selected, bold selected, alias parens
│   │   │   └── snapshots/                  # `cargo insta` baselines for popup render tests
│   │   └── status.rs                       # Status bar (model, spinner, status, working directory)
│   ├── event.rs                            # ChannelSink (mpsc transport for the TUI)
│   ├── glyphs.rs                           # Shared visual constants (chevrons, bar, tool indicators, spinner frames)
│   ├── markdown.rs                         # Markdown module root (pulldown-cmark + syntect renderer)
│   ├── markdown/
│   │   ├── highlight.rs                    # Syntax highlighting (syntect lazy-loaded SyntaxSet / ThemeSet)
│   │   └── render.rs                       # pulldown-cmark event walker, inline / block / list / table rendering
│   ├── terminal.rs                         # Terminal init / restore, synchronized output, panic hook
│   └── wrap.rs                             # Word-wrap with continuation indent for styled lines
├── util.rs                                 # Shared utilities module root
└── util/
    ├── env.rs                              # Environment-variable helpers (`string`, `bool`: empty-is-absent semantics)
    ├── fs.rs                               # Filesystem helpers — `create_private_dir_all` (0o700) + `atomic_write_private` (0o600 temp+rename)
    ├── lock.rs                             # Async retry helper for advisory locks (used by oauth)
    ├── log.rs                              # `tracing` subscriber init — file under $XDG_STATE_HOME in TUI mode, stderr otherwise
    ├── path.rs                             # Path display helpers (`tildify`: rewrite $HOME prefix as ~/)
    └── text.rs                             # Display-width-aware text helpers (`truncate_to_width`, `ELLIPSIS`)
```

## Coding Conventions

### Trait Design

- Per-instance metadata (display name, icon, input summary) goes on the trait, not in a separate `match name { ... }` table. Adding a new implementation should require editing only the new file, not switch arms scattered across the codebase.

### Error Handling

- Application code: `anyhow::Result` with `.context()` for actionable messages.
- Reach for `thiserror::Error` only when callers need to match on error variants (no current uses; add the dep when the first one lands).
- Avoid `unwrap()` / `expect()` in production code. Reserve them for cases with a clear invariant comment.

### Discarding Results

- Use `_ = expr` (no `let`) to discard a result you don't need — typically the `()` from `writeln!`/`write!` against a `String` (infallible by `fmt::Write`). `let _ = expr` adds nothing and makes the intent noisier; the bare `_ = ...` form is what the rest of the crate uses.

### Lint Suppression

- Use `#[expect(lint)]` instead of `#[allow(lint)]`. `#[expect]` warns when the suppressed lint is no longer triggered, preventing stale suppressions from accumulating.
- `#[expect]` reason strings must describe the current state, not future plans.
- For complexity / size lints (`clippy::too_many_lines`, `clippy::too_many_arguments`, `clippy::cognitive_complexity`, etc.), the default response is to **extract a helper**, not to silence the warning. The lint is a signal that a natural seam exists. Reach for `#[expect]` only when the function is irreducibly cohesive and any extraction would be an artificial split that hurts readability — and say so in the reason string. Trimming a docstring or inlining a small helper to dodge a one-line overage is not a fix; it's noise.

### Section Dividers

- Use `// ── Section Name ──` for section dividers in code (box-drawing character `─`, U+2500).
- In tests, use `// ── function_name ──` as section headers grouping tests by the function they cover.

### Comments

- Comment the **why**, not the **what**. Clear naming, types, and structure already say what the code does; comments earn their place by explaining intent, trade-offs, invariants, magic numbers, or constraints the code can't convey on its own. Skip comments that restate the code, narrate the change ("now uses X" — belongs in the commit message), or carry commented-out code (version control exists).
- Keep `//` comments tight — one line per thought, several lines only when the rationale genuinely needs them. Long structured prose belongs in `///` doc-comments, where rustdoc renders it; in-function `//` blocks should not grow into mini-essays.
- Write `//` comments as prose, not structured markdown. If list structure is genuinely useful, promote the comment to `///` so rustdoc renders it. Either way, follow our `**/*.md` markdownlint conventions: blank line before the list, single space after the marker, incremental numbering for ordered lists, no leading-space indent.

### Blank Lines

- One blank line between top-level items (functions, structs, enums, impls, constants). Exception: runs of closely-related one-line `const` / `static` declarations that share a theme (e.g., all the OAuth client constants, all the beta-header names) may sit together without blanks, then take one blank before unrelated items.
- One blank line before and after section dividers (`// ── Name ──`). This applies inside `#[cfg(test)]` modules too — the first divider takes a blank line after the `use super::*;` block.
- Inside function bodies, use blank lines to separate logical phases (e.g., setup → validation → execution → result).
- Group a single-line computation with its immediate validation guard (early-return `if`) — no blank between them. Multi-line `let` bindings (async chains, builder patterns) keep the blank before their guard.

### Module Organization

- New-style module paths: `foo.rs` alongside `foo/` directory, not `foo/mod.rs`.
- Keep files focused: one primary type or concern per file. When a file or function grows large, split it into smaller units proactively rather than letting it accumulate.
- Place functions and types in the module that reflects their conceptual domain — import paths should not mislead about what the item does. Create new modules when needed for clean organization.
- Avoid `pub use` re-exports that obscure where items are defined. Prefer consistent import paths — if some items are re-exported, re-export all related items so callers never mix paths.
- Order helper functions after their caller (top-down reading order) _within each section_. Whole trait impls or unrelated feature sections don't need to be reshuffled to satisfy this — the rule is about local readability, not cross-section call graphs.
- When adding new fields to structs or variants to enums, place them at the most semantically appropriate position among existing members, not simply appended at the bottom.
- A type used by N callers across M modules belongs in the module that names the **contract**, not the module of the first **implementation**. If `tui::event::AgentSink` is implemented by both a TUI channel and a stdio writer, the trait belongs in `agent::` (the contract), not `tui::` (one implementation).

### Visibility

- Default to the smallest visibility needed: private → `pub(crate)` → `pub`.
- `pub` items form the crate's API surface. Use `pub(crate)` for items shared across modules but not intended for external use.

### Imports

- Group `use` statements in three blocks separated by blank lines: std → external crates → internal modules. `super::` and `crate::` paths belong together in the internal block — do not split them.
- Within each block, sort alphabetically.

### String Literals

- Prefer raw strings (`r"..."`) when the string contains characters that would need escaping. Always use the minimum delimiter level needed (`r"..."` → `r#"..."#` → `r##"..."##`).
- Use `indoc!` / `formatdoc!` for multiline string content so the literal can be indented with surrounding code. Inline at the call site when the string is used once; use a named constant only when it is shared or very large. Avoid `\n` escapes and `\x20` workarounds for multiline content.
- Ellipsis: always `...` (three ASCII dots), never `…` (U+2026). Applies everywhere — prose, comments, doc comments, production strings, tests.

### Dependencies

- Versions centralized in `[workspace.dependencies]` in the root `Cargo.toml`. Member crates reference them with `dep.workspace = true`.
- Only add dependencies to the workspace when a PR first needs them.
- Prefer crates with minimal transitive dependencies.
- Platform-specific dependencies (Unix-only `nix`, macOS-only `security-framework`) are declared under `[target.'cfg(unix)'.dependencies]` / `[target.'cfg(target_os = "macos")'.dependencies]` in the crate's `Cargo.toml`. Code guarded by `#[cfg(unix)]` / `#[cfg(target_os = "macos")]` stays in the same module — do not split platform variants into separate files.

### Git Conventions

#### Commits

- Messages: `type(scope): description`
  - Types: `feat`, `fix`, `refactor`, `docs`, `test`, `ci`, `chore`, `style`, `perf`
  - Scope: the most specific area changed — module (e.g., `client`, `config`, `oauth`), doc target (e.g., `CLAUDE`, `research`), or crate name only for cross-module changes.
- Keep commits atomic — one logical change per commit.

#### Branches

- Format: `<type>/<short-name>`, using the same type set as commits (`feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `style`, `perf`, `ci`). Pick the type that matches the dominant change on the branch; if commits span multiple types, use the one that matches the PR's `type(scope):` title.

#### Pull Requests

- Assign to `hakula139`. Label `enhancement` for `feat`, `bug` for `fix`.
- Do not request review from the PR author (GitHub rejects it).
- Descriptions follow `.github/pull_request_template.md`:
  - Prose intro summarizing what and why.
  - Optional Design decisions section for non-trivial PRs — bullet list of tradeoffs (alternatives rejected, invariants preserved, intentional omissions). Skip for mechanical changes.
  - Per-file Changes table (for non-trivial PRs). Drop the `crates/<crate>/src/` prefix on crate sources (e.g. `tool/glob.rs`, not `crates/oxide-code/src/tool/glob.rs`); keep the full path for repo-root files (`CLAUDE.md`, `Cargo.toml`, `docs/...`, `.cspell/...`).
  - Test plan checklist.
- PR descriptions are review-facing and must not reference gitignored working docs (e.g., `.claude/plans/*`, `.claude/agent-memory-local/*`). Those are internal collaboration notes, not reader context. When deferring follow-ups, describe them inline in the PR body — a reader should not need a file they can't see to understand the PR.

### Testing

- Unit tests in the same file as the code they test (`#[cfg(test)]` module).
- Integration tests in `tests/` directory for cross-module behavior.
- Group tests by function under `// ── function_name ──` section headers. Section order must mirror the production function order in the same file. Within each section, order: happy path → variants → edge / error cases.
- Name tests after the scenario they cover, not the return type. Prefix with the function name being tested (e.g., `parse_sse_frame_missing_data`, `load_oauth_expired_token`). When the scenario and the return value are synonyms (unset env var → `None`), phrase the scenario side (`string_unset_is_absent`), not the mechanism (`string_unset_returns_none`). For parameterless single-behavior functions where the value IS the test, use property form (`icon_is_dollar_sign`), not mechanism form (`icon_returns_dollar_sign`).
- Use `indoc!` for multi-line string literals in tests.
- Reach for the established test infrastructure before hand-rolling: `wiremock` for HTTP round-trips, `temp-env` for environment-variable isolation, `ratatui::backend::TestBackend` + `insta` for TUI render snapshots (review with `cargo insta review`), and an extracted trait with an in-process fake (see `agent::AgentClient`) when a dependency is hard to mock at the network boundary.
- Write assertions that verify actual behavior, not just surface properties. Avoid uniform test data that makes `starts_with` / `ends_with` unfalsifiable, wildcard struct matches (`..`) that discard field values, and loose bounds that accept nearly any output. Each assertion should fail if the code under test has a plausible bug.
- Prefer a concise test suite with full coverage over many minimal tests. Drop tests that are subsumed by more thorough ones. Merge tests that cover the same code path when the combined test remains readable.

### Documentation Maintenance

- Keep `README.md` user-facing. It should describe value, supported features, and usage, not internal progress tracking.
- Keep `docs/roadmap.md` as the canonical in-repo roadmap / status summary. Update it when shipped capability areas or planned priorities change.
- Crate structure diagrams must match the actual filesystem. When adding, removing, or renaming modules, update the tree in this file. Entries are sorted alphabetically; directories sort alongside their parent `.rs` file.
- After substantive changes, sweep docs for stale claims: `README.md` status bullets, `docs/roadmap.md` Working Today / Current Focus sections, this file's crate tree and conventions, `docs/guide/*` user instructions, and `docs/research/*` deferred / follow-up notes that the change now resolves.

## Verification

Run after implementation and before review:

```bash
cargo fmt --all --check                            # Check formatting
cargo build                                        # Build
cargo clippy --all-targets -- -D warnings          # Lint (pedantic, zero warnings)
cargo test                                         # Run tests
cargo llvm-cov --ignore-filename-regex 'main\.rs'  # Check test coverage
pnpm lint                                          # Lint Markdown
pnpm spellcheck                                    # Spell check
```

The `pnpm` checks gate the `node-check` CI job. `cspell` covers Rust
sources too, so a new word in a doc comment fails the same way as one
in `README.md`.

### Mutation testing

Coverage reports whether a line ran; mutation testing reports whether
a mutation of that line would be caught. Run out-of-band before
large-scope changes ship — it is not part of CI because a full run is
slow:

```bash
cargo mutants --package oxide-code
```

Surviving mutants usually mean a test asserts something too loose
(e.g., `starts_with` on uniform input, or a wildcard pattern that
matches every output). Tighten the assertion; if the mutant is
genuinely equivalent, exclude it with an explanatory comment.

## Code Review

After verification passes, review for:

- Correctness and edge cases
- Adherence to project conventions (this file)
- Conciseness — prefer the simplest idiomatic solution
- DRY — flag duplicate logic across modules; look for extraction opportunities
- Cross-file consistency — parallel types and similar patterns should use the same structure, naming, ordering, and derive traits
- Idiomatic Rust — proper use of iterators, pattern matching, type system, ownership, and standard library
- Existing crates — flag hand-written logic that an established crate already handles
- Test coverage gaps
