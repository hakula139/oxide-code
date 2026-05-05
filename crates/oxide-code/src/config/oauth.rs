//! Claude Code OAuth credential loading and refresh.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

#[cfg(target_os = "macos")]
use crate::util::env;
use crate::util::fs::atomic_write_private;
use crate::util::lock;

const OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_SCOPES: &[&str] = &[
    "user:file_upload",
    "user:inference",
    "user:mcp_servers",
    "user:profile",
    "user:sessions:claude_code",
];
const TOKEN_EXPIRY_BUFFER_MS: u64 = 5 * 60 * 1000;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

const LOCK_STALE_THRESHOLD: Duration = Duration::from_secs(30);

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

// ── Credential Types ──

#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: OAuthCredential,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OAuthCredential {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_at: i64,
}

impl OAuthCredential {
    fn expires_at_ms(&self) -> u64 {
        u64::try_from(self.expires_at).unwrap_or(0)
    }
}

// ── Token Loading ──

/// Loads (and refreshes if near expiry) the Claude Code OAuth access token. On macOS the Keychain
/// is consulted first; non-macOS and Keychain-miss fall back to `~/.claude/.credentials.json`.
pub(super) async fn load_token() -> Result<String> {
    let file_path = credentials_path().context("could not determine home directory")?;
    let lock_path = lock_path().context("could not determine home directory")?;
    load_token_from(&file_path, &lock_path, OAUTH_TOKEN_URL, load_credentials).await
}

async fn load_token_from(
    file_path: &Path,
    lock_path: &Path,
    refresh_url: &str,
    loader: fn(&Path) -> Result<CredentialsFile>,
) -> Result<String> {
    let oauth = loader(file_path)?.claude_ai_oauth;
    let expires_at_ms = oauth.expires_at_ms();

    if !is_near_expiry(expires_at_ms) {
        return Ok(oauth.access_token);
    }

    if oauth.refresh_token.is_none() {
        if is_expired(expires_at_ms) {
            bail!("Claude Code OAuth token has expired — run `claude` to refresh");
        }
        warn!("OAuth token expires soon but no refresh token available");
        return Ok(oauth.access_token);
    }

    let _lock = acquire_lock(lock_path).await?;

    // Double-checked: a sibling process may have refreshed while we were waiting on the lock.
    let oauth = loader(file_path)?.claude_ai_oauth;
    let expires_at_ms = oauth.expires_at_ms();
    if !is_near_expiry(expires_at_ms) {
        return Ok(oauth.access_token);
    }

    let refresh_token = oauth
        .refresh_token
        .as_deref()
        .context("refresh token missing after re-read")?;

    match refresh_oauth_token(refresh_url, refresh_token).await {
        Ok(response) => {
            write_refreshed_credentials(file_path, &response)?;
            Ok(response.access_token)
        }
        Err(e) if is_expired(expires_at_ms) => {
            Err(e).context("failed to refresh expired OAuth token")
        }
        Err(e) => {
            warn!("failed to refresh OAuth token, using existing: {e:#}");
            Ok(oauth.access_token)
        }
    }
}

#[cfg(target_os = "macos")]
fn load_credentials(file_path: &Path) -> Result<CredentialsFile> {
    if let Some(kc) = read_keychain() {
        return Ok(kc);
    }
    read_credentials(file_path)
}

#[cfg(not(target_os = "macos"))]
fn load_credentials(file_path: &Path) -> Result<CredentialsFile> {
    read_credentials(file_path)
}

#[cfg(target_os = "macos")]
fn read_keychain() -> Option<CredentialsFile> {
    use security_framework::passwords::{PasswordOptions, generic_password};
    use tracing::debug;

    let account = keychain_account()?;
    let bytes = match generic_password(PasswordOptions::new_generic_password(
        KEYCHAIN_SERVICE,
        &account,
    )) {
        Ok(b) => b,
        Err(e) => {
            debug!("Keychain read failed: {e}");
            return None;
        }
    };
    let json = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            debug!("Keychain data is not valid UTF-8: {e}");
            return None;
        }
    };
    match serde_json::from_str(&json) {
        Ok(creds) => Some(creds),
        Err(e) => {
            debug!("Keychain JSON parse failed: {e}");
            None
        }
    }
}

