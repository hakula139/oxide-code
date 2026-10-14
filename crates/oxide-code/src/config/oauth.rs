//! Claude Code OAuth credential loading and refresh.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

#[cfg(target_os = "macos")]
use crate::util::env;
use crate::util::fs::atomic_write_private;
use crate::util::tls::apply_extra_ca_certs;

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
const LOCK_MAX_RETRIES: u32 = 5;
#[cfg(not(test))]
const LOCK_RETRY_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

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

/// Credentials parsed from one store, keeping the raw JSON so write-back preserves unknown fields.
struct StoredCredentials<'a> {
    source: Source<'a>,
    json: String,
    oauth: OAuthCredential,
}

impl<'a> StoredCredentials<'a> {
    fn parse(source: Source<'a>, json: String) -> Result<Self> {
        let file: CredentialsFile =
            serde_json::from_str(&json).context("failed to parse Claude Code credentials")?;
        Ok(Self {
            source,
            json,
            oauth: file.claude_ai_oauth,
        })
    }
}

/// The store credentials were read from, and therefore the only one their refresh updates.
enum Source<'a> {
    Keychain(&'a dyn Keychain),
    File(&'a Path),
}

// ── Token Loading ──

/// Loads (and refreshes if near expiry) the Claude Code OAuth access token. On macOS the Keychain
/// is consulted first. Other platforms and Keychain misses read `~/.claude/.credentials.json`.
pub(super) async fn load_token(extra_ca_certs: Option<&Path>) -> Result<String> {
    let file_path = credentials_path().context("could not determine home directory")?;
    let lock_path = lock_path().context("could not determine home directory")?;
    load_token_from(
        SYSTEM_KEYCHAIN,
        &file_path,
        &lock_path,
        OAUTH_TOKEN_URL,
        extra_ca_certs,
    )
    .await
}

async fn load_token_from(
    keychain: Option<&dyn Keychain>,
    file_path: &Path,
    lock_path: &Path,
    refresh_url: &str,
    extra_ca_certs: Option<&Path>,
) -> Result<String> {
    let oauth = load_credentials(keychain, file_path)?.oauth;
    let expires_at_ms = oauth.expires_at_ms();

    if !is_near_expiry(expires_at_ms) {
        return Ok(oauth.access_token);
    }

    if oauth.refresh_token.is_none() {
        if is_expired(expires_at_ms) {
            bail!("Claude Code OAuth token has expired (run `claude` to refresh it)");
        }
        warn!("OAuth token expires soon but no refresh token available");
        return Ok(oauth.access_token);
    }

    let _lock = match acquire_lock(lock_path).await {
        Ok(lock) => lock,
        Err(e) => {
            // The lock holder may have written a fresh token before releasing.
            let oauth = load_credentials(keychain, file_path)?.oauth;
            if is_expired(oauth.expires_at_ms()) {
                return Err(e);
            }
            warn!("skipping OAuth token refresh, using existing: {e:#}");
            return Ok(oauth.access_token);
        }
    };

    // Double-checked: a sibling process may have refreshed while we were waiting on the lock.
    let creds = load_credentials(keychain, file_path)?;
    let expires_at_ms = creds.oauth.expires_at_ms();
    if !is_near_expiry(expires_at_ms) {
        return Ok(creds.oauth.access_token);
    }

    let refresh_token = creds
        .oauth
        .refresh_token
        .as_deref()
        .context("refresh token missing after re-read")?;

    match refresh_oauth_token(refresh_url, refresh_token, extra_ca_certs).await {
        Ok(response) => {
            if let Err(e) = write_refreshed_credentials(&creds, &response) {
                warn!("failed to save refreshed OAuth credentials: {e:#}");
            }
            Ok(response.access_token)
        }
        Err(e) if is_expired(expires_at_ms) => {
            Err(e).context("failed to refresh expired OAuth token")
        }
        Err(e) => {
            warn!("failed to refresh OAuth token, using existing: {e:#}");
            Ok(creds.oauth.access_token)
        }
    }
}

fn load_credentials<'a>(
    keychain: Option<&'a dyn Keychain>,
    file_path: &'a Path,
) -> Result<StoredCredentials<'a>> {
    if let Some(keychain) = keychain {
        match keychain
            .read()
            .and_then(|json| StoredCredentials::parse(Source::Keychain(keychain), json))
        {
            Ok(creds) => return Ok(creds),
            Err(e) => debug!("Keychain credentials unavailable: {e:#}"),
        }
    }
    read_credentials(file_path)
}

