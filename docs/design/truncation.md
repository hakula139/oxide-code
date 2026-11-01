# Tool Output Truncation

Two-layer truncation combines per-tool view-shape caps with a centralized byte-budget safety net. Output cut by the dispatcher cap or a `bash` output capture is saved in full to a spill file the model can read back.

## Centralized Dispatcher Cap

The dispatcher caps every tool output at `MAX_OUTPUT_BYTES = 128 KiB`, including the omission marker. Oversized results spill to a file and retain their head and tail around the marker.

## Spill Files

- **Location**: `store::artifact_dir` designates the directory that holds a session's artifacts, `$XDG_DATA_HOME/ox/artifacts/<session-id>/`, with spill files placed in its `tool-results/` child. The engine passes the live handle's directory with each dispatch, so new spills land with the session in use after `/clear`, `/resume`, or a [lazy fork](persistence.md#lazy-fork). Tool results a fork copied still name the source session's spills.
- **Creation**: The first write creates the directory and the file and refuses to replace an existing file, so a result that fits leaves nothing on disk. On Unix, the directory gets `0700` perms and the file gets `0600`.
- **Size**: A file keeps at most `MAX_SPILL_BYTES`, equal to `MAX_TRACKED_FILE_SIZE` so `read` can open every spill, and beyond that limit the marker says `first 10 MiB of output saved to <path>`. If creating or writing the file fails, the result is still truncated, and the marker names the error in place of the path.
- **Access and cleanup**: `read` and `grep` open spill files without prompting, while editing them prompts, since they lie in oxide-code's data directory ([built-in rules](../guide/permissions.md#built-in-rules)). `/delete` removes the session's directory along with its rows, and archiving keeps both.

## Per-Tool View-Shape Caps

| Tool                       | View boundary                                                                                                                                                                                                                                  |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `bash`                     | Bounded head-and-tail streaming capture, with full output spilled to disk. A [shared pipe](bash.md#design-decisions) preserves stdout and stderr arrival order. Each background-shell read drains the output captured since the previous read. |
| `glob`                     | Newest matches with a total match count                                                                                                                                                                                                        |
| `grep`                     | Bounded prefix for each output mode. Count mode continues reading for exact totals, while content and file-list modes stop when the view fills.                                                                                                |
| `read`                     | Complete rows within a line and byte budget, refusing a row that cannot fit                                                                                                                                                                    |
| `edit` / `tasks` / `write` | Compact success messages                                                                                                                                                                                                                       |

## Design Decisions

1. **Two truncation layers, separated by responsibility.** View-shape stays per-tool because it needs tool-specific knowledge. Byte-budget runs after `Tool::run` as the absolute safety net.
2. **Head-and-tail strategy.** Keeps the command setup and final outcome, the two slices the model needs most. Tail-only would lose the prompt, and head-only would lose the result.
3. **Spill the cut output to a session artifact.** The model gets the head and tail inline, and the marker names a file holding the rest for `read` and `grep` to page through when the middle matters. Keying the directory by session ID ties the files to the session, so `/delete` removes them and nothing else needs to clean up. Because `bash` spills as it drains, a command's full output never sits in memory, and a background shell spills each read's window to its own file. Because the cap applies to each file, and so to each call or read, the total is unbounded: a long-running background shell read many times can fill the disk across its files, which [#679](https://github.com/hakula139/oxide-code/issues/679) tracks.
4. **Complete Read rows.** Read emits each line in full because Edit needs its exact text and Write needs coverage of every byte. Grep lists many hits per call and caps each line at 500 characters.
5. **A fixed 1 KiB window for line-boundary cuts.** A cut always moves to the nearest line break, so a wider window drops no more than the partial line, but it lets a partial line of several KiB vanish whole at the cost of up to that much of each end's budget. 1 KiB is under 2% of a 64 KiB end, while longer lines, such as minified code, keep the char cut and stay visible.

## Sources

- `crates/ox-core/src/file_tracker.rs`: `MAX_TRACKED_FILE_SIZE`.
- `crates/ox-core/src/session/store.rs`: `SessionStore::artifact_dir` and `SessionStore::delete`.
- `crates/ox-core/src/tool/bash.rs`: `OUTPUT_BUDGET`, `ShellTree::spawn()`, `PipeCapture`, `render_pipe()`, `ends_within()`.
- `crates/ox-core/src/tool/bash/background.rs`: `ShellOutput::take`, the per-read capture of a background shell.
- `crates/ox-core/src/tool/glob.rs`: `MAX_RESULTS`, the `GlobFiles` view's total.
- `crates/ox-core/src/tool/grep.rs`: `DEFAULT_HEAD_LIMIT`, `MAX_LINE_CHARS`, `truncate_line()`, and `MAX_WALK_FILE_SIZE`.
- `crates/ox-core/src/tool/read.rs`: `DEFAULT_LINE_LIMIT`, `OUTPUT_BUDGET`.
- `crates/ox-core/src/tool/search.rs`: `SearchRows`, the shared row and byte cap, `newest_first()`, `MAX_NOTE_PATHS`, and `push_path_note()`.
- `crates/ox-core/src/tool/spill.rs`: `run_capped`, `cap_output()`, `MAX_OUTPUT_BYTES`, `LINE_SNAP_BYTES`, `whole_line_head()`, `whole_line_tail()`, `Spill`, `MAX_SPILL_BYTES`, `SPILL_DIR`.
