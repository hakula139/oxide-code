# Sessions

oxide-code persists every conversation to disk so you can list and resume sessions later, including [headless runs](headless.md) (`ox -p`).

## Where Sessions Live

Sessions live in one SQLite database at `$XDG_DATA_HOME/ox/sessions.db` (default `~/.local/share/ox/sessions.db`), grouped by project: the Git repository containing the launch directory, or that directory outside a repository. This matches the [instruction files](instructions.md#where-instruction-files-can-live) project root, so subdirectories of one repository share sessions.

- **Saving**: A session is created on its first message, so quitting without typing saves nothing. Spilled tool output lives under `$XDG_DATA_HOME/ox/artifacts/` per session (up to 10 MiB per file, readable by the model).
- **Permissions**: On Unix, the database and saved tool output use mode `0600`, since tool output may contain secrets. Keep [exports](#moving-a-session-between-machines) equally private.
- **Location**: Keep the data directory on a local filesystem, outside network mounts and file-sync tools, which SQLite does not safely support.
- **Older releases**: Releases up to 0.1.0-alpha.9 saved each session as a JSON lines file under `$XDG_DATA_HOME/ox/sessions/`. The database ignores those files, so you can delete that directory once no longer needed.

### Backup and Recovery

Back up safely while `ox` runs using SQLite's `.backup` command (direct file copies can catch mid-write state):

```bash
sqlite3 "${XDG_DATA_HOME:-$HOME/.local/share}/ox/sessions.db" ".backup sessions-backup.db"
```

If the database is damaged (for example by a disk failure), `ox` fails at startup with recovery steps if the header or schema is unreadable, or fails with `database disk image is malformed` when a write or listing hits the damage. Quit every `ox` process and run:

```bash
cd "${XDG_DATA_HOME:-$HOME/.local/share}/ox"
sqlite3 sessions.db .recover | sqlite3 recovered.db
mkdir damaged && mv sessions.db* damaged/ && mv recovered.db sessions.db
```

Run `.recover` before moving files so commits in `sessions.db-wal` are included. To start fresh, run only `mkdir damaged && mv sessions.db* damaged/`.

## Listing Sessions

```bash
ox --list                # The 30 most recent unarchived sessions in the current project
ox -la                   # Same, across every project
ox --list --limit 100    # Raise the cap for one run
ox --list --limit 0      # Unbounded
ox --list --archived     # Archived sessions only
```

Sessions appear most-recent first in local time:

```text
ID         Last Active         Msgs   Title
a1b2c3d4   2026-04-18 09:20    12     Fix authentication bug
e5f6a7b8   2026-04-17 17:30    5      Add session persistence
```

With `--all`, a `Project` column shows each project (with `$HOME` as `~/...`):

```text
ID         Last Active         Msgs   Project          Title
a1b2c3d4   2026-04-18 09:20    12     ~/work/oxide     Fix authentication bug
9a0b1c2d   2026-04-18 08:05    3      ~/scratch        Investigate UTF-8 truncation
```

Long titles truncate with `...` on terminals (kept whole when piped). A trailing `... more` line appears when sessions exceed `--limit`.

## Resuming a Session

```bash
ox -c                          # Continue the latest unarchived session in this project
ox -r a1b2                     # Resume by ID prefix
ox -r ./a1b2.jsonl             # Resume a file that ox export wrote
ox -c --all                    # Latest unarchived session across all projects
ox -r a1b2 -a                  # Prefix match across all projects
ox -c "Run the tests again"    # Continue with a prompt
ox -r a1b2 -p "Summarize"      # Continue as a headless run
ox -r                          # Pick from this project's sessions
ox -r --all                    # Pick from every project's sessions
```

- **`-c` / `--continue`**: Continues the latest unarchived session in the current project.
- **`-r` / `--resume`**: Takes an ID prefix or [`ox export`](#moving-a-session-between-machines) file path. Prefixes search the current project first, falling back to all projects. With `--all` / `-a`, prefixes search all projects at once and list all matching candidates if ambiguous.
- **Trailing prompt**: Passing a prompt after `-c` or `-r <id-prefix>` continues the session with that prompt (in the TUI or [headless](headless.md) with `-p`).
- **Bare `-r`**: Opens the TUI [session picker](#mid-session-resume). **Enter** resumes the selected row and **Esc** quits without saving. Cannot combine with a prompt, `--continue`, `--list`, or `-p`, and requires a terminal on stdout.

Resumed sessions keep their ID and append new messages unless another process wrote to them ([Several Processes on One Session](#several-processes-on-one-session)). The [task list](quickstart.md#tasks-and-background-commands) and running session cost restore, while background shells end with their original process.

### Mid-Session Resume

Inside the TUI, `/resume` (alias `/continue`) opens the session picker in place.

- **Picker**: Type to filter by ID, title, or project. **Tab** toggles current / all projects, **Ctrl+A** toggles active / archived, and **Enter** resumes. Lists all sessions in scope (most recent first), loading older rows as you scroll while filtering searches all of them.
- **`/resume <id-prefix>`**: Jumps directly (listing candidates if ambiguous). Matches any saved active or archived session other than the live one, searching the current project first, falling back to all projects.
- **After resume**: The chat repopulates from the resumed transcript. Previously queued prompts are discarded, while a prompt sent during loading runs in the resumed session (or waits for a retried `/resume` if loading fails).

### Deleting a Session

Press **Ctrl+D** in the `/resume` picker or run `/delete <id-prefix>` for any saved session except the live one. **Y** deletes the session and its saved tool output and prints `Deleted session {id}: {title}`. **N**, **Esc**, or **Ctrl+C** cancels. Confirmation requires a 300 ms typing pause so type-ahead cannot confirm.

Once confirmed with **Y**, dismissing the dialog does not cancel the delete, which still reports its result in chat while dropping other session actions until done. A session open in another `ox` process cannot be deleted until that process quits or switches sessions.

### Archiving a Session

Archiving hides a session from `ox --list`, `/resume`, and `ox -c` without deleting it.

- **`/archive`**: Archives the live session and starts a fresh one (like `/clear`), noting the archived session in chat.
- **`/archive <id-prefix>`**: Archives another saved session (refused for the live session, pointing to bare `/archive`).
- **`/unarchive <id-prefix>`**: Restores visibility.
- **Picker**: **Ctrl+X** archives (or unarchives in the archived view), removes the row, advances the highlight, and notes the session in the footer.

Archiving preserves last-active timestamps, so unarchived sessions return to their previous position. Find archived sessions with `ox --list --archived` or **Ctrl+A** in `/resume`. Resuming an archived session unarchives it with a note.

## Several Processes on One Session

Two `ox` processes may hold the same session open (such as `ox -c` in two terminals). Each continues in place until another process changes the conversation. On the next message after a concurrent write, `ox` copies the local view into a new session and prints a notice like `Another ox process wrote to session a1b2c3d4, so this conversation continues as session e5f6a7b8`. Renaming or archiving leaves the conversation unchanged.

The fork keeps the title with `(fork of a1b2c3d4)` in `ox --list` and `/resume`, and a [headless run](headless.md) reports the fork's ID.

## Moving a Session Between Machines

`ox export <id-prefix>` writes a session as JSON lines (every branch and pre-compaction message included), and `ox -r <file>` imports and resumes it on any machine:

```bash
ox export a1b2 >a1b2.jsonl
ox -r a1b2.jsonl
```

Re-importing an existing session keeps whichever copy is further along, even across compactions or rewinds. Divergent copies import as a fork of the local session, and later imports extend that fork. Newly imported sessions belong to the project where `ox -r` runs.

## Titles

The TUI generates an AI title (3-7 words) shortly after your first prompt. Headless sessions use the first prompt.

`/rename` edits the current title, and `/rename <title>` sets it directly, replacing and disabling AI titling for that session. Titles keep only the first non-blank line and truncate with `...` past 80 columns (the `/rename` editor caps input at 80 columns).
