# Writing

Conventions for comments, docstrings, documentation, commit and PR text, and strings read by users or the model.

## Comments

- Default to no comments. Add one only when a future maintainer needs a non-obvious constraint that the code and names cannot convey.
- Do not restate what the code does, describe its layout or order, narrate the change or refer to the task behind it, or record provenance such as "split out so tests can reach it". Commit messages and PR descriptions preserve that history.
- Explain cryptic flags, literals, and API quirks, and label what a non-obvious regex matches.
- When editing, delete comments that repeat the code and correct any that have become misleading. Before deleting a comment that records a constraint, check whether the constraint still applies.
- Delete commented-out code because version control preserves it.
- Write `//` comments as prose. A docstring may use a Markdown list for parallel options or a sequence.
- Wrap comments, docstrings, and prose literals continued across lines by hand at 100 columns, matching rustfmt's `max_width`, since stable rustfmt wraps none of them. Fill each paragraph up to the limit, as editors' fill commands and rustfmt's `wrap_comments` do, so sentences continue on the same line and a blank comment line separates paragraphs. Break a line early only where that clearly reads better, such as to keep a code span, link, or short parenthetical whole, or to give each item of a list-like sentence its own line. Reword when the last line would hold one short word. Comments in shell, Nix, YAML, and TOML files follow the same rule.

## Docstrings

- The first `//!` line in each `.rs` file describes the file's responsibility.
- A `///` doc states the contract callers need, such as invariants, units, ownership, or errors. Default to one line, using more only when the contract requires it. Omit docs that merely restate the item name or implementation.
- Write docs in the third person. A doc on a function, or on a variant that names an action, says what it does, though a pure function's doc may name the value it returns. A predicate's doc starts with `Returns whether`, and a boolean field's starts with `Whether`. Other docs say what the item is. A clap flag's doc is its `--help` text, so it gives a command in one sentence, since clap drops the final period of a flag's help.

## Prose

These rules apply to comments, docs, commit messages, PR and issue text, `#[expect]` reasons, and strings shown to users or the model.

- State conclusions directly and match each claim's strength to its evidence. Describe what you checked, and avoid absolute claims such as "bug-free" or "guaranteed".
- Say each thing once, using the fewest words that preserve its meaning. Give messages of the same kind a consistent shape.
- Include units with numbers, as in `Retrying in 1 s`. Write 1024-based sizes in KiB and MiB, as in `64 KiB`, and keep KB and MB for 1000-based ones.
- Connect related thoughts with a period or a linking word such as `since`, `because`, `while`, `so`, or `but`. Use an em-dash only as a paired parenthetical or to separate a list label from its gloss.
- Avoid semicolons in prose. They are fine in code and literal syntax, and between list items that contain commas.
- Define things by what they are. Add a contrast with what they are not only when omitting it could lead to a materially wrong reading.
- Avoid lists of three or more similarly structured phrases when they add rhythm without supporting the argument.
- Use American spelling in prose and identifiers, such as `behavior`, `catalog`, and `recognize`. Keep the double l in `cancelled` and `cancelling` to match ecosystem names such as tokio-util's `is_cancelled` and `cancelled()`, and GitHub's `CANCELLED` check state.
- Place commas and periods outside closing quotation marks.
- Use straight quotes. Arrows and comparison operators (`→`, `≤`, `≠`) are welcome.
- Put spaces around `/` and arrows when they separate words or phrases, as in `Read / Write` and `low → high`. Keep compound terms such as `I/O` tight.
- Label a size shown in units by the base it counts in: `KiB`, `MiB`, and `GiB` for powers of 1024, and `kB` and `MB` for powers of 1000. `util::text::format_size` picks the binary unit for a byte count. An exact count stays in bytes.
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
- When you add or edit a claim in a research note, record when you verified it and which version you checked. Delete the claim if a newer test contradicts it or you can no longer verify it.
- Cite Claude Code, [Codex](https://github.com/openai/codex), [pi](https://github.com/badlogic/pi-mono), and [oh-my-pi](https://github.com/can1357/oh-my-pi) as reference implementations in new research and design notes. Existing notes may retain their opencode citations.
- Open a feature's design doc with a sentence saying what the feature does, then describe its mechanism in sections named for their topics. Follow with `## Design Decisions`, a numbered list of the choices that need a rationale, each giving its reason, and with `## Out of Scope`, linking the issues that track deferred work, when there is any. End each one with `## Sources`.
- In a design doc's `## Sources` section, list each source file it describes by full path, in alphabetical order, with a one-clause annotation and no line numbers:

  ```markdown
  - `crates/ox-core/src/tool/read.rs`: `DEFAULT_LINE_LIMIT`, `MAX_LINE_CHARS`, `OUTPUT_BUDGET`.
  ```
