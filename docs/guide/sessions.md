# Sessions

oxide-code saves every conversation to disk, so you can list past sessions and resume where you left off. Headless runs (`ox -p`) save sessions too, as [Headless Mode](headless.md) describes.

## Where Sessions Live

Sessions are stored in one SQLite database, `$XDG_DATA_HOME/ox/sessions.db` (default: `~/.local/share/ox/sessions.db`), and each belongs to a project: the Git repository containing the directory where you start `ox`, or that directory itself outside a repository. This is the same project root that [instruction files](instructions.md#where-instruction-files-can-live) load from, so sessions started in different subdirectories of one repository share a project.

- **Saving**: A session is saved with its first message, so launching `ox` and quitting without typing leaves nothing behind. Saved tool output lives under `$XDG_DATA_HOME/ox/artifacts/` in per-session directories, and each spill file retains up to 10 MiB, which the model can read back.
- **Permissions**: On Unix, the database and saved tool output are created with mode `0600`, since verbatim tool output may include secrets from bash commands. An [export](#moving-a-session-between-machines) holds the same content, so keep it as private.
- **Location**: Keep the data directory on a local filesystem, outside network mounts and file-sync tools, since SQLite cannot safely share its database over either.
- **Older releases**: Releases up to 0.1.0-alpha.9 saved each session as a JSON lines file under `$XDG_DATA_HOME/ox/sessions/`. The database never reads those files, so you can delete that directory once you no longer need them.

### Backup and Recovery

To back up your sessions, let SQLite copy the database, which works while `ox` is running. Copying the file directly can capture it mid-write.

```bash
sqlite3 "${XDG_DATA_HOME:-$HOME/.local/share}/ox/sessions.db" ".backup sessions-backup.db"
```

If the database is damaged, for example by a disk failure, `ox` fails at startup with the recovery steps when it cannot read the file's header or schema, and otherwise at the first write or listing that reaches the damage, with `database disk image is malformed`. To recover what SQLite can still read, quit every `ox` process and run:

```bash
cd "${XDG_DATA_HOME:-$HOME/.local/share}/ox"
sqlite3 sessions.db .recover | sqlite3 recovered.db
mkdir damaged && mv sessions.db* damaged/ && mv recovered.db sessions.db
```

Recover before moving anything, so that the recovery also reads the latest commits in `sessions.db-wal`. To start with an empty store instead, run only the `mkdir damaged && mv sessions.db* damaged/` step.

## Listing Sessions

```bash
ox --list                # The 30 most recent unarchived sessions in the current project
ox -la                   # Same, across every project
ox --list --limit 100    # Raise the cap for one run
ox --list --limit 0      # Unbounded
ox --list --archived     # Archived sessions only
```

The table lists the most recently active sessions first, in local time:

```text
ID         Last Active         Msgs   Title
a1b2c3d4   2026-04-18 09:20    12     Fix authentication bug
e5f6a7b8   2026-04-17 17:30    5      Add session persistence
```

With `--all`, a `Project` column shows each session's project, with paths under `$HOME` as `~/...`:

```text
ID         Last Active         Msgs   Project          Title
a1b2c3d4   2026-04-18 09:20    12     ~/work/oxide     Fix authentication bug
9a0b1c2d   2026-04-18 08:05    3      ~/scratch        Investigate UTF-8 truncation
```

Titles wider than the terminal are truncated with `...`, except when output is piped. When more sessions remain past the limit, a trailing `... more` line shows how to widen the listing.

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

- **`-c` / `--continue`** continues the most recent unarchived session in the current project.
- **`-r` / `--resume`** takes an ID prefix or the path of a file [`ox export`](#moving-a-session-between-machines) wrote. A prefix searches the current project first and every project when nothing there matches. With `--all` / `-a`, it searches every project, and a prefix that matches sessions in several projects lists them all as candidates.
- **A prompt** after `-c` or `-r <id-prefix>` continues the session with that prompt, in the TUI or, with `-p`, as a [headless run](headless.md).
- **`-r` without a value** opens the TUI to the same picker as [`/resume`](#mid-session-resume). **Enter** resumes the highlighted session, and **Esc** quits `ox` without saving a session. The picker cannot be combined with a prompt, `--continue`, `--list`, or `-p`, and fails when stdout is not a terminal.

A resumed session keeps its ID and gains the new messages, unless another `ox` process also writes to it, as [Several Processes on One Session](#several-processes-on-one-session) describes. The model's [task list](quickstart.md#tasks-and-background-commands) returns as it was, while background shells end with the run that started them. The status line's session cost continues from the requests the session recorded.

### Mid-Session Resume

Inside the TUI, `/resume` (alias `/continue`) opens a session picker without restarting the process.

- **Picker**: Type to filter by ID, title, or project. **Tab** toggles current-project / all-projects scope, **Ctrl+A** toggles active / archived sessions, and **Enter** resumes the highlighted session in place. The picker lists every session in the selected scope, most recent first, and loads older ones as you scroll, while a query searches all of them.
- **`/resume <id-prefix>`** jumps directly, and an ambiguous prefix lists the candidates. It matches active and archived sessions other than the live one, searching the current project first and all projects only when the current one has no match.
- **After a resume**, the chat repopulates from the resumed transcript. Prompts queued before the resume are discarded because they were typed for the previous session, and a prompt sent while it loads runs in the resumed session. If a resume fails, a prompt sent while it ran waits until a retried `/resume` lands.

### Deleting a Session

In the `/resume` picker, **Ctrl+D** on the cursor row asks for confirmation, and `/delete <id-prefix>` asks the same for any saved session except the live one. **Y** deletes the session and its saved tool output and prints `Deleted session {id}: {title}` in chat, and **N**, **Esc**, or **Ctrl+C** dismisses. The confirmation ignores keys until you pause typing for 300 ms, so a keystroke typed ahead cannot confirm it.

After **Y**, dismissing the modal does not stop the delete, which still reports its result in chat, and `ox` drops other session actions until it finishes. A session another `ox` process has open cannot be deleted until that process quits or moves to another session.

### Archiving a Session

Archiving hides a session from `ox --list`, the `/resume` picker, and `ox -c`, while keeping it saved.

- `/archive` archives the live session and starts a fresh one, as `/clear` does. A note in the new chat names the archived session.
- `/archive <id-prefix>` archives another saved session. If the prefix matches only the live session, the command is refused and points to bare `/archive`.
- `/unarchive <id-prefix>` makes a saved session visible in the list again.
- In the `/resume` picker, **Ctrl+X** archives the highlighted session, or unarchives it in the archived view. The session leaves the list, the highlight moves to the next one, and the footer briefly names the moved session.

Archiving and unarchiving leave a session's last-active time unchanged, so an unarchived session returns to its previous position in the list. To find archived sessions, run `ox --list --archived` or press **Ctrl+A** in the `/resume` picker. Resuming an archived session by any route unarchives it and displays a note.

## Several Processes on One Session

Two `ox` processes can hold the same session open, as after `ox -c` in two terminals. A process continues the session as long as no other process changed its conversation since this process last wrote to it or resumed it. Once another process has, the next message continues in a new session that copies the conversation as this process saw it, and `ox` shows a notice like `Another ox process wrote to session a1b2c3d4, so this conversation continues as session e5f6a7b8`. Renaming or archiving leaves the conversation unchanged.

The new session keeps the title and shows `(fork of a1b2c3d4)` after it in `ox --list` and the `/resume` picker. A [headless run](headless.md) that forks reports the new session's ID.

## Moving a Session Between Machines

`ox export <id-prefix>` prints a session as JSON lines, every branch and pre-compaction message included, and `ox -r <file>` imports that file and resumes it, on this machine or another:

```bash
ox export a1b2 >a1b2.jsonl
ox -r a1b2.jsonl
```

Importing a session the store already holds keeps whichever copy is further along, even when one side compacted or rewound since the other saw it. If both copies gained different messages, the import lands as a fork of the local session, and later imports of that session extend the same fork. A session the import creates belongs to the project where you run `ox -r`.

## Titles

In the TUI, oxide-code generates a concise AI title (3-7 words) shortly after your first prompt. Headless sessions keep the first prompt as the title.

`/rename` opens a title editor pre-filled with the current title, and `/rename <title>` sets it directly. Your title replaces the AI title and stops AI title generation for that session.

Every title keeps only its first non-blank line and is cut short with `...` past 80 columns, and the `/rename` editor stops taking input at 80 columns.