fn read_credentials(path: &Path) -> Result<StoredCredentials<'_>> {
    let json = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    enforce_private_mode(path);
    StoredCredentials::parse(Source::File(path), json)
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

// ── Keychain ──

/// Claude Code's macOS Keychain entry, holding the same JSON document as the credentials file.
trait Keychain: Sync {
    fn read(&self) -> Result<String>;
    fn write(&self, json: &str) -> Result<()>;
}

#[cfg(target_os = "macos")]
const SYSTEM_KEYCHAIN: Option<&dyn Keychain> = Some(&SystemKeychain);
#[cfg(not(target_os = "macos"))]
const SYSTEM_KEYCHAIN: Option<&dyn Keychain> = None;

#[cfg(target_os = "macos")]
struct SystemKeychain;

#[cfg(target_os = "macos")]
impl Keychain for SystemKeychain {
    fn read(&self) -> Result<String> {
        use security_framework::passwords::{PasswordOptions, generic_password};

        let account = keychain_account()?;
        let bytes = generic_password(PasswordOptions::new_generic_password(
            KEYCHAIN_SERVICE,
            &account,
        ))
        .context("failed to read from Keychain")?;
        String::from_utf8(bytes).context("Keychain data is not valid UTF-8")
    }

    fn write(&self, json: &str) -> Result<()> {
        use security_framework::passwords::set_generic_password;

        let account = keychain_account()?;
        set_generic_password(KEYCHAIN_SERVICE, &account, json.as_bytes())
            .context("failed to write to Keychain")
    }
}

#[cfg(target_os = "macos")]
fn keychain_account() -> Result<String> {
    env::string("USER").context("could not determine OS username")
}

// ── Token Refresh ──

#[derive(Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'a str,
    refresh_token: &'a str,
    client_id: &'a str,
    scope: &'a str,
}

