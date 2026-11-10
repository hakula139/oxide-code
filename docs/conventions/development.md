# Development

Conventions for the dev shell, toolchain, Nix and config file style, debug info, CI, and failing checks. [Installation](../guide/install.md) covers other ways to build `ox`.

## Dev Shell

The flake's dev shell provides the pinned Rust toolchain, `cargo-deny`, `cargo-insta`, `cargo-llvm-cov`, `cargo-mutants`, `git-cliff`, `sqlite3`, and the hook tools:

```bash
nix develop                # Enter the dev shell, which installs the hooks
nix develop -c run-hooks   # Run every hook on all tracked files
nix flake check            # Build the package and run the Home Manager and overlay checks
nix build                  # Build ox without running the tests
```

The [devkit hooks](https://github.com/hakula139/devkit/blob/v3/docs/hooks.md) run on every commit, so run `nix develop -c true` once in each new checkout or worktree. A hook that fixes files fails the commit, so stage its fixes and commit again. A commit that touches Rust files runs Clippy over the whole workspace.

With [direnv](https://direnv.net), run `direnv allow` once, and `.envrc` loads the dev shell, the gitignored `.envrc.local`, and `.env.local` whenever you enter the repository.

## Toolchain

- `rust-toolchain.toml` pins the Rust release for the dev shell, the hooks, CI's Rust, Rust Lint, and Windows jobs, and release builds. The Windows job and release builds install it with `rustup toolchain install`.
- Renovate bumps the pin in its automerged flake-inputs group. Fix any new lint failures in that PR.
- The Nix package builds with the Rust release of the flake's nixpkgs.
- `rust-version` in `Cargo.toml` is the minimum supported Rust version, which the `msrv` dev shell builds and [Versioning](../../RELEASING.md#versioning) covers.

## Nix Style

`nixfmt`, `statix`, and `deadnix` check `flake.nix` and `nix/`. They leave these rules open:

- Mark a file header with `=` banners at column 0 and each section with `-` banners indented with the code, ending every banner line at column 100. A file without banners stays without them.
- Put each argument of a top-level function on its own line. Take `...` only when the caller passes a broader set, as with flake outputs.
- In a `callPackage` file, such as `nix/package.nix`, declare each dependency as its own argument so `.override` can replace it, with `lib` first and the rest in alphabetical order. In the flake, reach packages through `pkgs`.
- Put `inherit` entries first in a `let` block and in a call's argument set.
- Group related bindings, order a `let` block so each binding follows what it depends on, and fall back to alphabetical order.
- Follow a multiline binding or attribute with a blank line, and keep single-line siblings together. A single-line binding may lead directly into a related multiline one.
- Write multiline text, such as a shell hook, as an indented string with literal line breaks.

## Config Style

Prettier, Taplo, and shfmt format YAML, TOML, and shell files but leave line length to you:

- Wrap comments as [Writing](writing.md#comments) describes, and other lines at about 100 columns. A line holding an unbreakable token, such as a URL, or content in a heredoc or literal block stays long.
- Wrap a long string only where its value stays the same, breaking it where [Rust](rust.md#strings) breaks a continued literal.
- Split a long shell command at its pipelines and `&&` / `||` lists. If a part is still too long, keep the command and its subcommands on the first line and put each flag with its value on its own line, one level deeper. End each line but the last with `\` unless it ends with `|`, `&&`, or `||`. Workflows put such a command in a `run: |` block.
- Put a multiline command substitution's command on the next line and its closing `)` on its own line, aligned with the assignment.

## Debug Info

Dev and test builds keep line tables for the workspace crates and no debug info for dependencies, which keeps `target/` small while panics and workspace backtrace frames keep their file and line. Set `CARGO_PROFILE_DEV_DEBUG=true` to rebuild with full debug info for a debugger.

## CI

CI runs on pushes to `main`, pull requests, and manual dispatch. The release workflow requires a passing run on the tagged commit and calls CI for a dry run from another ref.

| Job        | Systems      | Checks                                                                                                            |
| ---------- | ------------ | ----------------------------------------------------------------------------------------------------------------- |
| Nix        | Linux, macOS | `nix flake check`, then every hook on Linux and a dev-shell build on macOS                                        |
| Rust       | Linux, macOS | `cargo clippy --all-targets` on macOS, `cargo llvm-cov` with its Codecov upload, and doc tests                    |
| MSRV       | Linux        | `cargo check --all-targets` in the flake's `msrv` shell                                                           |
| Rust Lint  | Linux, macOS | `cargo clippy` without test targets, `cargo doc --no-deps`, and on Linux `cargo insta test --unreferenced=reject` |
| Windows    | Windows      | `cargo clippy --all-targets`, `cargo clippy`, `cargo doc --no-deps`, and `cargo llvm-cov` with its Codecov upload |
| Dependency | Linux        | `cargo deny check` in the flake's `lint` shell, which leaves out the pinned toolchain                             |
| CI Result  | Linux        | Fails unless every other job succeeded or was skipped                                                             |

The Nix, Rust, and MSRV jobs come from devkit's [reusable workflows](https://github.com/hakula139/devkit/blob/v3/docs/ci.md), and the Rust Lint job runs the checks `rust.yml` leaves out.

Branch protection requires CI Result and Codecov's `codecov/patch`. Add each new job to CI Result's `needs`, and give it a `timeout-minutes` several times its usual duration, since a hung test otherwise holds the runner for six hours. libtest names any test still running after 60 s, so a timed-out job's log shows which test hung.

Every job but Windows and CI Result reads the `hakula` Cachix cache, and a Cachix failure never fails a job. A download that stalls without failing still runs the job into its timeout, which [#715](https://github.com/hakula139/oxide-code/issues/715) tracks.

## Failing Checks

Fix the cause of a failing hook, lint, test, or CI job. Skips, `--exclude` flags, ignore and allowlist entries, `continue-on-error`, `|| true`, retries, and special cases for one path or commit only hide the failure. Restructure a test that cannot run in some environment, such as Windows, as [Hermetic Tests](testing.md#hermetic-tests) describes.

Keep an ignore entry or `continue-on-error` only when it is the design, such as a license the project accepts, an advisory with no fix, or the Cachix setup, and give the reason in the PR that adds it.
