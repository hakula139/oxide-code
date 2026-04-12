mod client;
mod config;
mod message;
mod prompt;
mod tool;
mod tui;

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use client::anthropic::{Client, ContentBlockInfo, Delta, StreamEvent};
use config::Config;
use message::{ContentBlock, Message, Role, strip_trailing_thinking};
use prompt::PromptParts;
use tool::{
    ToolDefinition, ToolMetadata, ToolOutput, ToolRegistry, bash::BashTool, edit::EditTool,
    glob::GlobTool, grep::GrepTool, read::ReadTool, write::WriteTool,
};
use tui::event::{AgentEvent, AgentSink, StdioSink, UserAction};

const MAX_TOOL_ROUNDS: usize = 25;

#[derive(Parser)]
#[command(name = "ox", version, about = "A terminal-based AI coding assistant")]
struct Cli {
    /// Disable the TUI and use a bare REPL instead.
    #[arg(long)]
    no_tui: bool,

    /// Run in headless mode: send a single prompt and print the response.
    #[arg(short, long, value_name = "PROMPT")]
    prompt: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::load().await?;
    let show_thinking = config.show_thinking;
    let model = config.model.clone();
    let client = Client::new(config)?;

    let tools = create_tool_registry();

    if let Some(prompt_text) = cli.prompt {
        return headless(&client, &tools, &model, show_thinking, &prompt_text).await;
    }

    if cli.no_tui || !std::io::stdout().is_terminal() {
        return bare_repl(&client, &tools, &model, show_thinking).await;
    }

    run_tui(&client, &model, tools).await
}

fn create_tool_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        Box::new(BashTool),
        Box::new(ReadTool),
        Box::new(WriteTool),
        Box::new(EditTool),
        Box::new(GlobTool),
        Box::new(GrepTool),
    ])
}

// ── TUI Mode ──

async fn run_tui(client: &Client, model: &str, tools: ToolRegistry) -> Result<()> {
    tui::terminal::install_panic_hook();

    let (agent_sink, agent_rx) = tui::event::channel();
    let (user_tx, user_rx) = mpsc::unbounded_channel::<UserAction>();

    let mut terminal = tui::terminal::init()?;
    let mut app = tui::app::App::new(model.to_owned(), agent_rx, user_tx);

    let agent_handle = {
        let client = client.clone();
        tokio::spawn(async move { agent_loop_task(client, tools, agent_sink, user_rx).await })
    };

    // Run the TUI on the main thread (it needs terminal access).
    let result = app.run(&mut terminal).await;

    tui::terminal::restore();

    // Cancel the agent loop — it may be blocked on an API stream.
    agent_handle.abort();
    match agent_handle.await {
        Ok(Err(e)) => warn!("agent loop error: {e}"),
        Err(e) if !e.is_cancelled() => warn!("agent task panicked: {e}"),
        _ => {}
    }

    result
}

async fn agent_loop_task(
    client: Client,
    tools: ToolRegistry,
    sink: tui::event::ChannelSink,
    mut user_rx: mpsc::UnboundedReceiver<UserAction>,
) -> Result<()> {
    let mut messages: Vec<Message> = Vec::new();

    while let Some(action) = user_rx.recv().await {
        match action {
            UserAction::SubmitPrompt(text) => {
                messages.push(Message::user(&text));
                let prompt = prompt::build_prompt(client.model()).await;
                if let Err(e) = agent_turn(&client, &tools, &mut messages, &prompt, &sink).await {
                    _ = sink.send(AgentEvent::Error(e.to_string()));
                }
                _ = sink.send(AgentEvent::TurnComplete);
            }
            UserAction::Quit => break,
        }
    }

    Ok(())
}

// ── Bare REPL Mode ──

async fn bare_repl(
    client: &Client,
    tools: &ToolRegistry,
    model: &str,
    show_thinking: bool,
) -> Result<()> {
    let sink = StdioSink::new(show_thinking);
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
        let prompt = prompt::build_prompt(model).await;
        agent_turn(client, tools, &mut messages, &prompt, &sink).await?;
        _ = sink.send(AgentEvent::TurnComplete);
    }

    Ok(())
}

// ── Headless Mode ──

async fn headless(
    client: &Client,
    tools: &ToolRegistry,
    model: &str,
    show_thinking: bool,
    prompt_text: &str,
) -> Result<()> {
    let sink = StdioSink::new(show_thinking);
    let mut messages = vec![Message::user(prompt_text)];
    let prompt = prompt::build_prompt(model).await;
    agent_turn(client, tools, &mut messages, &prompt, &sink).await?;
    println!();
    Ok(())
}

// ── Agent Turn (shared across all modes) ──

async fn agent_turn(
    client: &Client,
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    prompt: &PromptParts,
    sink: &dyn AgentSink,
) -> Result<()> {
    let tool_defs = tools.definitions();

    for _ in 0..MAX_TOOL_ROUNDS {
        strip_trailing_thinking(messages);
        let blocks = stream_response(client, messages, &tool_defs, prompt, sink).await?;

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
            _ = sink.send(AgentEvent::ToolCallStart {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });

            let output = match tools.get(&name) {
                Some(t) => t.run(input).await,
                None => ToolOutput {
                    content: format!("Unknown tool: {name}"),
                    is_error: true,
                    metadata: ToolMetadata::default(),
                },
            };

            _ = sink.send(AgentEvent::ToolCallEnd {
                id: id.clone(),
                title: output.metadata.title.clone(),
                content: output.content.clone(),
                is_error: output.is_error,
            });

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
    prompt: &PromptParts,
    sink: &dyn AgentSink,
) -> Result<Vec<ContentBlock>> {
    let mut rx = client.stream_message(
        messages,
        Some(&prompt.system),
        prompt.user_context.as_deref(),
        tools,
    )?;

    let mut blocks: Vec<Option<BlockAccumulator>> = Vec::new();

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
                let acc = init_accumulator(content_block, index);
                // Send initial text to display if non-empty (the API
                // typically sends empty initial text, but be safe).
                if let BlockAccumulator::Text(text) = &acc
                    && !text.is_empty()
                {
                    sink.send(AgentEvent::StreamToken(text.clone()))?;
                }
                blocks[index] = Some(acc);
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(Some(block)) = blocks.get_mut(index) {
                    apply_delta(block, delta, sink)?;
                }
            }
            StreamEvent::Error { error } => {
                bail!("API error ({}): {}", error.error_type, error.message);
            }
            _ => {}
        }
    }

    Ok(blocks
        .into_iter()
        .flatten()
        .filter_map(BlockAccumulator::into_content_block)
        .collect())
}

fn init_accumulator(content_block: ContentBlockInfo, index: usize) -> BlockAccumulator {
    match content_block {
        ContentBlockInfo::Text { text } => BlockAccumulator::Text(text),
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
    }
}

fn apply_delta(block: &mut BlockAccumulator, delta: Delta, sink: &dyn AgentSink) -> Result<()> {
    match (block, delta) {
        (BlockAccumulator::Text(buf), Delta::TextDelta { text }) => {
            buf.push_str(&text);
            sink.send(AgentEvent::StreamToken(text))?;
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
            sink.send(AgentEvent::ThinkingToken(thinking_delta))?;
        }
        (
            BlockAccumulator::Thinking { signature, .. },
            Delta::SignatureDelta {
                signature: sig_value,
            },
        ) => {
            *signature = sig_value;
        }
        (block, delta) => {
            debug!(?block, ?delta, "ignoring unhandled delta");
        }
    }
    Ok(())
}
