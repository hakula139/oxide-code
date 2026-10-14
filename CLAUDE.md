# CLAUDE.md: oxide-code

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
│   ├── compact_boundary.rs                 # Compact boundary persistence, file-tracker reset, live transcript replacement
│   ├── compaction.rs                       # /compact driver: stripped-transcript summarization request + summary-prefix wrapping
│   └── event.rs                            # AgentEvent, UserAction, AgentSink trait, StdioSink
├── client.rs                               # Client module root
├── client/
│   ├── anthropic.rs                        # Anthropic Messages API client (Client struct + streaming)
│   └── anthropic/
│       ├── betas.rs                        # Per-request `anthropic-beta` header computation, [1m] gating
│       ├── billing.rs                      # Anthropic billing attestation (fingerprint, cch hash, x-anthropic-billing-header)
│       ├── completion.rs                   # Non-streaming `Client::complete` + body builder for one-shots
│       ├── identity.rs                     # Per-machine `device_id` for `metadata.user_id`: lazy mint + persist at $XDG_DATA_HOME/ox/user-id
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
├── model.rs                                # Ground-truth table: display name, cutoff, capabilities, and unknown raw-id fallback
├── model/
│   └── pricing.rs                          # Per-million-token cost rates + USD estimator (excludes account / marketplace adjustments)
├── permission.rs                           # Permission module root: Mode, Policy::decide tiered gate, Target / GateTarget, dangerous-pattern deny defaults
├── permission/
│   └── rule.rs                             # Rule grammar: `tool(specifier)` parse + match (bash exact / prefix / wildcard, gitignore-style path globs)
├── prompt.rs                               # System prompt builder (section assembly)
├── prompt/
│   ├── environment.rs                      # Runtime environment detection (platform, git, date, knowledge cutoff)
│   ├── instructions.rs                     # Instruction file discovery and loading (CLAUDE.md, AGENTS.md)
│   └── sections.rs                         # Static prompt section constants (intro, guidance, style)
├── session.rs                              # Session module root
├── session/
│   ├── actor.rs                            # Session actor task body + SessionCmd protocol + receive-and-drain batching loop
│   ├── chain.rs                            # ChainBuilder: UUID-DAG message-chain reconstruction (fork-aware tip pick + parent walk)
│   ├── display.rs                          # Shared session-listing formatters (relative time, `id · when · N msgs · branch · project` line)
│   ├── entry.rs                            # JSONL entry types (Header, Message, Title, Summary, Compact, ToolResultMetadata, FileSnapshot) and metadata structs
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
│   ├── clear.rs                            # /clear (new, reset): forwards UserAction::Clear, resets ChatView, drops the AI title
│   ├── compact.rs                          # /compact [<instructions>]: forwards UserAction::Compact. Agent loop drives the summarization
│   ├── config.rs                           # /config: opens a KvOverview modal of resolved config + layered file paths
│   ├── confirm.rs                          # ConfirmDeleteSessionModal: destructive-action gate. Runs the unlink synchronously on Y, sticky inline error on failure
│   ├── context.rs                          # SlashContext (borrowed ChatView + LiveSessionInfo + modal slot) handed to each command's execute
│   ├── delete.rs                           # /delete <id-prefix>: typed-arg form, resolves prefix, pushes ConfirmDeleteSessionModal. Bare /delete rejected
│   ├── diff.rs                             # /diff: uncommitted git changes + untracked, 64 KB cap on UTF-8 boundary
│   ├── effort.rs                           # /effort: bare opens the slider. `/effort <level>` resolves the typed-arg shortcut
│   ├── effort_slider.rs                    # EffortSlider: Speed ↔ Intelligence horizontal slider, opened by bare /effort
│   ├── help.rs                             # /help: opens a KvOverview modal listing every registered command
│   ├── init.rs                             # /init: synthesize an AGENTS.md / CLAUDE.md author-or-update prompt
│   ├── matcher.rs                          # filter_and_rank: tier-ranked popup matches
│   ├── model.rs                            # /model: bare opens picker. `/model <id>` resolves alias → lookup → unique suffix → unique substring. `[1m]` first-class
│   ├── parser.rs                           # parse_slash + popup_query: detect `/cmd args`. Allows `:` and `.`
│   ├── picker.rs                           # ModelEffortPicker: combined model + effort modal. Emits a single SwapConfig
│   ├── registry.rs                         # SlashCommand trait + SlashOutcome + echoes_input + BUILT_INS slice + alias-aware lookup
│   ├── rename.rs                           # /rename: bare opens a single-line title editor. `/rename <title>` applies directly and locks out AI title generation
│   ├── resume.rs                           # /resume (alias /continue): bare opens a searchable session picker (Tab toggles current-project ↔ all-projects, Ctrl+D / Delete pushes the confirm-delete modal). `/resume <id-prefix>` jumps directly
│   ├── status.rs                           # /status: opens a KvOverview modal of session descriptors
│   └── theme.rs                            # /theme: bare opens the picker (live preview). `/theme <name>` validates against the curated roster and swaps
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
│   ├── components.rs                       # Components module root
│   ├── components/
│   │   ├── chat.rs                         # ChatView: scrollable chat with block stacking and history load
│   │   ├── chat/
│   │   │   ├── blocks.rs                   # ChatBlock trait + RenderCtx + icon-prefix helpers
│   │   │   └── blocks/
│   │   │       ├── assistant.rs            # AssistantText + AssistantThinking
│   │   │       ├── compacted.rs            # CompactedBlock: bordered surface with bold accent header (count + optional focus) + markdown summary body
│   │   │       ├── error.rs                # ErrorBlock
│   │   │       ├── git_diff.rs             # GitDiffBlock: unified-diff render reusing the Edit-tool `+` / `-` row-bg + line-number gutter
│   │   │       ├── interrupted.rs          # InterruptedMarker: dim italic `(interrupted)` line on cancel
│   │   │       ├── streaming.rs            # StreamingAssistant (in-flight buffer + render cache)
│   │   │       ├── system.rs               # SystemMessageBlock: left-bar accent + body text for slash-command output
│   │   │       ├── tool.rs                 # ToolCallBlock + ToolResultBlock (left-bar border machinery + per-variant dispatch)
│   │   │       ├── tool/
│   │   │       │   ├── bordered_row.rs     # Shared `[bar] [text]` row renderer for unnumbered body / header / footer rows
│   │   │       │   ├── diff.rs             # Edit-tool unified diff body: boundary trim + per-side budget + line-number gutter
│   │   │       │   ├── glob.rs             # Glob-tool body: header + flat path list + truncation footer
│   │   │       │   ├── grep.rs             # Grep-tool per-file groups of line-numbered matches (content mode)
│   │   │       │   ├── numbered_row.rs     # Shared `[bar] [number] [sep] [text]` row renderer: pipe sep for read / grep, sign sep for diff
│   │   │       │   ├── read_excerpt.rs     # Read-tool line-numbered excerpt body + path / range header
│   │   │       │   └── text.rs             # Default truncated-text body (fallback for tools without a richer view)
│   │   │       └── user.rs                 # UserMessage
│   │   ├── input.rs                        # Multi-line input area (ratatui-textarea) + slash-popup wiring + arg-mode `[id]` ghost text
│   │   ├── input/
│   │   │   ├── popup.rs                    # Slash-command autocomplete overlay: dim non-selected, bold selected, alias parens
│   │   │   └── snapshots/                  # `cargo insta` baselines for popup render tests
│   │   ├── snapshots/                      # `cargo insta` baselines for chat, input, status, and welcome render tests
│   │   ├── status.rs                       # Configurable status-line component state + run-state spinner
│   │   ├── status/
│   │   │   └── line.rs                     # Ordered segment rendering for the status line
│   │   └── welcome.rs                      # Empty-state welcome screen: identity ribbon + body column, themed via `accent`/`text`/`dim`
│   ├── cursor.rs                           # `place_clamped`: shared right-edge-clamp cursor placement for input surfaces
│   ├── event.rs                            # ChannelSink (mpsc transport for the TUI)
│   ├── glyphs.rs                           # Shared visual constants (chevrons, bar, tool indicators, spinner frames)
│   ├── markdown.rs                         # Markdown module root (pulldown-cmark + syntect renderer)
│   ├── markdown/
│   │   ├── highlight.rs                    # Syntax highlighting (syntect lazy-loaded SyntaxSet / ThemeSet)
│   │   └── render.rs                       # pulldown-cmark event walker, inline / block / list / table rendering
│   ├── modal.rs                            # Modal trait, ModalKey, ModalAction, ModalStack: focus-grabbing UI overlays
│   ├── modal/
│   │   ├── approval.rs                     # ApprovalModal: approve-or-deny overlay for a gated tool call, on_cancel resolves dismissals to Deny
│   │   ├── kv_overview.rs                  # Generic KvOverview / KvSection: read-only sectioned kv-table modal used by /status, /config, /help
│   │   ├── list_picker.rs                  # Generic ListPicker<T: PickerItem>: cursor + render primitive used by concrete pickers
│   │   └── searchable_list.rs              # Generic SearchableList<T: SearchableItem>: substring filter + scrollable viewport for searchable pickers
│   ├── pending_calls.rs                    # Tool-call correlation state for streaming and transcript resume
│   ├── snapshots/                          # `cargo insta` baselines for full App frame render tests
│   ├── terminal.rs                         # Terminal init / restore, alternate-scroll (DECSET 1007), synchronized output, panic hook
│   ├── theme.rs                            # Theme palette (Slot{fg,bg,modifiers} per role) + style helpers + LazyLock-cached Mocha default
│   ├── theme/
│   │   ├── builtin.rs                      # Built-in TOML catalogue (Mocha / Macchiato / Frappe / Latte / Material via include_str!) + lookup
│   │   ├── color.rs                        # Color string parsing (hex, ANSI named, indexed, reset)
│   │   └── loader.rs                       # Theme TOML deserialization + base+overrides resolution (resolve_theme + SlotPatch)
│   └── wrap.rs                             # Word-wrap with continuation indent for styled lines
├── util.rs                                 # Shared utilities module root
└── util/
    ├── env.rs                              # Environment-variable helpers (`string`, `bool`: empty-is-absent semantics)
    ├── fs.rs                               # Filesystem helpers: `create_private_dir_all` (0o700) + `atomic_write_private` (0o600 temp+rename)
    ├── git.rs                              # Git / `gh` probes for branch + open PR (best-effort, debug-logged failures)
    ├── lock.rs                             # Async retry helper for advisory locks (used by oauth)
    ├── log.rs                              # `tracing` subscriber init: file under $XDG_STATE_HOME in TUI mode, stderr otherwise
    ├── path.rs                             # Path display + expansion helpers (`tildify`: $HOME → ~/, `expand_user`: ~/ → $HOME)
    ├── text.rs                             # Display-width-aware text helpers (`truncate_to_width`, `ELLIPSIS`)
    ├── time.rs                             # Process-wide local-offset cache (`init_local_offset` at startup, `local_offset` reads)
    └── tls.rs                              # `load_extra_ca_certs`: parse a PEM bundle into `reqwest::Certificate`s for trust-store append
