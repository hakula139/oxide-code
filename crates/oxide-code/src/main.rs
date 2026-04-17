mod client;
mod config;
mod message;
mod prompt;
mod session;
mod tool;
mod tui;

use std::io::{IsTerminal, Write};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, warn};

use client::anthropic::{Client, ContentBlockInfo, Delta, StreamEvent};
use config::Config;
use message::{ContentBlock, Message, Role, strip_trailing_thinking};
use prompt::{PromptParts, environment::marketing_name};
use session::manager::SessionManager;
use session::store::SessionStore;
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

    /// Resume a session. Without a value, resumes the most recent session.
    /// With a session ID prefix, resumes that specific session.
    #[expect(
        clippy::option_option,
        reason = "clap uses Option<Option<T>> for optional flag values"
    )]
    #[arg(
        short = 'c',
        long = "continue",
        value_name = "SESSION_ID",
        conflicts_with = "prompt"
    )]
    resume: Option<Option<String>>,

    /// List recent sessions and exit.
    #[arg(long, conflicts_with_all = ["prompt", "resume"])]
    list: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Handle --list before loading config (no API access needed).
    if cli.list {
        return list_sessions();
    }

    let config = Config::load().await?;
    let show_thinking = config.show_thinking;
    let model = config.model.clone();

    // Resolve which session to resume (if any) before creating the client,
    // so we can pass the session ID to the API headers.
    let store = SessionStore::open()?;
    let (session, messages) = resolve_session(&store, &model, cli.resume.as_ref())?;
    let client = Client::new(config, Some(session.session_id().to_owned()))?;

    let tools = create_tool_registry();

    if let Some(prompt_text) = cli.prompt {
        return headless(
            &client,
            &tools,
            &model,
            show_thinking,
            &prompt_text,
            session,
        )
        .await;
    }

    if cli.no_tui || !std::io::stdout().is_terminal() {
        return bare_repl(&client, &tools, &model, show_thinking, session, messages).await;
    }

    run_tui(&client, &model, show_thinking, tools, session, messages).await
}

// ── Session Helpers ──

/// Log session I/O errors without aborting the agent loop.
fn log_session_err(result: anyhow::Result<()>) {
    if let Err(e) = result {
        warn!("session write failed: {e}");
    }
}

/// Print a table of recent sessions and exit.
fn list_sessions() -> Result<()> {
    let store = SessionStore::open()?;
    let sessions = store.list()?;

    if sessions.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }

    println!(
        "{:<10} {:<21} {:<20} {:<6} Title",
        "ID", "Created", "Model", "Msgs"
    );
    for s in &sessions {
        let id_prefix = &s.session_id[..s.session_id.len().min(8)];
        let created = s
            .created_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let created_short = &created[..created.len().min(16)];
        let model = marketing_name(&s.model).unwrap_or(&s.model);
        let msgs = s.message_count.map_or("-".to_owned(), |n| n.to_string());
        let title = s.title.as_deref().unwrap_or("(untitled)");
        println!("{id_prefix:<10} {created_short:<21} {model:<20} {msgs:<6} {title}");
    }

    Ok(())
}

