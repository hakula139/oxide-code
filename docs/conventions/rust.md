# Rust

Conventions for Rust code under `crates/`, starting with what tooling enforces.

## Enforced by Tooling

- `[workspace.lints]` in the root `Cargo.toml` sets every crate's lints, and `clippy.toml` allows `unwrap()` in tests.
- `.cargo/config.toml` denies warnings, so a warning fails builds, tests, Clippy, and `cargo doc`, including rustdoc's link and markup warnings. A denied warning also stops dependent crates from compiling, so set `CARGO_BUILD_WARNINGS=warn` to see every error at once. The MSRV job's older cargo and the Nix build, which leaves out `.cargo/`, don't apply the setting.
- The lints require `#[expect(lint, reason = "...")]` for every suppression, a `foo.rs` beside `foo/` in place of `mod.rs`, and the fewest `#` delimiters on a raw string. They reject `unwrap()` outside tests and untyped `let _ =`, so discard a value with `_ = expr`.
- Stable rustfmt neither wraps comments nor groups imports.

## Lint Suppression

- An `#[expect]` reason explains why the code triggers the lint today.
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
- Derive `thiserror::Error` only when callers match on its variants.
- Errors from tools and slash commands that are intended for the model or user use `Result<T, String>` and contain a complete message.
- Reserve `expect()` for calls that cannot fail, and state the invariant in the message, as in `.expect("vendored mocha.toml must parse")`.

### Error Messages

