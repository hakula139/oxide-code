//! Listing sessions with `--list` and resuming them with `--resume`.

use super::*;

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
    assert_eq!(
        output.stdout,
        b"No sessions found in this project (use --all to search every project)\n",
    );
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
fn resume_in_the_repl_imports_an_archived_export_and_unarchives_it_with_a_note() {
    let dir = tempfile::tempdir().unwrap();
    let export = dir.path().join("session.jsonl");
    std::fs::write(
        &export,
        indoc! {r#"
            {"type":"session","id":"abcdef01-2222-3333-4444-555555555555","forked_from":null,"project_root":"/repo","cwd":"/repo","git_branch":null,"model":"claude-opus-5-5","format":1,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","title":null,"title_manual":false,"archived_at":"2026-01-02T00:00:00Z","head_id":"00000000-0000-4000-8000-000000000001","message_count":1,"file_snapshots":null}
            {"type":"message","id":"00000000-0000-4000-8000-000000000001","parent_id":null,"role":"user","content":[{"type":"text","text":"hello"}],"context":null,"compact":null,"tool_metadata":{},"created_at":"2026-01-01T00:00:00Z"}
        "#},
    )
    .unwrap();

    let output = ox(dir.path())
        .arg("--no-tui")
        .arg("--resume")
        .arg(&export)
        .current_dir(dir.path())
        .env("ANTHROPIC_AUTH_TOKEN", "test-token")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Unarchived session abcdef01\n"), "{stderr}");
    let session = &exported(dir.path(), "abcdef01")[0];
    assert_eq!(session["type"], "session");
    assert_eq!(session["archived_at"], serde_json::Value::Null);
}

#[test]
fn resume_of_a_file_that_is_not_an_export_fails_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("session.jsonl");
    std::fs::write(
        &legacy,
        indoc! {r#"
            {"type":"header","session_id":"abcdef01-2222-3333-4444-555555555555","cwd":"/repo","model":"claude-opus-5-5","created_at":"2026-01-01T00:00:00Z","version":1}
            {"type":"message","uuid":"00000000-0000-4000-8000-000000000001","message":{"role":"user","content":[{"type":"text","text":"hello"}]},"timestamp":"2026-01-01T00:00:00Z"}
        "#},
    )
    .unwrap();

    let output = ox(dir.path())
        .args(["-p", "hello", "--resume"])
        .arg(&legacy)
        .current_dir(dir.path())
        .env("ANTHROPIC_AUTH_TOKEN", "test-token")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "Error: failed to import {}: line 1: not an ox session export, which starts with a \
             session record\n",
            legacy.display(),
        ),
    );
}

#[tokio::test]
async fn headless_continues_an_exported_session_in_a_fresh_data_dir() {
    let server = MockServer::start().await;
    mount_reply(&server, "Done.").await;
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let run = |root: &Path, args: &[&str]| {
        let mut command = tokio::process::Command::from(ox(root));
        command
            .args(args)
            .args(["--output", "json"])
            .current_dir(root)
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("ANTHROPIC_BASE_URL", server.uri());
        command
    };
    let session_id = |stdout: &[u8]| {
        let result: serde_json::Value = serde_json::from_slice(stdout).unwrap();
        result["data"]["session_id"].as_str().unwrap().to_owned()
    };
    let first = run(source.path(), &["-p", "first"]).output().await.unwrap();
    assert!(first.status.success(), "{first:?}");
    let id = session_id(&first.stdout);
    let export = ox(source.path())
        .args(["export", &id[..8]])
        .current_dir(source.path())
        .output()
        .unwrap();
    assert!(export.status.success(), "{export:?}");
    let file = target.path().join("moved.jsonl");
    std::fs::write(&file, &export.stdout).unwrap();

    let next = run(
        target.path(),
        &["-p", "second", "-r", file.to_str().unwrap()],
    )
    .output()
    .await
    .unwrap();

    assert!(next.status.success(), "{next:?}");
    assert_eq!(session_id(&next.stdout), id);
    let requests = request_bodies(&server).await;
    let texts: Vec<_> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_array().unwrap().last().unwrap()["text"].clone())
        .filter(|text| !text.as_str().unwrap().starts_with("<system-reminder>"))
        .collect();
    assert_eq!(texts, ["first", "Done.", "second"]);
    let saved: Vec<_> = exported(target.path(), &id[..8])
        .iter()
        .filter(|record| record["type"] == "message")
        .map(|message| message["content"][0]["text"].clone())
        .collect();
    assert_eq!(saved, ["first", "Done.", "second", "Done."]);
}

#[test]
fn resume_with_an_unmatched_prefix_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = ox(dir.path())
        .args(["--no-tui", "--resume", "deadbeef"])
        .current_dir(dir.path())
        .env("ANTHROPIC_AUTH_TOKEN", "test-token")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr.lines().next().unwrap(),
        "Error: no session matching `deadbeef`",
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
        stderr.contains("`--resume` without a session opens the picker in the TUI"),
        "{stderr}"
    );
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}
