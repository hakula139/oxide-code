//! Live end-to-end checks against the Anthropic API or a compatible gateway, skipped by default.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

const DEFAULT_MODEL: &str = "claude-haiku-5-5";
const DEFAULT_THINKING_MODEL: &str = "claude-opus-5-5";
const CREDENTIAL_VARS: [&str; 3] = [
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
];
/// Shorter `ANTHROPIC_CUSTOM_HEADERS` values, such as a `true` flag, would match ordinary text, so
/// the leak check skips them.
const MIN_SECRET_HEADER_LEN: usize = 8;

#[test]
#[ignore = "sends live API requests: run with OX_LIVE=1"]
fn headless_reply_streams_the_text_the_session_records() {
    let Some(live) = Live::start() else { return };

    let events = live.headless("Count from 1 to 30 in digits, separated by single spaces.");

    let chunks: Vec<&str> = events
        .iter()
        .filter(|event| event["type"] == "stream_token")
        .map(|event| event["data"].as_str().unwrap())
        .collect();
    assert!(
        chunks.len() > 1,
        "the reply arrived in one chunk: {chunks:?}"
    );
    let streamed = chunks.concat();
    assert!(streamed.contains("29 30"), "{streamed}");
    let session = live.session();
    let recorded: String = session
        .iter()
        .filter(|entry| entry["type"] == "message" && entry["message"]["role"] == "assistant")
        .flat_map(|entry| texts(&entry["message"]))
        .collect();
    assert_eq!(recorded, streamed);
}

#[test]
#[ignore = "sends live API requests: run with OX_LIVE=1"]
fn tool_round_trip_reads_the_first_request_from_the_prompt_cache() {
    let Some(live) = Live::start() else { return };
    let code = format!("falcon-{}", fastrand::u32(..));
    let notes = format!("The code word is {code}.\n");
    fs::write(live.cwd().join("notes.txt"), notes).unwrap();

    let events = live.headless(
        "Use the read tool to read notes.txt, then reply with only the code word it contains.",
    );

    let start = events
        .iter()
        .find(|event| event["type"] == "tool_call_start")
        .expect("the model called a tool");
    assert_eq!(start["data"]["name"], "read", "{start}");
    let end = events
        .iter()
        .find(|event| event["type"] == "tool_call_end")
        .expect("the tool call finished");
    assert_eq!(end["data"]["is_error"], false, "{end}");
    let result = &events.last().unwrap()["data"];
    let reply = result["message"].as_str().unwrap();
    assert!(reply.contains(&code), "{result}");
    let session = live.session();
    let usage = usage_records(&session);
    assert_eq!(usage.len(), 2, "{usage:?}");
    assert_reads_cached_prefix(usage[0], usage[1]);
}

#[test]
#[ignore = "sends live API requests: run with OX_LIVE=1"]
fn thinking_after_tool_results_is_replayed_across_three_turns() {
    let Some(live) = Live::start() else { return };
    fs::write(live.cwd().join("data.txt"), "91\n97\n221\n43\n87\n").unwrap();
    fs::write(live.cwd().join("more.txt"), "27\n37\n113\n49\n").unwrap();

    let mut repl = live.repl(&env_or("OX_LIVE_THINKING_MODEL", DEFAULT_THINKING_MODEL));
    repl.env("OX_EFFORT", "high");
    let output = live.send(
        repl,
        &[
            "Read data.txt. Which of its numbers are prime? Check each one carefully.",
            "Read more.txt. Across both files, which two numbers multiply closest to 4000?",
            "Use the grep tool to find lines containing 7 in both files. Give their sum modulo 13.",
        ],
    );

    assert!(output.status.success(), "{output:?}");
    let session = live.session();
    let turns = split_turns(&session);
    assert_eq!(turns.len(), 3, "{session:?}");
    for (number, turn) in (1..).zip(&turns) {
        let thought_after_tool_result = turn
            .iter()
            .skip_while(|message| !has_block(message, "tool_result"))
            .filter(|message| message["role"] == "assistant")
            .any(|message| has_signed_thinking(message));
        assert!(
            thought_after_tool_result,
            "turn {number} has no signed thinking after a tool result: {turn:?}"
        );
    }
    for pair in usage_records(&session).windows(2) {
        assert_reads_cached_prefix(pair[0], pair[1]);
    }
}

#[test]
#[ignore = "sends live API requests: run with OX_LIVE=1"]
fn auto_compaction_summarizes_from_the_cached_conversation() {
    let Some(live) = Live::start() else { return };
    fs::write(live.cwd().join("log-a.txt"), log_text(1, "copper-otter")).unwrap();
    fs::write(live.cwd().join("log-b.txt"), log_text(2, "silver-heron")).unwrap();

    let mut repl = live.repl(&env_or("OX_LIVE_MODEL", DEFAULT_MODEL));
    repl.env("OX_COMPACTION_AUTO_THRESHOLD_TOKENS", "50000");
    let output = live.send(
        repl,
        &[
            "Read log-a.txt and log-b.txt in full, then reply with only their final markers.",
            "Which two final markers did you report?",
        ],
    );

    assert!(output.status.success(), "{output:?}");
    let session = live.session();
    let compact = session
        .iter()
        .position(|entry| entry["type"] == "compact")
        .expect("the second prompt triggered auto-compaction");
    let before = usage_records(&session[..compact]);
    let after = usage_records(&session[compact..]);
    let (Some(last), [summary, next, ..]) = (before.last(), after.as_slice()) else {
        panic!("expected requests before and after compaction: {before:?} {after:?}");
    };
    assert_reads_cached_prefix(last, summary);
    assert!(
        prompt_tokens(next) < prompt_tokens(last) / 2,
        "compaction left {} of {} prompt tokens",
        prompt_tokens(next),
        prompt_tokens(last),
    );
}

