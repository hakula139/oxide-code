# Headless Mode

`ox -p "<prompt>"` (`--print`) answers one prompt without the TUI and exits when the turn ends. Use it in scripts, CI jobs, and programs that drive `ox`.

```bash
ox -p "Summarize the TODOs in src/"                 # Print the reply as text
git diff | ox -p "Review this diff"                 # Attach piped stdin to the prompt
ox -p "List the public functions" --output json     # Print one JSON result at the end
ox -p "Fix the failing test" --output stream-json   # Stream every event as a JSON line
ox -c -p "Run the tests again"                      # Continue the latest session
```

- **Input**: When stdin is not a terminal, `ox -p` reads it to the end and attaches it after the prompt in a `<stdin>` block, so the model can tell your instruction from the data. Piped stdin without a prompt, as in `ox -p < task.md`, is the prompt itself, and a run with neither fails with status `2`. A prompt that is exactly a slash command's name also fails with status `2`, so pass `--` before the word to send it. Bytes that are not valid UTF-8 are replaced with U+FFFD, so a stray byte in a diff or log does not end the run. A script or program that runs `ox` with an open stdin it never closes, such as a pipe it never writes to, leaves `ox` waiting, and after 3 seconds `ox` says so on stderr. Redirect stdin from `/dev/null` to keep it away from `ox`.
- **Sessions**: Each run starts and saves a new session, unless `-c` continues the latest one or `-r` names one by ID prefix or [export](sessions.md#moving-a-session-between-machines) path. Resume a run's session later with `ox -r <id-prefix>`, in the TUI or with another `-p`. A run that continues a session another `ox` process wrote to in the meantime continues in a [fork](sessions.md#several-processes-on-one-session) of it.
- **Approvals**: No one can respond to approval prompts during a headless run, so calls that require approval are denied, and the denial tells the model what [Approval Prompt](permissions.md#approval-prompt) describes. Add `allow` patterns for the calls your script needs, as described in [Rules](permissions.md#rules). Pass `--mode plan` to keep a run read-only, or `--mode yolo` to run every tool call that no deny rule or ask rule of yours holds. In `manual`, a `bash` command that uses shell syntax `ox` cannot analyze is denied, and its denial names the construct and asks the model to rewrite it as plain commands. Since `yolo` runs whatever an `exec` pattern misses, run untrusted work inside a sandbox or container, as [Modes](permissions.md#modes) explains.

## Output Formats

`--output` picks what stdout carries:

| Format           | stdout                                                                        |
| ---------------- | ----------------------------------------------------------------------------- |
| `text` (default) | The reply as it streams, plus dimmed thinking when `tui.show_thinking` is set |
| `json`           | One [result](#result) line once the run ends                                  |
| `stream-json`    | One JSON line per [event](#events), then the [result](#result)                |

Every format also writes tool calls, the task list, retries, notices, and errors as text to stderr, plus the model's progress notes unless a `text` run shows thinking on stdout because `tui.show_thinking` is set. When parsing JSON, read stdout alone.

If the program reading stdout closes it, such as a pipe into `head -5`, a `text` or `stream-json` run stops its turn at the next safe point, as Ctrl+C does, and still saves the session. A `json` run writes nothing until the end, so it finishes the turn and then fails to print the result.

## Exit Status

| Status              | Meaning                                                                                                              |
| ------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `0`                 | The turn completed                                                                                                   |
| `1`                 | The turn failed, output could not be written, the session could not be saved, or `ox` could not start the turn       |
| `2`                 | The command line was invalid, such as an unknown `--output`, or gave no prompt                                       |
| `101`               | `ox` crashed, and the JSON formats still print a `failed` result                                                     |
| `129`, `130`, `143` | SIGHUP, SIGINT, or SIGTERM stopped the run, and `ox` died by that signal, which shells report as 128 plus its number |

If `ox` cannot start a run, for example because of a configuration or credential error, it prints the error to stderr and emits no result.

## Result

The `json` and `stream-json` formats emit this result shape:

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

`usage` contains the following fields, each set to `null` when unknown:

- **`context_tokens`**: The input size of the latest request, including cached tokens. It shows how full the context window is.
- **`context_window`**: The model's context window, measured in tokens.
- **`estimated_cost_usd`**: The cumulative estimated session cost at public per-token rates, restored on resume, or `null` when no positive estimate is available.

A turn reports usage when it ends and before each automatic compaction inside it, so after a signal, `usage` omits the requests sent since the latest report.

## Events

Each `stream-json` line is an object with a `type` and, for most types, a `data` payload. The schema below is the compatibility surface scripts depend on. [Engine](../design/engine.md) describes the operation and event contract behind it, including which event ends each operation. Headless runs emit the following types:

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

- **`rate_limits_updated`** arrives with each response under a Claude subscription sign-in. Its `five_hour` and `seven_day` windows are each an object with `utilization`, the spent share from 0 to 1, and `resets_at`, the reset time in Unix seconds, or `null` when the response omits that window.
- **`tasks_updated`** follows the `tool_call_end` of each successful `tasks` call. Its `data` is the task list in order, each task with its `content` and a `status` of `pending`, `in_progress`, or `completed`. An empty array means the list cleared, which happens once every task is completed or when the model sends an empty list.
- **`session_compacted`** replaces the conversation with a summary when the context nears its window. Its `data` holds the `summary`, the `pre_count` of messages it replaced, `instructions` (`null` for automatic compactions), and the `trigger`, which is `mid_turn` between the turn's rounds and `auto` before the prompt of a continued or resumed session already past the auto-compaction threshold. After `mid_turn`, the turn goes on from the summary. A failed compaction ends the turn with `turn_failed`, after `prompt_unsent` when the prompt never went out. A headless prompt sends `/compact` to the model as text, so to compact after a failure, resume the session with `ox -r <id-prefix>` in the TUI or with `--no-tui` and run `/compact` there.
- **`turn_rolled_back`** precedes the `turn_failed` of a refused request. The session drops the refused turn, or after a `mid_turn` compaction, only the work that followed the summary.
- **`background_shells_updated`** arrives when a background shell starts, exits, or stops. Its `data` lists every open shell with its `id`, `command`, and `state`. The state's `type` is `running`, `exited` with the exit code in `data`, or `killed` with the signal number in `data`. A stopped shell leaves the list. When the run ends, every open shell stops without another event, so the last `background_shells_updated` can still list shells as running.

### Result Views

The `view` in `tool_call_end` is the structured result, an object with its own `type` and `data`:

| `view` type    | `data`                                                                                                                      |
| -------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `text`         | `content`, the result text                                                                                                  |
| `read_excerpt` | The file `path`, its numbered `lines` (`number`, `text`), and `total_lines`                                                 |
| `diff`         | The first replacement's `chunk`, with numbered `old` and `new` lines, and `locations`, the line each replacement applied at |
| `grep_matches` | Per-file `groups` (`path`, `lines` with `number`, `text`, and `is_match`), and whether the matches were `truncated`         |
| `glob_files`   | The `pattern`, the matched `files`, and their `total` count                                                                 |
| `tasks`        | The `tasks` the call sent, each with its `content` and `status`                                                             |

### Example

A run that lists files and then attempts `cargo test`, which needs approval, streams lines like these:

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

The run cannot ask, so the `cargo test` call ends with a `tool_call_end` whose `is_error` is `true` and whose `content` tells the model the call was denied, naming the allow pattern that would permit it when one can.

## Compatibility

Within a minor version, JSON output preserves its `type` tags, field names, `stop_reason` values, and `view` shapes. Later versions may add event types and fields, so skip any you don't recognize.

Display text can change in any release. This includes a tool call's `icon`, `label`, and `title`, the wording of `content`, `error`, and `message` strings, and everything written to stderr.

## Bare REPL

`ox --no-tui` reads prompts from stdin and keeps them in one session. It prints each reply to stdout in the `text` format, while its `>` prompt and all other output go to stderr. Use it when the TUI cannot run or to pipe a series of prompts into one session.

- **Input**: Each line is a prompt, except while an approval waits for its answer, so pasting several lines starts one turn per line. A prompt argument, as in `ox --no-tui "Explain this repo"`, runs before the first line. Blank lines are skipped. Bytes that are not valid UTF-8 are replaced with U+FFFD. The end of input or `/exit` exits the program, as does **Ctrl+D** in a terminal. Lines after `/exit` are not read.
- **Commands**: A subset of the slash commands runs, as listed in [When Commands Run](slash-commands.md#when-commands-run).
- **Approvals**: In a terminal, a call that needs approval prints its command or diff, and the rule that made it ask when one did, then waits for a letter: `y` approves, `a` allows matching calls for the rest of the session when the call offers that, `n` denies, `f` denies with feedback for the model, and `x` aborts the turn. With piped input, no approval is requested, and those calls are denied as in a `-p` run.
- **Interrupting**: In a terminal, **Ctrl+C** during a turn stops it and returns to the prompt, and at the prompt it discards the line, as in a shell. With piped input, **Ctrl+C** ends the run. If the program reading stdout closes it, the turn stops as **Ctrl+C** stops it, and the REPL reads no more lines.
- **Exit status**: The end of input or `/exit` exits with `1` if a prompt failed, a command was rejected, output could not be written, or the session could not be saved, and with `0` otherwise. A signal that ends the run makes `ox` die by that signal, which a shell reports as the status in the [table](#exit-status).
