# Releasing

Releases are produced by `.github/workflows/release.yml`, triggered when a tag matching `v[0-9]+.*` is pushed.

`CHANGELOG.md` is fully auto-generated from Conventional Commits via [`git-cliff`](https://git-cliff.org) — do not hand-edit it. The `cliff.toml` config groups commits into Keep a Changelog sections (`Breaking changes`, `Added`, `Fixed`, `Changed`, `Removed`, `Dependencies`) and is the single source of truth for both the in-repo changelog and GitHub Release notes (the `taiki-e/create-gh-release-action` step extracts the matching version section).

Any prose that should land in the changelog must come from a commit message: use `feat!:` / `fix!:` (or `feat(scope)!:` etc.) on PRs that introduce breaking changes so they surface in the `Breaking changes` section. Inline HTML in commit subjects is auto-backticked by a `commit_preprocessors` rule, so a subject like `feat(tui)!: render <pre> with syntax tint` renders correctly in the changelog without manual escaping.

## Standard release

1. Bump version in `Cargo.toml` (`workspace.package.version`).

2. Run `cargo build` to refresh `Cargo.lock`.

3. Prepend the new changelog section:

   ```bash
   git cliff --unreleased --tag vX.Y.Z --prepend CHANGELOG.md
   ```

   Use `--unreleased --prepend`, not `--output`. `--output` re-derives the whole file and resurfaces past pre-release tags as separate sections.

   Inspect the diff to confirm the new section reads well. If it doesn't, fix the underlying commits (rebase, amend, reword the squash commit subject) and regenerate — never edit the body sections of `CHANGELOG.md` directly.

4. Add the compare-link footer line manually. `--prepend` does not touch the footer block, so insert this line above the previous-version line: `[X.Y.Z]: https://github.com/hakula139/oxide-code/compare/<prev-tag>..vX.Y.Z`. For the very first tag, use `https://github.com/hakula139/oxide-code/releases/tag/vX.Y.Z` instead (no compare base).

5. Commit: `chore(release): vX.Y.Z`.

6. Tag and push:

   ```bash
   git tag vX.Y.Z
   git push origin main
   git push origin vX.Y.Z
   ```

7. The workflow creates the GitHub Release, extracting the matching `[X.Y.Z]` section from `CHANGELOG.md` as release notes, and uploads:

   - `oxide-code-x86_64-unknown-linux-gnu.tar.gz` (+ `.sha256`)
   - `oxide-code-aarch64-apple-darwin.tar.gz` (+ `.sha256`)
   - `oxide-code-x86_64-apple-darwin.tar.gz` (+ `.sha256`)
   - `oxide-code-x86_64-pc-windows-msvc.zip` (+ `.sha256`)

   Each archive contains the `ox` binary.

## Installing `git-cliff`

```bash
brew install git-cliff       # macOS / Homebrew
cargo install git-cliff      # any platform with cargo
```

## Re-cutting an existing tag

If a release needs to be redone (e.g., bad assets):

```bash
gh release delete vX.Y.Z --cleanup-tag --yes
git tag vX.Y.Z               # re-tag locally on the desired commit
git push origin vX.Y.Z
```

Or trigger `workflow_dispatch` from the Actions tab against the existing tag.

## Targets

The matrix in `release.yml` covers `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`, and `x86_64-pc-windows-msvc`. Add new targets by extending that matrix; the archive name template (`oxide-code-$target`) and `bin: ox` apply uniformly.
