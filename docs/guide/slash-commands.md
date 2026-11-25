# Slash Commands

Type `/` to open the command popup (**Up** / **Down** browse, **Tab** completes, **Enter** submits, **Esc** dismisses). Aliases display as `/clear (new)` and appear in `/help`. After `/effort`, `/init`, `/mode`, `/model`, or `/theme`, type a space for **arg mode** to list valid values.

## Built-In Commands

| Command                                     | Description                                                                                                                                   |
| ------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `/archive [<id-prefix>]`                    | Archive this session and start a fresh one, or archive a saved session by ID prefix.                                                          |
| `/clear` (aliases `/new`, `/reset`)         | Start a fresh session. The previous one stays resumable with `/resume` or `ox -r <id-prefix>`.                                                |
| `/compact [<instructions>]`                 | Compress the conversation into a summary. Trailing text steers the focus.                                                                     |
| `/config`                                   | Open the resolved configuration, where each value came from, and the loaded config files.                                                     |
| `/delete <id-prefix>`                       | Delete a saved session by ID prefix, after a **Y** / **N** confirmation.                                                                      |
| `/diff`                                     | Show the repository's uncommitted git changes and untracked files in chat, capped at 64 KiB.                                                  |
| `/effort [<level>]`                         | Open the slider, or set the level directly (`low`, `medium`, `high`, `xhigh`, `max`).                                                         |
| `/exit` (alias `/quit`)                     | Quit oxide-code, even during a turn.                                                                                                          |
| `/help`                                     | Open a read-only list of available commands.                                                                                                  |
| `/init [<file>]`                            | Propose edits to an existing `AGENTS.md` or `CLAUDE.md` as a diff to confirm. Without either, pick or name the one to create.                 |
| `/login`                                    | Sign in to a Claude account in the browser and store the login for oxide-code.                                                                |
| `/logout`                                   | Remove the oxide-code login. The Claude Code login stays in place.                                                                            |
| `/mode [<mode>]`                            | Open the permission mode picker, or switch directly to `auto`, `manual`, `plan`, or `yolo`.                                                   |
| `/model [<id>]`                             | Open the model and effort picker, or switch directly by catalog ID, unique substring, or any other ID a gateway serves.                       |
| `/rename [<title>]`                         | Open a single-line editor pre-filled with the current title, or set it directly.                                                              |
| `/resume [<id-prefix>]` (alias `/continue`) | Open the session picker (search, scope and archived toggles, **Ctrl+X** to archive or unarchive, **Ctrl+D** to delete), or jump by ID prefix. |
| `/status`                                   | Show the model, permission mode, auth, settings, session rules, and background shells.                                                        |
| `/theme [<name>]`                           | Open the theme picker with live preview, or switch directly to a built-in theme.                                                              |
| `/unarchive <id-prefix>`                    | Unarchive a saved session by ID prefix, so it shows in listings again.                                                                        |

Double a leading slash to send literal text (`//etc` sends `/etc`). Model, effort, permission mode, and theme choices last only for the session. No slash command edits config files.

## When Commands Run

Mid-turn, only `/config`, `/diff`, `/exit`, `/help`, `/mode`, `/status`, and `/theme` run (mode switches apply on the next tool call). Run other commands after the turn finishes.

## Pickers

Bare `/effort`, `/mode`, `/model`, and `/theme` open pickers (**Enter** applies, **Esc** cancels). In `/mode`, `/model`, and `/theme`, **Up** / **Down** or a number key selects a row. In `/effort` and `/model`, **Left** / **Right** or **h** / **l** adjusts effort.

- **`/model <id>`**: Accepts a full ID or unique substring (`fable`, `opus`, `sonnet`, `haiku`). Carries current effort across swaps ([`effort`](configuration.md#effort-reasoning-effort) lists defaults).
- **`/effort`**: Fails on models outside the supported catalog, which have no effort levels.
- **`/mode`**: Lists [permission modes](permissions.md#modes) (also cycled with **Tab** / **Shift+Tab**).
- **`/theme`**: Live-previews themes (**Esc** reverts). `/theme <name>` switches to `mocha`, `macchiato`, `frappe`, `latte`, or `material`. For a custom file, set `tui.theme.base` in config ([Theming](theming.md#custom-theme-files)).

## Compaction

`/compact` summarizes the visible conversation into a `Compacted N messages into a summary` block. Trailing text steers the summary, as in `/compact focus on the build error and how we fixed it`.

After compaction:

- **Transcript**: Pre-compact messages stay on disk, while `ox -c` resumes from the summary onward.
- **File tracking**: Resets, so files must be read again before editing.
- **Session state**: Queued prompts, task list, and background shells carry over.

`/compact` requires at least about 1,000 tokens (messages, tool calls, and tool output, excluding instructions and tool definitions). Empty summaries fail and leave the conversation unchanged.

Auto-compaction runs before a prompt and between rounds of a long turn (which continues from the summary). On failure:

- **Before a prompt**: Restores the prompt to the input unsent. Resubmit to retry.
- **Between rounds**: Stops the turn. Run `/compact`, then continue.
- **3 consecutive failures**: Pauses auto-compaction (`/status` shows it paused) and lets prompts proceed. A successful `/compact`, `/model` or `/effort` change, `/clear`, `/archive`, or `/resume` turns it back on.

## Sessions

`/rename`, `/resume`, `/delete`, `/archive`, and `/unarchive` manage sessions in the TUI. See [Titles](sessions.md#titles), [Mid-Session Resume](sessions.md#mid-session-resume), [Deleting a Session](sessions.md#deleting-a-session), and [Archiving a Session](sessions.md#archiving-a-session).
