use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;

const OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];
const TOKEN_EXPIRY_BUFFER_MS: u64 = 5 * 60 * 1000;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

const LOCK_MAX_RETRIES: u32 = 5;
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(1000);
const LOCK_STALE_THRESHOLD: Duration = Duration::from_secs(30);

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
    refresh_token: Option<String>,
    expires_at: i64,
}

impl OAuthCredential {
    fn expires_at_ms(&self) -> u64 {
        u64::try_from(self.expires_at).unwrap_or(0)
    }
}

// ── Token Loading ──

/// Load an OAuth access token from Claude Code's credentials file, refreshing
/// proactively if the token is within 5 minutes of expiry.
pub async fn load_token() -> Result<String> {
    let path = credentials_path().context("could not determine home directory")?;
    let oauth = read_credentials(&path)?.claude_ai_oauth;
    let expires_at_ms = oauth.expires_at_ms();

    // Token is valid and not near-expiry
    if !is_near_expiry(expires_at_ms) {
        return Ok(oauth.access_token);
    }

    // No refresh token — use as-is if not yet expired
    if oauth.refresh_token.is_none() {
        if is_expired(expires_at_ms) {
            bail!("Claude Code OAuth token has expired — run `claude` to refresh");
        }
        warn!("OAuth token expires soon but no refresh token available");
        return Ok(oauth.access_token);
    }

    // Acquire lock and re-read (another process may have refreshed)
    let lock_path = lock_path().context("could not determine home directory")?;
    let _lock = acquire_lock(&lock_path).await?;

    let oauth = read_credentials(&path)?.claude_ai_oauth;
    let expires_at_ms = oauth.expires_at_ms();

    if !is_near_expiry(expires_at_ms) {
        return Ok(oauth.access_token);
    }

    let refresh_token = oauth
        .refresh_token
        .as_deref()
        .context("refresh token missing after re-read")?;

    match refresh_oauth_token(refresh_token).await {
        Ok(response) => {
            write_refreshed_credentials(&path, &response)?;
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

fn read_credentials(path: &Path) -> Result<CredentialsFile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).context("failed to parse Claude Code credentials")
}

fn is_near_expiry(expires_at_ms: u64) -> bool {
    now_millis() + TOKEN_EXPIRY_BUFFER_MS >= expires_at_ms
}

fn is_expired(expires_at_ms: u64) -> bool {
    now_millis() >= expires_at_ms
}

// ── Token Refresh ──

#[derive(Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'a str,
    refresh_token: &'a str,
    client_id: &'a str,
    scope: &'a str,
}

async fn refresh_oauth_token(refresh_token: &str) -> Result<RefreshResponse> {
    let client = reqwest::Client::builder()
        .timeout(REFRESH_TIMEOUT)
        .build()?;

    let scope = OAUTH_SCOPES.join(" ");
    let response = client
        .post(OAUTH_TOKEN_URL)
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
    oauth["expiresAt"] = serde_json::json!(now_millis() + response.expires_in * 1000);

    if let Some(scope) = &response.scope {
        let scopes: Vec<&str> = scope.split(' ').collect();
        oauth["scopes"] = serde_json::json!(scopes);
    }

    let serialized = serde_json::to_string_pretty(&json)?;
    std::fs::write(path, serialized.as_bytes())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

fn now_millis() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_millis(),
    )
    .expect("current time fits in u64")
}

fn credentials_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join(".credentials.json"))
}

// ── File Locking ──

/// RAII guard that removes the lock directory on drop.
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Acquire a directory-based lock, compatible with `proper-lockfile` (used by
/// Claude Code). Retries with fixed interval and breaks stale locks.
async fn acquire_lock(path: &Path) -> Result<LockGuard> {
    for attempt in 0..=LOCK_MAX_RETRIES {
        match std::fs::create_dir(path) {
            Ok(()) => {
                return Ok(LockGuard {
                    path: path.to_owned(),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale_lock(path) {
                    let _ = std::fs::remove_dir_all(path);
                    continue;
                }
                if attempt == LOCK_MAX_RETRIES {
                    bail!(
                        "failed to acquire credentials lock after {LOCK_MAX_RETRIES} retries \
                         — another process may be refreshing"
                    );
                }
                tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to create lock at {}", path.display()));
            }
        }
    }
    unreachable!()
}

fn is_stale_lock(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_or(true, |t| {
            t.elapsed().unwrap_or_default() > LOCK_STALE_THRESHOLD
        })
}

fn lock_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(oauth["expiresAt"].as_u64().unwrap() > 1000);
        assert_eq!(
            oauth["scopes"],
            serde_json::json!(["user:profile", "user:inference"])
        );
        // Unknown fields preserved
        assert_eq!(oauth["subscriptionType"], "pro");
        assert_eq!(oauth["rateLimitTier"], "default");
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

        // Original scopes preserved when refresh response has no scope
        assert_eq!(oauth["scopes"], serde_json::json!(["user:profile"]));
    }
}
