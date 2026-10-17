# Rust

Conventions for Rust code under `crates/`. Tooling enforces some style rules, so this page lists those checks first, followed by the conventions reviewers need to check.

## Enforced by Tooling

- The root `Cargo.toml` configures `[workspace.lints]` to forbid `unsafe_code` and enable Clippy's `pedantic` group and the `allow_attributes`, `allow_attributes_without_reason`, `dbg_macro`, `let_underscore_untyped`, `mod_module_files`, `todo`, and `unwrap_used` lints. `clippy.toml` allows `unwrap()` in tests, and CI denies all warnings.
- These lints require every suppression to use `#[expect(lint, reason = "...")]`, every module with children to use a `foo.rs` file beside its `foo/` directory rather than a `mod.rs`, and every raw string to use as few `#` delimiters as possible (`pedantic`'s `needless_raw_string_hashes`). They also reject `unwrap()` outside tests and untyped `let _ =` statements, so discard a value with `_ = expr`.
- `cargo fmt` uses stable rustfmt, which does not wrap comments or group imports.
- [Development](development.md#pre-commit-hooks) lists the pre-commit hooks, including the check for module docs in every `.rs` file.

## Lint Suppression

- An `#[expect]` reason explains why the code triggers the lint today.
- For complexity and size lints such as `too_many_lines`, `cognitive_complexity`, and `too_many_arguments`, extract a helper or bundle parameters that belong together into a struct. Suppress the lint only when the function is irreducibly cohesive, and explain why in the reason.

## Architecture

- The [module map](../design/architecture.md) lists top-level modules by layer. New code imports only from its own layer or lower layers.
- Put functions and types in the modules that reflect their conceptual domains, creating modules as needed. A type or trait shared across modules belongs in the module that owns its contract.
- The TUI, bare REPL (`--no-tui`), and headless mode (`-p`) all drive the same `Engine` through an `EngineHandle`. Implement new agent behavior in the engine or a lower layer and report it through `Event`s, so every frontend receives it and each frontend handles only input and output. [#149](https://github.com/hakula139/oxide-code/issues/149) and [#307](https://github.com/hakula139/oxide-code/issues/307) track the behavior that still lives in frontends.

## Traits

- Put per-instance metadata, such as a display name, icon, or input summary, in a trait method so each implementation can be added by editing only its own file.

## Errors

- Application code returns `anyhow::Result` and adds `.context()` with an actionable message.
- Derive `thiserror::Error` only when callers match on its variants.
- Errors from tools and slash commands that are intended for the model or user use `Result<T, String>` and contain a complete message.
- Reserve `expect()` for calls that cannot fail, and state the invariant in the message, as in `.expect("vendored mocha.toml must parse")`.

### Error Messages

`anyhow` formats a chain as `headline: cause` with `{:#}` and puts the outermost headline first in a startup error, so each message in the chain should read as one clause of that sentence.

- Start with a headline that says what failed and identifies the variable, key, file, or value involved, as in `invalid OX_MAX_TOKENS "32k": expected a whole number from 1 to 4294967295`. Quote user-supplied text with `{:?}`.
- A message that composes into a chain is a fragment of that sentence, as the example above is. [Writing](writing.md#prose) owns the casing and punctuation of fragments and of messages shown on their own.
- Where a surface shows a chain on its own, give it a capitalized headline at the display site, as in `Rule rejected: {e:#}`, so the fragments inside stay lowercase. `main` prints a startup chain after `Error:`, so its outermost headline is lowercase too.
- Explain what is accepted or what went wrong in the user's terms. A raw library message such as `invalid digit found in string` does not explain the accepted range.
- If the user can fix the problem, end the message with a separate sentence giving the recovery step, as in `Not signed in. Run /login to sign in.` In a chain, that sentence follows the innermost cause, which renders last, as in `invalid OX_STATUS_LINE "a,,b": empty segment between commas. Remove the stray comma.` A short hint may qualify a fragment in parentheses, as in `no sessions to resume in this project (use --all to search every project)`. When such an error wraps a cause, format the cause into the headline so the recovery step stays last.
- Include units with numbers, as in `Retrying in 1 s`.
- Name the variable or store that holds a secret, never its value.

## Visibility

- Use the narrowest visibility that works: private first, then `pub(super)` within the parent module, then `pub(crate)`. Because the crate is a binary, it has no `pub` items.

## Modules

- Keep each file focused on one primary type or concern. Split it when it starts mixing several.
- Avoid `pub use` re-exports that obscure where items are defined. If a module re-exports some items, re-export all related items so callers can use consistent paths.
- Put helper functions after their callers within each section so the file reads from top to bottom.
- Place new struct fields and enum variants where they fit best semantically.
- Keep `#[cfg(unix)]`, `#[cfg(windows)]`, and `#[cfg(target_os = "macos")]` variants together in the modules they belong to, rather than splitting them into per-platform files.

## Layout

- Separate distinct groups of items with `// ── Section Name ──` banners using U+2500. Group tests by function under the same banners, as described in [Testing](testing.md#grouping-and-naming).
- Leave one blank line between top-level items. Closely related one-line `const` or `static` items may be grouped when they share a theme, such as the OAuth client constants. Leave one blank line after the group before unrelated items.
- Leave one blank line before and after each banner, including in test modules. In test modules, put the first banner after the `use super::*` block, with a blank line between them.
- In function bodies, use blank lines to separate logical phases such as setup, validation, execution, and result.
- Put a single-line computation directly above its early-return guard, with no blank line between them. For a multi-line `let` binding, such as an async chain or builder, leave a blank line before the guard.
- Order `use` groups as std, external crates, then internal paths (`crate::`, `super::`, `self::`, and the file's child modules), with a blank line between groups. rustfmt sorts imports within each group, putting `self` before `super` before `crate`, but does not move imports between groups. Authors and reviewers must maintain the grouping.

## Strings

- Prefer raw strings (`r"..."`) when a literal contains characters that would otherwise need escaping.
- Use `indoc!` for literals and `formatdoc!` for formatted text whenever prose or a structured document, such as a prompt, message, Markdown, diff, or file fixture, spans a paragraph break or three or more lines. This applies to production code and tests. Use `writedoc!` with a writer. Indent content with the surrounding code, inline it when used once, and name a constant only when shared or very large.
- Keep `\n` escapes for separators, separators around a single line, a single trailing newline, tiny non-prose fixtures, and tests or operations concerned with the newlines themselves. One- or two-line fixtures without paragraph breaks may stay inline when clear. Keep content whose meaning depends on trailing spaces or whitespace-only lines in an escaped literal, since the trailing-whitespace hook strips them.
- Keep each clap flag's `///` help to one sentence, since clap drops the final period of a flag's help.

## Dependencies

- Pin each dependency version in the root `Cargo.toml` under `[workspace.dependencies]`, then reference it from member crates with `dep.workspace = true`.
- Add dependencies to the workspace only when a change first requires them, and prefer crates with few transitive dependencies.
- Declare platform-specific dependencies in the crate's `Cargo.toml` under `[target.'cfg(unix)'.dependencies]` or `[target.'cfg(target_os = "macos")'.dependencies]`.
