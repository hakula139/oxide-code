# Writing

Conventions for comments, docstrings, documentation, commit and PR text, and strings read by users or the model.

## Comments

- Default to no comments. Add one only when a future maintainer needs a non-obvious constraint that the code and names cannot convey.
- Do not restate what the code does, describe its layout or order, narrate the change or refer to the task behind it, or record provenance such as "split out so tests can reach it". Commit messages and PR descriptions preserve that history.
- Explain cryptic flags, literals, and API quirks, and label what a non-obvious regex matches.
- When editing, delete comments that repeat the code and correct any that have become misleading. Before deleting a comment that records a constraint, check whether the constraint still applies.
- Delete commented-out code because version control preserves it.
- Write `//` comments as prose. A docstring may use a Markdown list for parallel options or a sequence.
- Wrap comment and docstring prose at 100 columns, including indentation and comment markers. Fill each paragraph naturally, allowing sentences to share a line, and separate paragraphs with a blank comment line. Break early when it improves readability, such as to keep a code span or link together. Reword a paragraph whose last line would contain one short word. Preserve intentional layout in code examples, lists, tables, and diagrams. Apply the same rule to shell, Nix, YAML, and TOML comments, and to prose literals continued by hand. The `comment-width` pre-commit hook checks this limit on every line that starts with `//` in Rust or `#` elsewhere, which includes string, heredoc, and literal-block lines that start that way, and exempts a comment holding only a URL, an `<url>`, or a link reference definition. It skips trailing comments after code.

## Docstrings

- The first `//!` line in each `.rs` file describes the file's responsibility.
- A `///` doc states the contract callers need, such as invariants, units, ownership, or errors. Default to one line, using more only when the contract requires it. Omit docs that merely restate the item name or implementation.
- Write docs in the third person. A doc on a function, or on a variant that names an action, says what it does, though a pure function's doc may name the value it returns. A predicate's doc starts with `Returns whether`, and a boolean field's starts with `Whether`. Other docs say what the item is. A clap flag's doc is its `--help` text, so it gives a command in one sentence, since clap drops the final period of a flag's help.

## Prose

These rules apply to comments, docs, commit messages, PR and issue text, `#[expect]` reasons, and strings shown to users or the model.

- State conclusions directly and match each claim's strength to its evidence. Describe what you checked, and avoid absolute claims such as "bug-free" or "guaranteed".
- Say each thing once, using the fewest words that preserve its meaning. Give messages of the same kind a consistent shape.
- Include units with numbers, as in `Retrying in 1 s`. Label sizes by their base: `KiB`, `MiB`, and `GiB` for powers of 1024, and `kB` and `MB` for powers of 1000. `util::text::format_size` picks the binary unit for a byte count. An exact count stays in bytes.
- Connect related thoughts with a period or a linking word such as `since`, `because`, `while`, `so`, or `but`. Use an em-dash only as a paired parenthetical or to separate a list label from its gloss.
- Avoid semicolons in prose. They are fine in code and literal syntax, and between list items that contain commas.
- Define things by what they are. Add a contrast with what they are not only when omitting it could lead to a materially wrong reading.
- Avoid lists of three or more similarly structured phrases when they add rhythm without supporting the argument.
- Use American spelling in prose and identifiers, such as `behavior`, `catalog`, and `recognize`. Keep the double l in `cancelled` and `cancelling` to match ecosystem names such as tokio-util's `is_cancelled` and `cancelled()`, and GitHub's `CANCELLED` check state.
- Place commas and periods outside closing quotation marks.
- Use straight quotes. Arrows and comparison operators (`→`, `≤`, `≠`) are welcome.
- Put spaces around `/` and arrows when they separate words or phrases, as in `Read / Write` and `low → high`. Keep compound terms such as `I/O` tight.
- Close a delimited block the same way it opens. If an opening tag or fence, such as `<system-reminder>`, appears on its own line, put its closing counterpart on its own line too.

## Messages

