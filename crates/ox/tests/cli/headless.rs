//! Headless `-p` runs: output formats, piped stdin, notices, and unattended approvals.

use super::*;

#[tokio::test]
async fn reports_startup_notices_and_retries_once_as_plain_text() {
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
            .env("OX_MODEL", "claude-nonexistent-9")
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

#[tokio::test]
async fn rejected_request_preserves_branch_and_user_message() {
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
    let init = isolated_git(dir.path())
        .args(["init", "--initial-branch", "fixture-branch"])
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");

    let output = tokio::process::Command::from(ox(dir.path()))
        .args(["-p", "hello"])
        .current_dir(dir.path())
        .env("ANTHROPIC_AUTH_TOKEN", "test-token")
        .env("ANTHROPIC_BASE_URL", server.uri())
        .output()
        .await
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("fixture rejection"), "{stderr}");
    let sessions = saved_sessions(dir.path());
    let [session] = &sessions[..] else {
        panic!("one saved session: {sessions:?}");
    };
    assert_eq!(session.git_branch.as_deref(), Some("fixture-branch"));
    let messages: Vec<_> = exported(dir.path(), &session.session_id)
        .into_iter()
        .filter(|record| record["type"] == "message")
        .map(|message| (message["role"].clone(), message["content"].clone()))
        .collect();
    assert_eq!(
        messages,
        [(
            serde_json::json!("user"),
            serde_json::json!([{"type": "text", "text": "hello"}]),
        )],
    );
}

#[tokio::test]
async fn attaches_piped_stdin_and_continues_its_session() {
    let server = MockServer::start().await;
    mount_reply(&server, "Done.").await;
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let mut command = tokio::process::Command::from(ox(dir.path()));
        command
            .args(args)
            .args(["--output", "json"])
            .current_dir(dir.path())
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("ANTHROPIC_BASE_URL", server.uri())
            .stdout(Stdio::piped());
        command
    };
    let session_id = |stdout: &[u8]| {
        let result: serde_json::Value = serde_json::from_slice(stdout).unwrap();
        result["data"]["session_id"].as_str().unwrap().to_owned()
    };

    let mut first = run(&["-p", "Review this diff"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = first.stdin.take().unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut stdin, b"+fix\n")
        .await
        .unwrap();
    drop(stdin);
    let first = first.wait_with_output().await.unwrap();
    let next = run(&["-c", "-p", "Next step"]).output().await.unwrap();

    assert!(first.status.success(), "{first:?}");
    assert!(next.status.success(), "{next:?}");
    assert_eq!(session_id(&first.stdout), session_id(&next.stdout));
    let prompts: Vec<Vec<serde_json::Value>> = request_bodies(&server)
        .await
        .iter()
        .map(|request| {
            let messages = request["messages"].as_array().unwrap();
            messages
                .iter()
                .filter(|message| message["role"] == "user")
                .map(|message| {
                    message["content"].as_array().unwrap().last().unwrap()["text"].clone()
                })
                .filter(|text| !text.as_str().unwrap().starts_with("<system-reminder>"))
                .collect()
        })
        .collect();
    let attached = "Review this diff\n\n<stdin>\n+fix\n</stdin>";
    assert_eq!(prompts, [vec![attached], vec![attached, "Next step"]]);
}

#[tokio::test]
async fn continue_reports_files_changed_since_the_session_read_them() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes.txt");
    std::fs::write(&notes, "v1\n").unwrap();
    let input = serde_json::json!({ "file_path": notes }).to_string();
    mount_tool_call_once(&server, "toolu_1", "read", &input).await;
    mount_reply(&server, "Done.").await;
    let run = |prompt: &[&str]| {
        let mut command = tokio::process::Command::from(ox(dir.path()));
        command
            .args(prompt)
            .current_dir(dir.path())
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("ANTHROPIC_BASE_URL", server.uri());
        command
    };
    let first = run(&["-p", "Read the notes"]).output().await.unwrap();
    assert!(first.status.success(), "{first:?}");
    std::fs::write(&notes, "v2\n").unwrap();

    let next = run(&["-c", "-p", "Next step"]).output().await.unwrap();

    assert!(next.status.success(), "{next:?}");
    let stderr = String::from_utf8(next.stderr).unwrap();
    assert!(
        stderr.contains("1 tracked file changed on disk since the resumed session read it"),
        "{stderr}"
    );
}

#[tokio::test]
async fn without_a_prompt_or_with_a_list_flag_fails_with_a_usage_error() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    for (args, message) in [
        (
            &["-p"][..],
            "`--print` needs a prompt argument or text on stdin",
        ),
        (&["--archived", "-p", "hello"], "cannot be used with"),
    ] {
        let output = tokio::process::Command::from(ox(dir.path()))
            .args(args)
            .current_dir(dir.path())
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("ANTHROPIC_BASE_URL", server.uri())
            .output()
            .await
            .unwrap();

        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(message), "{stderr}");
    }
    assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
}

#[tokio::test]
async fn mode_flag_overrides_the_configured_mode() {
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
            .is_some_and(|content| content.starts_with("The bash call was denied by `plan` mode,")),
        "{result}"
    );
}

#[tokio::test]
async fn unattended_runs_deny_a_call_that_needs_approval_and_read_no_line_as_an_answer() {
    for (args, input, requests_sent) in [
        (&["-p", "push it"][..], None, 2),
        (&["--no-tui"], Some(&b"push it\ny\n"[..]), 3),
    ] {
        let server = MockServer::start().await;
        mount_tool_call_once(&server, "toolu_1", "bash", r#"{"command":"git push"}"#).await;
        mount_reply(&server, "Done.").await;
        let dir = tempfile::tempdir().unwrap();
        let mut command = tokio::process::Command::from(ox(dir.path()));
        command
            .args(args)
            .current_dir(dir.path())
            .env("ANTHROPIC_AUTH_TOKEN", "test-token")
            .env("ANTHROPIC_BASE_URL", server.uri())
            .env_remove("OX_PERMISSION_MODE")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().unwrap();
        if let Some(input) = input {
            let mut stdin = child.stdin.take().unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut stdin, input)
                .await
                .unwrap();
        }

        let output = child.wait_with_output().await.unwrap();

        assert!(output.status.success(), "{args:?}: {output:?}");
        let requests = request_bodies(&server).await;
        assert_eq!(
            requests.len(),
            requests_sent,
            "{args:?}: a `y` line is a prompt"
        );
        let result = &requests[1]["messages"].as_array().unwrap().last().unwrap()["content"][0];
        assert!(
            result["content"].as_str().is_some_and(|content| {
                content.starts_with(
                "The bash call was denied because it needs approval and this run cannot ask for it."
            )
            }),
            "{args:?}: {result}"
        );
    }
}
