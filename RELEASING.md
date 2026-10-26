# Releasing

Pushing a tag that matches `v[0-9]+.*` starts `.github/workflows/release.yml`, which runs CI, builds and checks every target, and publishes a GitHub Release with the tag's `CHANGELOG.md` section as its notes. A tag with a prerelease suffix, such as `v0.1.0-rc.1`, publishes a prerelease.

## Versioning

- **Through 0.1.0:** tag `v0.1.0-alpha.N` from `main` whenever a user-visible batch or a fix for shipped behavior lands. Once only the release gates remain open in the [`v0.1.0` milestone](https://github.com/hakula139/oxide-code/milestone/1), tag `v0.1.0-rc.1`. There is no beta stage.
- **Soak:** each rc starts a seven-day soak. A bug found during the soak joins the milestone, its fix ships in the next rc, and the soak restarts. Once a soak ends without a new bug, tag `main` as `v0.1.0`.
- **Main during a soak:** merge only `fix`, `docs`, `test`, and `chore(release)` PRs. Feature and refactor PRs wait for the stable tag, so the stable commit matches the soaked rc plus its fixes. Renovate automerges dependency bumps, so set `"enabled": false` in `.github/renovate.json` when the soak starts and revert it after the stable tag, both through `ci(renovate)` PRs.
- **After 0.1.0:** a breaking change to the [stability surface](#stability-surface) bumps the minor version, and every other change bumps the patch version. Give a stable version a milestone (`v0.2.0`) once its scope is planned. Patch releases and prereleases get no milestone, and prereleases of the next minor (`v0.2.0-alpha.N`) follow the rules above.
- **Breaking changes:** a PR that breaks the stability surface marks its title with [`!`](docs/conventions/github.md#commits-and-branches) both before and after 0.1.0. Before 0.1.0, the version still advances within the prerelease sequence.
- **MSRV:** the CI MSRV job checks `rust-version`. A bump affects only `cargo install` users, so it ships as a normal change that names the new floor in its PR title.

### Stability Surface

Within a minor version, stable releases keep these compatible:

- CLI flags, subcommands, and exit codes
- Config keys and file locations, plus environment variables
- The flake's `packages`, `overlays.default`, and `homeModules.default` outputs, and the module's options
- Persisted formats: session JSONL, versioned by its header entry, and theme TOML
- Output from headless mode (`-p`), as defined in the [compatibility notes](docs/guide/headless.md#compatibility)
- Release asset names, which install scripts and the Homebrew formula depend on

The TUI layout, prompt text, slash-command output wording, and log format are outside the surface.

Before 0.1.0, config, environment variable, and session format changes may freely break earlier setups and files, with no migration hints, legacy fallbacks, or compatibility shims.

## Changelog

[git-cliff](https://git-cliff.org) drafts each release's `CHANGELOG.md` section from `cliff.toml`, and the release PR curates the draft into the published notes. Curate a section only in its release PR after git-cliff drafts it, and never regenerate an older section. The GitHub Release keeps its own copy of the notes, so once a PR that corrects a published section merges, run the Release workflow on `main` with the tag in its `notes` input, which republishes the tag's section from `main`'s `CHANGELOG.md` and builds nothing:

```bash
gh workflow run release.yml --ref main -f notes=vX.Y.Z
```

- Each squash-merged PR title becomes one draft entry.
- `feat`, `fix`, `perf`, and `revert` titles appear under Added, Fixed, Changed, and Removed, respectively. Titles with `!` appear under Breaking changes. All other types are skipped, including routine `chore(deps)` and `chore(flake)` bumps. Use `fix(deps): ...` for a bump that fixes an advisory or changes behavior in the shipped `ox`. `.github/dependabot.yml` applies this title to every Cargo security update. npm packages are development tools that never ship in `ox`, so their bumps remain `chore`, including advisory fixes.
- Within each group, the draft lists unscoped entries first, then the rest sorted by scope in merge order, so related entries sit together. Each `(#N)` links to its PR.
- A stable section includes every change since the previous stable release. A prerelease section includes only changes since the previous tag.
- `--prepend` adds one section while preserving the rest. Avoid `--output CHANGELOG.md`, which regenerates the whole file with a single tag pattern, so it drops every curated section and compare link, along with either the prerelease sections or the stable roll-ups.

### Curate the Section

The draft lists PR titles, which are too short to explain each change to users. Before opening the release PR, rewrite the draft for users, reading a PR's description wherever its title leaves the effect unclear. Follow these rules:

- For a stable section, start from the curated sections of the prereleases it rolls up and carry over every entry. The draft rebuilds entries from PR titles, so it drops the curated wording and omits PRs whose titles it skips, such as advisory bumps merged as `chore(deps)`. Then add the draft entries for changes since the last prerelease, and apply the rules below to the whole section.
- Describe what users can now do, what no longer breaks, and what now works differently, such as a check that now fails or a value that is now rejected. Omit implementation details.
- For a fix that closes a security gap, such as a permission bypass, state what the gap let through and which releases it affected, and link its published advisory after the PR links, as [SECURITY.md](SECURITY.md) promises. When the advisory is published after the release, add the link in a PR that [corrects each section](#changelog) listing the fix.
- Merge entries that describe the same user-visible change, keeping every PR link, as in `([#223](...), [#224](...))`. Fold a fix to a feature that is new in the same section into that feature's entries, and describe the feature as it now works. A merged entry takes the scope of the feature or main change it describes, and a PR with unrelated effects may be split across entries.
- Move each entry to the group that matches its effect. For example, move a `fix` that adds a capability to Added. Keep entries under Breaking changes there.
- Drop entries with no user-visible effect, such as a CI-only or test-only change titled `fix`.
- Within each group, order entries by user impact and keep their scope labels.
- Describe only what the PRs changed.
- Lead with the highlights when the release has any, and curate them like the entries.

## Prepare a Release

1. Branch off `main` as `chore/release-vX.Y.Z`.

2. Set `version` under `[workspace.package]` in `Cargo.toml` to `X.Y.Z`, then refresh `Cargo.lock`:

   ```bash
   cargo update --workspace
   ```

3. Pin the install snippets to the tag by setting the `VERSION=` line in `README.md` and `docs/guide/install.md` to `VERSION=vX.Y.Z`.

4. For a stable tag, add the release's [session resume fixture](docs/design/session/persistence.md#compatibility), which is the byte-compatibility snapshot with its insta header deleted by `sed` from the first line through the second `---` line. `INSTA_UPDATE=unseen` then records how the new fixture resumes, while still failing if an earlier release's fixture now resumes differently:

   ```bash
   session=crates/ox-core/src/session
   mkdir -p "$session/fixtures"
   sed '1,/^---$/d' \
     "$session/snapshots/ox_core__session__handle__tests__session_format__writes_every_entry_type_as_byte_compatible_jsonl.snap" \
     >"$session/fixtures/vX.Y.Z.jsonl"
   INSTA_UPDATE=unseen cargo test -p ox-core --lib session_format::resumes_from_every_release_fixture
   ```

5. Inside the Nix dev shell, which provides git-cliff, prepend the changelog section. Because `cliff.toml` bounds sections at stable tags only, a prerelease adds a `--tag-pattern` that bounds its section at every tag:

   ```bash
   git cliff --unreleased --tag vX.Y.Z --prepend CHANGELOG.md                                # Stable
   git cliff --unreleased --tag vX.Y.Z --tag-pattern '^v[0-9]+\.' --prepend CHANGELOG.md     # Prerelease
   ```

   For a notable release, list the highlights as Markdown bullets in `highlights.md` and add `--with-tag-message="$(<highlights.md)"`, which renders them under `### Highlights` at the top of the section. The `=` form prevents a message starting with a `-` bullet from parsing as a flag.

6. Because `--prepend` writes no compare link, add the section's link reference above the first one at the end of `CHANGELOG.md`. A stable release compares against the previous stable tag, and a prerelease against the previous tag:

   ```bash
   git describe --tags --abbrev=0 --match 'v[0-9]*' --exclude '*-*'    # Previous stable tag
   git describe --tags --abbrev=0 --match 'v[0-9]*'                    # Previous tag
   ```

   ```markdown
   [X.Y.Z]: https://github.com/hakula139/oxide-code/compare/<previous-tag>..vX.Y.Z
   ```

   When the stable lookup finds no tag, as for `v0.1.0`, link `https://github.com/hakula139/oxide-code/releases/tag/vX.Y.Z` instead.

7. [Curate the section](#curate-the-section) that git-cliff prepended.

8. Run the [verification block](AGENTS.md#verification). Because `cspell` scans `CHANGELOG.md`, add any unknown words from the section to `.cspell/words.txt`.

9. Commit, then open a PR titled `chore(release): vX.Y.Z`, whose title becomes the squash commit title on `main`. Record the checks in the PR's Test plan. The diff should touch only `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, `README.md`, `docs/guide/install.md`, and possibly `.cspell/words.txt`, plus, for a stable tag, the new fixture under `crates/ox-core/src/session/fixtures/` and its snapshot.

10. Merge after CI passes and the review is approved. If another PR merges first, recreate the branch from `main` and repeat the steps before curation, because updating the branch leaves a section that misses the new commit. Carry the curated entries over from the old branch, then curate the new ones.

## Tag

Pushing the tag starts the Release workflow and is hard to undo, so get approval for it separately from the release PR. Push it soon after the merge, because the pinned install snippets on `main` return 404 until the release publishes.

Tag the release PR's squash commit by its SHA, since a later PR may already sit on top of `main`:

```bash
git fetch origin
sha=$(gh pr view <release-pr> --json mergeCommit --jq .mergeCommit.oid)
git tag vX.Y.Z "$sha" && git push origin vX.Y.Z
```

## Verify Publication

Watch the [Release workflow](https://github.com/hakula139/oxide-code/actions/workflows/release.yml) with `gh run watch` and check:

- The release notes match the changelog section, and a prerelease tag published a prerelease.
- Every [target](#targets) archive and its `.sha256` sidecar are attached, and `gh attestation verify <archive> --repo hakula139/oxide-code` passes for one of them.
- `nix run github:hakula139/oxide-code/vX.Y.Z -- --version` prints the version on one host.

The workflow then renders the formula from the release sidecars, installs it through a local tap on `macos-latest` and `ubuntu-latest`, and runs `brew test` there before it opens a `chore(release): refresh Homebrew formula for vX.Y.Z` PR and dispatches CI on it. The same check runs on pull requests that touch the formula, its rendering script, or the release workflows. Merging that PR is the final release step. Because GitHub Actions opened the PR, GitHub holds the PR's own CI run until someone with write access approves it. The dispatched run does not report the required `CI Result` check to the PR either, so the PR stays blocked ([#364](https://github.com/hakula139/oxide-code/issues/364)). Once the dispatched run passes, merge the PR with `gh pr merge --squash --admin`. The formula follows the latest release, or the newest prerelease until the first stable release, so other tags skip this job. If the job fails, fix the cause and rerun it, since a rerun rebuilds the branch and reuses an open PR.

## Re-Cut an Existing Tag

The PR-based flow tags only reviewed, CI-green commits, so re-cutting should be rare. If a release still needs to be redone (e.g., bad assets), let any in-flight release run finish first. To rebuild the same commit, delete only the release with `gh release delete vX.Y.Z --yes` and run the release workflow from the Actions tab against the tag. To release a different commit, delete the release and its tag, then move and push the tag:

```bash
gh release delete vX.Y.Z --cleanup-tag --yes
git tag -f vX.Y.Z <commit> && git push origin vX.Y.Z
```

Either way, the new run updates the open formula PR with the new checksums, or opens a new one.

## Stable Gates for 0.1.0

Before tagging `v0.1.0-rc.1`:

- The [`v0.1.0` milestone](https://github.com/hakula139/oxide-code/milestone/1) holds no open issue except the release work for these gates.
- CI on `main` passes the full test suite on Linux, macOS, and Windows ([#127](https://github.com/hakula139/oxide-code/issues/127)).
- Each auth mode works live in a TUI turn that calls a tool, a `-p` run, and a `--no-tui` run: Claude Code OAuth login, a long-lived OAuth token ([#121](https://github.com/hakula139/oxide-code/issues/121)), an API key, and a gateway with a bearer token and custom headers ([#122](https://github.com/hakula139/oxide-code/issues/122)). Every Claude 5.5 model answers at least one turn. Record the matrix in the rc PR's Test plan.
- The persisted formats are documented: session JSONL in [`docs/design/session/persistence.md`](docs/design/session/persistence.md), config keys in [`docs/guide/configuration.md`](docs/guide/configuration.md) and [`docs/guide/permissions.md`](docs/guide/permissions.md), and the theme TOML in [`docs/guide/theming.md`](docs/guide/theming.md).
- `SECURITY.md` exists ([#200](https://github.com/hakula139/oxide-code/issues/200)), and every archive ships `THIRD-PARTY-NOTICES` ([#207](https://github.com/hakula139/oxide-code/issues/207)).

Before tagging `v0.1.0`:

- Every rc gate still holds, and `gh api repos/hakula139/oxide-code/milestones/1 --jq .open_issues` reports 0.
- A whole-repo review at the rc commit has no open findings. Fix each finding or file it into the milestone.
- The rc serves as the daily driver for the full soak without a new bug.

The `v0.1.0` release PR also switches the install snippets to `releases/latest/download/`, then removes the `VERSION=` lines and the [Prepare a Release](#prepare-a-release) step that pins them. Because it adds the first session resume fixture, it also drops `allow_empty_glob` from the fixture test in `crates/ox-core/src/session/handle.rs`, so the test fails again when no fixture matches.

## Targets

The build matrix in `release.yml` lists the targets. Each release attaches one archive per target with a `.sha256` sidecar and a build provenance attestation, and release asset names belong to the [stability surface](#stability-surface).

- The platform floors in `docs/guide/install.md` bind the build. The Linux job fails when the binary needs a newer glibc than the guide promises, reading the floor from the phrase `Linux with glibc <version> or later`, so keep that wording when editing it. If a runner image update trips the check, either pin the Linux build to the previous image or raise the floor in the guide. The macOS job fails when the binary's minimum macOS version differs from the guide's `**macOS**: <version> or later` floor, so keep that wording too. Raising that floor also means changing `MACOSX_DEPLOYMENT_TARGET` in `release.yml` and the formula's `depends_on macos:` in `scripts/update-homebrew-formula.sh`.
- Add a target by extending the matrix. Each target needs a runner that executes its binary, since every build job checks its archive there.
