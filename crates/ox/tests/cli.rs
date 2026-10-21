//! CLI startup, headless session persistence, signal exits, and session-list output.

#[path = "support/env.rs"]
mod env;

use std::path::Path;
use std::process::{Command, Stdio};

use indoc::indoc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use ox_core::engine::testing::{mount_reply, mount_tool_call_once, request_bodies};
use ox_core::util::tls::testing::TEST_CA_PATH;

fn ox(root: &Path) -> Command {
    let mut ox = Command::new(env!("CARGO_BIN_EXE_ox"));
    env::remove_inherited(&mut ox, |name| {
        env::is_provider_var(name)
            || env::is_ox_setting(name)
            || matches!(name, "RUST_BACKTRACE" | "RUST_LOG")
    });
    env::use_scratch_dirs(&mut ox, root);
    // On Linux, building the HTTP client reads the system trust store and fails when it holds no
    // certificates, so the tests pin it to a test CA.
    ox.env("SSL_CERT_FILE", TEST_CA_PATH);
    ox
}

#[tokio::test]
async fn headless_prints_a_startup_error_as_one_line_even_with_backtraces_on() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.pem");

    let output = tokio::process::Command::from(ox(dir.path()))
        .args(["-p", "hello"])
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("OX_EXTRA_CA_CERTS", &missing)
        .env("RUST_BACKTRACE", "1")
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    let stderr = String::from_utf8(output.stderr).unwrap();
    let headline = format!(
        "Error: failed to read extra CA bundle at {}: ",
        missing.display()
    );
    assert!(stderr.starts_with(&headline), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}

#[tokio::test]
async fn startup_error_escapes_control_characters_a_project_file_supplies() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".ox")).unwrap();
    std::fs::write(
        dir.path().join(".ox/config.toml"),
        "[client]\n\"\\u001b]0;pwned\\u0007\" = 1\n",
    )
    .unwrap();

    let output = tokio::process::Command::from(ox(dir.path()))
        .args(["-p", "hello"])
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(r"cannot set client.\u{1b}]0;pwned\u{7}."),
        "{stderr}"
    );
    assert!(!stderr.contains(['\u{1b}', '\u{7}']), "{stderr:?}");
}

#[tokio::test]
async fn headless_reports_startup_notices_and_retries_once_as_plain_text() {
    for format in ["text", "stream-json"] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(529).insert_header("retry-after", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_reply(&server, "Done.").await;
        let dir = tempfile::tempdir().unwrap();

        let output = tokio::process::Command::from(ox(dir.path()))
            .args(["-p", "hello", "--output", format])
            .current_dir(dir.path())
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("ANTHROPIC_BASE_URL", server.uri())
            .env("ANTHROPIC_MODEL", "claude-nonexistent-9")
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();

        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        let lines: Vec<&str> = stderr.lines().collect();
        assert_eq!(lines.len(), 2, "{stderr}");
        assert!(
            lines[0].starts_with("`claude-nonexistent-9` is not a supported model"),
            "{stderr}"
        );
        assert!(
            lines[1].contains("Retrying in 0 s (retry 1 of "),
            "{stderr}"
        );
        assert!(!stderr.contains('\x1b'), "{stderr:?}");
        if format == "stream-json" {
            let stdout = String::from_utf8(output.stdout).unwrap();
            let first: serde_json::Value =
                serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
            assert_eq!(
                first,
                serde_json::json!({"type": "notice", "data": lines[0]})
            );
        }
    }
}

#[test]
fn list_in_empty_project_reports_no_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let output = ox(dir.path())
        .arg("--list")
        .current_dir(dir.path())
        .env("XDG_DATA_HOME", dir.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"No sessions found in this project\n");
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

