mod client;
mod config;
mod message;
mod tool;

use std::io::Write;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::warn;

use client::anthropic::{Client, ContentBlockInfo, Delta, StreamEvent};
use config::Config;
use message::{ContentBlock, Message, Role};
use tool::{
    ToolDefinition, ToolMetadata, ToolOutput, ToolRegistry, bash::BashTool, edit::EditTool,
    glob::GlobTool, grep::GrepTool, read::ReadTool, write::WriteTool,
};

const MAX_TOOL_ROUNDS: usize = 25;

#[derive(Parser)]
#[command(name = "ox", version, about = "A terminal-based AI coding assistant")]
struct Cli {}

#[tokio::main]
async fn main() -> Result<()> {
    let _cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::load().await?;
    let client = Client::new(config)?;
    let tools = ToolRegistry::new(vec![
        Box::new(BashTool),
        Box::new(ReadTool),
        Box::new(WriteTool),
        Box::new(EditTool),
        Box::new(GlobTool),
        Box::new(GrepTool),
    ]);

    repl(&client, &tools).await
}

async fn repl(client: &Client, tools: &ToolRegistry) -> Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut messages: Vec<Message> = Vec::new();

    loop {
        eprint!("> ");
        std::io::stderr().flush()?;

        let Some(line) = lines.next_line().await? else {
            break; // EOF
        };

        let input = line.trim().to_owned();
        if input.is_empty() {
            continue;
        }

        messages.push(Message::user(&input));
        agent_turn(client, tools, &mut messages).await?;
    }

    Ok(())
}

async fn agent_turn(
    client: &Client,
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
) -> Result<()> {
    let tool_defs = tools.definitions();

    for _ in 0..MAX_TOOL_ROUNDS {
        let blocks = stream_response(client, messages, &tool_defs).await?;

        let tool_uses: Vec<_> = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect();

        messages.push(Message {
            role: Role::Assistant,
            content: blocks,
        });

        if tool_uses.is_empty() {
            return Ok(());
        }

        let mut results = Vec::new();
        for (id, name, input) in tool_uses {
            display_tool_call(&name, &input);

            let output = match tools.get(&name) {
                Some(t) => t.run(input).await,
                None => ToolOutput {
                    content: format!("Unknown tool: {name}"),
                    is_error: true,
                    metadata: ToolMetadata::default(),
                },
            };

            if let Some(title) = &output.metadata.title {
                eprintln!("  {title}");
            }
            display_tool_output(&output.content);

            results.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content: output.content,
                is_error: output.is_error,
            });
        }

        messages.push(Message {
            role: Role::User,
            content: results,
        });
    }

    bail!("exceeded {MAX_TOOL_ROUNDS} tool rounds")
}

// ── Stream Processing ──

enum BlockAccumulator {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        json_buf: String,
    },
}

impl BlockAccumulator {
    fn into_content_block(self) -> ContentBlock {
        match self {
            Self::Text(text) => ContentBlock::Text { text },
            Self::ToolUse { id, name, json_buf } => {
                let input = serde_json::from_str(&json_buf).unwrap_or_else(|e| {
                    warn!("malformed tool input JSON: {e}");
                    serde_json::Value::Object(serde_json::Map::new())
                });
                ContentBlock::ToolUse { id, name, input }
            }
        }
    }
}

async fn stream_response(
    client: &Client,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> Result<Vec<ContentBlock>> {
    let mut rx = client.stream_message(messages, None, tools)?;

    let mut blocks: Vec<Option<BlockAccumulator>> = Vec::new();
    let mut stdout = std::io::stdout();

    while let Some(event) = rx.recv().await {
        let event = event.context("stream error")?;

        match event {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                if blocks.len() <= index {
                    blocks.resize_with(index + 1, || None);
                }
                blocks[index] = Some(match content_block {
                    ContentBlockInfo::Text { text } => {
                        if !text.is_empty() {
                            write!(stdout, "{text}")?;
                            stdout.flush()?;
                        }
                        BlockAccumulator::Text(text)
                    }
                    ContentBlockInfo::ToolUse { id, name } => BlockAccumulator::ToolUse {
                        id,
                        name,
                        json_buf: String::new(),
                    },
                });
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(Some(block)) = blocks.get_mut(index) {
                    match (block, delta) {
                        (BlockAccumulator::Text(buf), Delta::TextDelta { text }) => {
                            buf.push_str(&text);
                            write!(stdout, "{text}")?;
                            stdout.flush()?;
                        }
                        (
                            BlockAccumulator::ToolUse { json_buf, .. },
                            Delta::InputJsonDelta { partial_json },
                        ) => {
                            json_buf.push_str(&partial_json);
                        }
                        _ => {}
                    }
                }
            }
            StreamEvent::Error { error } => {
                bail!("API error ({}): {}", error.error_type, error.message);
            }
            _ => {}
        }
    }

    // Streamed text deltas don't include a final newline.
    let has_text = blocks
        .iter()
        .any(|b| matches!(b, Some(BlockAccumulator::Text(_))));
    if has_text {
        writeln!(stdout)?;
    }

    Ok(blocks
        .into_iter()
        .flatten()
        .map(BlockAccumulator::into_content_block)
        .collect())
}

// ── Display ──

fn display_tool_call(name: &str, input: &serde_json::Value) {
    if name == "bash"
        && let Some(cmd) = input.get("command").and_then(serde_json::Value::as_str)
    {
        eprintln!("⟡ {name}: {cmd}");
        return;
    }
    eprintln!("⟡ {name}");
}

fn display_tool_output(content: &str) {
    let trimmed = content.trim();
    if !trimmed.is_empty() {
        eprintln!("{trimmed}");
    }
    eprintln!();
}
