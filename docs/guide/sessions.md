# Sessions

oxide-code automatically saves every conversation to disk. You can list past sessions and resume where you left off.

## Where Sessions Live

Sessions are stored under `$XDG_DATA_HOME/ox/sessions/` (default: `~/.local/share/ox/sessions/`), grouped by project. A session file is created on the first message, so launching `ox` and quitting without typing leaves nothing behind.

## Listing Sessions

```bash
ox --list                # Shows the 30 most recent unarchived sessions in the current project
ox -la                   # same, across every project
ox --list --limit 100    # raise the cap for one run
ox --list --limit 0      # unbounded
ox --list --archived     # archived sessions only
```

Prints a table of recent sessions (most recently active first, local time):

```text
ID         Last Active         Msgs   Title
a1b2c3d4   2026-04-18 09:20    12     Fix authentication bug
e5f6a7b8   2026-04-17 17:30    5      Add session persistence
```

With `--all`, a `Project` column is added so cross-project rows stay disambiguable (paths under `$HOME` render as `~/...`):

```text
ID         Last Active         Msgs   Project          Title
a1b2c3d4   2026-04-18 09:20    12     ~/work/oxide     Fix authentication bug
9a0b1c2d   2026-04-18 08:05    3      ~/scratch        Investigate UTF-8 truncation
```

Titles that overflow the terminal width are truncated with `...`. When output is piped, titles render untruncated so downstream tools can wrap at their own width.

By default, the listing shows at most 30 rows, so long-running projects don't dump hundreds of sessions on every invocation. If more sessions remain, a trailing `... more` line explains how to widen the listing with `--limit N` or `--limit 0`.

## Resuming a Session

Resume the most recent unarchived session in the current project:

```bash
ox -c              # short form
ox --continue      # long form
```

Resume by ID prefix (searches the current project):

```bash
ox -c a1b2
```

Widen the search to every project with `--all` / `-a`:

```bash
ox -c --all        # latest unarchived session across all projects
ox -c a1b2 -a      # prefix match across all projects
```

Resume from an explicit file path, useful for sessions copied between machines or kept outside the store root:

```bash
ox -c ./conversation.jsonl
ox -c /home/me/archive/2026-04/session.jsonl
```

On resume, oxide-code loads the saved history and appends new messages to the same file, so the session keeps its original ID.

### Mid-Session Resume

Inside the TUI, `/resume` (alias `/continue`) opens a session picker without restarting the process.

- Type to filter by id, title, or project. Press Tab to toggle current-project / all-projects scope and Ctrl+A to toggle active / archived sessions. Press Enter to resume the highlighted session in place.
- To skip the picker, pass `/resume <id-prefix>` to jump directly. An ambiguous prefix lists the candidates. This form matches active and archived sessions, excluding the live session. It checks the current project first and searches all projects only when there is no match in the current project.
- The picker loads the 30 most recent sessions in the selected scope and archive view, and notes in its footer when older ones are omitted.
- Chat repopulates from the resumed transcript, and any queued prompts are discarded because they were typed for the previous session. The next prompt continues the resumed thread just as it would after `ox -c`.

### Deleting a Session

Inside the `/resume` picker, **Ctrl+D** on the cursor row opens a Y/N confirm modal. **Y / Enter** unlinks the session JSONL and prints a `Deleted session {id}: {title}` line in chat. **N / Esc / Ctrl+C** dismisses. `/delete <id-prefix>` opens the same confirmation for any saved session except the live one.

### Archiving a Session

Archiving hides a session from `ox --list`, the `/resume` picker, and bare `ox -c`, while leaving its file in place.

- `/archive` archives the live session and starts a fresh one, as `/clear` does. A note in the new chat names the archived session.
- `/archive <id-prefix>` archives another saved session. If the prefix matches only the live session, the command is refused and points to bare `/archive`.
- `/unarchive <id-prefix>` makes a saved session visible in the list again.

The typed forms leave a session's last-active time unchanged, so an unarchived session returns to its previous place in the list.

To find archived sessions, run `ox --list --archived` or press **Ctrl+A** in the `/resume` picker to switch to the archived view. Resuming an archived session unarchives it and displays a note, whether you select it in that view or specify it with `/resume <id-prefix>`, `ox -c <id-prefix>`, or a file path.

## Titles

When the TUI is running, oxide-code generates a concise AI title (3-7 words) shortly after your first prompt. Outside the TUI (bare REPL, headless mode), sessions keep the first prompt as the title.

In the TUI, `/rename` opens a title editor pre-filled with the current title, and `/rename <title>` sets it directly. Your title replaces the AI title, and AI title generation stops for that session.

## Security

On Unix, session files are created with user-only permissions (`0o600`) because they contain verbatim tool output, which may include secrets from bash commands.

## Headless and REPL Modes

Sessions are recorded in every mode:

- **TUI** (`ox`): Saved automatically.
- **Bare REPL** (`ox --no-tui`): Saved automatically.
- **Headless** (`ox -p "prompt"`): Single-turn session saved, useful for audit trails.
