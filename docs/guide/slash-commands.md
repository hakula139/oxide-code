# Slash Commands

Slash commands are built-in shortcuts that run client-side, without involving the model. Type `/` to open the autocomplete popup, browse with Up / Down, and complete with Tab.

## Built-in Commands

| Command                             | Description                                                          |
| ----------------------------------- | -------------------------------------------------------------------- |
| `/help`                             | List available commands.                                             |
| `/clear` (aliases `/new`, `/reset`) | Start a fresh session. The previous one stays resumable via `ox -c`. |
| `/init`                             | Generate or update the project's `AGENTS.md` / `CLAUDE.md`.          |
| `/diff`                             | Show `git diff HEAD` plus untracked files, capped at 64 KB.          |
| `/status`                           | Show model, working directory, version, auth, session id.            |
| `/config`                           | Show the resolved configuration and the file paths it merged.        |

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

Read-only commands (`/help`, `/diff`, `/status`, `/config`) are safe to run while the agent is streaming. State-mutating commands (`/clear`, `/init`) refuse mid-turn — wait for the current turn to finish, then retry.

## Stance: No Silent Config Writes

Slash commands never write user config files. Future runtime mutations (`/model`, `/theme`) will stay session-local; restart returns to the user-declared configuration. Persisting a slash-command choice across restarts will require an explicit subcommand writing to an explicit user-opted-in path — never a silent merge.
