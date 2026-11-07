//! Inherited signal dispositions and signal exits while a request is in flight.

use super::*;

/// Runs `ox` with `args` until its request reaches a reply that never comes, then sends it
/// `signal`. Returns the signal that ended it, its stdout, and its stderr.
async fn interrupted_run(args: &[&str], signal: &str) -> (Option<i32>, String, String) {
    use std::os::unix::process::ExitStatusExt as _;
    use std::time::Duration;

    use tokio::io::AsyncWriteExt as _;

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
        .stderr(Stdio::piped())
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
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    (output.status.signal(), stdout, stderr)
}

#[tokio::test]
async fn preserves_ignored_signals_in_tool_subprocesses() {
    for signal in ["HUP", "INT", "TERM"] {
        let server = MockServer::start().await;
        let input = serde_json::json!({
            "command": format!("kill -{signal} $$; printf survived"),
        });
        mount_tool_call_once(&server, "toolu_1", "bash", &input.to_string()).await;
        mount_reply(&server, "Done.").await;
        let dir = tempfile::tempdir().unwrap();
        let configured = ox(dir.path());
        let mut command = Command::new("bash");
        command
            .args([
                "-c",
                "trap '' \"$1\"; shift; exec \"$@\"",
                "ignored-signal",
                signal,
            ])
            .arg(configured.get_program())
            .args(["-p", "run the command", "--mode", "yolo"])
            .current_dir(dir.path())
            .stdin(Stdio::null());
        for (name, value) in configured.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            } else {
                command.env_remove(name);
            }
        }

        let output = tokio::process::Command::from(command)
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("ANTHROPIC_BASE_URL", server.uri())
            .output()
            .await
            .unwrap();

        assert!(output.status.success(), "SIG{signal}: {output:?}");
        let requests = request_bodies(&server).await;
        let result = &requests[1]["messages"].as_array().unwrap().last().unwrap()["content"][0];
        assert_eq!(result["type"], "tool_result", "SIG{signal}: {result}");
        assert_eq!(result["tool_use_id"], "toolu_1", "SIG{signal}: {result}");
        assert_eq!(result["content"], "survived", "SIG{signal}: {result}");
        assert!(result.get("is_error").is_none(), "SIG{signal}: {result}");
    }
}

#[tokio::test]
async fn headless_dies_by_the_signal_after_a_shutdown_result() {
    for (signal, number) in [("HUP", 1), ("INT", 2), ("TERM", 15)] {
        let (signaled, stdout, _) =
            interrupted_run(&["-p", "hello", "--output", "json"], signal).await;

        assert_eq!(signaled, Some(number), "SIG{signal}");
        let result: serde_json::Value = serde_json::from_str(stdout.trim_end()).unwrap();
        assert_eq!(result["type"], "result", "{stdout}");
        assert_eq!(result["data"]["stop_reason"], "shutdown", "{stdout}");
    }
}
