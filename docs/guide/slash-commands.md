# Slash Commands

Slash commands are built-in shortcuts that run client-side, without involving the model. Type `/` to open the autocomplete popup, browse with Up / Down, and complete with Tab.

## Built-in Commands

| Command                             | Description                                                                                 |
| ----------------------------------- | ------------------------------------------------------------------------------------------- |
| `/clear` (aliases `/new`, `/reset`) | Start a fresh session. The previous one stays resumable via `ox -c`.                        |
| `/config`                           | Show the resolved configuration and the file paths it merged.                               |
| `/diff`                             | Show `git diff HEAD` plus untracked files, capped at 64 KB.                                 |
| `/effort [<level>\|auto]`           | List effort levels or set the active one (`low`, `medium`, `high`, `xhigh`, `max`, `auto`). |
| `/help`                             | List available commands.                                                                    |
| `/init`                             | Generate or update the project's `AGENTS.md` / `CLAUDE.md`.                                 |
| `/model [<id>]`                     | List models or swap the active one (alias / substring / exact, including older ids).        |
| `/status`                           | Show model, effort, working directory, version, auth, session id.                           |

## Autocomplete Popup

When you type `/`, a two-column popup appears above the input:

- **Up / Down** navigate the rows.
- **Tab** completes the selected command (`/<name>` plus a trailing space).
- **Enter** submits the current line.
- **Esc** dismisses the popup.

Matches are ranked by tier: name-prefix > alias-prefix > name-substring > alias-substring. Aliases display inline in the canonical row (`/clear (new, reset)`); typing any alias routes to the same impl.

## Sending a Literal `/foo`

To send a message that _starts_ with a slash without invoking a command, double the leading slash. Typing `//etc` sends the literal `/etc`.

## Mid-Turn Behavior

Read-only commands (`/config`, `/diff`, `/help`, `/status`, bare `/model`, bare `/effort`) are safe to run while the agent is streaming. State-mutating commands (`/clear`, `/init`, `/model <id>`, `/effort <level>`) refuse mid-turn — wait for the current response to complete, then retry.

## Switching the Model

`/model` (no argument) prints the curated list with the active row marked by `*`. `/model <id>` swaps to the matching row, resolved in three tiers:

- **Alias** — `/model opus`, `/model sonnet`, `/model haiku` route to the latest non-1M row of each family. `/model opus[1m]` and `/model sonnet[1m]` opt into the 1M-context variant. (Haiku 4.5 has no 1M variant — `/model haiku[1m]` errors with a clear message.)
- **Exact id** — `/model claude-opus-4-7` resolves to the bare row. `/model claude-opus-4-7[1m]` is required for the 1M variant. Exact match wins over substring.
- **Unique substring** — `/model haiku-4-5` resolves to `claude-haiku-4-5`. The `[1m]` tag is treated as significant: `/model sonnet-4-6` (no `[1m]`) only considers bare candidates, so it resolves cleanly without colliding with `claude-sonnet-4-6[1m]`.

The curated list (Opus 4.7, Sonnet 4.6, Haiku 4.5, plus 1M variants of Opus 4.7 and Sonnet 4.6) is what the bare list view shows. Manual entry is wider — any id from the model table works, so `/model claude-opus-4-6` or `/model claude-opus-4-1` swap to those older rows even though they aren't in the popup.

When you swap, your current effort tier is capped to the new model's max — for example, `xhigh` on Opus 4.7 becomes `high` on Sonnet 4.6 since Sonnet 4.6 doesn't accept `xhigh`. The confirmation message says so explicitly (`effort high (clamped from xhigh)`), and `Effort cleared (model has no effort tier)` is shown when the new model accepts no effort tier at all. Swapping back to a model that supported your original effort does not restore it; use `/effort <level>` to pick it back up, or restart to pick up the value from your config (or `ANTHROPIC_EFFORT` if you set one).

## Switching the Effort

`/effort` (no argument) lists every level for the active model with the current marked by `*`. Unsupported levels (e.g. `xhigh` on Sonnet 4.6) are annotated `(clamps to high)` so you know what'll happen if you pick.

`/effort <level>` swaps to that tier. Valid: `low`, `medium`, `high`, `xhigh`, `max`. The active model's caps clamp the pick — `/effort xhigh` on Sonnet 4.6 lands on `high` and the confirmation says `effort high (clamped from xhigh)`.

`/effort auto` (alias `unset`) clears your pick so the model's default kicks in (Opus 4.7 → `xhigh`, other effort-capable models → `high`, no-effort models → no parameter).

`/effort xhigh` on a model with no effort tier (Haiku 4.5) errors upfront with a recovery hint to `/model` first — silent acceptance would degrade to "no effort param" and confuse the user.

## Stance: No Silent Config Writes

Slash commands never write user config files. Runtime mutations (`/model`, `/effort` today, `/theme` later) stay session-local; restart returns to the user-declared configuration. Persisting a slash-command choice across restarts will require an explicit subcommand writing to an explicit user-opted-in path — never a silent merge.
