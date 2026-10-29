//! The `--no-tui` REPL.

use super::*;

#[tokio::test]
async fn exits_with_failure_when_its_prompt_cannot_be_written() {
    for input in ["", "/exit\n"] {
        let dir = tempfile::tempdir().unwrap();
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let (stdin, mut input_writer) = std::io::pipe().unwrap();
        std::io::Write::write_all(&mut input_writer, input.as_bytes()).unwrap();
        drop(input_writer);

        let output = tokio::process::Command::from(ox(dir.path()))
            .arg("--no-tui")
            .current_dir(dir.path())
            .env("ANTHROPIC_API_KEY", "test-key")
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(writer)
            .spawn()
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();

        assert_eq!(output.status.code(), Some(1), "{input:?}: {output:?}");
        assert_eq!(output.stdout, b"", "{input:?}");
    }
}
