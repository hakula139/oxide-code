# Headless Mode

`ox -p "<prompt>"` runs a single prompt without the TUI and exits when the turn ends. Use it in scripts, CI jobs, and programs that drive `ox`.

```bash
ox -p "Summarize the TODOs in src/"                 # Print the reply as text
ox -p "List the public functions" --output json     # Print one JSON result at the end
ox -p "Fix the failing test" --output stream-json   # Stream every event as a JSON line
```

- **Sessions**: Each run starts and saves a new session, which you can resume later in the TUI with `ox -c <session_id>`.
- **Approvals**: No one can respond to approval prompts during a headless run, so calls that require approval are denied. The denial tells the model which allow pattern would permit the call, or which rule holds it. Add `allow` patterns for the calls your script needs, as described in [Tool approval](configuration.md#permission-tool-approval).

## Output Formats

`--output` picks what stdout carries:

| Format           | stdout                                                                        |
| ---------------- | ----------------------------------------------------------------------------- |
| `text` (default) | The reply as it streams, plus dimmed thinking when `tui.show_thinking` is set |
| `json`           | One [result](#result) line once the run ends                                  |
| `stream-json`    | One JSON line per [engine event](#events), then the [result](#result)         |

Every format also writes tool calls, the task list, retries, and errors as text to stderr. When parsing JSON, read stdout alone.

## Exit Status

| Status              | Meaning                                                     |
| ------------------- | ----------------------------------------------------------- |
| `0`                 | The turn completed                                          |
| `1`                 | The turn failed, or `ox` could not start it                 |
| `2`                 | The command line was invalid, such as an unknown `--output` |
| `129`, `130`, `143` | SIGHUP, SIGINT, or SIGTERM stopped the run                  |

If `ox` cannot start a run, for example because of a configuration or credential error, it prints the error to stderr and emits no result.

## Result

The result is the line printed by `json` and the final line of `stream-json`:

```json
{"type":"result","data":{"stop_reason":"completed","error":null,"message":"Done.","usage":{"context_tokens":1000,"context_window":1000000,"estimated_cost_usd":0.018},"session_id":"3f2a9c1e-5b7d-4e8f-9a0b-1c2d3e4f5a6b"}}
```

| Field         | Description                                                                                                                          |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `stop_reason` | `completed` when the reply finished, `failed` when an error or a refusal ended the turn, or `shutdown` when a signal stopped the run |
| `error`       | Why the turn failed, or `null`                                                                                                       |
| `message`     | The text the model wrote after its last tool call, or `null` unless the turn completed                                               |
| `usage`       | The latest usage, or `null` when no request reported any                                                                             |
| `session_id`  | The run's session, which `ox -c` resumes                                                                                             |

`usage` contains the following fields, each set to `null` when unknown:

- **`context_tokens`**: The input size of the latest request, including cached tokens. It shows how full the context window is.
- **`context_window`**: The model's context window, measured in tokens.
- **`estimated_cost_usd`**: The estimated cost of all requests in the run at public per-token rates, or `null` for models with unknown rates.

A turn reports usage when it ends, so after a signal, `usage` omits requests from the interrupted turn.

## Events

Each `stream-json` line is an object with a `type` and, for most types, a `data` payload. Headless runs emit the following types:

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
| `error`                     | A problem that does not end the turn, such as a session write failure                              |
| `turn_complete`             | No `data`. The turn finished with a reply                                                          |
| `turn_failed`               | Why the turn failed, including a refused request                                                   |

### Event Payloads

- **`rate_limits_updated`** arrives with each response under a Claude subscription sign-in. Its `five_hour` and `seven_day` windows are each an object with `utilization`, the spent share from 0 to 1, and `resets_at`, the reset time in Unix seconds, or `null` when the response omits that window.
- **`tasks_updated`** follows the `tool_call_end` of each successful `tasks` call. Its `data` is the task list in order, each task with its `content` and a `status` of `pending`, `in_progress`, or `completed`. An empty array means the list cleared, which happens once every task is completed or when the model sends an empty list.
- **`background_shells_updated`** arrives when a background shell starts, exits, or stops. Its `data` lists every open shell with its `id`, `command`, and `state`. The state's `type` is `running`, `exited` with the exit code in `data`, or `killed` with the signal number in `data`. A stopped shell leaves the list. When the run ends, every open shell stops without another event, so the last `background_shells_updated` can still list shells as running.

### Result Views

The `view` in `tool_call_end` is the structured result, an object with its own `type` and `data`:

| `view` type    | `data`                                                                                                              |
| -------------- | ------------------------------------------------------------------------------------------------------------------- |
| `text`         | `content`, the result text                                                                                          |
| `read_excerpt` | The file `path`, its numbered `lines` (`number`, `text`), and `total_lines`                                         |
| `diff`         | `chunks` containing numbered `old` and `new` lines, `replace_all`, and the number of `replacements`                 |
| `grep_matches` | Per-file `groups` (`path`, `lines` with `number`, `text`, and `is_match`), and whether the matches were `truncated` |
| `glob_files`   | The `pattern`, the matched `files`, and their `total` count                                                         |
| `tasks`        | The `tasks` the call sent, each with its `content` and `status`                                                     |

### Example

A run that lists files and then attempts `cargo test`, which needs approval, streams lines like these:

```json
{"type":"tool_call_start","data":{"id":"call-1","name":"glob","input":{"pattern":"Cargo.toml"},"icon":"✱","label":"Glob(Cargo.toml)"}}
{"type":"tool_call_end","data":{"id":"call-1","content":"Cargo.toml","is_error":false,"title":"Found 1 file","view":{"type":"glob_files","data":{"pattern":"Cargo.toml","files":["Cargo.toml"],"total":1}}}}
{"type":"tool_call_start","data":{"id":"call-2","name":"bash","input":{"command":"cargo test"},"icon":"$","label":"cargo test"}}
{"type":"tool_call_end","data":{"id":"call-2","content":"The bash call was denied with this feedback: This run cannot ask for approval. To allow this call, add `\"cargo test\"` to the `exec` list ...","is_error":true,"title":null,"view":{"type":"text","data":{"content":"The bash call was denied with this feedback: This run cannot ask for approval. To allow this call, add `\"cargo test\"` to the `exec` list ..."}}}}
{"type":"stream_token","data":"Done."}
{"type":"usage_updated","data":{"context_tokens":1000,"context_window":1000000,"estimated_cost_usd":0.018}}
{"type":"turn_complete"}
{"type":"result","data":{"stop_reason":"completed","error":null,"message":"Done.","usage":{"context_tokens":1000,"context_window":1000000,"estimated_cost_usd":0.018},"session_id":"3f2a9c1e-5b7d-4e8f-9a0b-1c2d3e4f5a6b"}}
```

The run cannot ask, so the `cargo test` call ends with a `tool_call_end` whose `is_error` is `true` and whose `content` tells the model the call was denied, naming the allow pattern that would permit it when one can.

## Compatibility

Within a minor version, JSON output preserves its `type` tags, field names, `stop_reason` values, and `view` shapes. Later versions may add event types and fields, so skip any you don't recognize.

Display text can change in any release. This includes a tool call's `icon`, `label`, and `title`, the wording of `content`, `error`, and `message` strings, and everything written to stderr.
