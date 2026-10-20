# Rust

Conventions for Rust code under `crates/`. Tooling enforces some style rules, so this page lists those checks first, followed by the conventions reviewers need to check.

## Enforced by Tooling

- The root `Cargo.toml` configures `[workspace.lints]` to forbid `unsafe_code` and enable Clippy's `pedantic` group and the `allow_attributes`, `allow_attributes_without_reason`, `dbg_macro`, `let_underscore_untyped`, `mod_module_files`, `todo`, and `unwrap_used` lints. It allows the pedantic `missing_errors_doc`, `missing_panics_doc`, and `must_use_candidate` lints, which document exported items for callers outside the unpublished workspace. `clippy.toml` allows `unwrap()` in tests, and CI denies all warnings. The root manifest also denies the other rustdoc lints that warn by default, so `cargo doc` fails on broken or redundant links, bare URLs, invalid HTML tags, malformed code blocks, and broken or unused footnotes. It allows `private_intra_doc_links`, since `.cargo/config.toml` makes rustdoc document the private items those links reach.
- These lints require every suppression to use `#[expect(lint, reason = "...")]`, every module with children to use a `foo.rs` file beside its `foo/` directory rather than a `mod.rs`, and every raw string to use as few `#` delimiters as possible (`pedantic`'s `needless_raw_string_hashes`). They also reject `unwrap()` outside tests and untyped `let _ =` statements, so discard a value with `_ = expr`.
- `cargo fmt` uses stable rustfmt, which does not wrap comments or group imports.
- [Development](development.md#pre-commit-hooks) lists the pre-commit hooks.

## Lint Suppression

- An `#[expect]` reason explains why the code triggers the lint today.
- For complexity and size lints such as `too_many_lines`, `cognitive_complexity`, and `too_many_arguments`, extract a helper or bundle parameters that belong together into a struct. Suppress the lint only when the function is irreducibly cohesive, and explain why in the reason.

## Architecture

- The [module map](../design/architecture.md) lists top-level modules by layer. New code imports only from its own layer or lower layers.
- Put functions and types in the modules that reflect their conceptual domains, creating modules as needed. A type or trait shared across modules belongs in the module that owns its contract.
- The TUI, bare REPL (`--no-tui`), and headless mode (`-p`) all drive the same `Engine` through an `EngineHandle`. Implement new agent behavior in the engine or a lower layer and report it through `Event`s, so every frontend receives it and each frontend handles only input and output. [#149](https://github.com/hakula139/oxide-code/issues/149) tracks the behavior that still lives in frontends.

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
- If the user can fix the problem, end the message with a separate sentence giving the recovery step, as in ``Not signed in. Run `/login` to sign in.`` In a chain, that sentence follows the innermost cause, which renders last, as in `invalid OX_STATUS_LINE "a,,b": empty segment between commas. Remove the stray comma.` A short hint may qualify a fragment in parentheses, as in `no sessions to resume in this project (use --all to search every project)`. When such an error wraps a cause, format the cause into the headline so the recovery step stays last.
- Include units with numbers, as in `Retrying in 1 s`.
- Name the variable or store that holds a secret, never its value.

## Visibility

- Use the narrowest visibility that works: private first, then `pub(super)` within the parent module, then `pub(crate)`, then `pub`. Make an item `pub` only when another workspace crate uses it or a `pub` signature names it, and make every module on its path `pub` too, so `pub(crate)` means the item serves its own crate alone. The compiler never reports an unused `pub` item in a library, so a change that removes the last use from another crate also narrows or deletes the item. The binary crate `ox` has no `pub` items.
- Skip scoped forms such as `pub(in super::super)`. When modules outside an item's parent use it, move it into their nearest common module, or into a child of it with `pub(super)`.

## Modules

- Keep each file focused on one primary type or concern. Split it when it starts mixing several.
- Avoid `pub use` re-exports that obscure where items are defined. If a module re-exports some items, re-export all related items so callers can use consistent paths.
- Put helper functions after their callers within each section so the file reads from top to bottom.
- Place new struct fields and enum variants where they fit best semantically.
- Keep `#[cfg(unix)]`, `#[cfg(windows)]`, and `#[cfg(target_os = "macos")]` variants together in the modules they belong to, rather than splitting them into per-platform files.

## Layout

- Separate distinct groups of items with `// ── Section Name ──` banners using U+2500. Tests group by function in child modules, as described in [Testing](testing.md#grouping-and-naming).
- Leave one blank line between top-level items. Closely related one-line `const` or `static` items may be grouped when they share a theme, such as the OAuth client constants. Leave one blank line after the group before unrelated items.
- Leave one blank line before and after each banner.
- In function bodies, use blank lines to separate logical phases such as setup, validation, execution, and result.
- Put a single-line computation directly above its early-return guard, with no blank line between them. For a multi-line `let` binding, such as an async chain or builder, leave a blank line before the guard.
- Order `use` groups as std, external crates, workspace crates (`ox_core::`), then internal paths (`crate::`, `super::`, `self::`, and the file's child modules), with a blank line between groups. rustfmt sorts imports within each group, putting `self` before `super` before `crate`, but does not move imports between groups. Authors and reviewers must maintain the grouping.

## Strings

- Use a raw string (`r"..."`) only when a literal contains characters that would otherwise need escaping, and only when it fits on one line or forms a multiline `indoc!` or `formatdoc!` body. A raw string cannot continue with `\`, so prose that would run past 100 columns, such as a message, a tool description, or the text a test expects, becomes an escaped `\`-continued literal. A single-line machine-format fixture, such as compact JSON or a regex, stays raw at any length.
- Write text that renders as one line as a plain literal when it fits within 100 columns, and otherwise continue it with `\`, starting each continued line one column past the opening quote. Start each sentence on a new continued line. Break a sentence that does not fit at its strongest clause, parenthetical, or phrase boundary, without filling lines to the limit, so an edit reflows only the lines it touches. Keep the space between the joined words before the `\`, since the continuation also drops the next line's leading whitespace. When rustfmt moves a continued literal, it leaves the continuation lines where they were, so realign them by hand.
- Use `indoc!` for literals and `formatdoc!` for formatted text whenever prose or a structured document, such as a prompt, message, Markdown, diff, or file fixture, spans a paragraph break or three or more lines. This applies to production code and tests. Use `writedoc!` with a writer. Indent content with the surrounding code, inline it when used once, and name a constant only when shared or very large. For CRLF protocol text such as an HTTP request, end each line with `\r` and let the source line break supply the `\n`.
- In a multiline `indoc!` or `formatdoc!`, `}` begins its own line at the indentation of the line that opens the macro, which is `"}` when the text ends in a newline. Otherwise the closing `"` stays on the last content line, since a `"` on its own line adds a final newline. rustfmt lays out a multiline `writedoc!(writer, "...")` like a function call, so the writer, the literal, and the closing `)` each start their own line.
- In `formatdoc!` and `writedoc!`, capture each value inline by name, as in `{name}`, the inline style Clippy's `uninlined_format_args` enforces for `format!`. Bind a value that is not a plain identifier, such as `body.len() + 1`, to a local named for its placeholder before the call, so the template reads on its own.
- Inside `indoc!` and `formatdoc!`, keep each output line on one source line, even past 100 columns. When rustfmt moves such a call, it can re-indent a body line that ends in a `\` continuation without the lines around it, which changes the text.
- Keep `\n` escapes for separators, separators around a single line, a single trailing newline, tiny non-prose fixtures, and tests or operations concerned with the newlines themselves. One- or two-line fixtures without paragraph breaks may stay inline when clear. Keep content whose meaning depends on trailing spaces or whitespace-only lines in an escaped literal, since the trailing-whitespace hook strips them.
- Build an interpolated list or section without a trailing newline, joining its items with `join`, so the template that interpolates it owns the line breaks around it.
- Build a shared envelope, such as the `<system-reminder>` block, through one owning helper that wraps each body, so its delimiters are spelled out in one place.
- Convert a literal to a `String` with `.to_owned()` and keep the layout rustfmt gives it. Don't wrap the literal in `String::from` or bind it to a local only to change that layout.
- Keep each clap flag's `///` help to one sentence, since clap drops the final period of a flag's help.

## Dependencies

- Pin each dependency version in the root `Cargo.toml` under `[workspace.dependencies]`, then reference it from member crates with `dep.workspace = true`.
- Add dependencies to the workspace only when a change first requires them, and prefer crates with few transitive dependencies.
- Declare platform-specific dependencies in the crate's `Cargo.toml` under `[target.'cfg(unix)'.dependencies]` or `[target.'cfg(target_os = "macos")'.dependencies]`.
