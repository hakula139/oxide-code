//! CLI startup and argument handling, `ox config check`, credential discovery, the REPL and
//! headless drivers, session persistence, signal exits, and session-list output.

#[path = "support/env.rs"]
mod env;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use indoc::{formatdoc, indoc};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use ox_core::engine::testing::{mount_reply, mount_tool_call_once, request_bodies};
use ox_core::util::git::testing::{isolate_git, isolated_git};
use ox_core::util::tls::testing::TEST_CA_PATH;

fn ox(root: &Path) -> Command {
    let mut ox = Command::new(env!("CARGO_BIN_EXE_ox"));
    env::remove_inherited(&mut ox, |name| {
        env::is_provider_var(name)
            || env::is_ox_setting(name)
            || matches!(name, "RUST_BACKTRACE" | "RUST_LOG")
    });
    isolate_git(&mut ox);
    env::use_scratch_dirs(&mut ox, root);
    // On Linux, building the HTTP client reads the system trust store and fails when it holds no
    // certificates, so the tests pin it to a test CA.
    ox.env("SSL_CERT_FILE", TEST_CA_PATH);
    // Tokio's `output` inherits an unset stdin where std's closes it, and `--print` reads a
    // non-terminal stdin to its end, so a parent stdin that never closes would hang the run.
    ox.stdin(Stdio::null());
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
        "Error: invalid OX_EXTRA_CA_CERTS: failed to read extra CA bundle at {}: ",
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
        stderr.contains(r"unknown field `\u{1b}]0;pwned\u{7}`, expected"),
        "{stderr}"
    );
    assert!(!stderr.contains(['\u{1b}', '\u{7}']), "{stderr:?}");
}

/// Builds an `ox config check` run from `work/` under `root`, created empty when missing.
fn config_check(root: &Path) -> Command {
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("the scratch work directory can be created");
    let mut command = ox(root);
    command.args(["config", "check"]).current_dir(work);
    command
}

/// Writes `body` to the user config file under `root` and returns the file's path.
fn write_user_config(root: &Path, body: &str) -> PathBuf {
    let dir = root.join("config").join("ox");
    std::fs::create_dir_all(&dir).expect("the scratch config directory can be created");
    let file = dir.join("config.toml");
    std::fs::write(&file, body).expect("the scratch config file can be written");
    file
}

/// Every path under `root`, sorted.
fn tree(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("the scratch directory can be listed") {
            let path = entry.expect("the scratch entry can be read").path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

fn assert_private_auth_run(root: &Path, output: &Output) {
    let secrets = [
        "private-token-sentinel",
        "private-header-sentinel",
        "987654321",
    ];
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        for secret in secrets {
            assert!(!text.contains(secret), "{text}");
        }
    }
    for dir in [root.join("data"), root.join("state")] {
        if !dir.exists() {
            continue;
        }
        for path in tree(&dir).into_iter().filter(|path| path.is_file()) {
            let bytes = std::fs::read(&path).expect("the run's artifact can be read");
            let text = String::from_utf8_lossy(&bytes);
            for secret in secrets {
                assert!(!text.contains(secret), "{}: {text}", path.display());
            }
        }
    }
}

fn request_header<'a>(request: &'a wiremock::Request, name: &str) -> Option<&'a str> {
    request
        .headers
        .get(name)
        .map(|value| value.to_str().expect("the request header is text"))
}

