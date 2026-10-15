# AGENTS.md: oxide-code

oxide-code is a terminal-based AI coding assistant written in Rust, inspired by [Claude Code](https://code.claude.com/docs). Its binary, `ox`, talks to LLM APIs to help developers with software engineering tasks from the terminal.

This file holds the rules for every change and points to the conventions for each area. `README.md` covers features and scope for users, [`docs/`](docs/README.md) holds the user guides, conventions, design specs, and research notes, and open GitHub issues track planned work.

```bash
ox                                          # Start an interactive TUI session
ox --no-tui                                 # Bare REPL on stdin and stdout
ox -p "<prompt>"                            # Headless: run one turn and print the reply
```

## Project Layout

```text
.
├── .agents/                                # Agent skills (release procedure)
├── .cspell/                                # Project spelling dictionary
├── .github/                                # CI and release workflows, plus the shared Nix setup action
├── about.hbs                               # Plain-text third-party notice template
├── about.toml                              # Release dependency license selection
├── crates/oxide-code/                      # The `ox` crate, mapped in docs/design/architecture.md
├── deny.toml                               # Dependency license, advisory, source, and ban policy
├── docs/                                   # User guides, conventions, design specs, and research notes
├── Formula/                                # Homebrew formula, served from this repo as a tap
├── licenses/                               # Licenses for bundled syntax definitions and themes
└── scripts/                                # Release helpers and pre-commit checks
```

## Read Before You Edit

| When the change touches                              | Read                                                                               |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Rust code                                            | [Rust](docs/conventions/rust.md) and the [module map](docs/design/architecture.md) |
| Tests                                                | [Testing](docs/conventions/testing.md)                                             |
| Comments, docs, or text that users or the model read | [Writing](docs/conventions/writing.md)                                             |
| Commits, branches, PRs, or issues                    | [GitHub](docs/conventions/github.md)                                               |
| The dev shell, hooks, toolchain, or CI               | [Development](docs/conventions/development.md)                                     |
| A release                                            | [RELEASING.md](RELEASING.md)                                                       |

## Rules for Every Change

- Run the pre-commit hooks on every commit. In each new checkout or worktree, run `nix develop -c true` and `pnpm install` once to provide the generated config and Node tools the hooks need. Never bypass the hooks with `--no-verify` or `PRE_COMMIT_ALLOW_NO_CONFIG`.
- Run the [verification](#verification) block before review.
- Add new words to `.cspell/words.txt` in alphabetical order. `cspell` scans Rust sources too, so a new word in a doc comment fails the same way as one in `README.md`.
- Update whatever the change makes stale: the `README.md` Features and Scope sections, the user guides, design docs and the module map, research notes whose follow-ups it resolves, and open issues it resolves or invalidates.
- Issues, PRs, and their comments link only public material. Never reference gitignored or local material, such as working notes, local paths, or private repositories.

## Verification

Run after implementation and before review:

```bash
cargo fmt --all --check                            # Check formatting
cargo build                                        # Build
cargo clippy --all-targets -- -D warnings          # Lint (pedantic, zero warnings)
cargo test                                         # Run tests
cargo llvm-cov                                     # Check test coverage
pnpm format                                        # Check Markdown, TOML, JSON, and YAML formatting
pnpm lint                                          # Lint Markdown
pnpm spellcheck                                    # Spell check
```

[Development](docs/conventions/development.md#ci) lists which CI job runs each check.

## Code Review

Review the complete change and its affected callers against the convention docs, and check in particular:

- Cross-file consistency: parallel types use the same structure, naming, ordering, and derive traits.
- Comment hygiene: one-line docs where one line suffices, and a comment wherever a constraint is non-obvious.
- Visibility no wider than its use.
- Idiomatic Rust: iterators, pattern matching, the type system, ownership, and the standard library.
- Hand-written logic that an established crate already handles.
