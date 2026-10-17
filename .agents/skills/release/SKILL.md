---
name: release
description: Cut an oxide-code release tag. Use whenever the user asks to release, tag, ship, or cut version X.Y.Z of oxide-code (e.g., "release v0.1.0", "let's tag 0.2.0-rc.1", "ship the release"). Follows the canonical procedure in `RELEASING.md`. Use even if the user just says "release X.Y.Z" without further context.
---

# Cut an oxide-code Release

Read the root `RELEASING.md` and follow its release procedure step by step. It is the source of truth for releases. These steps are the easiest to skip:

- [Curate the section](../../../RELEASING.md#curate-the-section) before opening the release PR.
- [Tag](../../../RELEASING.md#tag) only after separate approval.
- [Verify publication](../../../RELEASING.md#verify-publication) once the Release workflow finishes.
- If the workflow opens a Homebrew formula PR, merge it as [Verify publication](../../../RELEASING.md#verify-publication) describes.
- Report the URLs of the release PR, the GitHub Release, and any formula PR.
