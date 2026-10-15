//! CLI startup and session-list output.

use std::process::{Command, Stdio};

use indoc::indoc;

#[test]
fn list_empty_project() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ox"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_ox"))
        .arg("--list")
        .env("XDG_DATA_HOME", file.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("Error: "), "{stderr}");
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

    let output = Command::new(env!("CARGO_BIN_EXE_ox"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_ox"))
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
