# Tool Output Truncation

Two-layer truncation pairs per-tool view-shape caps with a centralized byte-budget safety net. Output cut by the dispatcher cap or `bash` capture spills to a file the model can read back.

## Centralized Dispatcher Cap

The dispatcher caps every tool output at `MAX_OUTPUT_BYTES = 128 KiB` (including the omission marker), spilling oversized output and retaining head and tail around the marker.

## Spill Files

- **Location.** Stored under `tool-results/` inside `store::artifact_dir` (`$XDG_DATA_HOME/ox/artifacts/<session-id>/`). The engine passes the live handle's directory per dispatch so spills follow `/clear`, `/resume`, and [lazy forks](persistence.md#lazy-fork) (copied fork tool results keep referencing the source session's spills).
- **Creation.** Created lazily on first spill with no-overwrite creation (`0700` dir, `0600` file on Unix), leaving nothing on disk when output fits.
- **Size.** Capped at `MAX_SPILL_BYTES` (`== MAX_TRACKED_FILE_SIZE` so `read` can open every spill). Beyond that limit, the marker reports `first 10 MiB of output saved to <path>`. Spill write failures still truncate and put the error in the marker.
- **Access and cleanup.** Spill reads and edits obey [permission rules and mode](../guide/permissions.md). `/delete` removes artifacts with session rows, while archiving keeps both.

## Per-Tool View-Shape Caps

- **`bash`.** Bounded head-and-tail streaming capture spills oversized output, sharing one pipe ([**Share one pipe between stdout and stderr**](bash.md#design-decisions)) to preserve arrival order. Background-shell reads drain output since the prior read.
- **`glob`.** Keeps the newest matches plus a total count.
- **`grep`.** Keeps a bounded prefix per mode: count mode scans completely for exact totals, while content and file-list modes stop once full.
- **`read`.** Emits complete rows within line and byte budgets, refusing rows that cannot fit.
- **`edit`, `tasks`, and `write`.** Return compact success messages.

## Design Decisions

1. **Two truncation layers, separated by responsibility.** View-shape caps need tool-specific semantics, while the post-`Tool::run` byte budget acts as an unconditional safety net.
2. **Head-and-tail strategy.** Keeps both setup context (head) and final outcome (tail).
3. **Spill the cut output to a session artifact.** Lets `read` and `grep` inspect omitted middles on demand while tying cleanup to `/delete`. `bash` spills incrementally while draining so full output never sits in memory, and background shells spill per read. Because the 10 MiB cap applies per file, repeated reads of a long-running background shell remain unbounded in aggregate ([#679](https://github.com/hakulab/oxide-code/issues/679)).
4. **Complete Read rows.** `edit` requires exact line text and `write` requires full byte coverage, whereas `grep` lists many matches and truncates lines at 500 characters.
5. **A fixed 1 KiB window for line-boundary cuts.** Bounding the snap window to 1 KiB avoids dropping multi-KiB partial lines (keeping lost budget under 2% of each 64 KiB end) while falling back to character cuts so long lines like minified code stay visible.

## Sources

- `crates/ox-core/src/file_tracker.rs`: `MAX_TRACKED_FILE_SIZE`.
- `crates/ox-core/src/session/store.rs`: `SessionStore::artifact_dir` and `SessionStore::delete`.
- `crates/ox-core/src/tool/bash.rs`: `OUTPUT_BUDGET`, `ShellTree::spawn`, `PipeCapture`, `render_pipe`, `ends_within`.
- `crates/ox-core/src/tool/bash/background.rs`: `ShellOutput::take`, the per-read capture of a background shell.
- `crates/ox-core/src/tool/glob.rs`: `MAX_RESULTS`, the `GlobFiles` view's total.
- `crates/ox-core/src/tool/grep.rs`: `DEFAULT_HEAD_LIMIT`, `MAX_LINE_CHARS`, `truncate_line`, and `MAX_WALK_FILE_SIZE`.
- `crates/ox-core/src/tool/read.rs`: `DEFAULT_LINE_LIMIT`, `OUTPUT_BUDGET`.
- `crates/ox-core/src/tool/search.rs`: `SearchRows`, the shared row and byte cap, `newest_first`, `MAX_NOTE_PATHS`, and `push_path_note`.
- `crates/ox-core/src/tool/spill.rs`: `run_capped`, `cap_output`, `MAX_OUTPUT_BYTES`, `LINE_SNAP_BYTES`, `whole_line_head`, `whole_line_tail`, `Spill`, `MAX_SPILL_BYTES`, `SPILL_DIR`.
