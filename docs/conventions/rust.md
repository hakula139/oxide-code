# Rust

Follow the shared [Rust](https://github.com/hakulab/plugins/blob/main/plugins/hakula-languages/skills/rust/SKILL.md) and [code layout](https://github.com/hakulab/plugins/blob/main/plugins/hakula-workflow/skills/code-layout/SKILL.md) conventions for Rust sources and Cargo manifests. This page adds oxide-code's contracts and tooling requirements.

## Enforced by Tooling

- `[workspace.lints]` in the root `Cargo.toml` sets every crate's lints, and `clippy.toml` allows `unwrap()` in tests.
- `.cargo/config.toml` denies warnings across builds, tests, Clippy, and `cargo doc` (including rustdoc link and markup lints). Because a denied warning stops dependent crates from compiling, set `CARGO_BUILD_WARNINGS=warn` to see every error at once. The MSRV job's older cargo and the Nix build (which omits `.cargo/`) ignore this setting.
- The lints reject `unwrap()` outside tests.
- Stable rustfmt neither wraps comments nor groups imports.

## Lint Suppression

- The [Windows file ID query](../../crates/ox-core/src/util/fs.rs), [Unix signal-disposition query](../../crates/ox/src/driver.rs), and [Unix signal test's child-disposition reset](../../crates/ox/tests/cli/signals.rs) may use a function-scoped `#[expect(unsafe_code, reason = "...")]`. Keep raw handles and pointers inside those boundaries, document ownership and buffer invariants, and expose safe operations. Other code remains subject to the workspace's `unsafe_code` denial.
- For complexity and size lints such as `too_many_lines`, `cognitive_complexity`, and `too_many_arguments`, extract a helper or bundle parameters that belong together into a struct. Suppress the lint only when the function is irreducibly cohesive, and explain why in the reason.

## Architecture

- The [module map](../design/architecture.md) lists top-level modules by layer. New code imports only from its own layer or lower layers.
- Put functions and types in the modules that reflect their conceptual domains, creating modules as needed. A type or trait shared across modules belongs in the module that owns its contract.
- The TUI and headless mode (`-p`) drive the same `Engine` through an `EngineHandle`. Implement new agent behavior in the engine or a lower layer and report it through `Event`s, so every frontend receives it and each frontend handles only input and output.

## Traits

- Put per-instance metadata, such as a display name, icon, or input summary, in a trait method so each implementation can be added by editing only its own file.

## Errors

- Application code returns `anyhow::Result` and adds `.context()` with an actionable message.
- Errors from tools and slash commands that are intended for the model or user use `Result<T, String>` and contain a complete message.

### Error Messages

`anyhow` formats a chain as `headline: cause` with `{:#}` and puts the outermost headline first in a startup error, so each message in the chain should read as one clause of that sentence. [Writing](writing.md#messages) covers the wording, casing, and punctuation of every message, and these rules add the headline form and how a chain composes.

- Start with a headline that says what failed and identifies the variable, key, file, or value involved. A failed operation reads `failed to <verb> ...`, as in `failed to read the current working directory`, and rejected input names the problem first, as in `invalid OX_MAX_TOKENS "32k": expected a whole number from 1 to 4294967295`. Quote user-supplied text with `{:?}`.
- A message that composes into a chain is a fragment of that sentence, as the examples above are.
- Where a surface shows a chain on its own, give it a capitalized headline at the display site, as in `Rule rejected: {e:#}`, so the fragments inside stay lowercase. `main` prints a startup chain after `Error:`, so its outermost headline is lowercase too.
- In a chain, the recovery step follows the innermost cause, which renders last, as in `invalid OX_STATUS_LINE "a,,b": empty segment between commas. Remove the stray comma.` When such an error wraps a cause, format the cause into the headline so the recovery step stays last.

## Visibility

- Use `pub(crate)` in direct children of a crate root (where `pub(super)` already reaches the whole crate). Use `pub` only when another workspace crate uses the item or a `pub` signature exposes it, making every module on its path `pub` too so `pub(crate)` strictly means crate-internal. Because the compiler does not flag unused `pub` items in libraries, narrow or delete an item when removing its last cross-crate use. The binary crate `ox` has no `pub` items.
- Skip scoped forms such as `pub(in super::super)`. When modules outside an item's parent use it, move it into their nearest common module, or into a child of it with `pub(super)`.

## Modules

- Keep each file focused on one primary type or concern. Split it when it starts mixing several.
- A parent module may `pub use` items from private child modules to present a flat facade, re-exporting every related item so callers see one consistent path. Avoid `pub use` re-exports across public modules that obscure where items are defined.
- Put helper functions after their callers within each section so the file reads from top to bottom.
- Keep `#[cfg(unix)]`, `#[cfg(windows)]`, and `#[cfg(target_os = "macos")]` variants together in the modules they belong to.

## Layout

- List derives in the order `Debug`, `Clone`, `Copy`, `Default`, `PartialEq`, `Eq`, `PartialOrd`, `Ord`, `Hash`, followed by `Serialize`, `Deserialize`, and then other crates' derives, such as `thiserror::Error`. Give parallel types the same set.
- Separate distinct groups of items with `// ── Section Name ──` banners using U+2500. Tests group by function in child modules, as described in [Testing](testing.md#grouping-and-naming).
- Leave one blank line between top-level items. Closely related one-line `const` or `static` items may be grouped when they share a theme, with one blank line after the group before unrelated items.
- Leave one blank line before and after each banner.
- Put a single-line computation directly above its early-return guard, with no blank line between them. For a multi-line `let` binding, leave a blank line before the guard.
- Open a module with its `//!` doc, then its `mod` declarations, its re-exports at any visibility as a group of their own, and its `use` groups.
- rustfmt sorts imports within each group, putting `self` before `super` before `crate`, but does not move imports between groups. Authors and reviewers must maintain the grouping.
- Write `use` declarations, re-exports included, with `self::` for a module's children and `super::` for its parent and siblings, following `config/*`. Use `crate::` for more distant modules and test modules, which already glob-import their parent.

## Strings

- When continuing an escaped literal with `\`, keep the space between joined words before the `\`, since the continuation drops the next line's leading whitespace. Start each continued line one column past the opening quote and realign it by hand when rustfmt moves the literal.
- Use the multiline text macros whenever prose or a structured document spans a paragraph break or three or more lines. This applies to production code and tests. Inline text used once, and name a constant only when shared or very large. For CRLF protocol text such as an HTTP request, end each line with `\r` and let the source line break supply the `\n`.
- In a multiline `indoc!` or `formatdoc!`, `}` begins its own line at the indentation of the line that opens the macro, which is `"}` when the text ends in a newline. rustfmt lays out a multiline `writedoc!(writer, "...")` like a function call, so the writer, the literal, and the closing `)` each start their own line.
- Keep `\n` escapes for separators, separators around a single line, a single trailing newline, tiny non-prose fixtures, and tests or operations concerned with the newlines themselves. One- or two-line fixtures without paragraph breaks may stay inline when clear. Keep content whose meaning depends on trailing spaces or whitespace-only lines in an escaped literal, since the trailing-whitespace hook strips them.
- Build an interpolated list or section without a trailing newline, joining its items with `join`, so the template that interpolates it owns the line breaks around it.
- Build a shared envelope, such as the `<system-reminder>` block, through one owning helper that wraps each body, so its delimiters are spelled out in one place.
- Convert a literal to a `String` with `.to_owned()` and keep the layout rustfmt gives it. Don't wrap the literal in `String::from` or bind it to a local only to change that layout.

## Dependencies

- Add dependencies to the workspace only when a change first requires them, and prefer crates with few transitive dependencies.
