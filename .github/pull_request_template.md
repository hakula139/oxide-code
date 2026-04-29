<!-- markdownlint-disable-next-line first-line-heading -->
## Summary

<!-- Prose intro: what this PR does and why, in 1-3 sentences.
     Optional follow-up bullets, one per logical change, when prose alone
     doesn't carry the diff. Trivial PRs can skip the bullets entirely. -->

## Design decisions

<!-- Optional. Use for non-trivial PRs where the reviewer benefits from
     seeing the tradeoffs behind the chosen approach (alternatives
     rejected, invariants preserved, intentional omissions). One bullet
     per decision, lead with the choice in bold. Skip the section when
     the change is mechanical. -->

## Changes

<!-- Per-file breakdown. Remove for trivial PRs.
     Drop the `crates/<crate>/src/` prefix on crate sources (e.g. `tool/glob.rs`,
     not `crates/oxide-code/src/tool/glob.rs`). Keep the full path for
     repo-root files (`CLAUDE.md`, `Cargo.toml`, `docs/...`, `.cspell/...`). -->

| File | Description |
| ---- | ----------- |
|      |             |

## Test plan

- [ ] `cargo build` compiles cleanly
- [ ] `cargo clippy --all-targets -- -D warnings` — zero warnings
- [ ] `cargo test` — N tests pass
- [ ] `cargo llvm-cov --ignore-filename-regex 'main\.rs'` — N% line coverage
