# Slash Commands

Type `/` to open the autocomplete popup. Use **Up / Down** to browse, **Tab** to complete, **Enter** to submit, and **Esc** to dismiss. An alias runs the same command as its canonical name. Typing `/new` shows `/clear (new)` in the popup, and `/help` lists every alias.

After `/effort`, `/mode`, `/model`, or `/theme`, type a space to switch the popup to **arg mode**, which lists the valid values.

## Built-In Commands

| Command                                     | Description                                                                                                                                   |
| ------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `/archive [<id-prefix>]`                    | Archive this session and start a fresh one, or archive a saved session by ID prefix.                                                          |
| `/clear` (aliases `/new`, `/reset`)         | Start a fresh session. The previous one stays resumable with `ox -c`.                                                                         |
| `/compact [<instructions>]`                 | Compress the conversation into a summary. Trailing text steers the focus.                                                                     |
| `/config`                                   | Open the resolved configuration, where each value came from, and the loaded config files.                                                     |
| `/delete <id-prefix>`                       | Delete a saved session by ID prefix, after a Y / N confirmation.                                                                              |
| `/diff`                                     | Show uncommitted git changes and untracked files in chat, capped at 64 KB.                                                                    |
| `/effort [<level>]`                         | Open the slider, or set the level directly (`low`, `medium`, `high`, `xhigh`, `max`).                                                         |
| `/help`                                     | Open a read-only list of available commands.                                                                                                  |
| `/init`                                     | Create `AGENTS.md` or `CLAUDE.md`, or propose edits to an existing one as a diff to confirm.                                                  |
| `/login`                                    | Sign in to a Claude account in the browser and store the login for oxide-code.                                                                |
| `/logout`                                   | Remove the oxide-code login. The Claude Code login stays in place.                                                                            |
| `/mode [<mode>]`                            | Open the permission mode picker, or switch directly to `manual`, `plan`, or `yolo`.                                                           |
| `/model [<id>]`                             | Open the model and effort picker, or switch directly by ID or unique substring.                                                               |
| `/rename [<title>]`                         | Open a single-line editor pre-filled with the current title, or set it directly.                                                              |
| `/resume [<id-prefix>]` (alias `/continue`) | Open the session picker (search, scope and archived toggles, **Ctrl+X** to archive or unarchive, **Ctrl+D** to delete), or jump by ID prefix. |
| `/status`                                   | Show the model, auth, settings, session rules, and background shells.                                                                         |
| `/theme [<name>]`                           | Open the theme picker with live preview, or switch directly to a built-in theme.                                                              |
| `/unarchive <id-prefix>`                    | List an archived session again.                                                                                                               |

To send text that starts with `/`, double the slash. For example, typing `//etc` sends `/etc`.

Model, effort, permission mode, and theme choices last for the session. No slash command writes config files, so a restart returns to your configured values.

## When Commands Run

- **During a turn**, only `/config`, `/diff`, `/help`, `/mode`, `/status`, and the bare `/theme` picker run. A mode switch applies from the turn's next tool call. Retry other commands after the turn finishes.
- **In the bare REPL** (`--no-tui`), only `/clear`, `/compact`, `/effort <level>`, `/mode <mode>`, `/model <id>`, `/rename <title>`, and `/resume <id-prefix>` run, along with their aliases, and each prints a confirmation to stderr. The REPL rejects other commands, and a rejected or failed command makes it exit with a failure status at the end of input.

## Model and Effort

- **Bare `/model` and `/effort`** open pickers. Press **Enter** to apply your choice or **Esc** to cancel.
- **`/model <id>`** accepts a full ID or any unique substring, so `fable`, `opus`, `sonnet`, and `haiku` work as aliases. Switching models preserves the current effort.
- **`/effort`** fails if the session uses a model outside the supported catalog, because that model has no effort levels.

See [`effort`](configuration.md#effort-intelligence-tier) for each model's defaults.

## Permission Mode

`/mode` opens a picker of the [permission modes](configuration.md#permission-tool-approval). Use **Up / Down** or a number key to move, **Enter** to switch, and **Esc** to cancel. `/mode <mode>` switches directly, and **Shift+Tab** switches between `manual` and `plan`. [Tool Approval](configuration.md#permission-tool-approval) explains when its cycle includes `yolo`.

## Compaction

`/compact` summarizes the visible conversation in a single `Compacted N messages → 1 summary` block. The next prompt continues from that summary. Text after the command adds focus instructions, as in `/compact focus on the build error and how we fixed it`. This helps when only part of the work matters going forward.

After compaction:

- **Transcript**: The pre-compact transcript stays on disk, but `ox -c` resumes from the summary and the messages that follow it.
- **File tracking**: The file-change tracker resets, so the model must read a file again before editing it.
- **Session state**: Queued prompts, the task list, and background shells carry over.

`/compact` refuses conversations shorter than about 1,000 tokens. It counts messages, tool calls, and tool output, but not instructions or tool definitions. If the model returns an empty summary, compaction fails and leaves the conversation unchanged.

## Signing In

`/login` opens the Claude sign-in page in your browser and prints its URL in case the browser does not open. Sign-in returns to a local address, so complete it in a browser on the machine running `ox`. Signing in from a browser on another machine, such as over SSH, is tracked in [#239](https://github.com/hakula139/oxide-code/issues/239). Press **Esc** to cancel. Sign-in also ends automatically after 5 minutes.

After you approve access, oxide-code stores the login in `$XDG_DATA_HOME/ox/credentials.json` (default: `~/.local/share/ox/credentials.json`) and uses it for the current and later sessions.

`/logout` deletes that file. A session using it switches to the Claude Code login if one is available. Otherwise, it stays open signed out until the next `/login`. If the Claude Code login exists but cannot be used, for example because it expired, `/logout` explains why.

Both commands end by naming the credential that requests will now use. An environment variable or `client.api_key` takes precedence over either login, and `/login` tells you when one of them keeps the session on that credential. See [Authentication](configuration.md#authentication) for the full order.

## Sessions

`/rename`, `/resume`, `/delete`, `/archive`, and `/unarchive` manage session titles and saved sessions without leaving the TUI. See [Titles](sessions.md#titles), [Mid-Session Resume](sessions.md#mid-session-resume), [Deleting a Session](sessions.md#deleting-a-session), and [Archiving a Session](sessions.md#archiving-a-session).

## Theme

`/theme` opens the picker. Use **Up / Down** to preview themes live, number keys to jump to a row, **Enter** to apply a theme for the rest of the session, and **Esc** to revert.

`/theme <name>` switches directly to a built-in theme: `mocha`, `macchiato`, `frappe`, `latte`, or `material`. To use a custom theme file, set it as `[tui.theme] base` in your config, as described in [Theming](theming.md#custom-theme-files).