/// Create or resume a session based on CLI flags.
///
/// - `None`: no `--continue` flag → new session.
/// - `Some(None)`: `--continue` without value → resume latest.
/// - `Some(Some(id))`: `--continue <id>` → resume specific session.
fn resolve_session(
    store: &SessionStore,
    model: &str,
    resume: Option<&Option<String>>,
) -> Result<(SessionManager, Vec<Message>)> {
    match resume {
        None => {
            let session = SessionManager::start(store, model)?;
            Ok((session, Vec::new()))
        }
        Some(None) => {
            let parent_id = store
                .latest_session_id()?
                .context("no sessions to resume")?;
            let (session, messages) = SessionManager::resume(store, &parent_id, model)?;
            debug!("resuming session {parent_id}");
            Ok((session, messages))
        }
        Some(Some(prefix)) if prefix.trim().is_empty() => {
            // Treat `--continue ""` the same as `--continue` (resume latest).
            let parent_id = store
                .latest_session_id()?
                .context("no sessions to resume")?;
            let (session, messages) = SessionManager::resume(store, &parent_id, model)?;
            debug!("resuming session {parent_id}");
            Ok((session, messages))
        }
        Some(Some(prefix)) => {
            let sessions = store.list()?;
            let matched: Vec<_> = sessions
                .iter()
                .filter(|s| s.session_id.starts_with(prefix.as_str()))
                .collect();
            match matched.len() {
                0 => bail!("no session matching prefix '{prefix}'"),
                1 => {
                    let parent_id = &matched[0].session_id;
                    let (session, messages) = SessionManager::resume(store, parent_id, model)?;
                    debug!("resuming session {parent_id}");
                    Ok((session, messages))
                }
                n => bail!("ambiguous prefix '{prefix}' matches {n} sessions"),
            }
        }
    }
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

async fn run_tui(
    client: &Client,
    model: &str,
    show_thinking: bool,
    tools: ToolRegistry,
    session: SessionManager,
    resumed_messages: Vec<Message>,
) -> Result<()> {
    tui::terminal::install_panic_hook();

    let (agent_sink, agent_rx) = tui::event::channel();
    let (user_tx, user_rx) = mpsc::unbounded_channel::<UserAction>();

    let cwd = std::env::current_dir()
        .ok()
        .map(|p| {
            dirs::home_dir()
                .and_then(|home| p.strip_prefix(&home).ok().map(ToOwned::to_owned))
                .map_or_else(
                    || p.display().to_string(),
                    |rel| format!("~/{}", rel.display()),
                )
        })
        .unwrap_or_default();

    let display_model = match marketing_name(model) {
        Some(name) => name.to_owned(),
        None => model.to_owned(),
    };

    let mut terminal = tui::terminal::init()?;
    let mut app = tui::app::App::new(display_model, show_thinking, cwd, agent_rx, user_tx);

    let session = Arc::new(Mutex::new(session));

    let agent_handle = {
        let client = client.clone();
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            agent_loop_task(
                client,
                tools,
                agent_sink,
                user_rx,
                session,
                resumed_messages,
            )
            .await
        })
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

    // Write the session summary after abort to guarantee it runs.
    log_session_err(session.lock().await.finish());

    result
}

async fn agent_loop_task(
    client: Client,
    tools: ToolRegistry,
    sink: tui::event::ChannelSink,
    mut user_rx: mpsc::UnboundedReceiver<UserAction>,
    session: Arc<Mutex<SessionManager>>,
    resumed_messages: Vec<Message>,
) -> Result<()> {
    let mut messages: Vec<Message> = resumed_messages;

    while let Some(action) = user_rx.recv().await {
        match action {
            UserAction::SubmitPrompt(text) => {
                let user_msg = Message::user(&text);
                log_session_err(session.lock().await.record_message(&user_msg));
                messages.push(user_msg);
                let prompt = prompt::build_prompt(client.model()).await;
                let turn_result = {
                    let mut s = session.lock().await;
                    agent_turn(&client, &tools, &mut messages, &prompt, &sink, &mut s).await
                };
                if let Err(e) = turn_result {
                    _ = sink.send(AgentEvent::Error(e.to_string()));
                }
                _ = sink.send(AgentEvent::TurnComplete);
            }
            UserAction::Quit => break,
        }
    }

    // Summary is written by run_tui after abort, not here.
    Ok(())
}

// ── Bare REPL Mode ──

async fn bare_repl(
    client: &Client,
    tools: &ToolRegistry,
    model: &str,
    show_thinking: bool,
    mut session: SessionManager,
    resumed_messages: Vec<Message>,
) -> Result<()> {
    let sink = StdioSink::new(show_thinking);
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut messages: Vec<Message> = resumed_messages;

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

        let user_msg = Message::user(&input);
        log_session_err(session.record_message(&user_msg));
        messages.push(user_msg);
        let prompt = prompt::build_prompt(model).await;
        agent_turn(client, tools, &mut messages, &prompt, &sink, &mut session).await?;
        _ = sink.send(AgentEvent::TurnComplete);
    }

    log_session_err(session.finish());
    Ok(())
}

// ── Headless Mode ──

async fn headless(
    client: &Client,
    tools: &ToolRegistry,
    model: &str,
    show_thinking: bool,
    prompt_text: &str,
    mut session: SessionManager,
) -> Result<()> {
    let sink = StdioSink::new(show_thinking);
    let user_msg = Message::user(prompt_text);
    log_session_err(session.record_message(&user_msg));
    let mut messages = vec![user_msg];
    let prompt = prompt::build_prompt(model).await;
    agent_turn(client, tools, &mut messages, &prompt, &sink, &mut session).await?;
    log_session_err(session.finish());
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
    session: &mut SessionManager,
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

        let assistant_msg = Message {
            role: Role::Assistant,
            content: blocks,
        };
        log_session_err(session.record_message(&assistant_msg));
        messages.push(assistant_msg);

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

        let tool_result_msg = Message {
            role: Role::User,
            content: results,
        };
        log_session_err(session.record_message(&tool_result_msg));
        messages.push(tool_result_msg);
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
    let section_refs: Vec<&str> = prompt.system_sections.iter().map(String::as_str).collect();
    let mut rx = client.stream_message(
        messages,
        &section_refs,
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
