# Testing

Conventions for tests, coverage, and mutation testing.

## Placement

- Unit tests live in a `#[cfg(test)] mod tests` in the file they test.
- The crate is binary-only, so tests in `tests/` cannot import its modules. That directory is for end-to-end tests that run the `ox` binary through `CARGO_BIN_EXE_ox`, as `tests/cli.rs` does. Test cross-module behavior in the module that composes those modules, such as `engine` tests that run a full turn against a mock server.
- Share fixtures across test modules as `#[cfg(test)] pub(crate)` items in the module that owns what they build, often in a `testing` child module that can construct private fields.

## Grouping and Naming

- Group tests by the function they cover under `// ── function_name ──` banners, following the order of functions in production code. When several sections cover one function, add a topic after a colon, as in `// ── parse_color: hex ──`.
- Within each section, put portable tests before platform-specific ones. Order each group from happy path to variants, then edge and error cases.
- Name each test for its function and the scenario it covers, as in `parse_color_hex_rejects_three_digit_shorthand`. Describe the scenario and outcome in domain terms, as in `string_unset_or_empty_is_absent`, and omit mechanisms such as a returned `None` from the name.

## Data and Infrastructure

- Use generic, fictional test data such as `example.com` and `/repo`. Use real names and branded content only when the code under test matches on them, as with model IDs.
- Use established infrastructure: `wiremock` for HTTP, `temp-env` for environment isolation, `TestBackend` with `insta` for TUI snapshots, and extracted trait fakes for dependencies that are hard to mock.

## Assertions

- Each assertion should fail when the code under test has a plausible bug. Avoid patterns that cannot reveal one, such as checking uniform data with `starts_with`, using wildcard `..` matches or loose bounds, or calling `is_err()` without checking the error when it matters.
- Aim for a concise suite with full coverage, merging tests that cover the same path.

## Snapshots

`insta` snapshot tests compare TUI renders, session JSONL, and headless output against baselines in the module's `snapshots/` directory. When output changes, the test fails and writes a `.snap.new` file beside the baseline. Review the difference, then accept it with `INSTA_UPDATE=always cargo test <filter>` or `cargo insta review` if cargo-insta is installed, and commit the updated `.snap`. Delete baselines left behind by renamed or removed tests.

## Coverage

CI uploads `cargo llvm-cov` results to Codecov. Its PR comment reports patch coverage, the share of changed PR lines that tests run. Overall coverage can be high while patch coverage stays low if a PR adds untested glue.

- Cover changed lines with tests that check behavior. Run `cargo llvm-cov --show-missing-lines` locally to list uncovered lines.
- Leave a line uncovered only when it is untestable wiring, such as an event-loop entry point or a panic hook. Explain why in the PR's Test plan.

## Mutation Testing

Coverage shows whether a line ran. Mutation testing shows whether a mutation of that line would be caught. A full run is slow, so run it out of band before large-scope changes ship:

```bash
cargo mutants --package oxide-code
```

A surviving mutant usually means an assertion is too loose, so tighten it. If a mutant is genuinely equivalent, exclude it with an `exclude_re` pattern in `.cargo/mutants.toml`. Create the file for the first exclusion and add a comment explaining the equivalence.
