# Slash Commands

Slash commands are built-in shortcuts that run client-side, without involving the model. Type `/` to open the autocomplete popup, browse with Up / Down, and complete with Tab.

## Built-in Commands

| Command                             | Description                                                                |
| ----------------------------------- | -------------------------------------------------------------------------- |
| `/clear` (aliases `/new`, `/reset`) | Start a fresh session. The previous one stays resumable via `ox -c`.       |
| `/config`                           | Show the resolved configuration and the file paths it merged.              |
| `/diff`                             | Show `git diff HEAD` plus untracked files, capped at 64 KB.                |
| `/help`                             | List available commands.                                                   |
| `/init`                             | Generate or update the project's `AGENTS.md` / `CLAUDE.md`.                |
| `/model [<id>]`                     | List selectable models or swap the active one (alias / substring / exact). |
| `/status`                           | Show model, working directory, version, auth, session id.                  |

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

Read-only commands (`/config`, `/diff`, `/help`, `/status`, bare `/model`) are safe to run while the agent is streaming. State-mutating commands (`/clear`, `/init`, `/model <id>`) refuse mid-turn — wait for the current response to complete, then retry.

## Switching the Model

`/model` (no argument) prints the selectable list with the active row marked by `*`. `/model <id>` swaps to the matching row, resolved in three tiers:

- **Alias** — `/model opus`, `/model sonnet`, `/model haiku` route to the latest non-1M row of each family. `/model opus[1m]` and `/model sonnet[1m]` opt into the 1M-context variant. (Haiku 4.5 has no 1M variant.)
- **Exact id** — `/model claude-opus-4-7` resolves to the bare row, never the 1M variant. `/model claude-opus-4-7[1m]` is required for the 1M variant.
- **Unique substring** — `/model haiku-4-5` resolves to `claude-haiku-4-5`. When two rows match the substring (e.g. `/model opus-4-7` matches both bare and 1M), the error lists every candidate.

Selectable today: Opus 4.7, Sonnet 4.6, Haiku 4.5, plus 1M-context variants of Opus 4.7 and Sonnet 4.6. Older models stay supported by the capability layer (so `model = "claude-opus-4-1"` in your config still gets the right beta headers) but `/model` only swaps within the curated set.

When you swap, your current effort tier is capped to the new model's max — for example, `xhigh` on Opus 4.7 becomes `high` on Sonnet 4.6 since Sonnet 4.6 doesn't accept `xhigh`. The confirmation message says so explicitly (`effort high (clamped from xhigh)`), and `Effort cleared (model has no effort tier)` is shown when the new model accepts no effort tier at all. Swapping back to a model that supported your original effort does not restore it; restart to pick up the value from your config (or `ANTHROPIC_EFFORT` if you set one).

## Stance: No Silent Config Writes

Slash commands never write user config files. Runtime mutations (`/model` today, `/theme` later) stay session-local; restart returns to the user-declared configuration. Persisting a slash-command choice across restarts will require an explicit subcommand writing to an explicit user-opted-in path — never a silent merge.
