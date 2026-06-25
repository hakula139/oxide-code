# Releasing

Releases are produced by `.github/workflows/release.yml`, triggered when a tag matching `v[0-9]+.*` is pushed.

`CHANGELOG.md` is fully auto-generated from Conventional Commits via [`git-cliff`](https://git-cliff.org). Do not hand-edit it. The `cliff.toml` config groups commits into Keep a Changelog sections (`Breaking changes`, `Added`, `Fixed`, `Changed`, `Removed`, `Dependencies`) and is the single source of truth for both the in-repo changelog and GitHub Release notes (the `taiki-e/create-gh-release-action` step extracts the matching version section).

Any prose that should land in the changelog must come from a commit message: use `feat!:` / `fix!:` (or `feat(scope)!:` etc.) on PRs that introduce breaking changes so they surface in the `Breaking changes` section. Inline HTML in commit subjects is auto-backticked by a `commit_preprocessors` rule, so a subject like `feat(tui)!: render <pre> with syntax tint` renders correctly in the changelog without manual escaping.

## Standard release

The release lands through a PR, never a direct push to `main`. The order is: open a PR with the version bump and changelog, let CI pass, review, merge, then tag the merge commit. Tagging a reviewed and CI-green commit removes the need to amend or re-cut when something turns out wrong.

1. Branch off `main`: `git switch -c chore/release-vX.Y.Z`.

2. Bump version in `Cargo.toml` (`workspace.package.version`).

3. Run `cargo build` to refresh `Cargo.lock`.

4. Prepend the new changelog section:

   ```bash
   git cliff --unreleased --tag vX.Y.Z --prepend CHANGELOG.md
   ```

   Use `--unreleased --prepend`. Avoid `--output` because it re-derives the whole file and resurfaces past pre-release tags as separate sections.

   Inspect the diff to confirm the new section reads well. If it does not, fix the underlying commits (rebase, amend, reword the squash commit subject) and regenerate. Never edit the body sections of `CHANGELOG.md` directly.

5. Add the compare-link footer line manually. `--prepend` does not touch the footer block, so insert this line above the previous-version line: `[X.Y.Z]: https://github.com/hakula139/oxide-code/compare/<prev-tag>..vX.Y.Z`. For the very first tag, use `https://github.com/hakula139/oxide-code/releases/tag/vX.Y.Z` instead (no compare base).

6. Run the full local check suite before pushing, so CI does not surface anything avoidable. This is the same set CI gates:

   ```bash
   cargo fmt --all --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   pnpm lint
   pnpm spellcheck
   ```

   `cspell` scans `CHANGELOG.md`, and the new section is derived verbatim from commit subjects. A dependency bump such as `Bump idna ...` drops a crate name the dictionary does not know, which fails `pnpm spellcheck`. Add the bare word to `.cspell/words.txt` in its case-insensitive alphabetical slot. Never reword the changelog to dodge it: cliff must be able to regenerate the body.

7. Commit `chore(release): vX.Y.Z`, push the branch, and open a PR. Only `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, and possibly `.cspell/words.txt` should be in the diff.

8. Wait for CI to pass, fix anything it flags, then have the PR reviewed and merged. Do not tag until the PR is merged and green.

9. Tag the merge commit and push the tag:

   ```bash
   git switch main && git pull
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

10. The workflow creates the GitHub Release, extracting the matching `[X.Y.Z]` section from `CHANGELOG.md` as release notes, and uploads:

    - `oxide-code-x86_64-unknown-linux-gnu.tar.gz` (+ `.sha256`)
    - `oxide-code-aarch64-apple-darwin.tar.gz` (+ `.sha256`)
    - `oxide-code-x86_64-apple-darwin.tar.gz` (+ `.sha256`)
    - `oxide-code-x86_64-pc-windows-msvc.zip` (+ `.sha256`)

    Each archive contains the `ox` binary.

11. Refresh the Homebrew formula against the published artifacts. Run this after the workflow finishes uploading assets, otherwise the sidecar URLs return 404. The script regenerates `Formula/oxide-code.rb` by fetching each `.sha256` sidecar from the release, so it goes through its own PR:

    ```bash
    git switch -c chore/homebrew-vX.Y.Z
    ./scripts/update-homebrew-formula.sh vX.Y.Z
    git commit -am "chore(release): refresh Homebrew formula for vX.Y.Z"
    ```

    Push and open a PR as in steps 7 and 8.

## Installing `git-cliff`

```bash
brew install git-cliff       # macOS / Homebrew
cargo install git-cliff      # any platform with cargo
```

## Re-cutting an existing tag

The PR-based flow tags only reviewed, CI-green commits, so re-cutting should be rare. If a release still needs to be redone (e.g., bad assets), let any in-flight release run finish first so deletion does not race its uploads, then:

```bash
gh release delete vX.Y.Z --cleanup-tag --yes
git tag vX.Y.Z               # re-tag locally on the desired commit
git push origin vX.Y.Z
```

Or trigger `workflow_dispatch` from the Actions tab against the existing tag.

## Targets

The matrix in `release.yml` covers `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`, and `x86_64-pc-windows-msvc`. Add new targets by extending that matrix. The archive name template (`oxide-code-$target`) and `bin: ox` apply uniformly.
