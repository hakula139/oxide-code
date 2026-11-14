# AGENTS.md: oxide-code

oxide-code is a terminal-based AI coding assistant written in Rust, inspired by [Claude Code](https://code.claude.com/docs). Its binary, `ox`, talks to LLM APIs to help developers with software engineering tasks from the terminal.

This file adds oxide-code's requirements to the [shared instructions](https://github.com/hakula139/nixos-config/blob/main/home/modules/llm-assistants/instructions/shared.md). Open GitHub issues track planned work.

## Project Layout

The tree lists only the top-level entries whose names leave their purpose unclear. The [module map](docs/design/architecture.md) covers each crate's modules.

```text
.
├── about.toml             # Release dependency license selection
├── crates/ox/             # The `ox` binary with the CLI and headless frontend
├── crates/ox-core/        # The agent core library
├── crates/ox-tui/         # The terminal UI library
├── docs/                  # User guides, conventions, design specs, and research notes
├── Formula/               # Homebrew formula, served from this repo as a tap
├── licenses/              # Third-party notice templates and bundled asset licenses
├── nix/                   # Nix package and Home Manager module with its tests
└── scripts/               # Homebrew formula renderer and Ruby template
```

## Read Before You Edit

| When the change touches                                   | Read                                                                               |
| --------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Rust code                                                 | [Rust](docs/conventions/rust.md) and the [module map](docs/design/architecture.md) |
| Tests                                                     | [Testing](docs/conventions/testing.md)                                             |
| Comments, docs, or text that users or the model read      | [Writing](docs/conventions/writing.md)                                             |
| Commits, branches, PRs, or issues                         | [GitHub](docs/conventions/github.md)                                               |
| The dev shell, hooks, toolchain, CI, Nix, or config files | [Development](docs/conventions/development.md)                                     |
| A release                                                 | [RELEASING.md](RELEASING.md)                                                       |

## Rules for Every Change

- Run the pre-commit hooks on every commit. In each new checkout or worktree, run `nix develop -c true` once to install the hooks. Never bypass the hooks with `--no-verify` or `PRE_COMMIT_ALLOW_NO_CONFIG`.
- Run the [verification](#verification) block before review.
- Add new words, including those in Rust sources, to `.cspell/words.txt` in alphabetical order.
- Update whatever the change makes stale: the `README.md` Features and Scope sections, the trust model in `SECURITY.md`, the user guides, design docs and the module map, research surveys whose open questions it settles, open issues it resolves or invalidates, and the Home Manager module in `nix/home-manager.nix` when `ox` reads a new file from the user config directory.
- Survey the reference implementations that [Writing](docs/conventions/writing.md#documentation) lists when designing a change.
- Settle design choices, including the plan for a new subsystem, without waiting for the maintainer's approval. Record each uncertain choice, with the option taken, in the main worktree's gitignored `.agents/plans/questions.md`, raise them together in the next reply to the maintainer, and delete each entry once the maintainer answers it.

## Verification

Run after implementation and before review:

```bash
nix develop -c run-hooks   # Run the hooks, including rustfmt and Clippy with all targets
cargo build                # Build
cargo clippy               # Lint without test targets or their fixtures
cargo doc --no-deps        # Check rustdoc lints
cargo llvm-cov             # Run tests and check coverage
cargo deny check           # Check dependencies when they change
```

[CI](docs/conventions/development.md#ci) lists the job that runs each check.

## Code Review

Use the [shared review procedure](https://github.com/hakula139/plugins/blob/main/plugins/hakula-workflow/skills/review/SKILL.md) and the convention docs linked above.
