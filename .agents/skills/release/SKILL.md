---
name: release
description: Cut an oxide-code release tag. Use whenever the user asks to release, tag, ship, or cut version X.Y.Z of oxide-code (e.g., "release v0.1.0", "let's tag 0.2.0-rc.1", "ship the release"). Follows the canonical procedure in `RELEASING.md`. Use even if the user just says "release X.Y.Z" without further context.
---

# Cut an oxide-code release

Read the root `RELEASING.md` and follow its release procedure step by step. It is the source of truth for releases.

After the tag is pushed, watch the workflow with `gh run watch`, then report the release URL and the Homebrew formula PR, if the workflow opened one.