```

## Documentation

- [`docs/README.md`](docs/README.md): top-level index of design specs, research notes, user guides, and the roadmap.
- [`docs/guide/`](docs/guide/): user-facing docs (installation, quickstart, configuration, slash commands, instructions, sessions, theming).
- [`docs/design/`](docs/design/) and [`docs/research/`](docs/research/): internal architecture decisions and external research, indexed by `docs/design/README.md` and `docs/research/README.md`.
- [`docs/roadmap.md`](docs/roadmap.md): working features, current focus, and explicit non-goals.

## Coding Conventions

### Trait Design

- Per-instance metadata (display name, icon, input summary) goes on the trait instead of a separate `match name { ... }` table. Adding a new implementation should require editing only the new file.

### Error Handling

- Application code: `anyhow::Result` with `.context()` for actionable messages.
- `thiserror::Error` only when callers need to match on error variants.
- Avoid `unwrap()` / `expect()` in production code. Reserve for cases with a clear invariant comment.

### Discarding Results

- Use `_ = expr` (no `let`) to discard a result you don't need, typically the `()` from `writeln!`/`write!` against a `String` (infallible by `fmt::Write`). `let _ = expr` adds nothing and makes the intent noisier. The bare `_ = ...` form is what the rest of the crate uses.

### Lint Suppression

- Use `#[expect(lint)]` instead of `#[allow(lint)]`. `#[expect]` warns when the suppressed lint is no longer triggered, preventing stale suppressions from accumulating.
- `#[expect]` reason strings must describe the current state rather than future plans.
- For complexity / size lints (`clippy::too_many_lines`, `clippy::cognitive_complexity`, etc.), the default response is to **extract a helper**. Reach for `#[expect]` only when the function is irreducibly cohesive, and explain that in the reason string.