#[test]
#[ignore = "sends live API requests: run with OX_LIVE=1"]
fn continue_answers_from_the_resumed_transcript_and_cache() {
    let Some(live) = Live::start() else { return };
    let code = format!("heron-{}", fastrand::u32(..));

    let first = live.headless(&format!(
        "Remember the code word {code}. Reply with only the word OK."
    ));
    let session_id = first.last().unwrap()["data"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut repl = live.repl(&env_or("OX_LIVE_MODEL", DEFAULT_MODEL));
    repl.args(["--continue", &session_id]);
    let output = live.send(
        repl,
        &["What code word did I ask you to remember? Reply with only the word."],
    );

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&code), "{stdout}");
    let session = live.session();
    let usage = usage_records(&session);
    assert_eq!(usage.len(), 2, "{usage:?}");
    assert_reads_cached_prefix(usage[0], usage[1]);
}

// ── Harness ──

/// A scratch working directory with its own home, config, session, and log storage, so `ox` loads
/// no global instructions or stored login, and the endpoint and credentials come only from the
/// caller's environment.
struct Live {
    root: TempDir,
    credentials: Vec<(&'static str, String)>,
}

impl Live {
    /// `None` unless `OX_LIVE=1`, so `--include-ignored` alone sends no requests.
    fn start() -> Option<Self> {
        if std::env::var_os("OX_LIVE").is_none_or(|value| value != "1") {
            eprintln!("skipped: set OX_LIVE=1 to send live API requests");
            return None;
        }
        let root = tempfile::tempdir().expect("create a scratch directory");
        for dir in ["cwd", "home"] {
            fs::create_dir(root.path().join(dir)).expect("create a scratch subdirectory");
        }
        Some(Self {
            root,
            credentials: credentials(),
        })
    }

    fn cwd(&self) -> PathBuf {
        self.root.path().join("cwd")
    }

    /// Runs `prompt` headless and returns its `stream-json` events, which end with the result.
    fn headless(&self, prompt: &str) -> Vec<Value> {
        let mut command = self.ox(&env_or("OX_LIVE_MODEL", DEFAULT_MODEL));
        command.args(["-p", prompt, "--output", "stream-json"]);
        let output = self.send(command, &[]);
        assert!(output.status.success(), "{output:?}");
        let events = json_lines(&String::from_utf8_lossy(&output.stdout));
        let result = events.last().expect("a result line");
        assert_eq!(result["type"], "result", "{result}");
        assert_eq!(result["data"]["stop_reason"], "completed", "{result}");
        events
    }

    fn repl(&self, model: &str) -> Command {
        let mut command = self.ox(model);
        command.arg("--no-tui");
        command
    }

    fn ox(&self, model: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ox"));
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(is_behavior_var) {
                command.env_remove(name);
            }
        }
        command
            .current_dir(self.cwd())
            // Without `USER`, ox skips the Claude Code login in the macOS Keychain.
            .env_remove("USER")
            .env("HOME", self.root.path().join("home"))
            .env("ANTHROPIC_MODEL", model)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_STATE_HOME", self.root.path().join("state"));
        command
    }

    /// Pipes each prompt to `command` as one input line, closes its input, and waits for it to
    /// exit. Fails the test before returning when a credential reached the captured output or any
    /// file under the scratch directory, so later assertions never format one.
    fn send(&self, mut command: Command, prompts: &[&str]) -> Output {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start ox");
        let mut stdin = child.stdin.take().expect("piped stdin");
        for prompt in prompts {
            writeln!(stdin, "{prompt}").expect("write a prompt");
        }
        drop(stdin);
        let output = child.wait_with_output().expect("wait for ox");
        self.assert_no_credentials(&output);
        output
    }

    /// Names the source and the variable of a leaked credential without printing its value.
    fn assert_no_credentials(&self, output: &Output) {
        let captured = [("stdout", &output.stdout), ("stderr", &output.stderr)]
            .map(|(source, bytes)| (source.to_owned(), bytes.clone()));
        let files = files_under(self.root.path()).into_iter().map(|file| {
            let bytes = fs::read(&file).expect("read a file the runs wrote");
            (file.display().to_string(), bytes)
        });
        for (source, bytes) in captured.into_iter().chain(files) {
            let text = String::from_utf8_lossy(&bytes);
            for (name, value) in &self.credentials {
                assert!(!text.contains(value.as_str()), "{source} contains ${name}");
            }
        }
    }

