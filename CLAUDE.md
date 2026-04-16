# CLAUDE.md — oxide-code

## Project Overview

oxide-code is a terminal-based AI coding assistant written in Rust, inspired by [Claude Code](http://code.claude.com/docs). It communicates with LLM APIs to help developers with software engineering tasks directly from the terminal.

### CLI

```bash
ox     # Start an interactive session
```

### Project Layout

```text
.
├── crates/oxide-code/          # Main binary crate
├── docs/                       # Roadmap and research notes
└── target/                     # Build output
```

### Crate Structure (`crates/oxide-code/src/`)

```text
.
├── client.rs                   # Client module root
├── client/
│   ├── anthropic.rs            # Anthropic Messages API streaming client
│   └── billing.rs              # Billing attribution header (fingerprint, cch attestation)
├── config.rs                   # Configuration loading and layered merging
├── config/
│   ├── file.rs                 # TOML config file discovery, parsing, and merge (user + project)
│   └── oauth.rs                # Claude Code OAuth credentials (macOS Keychain + file), token refresh, file locking
├── main.rs                     # CLI entry point, agent loop, TUI / REPL / headless dispatch
├── message.rs                  # Conversation message types
├── prompt.rs                   # System prompt builder (section assembly)
├── prompt/
│   ├── environment.rs          # Runtime environment detection (platform, git, date, marketing name)
│   ├── instructions.rs         # Instruction file discovery and loading (CLAUDE.md, AGENTS.md)
│   └── sections.rs             # Static prompt section constants (intro, guidance, style)
├── session.rs                  # Session module root
├── session/
│   ├── entry.rs                # JSONL entry types (Header, Message, Summary) and SessionInfo
│   ├── manager.rs              # SessionManager: lifecycle (start, resume, record, finish)
│   └── store.rs                # SessionStore / SessionWriter: file I/O, XDG path, listing
├── tool.rs                     # Tool trait, registry, definitions
├── tool/
│   ├── bash.rs                 # Shell command execution with timeout
│   ├── edit.rs                 # Exact string replacement in files
│   ├── glob.rs                 # File pattern matching (glob)
│   ├── grep.rs                 # Content search via regex
│   ├── read.rs                 # File reading with line numbers and pagination
│   └── write.rs                # File writing with directory creation
├── tui.rs                      # TUI module root
└── tui/
    ├── app.rs                  # Root App struct, tokio::select! event loop, render dispatch
    ├── component.rs            # Component trait and Action enum
    ├── components.rs           # Components module root
    ├── components/
    │   ├── chat.rs             # Scrollable chat with markdown, tool styling, thinking display
    │   ├── input.rs            # Multi-line input area (ratatui-textarea)
    │   └── status.rs           # Status bar (model, spinner, status, working directory)
    ├── event.rs                # AgentEvent, UserAction, AgentSink trait, ChannelSink, StdioSink
    ├── markdown.rs             # Markdown module root (pulldown-cmark + syntect renderer)
    ├── markdown/
    │   ├── highlight.rs        # Syntax highlighting (syntect lazy-loaded SyntaxSet / ThemeSet)
    │   └── render.rs           # pulldown-cmark event walker, inline / block / list / table rendering
    ├── terminal.rs             # Terminal init / restore, synchronized output, panic hook
    ├── theme.rs                # Catppuccin Mocha palette, style helpers
    └── wrap.rs                 # Word-wrap with continuation indent for styled lines
```

## Coding Conventions

### Error Handling

- Application code: `anyhow::Result` with `.context()` for actionable messages.
- Library error types: `thiserror::Error` derive for errors that callers need to match on.
- Avoid `unwrap()` / `expect()` in production code. Reserve them for cases with a clear invariant comment.

### Lint Suppression

- Use `#[expect(lint)]` instead of `#[allow(lint)]`. `#[expect]` warns when the suppressed lint is no longer triggered, preventing stale suppressions from accumulating.
- `#[expect]` reason strings must describe the current state, not future plans.

### Section Dividers

- Use `// ── Section Name ──` for section dividers in code (box-drawing character `─`, U+2500).
- In tests, use `// ── function_name ──` as section headers grouping tests by the function they cover.

### Blank Lines

- One blank line between top-level items (functions, structs, enums, impls, constants).
- One blank line before and after section dividers (`// ── Name ──`).
- Inside function bodies, use blank lines to separate logical phases (e.g., setup → validation → execution → result).
- Group a single-line computation with its immediate validation guard (early-return `if`) — no blank between them. Multi-line `let` bindings (async chains, builder patterns) keep the blank before their guard.

### Module Organization

- New-style module paths: `foo.rs` alongside `foo/` directory, not `foo/mod.rs`.
- Keep files focused: one primary type or concern per file. When a file or function grows large, split it into smaller units proactively rather than letting it accumulate.
- Place functions and types in the module that reflects their conceptual domain — import paths should not mislead about what the item does. Create new modules when needed for clean organization.
- Avoid `pub use` re-exports that obscure where items are defined. Prefer consistent import paths — if some items are re-exported, re-export all related items so callers never mix paths.
- Order helper functions after their caller (top-down reading order).
- When adding new fields to structs or variants to enums, place them at the most semantically appropriate position among existing members, not simply appended at the bottom.

### Visibility

- Default to the smallest visibility needed: private → `pub(crate)` → `pub`.
- `pub` items form the crate's API surface. Use `pub(crate)` for items shared across modules but not intended for external use.

### Imports

- Group `use` statements in three blocks separated by blank lines: std → external crates → internal modules.
- Within each block, sort alphabetically.

### String Literals

- Prefer raw strings (`r"..."`) when the string contains characters that would need escaping. Always use the minimum delimiter level needed (`r"..."` → `r#"..."#` → `r##"..."##`).
- Use `indoc!` / `formatdoc!` for multiline string content so the literal can be indented with surrounding code. Inline at the call site when the string is used once; use a named constant only when it is shared or very large. Avoid `\n` escapes and `\x20` workarounds for multiline content.

### Enum String Mappings

- Use `strum` derives (`AsRefStr`, `EnumString`, `EnumIter`) for enum ↔ string conversions instead of handwritten matches.
- Keep manual `Display` impls when the display form differs from the serialized form (e.g., titlecase vs. lowercase).

### Dependencies

- Versions centralized in `[workspace.dependencies]` in the root `Cargo.toml`. Member crates reference them with `dep.workspace = true`.
- Only add dependencies to the workspace when a PR first needs them.
- Prefer crates with minimal transitive dependencies.

### Git Conventions

#### Commits

- Messages: `type(scope): description`
  - Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `style`, `perf`
  - Scope: the most specific area changed — module (e.g., `client`, `config`, `oauth`), doc target (e.g., `CLAUDE`, `research`), or crate name only for cross-module changes.
- Keep commits atomic — one logical change per commit.

#### Branches

- Feature branches: `feat/<feature-name>`

#### Pull Requests

- Assign to `hakula139`. Label `enhancement` for `feat`, `bug` for `fix`.
- Do not request review from the PR author (GitHub rejects it).
- Descriptions follow `.github/pull_request_template.md`:
  - Prose intro summarizing what and why.
  - Per-file Changes table (for non-trivial PRs).
  - Test plan checklist.

### Testing

- Unit tests in the same file as the code they test (`#[cfg(test)]` module).
- Integration tests in `tests/` directory for cross-module behavior.
- Group tests by function under `// ── function_name ──` section headers. Section order must mirror the production function order in the same file. Within each section, order: happy path → variants → edge / error cases.
- Name tests after the scenario they cover, not the return type. Prefix with the function name being tested (e.g., `parse_sse_frame_missing_data`, `load_oauth_expired_token`).
- Use `indoc!` for multi-line string literals in tests.
- Write assertions that verify actual behavior, not just surface properties. Avoid uniform test data that makes `starts_with` / `ends_with` unfalsifiable, wildcard struct matches (`..`) that discard field values, and loose bounds that accept nearly any output. Each assertion should fail if the code under test has a plausible bug.
- Prefer a concise test suite with full coverage over many minimal tests. Drop tests that are subsumed by more thorough ones. Merge tests that cover the same code path when the combined test remains readable.

### Documentation Maintenance

- Keep `README.md` user-facing. It should describe value, supported features, and usage, not internal progress tracking.
- Keep `docs/roadmap.md` as the canonical in-repo roadmap / status summary. Update it when shipped capability areas or planned priorities change.
- Crate structure diagrams must match the actual filesystem. When adding, removing, or renaming modules, update the tree in this file. Entries are sorted alphabetically; directories sort alongside their parent `.rs` file.

## Verification

Run after implementation and before review:

```bash
cargo fmt --all --check                            # formatting
cargo build
cargo clippy --all-targets -- -D warnings          # zero warnings (pedantic lints)
cargo test
cargo llvm-cov --ignore-filename-regex 'main\.rs'  # check test coverage
```

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
