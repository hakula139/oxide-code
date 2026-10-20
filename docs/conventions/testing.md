# Testing

Conventions for tests, their environment, timing, coverage, mutation testing, and live tests.

## Placement

- Unit tests live in a `#[cfg(test)] mod tests` in the file they test.
- `crates/ox/tests/` holds end-to-end tests that run the `ox` binary through `CARGO_BIN_EXE_ox`, as `tests/cli.rs` does. Test cross-module behavior in the module that composes those modules, such as `engine` tests that run a full turn against a mock server.
- Share fixtures across test modules from a `#[cfg(test)] mod testing` child of the module that owns what they build, so they can construct private fields. Give the child its own `use` imports and the narrowest [visibility](rust.md#visibility) its callers allow. Put an inline child under a `// ── Testing ──` banner, and declare a file-backed one with the other child modules. A test-only constructor on the type itself, such as `TokenUsage::new`, stays an `#[cfg(test)]` method in the type's `impl`.
- Gate a `testing` module on `#[cfg(any(test, feature = "testing"))]` when another crate's tests use its fixtures, directly or through another gated module's fixtures. Those crates turn it on through `ox-core`'s `testing` feature in their dev-dependencies. Put a test-only method they call, such as `SessionStore::list`, in an `impl` block inside that module, so the `.cargo/mutants.toml` exclusions for gated modules cover it. Any other item a gated fixture needs, such as a `Default` impl, takes the same gate in place at its narrowest visibility. Gated code also compiles without `cfg(test)`, where Clippy disallows `unwrap()`, so it states each invariant with `expect`. Move the module's fixtures that only its own crate's tests use into a sibling `#[cfg(test)] mod local_testing`, since the feature build would compile them as dead code.

## Grouping and Naming

These rules apply to unit-test modules (`mod tests`), while the integration tests under `crates/ox/tests/`, which drive the `ox` binary, keep their flat layout.

- Group each function's tests into its own child module of `mod tests` named after the function, as in `mod parse_color`, so that `cargo test parse_color::` runs them. Order the modules as their functions appear in production code, and omit banners because the module names mark each group.
- Nest a method's module in a snake-case module named after its type, as in `mod token_cost_rates { mod estimate_usd { ... } }` in `model/pricing.rs`. A trait-impl method takes a module named after the method, as in `rotating_file::write` for `io::Write::write`, and a trait's default methods nest under a module named after the trait.
- The file's primary type takes no type module, so its method modules sit directly in `mod tests`, except for any named like a free function of the file, which nests under the type module. The primary type is the type or trait whose name contains the file name's words as whole PascalCase words, singular or plural, as in `tool/bash.rs` → `BashTool`, `components/tasks.rs` → `TaskPanel`, and `components/input/popup.rs` → `SlashPopup`, though not `UnarchiveCmd` in `slash/archive.rs`. If several qualify, take the one with the most inherent, trait-impl, or declared trait methods, and among equals, prefer one named exactly like the file, then the first declared. A file that declares none but implements its parent module's type, as `app/render.rs` does for `App`, takes that type.
- Tests of a type as a whole go ahead of its method modules, directly in the type's module or, for the primary type, in `mod tests`. These include tests of its `Display`, `Debug`, and derived traits such as serde and clap's `Parser`, as well as tests that assert several of its methods together, such as a round trip through `from_str` and `Display`.
- When tests fall into topics, such as a function's input forms, keep each topic's tests together and order the topics as production code handles them, such as the arms of an `Event` match. A topic adds nothing to test names unless two would clash, in which case each clashing name starts with its topic in singular snake case, as in `edge_case_`. A variant's tests go in the module of its type or of the function under test, with the variant leading each name, as in `content_block::thinking_stores_its_replay_beside_the_thinking_text`.
- A test belongs to the module of the function whose behavior it asserts, which its name leads with or its act calls directly, even when it calls other functions to set up or observe that behavior. A method that only delegates to a free function, such as a trait-impl `Tool::run`, takes no module, so its tests go under that function even when they call through the method. A test of a function defined in another file stays in its own file, in a module named after the function and placed with the tests of the type it exercises.
- Behavior that spans several free functions or types goes in a module named for the behavior, as in `mod shell_id_input` in `tool/bash/background.rs`. A test that asserts a constant's value goes in a module named after the constant, as in `mod built_ins`, even when its act calls a function. Put such modules after the function modules.
- Start each module with `use super::*;`, which brings in the code under test along with the imports and helpers of each enclosing module. Child modules contain no other `use` declarations. Put imports at the top of `mod tests`, though imports inside a function body may stay there.
- Put each helper, constant, or fixture in the narrowest module enclosing every test that uses it, ahead of that module's tests, which is the top of `mod tests` for one that several modules share. Fixtures that tests in other files also use belong in the module's `testing` or `local_testing` child described above.
- When a test module's name clashes with a module that `mod tests` imports or that production code declares, drop the clashing import and refer to the other module by its full path, as in `crate::session::store` or `std::fs`.
- Within each module, place portable tests before platform-specific ones, ordering each group from happy paths to variants, then to edge and error cases. In a module split into topics, place each topic's platform-specific tests last within it. When every test in a module is platform-specific, gate the module once, as in `#[cfg(target_os = "macos")] mod evaluation_error`, because `use super::*` in a module left empty on other platforms fails the unused-import lint.
- Name each test for the scenario it covers, omitting the function's name or its shortened form because the module carries it, as in `string::unset_or_empty_is_absent` and the `handle_engine_event` tests that drop `handle_`. Describe the scenario and outcome in domain terms. For an absent result, name what the absence means, as in `resolve_effort::omits_effort_for_an_unknown_model`, and omit mechanisms such as a returned `None`, though a `none` may stay where it names a user-facing state, such as the "no effort" choice.
- When a change renames or moves a test or a snapshot baseline, update every reference to it in the docs, `RELEASING.md`, and CI workflows in the same PR.

## Data and Infrastructure

- Use generic, fictional test data such as `example.com` and `/repo`. Use real names and branded content only when the code under test matches on them, as with model IDs.
- Use established infrastructure: `wiremock` for HTTP, `temp-env` to isolate environment variables, `TestBackend` with `insta` for TUI snapshots, and extracted trait fakes for dependencies that are difficult to mock.
- Tests share one process environment, and `temp-env` serializes only code inside its closures. A read outside a closure can therefore see another test's temporary value. If a test's result depends on a variable that other tests set, such as the home directory or `XDG_DATA_HOME`, run it inside a `temp-env` closure that pins the variable. Pin the home directory through `HOME_VAR` in `util::path::testing`, because Windows reads `USERPROFILE`. Types constructed by many tests should take environment-derived values as arguments, as `Client::new` does with the TLS trust and device ID that `main` supplies.

## Hermetic Tests

Every test must run and pass wherever the suite runs: on each supported OS, in CI, and in the Nix build sandbox, which has no network access or system services such as macOS trustd. Never skip, `#[ignore]`, retry, or `cfg`-gate a test to get it past its environment. Restructure tests that depend on their environment until they pass everywhere. For example, move the dependency behind a trait fake, as `util::tls` does for the Security framework, or pin what the test reads. Use a platform `cfg` gate only for a test of code that exists only on that platform.

Run the suite as a non-root user, an environment requirement like having `git` installed. Root ignores the read denials that `tool::local_testing::deny_read_access` sets up, so the tests built on them fail.

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

`insta` snapshot tests compare TUI renders, session JSONL, and headless output against baselines in the module's `snapshots/` directory. When output changes, the test fails and writes a `.snap.new` file beside the baseline. Review the difference, accept it with `INSTA_UPDATE=always cargo test <filter>` or `cargo insta review`, and commit the updated `.snap`.

A baseline's file name consists of the test's `module_path!()`, which starts with the crate name, such as `ox_core`, `ox_tui`, or `ox`, followed by the snapshot name, with `__` replacing each `::`, as in `ox__driver__headless__tests__drive__stderr.snap`. An unnamed assertion takes the test's name, where insta strips a leading `test_` and appends `-2`, `-3`, and so on to each further unnamed snapshot in the same test. A `snapshot_suffix` setting appends `@<suffix>`. An explicit name drops the function prefix just as a test name does, so `drive_stderr` becomes `stderr` inside `mod drive`. Moving a test to another module renames every baseline it writes, while renaming a test renames only its unnamed ones. Rename each `.snap` file to match, and delete baselines left behind by removed tests. `cargo insta test --unreferenced=reject` fails whenever any baseline is left without a test.

The session fixtures in `crates/ox-core/src/session/fixtures/` are frozen release output, with one fixture per stable release starting at `v0.1.0`. Never edit them. A changed `session_format::resumes_from_every_release_fixture@<tag>` snapshot means an older release's sessions now resume differently, so accept it only as [Compatibility](../design/session/persistence.md#compatibility) allows.

## Coverage

CI uploads `cargo llvm-cov` results to Codecov. Its PR comment reports patch coverage, the share of changed PR lines that tests run. Overall coverage can be high while patch coverage stays low if a PR adds untested glue.

- Cover changed lines with tests that check behavior. Run `cargo llvm-cov --show-missing-lines` locally to list uncovered lines.
- Leave a line uncovered only when it is untestable wiring, such as an event-loop entry point or a panic hook. Explain why in the PR's Test plan.
- Check a shape in a test helper with `assert!(matches!(..))`, which leaves no panic arm for coverage to flag.

## Mutation Testing

Coverage shows whether a line ran. Mutation testing shows whether a mutation of that line would be caught. A full run is slow, so run it out of band before large-scope changes ship:

```bash
cargo mutants --workspace
```

`.cargo/mutants.toml` sets `test_workspace`, so each mutant faces every crate's tests. A surviving mutant usually means an assertion is too loose, so tighten it. If a mutant is genuinely equivalent, exclude it in that file with an `exclude_re` pattern and a comment explaining the equivalence.

## Live Tests

`crates/ox/tests/live.rs` covers what only a real endpoint can show: streamed text, a tool round trip, interleaved thinking replayed across turns, prompt cache reads, auto-compaction, and `--continue`. Its tests send paid requests, so they are ignored by default and return early unless `OX_LIVE=1`. Run them after changes to the client, the prompt, thinking, compaction, or session resume:

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
   grep -rqF -- "$ANTHROPIC_AUTH_TOKEN" \
     "${XDG_STATE_HOME:-$HOME/.local/state}/ox" \
     "${XDG_DATA_HOME:-$HOME/.local/share}/ox/sessions"
   echo $?
   ```
