# Slash Commands

Type `/` to open the autocomplete popup. Use **Up / Down** to browse, **Tab** to complete, **Enter** to submit, and **Esc** to dismiss. An alias runs the same command as its canonical name. Typing `/new` shows `/clear (new)` in the popup, and `/help` lists every alias.

After `/effort`, `/init`, `/mode`, `/model`, or `/theme`, type a space to switch the popup to **arg mode**, which lists the valid values.

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
| `/mode [<mode>]`                            | Open the permission mode picker, or switch directly to `manual`, `plan`, or `yolo`.                                                           |
| `/model [<id>]`                             | Open the model and effort picker, or switch directly by catalog ID, unique substring, or any other ID a gateway serves.                       |
| `/rename [<title>]`                         | Open a single-line editor pre-filled with the current title, or set it directly.                                                              |
| `/resume [<id-prefix>]` (alias `/continue`) | Open the session picker (search, scope and archived toggles, **Ctrl+X** to archive or unarchive, **Ctrl+D** to delete), or jump by ID prefix. |
| `/status`                                   | Show the model, permission mode, auth, settings, session rules, and background shells.                                                        |
| `/theme [<name>]`                           | Open the theme picker with live preview, or switch directly to a built-in theme.                                                              |
| `/unarchive <id-prefix>`                    | Unarchive a saved session by ID prefix, so it shows in listings again.                                                                        |

To send text that starts with `/`, double the slash. For example, typing `//etc` sends `/etc`.

Model, effort, permission mode, and theme choices last for the session. No slash command writes config files, so a restart returns to your configured values.

## When Commands Run

- **During a turn**, only `/config`, `/diff`, `/exit`, `/help`, `/mode`, `/status`, and `/theme` run. A mode switch applies from the turn's next tool call. Retry other commands after the turn finishes.
- **In the bare REPL** (`--no-tui`), only `/clear`, `/compact`, `/effort <level>`, `/exit`, `/help`, `/init [<file>]`, `/mode <mode>`, `/model <id>`, `/rename <title>`, and `/resume <id-prefix>` run, along with their aliases. `/init` starts a turn like a prompt, `/exit` quits, and the others print their text to stderr. The REPL has no picker, so a project without an instruction file needs `/init AGENTS.md` or `/init CLAUDE.md`. The REPL rejects other commands, and a rejected or failed command makes it exit with a failure status at the end of input.

## Model and Effort

- **Bare `/model` and `/effort`** open pickers. Press **Enter** to apply your choice or **Esc** to cancel.
- **`/model <id>`** accepts a full ID or any unique substring, so `fable`, `opus`, `sonnet`, and `haiku` work as aliases. Switching models preserves the current effort.
- **`/effort`** fails if the session uses a model outside the supported catalog, because that model has no effort levels.

See [`effort`](configuration.md#effort-reasoning-effort) for each model's defaults.

## Permission Mode

`/mode` opens a picker of the [permission modes](permissions.md#modes). Use **Up / Down** or a number key to move, **Enter** to switch, and **Esc** to cancel. `/mode <mode>` switches directly, and **Tab** or **Shift+Tab** changes the mode in the cycle that page describes.

## Compaction

`/compact` summarizes the visible conversation in a single `Compacted N messages into a summary` block. The next prompt continues from that summary. Text after the command adds focus instructions, as in `/compact focus on the build error and how we fixed it`. This helps when only part of the work matters going forward.

After compaction:

- **Transcript**: The pre-compact transcript stays on disk, but `ox -c` resumes from the summary and the messages that follow it.
- **File tracking**: The file-change tracker resets, so the model must read a file again before editing it.
- **Session state**: Queued prompts, the task list, and background shells carry over.

`/compact` refuses conversations shorter than about 1,000 tokens. It counts messages, tool calls, and tool output, but not instructions or tool definitions. If the model returns an empty summary, compaction fails and leaves the conversation unchanged.

Automatic compaction runs before a prompt and between the tool rounds of a long turn, which then continues from the summary on its own. If the compaction before a prompt fails, oxide-code does not send the prompt, and the TUI puts it back in the input. Sending it again retries the compaction. If the compaction between tool rounds fails, the turn stops, so run `/compact`, then send a message to continue. After 3 failures in a row, automatic compaction pauses, so the next prompt goes out without it, and `/status` shows it as paused. A successful `/compact`, a `/model` or `/effort` change, `/clear`, `/archive`, or `/resume` turns it back on.

## Sessions

`/rename`, `/resume`, `/delete`, `/archive`, and `/unarchive` manage session titles and saved sessions without leaving the TUI. See [Titles](sessions.md#titles), [Mid-Session Resume](sessions.md#mid-session-resume), [Deleting a Session](sessions.md#deleting-a-session), and [Archiving a Session](sessions.md#archiving-a-session).

## Theme

`/theme` opens the picker. Use **Up / Down** to preview themes live, number keys to jump to a row, **Enter** to apply a theme for the rest of the session, and **Esc** to revert.

`/theme <name>` switches directly to a built-in theme: `mocha`, `macchiato`, `frappe`, `latte`, or `material`. To use a custom theme file, set it as `[tui.theme] base` in your config, as described in [Theming](theming.md#custom-theme-files).
