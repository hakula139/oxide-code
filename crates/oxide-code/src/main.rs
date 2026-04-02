mod client;
mod config;
mod message;

use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};

use client::anthropic::{Client, Delta, StreamEvent};
use config::Config;
use message::Message;

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

    repl(&client).await
}

async fn repl(client: &Client) -> Result<()> {
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

        let reply = send_and_print(client, &messages).await?;
        messages.push(Message::assistant(&reply));
    }

    Ok(())
}

async fn send_and_print(client: &Client, messages: &[Message]) -> Result<String> {
    let mut rx = client.stream_message(messages, None);
    let mut full_text = String::new();
    let mut stdout = std::io::stdout();

    while let Some(event) = rx.recv().await {
        let event = event.context("stream error")?;

        match event {
            StreamEvent::ContentBlockDelta {
                delta: Delta::TextDelta { text },
                ..
            } => {
                full_text.push_str(&text);
                write!(stdout, "{text}")?;
                stdout.flush()?;
            }
            StreamEvent::Error { error } => {
                anyhow::bail!("API error ({}): {}", error.error_type, error.message);
            }
            _ => {}
        }
    }

    // Newline after streamed response
    writeln!(stdout)?;

    Ok(full_text)
}
