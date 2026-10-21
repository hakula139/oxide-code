# AGENTS.md: oxide-code

oxide-code is a terminal-based AI coding assistant written in Rust, inspired by [Claude Code](https://code.claude.com/docs). Its binary, `ox`, talks to LLM APIs to help developers with software engineering tasks from the terminal.

This file holds the rules for every change and points to the conventions for each area. `README.md` covers features and scope for users, [`docs/`](docs/) holds the user guides, conventions, design specs, and research notes, and open GitHub issues track planned work.

## Project Layout

The tree omits the root config files, which follow their tools' conventions.

```text
.
├── .agents/                                # Agent skills for the release procedure
├── .claude/                                # Link to the agent skills for Claude Code
├── about.hbs                               # Plain-text third-party notice template
├── about.toml                              # Release dependency license selection
├── crates/ox/                              # The `ox` binary crate with the CLI and the stdio frontends, mapped in docs/design/architecture.md
├── crates/ox-core/                         # The `ox-core` library crate with the agent core, mapped in docs/design/architecture.md
├── crates/ox-tui/                          # The `ox-tui` library crate with the terminal UI, mapped in docs/design/architecture.md
├── docs/                                   # User guides, conventions, design specs, and research notes
├── Formula/                                # Homebrew formula, served from this repo as a tap
├── licenses/                               # Licenses for bundled syntax definitions and themes
└── scripts/                                # Homebrew formula updater run by the Release workflow
```

## Read Before You Edit

| When the change touches                              | Read                                                                               |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Rust code                                            | [Rust](docs/conventions/rust.md) and the [module map](docs/design/architecture.md) |
| Tests                                                | [Testing](docs/conventions/testing.md)                                             |
| Comments, docs, or text that users or the model read | [Writing](docs/conventions/writing.md)                                             |
| Commits, branches, PRs, or issues                    | [GitHub](docs/conventions/github.md)                                               |
| The dev shell, hooks, toolchain, CI, or config files | [Development](docs/conventions/development.md)                                     |
| A release                                            | [RELEASING.md](RELEASING.md)                                                       |

## Rules for Every Change

- Run the pre-commit hooks on every commit. In each new checkout or worktree, run `nix develop -c true` and `pnpm install` once to provide the generated config and Node tools the hooks need. Never bypass the hooks with `--no-verify` or `PRE_COMMIT_ALLOW_NO_CONFIG`.
- Run the [verification](#verification) block before review.
- Add new words to `.cspell/words.txt` in alphabetical order. `cspell` scans Rust sources too, so a new word in a doc comment fails the same way as one in `README.md`.
- Update whatever the change makes stale: the `README.md` Features and Scope sections, the user guides, design docs and the module map, research notes whose follow-ups it resolves, and open issues it resolves or invalidates.
- Design each change to hold up long term. Survey the reference implementations that [Writing](docs/conventions/writing.md#documentation) lists, reason from first principles toward a design better than any of them, and land any refactor the design needs first. Fix a problem in the layer that owns it, so when `ox` misbehaves in some environment, fix `ox` and leave the environment alone. Never ship a stopgap labeled temporary.
- When a change or review exposes a gap or inconsistency in the conventions, settle the standard from first principles, record it in the convention doc that owns the area, and bring existing code and docs up to it in a scoped refactor PR.
- Issues, PRs, and their comments link only public material. Never reference gitignored or local material, such as working notes, local paths, or private repositories.

## Verification

Run after implementation and before review:

```bash
cargo fmt --all --check                               # Check formatting
cargo build                                           # Build
cargo clippy --all-targets -- -D warnings             # Lint (pedantic, zero warnings)
cargo clippy -- -D warnings                           # Lint without test targets or their fixtures
cargo doc --no-deps                                   # Check rustdoc lints
cargo llvm-cov                                        # Run tests and check coverage
cargo deny check licenses advisories sources bans     # Check dependencies when they change
pnpm format                                           # Check JSON, Markdown, and YAML formatting
pnpm lint                                             # Lint Markdown
pnpm spellcheck                                       # Spell check
```

[Development](docs/conventions/development.md#ci) lists which CI job runs each check.

## Code Review

Review the complete change, its affected callers, and the adjacent code it touches against the convention docs. Propose a fix wherever that nearby code could be more concise or elegant, even when the fix is a large refactor. Check in particular:

- Cross-file consistency: parallel types use the same structure, naming, ordering, and derive traits.
- Comment hygiene: one-line docs where one line suffices, and a comment wherever a constraint is non-obvious.
- Visibility no wider than its use.
- Idiomatic Rust: iterators, pattern matching, the type system, ownership, and the standard library.
- Hand-written logic that an established crate already handles.
