# Development

Conventions for the dev shell, toolchain, Nix and config file style, debug info, and CI. [Installation](../guide/install.md) covers other ways to build `ox`.

## Dev Shell

Use the [dev shell](../../flake.nix) for local builds and checks:

```bash
nix develop                # Enter the dev shell, which installs the hooks
nix develop -c run-hooks   # Run every hook on all tracked files
nix flake check            # Build the package and run the Home Manager and overlay checks
nix build                  # Build ox without running the tests
```

If a hook modifies files, stage its fixes and commit again. Commits touching Rust files run workspace-wide Clippy.

With [direnv](https://direnv.net), run `direnv allow` once so `.envrc` loads the dev shell, gitignored `.envrc.local`, and `.env.local` on entry.

## Toolchain

- `rust-toolchain.toml` pins the Rust release for the dev shell, the hooks, CI's Rust, Rust Lint, and Windows jobs, and release builds. The Windows job and release builds install it with `rustup toolchain install`.
- Renovate bumps the pin in its automerged flake-inputs group. Fix any new lint failures in that PR.
- The Nix package builds with nixpkgs' Rust when it meets `rust-version`, and otherwise falls back to the `rust-toolchain.toml` toolchain.
- `rust-version` in `Cargo.toml` is the minimum supported Rust version, which the `msrv` dev shell builds and [Versioning](../../RELEASING.md#versioning) covers.

## Nix Style

Follow the shared [Nix](https://github.com/hakulab/plugins/blob/main/plugins/hakula-languages/skills/nix/SKILL.md) and [code layout](https://github.com/hakulab/plugins/blob/main/plugins/hakula-workflow/skills/code-layout/SKILL.md) conventions. `nixfmt`, `statix`, and `deadnix` check `flake.nix` and `nix/`, with these repository additions:

- Mark a file header with `=` banners at column 0 and each section with `-` banners indented with the code, ending every banner line at column 100. A file without banners stays without them.
- In a `callPackage` file, such as `nix/package.nix`, declare each dependency as its own argument so `.override` can replace it, with `lib` first and the rest in alphabetical order. In the flake, reach packages through `pkgs`.
- Put `inherit` entries first in a `let` block and in a call's argument set.

## Config Style

Prettier, Taplo, and shfmt format YAML, TOML, and shell files. Follow the shared [code layout](https://github.com/hakulab/plugins/blob/main/plugins/hakula-workflow/skills/code-layout/SKILL.md) conventions for commands and multiline text, with these width and syntax requirements:

- Wrap comments as [Writing](writing.md#comments) describes, and other lines at about 100 columns. A line holding an unbreakable token, such as a URL, or content in a heredoc or literal block stays long.
- Wrap a long string only where its value stays the same, breaking it where [Rust](rust.md#strings) breaks a continued literal.
- End each continued shell line with `\` unless it ends with `|`, `&&`, or `||`. Workflows put multiline commands in a `run: |` block.
- Put a multiline command substitution's command on the next line and its closing `)` on its own line, aligned with the assignment.

## Debug Info

Dev and test builds keep line tables for the workspace crates and no debug info for dependencies, which keeps `target/` small while panics and workspace backtrace frames keep their file and line. Set `CARGO_PROFILE_DEV_DEBUG=true` to rebuild the workspace crates with full debug info for a debugger.

## CI

The [CI workflow](../../.github/workflows/ci.yml) defines the jobs and calls devkit's [shared checks](https://github.com/hakulab/devkit/blob/v3/docs/ci.md). The [release workflow](../../.github/workflows/release.yml) requires a passing run on the tagged commit.

Branch protection requires CI Result and Codecov's `codecov/patch`. Add new jobs to CI Result's `needs` (the gate runs once listed jobs finish and skips any job still running), and set `timeout-minutes` to several times normal duration so hung tests cannot hold a runner for six hours (libtest names any test still running after 60 s).

Every job except Windows and CI Result reads the `hakula` Cachix cache. Cache errors never fail a job, though stalled downloads can still hit the job timeout ([#715](https://github.com/hakulab/oxide-code/issues/715)).

[Hermetic Tests](testing.md#hermetic-tests) covers tests whose environment affects their result.
