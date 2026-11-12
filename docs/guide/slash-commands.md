# Slash Commands

Type `/` to open the autocomplete popup. Use **Up / Down** to browse, **Tab** to complete, **Enter** to submit, and **Esc** to dismiss. Typing an alias such as `/new` shows `/clear (new)` in the popup, and `/help` lists every alias. After `/effort`, `/init`, `/mode`, `/model`, or `/theme`, type a space to switch the popup to **arg mode**, which lists the valid values.

## Built-In Commands

| Command                                     | Description                                                                                                                                   |
| ------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `/archive [<id-prefix>]`                    | Archive this session and start a fresh one, or archive a saved session by ID prefix.                                                          |
| `/clear` (aliases `/new`, `/reset`)         | Start a fresh session. The previous one stays resumable with `/resume` or `ox -r <id-prefix>`.                                                |
| `/compact [<instructions>]`                 | Compress the conversation into a summary. Trailing text steers the focus.                                                                     |
| `/config`                                   | Open the resolved configuration, where each value came from, and the loaded config files.                                                     |
| `/delete <id-prefix>`                       | Delete a saved session by ID prefix, after a Y / N confirmation.                                                                              |
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

To send text that starts with `/`, double the slash, so typing `//etc` sends `/etc`.

Model, effort, permission mode, and theme choices last for the session. No slash command writes config files, so a restart returns to your configured values.

## When Commands Run

During a turn, only `/config`, `/diff`, `/exit`, `/help`, `/mode`, `/status`, and `/theme` run. A mode switch applies from the turn's next tool call. Retry other commands after the turn finishes.

## Pickers

Bare `/effort`, `/mode`, `/model`, and `/theme` open pickers that take **Enter** to apply and **Esc** to cancel. In the `/mode`, `/model`, and `/theme` pickers, **Up** / **Down** or a number key moves to a row. In the `/effort` and `/model` pickers, **Left** / **Right** or **h** / **l** steps the effort level.

- **`/model <id>`** accepts a full ID or any unique substring, so `fable`, `opus`, `sonnet`, and `haiku` work as aliases. Switching models keeps the current effort, and [`effort`](configuration.md#effort-reasoning-effort) lists each model's default.
- **`/effort`** fails on a model outside the supported catalog, since that model has no effort levels.
- **`/mode`** lists the [permission modes](permissions.md#modes), which **Tab** and **Shift+Tab** also cycle through.
- **`/theme`** previews each theme live as you move, and **Esc** reverts the preview. `/theme <name>` switches directly to a built-in theme: `mocha`, `macchiato`, `frappe`, `latte`, or `material`. To use a custom theme file, set it as `[tui.theme] base` in your config, as [Theming](theming.md#custom-theme-files) describes.

## Compaction

`/compact` summarizes the visible conversation in a single `Compacted N messages into a summary` block, and the next prompt continues from that summary. Text after the command focuses the summary, as in `/compact focus on the build error and how we fixed it`.

After compaction:

- **Transcript**: The pre-compact transcript stays on disk, but `ox -c` resumes from the summary and the messages that follow it.
- **File tracking**: The file-change tracker resets, so the model must read a file again before editing it.
- **Session state**: Queued prompts, the task list, and background shells carry over.

`/compact` refuses conversations shorter than about 1,000 tokens, counting messages, tool calls, and tool output, but not instructions or tool definitions. If the model returns an empty summary, compaction fails and leaves the conversation unchanged.

Automatic compaction runs before a prompt and between the tool rounds of a long turn, which then continues from the summary on its own. When it fails:

- **Before a prompt**, oxide-code does not send the prompt and puts it back in the input. Sending it again retries the compaction.
- **Between tool rounds**, the turn stops. Run `/compact`, then send a message to continue.
- **After 3 failures in a row**, automatic compaction pauses, so the next prompt goes out without it, and `/status` shows it as paused. A successful `/compact`, a `/model` or `/effort` change, `/clear`, `/archive`, or `/resume` turns it back on.

## Sessions

`/rename`, `/resume`, `/delete`, `/archive`, and `/unarchive` manage session titles and saved sessions without leaving the TUI. See [Titles](sessions.md#titles), [Mid-Session Resume](sessions.md#mid-session-resume), [Deleting a Session](sessions.md#deleting-a-session), and [Archiving a Session](sessions.md#archiving-a-session).