#[cfg(target_os = "macos")]
fn keychain_account() -> Option<String> {
    env::string("USER")
}

fn read_credentials(path: &Path) -> Result<CredentialsFile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    enforce_private_mode(path);
    serde_json::from_str(&content).context("failed to parse Claude Code credentials")
}

#[cfg(unix)]
fn enforce_private_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        debug!("failed to reassert 0o600 on {}: {e}", path.display());
    }
}

#[cfg(not(unix))]
fn enforce_private_mode(_path: &Path) {}

fn is_near_expiry(expires_at_ms: u64) -> bool {
    now_millis().is_none_or(|now| now.saturating_add(TOKEN_EXPIRY_BUFFER_MS) >= expires_at_ms)
}

fn is_expired(expires_at_ms: u64) -> bool {
    now_millis().is_none_or(|now| now >= expires_at_ms)
}

// ── Token Refresh ──

#[derive(Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'a str,
    refresh_token: &'a str,
    client_id: &'a str,
    scope: &'a str,
}

async fn refresh_oauth_token(url: &str, refresh_token: &str) -> Result<RefreshResponse> {
    let client = reqwest::Client::builder()
        .timeout(REFRESH_TIMEOUT)
        .build()?;

    let scope = OAUTH_SCOPES.join(" ");
    let response = client
        .post(url)
        .json(&RefreshRequest {
            grant_type: "refresh_token",
            refresh_token,
            client_id: OAUTH_CLIENT_ID,
            scope: &scope,
        })
        .send()
        .await
        .context("failed to send token refresh request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("token refresh failed with {status}: {body}");
    }

    response
        .json()
        .await
        .context("failed to parse token refresh response")
}

#[cfg_attr(test, derive(Debug))]
#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    #[serde(default)]
    scope: Option<String>,
}

fn write_refreshed_credentials(path: &Path, response: &RefreshResponse) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let mut json: serde_json::Value =
        serde_json::from_str(&content).context("failed to parse credentials for update")?;

    let oauth = json
        .get_mut("claudeAiOauth")
        .context("missing claudeAiOauth in credentials")?;

    oauth["accessToken"] = serde_json::Value::String(response.access_token.clone());
    oauth["refreshToken"] = serde_json::Value::String(response.refresh_token.clone());
    let now = now_millis().context("system clock before UNIX epoch; cannot record token expiry")?;
    oauth["expiresAt"] =
        serde_json::json!(now.saturating_add(response.expires_in.saturating_mul(1000)));

    if let Some(scope) = &response.scope {
        let scopes: Vec<&str> = scope.split_whitespace().collect();
        oauth["scopes"] = serde_json::json!(scopes);
    }

    let serialized = serde_json::to_string_pretty(&json)?;
    atomic_write_private(path, serialized.as_bytes())?;

    #[cfg(all(target_os = "macos", not(test)))]
    if let Err(e) = write_keychain(&serialized) {
        warn!("failed to update Keychain: {e:#}");
    }

    Ok(())
}

#[cfg(all(target_os = "macos", not(test)))]
fn write_keychain(json: &str) -> Result<()> {
    use security_framework::passwords::set_generic_password;

    let account = keychain_account().context("could not determine OS username")?;
    set_generic_password(KEYCHAIN_SERVICE, &account, json.as_bytes())
        .context("failed to write to Keychain")
}

fn now_millis() -> Option<u64> {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()
}

fn credentials_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join(".credentials.json"))
}

// ── File Locking ──

struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            debug!("failed to release lock {}: {e}", self.path.display());
        }
    }
}