#[test]
fn list_with_unusable_storage_fails() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let output = ox(file.path())
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

    let output = ox(dir.path())
        .arg("--no-tui")
        .arg("--continue")
        .arg(&session)
        .current_dir(dir.path())
        .env("ANTHROPIC_AUTH_TOKEN", "test-token")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Unarchived session abcdef01\n"), "{stderr}");
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
fn continue_with_an_unmatched_prefix_fails_with_a_scope_hint() {
    let dir = tempfile::tempdir().unwrap();
    let output = ox(dir.path())
        .args(["--no-tui", "--continue", "deadbeef"])
        .current_dir(dir.path())
        .env("ANTHROPIC_AUTH_TOKEN", "test-token")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr.lines().next().unwrap(),
        "Error: no session matching `deadbeef` in this project \
         (use --all to search every project)",
    );
}

#[test]
fn resume_without_a_terminal_fails_before_writing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let output = ox(dir.path())
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
    let output = tokio::process::Command::from(ox(dir.path()))
        .args(["-p", "hello"])
        .current_dir(dir.path())
        .env("XDG_DATA_HOME", &data_dir)
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
    mount_tool_call_once(&server, "toolu_1", "bash", r#"{"command":"ls"}"#).await;
    mount_reply(&server, "Done.").await;
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config/ox");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[permission]\nmode = \"yolo\"\n",
    )
    .unwrap();

    let output = tokio::process::Command::from(ox(dir.path()))
        .args(["-p", "list the files", "--mode", "plan"])
        .current_dir(dir.path())
        .env("ANTHROPIC_AUTH_TOKEN", "test-token")
        .env("ANTHROPIC_BASE_URL", server.uri())
        .env_remove("OX_PERMISSION_MODE")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let requests = request_bodies(&server).await;
    assert_eq!(requests.len(), 2, "the denial goes back to the model");
    let result = &requests[1]["messages"].as_array().unwrap().last().unwrap()["content"][0];
    assert_eq!(result["type"], "tool_result", "{result}");
    assert!(
        result["content"]
            .as_str()
            .is_some_and(|content| content.starts_with("The bash call was denied by `plan` mode.")),
        "{result}"
    );
}

/// Runs `ox` with `args` until its request reaches a reply that never comes, then sends it
/// `signal`. Returns its exit status and stdout.
#[cfg(unix)]
async fn interrupted_run(args: &[&str], signal: &str) -> (Option<i32>, String) {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

    use ox_core::engine::testing::{mount_stalled_reply_once, until_requested};

    let server = MockServer::start().await;
    mount_stalled_reply_once(&server).await;
    let dir = tempfile::tempdir().expect("create a scratch directory");
    let mut child = tokio::process::Command::from(ox(dir.path()))
        .args(args)
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", server.uri())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start ox");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    stdin.write_all(b"hello\n").await.expect("write the prompt");
    drop(stdin);

    tokio::time::timeout(Duration::from_secs(60), until_requested(&server, 1))
        .await
        .expect("ox sends its request");
    let pid = child.id().expect("ox is still running").to_string();
    let killed = Command::new("kill")
        .args(["-s", signal, &pid])
        .status()
        .expect("run kill");
    assert!(killed.success());
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("ox exits after the signal")
        .expect("wait for ox");

    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    (output.status.code(), stdout)
}

#[cfg(unix)]
#[tokio::test]
async fn headless_exits_with_the_signal_status_after_a_shutdown_result() {
    for (signal, status) in [("HUP", 129), ("INT", 130), ("TERM", 143)] {
        let (code, stdout) = interrupted_run(&["-p", "hello", "--output", "json"], signal).await;

        assert_eq!(code, Some(status), "SIG{signal}");
        let result: serde_json::Value = serde_json::from_str(stdout.trim_end()).unwrap();
        assert_eq!(result["type"], "result", "{stdout}");
        assert_eq!(result["data"]["stop_reason"], "shutdown", "{stdout}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn repl_with_piped_input_exits_with_the_signal_status() {
    for (signal, status) in [("HUP", 129), ("INT", 130), ("TERM", 143)] {
        let (code, _) = interrupted_run(&["--no-tui"], signal).await;

        assert_eq!(code, Some(status), "SIG{signal}");
    }
}
