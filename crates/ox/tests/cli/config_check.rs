//! `ox config check`: what it validates, the files it reads, and what it keeps private.

use super::*;

#[test]
fn accepts_native_auth_without_reading_credential_files() {
    for kind in ["bearer", "api-key", "oauth"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let missing = root.join("missing-token");
        let missing = missing.display();
        write_user_config(
            root,
            &formatdoc! {"
                [client.auth]
                type = '{kind}'
                file = '{missing}'
            "},
        );
        let before = tree(root);

        let output = config_check(root).output().unwrap();
        assert!(output.status.success(), "{kind}: {output:?}");

        assert!(
            String::from_utf8_lossy(&output.stdout).ends_with("Config is valid\n"),
            "{output:?}"
        );
        assert!(output.stderr.is_empty(), "{output:?}");
        let after: Vec<_> = tree(root)
            .into_iter()
            .filter(|path| path != &root.join("work"))
            .collect();
        assert_eq!(after, before, "{kind}");
    }
}

#[test]
fn rejects_a_malformed_credential_without_echoing_it() {
    for (body, expected) in [
        (
            "client.auth = { type = 'bearer', value = 987654321 }",
            "in `client.auth`: expected a string",
        ),
        (
            indoc! {"
                client.auth = { type = 'bearer', value = 'private-token-sentinel' }
                [private-token-sentinel
            "},
            "unclosed table, expected `]`",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = write_user_config(dir.path(), body);

        let output = config_check(dir.path()).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{body}: {output:?}");

        let stderr = String::from_utf8_lossy(&output.stderr);
        let headline = format!("Error: invalid config at {}: ", config.display());
        assert!(stderr.starts_with(&headline), "{body}: {stderr}");
        assert!(stderr.contains(expected), "{body}: {stderr}");
        assert_private_auth_run(dir.path(), &output);
    }
}

#[test]
fn rejects_a_credential_env_var_no_header_can_carry() {
    for (var, expected) in [
        (
            "ANTHROPIC_API_KEY",
            "API key (ANTHROPIC_API_KEY) is not a valid HTTP header value",
        ),
        (
            "ANTHROPIC_AUTH_TOKEN",
            "Auth token (ANTHROPIC_AUTH_TOKEN) is not a valid HTTP header value",
        ),
        (
            "CLAUDE_CODE_OAUTH_TOKEN",
            "OAuth (CLAUDE_CODE_OAUTH_TOKEN) is not a valid HTTP header value",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();

        let output = config_check(dir.path())
            .env(var, "bad\u{7}key")
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(1), "{var}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{var}: {stderr}");
        assert!(!stderr.contains("bad"), "{var}: {stderr}");
    }
}

#[test]
fn keeps_native_connection_settings_user_only() {
    for (setting, value) in [
        (
            "auth",
            "{ type = 'bearer', value = 'private-token-sentinel' }",
        ),
        ("headers", "{ x-private = 'private-header-sentinel' }"),
        ("attribution", "false"),
        ("structured_outputs", "false"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let work = root.join("work");
        std::fs::create_dir_all(work.join(".git")).unwrap();
        std::fs::create_dir(work.join(".ox")).unwrap();
        std::fs::write(
            work.join(".ox/config.toml"),
            format!("client.{setting} = {value}\n"),
        )
        .unwrap();

        let output = config_check(root).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{setting}: {output:?}");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("cannot set client.{setting}.")),
            "{stderr}"
        );
        assert_private_auth_run(root, &output);
    }
}

#[test]
fn validates_native_auth_in_each_layer_before_replacement() {
    for body in [
        "client.auth = { type = 'bearer' }",
        "client.auth = { type = 'login', extra = 'private-token-sentinel' }",
        "client.headers = { content-type = 'private-header-sentinel' }",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let config = write_user_config(root, body);
        let drop_ins = root.join("config/ox/config.d");
        std::fs::create_dir(&drop_ins).unwrap();
        std::fs::write(
            drop_ins.join("50-profile.toml"),
            "client.auth = { type = 'login' }\nclient.headers = {}\n",
        )
        .unwrap();

        let output = config_check(root).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{body}: {output:?}");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&config.display().to_string()), "{stderr}");
        assert_private_auth_run(root, &output);
    }
}

#[test]
fn rejects_native_auth_header_conflicts_without_reading_files() {
    for (kind, header) in [
        ("bearer", "authorization"),
        ("api-key", "x-api-key"),
        ("oauth", "authorization"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let missing = root.join("missing-token");
        let missing = missing.display();
        write_user_config(
            root,
            &formatdoc! {"
                [client]
                headers = {{ {header} = 'private-header-sentinel' }}
                auth = {{ type = '{kind}', file = '{missing}' }}
            "},
        );

        let output = config_check(root).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{kind}: {output:?}");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("client.headers"), "{stderr}");
        assert!(stderr.contains(header), "{stderr}");
        assert!(stderr.contains("cannot be overridden"), "{stderr}");
        assert_private_auth_run(root, &output);
    }
}

#[test]
fn lists_each_file_read_in_load_order_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let config_dir = root.join("home").join(".config").join("ox");
    std::fs::create_dir_all(config_dir.join("config.d")).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[tui]\nshow_thinking = true\n",
    )
    .unwrap();
    std::fs::write(
        config_dir.join("config.d").join("50-a.toml"),
        "[client]\neffort = \"high\"\n",
    )
    .unwrap();
    let work = root.join("work");
    std::fs::create_dir_all(work.join(".git")).unwrap();
    std::fs::create_dir(work.join(".ox")).unwrap();
    std::fs::write(work.join(".ox/config.toml"), "[client]\neffort = \"low\"\n").unwrap();
    let before = tree(root);

    let output = config_check(root)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines[..2],
        [
            "Loaded ~/.config/ox/config.toml",
            "Loaded ~/.config/ox/config.d/50-a.toml",
        ],
        "{stdout}",
    );
    let project = lines[2].strip_prefix("Loaded ").expect("a third file");
    assert!(
        Path::new(project).ends_with("work/.ox/config.toml"),
        "{stdout}"
    );
    assert_eq!(lines[3..], ["Config is valid"], "{stdout}");
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    assert_eq!(tree(root), before);
}

#[test]
fn skips_the_credential_that_a_session_needs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let output = config_check(root)
        .env("ANTHROPIC_BASE_URL", "https://gw.example.com")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    assert_eq!(output.stdout, b"Config is valid\n");
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);

    let output = ox(root)
        .args(["-p", "hello"])
        .current_dir(root.join("work"))
        .env("ANTHROPIC_BASE_URL", "https://gw.example.com")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("no credentials for https://gw.example.com. Set ANTHROPIC_AUTH_TOKEN"),
        "{stderr}"
    );
}