/// Directory-based advisory lock around credential refresh. `mkdir` is the cross-platform atomic
/// primitive; the guard `rmdir`s on drop. A lock older than [`LOCK_STALE_THRESHOLD`] is treated
/// as abandoned by a crashed sibling and cleared on the next retry.
async fn acquire_lock(path: &Path) -> Result<LockGuard> {
    lock::retry_acquire(
        || match std::fs::create_dir(path) {
            Ok(()) => Ok(Some(LockGuard {
                path: path.to_owned(),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale_lock(path)
                    && let Err(e) = std::fs::remove_dir_all(path)
                {
                    debug!("failed to clear stale lock {}: {e}", path.display());
                }
                Ok(None)
            }
            Err(e) => Err(anyhow::Error::new(e)
                .context(format!("failed to create lock at {}", path.display()))),
        },
        lock::MAX_RETRIES,
        lock::RETRY_INTERVAL,
        || {
            anyhow!(
                "failed to acquire credentials lock after {} retries \
                 — another process may be refreshing",
                lock::MAX_RETRIES,
            )
        },
    )
    .await
}

fn is_stale_lock(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().unwrap_or_default() > LOCK_STALE_THRESHOLD)
}

fn lock_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude.lock"))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;

    // ── OAuthCredential::expires_at_ms ──

    #[test]
    fn expires_at_ms_positive_value() {
        let cred = OAuthCredential {
            access_token: String::new(),
            refresh_token: None,
            expires_at: 1_700_000_000_000,
        };
        assert_eq!(cred.expires_at_ms(), 1_700_000_000_000);
    }

    #[test]
    fn expires_at_ms_negative_clamps_to_zero() {
        let cred = OAuthCredential {
            access_token: String::new(),
            refresh_token: None,
            expires_at: -1,
        };
        assert_eq!(cred.expires_at_ms(), 0);
    }

    // ── load_token ──

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn load_token_resolves_credentials_relative_to_home() {
        let home = tempfile::tempdir().unwrap();
        let claude_dir = home.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        write_creds(
            &claude_dir.join(".credentials.json"),
            "token-from-home",
            None,
            9_999_999_999_999,
        );

        let token = temp_env::async_with_vars(
            [("HOME", Some(home.path().to_string_lossy().into_owned()))],
            async { load_token().await.unwrap() },
        )
        .await;
        assert_eq!(token, "token-from-home");
    }

    // ── load_token_from ──

    fn write_creds(path: &Path, access: &str, refresh: Option<&str>, expires_at: u64) {
        let mut oauth = serde_json::json!({
            "accessToken": access,
            "expiresAt": expires_at,
        });
        if let Some(r) = refresh {
            oauth["refreshToken"] = r.into();
        }
        let body = serde_json::json!({ "claudeAiOauth": oauth }).to_string();
        std::fs::write(path, body).unwrap();
    }

    #[tokio::test]
    async fn load_token_from_keeps_existing_when_far_from_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(&creds, "tok", Some("ref"), 9_999_999_999_999);

        let token = load_token_from(
            &creds,
            &lock,
            "http://should-not-be-called",
            read_credentials,
        )
        .await
        .unwrap();
        assert_eq!(token, "tok");
    }

    #[tokio::test]
    async fn load_token_from_without_refresh_token_keeps_nonexpired_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(&creds, "tok", None, now_millis().unwrap() + 60_000);

        let token = load_token_from(
            &creds,
            &lock,
            "http://should-not-be-called",
            read_credentials,
        )
        .await
        .unwrap();
        assert_eq!(token, "tok");
    }

    #[tokio::test]
    async fn load_token_from_without_refresh_token_bails_when_expired() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(&creds, "tok", None, 0);

        let err = load_token_from(&creds, &lock, "http://unused", read_credentials)
            .await
            .expect_err("expired without refresh must bail");
        assert!(format!("{err:#}").contains("expired"));
    }

    #[tokio::test]
    async fn load_token_from_refreshes_near_expiry_and_writes_back() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_refresh_body(
                "fresh-access",
                "fresh-refresh",
                3600,
            )))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(
            &creds,
            "old",
            Some("old-refresh"),
            now_millis().unwrap() + 1_000,
        );

        let token = load_token_from(&creds, &lock, &server.uri(), read_credentials)
            .await
            .unwrap();
        assert_eq!(token, "fresh-access");

        let content = std::fs::read_to_string(&creds).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["claudeAiOauth"]["accessToken"], "fresh-access");
        assert_eq!(json["claudeAiOauth"]["refreshToken"], "fresh-refresh");
        let expires_at = json["claudeAiOauth"]["expiresAt"].as_u64().unwrap();
        let now = now_millis().unwrap();
        assert!(
            expires_at >= now + 3_500_000 && expires_at <= now + 3_700_000,
            "expiresAt out of band: {expires_at}",
        );
    }

    #[tokio::test]
    async fn load_token_from_refresh_endpoint_down_keeps_existing_token_if_unexpired() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(
            &creds,
            "stale",
            Some("old-refresh"),
            now_millis().unwrap() + 60_000,
        );

        let token = load_token_from(&creds, &lock, &server.uri(), read_credentials)
            .await
            .unwrap();
        assert_eq!(token, "stale");
    }

    #[tokio::test]
    async fn load_token_from_refresh_endpoint_down_bails_if_expired() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(&creds, "dead", Some("old"), 0);

        let err = load_token_from(&creds, &lock, &server.uri(), read_credentials)
            .await
            .expect_err("expired + refresh down must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("expired OAuth"), "wrapped context: {msg}");
    }

    // ── read_credentials ──

    #[test]
    fn read_credentials_valid_with_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");

        std::fs::write(
            &path,
            indoc::indoc! {r#"
                {
                    "claudeAiOauth": {
                        "accessToken": "tok",
                        "refreshToken": "ref",
                        "expiresAt": 9999999999999
                    }
                }
            "#},
        )
        .unwrap();

        let creds = read_credentials(&path).unwrap();
        assert_eq!(creds.claude_ai_oauth.access_token, "tok");
        assert_eq!(creds.claude_ai_oauth.refresh_token.as_deref(), Some("ref"));
        assert_eq!(creds.claude_ai_oauth.expires_at, 9_999_999_999_999);
    }

    #[test]
    fn read_credentials_missing_refresh_token_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");

        std::fs::write(
            &path,
            indoc::indoc! {r#"
                {
                    "claudeAiOauth": {
                        "accessToken": "tok",
                        "expiresAt": 9999999999999
                    }
                }
            "#},
        )
        .unwrap();

        let creds = read_credentials(&path).unwrap();
        assert!(creds.claude_ai_oauth.refresh_token.is_none());
    }

    #[test]
    fn read_credentials_missing_file() {
        assert!(read_credentials(Path::new("/nonexistent/creds.json")).is_err());
    }

    #[test]
    fn read_credentials_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(read_credentials(&path).is_err());
    }

    #[test]
    fn read_credentials_missing_oauth_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, r#"{"other": "data"}"#).unwrap();
        assert!(read_credentials(&path).is_err());
    }

    // ── is_near_expiry ──

    #[test]
    fn is_near_expiry_far_future() {
        assert!(!is_near_expiry(u64::MAX));
    }

    #[test]
    fn is_near_expiry_zero_is_expired() {
        assert!(is_near_expiry(0));
    }

    // ── is_expired ──

    #[test]
    fn is_expired_far_future() {
        assert!(!is_expired(u64::MAX));
    }

    #[test]
    fn is_expired_zero() {
        assert!(is_expired(0));
    }

    // ── refresh_oauth_token ──

    fn ok_refresh_body(access: &str, refresh: &str, expires_in: u64) -> serde_json::Value {
        serde_json::json!({
            "access_token": access,
            "refresh_token": refresh,
            "expires_in": expires_in,
            "scope": "user:profile user:inference",
        })
    }

    #[tokio::test]
    async fn refresh_oauth_token_sends_grant_and_client_id_and_produces_parsed_response() {
        let server = MockServer::start().await;
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let captured_clone = Arc::clone(&captured);
        Mock::given(method("POST"))
            .and(wm_path("/"))
            .respond_with(move |req: &Request| {
                *captured_clone.lock().unwrap() = serde_json::from_slice(&req.body).ok();
                ResponseTemplate::new(200).set_body_json(ok_refresh_body(
                    "new-access",
                    "new-refresh",
                    3600,
                ))
            })
            .mount(&server)
            .await;

        let response = refresh_oauth_token(&server.uri(), "old-refresh")
            .await
            .unwrap();
        assert_eq!(response.access_token, "new-access");
        assert_eq!(response.refresh_token, "new-refresh");
        assert_eq!(response.expires_in, 3600);
        assert_eq!(
            response.scope.as_deref(),
            Some("user:profile user:inference")
        );

        let body = captured.lock().unwrap().take().expect("body captured");
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "old-refresh");
        assert_eq!(body["client_id"], OAUTH_CLIENT_ID);
        assert_eq!(body["scope"], OAUTH_SCOPES.join(" "));
    }

    #[tokio::test]
    async fn refresh_oauth_token_propagates_http_error_with_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string(r#"{"error":"invalid_grant"}"#),
            )
            .mount(&server)
            .await;

        let err = refresh_oauth_token(&server.uri(), "bad")
            .await
            .expect_err("expected HTTP error");
        let msg = format!("{err:#}");
        assert!(msg.contains("401"), "status: {msg}");
        assert!(msg.contains("invalid_grant"), "body: {msg}");
    }

    #[tokio::test]
    async fn refresh_oauth_token_malformed_json_errors_with_context() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<not json>"))
            .mount(&server)
            .await;

        let err = refresh_oauth_token(&server.uri(), "tok")
            .await
            .expect_err("expected parse error");
        assert!(format!("{err:#}").contains("parse"));
    }

    // ── write_refreshed_credentials ──

    #[test]
    fn write_refreshed_credentials_preserves_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");

        std::fs::write(
            &path,
            indoc::indoc! {r#"
                {
                    "claudeAiOauth": {
                        "accessToken": "old-access",
                        "refreshToken": "old-refresh",
                        "expiresAt": 1000,
                        "scopes": ["user:profile"],
                        "subscriptionType": "pro",
                        "rateLimitTier": "default"
                    }
                }
            "#},
        )
        .unwrap();

        let response = RefreshResponse {
            access_token: "new-access".to_owned(),
            refresh_token: "new-refresh".to_owned(),
            expires_in: 3600,
            scope: Some("user:profile user:inference".to_owned()),
        };

        write_refreshed_credentials(&path, &response).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        let oauth = &json["claudeAiOauth"];

        assert_eq!(oauth["accessToken"], "new-access");
        assert_eq!(oauth["refreshToken"], "new-refresh");
        let expires_at = oauth["expiresAt"].as_u64().unwrap();
        let now = now_millis().unwrap();
        assert!(
            expires_at >= now + 3_500_000,
            "expiresAt too early: {expires_at}"
        );
        assert!(
            expires_at <= now + 3_700_000,
            "expiresAt too late: {expires_at}"
        );
        assert_eq!(
            oauth["scopes"],
            serde_json::json!(["user:profile", "user:inference"])
        );
        assert_eq!(oauth["subscriptionType"], "pro");
        assert_eq!(oauth["rateLimitTier"], "default");
    }

    #[test]
    fn write_refreshed_credentials_tolerates_whitespace_in_scope_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");

        std::fs::write(
            &path,
            indoc::indoc! {r#"
                {
                    "claudeAiOauth": {
                        "accessToken": "t",
                        "refreshToken": "r",
                        "expiresAt": 1000,
                        "scopes": []
                    }
                }
            "#},
        )
        .unwrap();

        let response = RefreshResponse {
            access_token: "t2".to_owned(),
            refresh_token: "r2".to_owned(),
            expires_in: 3600,
            scope: Some("  user:profile   user:inference  ".to_owned()),
        };

        write_refreshed_credentials(&path, &response).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            json["claudeAiOauth"]["scopes"],
            serde_json::json!(["user:profile", "user:inference"]),
        );
    }

    #[test]
    fn write_refreshed_credentials_skips_scopes_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");

        std::fs::write(
            &path,
            indoc::indoc! {r#"
                {
                    "claudeAiOauth": {
                        "accessToken": "old",
                        "refreshToken": "old",
                        "expiresAt": 1000,
                        "scopes": ["user:profile"]
                    }
                }
            "#},
        )
        .unwrap();

        let response = RefreshResponse {
            access_token: "new".to_owned(),
            refresh_token: "new".to_owned(),
            expires_in: 3600,
            scope: None,
        };

        write_refreshed_credentials(&path, &response).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        let oauth = &json["claudeAiOauth"];

        assert_eq!(oauth["scopes"], serde_json::json!(["user:profile"]));
    }

    // ── acquire_lock ──

    #[tokio::test]
    async fn acquire_lock_creates_and_drop_removes() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");

        let guard = acquire_lock(&lock_path).await.unwrap();
        assert!(lock_path.exists());

        drop(guard);
        assert!(!lock_path.exists());
    }
}
