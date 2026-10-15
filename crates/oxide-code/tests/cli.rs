//! CLI startup and session-list output.

use std::process::Command;

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