### Section Dividers

- Use `// ── Section Name ──` for section dividers in code (box-drawing character `─`, U+2500).
- In tests, use `// ── function_name ──` as section headers grouping tests by the function they cover.

### Comments

- Comment the **why** instead of the **what**. Comments earn their place by explaining intent, trade-offs, invariants, or constraints the code can't convey on its own. Skip comments that restate the code or narrate the change.
- Keep `//` comments to one line per thought. Multi-line only when the rationale genuinely needs it.
- Doc comments (`///`) state the **contract** rather than **mechanics**. One-line doc is the default. Use multi-line only when the contract genuinely warrants it.
- Wrap comments at **100 columns** (matching `rustfmt` max_width).
- Write `//` comments as prose. Promote to `///` if list structure is genuinely useful.

### Blank Lines

- One blank line between top-level items (functions, structs, enums, impls, constants). Exception: runs of closely-related one-line `const` / `static` declarations that share a theme (e.g., all the OAuth client constants, all the beta-header names) may sit together without blanks, then take one blank before unrelated items.
- One blank line before and after section dividers (`// ── Name ──`). This applies inside `#[cfg(test)]` modules too. The first divider takes a blank line after the `use super::*` block.
- Inside function bodies, use blank lines to separate logical phases (e.g., setup → validation → execution → result).
- Group a single-line computation with its immediate validation guard (early-return `if`): no blank between them. Multi-line `let` bindings (async chains, builder patterns) keep the blank before their guard.