#[test]
fn config_check_accepts_native_auth_without_reading_credential_files() {
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

/// A config body and the message that `ox config check` must report for it.
const INVALID_AUTH_CONFIGS: [(&str, &str); 19] = [
    (
        "client.auth = 'private-token-sentinel'",
        "expected a table with type bearer, api-key, oauth, or login",
    ),
    (
        "client.auth = { type = 'private-token-sentinel', value = 'private-token-sentinel' }",
        "in `client.auth.type`: expected one of `bearer`, `api-key`, `oauth`, `login`",
    ),
    (
        "client.auth = { type = 'bearer', value = ['private-token-sentinel'] }",
        "in `client.auth`: expected a string",
    ),
    (
        "client.auth = { type = 'bearer', value = 987654321 }",
        "in `client.auth`: expected a string",
    ),
    (
        "client.auth = { type = 'bearer', file = ['private-token-sentinel'] }",
        "in `client.auth`: expected a string",
    ),
    (
        "client.auth = { type = 'bearer' }",
        "client.auth requires exactly one of value or file",
    ),
    (
        "client.auth = { type = 'bearer', value = '' }",
        "client.auth.value must not be empty",
    ),
    (
        "client.auth = { type = 'bearer', value = 'private-token-sentinel', file = '/token' }",
        "client.auth requires exactly one of value or file",
    ),
    (
        "client.auth = { type = 'login', value = 'private-token-sentinel' }",
        "client.auth type login takes no other keys",
    ),
    (
        "client.auth = { type = 'login', file = '/token' }",
        "client.auth type login takes no other keys",
    ),
    (
        "client.auth = { type = 'bearer', value = 'private-token-sentinel', extra = true }",
        "unknown field `extra`, expected `value` or `file`",
    ),
    (
        "client.api_key = 'private-token-sentinel'",
        "unknown field `api_key`",
    ),
    (
        "client.headers = 'private-header-sentinel'",
        "in `client.headers`: expected a map",
    ),
    (
        "client.headers = { x-private = ['private-header-sentinel'] }",
        "in `client.headers.x-private`: expected a string",
    ),
    (
        "client.headers = { 'private-header-sentinel name' = 'private-header-sentinel' }",
        "invalid client.headers: invalid header name",
    ),
    (
        r#"client.headers = { x-private = "private-header-sentinel\nsecond-line" }"#,
        "invalid client.headers: the x-private value contains a character a header cannot carry",
    ),
    (
        "client.headers = { X-Private = 'private-header-sentinel', x-private = 'private-header-sentinel' }",
        "invalid client.headers: x-private is set more than once",
    ),
    (
        r#"client.auth = { type = "bearer", value = "bad\u0007key" }"#,
        "invalid client.auth.value: expected an HTTP header value",
    ),
    (
        indoc! {"
                client.auth = { type = 'bearer', value = 'private-token-sentinel' }
                [private-token-sentinel
            "},
        "unclosed table, expected `]`",
    ),
];

#[test]
fn config_check_rejects_native_auth_shapes_without_echoing_private_values() {
    for (body, expected) in INVALID_AUTH_CONFIGS {
        let dir = tempfile::tempdir().unwrap();
        let config = write_user_config(dir.path(), body);

        let output = config_check(dir.path()).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{body}: {output:?}");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&config.display().to_string()),
            "{body}: {stderr}"
        );
        assert!(stderr.contains("invalid config"), "{body}: {stderr}");
        assert!(stderr.contains(expected), "{body}: {stderr}");
        assert_private_auth_run(dir.path(), &output);
    }
}

#[test]
fn config_check_rejects_a_credential_env_var_no_header_can_carry() {
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
fn config_check_rejects_relative_native_credential_paths() {
    for file in ["token", "./token", "../token", "~other/token", ""] {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            &formatdoc! {"
                [client.auth]
                type = 'bearer'
                file = '{file}'
            "},
        );

        let output = config_check(dir.path()).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{file:?}: {output:?}");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("client.auth"), "{stderr}");
        assert!(
            stderr.contains("absolute") && stderr.contains("~/"),
            "{stderr}"
        );
    }
}

#[test]
fn config_check_keeps_native_connection_settings_user_only() {
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
fn config_check_validates_native_auth_in_each_layer_before_replacement() {
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
fn config_check_rejects_native_auth_header_conflicts_without_reading_files() {
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

#[tokio::test]
async fn native_auth_sends_each_inline_and_file_credential_with_its_request_shape() {
    for kind in ["bearer", "api-key", "oauth"] {
        for source in ["value", "file"] {
            let server = MockServer::start().await;
            mount_reply(&server, "Done.").await;
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let token = root.join("token");
            std::fs::write(&token, "private-token-sentinel\r\n").unwrap();
            let value = if source == "file" {
                token.display().to_string()
            } else {
                "private-token-sentinel".to_owned()
            };
            let url = server.uri();
            write_user_config(
                root,
                &formatdoc! {"
                    [client]
                    base_url = '{url}'
                    headers = {{ x-private = 'private-header-sentinel' }}

                    [client.auth]
                    type = '{kind}'
                    {source} = '{value}'
                "},
            );

            let output = tokio::process::Command::from(ox(root))
                .args(["-p", "hello"])
                .current_dir(root)
                .env("RUST_LOG", "trace")
                .output()
                .await
                .unwrap();
            assert!(output.status.success(), "{kind}/{source}: {output:?}");

            let requests = server.received_requests().await.unwrap();
            let request = requests.first().expect("a Messages request was sent");
            assert_eq!(request.url.path(), "/v1/messages");
            assert_eq!(
                request_header(request, "x-api-key"),
                (kind == "api-key").then_some("private-token-sentinel"),
                "{kind}/{source}"
            );
            assert_eq!(
                request_header(request, "authorization"),
                (kind != "api-key").then_some("Bearer private-token-sentinel"),
                "{kind}/{source}"
            );
            assert_eq!(
                request_header(request, "anthropic-beta")
                    .unwrap()
                    .split(',')
                    .any(|beta| beta == "oauth-2025-04-20"),
                kind == "oauth",
                "{kind}/{source}"
            );
            assert_eq!(
                request_header(request, "x-private"),
                Some("private-header-sentinel")
            );
            assert!(
                tree(&root.join("data"))
                    .iter()
                    .any(|path| path.extension().is_some_and(|ext| ext == "jsonl")),
                "the run persisted its session"
            );
            assert_private_auth_run(root, &output);
        }
    }
}

#[tokio::test]
async fn native_auth_preserves_environment_credential_precedence_without_opening_losing_files() {
    for (bearer, api_key, file, expected_header, expected_value) in [
        (
            Some("env-bearer"),
            Some("env-key"),
            true,
            "authorization",
            "Bearer env-bearer",
        ),
        (None, Some("env-key"), true, "x-api-key", "env-key"),
        (
            Some(""),
            Some(""),
            false,
            "authorization",
            "Bearer private-token-sentinel",
        ),
    ] {
        let server = MockServer::start().await;
        mount_reply(&server, "Done.").await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let auth = if file {
            format!("file = '{}'", root.join("missing-token").display())
        } else {
            "value = 'private-token-sentinel'".to_owned()
        };
        write_user_config(
            root,
            &formatdoc! {"
                [client.auth]
                type = 'bearer'
                {auth}
            "},
        );

        let output = tokio::process::Command::from(ox(root))
            .args(["-p", "hello"])
            .current_dir(root)
            .env("ANTHROPIC_BASE_URL", server.uri())
            .envs(bearer.map(|value| ("ANTHROPIC_AUTH_TOKEN", value)))
            .envs(api_key.map(|value| ("ANTHROPIC_API_KEY", value)))
            .env("CLAUDE_CODE_OAUTH_TOKEN", "lower-oauth")
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{expected_header}: {output:?}");

        let requests = server.received_requests().await.unwrap();
        let request = requests
            .first()
            .expect("the winning credential sent a request");
        assert_eq!(
            request_header(request, expected_header),
            Some(expected_value)
        );
        let other = if expected_header == "authorization" {
            "x-api-key"
        } else {
            "authorization"
        };
        assert_eq!(request_header(request, other), None);
        assert_private_auth_run(root, &output);
    }
}

#[test]
fn native_auth_reports_selected_file_failures_without_disclosing_content() {
    for (name, bytes) in [
        ("missing", None),
        ("empty", Some(&b""[..])),
        ("line-endings", Some(&b"\r\n\n"[..])),
        ("utf8", Some(&b"private-token-sentinel\xff"[..])),
        ("header", Some(&b"private-token-sentinel\nsecond-line"[..])),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let token = root.join(name);
        if let Some(bytes) = bytes {
            std::fs::write(&token, bytes).unwrap();
        }
        let path = token.display();
        write_user_config(
            root,
            &formatdoc! {"
                [client.auth]
                type = 'bearer'
                file = '{path}'
            "},
        );

        let output = ox(root)
            .args(["-p", "hello"])
            .current_dir(root)
            .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("client.auth"), "{name}: {stderr}");
        assert!(
            stderr.contains(&token.display().to_string()),
            "{name}: {stderr}"
        );
        assert_private_auth_run(root, &output);
    }
}

#[tokio::test]
async fn native_auth_expands_home_paths_independently_of_cwd_and_rereads_on_the_next_start() {
    let server = MockServer::start().await;
    mount_reply(&server, "Done.").await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("home")).unwrap();
    write_user_config(
        root,
        indoc! {"
            [client.auth]
            type = 'bearer'
            file = '~/token'
        "},
    );

    for (cwd, token) in [("first", "first-token"), ("second", "second-token")] {
        let work = root.join(cwd);
        std::fs::create_dir(&work).unwrap();
        std::fs::write(root.join("home/token"), format!("{token}\n")).unwrap();

        let output = tokio::process::Command::from(ox(root))
            .args(["-p", "hello"])
            .current_dir(&work)
            .env("ANTHROPIC_BASE_URL", server.uri())
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{cwd}: {output:?}");
    }

    let requests = server.received_requests().await.unwrap();
    let credentials: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/messages")
        .map(|request| request_header(request, "authorization"))
        .collect();
    assert_eq!(
        credentials,
        [Some("Bearer first-token"), Some("Bearer second-token")]
    );
}

#[tokio::test]
async fn native_auth_replaces_layer_credentials_and_header_maps_as_whole_choices() {
    for (lower_auth, upper_auth, upper_headers, expected_header, expected_value) in [
        (
            "auth = { type = 'api-key', value = 'old-key' }",
            "auth = { type = 'bearer', value = 'new-token' }",
            "{ x-route = 'new' }",
            "authorization",
            "Bearer new-token",
        ),
        (
            "auth = { type = 'bearer', value = 'old-token' }",
            "auth = { type = 'api-key', value = 'new-key' }",
            "{}",
            "x-api-key",
            "new-key",
        ),
        (
            "auth = { type = 'bearer', value = 'old-token' }",
            "auth = { type = 'api-key', file = 'TOKEN' }",
            "{}",
            "x-api-key",
            "new-file-key",
        ),
        (
            "auth = { type = 'bearer', value = 'old-token' }",
            "auth = { type = 'login' }",
            "{}",
            "authorization",
            "Bearer oauth-fallback",
        ),
        (
            "auth = { type = 'api-key', value = 'old-key' }",
            "auth = { type = 'login' }",
            "{}",
            "authorization",
            "Bearer oauth-fallback",
        ),
    ] {
        let server = MockServer::start().await;
        mount_reply(&server, "Done.").await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let token = root.join("token");
        std::fs::write(&token, "new-file-key\n").unwrap();
        write_user_config(
            root,
            &formatdoc! {"
                [client]
                {lower_auth}
                headers = {{ x-obsolete = 'old', x-route = 'old' }}
            "},
        );
        let drop_ins = root.join("config/ox/config.d");
        std::fs::create_dir(&drop_ins).unwrap();
        let upper_auth = upper_auth.replace("TOKEN", &token.display().to_string());
        std::fs::write(
            drop_ins.join("50-profile.toml"),
            formatdoc! {"
                [client]
                {upper_auth}
                headers = {upper_headers}
            "},
        )
        .unwrap();

        let output = tokio::process::Command::from(ox(root))
            .args(["-p", "hello"])
            .current_dir(root)
            .env("ANTHROPIC_BASE_URL", server.uri())
            .env("CLAUDE_CODE_OAUTH_TOKEN", "oauth-fallback")
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{upper_auth}: {output:?}");

        let requests = server.received_requests().await.unwrap();
        let request = requests.first().expect("the upper layer authenticated");
        assert_eq!(
            request_header(request, expected_header),
            Some(expected_value)
        );
        assert_eq!(request_header(request, "x-obsolete"), None);
        assert_eq!(
            request_header(request, "x-route"),
            (upper_headers != "{}").then_some("new")
        );
    }
}

#[test]
fn native_auth_login_keeps_stored_credentials_away_from_gateways() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_user_config(root, "client.auth = { type = 'login' }\n");
    let data = root.join("data/ox");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("credentials.json"), "private-token-sentinel").unwrap();

    let output = ox(root)
        .args(["-p", "hello"])
        .current_dir(root)
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Stored logins are sent only to the Anthropic API."),
        "{stderr}"
    );
    assert!(!stderr.contains("private-token-sentinel"), "{stderr}");
    assert!(!stderr.contains("credentials.json"), "{stderr}");
}

#[tokio::test]
async fn native_auth_keeps_endpoint_header_attribution_model_and_effort_environment_overrides() {
    for override_env in [false, true] {
        let server = MockServer::start().await;
        mount_reply(&server, "Done.").await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let url = server.uri();
        let configured_url = if override_env {
            "http://127.0.0.1:9"
        } else {
            &url
        };
        write_user_config(
            root,
            &formatdoc! {"
                [client]
                base_url = '{configured_url}'
                model = 'claude-opus-5-5'
                effort = 'high'
                headers = {{ x-file = 'file' }}
                attribution = false
                structured_outputs = false
                auth = {{ type = 'bearer', value = 'private-token-sentinel' }}
            "},
        );
        let work = root.join("work");
        std::fs::create_dir_all(work.join(".git")).unwrap();
        std::fs::create_dir(work.join(".ox")).unwrap();
        std::fs::write(
            work.join(".ox/config.toml"),
            "client.model = 'claude-sonnet-5-5'\n",
        )
        .unwrap();
        let mut command = tokio::process::Command::from(ox(root));
        command.args(["-p", "hello"]).current_dir(&work);
        if override_env {
            command
                .env("ANTHROPIC_BASE_URL", &url)
                .env("ANTHROPIC_CUSTOM_HEADERS", "x-env: env")
                .env("CLAUDE_CODE_ATTRIBUTION_HEADER", "1")
                .env("CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS", "0")
                .env("OX_MODEL", "claude-haiku-5-5")
                .env("OX_EFFORT", "low");
        }

        let output = command.output().await.unwrap();
        assert!(output.status.success(), "{override_env}: {output:?}");

        let requests = server.received_requests().await.unwrap();
        let request = requests
            .first()
            .expect("the configured endpoint received the request");
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body["model"],
            if override_env {
                "claude-haiku-5-5"
            } else {
                "claude-sonnet-5-5"
            }
        );
        assert_eq!(
            body["output_config"]["effort"],
            if override_env { "low" } else { "high" }
        );
        assert_eq!(
            request_header(request, "x-file"),
            (!override_env).then_some("file")
        );
        assert_eq!(
            request_header(request, "x-env"),
            override_env.then_some("env")
        );
        assert_eq!(
            body.to_string().contains("x-anthropic-billing-header"),
            override_env
        );
        assert_private_auth_run(root, &output);
    }
}

#[tokio::test]
async fn native_auth_rejection_names_the_setting_and_keeps_the_credential_private() {
    for kind in ["bearer", "api-key", "oauth"] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "type": "error",
                "error": { "type": "authentication_error", "message": "Rejected" },
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_user_config(
            root,
            &formatdoc! {"
                [client.auth]
                type = '{kind}'
                value = 'private-token-sentinel'
            "},
        );

        let output = tokio::process::Command::from(ox(root))
            .args(["-p", "hello"])
            .current_dir(root)
            .env("ANTHROPIC_BASE_URL", server.uri())
            .env("RUST_LOG", "trace")
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{kind}: {output:?}");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("client.auth"), "{stderr}");
        assert_private_auth_run(root, &output);
    }
}

#[test]
fn config_check_lists_each_file_read_in_load_order_and_writes_nothing() {
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
fn config_check_skips_the_credential_that_a_session_needs() {
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
fn config_check_skips_the_ca_bundle_that_a_session_reads() {
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
fn config_check_rejects_an_unknown_key_with_its_file() {
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
fn config_check_rejects_an_invalid_environment_variable() {
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
fn config_check_rejects_a_theme_that_does_not_exist() {
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
fn config_check_applies_the_project_layer_and_its_limits() {
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
fn config_check_warns_about_an_unsupported_model_and_ignored_theme_overrides() {
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
fn config_check_escapes_control_characters_a_project_file_supplies() {
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
fn resume_in_the_repl_unarchives_the_session_with_a_note() {
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
        .arg("--resume")
        .arg(&session)
        .current_dir(dir.path())
        .env("ANTHROPIC_AUTH_TOKEN", "test-token")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
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
    let init = isolated_git(dir.path())
        .args(["init", "--initial-branch", "fixture-branch"])
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
    let (sessions, sidecars): (Vec<_>, Vec<_>) = std::fs::read_dir(&projects[0])
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .partition(|path| path.extension().is_some_and(|ext| ext == "jsonl"));
    assert_eq!(sidecars, [projects[0].join(".writer.lock")]);
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
async fn headless_attaches_piped_stdin_and_continues_its_session() {
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
async fn headless_continue_reports_files_changed_since_the_session_read_them() {
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
async fn headless_without_a_prompt_or_with_a_list_flag_fails_with_a_usage_error() {
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

#[tokio::test]
async fn repl_exits_with_failure_when_its_prompt_cannot_be_written() {
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

/// Runs `ox` with `args` until its request reaches a reply that never comes, then sends it
/// `signal`. Returns the signal that ended it, its stdout, and its stderr.
#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
#[tokio::test]
async fn repl_with_piped_input_dies_by_the_signal() {
    for (signal, number) in [("HUP", 1), ("INT", 2), ("TERM", 15)] {
        let (signaled, _, stderr) = interrupted_run(&["--no-tui"], signal).await;

        assert_eq!(signaled, Some(number), "SIG{signal}");
        assert!(
            stderr.ends_with("> \n"),
            "SIG{signal}: the break after the prompt proves the handler ran: {stderr:?}"
        );
    }
}
