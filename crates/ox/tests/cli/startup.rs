//! Errors `ox` reports before any frontend starts.

use super::*;

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
        "Error: invalid OX_EXTRA_CA_CERTS: failed to read extra CA bundle at {}: ",
        missing.display()
    );
    assert!(stderr.starts_with(&headline), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}

#[tokio::test]
async fn error_escapes_control_characters_a_project_file_supplies() {
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
        stderr.contains(r"unknown field `\u{1b}]0;pwned\u{7}`, expected"),
        "{stderr}"
    );
    assert!(!stderr.contains(['\u{1b}', '\u{7}']), "{stderr:?}");
}