    /// Entries of the one session file the runs wrote.
    fn session(&self) -> Vec<Value> {
        let files: Vec<PathBuf> = children(&self.root.path().join("data/ox/sessions"))
            .iter()
            .flat_map(|project| children(project))
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        let [file] = files.as_slice() else {
            panic!("expected one session file: {files:?}");
        };
        json_lines(&fs::read_to_string(file).expect("read the session file"))
    }
}

/// The credential values in the environment, each with the variable that holds it.
fn credentials() -> Vec<(&'static str, String)> {
    let tokens = CREDENTIAL_VARS
        .into_iter()
        .filter_map(|name| Some((name, std::env::var(name).ok()?)))
        .filter(|(_, value)| !value.is_empty());
    let headers = std::env::var("ANTHROPIC_CUSTOM_HEADERS").unwrap_or_default();
    let header_values = headers
        .lines()
        .filter_map(|line| Some(line.split_once(':')?.1.trim()))
        .filter(|value| value.len() >= MIN_SECRET_HEADER_LEN)
        .map(|value| ("ANTHROPIC_CUSTOM_HEADERS", value.to_owned()));
    tokens.chain(header_values).collect()
}

/// Returns whether an inherited variable changes what the tests observe. The endpoint, credential,
/// header, CA, and attribution variables pass through.
fn is_behavior_var(name: &str) -> bool {
    name.starts_with("OX_") && name != "OX_EXTRA_CA_CERTS"
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    children(dir)
        .into_iter()
        .flat_map(|path| {
            if path.is_dir() {
                files_under(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

fn children(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect()
        })
        .unwrap_or_else(|error| panic!("list {}: {error}", dir.display()))
}

fn json_lines(text: &str) -> Vec<Value> {
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect()
}

fn texts(message: &Value) -> Vec<&str> {
    blocks(message)
        .filter_map(|block| block["text"].as_str())
        .collect()
}

fn has_block(message: &Value, kind: &str) -> bool {
    blocks(message).any(|block| block["type"] == kind)
}

fn has_signed_thinking(message: &Value) -> bool {
    blocks(message).any(|block| {
        block["type"] == "thinking"
            && block["signature"]
                .as_str()
                .is_some_and(|signature| !signature.is_empty())
    })
}

fn blocks(message: &Value) -> impl Iterator<Item = &Value> {
    message["content"].as_array().into_iter().flatten()
}

/// Groups the transcript's messages into turns, each starting at a user message with text.
fn split_turns(session: &[Value]) -> Vec<Vec<&Value>> {
    let mut turns: Vec<Vec<&Value>> = Vec::new();
    for entry in session.iter().filter(|entry| entry["type"] == "message") {
        let message = &entry["message"];
        if message["role"] == "user" && has_block(message, "text") {
            turns.push(Vec::new());
        }
        if let Some(turn) = turns.last_mut() {
            turn.push(message);
        }
    }
    turns
}

fn usage_records(entries: &[Value]) -> Vec<&Value> {
    entries
        .iter()
        .filter(|entry| entry["type"] == "usage")
        .collect()
}

/// Asserts that `later` read from the cache everything `earlier` wrote to or read from it, so the
/// prefix `later` replayed matched `earlier` byte for byte.
fn assert_reads_cached_prefix(earlier: &Value, later: &Value) {
    let cached =
        tokens(earlier, "cache_creation_input_tokens") + tokens(earlier, "cache_read_input_tokens");
    let read = tokens(later, "cache_read_input_tokens");
    assert!(
        read >= cached,
        "read {read} cached tokens after an earlier request cached {cached}: {earlier} → {later}"
    );
}

/// Input, cache-write, and cache-read tokens together: the request's whole prompt.
fn prompt_tokens(usage: &Value) -> u64 {
    tokens(usage, "input_tokens")
        + tokens(usage, "cache_creation_input_tokens")
        + tokens(usage, "cache_read_input_tokens")
}

fn tokens(usage: &Value, field: &str) -> u64 {
    let Some(count) = usage[field].as_u64() else {
        panic!("usage record without {field}: {usage}");
    };
    count
}

/// About 60 KB of word lines ending in `marker`, roughly 27K tokens.
fn log_text(seed: u64, marker: &str) -> String {
    const WORDS: [&str; 26] = [
        "amber", "basil", "cedar", "delta", "ember", "fable", "garnet", "harbor", "indigo",
        "juniper", "kestrel", "lantern", "meadow", "nectar", "orchid", "pebble", "quartz", "raven",
        "saffron", "thistle", "umber", "velvet", "willow", "xenon", "yarrow", "zephyr",
    ];
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut text = String::new();
    for number in 1..=680 {
        let words: Vec<&str> = (0..11).map(|_| WORDS[rng.usize(..WORDS.len())]).collect();
        writeln!(text, "entry {number:04}: {}", words.join(" "))
            .expect("String writes are infallible");
    }
    writeln!(text, "final marker: {marker}").expect("String writes are infallible");
    text
}
