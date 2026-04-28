<!-- markdownlint-disable-next-line first-line-heading -->
## Summary

<!-- Prose intro: what this PR does and why, in 1-3 sentences. -->

<!-- Then one bullet per logical change. -->

-

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
