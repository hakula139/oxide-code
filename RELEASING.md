# Releasing

Pushing a tag that matches `v[0-9]+.*` starts `.github/workflows/release.yml`, which runs CI, builds and checks every target, and publishes a GitHub Release with the tag's `CHANGELOG.md` section as its notes. A tag with a prerelease suffix, such as `v0.1.0-rc.1`, publishes a prerelease.

## Versioning

- **Through 0.1.0:** tag `v0.1.0-alpha.N` from `main` whenever a user-visible batch or a fix for shipped behavior lands. Once only the release gates remain open in the [`v0.1.0` milestone](https://github.com/hakula139/oxide-code/milestone/1), tag `v0.1.0-rc.1`. There is no beta stage.
- **Soak:** each rc starts a seven-day soak. A bug found during the soak joins the milestone, its fix ships in the next rc, and the soak restarts. Once a soak ends without a new bug, tag `main` as `v0.1.0`.
- **Main during a soak:** merge only `fix`, `docs`, `test`, and `chore(release)` PRs. Feature and refactor PRs wait for the stable tag, so the stable commit matches the soaked rc plus its fixes. Renovate automerges dependency bumps, so set `"enabled": false` in `.github/renovate.json` when the soak starts and revert it after the stable tag, both through `ci(renovate)` PRs.
- **After 0.1.0:** a breaking change to the [stability surface](#stability-surface) bumps the minor version, and every other change bumps the patch version. Give a stable version a milestone (`v0.2.0`) once its scope is planned. Patch releases and prereleases get no milestone, and prereleases of the next minor (`v0.2.0-alpha.N`) follow the rules above.
- **Breaking changes:** put `!` in the title of every PR that breaks the stability surface, as in `feat(config)!: ...`, both before and after 0.1.0. Before 0.1.0, the version still advances within the prerelease sequence.
- **MSRV:** the CI MSRV job checks `rust-version`. A bump affects only `cargo install` users, so it ships as a normal change that names the new floor in its PR title.

### Stability surface

Within a minor version, stable releases keep these compatible:

- CLI flags, subcommands, and exit codes
- Config keys and file locations, plus environment variables
- Persisted formats: session JSONL, versioned by its header entry, and theme TOML
- Headless (`-p`) output
- Release asset names, which install scripts and the Homebrew formula depend on

The TUI layout, prompt text, slash-command output wording, and log format are outside the surface.

## Changelog

[git-cliff](https://git-cliff.org) generates `CHANGELOG.md` from `cliff.toml`, so never edit the file by hand.

- Each squash-merged PR title becomes one entry, so write PR titles for users.
- `feat`, `fix`, `perf`, and `revert` titles land under Added, Fixed, Changed, and Removed, and `!` titles land under Breaking changes. Every other type is skipped, including routine `chore(deps)` and `chore(flake)` bumps. Title a bump that fixes an advisory or changes behavior as `fix(deps): ...`.
- A stable section rolls up every change since the previous stable release. A prerelease section lists only the changes since the previous tag.
- Highlights come from the annotated tag message, so regenerating a section reproduces them.
- `--prepend` adds one section and keeps the rest. Avoid `--output CHANGELOG.md`, which regenerates the whole file with a single tag pattern, so it drops either the prerelease sections or the stable roll-ups, along with every compare link.

## Prepare a release

1. Branch off `main` as `chore/release-vX.Y.Z`.
2. Inside the Nix dev shell, which provides git-cliff, run:

   ```bash
   ./scripts/prepare-release.sh vX.Y.Z [highlights.md]
   ```

   The script sets `workspace.package.version`, refreshes `Cargo.lock`, prepends the changelog section with its compare link, and points the `VERSION=` install snippets in `README.md` and `docs/guide/install.md` at the tag. For a notable release, pass a Markdown file of highlights and keep it for tagging.

3. Review the new section. Merged PR titles cannot change on `main`, so reword a misleading entry with a `commit_preprocessors` rule in `cliff.toml`, discard the script's changes, and run it again. The script refuses to run over uncommitted changes to the files it edits.
4. Run the [verification block](AGENTS.md#verification). `cspell` scans `CHANGELOG.md`, so add any unknown word from a title to `.cspell/words.txt`.
5. Commit, then open a PR titled `chore(release): vX.Y.Z`, which becomes the squash commit on `main`, and record the checks in its Test plan. The diff touches only `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, `README.md`, `docs/guide/install.md`, and possibly `cliff.toml` or `.cspell/words.txt`.
6. Merge it after CI passes and review approves. If another PR merges first, recreate the branch from `main` and run the script again, since updating the branch keeps a section that misses the new commit.

## Tag

Pushing the tag starts the Release workflow and is hard to undo, so get approval for it separately from the release PR. Push it soon after the merge, because the pinned install snippets on `main` return 404 until the release publishes.

Tag the release PR's squash commit by its SHA, since a later PR may already sit on top of `main`. Annotate the tag with the highlights file when there is one, and keep every line with `--cleanup=verbatim`, which stops Git from stripping lines that start with `#`:

```bash
git fetch origin
sha=$(gh pr view <release-pr> --json mergeCommit --jq .mergeCommit.oid)
git tag vX.Y.Z "$sha"                                            # without highlights
git tag -a --cleanup=verbatim -F highlights.md vX.Y.Z "$sha"    # with highlights
git push origin vX.Y.Z
```

## Verify publication

Watch the [Release workflow](https://github.com/hakula139/oxide-code/actions/workflows/release.yml) with `gh run watch` and check:

- The release notes match the changelog section, and a prerelease tag published a prerelease.
- Every [target](#targets) archive and its `.sha256` sidecar are attached, and `gh attestation verify <archive> --repo hakula139/oxide-code` passes for one of them.
- `nix run github:hakula139/oxide-code/vX.Y.Z -- --version` prints the version on one host.

The workflow then opens a `chore(release): refresh Homebrew formula for vX.Y.Z` PR and dispatches CI on it. Merging that PR is the final release step, so merge it like any other PR, then check that `brew upgrade oxide-code` installs the version. The formula follows the latest release, or the newest prerelease until the first stable release, so other tags skip this job. If the job fails, fix the cause and rerun it, since a rerun rebuilds the branch and reuses an open PR.

## Re-cut an existing tag

The PR-based flow tags only reviewed, CI-green commits, so re-cutting should be rare. If a release still needs to be redone (e.g., bad assets), let any in-flight release run finish first. To rebuild the same commit, delete only the release with `gh release delete vX.Y.Z --yes` and run the release workflow from the Actions tab against the tag. To release a different commit, delete the release and its tag, then move and push the tag. Recreate an annotated tag with the same highlights file, or its highlights are lost:

```bash
gh release delete vX.Y.Z --cleanup-tag --yes
git tag -f vX.Y.Z <commit>                                           # without highlights
git tag -f -a --cleanup=verbatim -F highlights.md vX.Y.Z <commit>    # with highlights
git push origin vX.Y.Z
```

Either way, the new run updates the open formula PR with the new checksums, or opens a new one.

## Stable gates for 0.1.0

Before tagging `v0.1.0-rc.1`:

- The [`v0.1.0` milestone](https://github.com/hakula139/oxide-code/milestone/1) holds no open issue except the release work for these gates.
- CI on `main` passes the full test suite on Linux, macOS, and Windows ([#127](https://github.com/hakula139/oxide-code/issues/127)).
- Each auth mode works live in a TUI turn that calls a tool, a `-p` run, and a `--no-tui` run: Claude Code OAuth login, a long-lived OAuth token ([#121](https://github.com/hakula139/oxide-code/issues/121)), an API key, and a gateway with a bearer token and custom headers ([#122](https://github.com/hakula139/oxide-code/issues/122)). Every Claude 5.5 model answers at least one turn. Record the matrix in the rc PR's Test plan.
- The persisted formats are documented: session JSONL in [`docs/design/session/persistence.md`](docs/design/session/persistence.md), config keys in [`docs/guide/configuration.md`](docs/guide/configuration.md), and theme TOML in [`docs/guide/theming.md`](docs/guide/theming.md). Tests resume a transcript written by every earlier prerelease ([#208](https://github.com/hakula139/oxide-code/issues/208)).
- `SECURITY.md` exists ([#200](https://github.com/hakula139/oxide-code/issues/200)), and every archive ships `THIRD-PARTY-NOTICES` ([#207](https://github.com/hakula139/oxide-code/issues/207)).

Before tagging `v0.1.0`:

- Every rc gate still holds, and `gh api repos/hakula139/oxide-code/milestones/1 --jq .open_issues` reports 0.
- A whole-repo review at the rc commit has no open findings. Fix each finding or file it into the milestone.
- The rc serves as the daily driver for the full soak without a new bug.

The `v0.1.0` release PR also switches the install snippets to `releases/latest/download/`, then removes the `VERSION=` lines and the step in `scripts/prepare-release.sh` that rewrites them.

## Targets

The matrix in `release.yml` builds `x86_64-unknown-linux-gnu` on the latest Ubuntu runner, `aarch64-apple-darwin` on macOS, and `x86_64-pc-windows-msvc` on Windows. Each release attaches `oxide-code-<target>.tar.gz`, or `.zip` on Windows, with an `oxide-code-<target>.sha256` sidecar and a build provenance attestation, and each archive contains the `ox` binary, `LICENSE`, and `THIRD-PARTY-NOTICES`. The build job generates the notices with cargo-about and appends the bundled syntect asset licenses from `licenses/`.

- Every build job extracts its archive on its own runner, checks both license files against their inputs, and checks that `ox --version` reports the tag's version, or the Cargo version outside a tag.
- The Linux job fails when the binary needs a newer glibc than the floor in `docs/guide/install.md`. The check reads the floor from the guide's OS line. If a runner image update trips it, either pin the Linux build to the previous image or raise the floor in the guide.
- Publishing also waits for the full CI workflow, which the release workflow calls on the tag.
- Pull requests that change `release.yml`, the notice configuration or template, bundled asset licenses, the Cargo manifests, `Cargo.lock`, or `rust-toolchain.toml` build and check every target without publishing. Running the workflow from the Actions tab on a branch adds the CI run.
- Add a target by extending the matrix, since the archive name template (`oxide-code-$target`) and `bin: ox` apply uniformly. Each target needs a runner that executes its binary.
