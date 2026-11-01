# Sessions

oxide-code automatically saves every conversation to disk. You can list past sessions and resume where you left off.

## Where Sessions Live

Sessions are stored in one SQLite database, `$XDG_DATA_HOME/ox/sessions.db` (default: `~/.local/share/ox/sessions.db`), and each belongs to a project. A session's project is the Git repository containing the directory where you start `ox`, or that directory itself outside a repository, which is the same project root that [instruction files](instructions.md#where-instruction-files-can-live) load from. Sessions started in different subdirectories of one repository share a project.

Keep the data directory on a local filesystem, outside network mounts and file-sync tools, since SQLite cannot safely share its database over either. To back up your sessions, let SQLite copy the database, which works while `ox` is running, as in `sqlite3 "${XDG_DATA_HOME:-$HOME/.local/share}/ox/sessions.db" ".backup sessions-backup.db"` or `sqlite3 "${XDG_DATA_HOME:-$HOME/.local/share}/ox/sessions.db" "VACUUM INTO 'sessions-backup.db'"`. Copying the file directly can capture it mid-write.

If the database is damaged, for example by a disk failure, `ox` fails at startup with the recovery steps when it cannot read the file's header or schema, and otherwise at the first write or listing that reaches the damage, with `database disk image is malformed`. To recover what SQLite can still read, quit every `ox` process, recover before moving anything so that the recovery also reads the latest commits in `sessions.db-wal`, then move the database aside together with its `-wal` and `-shm` files:

```bash
cd "${XDG_DATA_HOME:-$HOME/.local/share}/ox"
sqlite3 sessions.db .recover | sqlite3 recovered.db
mkdir damaged && mv sessions.db* damaged/ && mv recovered.db sessions.db
```

To start with an empty store instead, run only `mkdir damaged && mv sessions.db* damaged/`.

Releases up to 0.1.0-alpha.9 saved each session as a JSON lines file under `$XDG_DATA_HOME/ox/sessions/`. The database starts empty and never reads those files, so you can delete that directory once you no longer need them.

A session is saved with its first message, so launching `ox` and quitting without typing leaves nothing behind.

When a tool's output is too long to send whole to the model, the full output is saved under `$XDG_DATA_HOME/ox/artifacts/` in a directory named after the session ID, where the model can read it back.

## Listing Sessions

