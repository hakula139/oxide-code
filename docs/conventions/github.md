# GitHub

Follow the [shared Git workflow] and devkit's [GitHub conventions]. This page adds oxide-code's changelog, label, and issue requirements.

## Commit Titles

- Use the most specific scope the change touches, such as a module (`client`, `config`, `oauth`) or a documentation target (`AGENTS`, `research`). Use a crate name, such as `ox-core`, only for changes that span its modules, and omit the scope for changes that span crates.
- If a change breaks the [stability surface](../../RELEASING.md#stability-surface), add `!` after the scope, as in `feat(config)!: ...`.

## Pull Requests

- The squashed PR title becomes the draft [changelog](../../RELEASING.md#release-preparation) entry, so write it for users.
- Use the PR template inherited from [`hakula139/.github`](https://github.com/hakula139/.github).
- Maintainers with triage access assign the PR to `hakula139`, set its milestone to the linked issue's milestone or the planned delivery batch when no issue is linked, and add one type label plus the `area:` labels for the areas it changes:

| Commit type               | Label                            |
| ------------------------- | -------------------------------- |
| `feat`                    | `enhancement`                    |
| `fix`                     | `bug`                            |
| `perf`                    | `performance`                    |
| `refactor`                | `refactor`                       |
| `docs`                    | `documentation`                  |
| `test`                    | `test`                           |
| `ci`                      | `ci`                             |
| `build`, `chore`, `style` | `chore`                          |
| `revert`                  | The reverted change's type label |

## Issues

- Open one issue for each actionable outcome using the forms in `.github/ISSUE_TEMPLATE/`, and include acceptance criteria. Label it like a PR.
- Use open issues to track planned work. For a theme spanning several issues, create an `epic` parent with native sub-issues.
- If an issue depends on another landing first, record that order with a native blocked-by dependency.
- Label open design questions `design`, and close them once the decision is documented in `docs/design/`.
- Close out-of-scope requests as not planned, pointing to the Scope section of `README.md`.
- Close an issue as completed when its PR merges, and tick each acceptance criterion the merged work satisfies. Leave an unmet criterion unchecked, and track it in a follow-up issue.
- Before starting new feature work, audit open issues and their epics for unmet acceptance criteria and unfinished sub-issues, and resolve those first.
- Assign issues accepted for a planned delivery batch to that release's milestone, including prereleases. Keep work required for the first stable release in `v0.1.0` until it joins an earlier delivery batch. Leave unplanned work without a milestone.

[shared Git workflow]: https://github.com/hakula139/plugins/blob/main/plugins/hakula-workflow/skills/git-workflow/SKILL.md
[GitHub conventions]: https://github.com/hakula139/devkit/blob/v3/docs/github.md
