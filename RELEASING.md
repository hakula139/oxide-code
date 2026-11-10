# Releasing

Pushing a tag that matches `v[0-9]+.*` starts `.github/workflows/release.yml`, which waits for a passing CI run on the tagged commit, builds and checks every target, and publishes a GitHub Release with the tag's `CHANGELOG.md` section as its notes. A tag with a prerelease suffix, such as `v0.1.0-rc.1`, publishes a prerelease.

Before publication, devkit's `nix-cache.yml` pushes the flake's x86_64 Linux and Apple Silicon macOS packages to the `hakula` Cachix cache and pins each as `oxide-code-<tag>-<system>` to retain its closure. [Recover a release](https://github.com/hakula139/devkit/blob/v3/docs/releasing.md#recover-a-release) shows how to restore a deleted pin.

## Versioning

- **Through 0.1.0:** tag `v0.1.0-alpha.N` from `main` whenever a user-visible batch or a fix for shipped behavior lands. Once the [rc gates](#stable-gates-for-010) are met, tag `v0.1.0-rc.1`. There is no beta stage.
- **Soak:** each rc starts a seven-day soak. A bug found during the soak joins the next rc's milestone, its fix ships in the next rc, and the soak restarts. Once a soak ends without a new bug, tag `main` as `v0.1.0`.
- **Main during a soak:** merge only `fix`, `docs`, `test`, and `chore(release)` PRs. Feature and refactor PRs wait for the stable tag, so the stable commit matches the soaked rc plus its fixes. Renovate automerges dependency bumps, so set `"enabled": false` in `.github/renovate.json` when the soak starts and revert it after the stable tag, both through `ci(renovate)` PRs.
- **After 0.1.0:** a breaking change to the [stability surface](#stability-surface) bumps the minor version, and every other change bumps the patch version. Prereleases of the next minor (`v0.2.0-alpha.N`) follow the rules above.
- **Milestones:** each planned release batch, including a patch release or prerelease, has its own milestone, such as `v0.2.0` or `v0.2.0-alpha.1`.
- **Breaking changes:** a PR that breaks the stability surface marks its title with [`!`](docs/conventions/github.md#commits-and-branches) both before and after 0.1.0. Before 0.1.0, the version still advances within the prerelease sequence.
- **MSRV:** the CI MSRV job checks `rust-version`. A bump affects `cargo install` users and Nix source builds, so it ships as a normal change that names the new floor in its PR title. The Nix jobs fail when the floor passes the Rust release of the flake's nixpkgs.

### Stability Surface

Within a minor version, stable releases keep these compatible:

- CLI flags, subcommands, and exit codes
- Config keys and file locations, plus environment variables
- The flake's `packages`, `overlays.default`, and `homeModules.default` outputs, and the module's options
- Persisted formats: the session store, versioned by its schema revision and each session's content format, the session export that `ox export` writes, and theme TOML
- Output from headless mode (`-p`), as defined in the [compatibility notes](docs/guide/headless.md#compatibility)
- Release asset names, which install scripts and the Homebrew formula depend on

The TUI layout, prompt text, slash-command output wording, and log format are outside the surface.

Before 0.1.0, config, environment variable, and session format changes may freely break earlier setups and files, with no migration hints, legacy fallbacks, or compatibility shims.

## Release Preparation

Follow the [shared release procedure](https://github.com/hakula139/devkit/blob/v3/docs/releasing.md) for changelog curation, the release PR, tagging its exact merged commit after separate approval, and CI recovery. Draft `CHANGELOG.md` with the shared configuration.

Security fixes describe the gap, affected releases, and published advisory as [SECURITY.md](SECURITY.md) promises. Add an advisory published later to every affected changelog section through a correction PR.

1. Set `[workspace.package].version` in `Cargo.toml`, then run `cargo update --workspace`.

2. Set the `VERSION=` line in `docs/guide/install.md` to the release tag.

3. For a stable tag, add the release's two [session resume fixtures](docs/design/persistence.md#compatibility), the SQL dump and the export snapshots with their insta headers deleted by `sed` from the first line through the second `---` line. `INSTA_UPDATE=unseen` then records how the new fixtures resume, while still failing if an earlier release's fixture now resumes differently:

   ```bash
   session=crates/ox-core/src/session
   snapshot="$session/snapshots/ox_core__session__handle__tests__session_format"
   mkdir -p "$session/fixtures"
   sed '1,/^---$/d' "${snapshot}__writes_every_row_kind.snap" >"$session/fixtures/vX.Y.Z.sql"
   sed '1,/^---$/d' "${snapshot}__exports_every_row_kind.snap" >"$session/fixtures/vX.Y.Z.jsonl"
   INSTA_UPDATE=unseen cargo test -p ox-core --lib session_format::resumes_from_every_release_fixture
   ```

4. Run the [verification block](AGENTS.md#verification), including session compatibility checks for stable releases. The release PR changes the version files, changelog, pinned install snippet, any needed spelling entries, and stable-release fixtures and snapshots.

## Verify Publication

Watch the [Release workflow](https://github.com/hakula139/oxide-code/actions/workflows/release.yml) with `gh run watch` and check:

- The release notes match the changelog section, and a prerelease tag published a prerelease.
- Every [target](#targets) archive and its `.sha256` sidecar are attached, and `gh attestation verify <archive> --repo hakula139/oxide-code` passes for one of them.
- `nix run github:hakula139/oxide-code/vX.Y.Z -- --version` prints the version on one host.

The workflow then renders the Homebrew formula from the release sidecars, runs `brew test` on macOS and Linux, and opens a `chore(release): refresh Homebrew formula for vX.Y.Z` PR. Merging that PR once its checks pass is the final release step. The formula follows the latest release, or the newest prerelease until the first stable release, so other tags skip this job. If the job fails, fix the cause and rerun it, which reuses an open PR.

## Recovery

Follow the shared release procedure for notes corrections and CI retries. Let an in-flight release finish before rebuilding assets, and rebuild the original commit for the same tag. A source change requires a new version and release PR.

## Stable Gates for 0.1.0

Before tagging `v0.1.0-rc.1`:

- All planned prerelease work is complete, and the [`v0.1.0` milestone](https://github.com/hakula139/oxide-code/milestone/1) holds no open issue except the release work for these gates.
- CI on `main` passes the full test suite on Linux, macOS, and Windows ([#127](https://github.com/hakula139/oxide-code/issues/127)).
- Each auth mode works live in a TUI turn that calls a tool and a `-p` run: Claude Code OAuth login, a long-lived OAuth token ([#121](https://github.com/hakula139/oxide-code/issues/121)), an API key, and a gateway with a bearer token and custom headers ([#122](https://github.com/hakula139/oxide-code/issues/122)). Every Claude 5.5 model answers at least one turn. Record the matrix in the rc PR's Test plan.
- The persisted formats are documented: the session store and export in [`docs/design/persistence.md`](docs/design/persistence.md), config keys in [`docs/guide/configuration.md`](docs/guide/configuration.md) and [`docs/guide/permissions.md`](docs/guide/permissions.md), and the theme TOML in [`docs/guide/theming.md`](docs/guide/theming.md).
- `SECURITY.md` exists ([#200](https://github.com/hakula139/oxide-code/issues/200)), and every archive ships `THIRD-PARTY-NOTICES` ([#207](https://github.com/hakula139/oxide-code/issues/207)).

Before tagging `v0.1.0`:

- Every rc gate still holds, and `gh api repos/hakula139/oxide-code/milestones/1 --jq .open_issues` reports 0.
- A whole-repo review at the rc commit has no open findings. Fix each finding or file it into the milestone.
- The rc serves as the daily driver for the full soak without a new bug.

The `v0.1.0` release PR also switches the install snippets to `releases/latest/download/`, then removes the `VERSION=` lines and the [Release Preparation](#release-preparation) step that pins them. Because it adds the first session resume fixtures, it also drops `allow_empty_glob` from the fixture test in `crates/ox-core/src/session/handle.rs`, so the test fails again when no fixture matches.

## Targets

The build matrix in `release.yml` lists the targets. Each release attaches one archive per target with a `.sha256` sidecar and a build provenance attestation, and release asset names belong to the [stability surface](#stability-surface).

- The release jobs read the platform floors from `docs/guide/install.md`, so keep its `Linux with glibc <version> or later` and `**macOS**: <version> or later` wording. The Linux job fails when the binary needs a newer glibc, which a runner image update can cause, so pin the previous image or raise the floor. The macOS job fails when the binary's minimum macOS differs from the floor, and raising it also changes `MACOSX_DEPLOYMENT_TARGET` in `release.yml` and `depends_on macos:` in `scripts/oxide-code-formula.rb`.
- Add a target by extending the matrix. Each target needs a runner that executes its binary, since every build job checks its archive there.
