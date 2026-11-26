# Releasing

Follow the [shared release procedure](https://github.com/hakulab/plugins/blob/main/plugins/hakula-workflow/skills/release/SKILL.md) and devkit's [release guide](https://github.com/hakulab/devkit/blob/v3/docs/releasing.md). This page records oxide-code's versioning, release preparation, and publication contracts.

## Versioning

- **Through 0.1.0:** tag `v0.1.0-alpha.N` from `main` for each user-visible batch or fix. Once the [rc gates](#stable-gates-for-010) pass, tag `v0.1.0-rc.1` (no beta stage).
- **Soak:** each rc starts a seven-day soak. Any bug found joins the next rc's milestone, ships in the next rc, and restarts the soak. After a soak ends without a new bug, tag `main` as `v0.1.0`.
- **Main during a soak:** merge only `fix`, `docs`, `test`, and `chore(release)` PRs so the stable commit matches the soaked rc plus its fixes, holding feature and refactor PRs until after the tag. Disable Renovate automerges by setting `"enabled": false` in `.github/renovate.json` at soak start and reverting after the stable tag (both via `ci(renovate)` PRs).
- **After 0.1.0:** breaking changes to the [stability surface](#stability-surface) bump the minor version, and other changes bump the patch version. Prereleases of the next minor (`v0.2.0-alpha.N`) follow the same rules.
- **Milestones:** each planned release batch (including patches and prereleases) gets its own milestone, such as `v0.2.0` or `v0.2.0-alpha.1`.
- **Breaking changes:** mark stability-surface breaks with [`!`](docs/conventions/github.md#commit-titles) in the PR title both before and after 0.1.0 (before 0.1.0, versions still advance within prereleases).
- **MSRV:** CI's MSRV job checks `rust-version`. Bumps affect `cargo install`, so ship them as normal changes naming the new floor in the PR title.

### Stability Surface

Within a minor version, stable releases keep these compatible:

- CLI flags, subcommands, and exit codes
- Config keys and file locations, plus environment variables
- The flake's `packages`, `overlays.default`, and `homeModules.default` outputs, and the module's options
- Persisted formats: the session store, versioned by its schema revision and each session's content format, the session export that `ox export` writes, and theme TOML
- Output from headless mode (`-p`), as defined in the [compatibility notes](docs/guide/headless.md#compatibility)
- Release asset names, which install scripts and the Homebrew formula depend on

The TUI layout, prompt text, slash-command output wording, and log format are outside the surface.

Before 0.1.0, config, environment variable, and session formats may break freely with no migration hints, legacy fallbacks, or compatibility shims.

## Release Preparation

Use devkit's shared changelog configuration. Security fixes describe the gap, affected releases, and published advisory as [SECURITY.md](SECURITY.md) promises (add later-published advisories to every affected changelog section via a correction PR).

1. Set `[workspace.package].version` in `Cargo.toml`, then run `cargo update --workspace`.

2. Set the `VERSION=` line in `docs/guide/install.md` to the release tag.

3. For a stable tag, add the two [session resume fixtures](docs/design/persistence.md#compatibility) by stripping insta headers (`sed` through the second `---`) from the SQL dump and export snapshots, then record them with `INSTA_UPDATE=unseen` (still failing if an earlier release's fixture resumes differently):

   ```bash
   session=crates/ox-core/src/session
   snapshot="$session/snapshots/ox_core__session__handle__tests__session_format"
   mkdir -p "$session/fixtures"
   sed '1,/^---$/d' "${snapshot}__writes_every_row_kind.snap" >"$session/fixtures/vX.Y.Z.sql"
   sed '1,/^---$/d' "${snapshot}__exports_every_row_kind.snap" >"$session/fixtures/vX.Y.Z.jsonl"
   INSTA_UPDATE=unseen cargo test -p ox-core --lib session_format::resumes_from_every_release_fixture
   ```

Include session compatibility checks in the [verification block](AGENTS.md#verification) for stable releases.

## Verify Publication

After the [Release workflow](https://github.com/hakulab/oxide-code/actions/workflows/release.yml) completes, verify:

- Every [target](#targets) archive and its `.sha256` sidecar are attached, and `gh attestation verify <archive> --repo hakulab/oxide-code` passes for one of them.
- `nix run github:hakulab/oxide-code/vX.Y.Z -- --version` prints the version on one host.

Successful publication notifies the [Homebrew tap](https://github.com/hakulab/homebrew-tap#maintain-formulae). Check its formula update PR.

## Recovery

Asset rebuilds use the original commit for the same tag. A source change requires a new version and release PR.

## Stable Gates for 0.1.0

Before tagging `v0.1.0-rc.1`:

- All planned prerelease work is complete, and the [`v0.1.0` milestone](https://github.com/hakulab/oxide-code/milestone/1) holds no open issue except the release work for these gates.
- CI on `main` passes the full test suite on Linux, macOS, and Windows ([#127](https://github.com/hakulab/oxide-code/issues/127)).
- Each auth mode works live in a TUI turn that calls a tool and a `-p` run: Claude Code OAuth login, a long-lived OAuth token ([#121](https://github.com/hakulab/oxide-code/issues/121)), an API key, and a gateway with a bearer token and custom headers ([#122](https://github.com/hakulab/oxide-code/issues/122)). Every Claude 5.5 model answers at least one turn. Record the matrix in the rc PR's Verification section.
- The persisted formats are documented: the session store and export in [`docs/design/persistence.md`](docs/design/persistence.md), config keys in [`docs/guide/configuration.md`](docs/guide/configuration.md) and [`docs/guide/permissions.md`](docs/guide/permissions.md), and the theme TOML in [`docs/guide/theming.md`](docs/guide/theming.md).
- `SECURITY.md` exists ([#200](https://github.com/hakulab/oxide-code/issues/200)), and every archive ships `THIRD-PARTY-NOTICES` ([#207](https://github.com/hakulab/oxide-code/issues/207)).

Before tagging `v0.1.0`:

- Every rc gate still holds, and `gh api repos/hakulab/oxide-code/milestones/1 --jq .open_issues` reports 0.
- A whole-repo review at the rc commit has no open findings. Fix each finding or file it into the milestone.
- The rc serves as the daily driver for the full soak without a new bug.

The `v0.1.0` release PR also switches the install snippets to `releases/latest/download/`, then removes the `VERSION=` lines and the [Release Preparation](#release-preparation) step that pins them. Because it adds the first session resume fixtures, it also drops `allow_empty_glob` from the fixture test in `crates/ox-core/src/session/handle.rs`, so the test fails again when no fixture matches.

## Targets

The build matrix in `release.yml` lists the targets. Each release attaches one archive per target with a `.sha256` sidecar and a build provenance attestation, and release asset names belong to the [stability surface](#stability-surface).

- The release jobs read the platform floors from `docs/guide/install.md`, so keep its `Linux with glibc <version> or later` and `**macOS**: <version> or later` wording. The Linux job fails when the binary needs a newer glibc, which a runner image update can cause, so pin the previous image or raise the floor. The macOS job fails when the binary's minimum macOS differs from the floor, and raising it also changes `MACOSX_DEPLOYMENT_TARGET` in `release.yml` and `depends_on macos:` in the [tap formula template](https://github.com/hakulab/homebrew-tap/blob/main/scripts/formula/templates/oxide-code.rb).
- Add a target by extending the matrix. Each target needs a runner that executes its binary, since every build job checks its archive there.