#[test]
fn skips_the_ca_bundle_that_a_session_reads() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let missing = root.join("missing.pem");
    write_user_config(
        root,
        &format!("[client]\nextra_ca_certs = '{}'\n", missing.display()),
    );

    let output = config_check(root).output().unwrap();
    assert!(output.status.success(), "{output:?}");

    let output = ox(root)
        .args(["-p", "hello"])
        .current_dir(root.join("work"))
        .env("ANTHROPIC_API_KEY", "test-key")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");

    let stderr = String::from_utf8(output.stderr).unwrap();
    let headline = format!(
        "Error: invalid client.extra_ca_certs: failed to read extra CA bundle at {}: ",
        missing.display()
    );
    assert!(stderr.starts_with(&headline), "{stderr}");
}

#[test]
fn rejects_an_unknown_key_with_its_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let file = write_user_config(root, "[client]\nshow_thinking = true\n");

    let output = config_check(root).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");

    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.starts_with("Error:"), "{stderr}");
    assert!(stderr.contains(&file.display().to_string()), "{stderr}");
    assert!(stderr.contains("unknown field `show_thinking`"), "{stderr}");
}

#[test]
fn rejects_an_invalid_environment_variable() {
    let dir = tempfile::tempdir().unwrap();

    let output = config_check(dir.path())
        .env("OX_EFFORT", "extreme")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");

    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Error: invalid OX_EFFORT \"extreme\": expected one of low, medium, high, xhigh, max\n",
    );
}

#[test]
fn rejects_a_theme_that_does_not_exist() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_user_config(root, "[tui.theme]\nbase = \"nope\"\n");

    let output = config_check(root).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("Error: invalid [tui.theme] base \"nope\": neither a built-in theme ("),
        "{stderr}",
    );
}

#[test]
fn applies_the_project_layer_and_its_limits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let work = root.join("work");
    std::fs::create_dir_all(work.join(".git")).unwrap();
    std::fs::create_dir(work.join(".ox")).unwrap();
    std::fs::write(
        work.join(".ox/config.toml"),
        "[client]\nbase_url = \"https://gw.example.com\"\n",
    )
    .unwrap();

    let output = config_check(root).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("cannot set client.base_url."), "{stderr}");
}

#[test]
fn warns_about_an_unsupported_model_and_ignored_theme_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_user_config(
        root,
        indoc! {r##"
            [client]
            model = "claude-opus-4"

            [client.compaction]
            auto_threshold_tokens = 400000

            [tui.theme.overrides]
            accnet = "#ffffff"
        "##},
    );

    let output = config_check(root).output().unwrap();
    assert!(output.status.success(), "{output:?}");

    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Warning: `claude-opus-4` is not a supported model, \
         so requests go without thinking, effort, and context management, \
         and cost tracking stays off. \
         Auto-compaction requires an explicit token threshold. \
         Did you mean `claude-opus-5-5` (Claude Opus 5.5)?\n\
         Warning: Ignored the `accnet` theme override: unknown slot \"accnet\"\n",
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.ends_with("Config is valid\n"), "{stdout}");
}

#[test]
fn escapes_control_characters_a_project_file_supplies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let work = root.join("work");
    std::fs::create_dir_all(work.join(".ox")).unwrap();
    std::fs::write(
        work.join(".ox/config.toml"),
        "[client]\nmodel = \"x\\u001b]0;pwned\\u0007\"\n",
    )
    .unwrap();

    let output = config_check(root).output().unwrap();
    assert!(output.status.success(), "{output:?}");

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with(r"Warning: `x\u{1b}]0;pwned\u{7}` is not a supported model"),
        "{stderr:?}",
    );
    assert!(!stderr.contains(['\u{1b}', '\u{7}']), "{stderr:?}");
}