```bash
ox --list                # Shows the 30 most recent unarchived sessions in the current project
ox -la                   # Same, across every project
ox --list --limit 100    # Raise the cap for one run
ox --list --limit 0      # Unbounded
ox --list --archived     # Archived sessions only
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

Continue the most recent unarchived session in the current project:

```bash
ox -c              # Short form
ox --continue      # Long form
```

Resume a specific session with `--resume` / `-r`, using an ID prefix or the path of a file [`ox export`](#moving-a-session-between-machines) wrote. A prefix searches the current project first and every project when nothing there matches:

```bash
ox -r a1b2
ox -r ./a1b2.jsonl
```

Widen the search to every project with `--all` / `-a`, where a prefix that matches sessions in several projects lists them all as candidates:

```bash
ox -c --all        # Latest unarchived session across all projects
ox -r a1b2 -a      # Prefix match across all projects
```

A prompt after `-c` or after `-r <id-prefix>` continues the session with that prompt, in the TUI or, with `-p`, as a [headless run](headless.md):

```bash
ox -c "Run the tests again"
ox -r a1b2 -p "Summarize what changed"
```

Without a value, `--resume` lets you pick a session from a list:

```bash
ox -r              # Sessions in the current project
ox -r --all        # Sessions in every project
```

This opens the TUI to the same picker as [`/resume`](#mid-session-resume). Press **Enter** to resume the highlighted session or **Esc** to quit `ox` without saving a session. The picker cannot be combined with a prompt, `--continue`, `--list`, `--no-tui`, or `-p`. It requires the TUI and fails when stdout is not a terminal.

On resume, oxide-code loads the saved history and adds new messages to the same session, so it keeps its original ID unless another `ox` process also writes to it, as [Several Processes on One Session](#several-processes-on-one-session) describes. The model's [task list](quickstart.md#tasks-and-background-commands) returns as it was, while background shells do not, since they end with the run that started them. The status line's session cost continues from the requests the session recorded.

### Mid-Session Resume

Inside the TUI, `/resume` (alias `/continue`) opens a session picker without restarting the process.

- Type to filter by ID, title, or project. Press **Tab** to toggle current-project / all-projects scope and **Ctrl+A** to toggle active / archived sessions. Press **Enter** to resume the highlighted session in place.
- To skip the picker, pass `/resume <id-prefix>` to jump directly, which also works in the `--no-tui` REPL. An ambiguous prefix lists the candidates. This form matches active and archived sessions, excluding the live session. It checks the current project first and searches all projects only when there is no match in the current project.
- The picker lists every session in the selected scope and archive view, most recent first, and loads older ones as you scroll. Typing a query searches all of them.
- Chat repopulates from the resumed transcript. Prompts queued before the resume are discarded because they were typed for the previous session, and a prompt sent while it loads runs in the resumed session. If a resume fails, a prompt sent while it ran waits, and runs once a retried `/resume` lands. The next prompt continues the resumed thread just as it would after `ox -c`.

### Deleting a Session

Inside the `/resume` picker, **Ctrl+D** on the cursor row opens a Y/N confirm modal. The modal ignores keys until you pause typing for 300 ms, so a keystroke typed ahead cannot confirm it. **Y** deletes the session and its saved tool output, and prints a `Deleted session {id}: {title}` line in chat. **N / Esc / Ctrl+C** dismisses. Dismissing the modal after **Y** does not stop the delete, which still reports its result in chat, and ox drops other session actions until it finishes. `/delete <id-prefix>` opens the same confirmation for any saved session except the live one. A session another `ox` process has open cannot be deleted until that process quits or moves to another session.

### Archiving a Session

Archiving hides a session from `ox --list`, the `/resume` picker, and `ox -c`, while keeping it saved.

- `/archive` archives the live session and starts a fresh one, as `/clear` does. A note in the new chat names the archived session.
- `/archive <id-prefix>` archives another saved session. If the prefix matches only the live session, the command is refused and points to bare `/archive`.
- `/unarchive <id-prefix>` makes a saved session visible in the list again.
- In the `/resume` picker, **Ctrl+X** archives the highlighted session or unarchives it when viewing archived sessions. The session disappears from the list once the archive completes, the highlight moves to the next session, and the footer names the session that was moved for a few seconds. The filter and scope remain unchanged.

The typed commands and **Ctrl+X** leave a session's last-active time unchanged, so an unarchived session returns to its previous position in the list.

To find archived sessions, run `ox --list --archived` or press **Ctrl+A** in the `/resume` picker to switch to the archived view. Resuming an archived session unarchives it and displays a note, whether you select it in that view or specify it with `/resume <id-prefix>`, `ox -r <id-prefix>`, or an export's path.

## Several Processes on One Session

Two `ox` processes can hold the same session open, such as after `ox -c` in two terminals. A process continues the session as long as no other process changed its conversation since this process last wrote to it or resumed it. Once another process has, the next message continues in a new session that copies the conversation as this process saw it, and `ox` shows a notice like `Another ox process wrote to session a1b2c3d4, so this conversation continues as session e5f6a7b8`. Renaming or archiving leaves the conversation unchanged.

The new session keeps the title and shows `(fork of a1b2c3d4)` after it in `ox --list` and the `/resume` picker. A [headless run](headless.md) that forks reports the new session's ID.

## Moving a Session Between Machines

`ox export <id-prefix>` prints a session as JSON lines, every branch and pre-compaction message included, and `ox -r <file>` imports that file and resumes it, on this machine or another:

```bash
ox export a1b2 >a1b2.jsonl
ox -r a1b2.jsonl
```

Importing a session the store already holds keeps whichever copy is further along, even when one side compacted or rewound since the other saw it. If both copies gained different messages, the import lands as a fork of the local session, and later imports of that session extend the same fork. A session the import creates belongs to the project where you run `ox -r`, so it lists there even when the other machine kept it under another path.

## Titles

When the TUI is running, oxide-code generates a concise AI title (3-7 words) shortly after your first prompt. Outside the TUI (bare REPL, headless mode), sessions keep the first prompt as the title, or the command line when the session starts with `/init`, such as `/init AGENTS.md`.

In the TUI, `/rename` opens a title editor pre-filled with the current title, and `/rename <title>` sets it directly, as it does in the `--no-tui` REPL. Your title replaces the AI title, and AI title generation stops for that session.

Every title, whether it comes from the first prompt, the AI, or a typed `/rename <title>`, keeps only its first non-blank line and is cut short with `...` past 80 columns. The `/rename` editor stops taking input at 80 columns.

## Security

On Unix, the session database and its saved tool output are created with user-only permissions (`0600`) because they contain verbatim tool output, which may include secrets from bash commands. An export holds the same content, so keep it as private as the database.

## Headless and REPL Modes

Both the bare REPL (`ox --no-tui`) and headless runs (`ox -p`) save sessions. [Headless Mode](headless.md) describes how a headless run starts or continues one.
