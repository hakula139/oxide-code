# Session Persistence (Reference)

Research on session storage patterns. Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87) and [OpenAI Codex](https://github.com/openai/codex).

## Claude Code (TypeScript)

- JSONL at `~/.claude/projects/{SANITIZED_PATH}/{UUID}.jsonl` — project-scoped.
- 18+ entry types (messages, tags, titles, agents, PR links, context compression, file history, attribution, worktree state).
- Head / tail extraction for listing. 32 concurrent reads, pagination.
- Async write queue batched at 100 ms (`FLUSH_INTERVAL_MS`).
- `parentUuid` chain for conversation forking (`--fork-session`).
- File permissions `0o600`, parent dir `0o700`.

## OpenAI Codex (Rust)

- Date-hierarchical paths: `~/.codex/sessions/YYYY/MM/DD/rollout-{TIMESTAMP}-{UUID}.jsonl`.
- SQLite index for fast metadata queries.
- `RolloutItem` replay model.
- Ephemeral flag to skip persistence for testing.
- Bounded `mpsc` channel + `tokio::spawn`-ed `RolloutWriterTask`. Cmds: `AddItems`, `Persist`, `Flush`, `Shutdown` — each carries a `oneshot::Sender<io::Result<()>>` ack.
- Receive-and-drain inside the writer loop — `recv().await` for the first cmd, then `try_recv()` non-blocking to coalesce queued cmds into a single batch flush.
