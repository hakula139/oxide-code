---
name: release
description: Cut an oxide-code release tag. Use whenever the user asks to release, tag, ship, or cut version X.Y.Z of oxide-code (e.g., "release v0.1.0", "let's tag 0.2.0-rc.1", "ship the release"). Wraps the canonical procedure in `RELEASING.md` with reminders about the common pitfalls (cliff `--prepend` vs `--output`, the manual compare-link footer). Use even if the user just says "release X.Y.Z" without further context.
---

# Cut an oxide-code release

Read `RELEASING.md` at the repo root and execute the procedure step by step. It is the source of truth — do not improvise around it.

## Reminders

These are the steps where mistakes are most likely:

- **Cliff invocation.** Use `git cliff --unreleased --tag vX.Y.Z --prepend CHANGELOG.md`. Never `--output` — that resurfaces pre-release tags as phantom sections.
- **Compare-link footer.** `--prepend` does not regenerate the footer block. After cliff runs, add the new line by hand above the previous-version line. The very first tag has no compare base, so use the `releases/tag/<tag>` form instead.
- **Diff sanity check.** Only `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md` should change. Anything else means an unrelated edit slipped in.
- **Tag confirmation.** Pushing the tag triggers the GitHub release workflow and is hard to undo cleanly. Confirm with the user before `git push origin vX.Y.Z`, even if they already approved the version bump.

After the tag is pushed, watch the workflow with `gh run watch` and report the resulting release URL.
