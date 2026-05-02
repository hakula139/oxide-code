# Slash Commands

Slash commands are built-in shortcuts that run client-side, without involving the model. Type `/` to open the autocomplete popup, browse with Up / Down, and complete with Tab.

## Built-in Commands

| Command                             | Description                                                             |
| ----------------------------------- | ----------------------------------------------------------------------- |
| `/clear` (aliases `/new`, `/reset`) | Start a fresh session. The previous one stays resumable via `ox -c`.    |
| `/config`                           | Show the resolved configuration and the file paths it merged.           |
| `/diff`                             | Show `git diff HEAD` plus untracked files, capped at 64 KB.             |
| `/help`                             | List available commands.                                                |
| `/init`                             | Generate or update the project's `AGENTS.md` / `CLAUDE.md`.             |
| `/model [<id>]`                     | List models or swap the active one (a unique substring of an id works). |
| `/status`                           | Show model, working directory, version, auth, session id.               |

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

Read-only commands (`/config`, `/diff`, `/help`, `/status`) are safe to run while the agent is streaming. State-mutating commands (`/clear`, `/init`, `/model`) refuse mid-turn — wait for the current turn to finish, then retry.

## Switching the Model

`/model` (no argument) prints the model table with the active row marked. `/model <id>` swaps to the matching row; a unique substring works (`/model haiku-4-5` resolves to `claude-haiku-4-5`). Family-base ids (`/model opus-4`) are intentionally ambiguous — type the version (`opus-4-7`).

The swap re-clamps `effort` against the new model's ceiling (e.g. `xhigh` on Opus 4.7 → `high` on Sonnet 4.6). The original pick is not preserved across swap-back; restart restores it from your config or `ANTHROPIC_EFFORT`.

## Stance: No Silent Config Writes

Slash commands never write user config files. Runtime mutations (`/model` today, `/theme` later) stay session-local; restart returns to the user-declared configuration. Persisting a slash-command choice across restarts will require an explicit subcommand writing to an explicit user-opted-in path — never a silent merge.
