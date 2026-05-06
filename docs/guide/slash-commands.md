# Slash Commands

Slash commands are built-in shortcuts that run client-side, without involving the model. Type `/` to open the autocomplete popup, browse with Up / Down, and complete with Tab.

## Built-in Commands

| Command                             | Description                                                                          |
| ----------------------------------- | ------------------------------------------------------------------------------------ |
| `/clear` (aliases `/new`, `/reset`) | Start a fresh session. The previous one stays resumable via `ox -c`.                 |
| `/config`                           | Show the resolved configuration and the file paths it merged.                        |
| `/diff`                             | Show `git diff HEAD` plus untracked files, capped at 64 KB.                          |
| `/effort [<level>]`                 | Open the slider, or set the tier directly (`low`, `medium`, `high`, `xhigh`, `max`). |
| `/help`                             | List available commands.                                                             |
| `/init`                             | Generate or update the project's `AGENTS.md` / `CLAUDE.md`.                          |
| `/model [<id>]`                     | Open the model + effort picker, or swap directly (alias / substring / exact id).     |
| `/status`                           | Show model, effort, working directory, version, auth, session id.                    |

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

Read-only commands (`/config`, `/diff`, `/help`, `/status`, bare `/model` and bare `/effort` which open modals) are safe to run while the agent is streaming. State-mutating commands (`/clear`, `/init`, `/model <id>`, `/effort <level>`) refuse mid-turn — wait for the current response to complete, then retry.

## Model and Effort

Bare `/model` opens the combined model + effort picker; bare `/effort` opens a Speed ↔ Intelligence slider. Both apply on Enter and cancel on Esc.

`/model <id>` resolves in four tiers: alias (`opus`, `sonnet`, `haiku`, with optional `[1m]` for the 1M-context variants) → exact / dated id → unique suffix → unique substring. Swapping clamps the current effort to the new model's caps; `/effort` on a model without effort (Haiku 4.5) errors with a recovery hint. See [Configuration](configuration.md) for the full tier reference and per-model defaults.

## Stance: No Silent Config Writes

Slash commands never write user config files. Runtime mutations (`/model`, `/effort` today, `/theme` later) stay session-local; restart returns to the user-declared configuration. Persisting a slash-command choice across restarts will require an explicit subcommand writing to an explicit user-opted-in path — never a silent merge.
