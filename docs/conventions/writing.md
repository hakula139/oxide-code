# Writing

Conventions for comments, docstrings, documentation, commit and PR text, and strings read by users or the model.

## Comments

- Default to no comments. Add one only when a future maintainer needs a non-obvious constraint that the code and names cannot convey.
- Do not restate what the code does, describe its layout or order, narrate the change or refer to the task behind it, or record provenance such as "split out so tests can reach it". Commit messages and PR descriptions preserve that history.
- Explain cryptic flags, literals, and API quirks, and label what a non-obvious regex matches.
- When editing, delete comments that repeat the code and correct any that have become misleading. Before deleting a comment that records a constraint, check whether the constraint still applies.
- Delete commented-out code because version control preserves it.
- Write `//` comments as prose, with one line per thought. Use additional lines only when the rationale needs them. Use `///` when a list structure genuinely helps.
- Wrap comments at 100 columns to match rustfmt's `max_width`. Since rustfmt does not wrap comments, do so by hand. Tighten the wording if the final line would contain just one short word.

## Docstrings

- The first `//!` line in each `.rs` file describes the file's responsibility.
- A `///` doc states the contract callers need, such as invariants, units, ownership, or errors. Default to one line, using more only when the contract requires it. Omit docs that merely restate the item name or implementation.

## Prose

These rules apply to comments, docs, commit messages, PR and issue text, and strings shown to users or the model. Many Rust comments predate these rules. Issue [#220](https://github.com/hakula139/oxide-code/issues/220) tracks updating them.

- State conclusions directly and match each claim's strength to its evidence. Describe what you checked, and avoid absolute claims such as "bug-free" or "guaranteed".
- Connect related thoughts with a period or a linking word such as `since`, `because`, `while`, `so`, or `but`. Use an em-dash only as a paired parenthetical or to separate a list label from its gloss.
- Avoid semicolons in prose. They are fine in code and literal syntax, and between list items that contain commas.
- Define things by what they are. Add a contrast with what they are not only when omitting it could lead to a materially wrong reading.
- Avoid lists of three or more similarly structured phrases when they add rhythm without supporting the argument.
- Use straight quotes. Arrows and comparison operators (`→`, `≤`, `≠`) are welcome.
- Put spaces around `/` and arrows when they separate words or phrases, as in `Read / Write` and `low → high`. Keep compound terms such as `I/O` tight.
- Place commas and periods outside closing quotation marks.

## Markdown

- Put each paragraph on one line without hard wrapping, and separate paragraphs with a blank line.
- Use prose for explanations, bullets for parallel options or requirements, numbered lists for sequences, and tables for comparisons.
- Align trailing comments in directory trees and similar listings in one column, several spaces beyond the longest entry.

## Documentation

- Keep `README.md` focused on users: explain the value, supported features, scope, and usage.
- Use `docs/` to describe current behavior and its rationale. Track planned work, deferred follow-ups, and open design questions in GitHub issues, and link to the relevant issue from the docs.
- Organize each page around the questions its readers bring. Keep each paragraph focused on one idea, and verify every claim against the code before citing it.
- For each claim in a research note, record when it was last verified and which version it was checked against. Delete the claim if a newer test contradicts it or it can no longer be verified.
- Cite Claude Code, [Codex](https://github.com/openai/codex), [pi](https://github.com/badlogic/pi-mono), and [oh-my-pi](https://github.com/can1357/oh-my-pi) as reference implementations in new research and design notes. Existing notes may retain their opencode citations.
- In a design doc's `## Sources` section, list each source file it describes by full path, in alphabetical order, with a one-clause annotation and no line numbers:

  ```markdown
  - `crates/oxide-code/src/tool/read.rs`: `DEFAULT_LINE_LIMIT`, `MAX_LINE_CHARS`, `OUTPUT_BUDGET`.
  ```
