# Terminal UI

`ox` opens with the status line at top, chat in the middle, and input box at bottom. See [Configuration](configuration.md#tui-terminal-ui) for `[tui]` settings and segments, and [Theming](theming.md) for colors.

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

- **Shift+Enter** requires a terminal that reports it (kitty, WezTerm, Ghostty). Elsewhere it sends plain **Enter** and submits. In VS Code and Cursor, bind **Shift+Enter** to send **Esc** then **Enter** (as Claude Code's `/terminal-setup` installs).
- **Backspace** and **Delete** remove whole grapheme clusters (such as emoji or accented letters) in one press.
- **Alt keys on macOS** require **Option**-as-Meta in Terminal.app, iTerm2, and VS Code.
- **Pasting** inserts text including newlines without submitting. **Ctrl+V** stays with the terminal's paste.
- **Leading `/`** opens the command popup (**Tab** completes). See [Slash Commands](slash-commands.md).

The footer below the input lists available shortcuts.

## Prompt History

**Ctrl+R** searches prompts retained from this project (case-insensitive substring match).

| Key                       | Action                                         |
| ------------------------- | ---------------------------------------------- |
| **Ctrl+S**                | Toggle between this project and every project  |
| **Up** / **Down**         | Select a prompt                                |
| **PageUp** / **PageDown** | Scroll the selected prompt's multiline preview |
| **Enter**                 | Replace the draft with the prompt, unsent      |
| **Esc** / **Ctrl+C**      | Close and keep the draft                       |

After recalling a prompt, press **Ctrl+Z** twice to restore a nonempty draft. Prompts from archived sessions, rewound turns, and surviving forks remain searchable until their sessions are deleted.

## Queue Prompts During a Turn

The input box stays active during a turn. Prompts submitted mid-turn appear as dim queued lines above the input and join the next model request. If the current turn finishes first, only the oldest queued prompt starts the next turn. Enable the optional `queued-prompts` status-line segment to show the waiting count. See [When Commands Run](slash-commands.md#when-commands-run) for mid-turn slash commands.

## Interrupt and Exit

- **Esc** or **Ctrl+C** stops the running turn or compaction. Queued prompts remain queued until you submit again, which appends to the queue and starts a turn from the oldest queued prompt.
- When idle with an empty input box, **Esc** moves the most recently queued prompt back into the input for editing.
- **Ctrl+C** twice within a second exits while idle (even with a draft). **Ctrl+D** twice exits on an empty input. With chat text selected, **Ctrl+C** copies instead.

## Scroll and Expand

- **Scroll**: Mouse wheel, **PageUp** / **PageDown**, or **Ctrl+Home** (top). Scrolling up pauses auto-follow and shows a marker such as `2 new messages · Ctrl+End to jump to bottom`. Scroll to the bottom or press **Ctrl+End** to follow again.
- **Expand**: Long tool output collapses with a footer such as `... +3 lines · Ctrl+O to expand`, and finished thinking collapses to a `Thought` header. **Ctrl+O** toggles every block, while clicking a footer or `Thought` header expands that block alone.

## Select and Copy

Drag over chat text and release to copy to the clipboard (double-click for a word, triple-click for a row). The selection persists until the next keystroke, and **Ctrl+C** re-copies without interrupting. A corner notice reports copy status and terminal fix hints on failure.

- **Over SSH**: Copies through tmux 3.2+ or OSC 52 (unsupported by VS Code Remote SSH).
- **Linux X11**: Copied text stays on the clipboard only while `ox` runs unless a clipboard manager retains it.
- **Native selection**: Hold **Shift** while dragging (**Fn** in Terminal.app, **Option** in iTerm2, or **Option** in VS Code / Cursor on macOS with `terminal.integrated.macOptionClickForcesSelection` enabled).

## Approve Tool Calls

When a tool call requires approval, an approval prompt replaces the input box. See [Approval Prompt](permissions.md#approval-prompt).

## Answer Questions

When the assistant asks a mid-turn question, a question prompt replaces the input box with suggested choices (marking any recommended choice), a freeform field, and any [interaction](configuration.md#interaction-question-and-approval-deadlines) deadline hint.

| Key                       | Action                                              |
| ------------------------- | --------------------------------------------------- |
| **Up** / **Down**         | Move between suggested choices and the answer field |
| **Tab**                   | Jump to the freeform answer field                   |
| **PageUp** / **PageDown** | Scroll the question body when it overflows          |
| **Enter**                 | Submit the highlighted choice or the typed answer   |
| **Ctrl+S**                | Skip the question without answering                 |
| **Esc** / **Ctrl+C**      | Abort the question                                  |

Typing or pasting while on a choice moves focus to the freeform field. Like approval and delete prompts, the question prompt ignores keys until a 300 ms typing pause so type-ahead cannot submit accidentally.