### Module Organization

- New-style module paths: `foo.rs` alongside `foo/` directory. Avoid `foo/mod.rs`.
- Keep files focused: one primary type or concern per file. Split proactively when files grow large.
- Place types in the module that reflects their conceptual domain. A cross-module trait belongs where the **contract** lives rather than beside the first implementation.
- Avoid `pub use` re-exports that obscure where items are defined.
- Order helper functions after their caller (top-down reading) _within each section_.
- New struct fields / enum variants go at the most semantically appropriate position instead of being appended by default.

### Visibility

- Default to the smallest visibility needed: private → `pub(crate)` → `pub`.
- `pub` items form the crate's API surface. Use `pub(crate)` for items shared across modules but not intended for external use.

### Imports

- Group `use` statements in three blocks separated by blank lines: std → external crates → internal modules. `super::` and `crate::` paths belong together in the internal block: do not split them.
- Within each block, sort alphabetically.

### String Literals

- Prefer raw strings (`r"..."`) when the string contains characters that would need escaping. Always use the minimum delimiter level needed (`r"..."` → `r#"..."#` → `r##"..."##`).
- Use `indoc!` / `formatdoc!` for multiline string content so the literal can be indented with surrounding code. Inline at the call site when the string is used once. Use a named constant only when it is shared or very large. Avoid `\n` escapes and `\x20` workarounds for multiline content.
- Ellipsis: always `...` (three ASCII dots), never U+2026. Applies everywhere: prose, comments, doc comments, production strings, tests.

### Dependencies

- Versions centralized in `[workspace.dependencies]` in the root `Cargo.toml`. Member crates reference them with `dep.workspace = true`.
- Only add dependencies to the workspace when a PR first needs them.
- Prefer crates with minimal transitive dependencies.
- Platform-specific dependencies (Unix-only `nix`, macOS-only `security-framework`) are declared under `[target.'cfg(unix)'.dependencies]` / `[target.'cfg(target_os = "macos")'.dependencies]` in the crate's `Cargo.toml`. Code guarded by `#[cfg(unix)]` / `#[cfg(target_os = "macos")]` stays in the same module: do not split platform variants into separate files.

