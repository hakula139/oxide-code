use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::util::{env, lock};

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

/// Directory mtime threshold above which an existing lock is treated
/// as stale and removed before re-attempting acquisition. Guards
/// against a peer that crashed without cleaning up its lock dir.
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

/// Load an OAuth access token from Claude Code credentials, refreshing
/// proactively if the token is within 5 minutes of expiry.
pub async fn load_token() -> Result<String> {
    let file_path = credentials_path().context("could not determine home directory")?;
    let oauth = load_credentials(&file_path)?.claude_ai_oauth;
    let expires_at_ms = oauth.expires_at_ms();

    // Token is valid and not near-expiry.
    if !is_near_expiry(expires_at_ms) {
        return Ok(oauth.access_token);
    }

    // No refresh token — use as-is if not yet expired.
    if oauth.refresh_token.is_none() {
        if is_expired(expires_at_ms) {
            bail!("Claude Code OAuth token has expired — run `claude` to refresh");
        }
        warn!("OAuth token expires soon but no refresh token available");
        return Ok(oauth.access_token);
    }

    // Acquire lock and re-read (another process may have refreshed).
    let lock_path = lock_path().context("could not determine home directory")?;
    let _lock = acquire_lock(&lock_path).await?;

    let oauth = load_credentials(&file_path)?.claude_ai_oauth;
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
            write_refreshed_credentials(&file_path, &response)?;
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

/// Load credentials from the best available source.
///
/// On macOS, the Keychain is the authoritative source — preferred whenever
/// present, with the credentials file as a fallback. This keeps trust inverted
/// from the more-permissive file: an attacker who can write `~/.claude/.credentials.json`
/// cannot override a valid Keychain entry by claiming a far-future expiry.
/// Near-expired Keychain entries are still used; [`is_near_expiry`] triggers a
/// refresh that writes both sources back in sync.
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
    let parsed =
        serde_json::from_str(&content).context("failed to parse Claude Code credentials")?;
    enforce_private_mode(path);
    Ok(parsed)
}

/// Reassert owner-only permissions on the credentials file.
///
/// `claude` normally creates the file with `0o600`, but older versions or a
/// user who `cp`'d the file may have left laxer perms in place. We reapply
/// the strict mode every time we read so the window of exposure is bounded.
/// Failures are logged at debug level — we have no fallback but shouldn't
/// prevent the user from authenticating.
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

/// Write refreshed tokens back to the credentials file (and macOS Keychain),
/// preserving unknown fields.
///
/// The file is replaced atomically via write-to-temp + rename so a crash
/// between the open-truncate and the final write cannot leave the file
/// empty or half-written — a corruption there would invalidate login for
/// both `ox` and `claude`.
///
/// Must be called while holding the [`LockGuard`] from [`acquire_lock`].
fn write_refreshed_credentials(path: &Path, response: &RefreshResponse) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let mut json: serde_json::Value =
        serde_json::from_str(&content).context("failed to parse credentials for update")?;

    let oauth = json
        .get_mut("claudeAiOauth")
        .context("missing claudeAiOauth in credentials")?;

    oauth["accessToken"] = serde_json::Value::String(response.access_token.clone());
    oauth["refreshToken"] = serde_json::Value::String(response.refresh_token.clone());
    // Computed from local clock — will be wrong if the machine clock is skewed,
    // but the refresh endpoint only returns a relative `expires_in`, not an
    // absolute timestamp.
    oauth["expiresAt"] = serde_json::json!(now_millis() + response.expires_in * 1000);

    if let Some(scope) = &response.scope {
        // `split_whitespace` tolerates extra or leading/trailing spaces in the
        // server's `scope` field; `split(' ')` would emit empty strings.
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

/// Write `bytes` to `path` atomically with owner-only (`0o600`) permissions
/// on Unix. Creates a sibling `.tmp.<uuid>` file, chmods it, then renames —
/// the rename is atomic on POSIX, so any reader sees either the old or the
/// new content, never a truncated state.
fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("credentials path has no parent directory")?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("creds"),
        uuid::Uuid::new_v4().simple(),
    ));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .with_context(|| format!("failed to create temp file {}", tmp.display()))?;

    let write_result = std::io::Write::write_all(&mut file, bytes)
        .and_then(|()| file.sync_all())
        .map_err(anyhow::Error::from);

    if let Err(e) = write_result {
        _ = std::fs::remove_file(&tmp);
        return Err(e.context(format!(
            "failed to write temp credentials {}",
            tmp.display()
        )));
    }
    drop(file);

    if let Err(e) = std::fs::rename(&tmp, path) {
        _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::from(e).context(format!(
            "failed to install credentials at {}",
            path.display()
        )));
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
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            debug!("failed to release lock {}: {e}", self.path.display());
        }
    }
}

/// Acquire a directory-based lock, compatible with `proper-lockfile`
/// (used by Claude Code). Retries contended locks via the shared
/// [`lock::retry_acquire`] helper and breaks stale lock directories
/// on each attempt.
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
    // Treat unreadable metadata as *not* stale — safer to back off and retry
    // than to clobber a lock we can't inspect (EACCES, EIO, ...).
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().unwrap_or_default() > LOCK_STALE_THRESHOLD)
}

fn lock_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude.lock"))
}

#[cfg(test)]
mod tests {
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
        let now = now_millis();
        // expires_in is 3600s → 3_600_000ms from now, with tolerance for test execution time
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