async fn refresh_oauth_token(
    url: &str,
    refresh_token: &str,
    extra_ca_certs: Option<&Path>,
) -> Result<RefreshResponse> {
    let builder = reqwest::Client::builder().timeout(REFRESH_TIMEOUT);
    let builder = apply_extra_ca_certs(builder, extra_ca_certs)?;
    let client = builder
        .build()
        .context("failed to build OAuth HTTP client")?;

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

fn write_refreshed_credentials(
    creds: &StoredCredentials<'_>,
    response: &RefreshResponse,
) -> Result<()> {
    let mut json: serde_json::Value =
        serde_json::from_str(&creds.json).context("failed to parse credentials for update")?;

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
    match creds.source {
        Source::Keychain(keychain) => keychain.write(&serialized),
        Source::File(path) => atomic_write_private(path, serialized.as_bytes()),
    }
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
    for attempt in 0..=LOCK_MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
        }
        match std::fs::create_dir(path) {
            Ok(()) => {
                return Ok(LockGuard {
                    path: path.to_owned(),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale_lock(path)
                    && let Err(e) = std::fs::remove_dir_all(path)
                {
                    debug!("failed to clear stale lock {}: {e}", path.display());
                }
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to create lock at {}", path.display()));
            }
        }
    }
    bail!(
        "failed to acquire credentials lock after {LOCK_MAX_RETRIES} retries (another process may \
         be refreshing)"
    )
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
            async { load_token(None).await.unwrap() },
        )
        .await;
        assert_eq!(token, "token-from-home");
    }

    // ── load_token_from ──

    fn creds_json(access: &str, refresh: Option<&str>, expires_at: u64) -> serde_json::Value {
        let mut oauth = serde_json::json!({
            "accessToken": access,
            "expiresAt": expires_at,
        });
        if let Some(r) = refresh {
            oauth["refreshToken"] = r.into();
        }
        serde_json::json!({ "claudeAiOauth": oauth })
    }

    fn write_creds(path: &Path, access: &str, refresh: Option<&str>, expires_at: u64) {
        std::fs::write(path, creds_json(access, refresh, expires_at).to_string()).unwrap();
    }

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    async fn mock_refresh_server() -> MockServer {
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
        server
    }

    struct FakeKeychain {
        entry: Mutex<String>,
        writable: bool,
    }

    impl FakeKeychain {
        fn holding(entry: impl Into<String>) -> Self {
            Self {
                entry: Mutex::new(entry.into()),
                writable: true,
            }
        }

        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.entry.lock().unwrap()).unwrap()
        }
    }

    impl Keychain for FakeKeychain {
        fn read(&self) -> Result<String> {
            Ok(self.entry.lock().unwrap().clone())
        }

        fn write(&self, json: &str) -> Result<()> {
            if !self.writable {
                bail!("Keychain write denied");
            }
            json.clone_into(&mut self.entry.lock().unwrap());
            Ok(())
        }
    }

    #[tokio::test]
    async fn load_token_from_keeps_existing_when_far_from_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(&creds, "tok", Some("ref"), 9_999_999_999_999);

        let token = load_token_from(None, &creds, &lock, "http://should-not-be-called", None)
            .await
            .unwrap();
        assert_eq!(token, "tok");
    }

    #[tokio::test]
    async fn load_token_from_near_expiry_refreshes_and_persists_new_credentials() {
        let server = mock_refresh_server().await;

        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(
            &creds,
            "old",
            Some("old-refresh"),
            now_millis().unwrap() + 1_000,
        );

        let token = load_token_from(None, &creds, &lock, &server.uri(), None)
            .await
            .unwrap();
        assert_eq!(token, "fresh-access");

        let json = read_json(&creds);
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
    async fn load_token_from_keychain_refresh_writes_back_to_keychain_without_creating_file() {
        let server = mock_refresh_server().await;

        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        let keychain = FakeKeychain::holding(
            creds_json("old", Some("old-refresh"), now_millis().unwrap() + 1_000).to_string(),
        );

        let token = load_token_from(Some(&keychain), &creds, &lock, &server.uri(), None)
            .await
            .unwrap();
        assert_eq!(token, "fresh-access");

        let json = keychain.json();
        assert_eq!(json["claudeAiOauth"]["accessToken"], "fresh-access");
        assert_eq!(json["claudeAiOauth"]["refreshToken"], "fresh-refresh");
        assert!(!creds.exists(), "file must not be created");
    }

    #[tokio::test]
    async fn load_token_from_keychain_refresh_leaves_existing_file_untouched() {
        let server = mock_refresh_server().await;

        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        let mut file_json = creds_json("file-access", Some("file-refresh"), 0);
        file_json["claudeAiOauth"]["subscriptionType"] = "pro".into();
        std::fs::write(&creds, file_json.to_string()).unwrap();
        let mut keychain_json =
            creds_json("old", Some("old-refresh"), now_millis().unwrap() + 1_000);
        keychain_json["claudeAiOauth"]["subscriptionType"] = "max".into();
        let keychain = FakeKeychain::holding(keychain_json.to_string());

        let token = load_token_from(Some(&keychain), &creds, &lock, &server.uri(), None)
            .await
            .unwrap();
        assert_eq!(token, "fresh-access");

        let json = keychain.json();
        assert_eq!(json["claudeAiOauth"]["accessToken"], "fresh-access");
        assert_eq!(json["claudeAiOauth"]["subscriptionType"], "max");
        assert_eq!(read_json(&creds), file_json);
    }

    #[tokio::test]
    async fn load_token_from_unparsable_keychain_falls_back_to_file_and_refreshes_only_the_file() {
        let server = mock_refresh_server().await;

        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(
            &creds,
            "old",
            Some("old-refresh"),
            now_millis().unwrap() + 1_000,
        );
        let keychain = FakeKeychain::holding("not json");

        let token = load_token_from(Some(&keychain), &creds, &lock, &server.uri(), None)
            .await
            .unwrap();
        assert_eq!(token, "fresh-access");

        assert_eq!(
            read_json(&creds)["claudeAiOauth"]["accessToken"],
            "fresh-access"
        );
        assert_eq!(*keychain.entry.lock().unwrap(), "not json");
    }

    #[tokio::test]
    async fn load_token_from_write_back_failure_still_uses_refreshed_token() {
        let server = mock_refresh_server().await;

        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        let keychain = FakeKeychain {
            writable: false,
            ..FakeKeychain::holding(
                creds_json("old", Some("old-refresh"), now_millis().unwrap() + 1_000).to_string(),
            )
        };

        let token = load_token_from(Some(&keychain), &creds, &lock, &server.uri(), None)
            .await
            .unwrap();
        assert_eq!(token, "fresh-access");
    }

    #[tokio::test]
    async fn load_token_from_without_refresh_token_keeps_nonexpired_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(&creds, "tok", None, now_millis().unwrap() + 60_000);

        let token = load_token_from(None, &creds, &lock, "http://should-not-be-called", None)
            .await
            .unwrap();
        assert_eq!(token, "tok");
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

        let token = load_token_from(None, &creds, &lock, &server.uri(), None)
            .await
            .unwrap();
        assert_eq!(token, "stale");
    }

    #[tokio::test]
    async fn load_token_from_lock_held_keeps_existing_token_if_unexpired() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(&creds, "tok", Some("ref"), now_millis().unwrap() + 60_000);
        std::fs::create_dir(&lock).unwrap();

        let token = load_token_from(None, &creds, &lock, "http://should-not-be-called", None)
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

        let err = load_token_from(None, &creds, &lock, "http://unused", None)
            .await
            .expect_err("expired without refresh must bail");
        assert!(format!("{err:#}").contains("expired"));
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

        let err = load_token_from(None, &creds, &lock, &server.uri(), None)
            .await
            .expect_err("expired + refresh down must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("expired OAuth"), "wrapped context: {msg}");
    }

    #[tokio::test]
    async fn load_token_from_lock_held_bails_if_expired() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(&creds, "dead", Some("ref"), 0);
        std::fs::create_dir(&lock).unwrap();

        let err = load_token_from(None, &creds, &lock, "http://should-not-be-called", None)
            .await
            .expect_err("expired + lock held must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("credentials lock"), "{msg}");
    }

    #[tokio::test]
    async fn load_token_from_lock_held_picks_up_sibling_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let lock = dir.path().join("lock");
        write_creds(&creds, "dead", Some("ref"), 0);
        std::fs::create_dir(&lock).unwrap();

        let sibling_creds = creds.clone();
        let sibling = tokio::spawn(async move {
            tokio::time::sleep(LOCK_RETRY_INTERVAL * 2).await;
            write_creds(
                &sibling_creds,
                "sibling",
                Some("ref"),
                now_millis().unwrap() + 60 * 60 * 1000,
            );
        });

        let token = load_token_from(None, &creds, &lock, "http://should-not-be-called", None)
            .await
            .unwrap();
        sibling.await.unwrap();
        assert_eq!(token, "sibling");
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
        assert_eq!(creds.oauth.access_token, "tok");
        assert_eq!(creds.oauth.refresh_token.as_deref(), Some("ref"));
        assert_eq!(creds.oauth.expires_at, 9_999_999_999_999);
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
        assert!(creds.oauth.refresh_token.is_none());
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

        let response = refresh_oauth_token(&server.uri(), "old-refresh", None)
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

        let err = refresh_oauth_token(&server.uri(), "bad", None)
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

        let err = refresh_oauth_token(&server.uri(), "tok", None)
            .await
            .expect_err("expected parse error");
        assert!(format!("{err:#}").contains("parse"));
    }

    #[tokio::test]
    async fn refresh_oauth_token_honors_extra_ca_certs() {
        // A valid PEM bundle must thread through the client builder without disrupting the
        // existing request flow. Pairs with the surfaces-loader-error test below.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_refresh_body(
                "new-access",
                "new-refresh",
                3600,
            )))
            .mount(&server)
            .await;

        let pem = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut pem.as_file(), crate::util::tls::TEST_CA_PEM.as_bytes())
            .unwrap();

        let response = refresh_oauth_token(&server.uri(), "refresh", Some(pem.path()))
            .await
            .expect("valid PEM must build a client and produce a response");
        assert_eq!(response.access_token, "new-access");
    }

    #[tokio::test]
    async fn refresh_oauth_token_surfaces_extra_ca_certs_error_with_path() {
        // An unreadable bundle must fail before any network call, citing the path so the user
        // can debug the configuration rather than chase a TLS error downstream.
        let missing = Path::new("/definitely/does/not/exist.pem");
        let err = refresh_oauth_token("http://unused.invalid/", "refresh", Some(missing))
            .await
            .expect_err("missing CA bundle must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to read extra CA bundle"), "{msg}");
        assert!(msg.contains("/definitely/does/not/exist.pem"), "{msg}");
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

        write_refreshed_credentials(&read_credentials(&path).unwrap(), &response).unwrap();

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

        write_refreshed_credentials(&read_credentials(&path).unwrap(), &response).unwrap();

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

        write_refreshed_credentials(&read_credentials(&path).unwrap(), &response).unwrap();

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

    #[tokio::test]
    async fn acquire_lock_clears_stale_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");
        std::fs::create_dir(&lock_path).unwrap();
        let abandoned_at =
            std::time::SystemTime::now() - LOCK_STALE_THRESHOLD - Duration::from_secs(1);
        std::fs::File::open(&lock_path)
            .unwrap()
            .set_modified(abandoned_at)
            .unwrap();

        let _guard = acquire_lock(&lock_path).await.unwrap();
        assert!(!is_stale_lock(&lock_path));
    }

    #[tokio::test]
    async fn acquire_lock_surfaces_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("missing").join("test.lock");

        let err = acquire_lock(&lock_path)
            .await
            .err()
            .expect("missing parent must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to create lock"), "{msg}");
    }
}
