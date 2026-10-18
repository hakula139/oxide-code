# Testing

Conventions for tests, their environment, timing, coverage, mutation testing, and live tests.

## Placement

- Unit tests live in a `#[cfg(test)] mod tests` in the file they test.
- The crate is binary-only, so tests in `tests/` cannot import its modules. That directory is for end-to-end tests that run the `ox` binary through `CARGO_BIN_EXE_ox`, as `tests/cli.rs` does. Test cross-module behavior in the module that composes those modules, such as `engine` tests that run a full turn against a mock server.
- Share fixtures across test modules from a `#[cfg(test)] mod testing` child of the module that owns what they build, so they can construct private fields. Put the child under a `// ── Testing ──` banner, give it its own `use` imports, and widen its visibility only as far as its callers need.

## Grouping and Naming

- Group tests by the function they cover under `// ── function_name ──` banners, following the order of functions in production code. When several sections cover one function, add a topic after a colon, as in `// ── parse_color: hex ──`.
- Within each section, put portable tests before platform-specific ones. Order each group from happy path to variants, then edge and error cases.
- Name each test for its function and the scenario it covers, as in `parse_color_hex_rejects_three_digit_shorthand`. Describe the scenario and outcome in domain terms, as in `string_unset_or_empty_is_absent`, and omit mechanisms such as a returned `None` from the name.

## Data and Infrastructure

- Use generic, fictional test data such as `example.com` and `/repo`. Use real names and branded content only when the code under test matches on them, as with model IDs.
- Use established infrastructure: `wiremock` for HTTP, `temp-env` to isolate environment variables, `TestBackend` with `insta` for TUI snapshots, and extracted trait fakes for dependencies that are difficult to mock.
- Tests share one process environment, and `temp-env` serializes only code inside its closures. A read outside a closure can therefore see another test's temporary value. If a test's result depends on a variable that other tests set, such as the home directory or `XDG_DATA_HOME`, run it inside a `temp-env` closure that pins the variable. Pin the home directory through `HOME_VAR` in `util::path::testing`, because Windows reads `USERPROFILE`. Types constructed by many tests should take environment-derived values as arguments, as `Client::new` does with the device ID loaded by `main`.

## Hermetic Tests

Every test must run and pass wherever the suite runs: on each supported OS, in CI, and in the Nix build sandbox, which has no network access or system services such as macOS trustd. Never skip, `#[ignore]`, retry, or `cfg`-gate a test to get it past its environment. Restructure tests that depend on their environment until they pass everywhere. For example, move the dependency behind a trait fake, as `util::tls` does for the Security framework, or pin what the test reads. Use a platform `cfg` gate only for a test of code that exists only on that platform.

The [live tests](#live-tests) are the sole exception because they send paid requests to a real endpoint.

## Time

- Drive timers with `#[tokio::test(start_paused = true)]` and the production constants. Tokio advances the paused clock through each `sleep`, making the test deterministic and independent of wall-clock time.
- Never swap a timing constant under `cfg(test)`, since the tests would then exercise values the binary never ships.
- Never sleep to order events or assert on elapsed time. Wait for the event itself through a channel or returned handle.
- Paused time cannot drive a test waiting on a child process because the paused clock stops advancing while a blocking-pool task runs, and on Windows Tokio reads child pipes on that pool. Have the code under test take its deadline as a future that the test completes when an event is observable, or poll with a bounded deadline for a condition caused by the child, such as a file it writes. `tool::bash` does both, through `execute_until` and the PID helpers in its `testing` module.

## Assertions

- Each assertion should fail when the code under test has a plausible bug. Avoid patterns that cannot reveal one, such as checking uniform data with `starts_with`, using wildcard `..` matches or loose bounds, or calling `is_err()` without checking the error when it matters.
- Aim for a concise suite with full coverage, merging tests that cover the same path.

## Snapshots

`insta` snapshot tests compare TUI renders, session JSONL, and headless output against baselines in the module's `snapshots/` directory. When output changes, the test fails and writes a `.snap.new` file beside the baseline. Review the difference, then accept it with `INSTA_UPDATE=always cargo test <filter>` or `cargo insta review` if cargo-insta is installed, and commit the updated `.snap`. Delete baselines left behind by renamed or removed tests.

The session fixtures in `crates/oxide-code/src/session/fixtures/` are frozen release output, with one fixture per stable release starting at `v0.1.0`. Never edit them. A changed `session_resumes_from_every_release_fixture@<tag>` snapshot means an older release's sessions now resume differently, so accept it only as [Compatibility](../design/session/persistence.md#compatibility) allows.

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

## Live Tests

`tests/live.rs` covers what only a real endpoint can show: streamed text, a tool round trip, interleaved thinking replayed across turns, prompt cache reads, auto-compaction, and `--continue`. Its tests send paid requests, so they are ignored by default and return early unless `OX_LIVE=1`. Run them after changes to the client, the prompt, thinking, compaction, or session resume:

```bash
OX_LIVE=1 cargo test --test live -- --ignored
```

Each test runs `ox` with a scratch directory as its home and for its config, sessions, and logs. `ox` loads no global instructions, config file, or stored login there, so the endpoint and credentials come only from the [environment variables](../guide/configuration.md#environment-variables), and one credential variable must be set. The harness also drops inherited `OX_*` settings other than `OX_EXTRA_CA_CERTS`, so they cannot change what the tests observe. `OX_LIVE_MODEL` picks the model, `claude-haiku-5-5` by default, and `OX_LIVE_THINKING_MODEL` picks the one for the thinking test, `claude-opus-5-5` by default.

After each run, a test fails when the value of `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, or `CLAUDE_CODE_OAUTH_TOKEN`, or an `ANTHROPIC_CUSTOM_HEADERS` value of 8 or more characters, appears in the run's output or in any file under the scratch directory. The failure names the variable without printing the value.

Run the suite once against the Anthropic API, where the billing header carries a live `cch` hash, and once through each Anthropic-compatible gateway you support.

### TUI Checklist

The tests drive headless and REPL runs. Check the TUI-only paths by hand in a scratch session on Haiku, switching models where a step says so:

1. **`/compact`**: Read a few files, run `/compact`, then ask about them. The reply still knows the files, and in the session file the usage record after the `compact` entry reads the earlier prefix from the cache.
2. **`/model` swap**: Send a prompt on Opus, switch to Haiku with `/model`, and send another. Then switch back to Opus and send a third. Every reply succeeds and remembers the earlier exchanges.
3. **Cancellation**: Press Esc while a long reply streams, and check that the stream stops within a second. Then send a new prompt and verify that the reply answers only that prompt.
4. **Leak check**: Search the logs and session files for each credential. Pass the value straight to `grep` so it never prints, and check the exit status: `1` means no match, `0` a match, and `2` an error such as a missing directory.

   ```bash
   grep -rqF -- "$ANTHROPIC_AUTH_TOKEN" "${XDG_STATE_HOME:-$HOME/.local/state}/ox" "${XDG_DATA_HOME:-$HOME/.local/share}/ox/sessions"
   echo $?
   ```
