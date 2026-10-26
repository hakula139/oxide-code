# Session Persistence (Reference)

Research on session storage patterns. Based on [Claude Code](https://github.com/hakula139/claude-code) and [OpenAI Codex](https://github.com/openai/codex).

## Claude Code (TypeScript)

- JSONL at `~/.claude/projects/{SANITIZED_PATH}/{UUID}.jsonl`, project-scoped.
- 18+ entry types (messages, tags, titles, agents, PR links, context compression, file history, attribution, worktree state).
- Head / tail extraction for listing. 32 concurrent reads, pagination.
- Async write queue batched at 100 ms (`FLUSH_INTERVAL_MS`).
- `parentUuid` chain for conversation forking (`--fork-session`).
- File permissions `0600`, parent dir `0700`.

## OpenAI Codex (Rust)

- Date-hierarchical paths: `~/.codex/sessions/YYYY/MM/DD/rollout-{TIMESTAMP}-{UUID}.jsonl`.
- SQLite index for fast metadata queries.
- `RolloutItem` replay model.
- Ephemeral flag to skip persistence for testing.
- Bounded `mpsc` channel to a spawned writer, with `RolloutWriterTask` as a state struct rather than the task. Cmds: `AddItems`, `Discard`, `Persist`, `Flush`, `Shutdown`, and only some carry a `oneshot::Sender<io::Result<()>>` ack, which `AddItems` does not.
- The writer handles one `recv().await` at a time, with no `try_recv()` coalescing of queued cmds.
- Checked 2026-10-26 at codex-rs `2351d9e1b6`, in `codex-rs/rollout/src/recorder.rs`.