### Git Conventions

Follows global CLAUDE.md commit / branch / PR conventions, plus:

- **Scope**: the most specific area changed: module (e.g., `client`, `config`, `oauth`), doc target (e.g., `CLAUDE`, `research`), or crate name only for cross-module changes.
- **PRs**: assign to `hakula139`. Label `enhancement` for `feat`, `bug` for `fix`. Descriptions follow `.github/pull_request_template.md`. Drop `crates/<crate>/src/` prefix on crate sources in the Changes table. Must not reference gitignored working docs.

### Testing

- Unit tests in the same file as the code they test (`#[cfg(test)]` module).
- Integration tests in `tests/` directory for cross-module behavior.
- Group tests by function under `// ── function_name ──` section headers. Section order must mirror the production function order in the same file. Within each section, order: happy path → variants → edge / error cases.
- Name tests after the scenario they cover, prefixed by the function name (e.g., `parse_sse_frame_missing_data`). Phrase the scenario side (`string_unset_is_absent`) instead of the mechanism (`string_unset_returns_none`).
- Use `indoc!` for multi-line string literals in tests.
- Use established test infra: `wiremock` for HTTP, `temp-env` for env isolation, `TestBackend` + `insta` for TUI snapshots, extracted trait fakes for hard-to-mock dependencies.
- Assertions must verify actual behavior. Each should fail if the code under test has a plausible bug.
- Prefer a concise suite with full coverage over many minimal tests. Merge tests that cover the same path.

### Documentation Maintenance

- Keep `README.md` user-facing. It should describe value, supported features, and usage rather than internal progress tracking.
- Keep `docs/roadmap.md` as the canonical in-repo roadmap / status summary. Update it when shipped capability areas or planned priorities change.
- Crate structure diagrams must match the actual filesystem. When adding, removing, or renaming modules, update the tree in this file. Entries are sorted alphabetically. Directories sort alongside their parent `.rs` file.
- After substantive changes, sweep docs for stale claims: `README.md` status bullets, `docs/roadmap.md` Working Today / Current Focus sections, this file's crate tree and conventions, `docs/guide/*` user instructions, and `docs/research/**/*` deferred / follow-up notes that the change now resolves.

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

The `pnpm` checks gate the `node-check` CI job. `cspell` covers Rust sources too, so a new word in a doc comment fails the same way as one in `README.md`.

`nix develop` provisions the hook toolchain and installs a [pre-commit](https://pre-commit.com) hook (generated by [`git-hooks.nix`](https://github.com/cachix/git-hooks.nix)) that runs the compile-free subset of these checks at commit time: `rustfmt`, `nixfmt`, `markdownlint`, and `cspell`. `nix flake check` runs the same hooks. `clippy`, tests, and coverage stay out of the hook because their build cost would gate every commit.

### Mutation testing

Coverage reports whether a line ran. Mutation testing reports whether a mutation of that line would be caught. Run out-of-band before large-scope changes ship because a full run is slow:

```bash
cargo mutants --package oxide-code
```

Surviving mutants usually mean a test asserts something too loose. Tighten the assertion. If the mutant is genuinely equivalent, exclude it with an explanatory comment.

## Code Review

After verification passes, review for:

- Correctness and edge cases
- Adherence to project conventions (this file)
- Conciseness: prefer the simplest idiomatic solution
- DRY: flag duplicate logic across modules and look for extraction opportunities
- Cross-file consistency: parallel types should use the same structure, naming, ordering, and derive traits
- Comment hygiene: verbose multi-line docs that should be one-liners, missing WHY comments where non-obvious
- Visibility: `pub(crate)` where `pub(super)` or private suffices
- Idiomatic Rust: iterators, pattern matching, type system, ownership, standard library
- Existing crates: flag hand-written logic that an established crate already handles
- Test coverage gaps
