# GitHub

Conventions for commits, branches, pull requests, and issues.

## Commits and Branches

- Keep each commit to one logical change. Use the imperative mood for its title, formatted as `type(scope): description`. For substantial changes, add a body explaining the problem, rationale, and tradeoffs.
- Choose one of these types: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `ci`, `build`, `chore`, `style`, or `revert`.
- Use the most specific scope the change touches, such as a module (`client`, `config`, `oauth`) or a documentation target (`AGENTS`, `research`). Use the crate name only for changes that span modules.
- If a change breaks the [stability surface](../../RELEASING.md#stability-surface), add `!` after the scope, as in `feat(config)!: ...`.
- Name branches `<type>/<short-name>`, using the same types.

## Pull Requests

- Merge into `main` only with squash merges. The squash commit uses the PR title, which becomes the PR's draft [changelog](../../RELEASING.md#changelog) entry, so write the title for users.
- Fill in `.github/pull_request_template.md`. Start the Summary with prose describing the problem and resulting behavior, then cover each part of the change in a bullet with a bold label. In the Test plan, list the checks actually run and note any verification limits.
- Add `Closes #N` for each resolved issue.
- Do not request a review from the PR author because GitHub rejects the request.
- Maintainers with triage access assign the PR to `hakula139`, set its milestone to match the issue it closes, and add one type label plus the `area:` labels for the areas it changes:

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
- The `v0.1.0` milestone includes all work that must land before the first stable release. Add an issue when it concerns a bug in shipped behavior or the maintainer accepts it into the release. Otherwise, leave it out of any milestone.
