mod client;
mod config;
mod message;
mod tool;

use std::io::Write;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, warn};

use client::anthropic::{Client, ContentBlockInfo, Delta, StreamEvent};
use config::Config;
use message::{ContentBlock, Message, Role, strip_trailing_thinking};
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
        strip_trailing_thinking(messages);
        // The API rejects assistant messages with empty content (e.g., after
        // stripping an all-thinking response).
        messages.retain(|m| !(m.role == Role::Assistant && m.content.is_empty()));
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

#[derive(Debug)]
enum BlockAccumulator {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        json_buf: String,
    },
    ServerToolUse {
        id: String,
        name: String,
        json_buf: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    /// Placeholder for unrecognized content block types. Absorbs deltas silently
    /// and produces no [`ContentBlock`] at the end.
    Skipped,
}

impl BlockAccumulator {
    fn into_content_block(self) -> Option<ContentBlock> {
        match self {
            Self::Text(text) => Some(ContentBlock::Text { text }),
            Self::ToolUse { id, name, json_buf } => Some(ContentBlock::ToolUse {
                id,
                name,
                input: parse_tool_json(&json_buf),
            }),
            Self::ServerToolUse { id, name, json_buf } => Some(ContentBlock::ServerToolUse {
                id,
                name,
                input: parse_tool_json(&json_buf),
            }),
            Self::Thinking {
                thinking,
                signature,
            } => Some(ContentBlock::Thinking {
                thinking,
                signature,
            }),
            Self::RedactedThinking { data } => Some(ContentBlock::RedactedThinking { data }),
            Self::Skipped => None,
        }
    }
}

fn parse_tool_json(json_buf: &str) -> serde_json::Value {
    serde_json::from_str(json_buf).unwrap_or_else(|e| {
        warn!("malformed tool input JSON: {e}");
        serde_json::Value::Object(serde_json::Map::new())
    })
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
                blocks[index] = Some(init_accumulator(content_block, index, &mut stdout)?);
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(Some(block)) = blocks.get_mut(index) {
                    apply_delta(block, delta, &mut stdout)?;
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
        .any(|b| matches!(b, Some(BlockAccumulator::Text(s)) if !s.is_empty()));
    if has_text {
        writeln!(stdout)?;
    }

    Ok(blocks
        .into_iter()
        .flatten()
        .filter_map(BlockAccumulator::into_content_block)
        .collect())
}

fn init_accumulator(
    content_block: ContentBlockInfo,
    index: usize,
    stdout: &mut std::io::Stdout,
) -> Result<BlockAccumulator> {
    Ok(match content_block {
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
        ContentBlockInfo::ServerToolUse { id, name } => BlockAccumulator::ServerToolUse {
            id,
            name,
            json_buf: String::new(),
        },
        ContentBlockInfo::Thinking {
            thinking,
            signature,
        } => BlockAccumulator::Thinking {
            thinking,
            signature,
        },
        ContentBlockInfo::RedactedThinking { data } => BlockAccumulator::RedactedThinking { data },
        ContentBlockInfo::Unknown => {
            warn!("skipping unknown content block at index {index}");
            BlockAccumulator::Skipped
        }
    })
}

fn apply_delta(
    block: &mut BlockAccumulator,
    delta: Delta,
    stdout: &mut std::io::Stdout,
) -> Result<()> {
    match (block, delta) {
        (BlockAccumulator::Text(buf), Delta::TextDelta { text }) => {
            buf.push_str(&text);
            write!(stdout, "{text}")?;
            stdout.flush()?;
        }
        (
            BlockAccumulator::ToolUse { json_buf, .. }
            | BlockAccumulator::ServerToolUse { json_buf, .. },
            Delta::InputJsonDelta { partial_json },
        ) => {
            json_buf.push_str(&partial_json);
        }
        (
            BlockAccumulator::Thinking { thinking, .. },
            Delta::ThinkingDelta {
                thinking: thinking_delta,
            },
        ) => {
            thinking.push_str(&thinking_delta);
        }
        (
            BlockAccumulator::Thinking { signature, .. },
            Delta::SignatureDelta {
                signature: sig_value,
            },
        ) => {
            // Signature is a full value, not incremental.
            *signature = sig_value;
        }
        (block, delta) => {
            debug!(?block, ?delta, "ignoring unhandled delta");
        }
    }
    Ok(())
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
