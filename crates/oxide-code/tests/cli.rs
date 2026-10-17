//! CLI startup, headless session persistence, and session-list output.

use std::fmt::Write as _;
use std::process::{Command, Stdio};

use indoc::{indoc, writedoc};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ox() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ox"))
}

#[test]
fn list_in_empty_project_reports_no_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let output = ox()
        .arg("--list")
        .current_dir(dir.path())
        .env("XDG_DATA_HOME", dir.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"No sessions found in this project.\n");
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

#[test]
fn list_with_unusable_storage_fails() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let output = ox()
        .arg("--list")
        .env("XDG_DATA_HOME", file.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("Error: "), "{stderr}");
    assert!(
        stderr.contains(&file.path().display().to_string()),
        "{stderr}"
    );
}

#[test]
fn continue_in_the_repl_unarchives_the_session_with_a_note() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("session.jsonl");
    std::fs::write(
        &session,
        indoc! {r#"
            {"type":"header","session_id":"abcdef01-2222-3333-4444-555555555555","cwd":"/repo","model":"claude-opus-5-5","created_at":"2026-01-01T00:00:00Z","version":1}
            {"type":"message","uuid":"00000000-0000-4000-8000-000000000001","message":{"role":"user","content":[{"type":"text","text":"hello"}]},"timestamp":"2026-01-01T00:00:00Z"}
            {"type":"archive","archived":true,"at":"2026-01-02T00:00:00Z"}
        "#},
    )
    .unwrap();

    let output = ox()
        .arg("--no-tui")
        .arg("--continue")
        .arg(&session)
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("ANTHROPIC_AUTH_TOKEN", "test-token")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Unarchived session abcdef01."), "{stderr}");
    let transcript = std::fs::read_to_string(&session).unwrap();
    let archive_state = transcript
        .lines()
        .rfind(|line| line.contains(r#""type":"archive""#))
        .unwrap();
    assert!(
        archive_state.contains(r#""archived":false"#),
        "{transcript}"
    );
}

#[test]
fn resume_without_a_terminal_fails_before_writing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let output = ox()
        .arg("--resume")
        .current_dir(dir.path())
        .env("XDG_DATA_HOME", dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("--resume opens the session picker in the TUI"),
        "{stderr}"
    );
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn headless_rejected_request_preserves_branch_and_user_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "fixture rejection",
            },
        })))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let init = Command::new("git")
        .args(["init", "--initial-branch", "fixture-branch"])
        .current_dir(dir.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");

    let data_dir = dir.path().join("data");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ox"))
        .args(["-p", "hello"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", &data_dir)
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("ANTHROPIC_AUTH_TOKEN", "test-token")
        .env("ANTHROPIC_BASE_URL", server.uri())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("fixture rejection"), "{stderr}");

    let projects: Vec<_> = std::fs::read_dir(data_dir.join("ox/sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(projects.len(), 1, "session projects: {projects:?}");
    let sessions: Vec<_> = std::fs::read_dir(&projects[0])
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(sessions.len(), 1, "session files: {sessions:?}");
    let transcript = std::fs::read_to_string(&sessions[0]).unwrap();
    let entries: Vec<serde_json::Value> = transcript
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(entries[0]["type"], "header");
    assert_eq!(entries[0]["git_branch"], "fixture-branch");
    let user_messages: Vec<_> = entries
        .iter()
        .filter(|entry| entry["type"] == "message" && entry["message"]["role"] == "user")
        .map(|entry| entry["message"].clone())
        .collect();
    assert_eq!(
        user_messages,
        vec![serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        })],
    );
}

#[tokio::test]
async fn headless_mode_flag_overrides_the_configured_mode() {
    let server = MockServer::start().await;
    let start = serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-opus-5-5", "usage": {"input_tokens": 1, "output_tokens": 0}}});
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse(&[
            start.clone(),
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "bash"}}),
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": r#"{"command":"ls"}"#}}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 1}}),
            serde_json::json!({"type": "message_stop"}),
        ]))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse(&[
            start,
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "Done."}}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
            serde_json::json!({"type": "message_stop"}),
        ]))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config/ox");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[permission]\nmode = \"yolo\"\n",
    )
    .unwrap();

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ox"))
        .args(["-p", "list the files", "--mode", "plan"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("ANTHROPIC_AUTH_TOKEN", "test-token")
        .env("ANTHROPIC_BASE_URL", server.uri())
        .env_remove("OX_PERMISSION_MODE")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "the denial goes back to the model");
    let followup: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let result = &followup["messages"].as_array().unwrap().last().unwrap()["content"][0];
    assert_eq!(result["type"], "tool_result", "{result}");
    assert!(
        result["content"]
            .as_str()
            .is_some_and(|content| content.starts_with("The bash call was denied by `plan` mode.")),
        "{result}"
    );
}

/// A streamed Messages reply of `frames`, each under an event named for its `type`.
fn sse(frames: &[serde_json::Value]) -> ResponseTemplate {
    let mut body = String::new();
    for frame in frames {
        _ = writedoc!(
            body,
            "
            event: {event}
            data: {frame}

            ",
            event = frame["type"].as_str().expect("each frame has a type"),
        );
    }
    ResponseTemplate::new(200)
        .set_body_string(body)
        .insert_header("content-type", "text/event-stream")
}
