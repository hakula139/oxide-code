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
- Write docs in the third person. A doc on a function, or on a variant that names an action, says what it does, as in `Returns ...` or `Restores ...`. Other docs say what the item is. A doc on a boolean field or predicate starts with `Whether`.
- A module doc points to its design doc in a closing ``Design: `docs/design/<area>/<doc>.md`.`` line.

## Prose

These rules apply to comments, docs, commit messages, PR and issue text, `#[expect]` reasons, and strings shown to users or the model. Many Rust comments predate these rules. Issue [#220](https://github.com/hakula139/oxide-code/issues/220) tracks updating them.

- State conclusions directly and match each claim's strength to its evidence. Describe what you checked, and avoid absolute claims such as "bug-free" or "guaranteed".
- Say each thing once, using the fewest words that preserve its meaning. Give messages of the same kind a consistent shape, such as stating the cause followed by the step that fixes it.
- Connect related thoughts with a period or a linking word such as `since`, `because`, `while`, `so`, or `but`. Use an em-dash only as a paired parenthetical or to separate a list label from its gloss.
- Avoid semicolons in prose. They are fine in code and literal syntax, and between list items that contain commas.
- Define things by what they are. Add a contrast with what they are not only when omitting it could lead to a materially wrong reading.
- Avoid lists of three or more similarly structured phrases when they add rhythm without supporting the argument.
- Use American spelling in prose and identifiers, such as `behavior`, `catalog`, and `recognize`. `cspell` uses an American English dictionary that also accepts some British forms, including `catalogue`. Keep the double l in `cancelled` and `cancelling` to match ecosystem names such as tokio-util's `is_cancelled` and `cancelled()`, and GitHub's `CANCELLED` check state.
- In strings shown to users and in `#[expect]` reasons, terminal punctuation separates sentences. One-sentence text, such as a status line, label, notice, or error, takes no final period, whether or not it is grammatically complete, and a question keeps its `?`. Text with two or more sentences ends each sentence with terminal punctuation, including the last. Text the model reads, such as prompts, tool descriptions and schemas, tool results, and transcript markers, keeps its punctuation.
- Text composed around variable external content, such as an API payload, an `{e:#}` cause, or a refusal explanation, keeps the inserted text as it arrives and adapts the separator after it to how that text ends, so each sentence ends once. An API error reads `{headline}: {details}. {advice}`, and when the details already end in `.`, `?`, `!`, `...`, the single-character ellipsis U+2026, or a fullwidth `。`, `？`, or `！`, a space alone separates them from the advice. `util::text::join_sentences` applies this separator. Our own literals still carry their punctuation.
- Begin standalone text, such as a notice or slash-command error, with a capital letter. Begin a fragment that forms part of an error chain with a lowercase letter because it continues the headline before its colon. Keep identifiers and proper nouns in their original case wherever they appear, as in a message that starts with `macOS` or `old_string`.
- Place commas and periods outside closing quotation marks.
- Use straight quotes. Arrows and comparison operators (`→`, `≤`, `≠`) are welcome.
- Put spaces around `/` and arrows when they separate words or phrases, as in `Read / Write` and `low → high`. Keep compound terms such as `I/O` tight.
- Close a delimited block the same way it opens. If an opening tag or fence, such as `<system-reminder>`, appears on its own line, put its closing counterpart on its own line too.

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
- Organize each page around the questions its readers bring. Keep each paragraph focused on one idea, and verify every claim against the code before citing it.
- When you add or edit a claim in a research note, record when you verified it and which version you checked. Delete the claim if a newer test contradicts it or you can no longer verify it.
- Cite Claude Code, [Codex](https://github.com/openai/codex), [pi](https://github.com/badlogic/pi-mono), and [oh-my-pi](https://github.com/can1357/oh-my-pi) as reference implementations in new research and design notes. Existing notes may retain their opencode citations.
- In a design doc's `## Sources` section, list each source file it describes by full path, in alphabetical order, with a one-clause annotation and no line numbers:

  ```markdown
  - `crates/oxide-code/src/tool/read.rs`: `DEFAULT_LINE_LIMIT`, `MAX_LINE_CHARS`, `OUTPUT_BUDGET`.
  ```
