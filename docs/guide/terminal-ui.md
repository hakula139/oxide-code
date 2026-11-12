# Terminal UI

Running `ox` opens the terminal UI, with the status line on the top row, then the chat and the input box. [Configuration](configuration.md#tui-terminal-ui) covers the `[tui]` settings and status-line segments, and [Theming](theming.md) covers the colors.

## Write a Prompt

| Key                                        | Action                                                                                    |
| ------------------------------------------ | ----------------------------------------------------------------------------------------- |
| **Enter**                                  | Send the prompt                                                                           |
| **Shift+Enter**, **Alt+Enter**, **Ctrl+J** | Insert a new line                                                                         |
| **Up** / **Down**                          | Move through a multi-line prompt, and scroll the chat once the cursor cannot move further |
| **Ctrl+A** / **Ctrl+E**                    | Move to the start / end of the line                                                       |
| **Alt+B** / **Alt+F**                      | Move back / forward one word                                                              |
| **Backspace**                              | Delete the character before the cursor                                                    |
| **Delete** / **Ctrl+D**                    | Delete the character after the cursor                                                     |
| **Ctrl+W** / **Ctrl+K** / **Ctrl+U**       | Cut the word before the cursor / to the end of the line / to the start of the line        |
| **Ctrl+Y**                                 | Paste the last cut text                                                                   |
| **Ctrl+Z** or **Ctrl+_** / **Alt+R**       | Undo / redo an edit                                                                       |
| **Ctrl+R**                                 | Search the [prompt history](#prompt-history)                                              |
| **Tab** / **Shift+Tab**                    | Step through the [permission modes](permissions.md#modes)                                 |

- **Shift+Enter** needs a terminal that reports it, such as kitty, WezTerm, and Ghostty. Elsewhere it arrives as a plain Enter and sends the prompt. VS Code and Cursor support it once a keybinding sends ESC followed by Enter for Shift+Enter, as Claude Code's `/terminal-setup` installs.
- **Backspace and Delete** remove a whole grapheme cluster, so an emoji or an accented letter goes in one press.
- **Alt keys on macOS** work in Terminal.app, iTerm2, and VS Code only once the Option key is set to act as Meta in their settings.
- **A paste** inserts its text, line breaks included, without sending it. **Ctrl+V** stays with the terminal's paste.
- **Typing `/`** at the start of the input opens the command popup, where **Tab** completes the selection. [Slash Commands](slash-commands.md) lists the commands and the popup keys.

The footer below the input shows the available shortcuts.

## Prompt History

**Ctrl+R** searches the prompts retained from this project. The filter matches any part of a prompt, ignoring case.

| Key                       | Action                                         |
| ------------------------- | ---------------------------------------------- |
| **Ctrl+S**                | Toggle between this project and every project  |
| **Up** / **Down**         | Select a prompt                                |
| **PageUp** / **PageDown** | Scroll the selected prompt's multiline preview |
| **Enter**                 | Replace the draft with the prompt, unsent      |
| **Esc** / **Ctrl+C**      | Close and keep the draft                       |

After a recall, **Ctrl+Z** twice restores a nonempty draft. Prompts from archived sessions and rewound turns stay available until their sessions are deleted, and a surviving fork keeps the prompts it copied.

## Queue Prompts During a Turn

The input box stays open while the assistant works. A prompt you send during a turn waits in a queue, shown as dim lines above the input box, and the assistant reads it with its next request to the model. If the turn ends first, the oldest queued prompt starts the next turn. The `queued-prompts` status-line segment, which the default status line leaves out, counts the waiting prompts.

[When Commands Run](slash-commands.md#when-commands-run) lists which slash commands work during a turn.

## Interrupt and Exit

- **Esc** or **Ctrl+C** stops the running turn or compaction. Queued prompts stay queued, and nothing runs until you send something. The next prompt you send joins the back of the queue, and a turn starts with the oldest queued prompt.
- Once no turn runs, **Esc** with the input box empty moves the most recently queued prompt back into it for editing.
- **Ctrl+C** twice within a second exits while the assistant is idle, even when the input box holds a draft. **Ctrl+D** twice exits from an empty input box. While chat text is selected, **Ctrl+C** copies it instead.

## Scroll and Expand

- **Scroll**: Use the mouse wheel or **PageUp** and **PageDown**, or press **Ctrl+Home** to jump to the top. Scrolling up stops the chat from following new output, and a marker such as `2 new messages · Ctrl+End to jump to bottom` counts what arrives meanwhile. Scroll back to the bottom or press **Ctrl+End** to follow again.
- **Expand**: Long tool output shows its first rows and a footer such as `... +3 lines · Ctrl+O to expand`, and finished thinking shows only its `Thought` header. **Ctrl+O** shows every block in full and collapses them again, and clicking a footer or `Thought` header expands only that block.

## Select and Copy

Drag over chat text to select it, and release to copy it to the clipboard. Double-click selects a word and triple-click a row. The highlight stays until the next keystroke, and **Ctrl+C** copies it again without interrupting the turn. A notice in the chat's corner reports whether the copy landed and, if not, how to fix it for your terminal.

- **Over SSH**, copying goes through tmux 3.2 or later, or else through the terminal's OSC 52 support, which VS Code's Remote SSH terminal lacks.
- **On Linux under X11**, the copied text stays on the clipboard only while `ox` runs, unless a clipboard manager keeps it.
- **The terminal's own selection** works while you hold **Shift** as you drag, **Fn** in Terminal.app, or **Option** in iTerm2, and in VS Code and Cursor on macOS once `terminal.integrated.macOptionClickForcesSelection` is enabled.

## Approve Tool Calls

When a tool call needs approval, an approval prompt replaces the input box. [Approval Prompt](permissions.md#approval-prompt) describes its keys.
