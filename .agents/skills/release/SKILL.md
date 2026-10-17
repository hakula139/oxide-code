---
name: release
description: Cut an oxide-code release tag. Use whenever the user asks to release, tag, ship, or cut version X.Y.Z of oxide-code (e.g., "release v0.1.0", "let's tag 0.2.0-rc.1", "ship the release"). Follows the canonical procedure in `RELEASING.md`. Use even if the user just says "release X.Y.Z" without further context.
---

# Cut an oxide-code release

Read the root `RELEASING.md` and follow its release procedure step by step. It is the source of truth for releases, and these steps are the easiest to skip:

- Before opening the release PR, curate the changelog section drafted by the script, following [Curate the section](../../../RELEASING.md#curate-the-section).
- Ask for approval to push the tag separately from approval of the release PR.
- Tag the release PR's squash commit by its SHA.
- Watch the Release workflow with `gh run watch` until it finishes, then run the publication checks.
- Merge the Homebrew formula PR opened by the workflow, since that is the final release step.
- Report the URLs of the release PR, the GitHub Release, and the formula PR.
