//! Credentials from `client.auth` and the environment, as the endpoint receives them.

use super::*;

#[tokio::test]
async fn sends_each_inline_and_file_credential_with_its_request_shape() {
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
            assert_eq!(
                saved_sessions(root).len(),
                1,
                "the run persisted its session"
            );
            assert_private_auth_run(root, &output);
        }
    }
}

#[tokio::test]
async fn preserves_environment_credential_precedence_without_opening_losing_files() {
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
fn reports_selected_file_failures_without_disclosing_content() {
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
async fn expands_home_paths_independently_of_cwd_and_rereads_on_the_next_start() {
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
async fn replaces_layer_credentials_and_header_maps_as_whole_choices() {
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
fn login_keeps_stored_credentials_away_from_gateways() {
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
async fn keeps_endpoint_header_attribution_model_and_effort_environment_overrides() {
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
async fn rejection_names_the_setting_and_keeps_the_credential_private() {
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
