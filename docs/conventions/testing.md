# Testing

Follow the [shared test requirements](https://github.com/hakulab/nixos-config/blob/main/home/modules/llm-assistants/shared/instructions/docs/verification-and-tests.md#tests) for test inputs, structure, and behavioral assertions. This page adds oxide-code's test layout, fixtures, timing, coverage, mutation testing, and live tests.

## Placement

- Unit tests live in `#[cfg(test)] mod tests` in the file they test.
- `crates/ox/tests/` holds end-to-end tests running `ox` via `CARGO_BIN_EXE_ox`. Non-live tests build as one binary, `tests/cli/`, whose `main.rs` holds shared helpers and declares one module per area (such as `config_check` or `headless`) so the suite links once. `tests/live.rs` holds [live tests](#live-tests). Test cross-module behavior in the composing module (such as `engine` tests against a mock server).
- Share fixtures across test modules from a `#[cfg(test)] mod testing` child of the owning module so they can access private fields, with its own `use` imports and the narrowest [visibility](rust.md#visibility) needed. Put inline `testing` modules under `// ── Testing ──`, and declare file-backed ones alongside other child modules. Keep test-only constructors on the type itself (such as `TokenUsage::new`) as `#[cfg(test)]` methods in its `impl`.
- Gate cross-crate fixtures on `#[cfg(any(test, feature = "testing"))]` (enabled via `ox-core`'s `testing` dev-dependency feature). Put test-only methods they call (such as `SessionStore::open_at`) in an `impl` block inside that module so `.cargo/mutants.toml` exclusions cover them, and apply the same gate at narrowest visibility to any other helper item (such as a `Default` impl). Because gated code compiles without `cfg(test)`, use `expect` where Clippy disallows `unwrap()`. Put single-crate fixtures in a sibling `#[cfg(test)] mod local_testing` so the feature build does not compile them as dead code.

## Grouping and Naming

Unit tests group under `mod tests` by what they exercise:

| Subject                     | Module                                                                  |
| --------------------------- | ----------------------------------------------------------------------- |
| Free function               | Named after the function                                                |
| Method, trait impl included | Named after the method, inside a snake-case type module                 |
| Primary type's method       | Directly under `tests`, except a name collision nests it under the type |
| Trait default method        | Named after the method, inside a snake-case trait module                |
| Type-wide behavior          | Directly in the type module, or `tests` for the primary type            |
| Multi-function behavior     | Named after the behavior, after function modules                        |
| Constant value              | Named after the constant, after function modules                        |

The primary type is the type named by the file, or the parent module's type the file implements. Its method nests under the type only when its name collides with a free function in the file. Whole-type contracts and tests that exercise several methods precede method modules. Order function and method modules as their functions appear in production code, and omit banners because the module names mark each group.

A test belongs to the function whose behavior it asserts, as identified by its name or act, even when other functions set up or observe that behavior. A method that only delegates to a free function takes no test module, so its tests go under that function even when they call through the method. A test of a function defined in another file stays in its existing file with the tests of the type it exercises, in a module named after the function. A constant's value test belongs to its constant module even when its act calls a function.

Name each test for its scenario and outcome in domain terms, omitting the function's name or shortened form. Name absent results for what their absence means without naming their returned representation, keeping `none` only when it names a user-facing state. Variant tests belong to their type's or tested function's module and start with the variant. Keep topic groups in production-code order. A topic adds nothing to test names unless they would clash, when each clashing name starts with its topic in singular snake case.

Each module starts with `use super::*;`. Child modules have no other `use` declarations. Put imports at the top of `mod tests`, though function-local imports may stay there. When a test module's name clashes with an imported or production module, drop the clashing import and use the other module's full path.

Put helpers, constants, and fixtures in the narrowest module enclosing all their callers, before its tests. Shared fixtures for tests in other files belong in `testing` or `local_testing`, as [Placement](#placement) describes.

Within each module or topic, put portable tests before platform-specific tests, ordering each group from happy paths to variants, then edge and error cases. Gate an entirely platform-specific module once, so its `use super::*` does not become unused on other platforms. When moving or renaming a test or snapshot, update its references in docs, `RELEASING.md`, and CI in the same PR.

The end-to-end tests in `crates/ox/tests/cli/` group by area as [Placement](#placement) describes. Each area module starts with `use super::*;`, and each test name omits the area's name.

## Data and Infrastructure

- Use real names and branded content only when the code under test matches on them, as with model IDs.
- Use established infrastructure: `wiremock` for HTTP, `temp-env` to isolate environment variables, `TestBackend` with `insta` for TUI snapshots, and extracted trait fakes for dependencies that are difficult to mock.
- Tests share one process environment, and `temp-env` serializes only code inside its closures. Any test depending on a variable other tests modify (such as home directory or `XDG_DATA_HOME`) must pin it inside a `temp-env` closure. Pin home via `HOME_VAR` in `util::path::testing` (Windows reads `USERPROFILE`). Types constructed by many tests should take environment-derived values as arguments (as `Client::new` takes TLS trust and device ID from `main`).

## Hermetic Tests

Every test must pass on every supported OS and in CI. Never skip, `#[ignore]`, retry, or `cfg`-gate a test around environment issues (only [live tests](#live-tests) are ignored, since they send paid requests). Put environment dependencies behind a trait fake (as `util::tls` does for the Security framework) or pin what the test reads. Reserve platform `cfg` gates for platform-only code.

Run the suite as a non-root user, since root ignores access denials and read-only files that `util::fs::testing` sets up.

## Time

- Drive timers with `#[tokio::test(start_paused = true)]` and production constants so Tokio advances paused time deterministically.
- Never swap a timing constant under `cfg(test)`, since tests would exercise values the binary never ships.
- Never sleep to order events or assert on elapsed time. Wait for the event itself through a channel or handle.
- Paused time cannot drive tests waiting on a child process because the paused clock stops while a blocking-pool task runs (and on Windows, Tokio reads child pipes on that pool). Pass the deadline as a future the test completes once an event is observable, or poll with a bounded deadline for a child-produced condition such as a file (`tool::bash` does both via `execute_until` and PID helpers in its `testing` module).

## Assertions

- Inline an act inside its assertion when checking only a return value (pure function, query, or render). Separate acts whose side effects are checked. Multi-state tests repeat act and assert with each step's setup directly above its act. Multi-case tests of distinct shapes separate cases with blank lines, while homogeneous cases go in a table driven by a loop.
- Assert an empty sequence (`Vec`, slice, `VecDeque`) with `assert_eq!(x, [])` (printing unexpected items on failure), writing `[] as [T; 0]` when the element type does not infer. Compare other collections against an empty instance, as in `assert_eq!(map, HashMap::new())`.

## Snapshots

`insta` snapshot tests compare TUI renders, the session store's SQL dump and export, and headless output against baselines in the module's `snapshots/` directory. When output changes, review it with `cargo insta review` and commit the updated `.snap`.

- A baseline's file name follows the test's module path, so moving a test renames every baseline it writes, and renaming it renames its unnamed ones. Rename each `.snap` to match, and delete baselines that removed tests leave behind. `cargo insta test --unreferenced=reject` and CI's Rust Lint Check (Linux) job fail on a baseline no test asserts.
- An explicit snapshot name drops the function prefix just as a test name does, so `drive_stderr` becomes `stderr` inside `mod drive`.

The session fixtures in `crates/ox-core/src/session/fixtures/` are frozen release output, with a SQL dump and an export per stable release starting at `v0.1.0`. Never edit them. A changed `session_format::resumes_from_every_release_fixture@<tag>` snapshot means an older release's sessions now resume differently, so accept it only as [Compatibility](../design/persistence.md#compatibility) allows.

## Coverage

CI uploads `cargo llvm-cov` results to Codecov, and the required `codecov/patch` status measures the share of changed lines that tests run.

- Cover changed lines with tests that check behavior. `cargo llvm-cov --show-missing-lines` lists uncovered lines.
- Leave a line uncovered only when it is untestable wiring, such as an event-loop entry point or a panic hook. Explain why in the PR's Verification section.
- Check a shape in a test helper with `assert!(matches!(..))`, which leaves no panic arm for coverage to flag.

## Mutation Testing

Mutation testing shows whether the tests would catch a changed line. A full run is slow, so run `cargo mutants` out of band before a large change ships. Each mutant faces every crate's tests. Tighten the assertion a surviving mutant slips past, and exclude a genuinely equivalent mutant in `.cargo/mutants.toml` with an `exclude_re` pattern and a comment explaining the equivalence.

## Live Tests

`crates/ox/tests/live.rs` covers what only a real endpoint can show: streamed text, a tool round trip, interleaved thinking replayed across turns, prompt cache reads, auto-compaction, and `--resume`. Its tests send paid requests, so they are ignored by default and return early unless `OX_LIVE=1`. Run them after changes to the client, the prompt, thinking, compaction, or session resume:

```bash
OX_LIVE=1 cargo live
```

Each test runs `ox` with a scratch home, so the endpoint and credentials come only from the [environment variables](../guide/configuration.md#environment-variables), and one credential variable must be set. The harness drops inherited `OX_*` settings other than `OX_EXTRA_CA_CERTS`.

| Variable                 | Picks                           | Default            |
| ------------------------ | ------------------------------- | ------------------ |
| `OX_LIVE_MODEL`          | The model for every test        | `claude-haiku-5-5` |
| `OX_LIVE_THINKING_MODEL` | The model for the thinking test | `claude-opus-5-5`  |

A test fails when a credential value, or an `ANTHROPIC_CUSTOM_HEADERS` value of 8 or more characters, appears in the run's output, a file under the scratch home, or the saved session's `ox export`. The failure names the variable without printing the value.

Run the suite once against the Anthropic API, where the billing header carries a live `cch` hash, and once through each Anthropic-compatible gateway you support.

### TUI Checklist

The tests drive headless runs. Check the TUI-only paths by hand in a scratch session on Haiku, switching models where a step says so:

1. **`/compact`**: Read a few files, run `/compact`, then ask about them. The reply still knows the files, and in the session's `ox export`, the first usage record after the compaction root's `created_at` reads the earlier prefix from the cache.
2. **`/model` swap**: Send a prompt on Opus, switch to Haiku with `/model`, and send another. Then switch back to Opus and send a third. Every reply succeeds and remembers the earlier exchanges.
3. **Cancellation**: Press Esc while a long reply streams, and check that the stream stops within a second. Then send a new prompt and verify that the reply answers only that prompt.
4. **Scrolling and the mouse**: In a bare terminal, inside tmux with `mouse on`, and over SSH, wheel-scroll the chat and check that tmux stays out of copy mode. Press PageUp until the jump-to-bottom pill appears, then press Ctrl+End and check that the chat follows the tail again. Drag-select a region and paste it elsewhere, double-click a word, triple-click a row, and hold a drag past the top edge until the chat scrolls. Press Ctrl+C on a selection and check that it copies without interrupting, then hold the bypass modifier and check that native selection still works. Click a `... +N lines` footer and a `Thought` header, and check that only that block expands. Click each pull request segment, then Ctrl+click it (Cmd+click in iTerm2 and Terminal.app), and check that each opens its page once. Quit, and check that the terminal screen is restored.
