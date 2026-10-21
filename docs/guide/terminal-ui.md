# Terminal UI

Running `ox` opens the terminal UI, which shows the chat, the input box, and the status line. [Configuration](configuration.md#tui-terminal-ui) covers the `[tui]` settings and the status-line segments, and [Theming](theming.md) covers the colors.

## Write a Prompt

- **Send**: Press **Enter** to send the prompt.
- **New line**: Press **Shift+Enter** or **Alt+Enter** to start a new line. VS Code and Cursor send **Alt+Enter** for **Shift+Enter**, so both work there. A paste inserts its text, line breaks included, without sending it.
- **Edit**: **Ctrl+A** and **Ctrl+E** move to the start and end of the line, and **Alt+B** and **Alt+F** move by word. **Ctrl+W** deletes the word before the cursor, **Ctrl+K** deletes to the end of the line, and **Ctrl+Y** pastes the text they deleted. **Ctrl+U** undoes an edit, and **Ctrl+R** redoes it.
- **Move between lines**: **Up** and **Down** move the cursor through a multi-line prompt. Where the cursor cannot move further, they scroll the chat.
- **Slash commands**: Typing `/` at the start of the input opens the command popup. [Slash Commands](slash-commands.md) lists the commands and the popup keys.

## Queue Prompts During a Turn

The input box stays open while the assistant works. Sending a prompt during a turn queues it, and the turn reads it at its next tool round, so the assistant sees it before it replies. A prompt still queued when the turn ends starts the next turn. The `queued-prompts` status-line segment counts the waiting prompts.

Slash commands that only show information, such as `/status`, run at once during a turn, and `/mode` switches the mode for the turn's next tool call. Commands that change the session, such as `/clear`, `/compact`, and `/model`, run only when the assistant is idle.

## Interrupt and Exit

- **Interrupt**: Press **Esc** or **Ctrl+C** to stop the running turn or compaction. Queued prompts stay queued. The next prompt you send joins the back of the queue, and a turn starts with the oldest one.
- **Recall a queued prompt**: When the assistant is idle and the input box is empty, **Esc** moves the most recently queued prompt back into the input box for editing.
- **Exit**: When the assistant is idle, press **Ctrl+C** twice within a second, or press **Ctrl+D** with an empty input box. While chat text is selected, **Ctrl+C** copies it instead.

## Scroll and Expand

- **Scroll**: Use the mouse wheel or **PageUp** and **PageDown**. Scrolling up stops the chat from following new output. Press **Ctrl+Home** to jump to the top and **Ctrl+End** to jump to the bottom and follow new output again.
- **Expand output**: Long tool output shows its first rows and a footer such as `... +3 lines · Ctrl+O to expand`, and finished thinking shows only its `Thought` header. Press **Ctrl+O** to show every block in full and again to collapse them, or click a footer or `Thought` header to expand only that block.

## Select and Copy

Drag over chat text to select it, and release to copy it to the clipboard. Double-click selects a word and triple-click a row. The highlight stays until the next keystroke, and **Ctrl+C** copies it again without interrupting the turn. A notice in the chat's corner reports whether the copy landed and, if not, how to fix it for your terminal. Over SSH, copying goes through tmux 3.2 or later, or else through the terminal's OSC 52 support, which VS Code's Remote SSH terminal lacks. On Linux under X11, the copied text stays on the clipboard only while `ox` runs, unless a clipboard manager keeps it. To use the terminal's own selection instead, hold **Shift** while dragging, **Fn** in Terminal.app, or **Option** in iTerm2 and in VS Code and Cursor on macOS once `terminal.integrated.macOptionClickForcesSelection` is enabled.

## Approve Tool Calls

When a tool call needs approval, the chat shows a prompt that takes **Y**, **A**, **N**, **F**, or **Esc**, as [Quickstart](quickstart.md#approve-tool-calls) shows. [Permissions](permissions.md#approval-prompt) describes each key and the session rules that **A** adds.
