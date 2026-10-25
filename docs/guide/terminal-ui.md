# Terminal UI

Running `ox` opens the terminal UI, which shows the chat, the input box, and the status line. [Configuration](configuration.md#tui-terminal-ui) covers the `[tui]` settings and the status-line segments, and [Theming](theming.md) covers the colors.

## Write a Prompt

- **Send**: Press **Enter** to send the prompt.
- **New line**: Press **Shift+Enter** in terminals that report it, such as kitty, WezTerm, and Ghostty. **Alt+Enter** also inserts a new line. Elsewhere, Shift+Enter arrives as a plain Enter and sends the prompt. VS Code and Cursor support it once a keybinding sends ESC followed by Enter for Shift+Enter, as Claude Code's `/terminal-setup` installs. A paste inserts its text, line breaks included, without sending it.
- **Edit**: **Ctrl+A** and **Ctrl+E** move to the start and end of the line, and **Alt+B** and **Alt+F** move by word. **Ctrl+W** deletes the word before the cursor, **Ctrl+K** deletes to the end of the line, and **Ctrl+Y** pastes the text they deleted. **Ctrl+U** undoes an edit, and **Ctrl+R** redoes it. **Ctrl+J** deletes to the start of the line, which readline binds to **Ctrl+U**.
- **Alt keys on macOS**: Terminal.app, iTerm2, and VS Code send Alt shortcuts only once the Option key is set to act as Meta in their settings.
- **Move between lines**: **Up** and **Down** move the cursor through a multi-line prompt. Where the cursor cannot move further, they scroll the chat.
- **Slash commands**: Typing `/` at the start of the input opens the command popup. [Slash Commands](slash-commands.md) lists the commands and the popup keys.
- **Switch modes**: Press **Tab** or **Shift+Tab** to cycle through the `manual`, `yolo`, and read-only `plan` [permission modes](permissions.md#modes). **Tab** completes a selection while the command popup is open.

The footer below the input shows the available shortcuts, switching to completion keys while the command popup is open. It drops trailing hints on narrow terminals and hides when there is no spare row.

## Queue Prompts During a Turn

The input box stays open while the assistant works. A prompt you send during a turn waits in a queue, shown as dim lines above the input box, and the assistant reads it with its next request to the model. If the turn ends first, the oldest queued prompt starts the next turn. The `queued-prompts` status-line segment, which the default status line leaves out, counts the waiting prompts.

[When Commands Run](slash-commands.md#when-commands-run) lists which slash commands work during a turn.

## Interrupt and Exit

1. Press **Esc** or **Ctrl+C** to stop the running turn or compaction. Prompts still in the queue stay there, and nothing runs until you send something.
2. To edit a queued prompt first, press **Esc** with the input box empty, which moves the most recently queued prompt back into it.
3. The next prompt you send joins the back of the queue, and a turn starts with the oldest queued prompt.

To exit while the assistant is idle, press **Ctrl+C** twice within a second, which exits even when the input box holds a draft, or press **Ctrl+D** with an empty input box. While chat text is selected, **Ctrl+C** copies it instead.

## Scroll and Expand

- **Scroll**: Use the mouse wheel or **PageUp** and **PageDown**, or press **Ctrl+Home** to jump to the top. Scrolling up stops the chat from following new output, and a marker such as `2 new messages (ctrl+End) ↓` counts what arrives meanwhile. Scroll back to the bottom or press **Ctrl+End** to follow new output again.
- **Expand output**: Long tool output shows its first rows and a footer such as `... +3 lines · Ctrl+O to expand`, and finished thinking shows only its `Thought` header. Press **Ctrl+O** to show every block in full and again to collapse them, or click a footer or `Thought` header to expand only that block.

## Select and Copy

Drag over chat text to select it, and release to copy it to the clipboard. Double-click selects a word and triple-click a row. The highlight stays until the next keystroke, and **Ctrl+C** copies it again without interrupting the turn. A notice in the chat's corner reports whether the copy landed and, if not, how to fix it for your terminal. Over SSH, copying goes through tmux 3.2 or later, or else through the terminal's OSC 52 support, which VS Code's Remote SSH terminal lacks. On Linux under X11, the copied text stays on the clipboard only while `ox` runs, unless a clipboard manager keeps it. To use the terminal's own selection instead, hold **Shift** while dragging, **Fn** in Terminal.app, or **Option** in iTerm2 and in VS Code and Cursor on macOS once `terminal.integrated.macOptionClickForcesSelection` is enabled.

## Approve Tool Calls

When a tool call needs approval, an approval prompt replaces the input box and takes **Y**, **A**, **N**, **F**, or **Esc**. [Approval Prompt](permissions.md#approval-prompt) describes each key and the session rules that **A** adds.
