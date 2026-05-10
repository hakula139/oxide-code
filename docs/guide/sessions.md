# Sessions

oxide-code automatically saves every conversation to disk. You can list past sessions and resume where you left off.

## Where Sessions Live

Sessions are stored under `$XDG_DATA_HOME/ox/sessions/` (default: `~/.local/share/ox/sessions/`), grouped by project. A session file is created on the first message, so launching `ox` and quitting without typing leaves nothing behind.

## Listing Sessions

```bash
ox --list                # 30 most recent sessions in the current project
ox -la                   # same, across every project
ox --list --limit 100    # raise the cap for one run
ox --list --limit 0      # unbounded (the original behavior)
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

By default the listing caps at 30 rows so a long-running project doesn't dump hundreds of sessions on every invocation. When the cap clips the result, the trailing line shows the hidden count and how to widen it (`--limit N` or `--limit 0`).

## Resuming a Session

Resume the most recent session in the current project:

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
ox -c --all        # latest session anywhere
ox -c a1b2 -a      # prefix match across all projects
```

Resume from an explicit file path — useful for sessions copied between machines or kept outside the store root:

```bash
ox -c ./conversation.jsonl
ox -c /home/me/archive/2026-04/session.jsonl
```

On resume, oxide-code loads the full history and appends new messages to the same file, so the session keeps its original ID.

### Mid-Session Resume

Inside the TUI, `/resume` (alias `/continue`) opens a session picker without restarting the process. Type to filter by id, title, or project, press Tab to widen the scope from current-project to all projects, and press Enter to resume the highlighted session in place. To skip the picker, pass `/resume <id-prefix>` to jump directly. Mid-session resume behaves identically to `ox -c` from launch.

### Deleting a Session

Inside the `/resume` picker, press **Ctrl+D** (or **Delete**) on the cursor row to open a Y/N confirm modal. **Y / Enter** unlinks the session JSONL and prints a `Deleted session {id}: {title}` line in chat. **N / Esc / Ctrl+C** dismisses. To skip the picker, run `/delete <id-prefix>` to open the same confirm directly. Only finalized sessions can be deleted: the live session is filtered out of both paths and refused at the store layer.

## Titles

When the TUI is running, oxide-code generates a concise AI title (3-7 words) shortly after your first prompt. Outside the TUI (bare REPL, headless mode), sessions keep the first prompt as the title, and setting `/rename` cancels and replaces the AI title for the rest of the session.

## Security

On Unix, session files are created with user-only permissions (`0o600`) because they contain verbatim tool output, which may include secrets from bash commands.

## Headless and REPL Modes

Sessions are recorded in every mode:

- **TUI** (`ox`) — saved automatically.
- **Bare REPL** (`ox --no-tui`) — saved automatically.
- **Headless** (`ox -p "prompt"`) — single-turn session saved (useful for audit trails).
