# Slash Commands

Type `/` to open the autocomplete popup. **Up / Down** browse, **Tab** completes, **Enter** submits, **Esc** dismisses. Aliases (`/clear (new, reset)`) all route to the same command.

For commands with curated arguments (`/model`, `/effort`, `/theme`), the popup switches to **arg mode** after `/<name>` + space, listing valid values.

## Built-in Commands

| Command                                     | Description                                                                                 |
| ------------------------------------------- | ------------------------------------------------------------------------------------------- |
| `/archive [<id-prefix>]`                    | Archive this session and start a fresh one, or archive a saved session by id prefix.        |
| `/clear` (aliases `/new`, `/reset`)         | Start a fresh session. The previous one stays resumable via `ox -c`.                        |
| `/compact [<instructions>]`                 | Compress the conversation into a summary. Trailing text steers the focus.                   |
| `/config`                                   | Open the resolved configuration, where each value came from, and the loaded config files.   |
| `/delete <id-prefix>`                       | Delete a saved session by id prefix, with a Y/N confirm modal before the unlink.            |
| `/diff`                                     | Show uncommitted git changes plus untracked files in chat, capped at 64 KB.                 |
| `/effort [<level>]`                         | Open the slider, or set the tier directly (`low`, `medium`, `high`, `xhigh`, `max`).        |
| `/help`                                     | Open a read-only modal listing available commands.                                          |
| `/init`                                     | Generate or update the project's `AGENTS.md` / `CLAUDE.md`.                                 |
| `/login`                                    | Sign in to a Claude account in the browser and store the login for oxide-code.              |
| `/logout`                                   | Remove the oxide-code login. The Claude Code login stays in place.                          |
| `/model [<id>]`                             | Open the model + effort picker, or swap directly (alias / substring / exact id).            |
| `/rename [<title>]`                         | Open a single-line modal pre-filled with the current title, or set it directly.             |
| `/resume [<id-prefix>]` (alias `/continue`) | Open the session picker (search, scope and archived toggles), or jump by id prefix.         |
| `/status`                                   | Open a read-only modal of model, effort, cwd, version, auth, session id, and session rules. |
| `/theme [<name>]`                           | Open the theme picker (live preview), or swap directly to a built-in theme.                 |
| `/unarchive <id-prefix>`                    | List an archived session again.                                                             |

## Sending a Literal `/foo`

Double the leading slash. Typing `//etc` sends the literal `/etc`.

## Mid-Turn Behavior

State-mutating commands are refused while a turn is in flight, so retry them after the turn finishes. That includes `/archive`, `/clear`, `/compact`, `/delete`, `/init`, `/login`, `/logout`, `/rename`, `/resume`, typed `/theme <name>`, `/unarchive`, and both bare and typed forms of `/effort` and `/model`. Read-only commands (`/config`, `/diff`, `/help`, `/status`) and the bare `/theme` picker run immediately.

## Model and Effort

Bare `/model` and `/effort` open pickers. Both apply on Enter and cancel on Esc.

`/model <id>` accepts a full id or any unique substring of one, so `opus`, `sonnet`, and `haiku` work as aliases. Swapping models keeps the current effort. `/effort` errors on a model id outside the supported catalogue. See [Configuration](configuration.md) for per-model effort defaults.

## Compaction

`/compact` summarizes the visible conversation into a single `Compacted N messages → 1 summary` block. The next prompt continues from that summary, which replaces the prior chat in the model's context.

`/compact <instructions>` appends free-text focus instructions to the rubric (e.g., `/compact focus on the build error and how we fixed it`). Useful when only a slice of the work matters going forward.

The pre-compact transcript stays on disk, but resuming via `ox -c` starts from the compacted summary and post-compact tail. The file-change tracker resets on compact, so any `Edit` after `/compact` requires a fresh `Read`. Queued prompts survive the compaction.

`/compact` refuses to run if fewer than 4 messages contain text or if the model returns an empty summary.

## Signing In

`/login` opens the Claude sign-in page in your browser and prints its URL in case the browser does not open. The sign-in returns to a local address, so finish it in a browser on the machine running `ox`. Signing in from a browser on another machine, such as over SSH, is tracked in [#239](https://github.com/hakula139/oxide-code/issues/239). After you approve access, oxide-code stores the login in `$XDG_DATA_HOME/ox/credentials.json` (default: `~/.local/share/ox/credentials.json`) and uses it for the rest of the session and for later sessions. Press **Esc** to cancel a sign-in, which also ends on its own after 5 minutes.

`/logout` deletes that file. A session that was using it switches to the Claude Code login when one is available, or stays open signed out until the next `/login`. When the Claude Code login exists but cannot be used, for example because it expired, `/logout` says why. Both commands end by naming the credential requests use. An environment variable or `client.api_key` takes precedence over both logins, so `/login` says when one of them keeps the session on that credential. See [Authentication](configuration.md#authentication) for the full order.

## Sessions

`/rename`, `/resume`, `/delete`, `/archive`, and `/unarchive` manage session titles and saved sessions without leaving the TUI. See [Titles](sessions.md#titles), [Mid-Session Resume](sessions.md#mid-session-resume), [Deleting a Session](sessions.md#deleting-a-session), and [Archiving a Session](sessions.md#archiving-a-session).

## Theme

`/theme` opens the picker. **Up / Down** preview each theme live, number keys jump to a row, **Enter** applies for the rest of the session, and **Esc** reverts.

`/theme <name>` swaps directly to a built-in (`mocha`, `macchiato`, `frappe`, `latte`, `material`). Custom themes go in `~/.config/ox/config.toml` under `[tui.theme] base`.

## Persistence

Slash-command choices stay session-local and no slash command writes config files, so a restart returns to your configured values.