`anyhow` formats a chain as `headline: cause` with `{:#}` and puts the outermost headline first in a startup error, so each message in the chain should read as one clause of that sentence. [Writing](writing.md#messages) covers the wording, casing, and punctuation of every message, and these rules add the headline form and how a chain composes.

- Start with a headline that says what failed and identifies the variable, key, file, or value involved. A failed operation reads `failed to <verb> ...`, as in `failed to read the current working directory`, and rejected input names the problem first, as in `invalid OX_MAX_TOKENS "32k": expected a whole number from 1 to 4294967295`. Quote user-supplied text with `{:?}`.
- A message that composes into a chain is a fragment of that sentence, as the examples above are.
- Where a surface shows a chain on its own, give it a capitalized headline at the display site, as in `Rule rejected: {e:#}`, so the fragments inside stay lowercase. `main` prints a startup chain after `Error:`, so its outermost headline is lowercase too.
- In a chain, the recovery step follows the innermost cause, which renders last, as in `invalid OX_STATUS_LINE "a,,b": empty segment between commas. Remove the stray comma.` When such an error wraps a cause, format the cause into the headline so the recovery step stays last.

### Log Events

Keep each `tracing` message a constant phrase and pass variable data as fields, so that log filters and readers can match one event across occurrences. Record an error chain as `error = %format_args!("{e:#}")` and a `Display` value as `name = %value`, as in `warn!(?delay, retry, error = %format_args!("{error:#}"), "retrying Anthropic request")`.

## Visibility

- Use the narrowest visibility that works: private first, then `pub(super)` within the parent module, then `pub(crate)`, then `pub`. In a direct child of a crate root, where `pub(super)` already reaches the whole crate, write `pub(crate)`. Make an item `pub` only when another workspace crate uses it or a `pub` signature names it, and make every module on its path `pub` too, so `pub(crate)` means the item serves its own crate alone. The compiler never reports an unused `pub` item in a library, so a change that removes the last use from another crate also narrows or deletes the item. The binary crate `ox` has no `pub` items.
- Skip scoped forms such as `pub(in super::super)`. When modules outside an item's parent use it, move it into their nearest common module, or into a child of it with `pub(super)`.

## Modules

- Keep each file focused on one primary type or concern. Split it when it starts mixing several.
- Avoid `pub use` re-exports that obscure where items are defined. If a module re-exports some items, re-export all related items so callers can use consistent paths.
- Put helper functions after their callers within each section so the file reads from top to bottom.
- Keep `#[cfg(unix)]`, `#[cfg(windows)]`, and `#[cfg(target_os = "macos")]` variants together in the modules they belong to, rather than splitting them into per-platform files.

## Layout

Follow the shared [code layout](https://github.com/hakula139/plugins/blob/main/plugins/hakula-workflow/skills/code-layout/SKILL.md) conventions, with these Rust additions:

- List derives in the order `Debug`, `Clone`, `Copy`, `Default`, `PartialEq`, `Eq`, `PartialOrd`, `Ord`, `Hash`, followed by `Serialize`, `Deserialize`, and then other crates' derives, such as `thiserror::Error`. Give parallel types the same set.
- Separate distinct groups of items with `// ── Section Name ──` banners using U+2500. Tests group by function in child modules, as described in [Testing](testing.md#grouping-and-naming).
- Leave one blank line between top-level items. Closely related one-line `const` or `static` items may be grouped when they share a theme, such as the OAuth client constants. Leave one blank line after the group before unrelated items.
- Leave one blank line before and after each banner.
- Put a single-line computation directly above its early-return guard, with no blank line between them. For a multi-line `let` binding, such as an async chain or builder, leave a blank line before the guard.
- Open a module with its `//!` doc, then its `mod` declarations, its re-exports at any visibility as a group of their own, and its `use` groups.
- Order `use` groups as std, external crates, workspace crates (`ox_core::`, `ox_tui::`), then internal paths (`crate::`, `super::`, and `self::`), with a blank line between groups. rustfmt sorts imports within each group, putting `self` before `super` before `crate`, but does not move imports between groups. Authors and reviewers must maintain the grouping.
- Write `use` declarations, re-exports included, with `self::` for a module's children and `super::` for its parent and siblings, following `config/*`. Use `crate::` for more distant modules and test modules, which already glob-import their parent.
- Import a trait as `Trait as _` when its methods are the only things used, as with `std::fmt::Write as _` and `PermissionsExt as _`. Import it by name only when code refers to the trait itself, such as in a bound. Clippy's `unused_trait_names` lint enforces this.

## Serde

- Omit `#[serde(default)]` on an `Option` field, since derived `Deserialize` reads a missing key as `None` unless the field sets `with` or `deserialize_with`.

## Strings

- Use a raw string (`r"..."`) only when a literal contains characters that would otherwise need escaping, and only when it fits on one line or forms a multiline `indoc!` or `formatdoc!` body. A raw string cannot continue with `\`, so prose that would run past 100 columns, such as a message, a tool description, or the text a test expects, becomes an escaped `\`-continued literal. A single-line machine-format fixture, such as compact JSON or a regex, stays raw at any length.
- Write text that renders as one line as a plain literal when it fits within 100 columns, and otherwise continue it with `\`, starting each continued line one column past the opening quote and breaking lines as [Writing](writing.md#comments) describes. Keep the space between the joined words before the `\`, since the continuation also drops the next line's leading whitespace. When rustfmt moves a continued literal, it leaves the continuation lines where they were, so realign them by hand. A rendered terminal row that a test compares line by line stays on one source line at any length, since the test checks its column alignment.
- Use `indoc!` for literals and `formatdoc!` for formatted text whenever prose or a structured document, such as a prompt, message, Markdown, diff, or file fixture, spans a paragraph break or three or more lines. This applies to production code and tests. Use `writedoc!` with a writer. Indent content with the surrounding code, inline it when used once, and name a constant only when shared or very large. For CRLF protocol text such as an HTTP request, end each line with `\r` and let the source line break supply the `\n`.
- In a multiline `indoc!` or `formatdoc!`, `}` begins its own line at the indentation of the line that opens the macro, which is `"}` when the text ends in a newline. Otherwise the closing `"` stays on the last content line, since a `"` on its own line adds a final newline. rustfmt lays out a multiline `writedoc!(writer, "...")` like a function call, so the writer, the literal, and the closing `)` each start their own line.
- In `formatdoc!` and `writedoc!`, capture each value inline by name, as in `{name}`, the inline style Clippy's `uninlined_format_args` enforces for `format!`. Bind a value that is not a plain identifier, such as `body.len() + 1`, to a local named for its placeholder before the call, so the template reads on its own.
- Inside `indoc!` and `formatdoc!`, keep each output line on one source line, even past 100 columns. When rustfmt moves such a call, it can re-indent a body line that ends in a `\` continuation without the lines around it, which changes the text.
- Keep `\n` escapes for separators, separators around a single line, a single trailing newline, tiny non-prose fixtures, and tests or operations concerned with the newlines themselves. One- or two-line fixtures without paragraph breaks may stay inline when clear. Keep content whose meaning depends on trailing spaces or whitespace-only lines in an escaped literal, since the trailing-whitespace hook strips them.
- Build an interpolated list or section without a trailing newline, joining its items with `join`, so the template that interpolates it owns the line breaks around it.
- Build a shared envelope, such as the `<system-reminder>` block, through one owning helper that wraps each body, so its delimiters are spelled out in one place.
- Convert a literal to a `String` with `.to_owned()` and keep the layout rustfmt gives it. Don't wrap the literal in `String::from` or bind it to a local only to change that layout.

## Dependencies

- Pin each dependency version in the root `Cargo.toml` under `[workspace.dependencies]`, then reference it from member crates with `dep.workspace = true`.
- Add dependencies to the workspace only when a change first requires them, and prefer crates with few transitive dependencies.
- Declare a platform-specific dependency in the crate's `Cargo.toml` under the narrowest `[target.'cfg(...)'.dependencies]` table that covers every use.