These rules apply to errors, warnings, notices, and other strings shown to users. [Rust](rust.md#error-messages) adds how an `anyhow` chain composes them.

- Explain what went wrong or what is accepted in the user's terms. A raw library message such as `invalid digit found in string` does not explain the accepted range.
- When the user can fix the problem, end the message with a separate sentence giving the recovery step, as in ``Not signed in. Run `/login` to sign in.`` A short hint may instead qualify a fragment in parentheses, as in `no sessions to resume in this project (use --all to search every project)`.
- Name the variable or store that holds a secret, never its value.
- A setting shown without a value, as in `/status` and `/config`, reads as plain words in the letter case of the values it replaces, such as `none` and `unbounded`, or `Not signed in` among the capitalized credential labels, without enclosing parentheses. A short reason may follow in parentheses, as in `none (model has no effort levels)` and `off (no threshold)`.
- In strings shown to users and in `#[expect]` reasons, terminal punctuation separates sentences. One-sentence text, such as a status line, label, notice, or error, takes no final period, whether or not it is grammatically complete, and a question keeps its `?`. Text with two or more sentences ends each sentence with terminal punctuation, including the last. Text the model reads, such as prompts, tool descriptions and schemas, tool results, and transcript markers, keeps its punctuation.
- Text composed around variable external content, such as an API payload, an `{e:#}` cause, or a refusal explanation, keeps the inserted text as it arrives and adapts the separator after it to how that text ends, so each sentence ends once. An API error reads `{headline}: {details}. {advice}`, and when the details already end in `.`, `?`, `!`, `...`, the single-character ellipsis U+2026, or a fullwidth `。`, `？`, or `！`, a space alone separates them from the advice. `util::text::join_sentences` applies this separator. Our own literals still carry their punctuation.
- Begin standalone text, such as a notice or slash-command error, with a capital letter. Begin a fragment that forms part of an error chain with a lowercase letter because it continues the headline before its colon. Keep identifiers and proper nouns in their original case wherever they appear, as in a message that starts with `macOS` or `old_string`.

## Markdown

- Put each paragraph on one line without hard wrapping, and separate paragraphs with a blank line.
- Use prose for explanations, bullets for parallel options or requirements, numbered lists for sequences, and tables for comparisons.
- Make a list loose, with a blank line between items, when any item holds a nested list, a second paragraph, or a code block. Keep a list of single-paragraph items tight, even when it is nested in a loose list.
- Align trailing comments in directory trees and similar listings in one column, several spaces beyond the longest entry.
- Write headings in Title Case. Capitalize every word except articles, coordinating conjunctions, and prepositions of four letters or fewer, such as `a`, `and`, `for`, `from`, and `with`, unless the word starts or ends the heading. In a hyphenated compound, capitalize each part except an inner small word, as in `Built-In` and `Fork-on-Conflict`. Keep code spans and lowercase names such as oxide-code and opencode as written. PR, issue, and release bodies follow GitHub's sentence case, so the PR template keeps `## Test plan` and `CHANGELOG.md` keeps `### Breaking changes`.
- Join a modifier to its key or mouse action with `+`, as in Shift+Tab and Ctrl+click, and write approval keys as the uppercase letters the TUI prompt shows. In the README and user guides, also put key names in bold, such as **Esc** and **Ctrl+C**.
- Write "ID" in prose, and keep lowercase `id` for identifiers, flags, and placeholders such as `<id-prefix>`. Write file modes as four octal digits, as in `0600`, and percentages without a space, as in 80%. In the README and user guides, write "working directory" in place of cwd.

## Documentation

- Keep `README.md` focused on users: explain the value, supported features, scope, and usage.
- Use `docs/` to describe current behavior and its rationale. Track planned work, deferred follow-ups, and open design questions in GitHub issues, and link to the relevant issue from the docs.
- Keep every command shown in the docs minimal. When a command would need extra flags or environment variables, move the setting into the tool's config, rely on its default, or add a project script, then document the short form.
- Organize each page around the questions its readers bring. Keep each paragraph focused on one idea, and verify every claim against the code before citing it.
- Add a row to the configuration guide's [Reference](../guide/configuration.md#reference) for each new config key and each new environment variable that configures `ox`. The `config::tests::reference` tests catch a missing key or `ANTHROPIC_`, `CLAUDE_`, or `OX_` variable, but not a variable with another prefix.
- Cite Claude Code, [Codex](https://github.com/openai/codex), [pi](https://github.com/badlogic/pi-mono), and [oh-my-pi](https://github.com/can1357/oh-my-pi) as reference implementations in new research and design notes. Existing notes may retain their opencode citations.

### Design Docs

- A design doc explains how a feature's mechanism works at module level and why it works that way, for the maintainer who changes it next. Leave per-function walkthroughs to the code. Leave a set the code enumerates, such as accepted options, builtin names, or enum variants, to the constant or type that holds it, whose rustdoc says what the set is for without restating its members. A set users rely on, such as the commands `manual` runs without asking, belongs in the user guide. Link the guide for user-facing behavior, such as commands, keys, and config keys, and keep the design doc to what that behavior means for the mechanism.

- Open a feature's design doc with a sentence saying what the feature does, then describe its mechanism in sections named for their topics, followed by the sections below in this order.

- Follow with `## Design Decisions`, a numbered list of the choices a maintainer could plausibly make differently, each giving its reason. An alternative way to build the shipped mechanism belongs in the reason of the choice that beat it, while that reason still constrains the mechanism. Leave out facts that had no alternative, such as a trait implementation the compiler requires.

- Cite a design decision by its bold title, since inserting a decision renumbers the list and leaves every numeric reference pointing at a different choice. Use `(**Title**)` in the same page and `[**Title**](page.md#design-decisions)` from another page.

- Add `## Out of Scope` when the page leaves out a feature. Each bullet opens with a bold label naming the feature, then links its issue if the feature is deferred or gives the reason if it is declined.

- End the doc with `## Sources`, listing the files that own the mechanism by full path, in alphabetical order, with a one-clause annotation and no line numbers. Files that only call into the mechanism stay out, since the [module map](../design/architecture.md) places their modules:

  ```markdown
  - `crates/ox-core/src/tool/read.rs`: `DEFAULT_LINE_LIMIT`, `MAX_LINE_CHARS`, `OUTPUT_BUDGET`.
  ```

### Research Notes

- `docs/research/` holds API references and surveys that inform open design questions. Each survey links the `design` issues it informs. The PR that settles one of them moves any comparison its decision still depends on into that decision's reason and trims the survey's part on it, and the PR that closes the last one deletes the survey, since nobody re-verifies a survey whose questions are closed while its claims drift as the reference implementations change.
- Follow each survey's opening paragraph with a `Verified <date> against <versions>.` line naming the reference implementations and versions its last full re-read covered. A claim you add or edit afterward in any research note carries its own date and version. Delete a claim if a newer test contradicts it or you can no longer verify it.
