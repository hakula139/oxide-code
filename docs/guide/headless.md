# Headless Mode

`ox -p "<prompt>"` (`--print`) runs one turn without the TUI and exits. Use it in scripts, CI jobs, and automation. Interactive mode requires terminals on both stdin and stdout.

```bash
ox -p "Summarize the TODOs in src/"                 # Print the reply as text
git diff | ox -p "Review this diff"                 # Attach piped stdin to the prompt
ox -p "List the public functions" --output json     # Print one JSON result at the end
ox -p "Fix the failing test" --output stream-json   # Stream every event as a JSON line
ox -c -p "Run the tests again"                      # Continue the latest session
```

- **Input**: Non-terminal stdin is read to EOF and appended after the prompt in a `<stdin>` block. Without a prompt, piped stdin becomes the prompt (`ox -p < task.md`). Invalid UTF-8 bytes become U+FFFD.
- **Open stdin**: If a caller leaves stdin open, `ox` waits and warns on stderr after 3 seconds. Redirect stdin from `/dev/null` to avoid blocking.
- **Usage errors**: Missing both a prompt and stdin exits with status `2`, as does a prompt matching a bare slash-command name (pass `--` before it to send it).
- **Sessions**: Each run saves a new session unless `-c` continues the latest or `-r` resumes by ID prefix or [export](sessions.md#moving-a-session-between-machines) path. Resume later with `ox -r <id-prefix>` in the TUI or `-p`. Continuing a session that another `ox` process modified continues in a [fork](sessions.md#several-processes-on-one-session).
- **Questions**: `ask` aborts immediately without reading stdin.
- **Approvals**: Calls needing approval are denied with the message described in [Approval Prompt](permissions.md#approval-prompt). Add `allow` rules ([Rules](permissions.md#rules)), pass `--mode plan` for read-only runs, or pass `--mode yolo` to allow calls left open by deny and ask rules (run untrusted work in a sandbox or container, as [SECURITY.md](../../SECURITY.md#trust-model) explains). Commands with syntax `ox` cannot analyze skip command-specific allow rules and fall back to the mode ([Chained Commands and Unsupported Syntax](permissions.md#chained-commands-and-unsupported-syntax)).

## Output Formats

`--output` selects the stdout format:

| Format           | stdout                                                                        |
| ---------------- | ----------------------------------------------------------------------------- |
| `text` (default) | The reply as it streams, plus dimmed thinking when `tui.show_thinking` is set |
| `json`           | One [result](#result) line once the run ends                                  |
| `stream-json`    | One JSON line per [event](#events), then the [result](#result)                |

All formats write tool calls, task lists, retries, notices, and errors to stderr, plus progress notes unless `text` already streams thinking on stdout (`tui.show_thinking`). Parse JSON from stdout only.

If stdout closes early (such as piping to `head -5`), `text` and `stream-json` stop at the next safe point like **Ctrl+C** and still save the session. `json` buffers until the end, so it finishes the turn before failing to write.

## Exit Status

| Status              | Meaning                                                                                                              |
| ------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `0`                 | The turn completed                                                                                                   |
| `1`                 | The turn failed, output could not be written, the session could not be saved, or `ox` could not start the turn       |
| `2`                 | The command line was invalid, such as an unknown `--output`, or gave no prompt                                       |
| `101`               | `ox` crashed, and the JSON formats still print a `failed` result                                                     |
| `129`, `130`, `143` | SIGHUP, SIGINT, or SIGTERM stopped the run, and `ox` died by that signal, which shells report as 128 plus its number |

Startup failures (such as config or credential errors) print to stderr and emit no JSON result.

## Result

`json` and `stream-json` emit this result object:

```json
{
  "type": "result",
  "data": {
    "stop_reason": "completed",
    "error": null,
    "session_write_error": null,
    "message": "Done.",
    "usage": {
      "context_tokens": 1000,
      "context_window": 1000000,
      "estimated_cost_usd": 0.018
    },
    "session_id": "3f2a9c1e-5b7d-4e8f-9a0b-1c2d3e4f5a6b"
  }
}
```

| Field                 | Description                                                                                                                          |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `stop_reason`         | `completed` when the reply finished, `failed` when an error or a refusal ended the turn, or `shutdown` when a signal stopped the run |
| `error`               | Why the turn failed, or `null`                                                                                                       |
| `session_write_error` | Why the session could not be saved, which leaves part of the run missing from its transcript, or `null`                              |
| `message`             | The text the model wrote after its last tool call, or `null` unless the turn completed                                               |
| `usage`               | The latest usage, or `null` when no request reported any                                                                             |
| `session_id`          | The run's session, or the fork it continued in, which `ox -r <id-prefix>` resumes                                                    |

`usage` fields (each `null` when unknown):

- **`context_tokens`**: Latest request input tokens, including cached tokens (shows context window fill).
- **`context_window`**: Model context window in tokens.
- **`estimated_cost_usd`**: Cumulative estimated session cost at public per-token rates (restored on resume), or `null` when no positive estimate is available.

Usage is reported at turn end and before each automatic compaction, so after a signal it omits requests sent since the latest report.

## Events

Each `stream-json` line has a `type` and, for most types, a `data` payload. [Engine](../design/engine.md) defines the event contract, including which event ends each operation.

| `type`                      | `data`                                                                                             |
| --------------------------- | -------------------------------------------------------------------------------------------------- |
| `stream_token`              | The next chunk of reply text                                                                       |
| `thinking_token`            | The next chunk of the model's thinking, when the model streams it                                  |
| `progress_update`           | A note the model wrote before a tool call, which also appears in its thinking                      |
| `retrying`                  | Notice that a request failed and will be sent again                                                |
| `tool_call_start`           | A tool call: `id`, `name`, the model's `input`, and its display `icon` and `label`                 |
| `tool_call_end`             | The call's result: `id`, the `content` the model sees, `is_error`, a `title` or `null`, and `view` |
| `usage_updated`             | The usage object described under [Result](#result)                                                 |
| `rate_limits_updated`       | A subscription's usage limits, described below                                                     |
| `tasks_updated`             | The session's whole task list, described below                                                     |
| `background_shells_updated` | Every open background shell, described below                                                       |
| `auto_compaction_started`   | No `data`. An automatic compaction started, before the prompt or between the turn's rounds         |
| `summary_token`             | The next chunk of the compaction summary                                                           |
| `summary_discarded`         | No `data`. The summary streamed so far was dropped, because a retry starts over or it failed       |
| `session_compacted`         | The compaction finished, described below                                                           |
| `notice`                    | A warning the run works around, such as an instruction file it cannot read                         |
| `error`                     | A problem that does not end the turn, such as a session write failure                              |
| `turn_complete`             | No `data`. The turn finished with a reply                                                          |
| `prompt_unsent`             | No `data`. A failure or a cancel stopped the prompt before it reached the model                    |
| `turn_rolled_back`          | No `data`. The model refused, so the turn was removed from the session                             |
| `turn_failed`               | Why the turn failed, including a refused request                                                   |

### Event Payloads

- **`rate_limits_updated`**: Sent with each Claude subscription response. `five_hour` and `seven_day` each hold `utilization` (0 to 1) and `resets_at` (Unix seconds), or `null` when omitted.
- **`tasks_updated`**: Follows each successful `tasks` `tool_call_end`. `data` is the ordered task list (`content` and `status`: `pending`, `in_progress`, or `completed`). An empty array clears the list (when all tasks complete or the model sends an empty list).
- **`session_compacted`**: Replaces the conversation with a summary near the context limit. `data` holds `summary`, `pre_count` (replaced messages), `instructions` (`null` for automatic compactions), and `trigger` (`mid_turn` between rounds, or `auto` before a continued or resumed prompt already past the threshold). Turns continue after `mid_turn`. Failed compactions end with `turn_failed` (after `prompt_unsent` if the prompt never went out). Because `-p` sends `/compact` as text, resume with `ox -r <id-prefix>` in the TUI to compact manually.
- **`turn_rolled_back`**: Precedes `turn_failed` on a refusal, dropping the refused turn (or only post-summary work after a `mid_turn` compaction).
- **`background_shells_updated`**: Sent when a background shell starts, exits, or stops. `data` lists open shells with `id`, `command`, and `state` (`type` is `running`, `exited` with exit code in `data`, or `killed` with signal number in `data`). Stopped shells drop from the list. Remaining shells stop at run end without emitting a final event.

### Result Views

`view` in `tool_call_end` is a typed structured result (`type` and `data`):

| `view` type    | `data`                                                                                                              |
| -------------- | ------------------------------------------------------------------------------------------------------------------- |
| `text`         | `content`, the result text                                                                                          |
| `read_excerpt` | The file `path`, its numbered `lines` (`number`, `text`), and `total_lines`                                         |
| `diff`         | The `hunks` in file order, `locations` (the line each replacement applied at), and `omitted_lines`                  |
| `grep_matches` | Per-file `groups` (`path`, `lines` with `number`, `text`, and `is_match`), and whether the matches were `truncated` |
| `glob_files`   | The `pattern`, the matched `files`, and their `total` count                                                         |
| `tasks`        | The `tasks` the call sent, each with its `content` and `status`                                                     |

Each `diff` hunk's `lines` carry `kind` (`context`, `removed`, or `added`), `number` (old-file line for `removed`, new-file line otherwise), `text` without line endings, and `changed` byte ranges (`start`, `end`) for partial-line edits. For line-ending-only diffs, `text` keeps endings as `␍` (`\r`) and `↵` (`\n`). Diffs truncate at 2000 lines, with `omitted_lines` counting the rest.

### Example

Example stream listing files and attempting `cargo test` (denied for needing approval):

```json
{"type":"tool_call_start","data":{"id":"call-1","name":"glob","input":{"pattern":"Cargo.toml"},"icon":"✱","label":"Glob(Cargo.toml)"}}
{"type":"tool_call_end","data":{"id":"call-1","content":"Cargo.toml","is_error":false,"title":"Found 1 file","view":{"type":"glob_files","data":{"pattern":"Cargo.toml","files":["Cargo.toml"],"total":1}}}}
{"type":"tool_call_start","data":{"id":"call-2","name":"bash","input":{"command":"cargo test"},"icon":"$","label":"cargo test"}}
{"type":"tool_call_end","data":{"id":"call-2","content":"The bash call was denied because it needs approval and this run cannot ask for it. No allow pattern matches it. Do not retry it as written. To allow it, ask the user to add `\"cargo test\"` to `permission.allow.exec` in the user config.","is_error":true,"title":null,"view":{"type":"text","data":{"content":"The bash call was denied because it needs approval and this run cannot ask for it. No allow pattern matches it. Do not retry it as written. To allow it, ask the user to add `\"cargo test\"` to `permission.allow.exec` in the user config."}}}}
{"type":"stream_token","data":"Done."}
{"type":"usage_updated","data":{"context_tokens":1000,"context_window":1000000,"estimated_cost_usd":0.018}}
{"type":"turn_complete"}
{"type":"result","data":{"stop_reason":"completed","error":null,"session_write_error":null,"message":"Done.","usage":{"context_tokens":1000,"context_window":1000000,"estimated_cost_usd":0.018},"session_id":"3f2a9c1e-5b7d-4e8f-9a0b-1c2d3e4f5a6b"}}
```

## Compatibility

Within a minor version, JSON output preserves `type` tags, field names, `stop_reason` values, and `view` shapes. Ignore unknown event types and fields added in later versions. Display text (`icon`, `label`, `title`, `content`, `error`, `message`, and stderr output) may change in any release.
