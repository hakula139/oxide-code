---
name: release
description: Cut an oxide-code release tag. Use whenever the user asks to release, tag, ship, or cut version X.Y.Z of oxide-code (e.g., "release v0.1.0", "let's tag 0.2.0-rc.1", "ship the release"). Wraps the canonical procedure in `RELEASING.md` with reminders about the common pitfalls (local checks before pushing, cliff `--prepend` vs `--output`, the manual compare-link footer). Use even if the user just says "release X.Y.Z" without further context.
---

# Cut an oxide-code release

Read `RELEASING.md` at the repo root and execute the procedure step by step. It is the source of truth. Do not improvise around it.

The release lands through a PR, never a direct push to `main`: bump and changelog on a branch, CI green, review, merge, then tag the merged commit. Tagging a reviewed CI-green commit avoids amend-or-re-cut recovery when something turns out wrong.

## Reminders

These are the steps where mistakes are most likely, in procedure order:

- **Run the local checks before pushing.** `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `pnpm lint`, and `pnpm spellcheck` are the same gates CI runs. Run them locally first so the PR does not bounce on something avoidable.
- **Spell-check catches changelog crate names.** `cspell` scans `CHANGELOG.md`, and the new section is derived verbatim from commit subjects. A dependency bump like `Bump idna ...` drops a crate name the dictionary does not know. Add the bare word to `.cspell/words.txt` in its case-insensitive alphabetical slot. Never reword the changelog to dodge it, since cliff must be able to regenerate the body.
- **Cliff invocation.** Use `git cliff --unreleased --tag vX.Y.Z --prepend CHANGELOG.md`. Never `--output`, since that resurfaces pre-release tags as phantom sections.
- **Compare-link footer.** `--prepend` does not regenerate the footer block. Add the new line by hand above the previous-version line. The very first tag has no compare base, so use the `releases/tag/<tag>` form instead.
- **PR diff sanity check.** Only `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, and possibly `.cspell/words.txt` should change. Anything else means an unrelated edit slipped in.
- **Tag only after merge.** Wait for the PR to merge with CI green, then tag the merge commit. Pushing the tag triggers the release workflow and is hard to undo, so confirm with the user before `git push origin vX.Y.Z`.
- **Homebrew formula.** After the release workflow finishes uploading assets, run `./scripts/update-homebrew-formula.sh vX.Y.Z` on its own branch and open a second PR. The script fetches the `.sha256` sidecars, so it 404s if run before uploads complete.

After the tag is pushed, watch the workflow with `gh run watch` and report the resulting release URL.
