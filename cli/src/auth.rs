//! Credentials for talking to a session server: where they live, how they are
//! read and written, and how an expired access token is refreshed.
//!
//! One file holds every server the user has logged into, keyed by normalised
//! URL, so `--server` picks the right credential without further configuration.

use crate::config::HorsieConfig;
use crate::error::CliError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Refresh this long before the access token actually expires, so a token does
/// not die between the check and the request it was attached to.
pub const EXPIRY_MARGIN_SECS: i64 = 60;

/// Environment override, for scripts and CI: skips the credential file
/// entirely.
pub const TOKEN_ENV: &str = "HORSIE_TOKEN";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerCredentials {
    pub access_token: String,
    /// Empty for a pasted token, which has no refresh half.
    pub refresh_token: String,
    /// Unix epoch seconds at which `access_token` stops working.
    pub expires_at: i64,
}

impl ServerCredentials {
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at - EXPIRY_MARGIN_SECS
    }
}

/// Every server this machine has credentials for. `BTreeMap` so the file has a
/// stable order and diffs cleanly if a human ever looks at it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub servers: BTreeMap<String, ServerCredentials>,
}

impl Credentials {
    /// A missing file is an empty set: not being logged in anywhere is a normal
    /// state, not an error.
    pub fn load(path: &Path) -> Result<Self, CliError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| CliError::Config(format!("parse {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(CliError::Io(format!("read {}: {e}", path.display()))),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), CliError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CliError::Io(format!("create {}: {e}", parent.display())))?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| CliError::Config(format!("serialize credentials: {e}")))?;
        std::fs::write(path, format!("{text}\n"))
            .map_err(|e| CliError::Io(format!("write {}: {e}", path.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| CliError::Io(format!("chmod {}: {e}", path.display())))?;
        }
        Ok(())
    }

    pub fn get(&self, server: &str) -> Option<&ServerCredentials> {
        self.servers.get(&normalize_server(server))
    }

    pub fn set(&mut self, server: &str, creds: ServerCredentials) {
        self.servers.insert(normalize_server(server), creds);
    }

    pub fn remove(&mut self, server: &str) -> Option<ServerCredentials> {
        self.servers.remove(&normalize_server(server))
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}

/// `<config-dir>/horsie/credentials.json`, beside the CLI's other state.
pub fn credentials_path() -> PathBuf {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => match std::env::var_os("HOME") {
            Some(h) if !h.is_empty() => PathBuf::from(h).join(".config"),
            _ => PathBuf::from(".horsie"),
        },
    };
    base.join("horsie").join("credentials.json")
}

/// Scheme and host lowercased, trailing slash dropped. Without this,
/// `http://Localhost:3789/` and `http://localhost:3789` would be two entries
/// and logging in through one would not satisfy the other.
pub fn normalize_server(server: &str) -> String {
    let trimmed = server.trim().trim_end_matches('/');
    match trimmed.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = match rest.split_once('/') {
                Some((h, p)) => (h, Some(p)),
                None => (rest, None),
            };
            let base = format!(
                "{}://{}",
                scheme.to_ascii_lowercase(),
                host.to_ascii_lowercase()
            );
            match path {
                Some(p) => format!("{base}/{p}"),
                None => base,
            }
        }
        None => trimmed.to_ascii_lowercase(),
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The subset of the server's device-flow responses the CLI reads. Declared
/// here rather than taken from `horsie_models` so the CLI does not depend on
/// the server's whole wire surface for four fields.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u32,
    interval: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenPair {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    code: String,
    #[serde(default)]
    message: String,
}

fn api_url(server: &str, path: &str) -> String {
    format!("{}{path}", normalize_server(server))
}

/// POST a JSON body and read the server's error envelope on failure.
async fn post_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<Result<T, ApiErrorBody>, CliError> {
    let res = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| CliError::Server(format!("{url}: {e}")))?;
    if res.status().is_success() {
        let parsed = res
            .json::<T>()
            .await
            .map_err(|e| CliError::Server(format!("{url}: unexpected response: {e}")))?;
        return Ok(Ok(parsed));
    }
    let status = res.status();
    let err = res.json::<ApiErrorBody>().await.unwrap_or(ApiErrorBody {
        code: format!("http_{}", status.as_u16()),
        message: status.to_string(),
    });
    Ok(Err(err))
}

/// Whether a login should become the configured default: the user asked with
/// `--default`, or this is the machine's first stored credential (nothing to
/// displace, so the first server is the natural default).
pub fn should_default(creds_before: &Credentials, default_flag: bool) -> bool {
    default_flag || creds_before.is_empty()
}

/// `horsie auth login`. With `token`, validate and store a pasted credential;
/// otherwise run the device flow to completion. The server becomes the
/// configured default when `default` is set, or when it is the first
/// credential stored.
pub async fn login(server: &str, token: Option<&str>, default: bool) -> Result<(), CliError> {
    let path = credentials_path();
    let client = reqwest::Client::new();

    if let Some(token) = token {
        validate_token(&client, server, token).await?;
        let mut creds = Credentials::load(&path)?;
        let is_default = should_default(&creds, default);
        creds.set(
            server,
            ServerCredentials {
                access_token: token.to_string(),
                // A pasted token has no refresh half; when it stops working the
                // user pastes another. No expiry we could act on, so never
                // treat it as stale.
                refresh_token: String::new(),
                expires_at: i64::MAX,
            },
        );
        creds.save(&path)?;
        println!("stored a token for {}", normalize_server(server));
        if is_default {
            crate::config::set_default_server(server, None)?;
            println!("{} is now your default server", normalize_server(server));
        }
        return Ok(());
    }

    let start: DeviceCode = post_json(
        &client,
        &api_url(server, "/api/auth/device/code"),
        &serde_json::json!({}),
    )
    .await?
    .map_err(|e| CliError::Server(format!("starting the login: {}", e.message)))?;

    println!("To authorize this machine, open:\n");
    println!("    {}\n", start.verification_uri_complete);
    println!("and confirm the code:  {}\n", start.user_code);
    println!(
        "(If the link does not open, go to {} and type the code.)",
        start.verification_uri
    );

    let deadline = now_secs() + i64::from(start.expires_in);
    // The server's `interval` is a floor, and it answers `slow_down` if we
    // ignore it — so back off on that rather than hammering.
    let mut interval = u64::from(start.interval);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        if now_secs() >= deadline {
            return Err(CliError::Server(
                "the code expired before it was approved; run `horsie auth login` again".into(),
            ));
        }
        let polled: Result<TokenPair, ApiErrorBody> = post_json(
            &client,
            &api_url(server, "/api/auth/device/token"),
            &serde_json::json!({ "deviceCode": start.device_code }),
        )
        .await?;
        match polled {
            Ok(pair) => {
                let mut creds = Credentials::load(&path)?;
                let is_default = should_default(&creds, default);
                creds.set(
                    server,
                    ServerCredentials {
                        access_token: pair.access_token,
                        refresh_token: pair.refresh_token,
                        expires_at: now_secs() + pair.expires_in,
                    },
                );
                creds.save(&path)?;
                println!("\nLogged in to {}.", normalize_server(server));
                if is_default {
                    crate::config::set_default_server(server, None)?;
                    println!("{} is now your default server", normalize_server(server));
                }
                return Ok(());
            }
            Err(e) => match e.code.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval = interval.saturating_add(5),
                "access_denied" => {
                    return Err(CliError::Server("that login was denied".into()));
                }
                _ => {
                    return Err(CliError::Server(format!(
                        "login failed: {} ({})",
                        e.message, e.code
                    )));
                }
            },
        }
    }
}

/// Confirm a pasted token actually authenticates before storing it — otherwise
/// the first failure would surface much later, somewhere unrelated.
async fn validate_token(
    client: &reqwest::Client,
    server: &str,
    token: &str,
) -> Result<(), CliError> {
    let url = api_url(server, "/api/auth/status");
    let res = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CliError::Server(format!("{url}: {e}")))?;
    let status: serde_json::Value = res
        .json()
        .await
        .map_err(|e| CliError::Server(format!("{url}: unexpected response: {e}")))?;
    if status.get("authenticated").and_then(|v| v.as_bool()) == Some(true) {
        Ok(())
    } else if status.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        Err(CliError::Server(
            "that server has authentication disabled, so it needs no token".into(),
        ))
    } else {
        Err(CliError::Server("that token was not accepted".into()))
    }
}

/// `horsie auth logout`. Revocation is best-effort: forgetting the credential
/// locally is the part the user asked for, and a server that cannot be reached
/// must not leave a dead entry in the file.
pub async fn logout(server: Option<&str>) -> Result<(), CliError> {
    let path = credentials_path();
    let mut creds = Credentials::load(&path)?;
    let targets: Vec<String> = match server {
        Some(s) => vec![normalize_server(s)],
        None => creds.servers.keys().cloned().collect(),
    };
    if targets.is_empty() {
        println!("not logged in to anything");
        return Ok(());
    }
    let client = reqwest::Client::new();
    for target in targets {
        if let Some(c) = creds.remove(&target) {
            let url = api_url(&target, "/api/auth/logout");
            match client.post(&url).bearer_auth(&c.access_token).send().await {
                Ok(_) => println!("logged out of {target}"),
                Err(e) => println!("forgot {target} locally (could not reach it: {e})"),
            }
        } else {
            println!("not logged in to {target}");
        }
    }
    creds.save(&path)?;
    Ok(())
}

/// `horsie auth status`.
pub fn status() -> Result<(), CliError> {
    let path = credentials_path();
    let creds = Credentials::load(&path)?;
    if creds.is_empty() {
        println!("not logged in to any server");
        println!("run `horsie auth login --server <url>` to log in");
        return Ok(());
    }
    let now = now_secs();
    println!("credentials in {}\n", path.display());
    for (server, c) in &creds.servers {
        let state = if c.refresh_token.is_empty() {
            "pasted token".to_string()
        } else if c.is_expired(now) {
            "access token expired (will refresh on next use)".to_string()
        } else {
            format!("valid for {}m", (c.expires_at - now) / 60)
        };
        println!("  {server}  —  {state}{}", default_marker(server));
    }
    Ok(())
}

/// `(default)` suffix for the row matching the configured default server,
/// normalized so `https://Auth.Horsie.dev/` matches `https://auth.horsie.dev`.
/// Reads fail open: an unreadable config marks nothing.
fn default_marker(server: &str) -> String {
    let configured = match HorsieConfig::resolve(None) {
        Ok(cfg) => cfg.default_server,
        Err(_) => None,
    };
    marker_for(server, configured.as_deref())
}

fn marker_for(server: &str, configured_default: Option<&str>) -> String {
    let configured = configured_default.map(normalize_server);
    let server = normalize_server(server);
    if configured.as_deref() == Some(server.as_str()) {
        "  (default)".to_string()
    } else {
        String::new()
    }
}

/// Whether `server` requires a credential. Used for a pre-flight check: an
/// agent that dials without one gets a 401 it will retry forever, and "run
/// `horsie auth login`" is far more useful than a backoff loop.
///
/// Fails **open**. This is a courtesy check, not an authorization decision —
/// anything unexpected (unreachable, a non-JSON body, an older server with no
/// such endpoint) answers `false` so the dial proceeds and the server itself
/// decides. A probe that can block a working setup is worse than no probe.
pub async fn server_requires_auth(server: &str) -> bool {
    let Ok(res) = reqwest::Client::new()
        .get(api_url(server, "/api/auth/status"))
        .send()
        .await
    else {
        return false;
    };
    let Ok(status) = res.json::<serde_json::Value>().await else {
        return false;
    };
    status
        .get("enabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// The bearer to send to `server`, refreshing a stale access token first.
/// `None` means "no credential configured", which callers report as a prompt to
/// log in rather than as a failure.
pub async fn resolve_token(server: &str) -> Result<Option<String>, CliError> {
    resolve_token_with(server, &credentials_path(), std::env::var(TOKEN_ENV).ok()).await
}

async fn resolve_token_with(
    server: &str,
    path: &Path,
    env_token: Option<String>,
) -> Result<Option<String>, CliError> {
    if let Some(t) = env_token.filter(|t| !t.is_empty()) {
        return Ok(Some(t));
    }
    let mut creds = Credentials::load(path)?;
    let Some(current) = creds.get(server).cloned() else {
        return Ok(None);
    };
    if !current.is_expired(now_secs()) || current.refresh_token.is_empty() {
        return Ok(Some(current.access_token));
    }

    let client = reqwest::Client::new();
    let refreshed: Result<TokenPair, ApiErrorBody> = post_json(
        &client,
        &api_url(server, "/api/auth/refresh"),
        &serde_json::json!({ "refreshToken": current.refresh_token }),
    )
    .await?;
    match refreshed {
        Ok(pair) => {
            let updated = ServerCredentials {
                access_token: pair.access_token,
                refresh_token: pair.refresh_token,
                expires_at: now_secs() + pair.expires_in,
            };
            creds.set(server, updated.clone());
            creds.save(path)?;
            Ok(Some(updated.access_token))
        }
        Err(_) => {
            // The refresh token is dead (rotated away, revoked, or expired).
            // Drop it so the next run says "log in" instead of retrying a
            // credential that can never work again.
            creds.remove(server);
            creds.save(path)?;
            Err(CliError::Server(format!(
                "the stored login for {} is no longer valid — run `horsie auth login --server {}`",
                normalize_server(server),
                normalize_server(server)
            )))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn server_urls_normalize_so_one_server_is_one_entry() {
        assert_eq!(
            normalize_server("http://Localhost:3789/"),
            "http://localhost:3789"
        );
        assert_eq!(
            normalize_server("http://localhost:3789"),
            "http://localhost:3789"
        );
        assert_eq!(
            normalize_server("HTTPS://Horsie.Example.COM/"),
            "https://horsie.example.com"
        );
        // A path is kept: someone may host horsie under a prefix.
        assert_eq!(
            normalize_server("https://x.com/horsie/"),
            "https://x.com/horsie"
        );
    }

    #[test]
    fn credentials_round_trip_through_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials.json");

        let mut creds = Credentials::default();
        assert!(creds.get("http://localhost:3789").is_none());

        creds.set(
            "http://localhost:3789/",
            ServerCredentials {
                access_token: "hsk_usr_a".into(),
                refresh_token: "hsk_ref_r".into(),
                expires_at: 1000,
            },
        );
        creds.save(&path).unwrap();

        let back = Credentials::load(&path).unwrap();
        let c = back
            .get("http://localhost:3789")
            .expect("normalized lookup");
        assert_eq!(c.access_token, "hsk_usr_a");
        assert_eq!(c.expires_at, 1000);

        let mut back = back;
        assert!(back.remove("http://nope").is_none());
        assert!(back.remove("http://localhost:3789").is_some());
        assert!(back.get("http://localhost:3789").is_none());
    }

    #[test]
    fn a_missing_file_is_an_empty_set_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let creds = Credentials::load(&tmp.path().join("absent.json")).unwrap();
        assert!(creds.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_credential_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("credentials.json");
        let mut creds = Credentials::default();
        creds.set(
            "http://x",
            ServerCredentials {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_at: 0,
            },
        );
        creds.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    #[test]
    fn an_access_token_near_expiry_counts_as_expired() {
        let c = ServerCredentials {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 1000,
        };
        // Fresh with room to spare.
        assert!(!c.is_expired(900));
        // Inside the safety margin: treat as expired rather than send a token
        // that dies in flight.
        assert!(c.is_expired(1000 - EXPIRY_MARGIN_SECS + 1));
        assert!(c.is_expired(2000));
    }

    #[tokio::test]
    async fn resolve_token_prefers_the_environment_override() {
        // The env override exists precisely so scripts need no credential file.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials.json");
        let token = resolve_token_with(
            "http://localhost:3789",
            &path,
            Some("hsk_usr_from_env".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(token.as_deref(), Some("hsk_usr_from_env"));
    }

    #[tokio::test]
    async fn resolve_token_returns_a_live_stored_token_without_refreshing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials.json");
        let mut creds = Credentials::default();
        creds.set(
            "http://localhost:3789",
            ServerCredentials {
                access_token: "hsk_usr_live".into(),
                refresh_token: "hsk_ref_r".into(),
                // Comfortably in the future, so no network call is attempted —
                // which is what makes this test hermetic.
                expires_at: now_secs() + 3600,
            },
        );
        creds.save(&path).unwrap();

        let token = resolve_token_with("http://localhost:3789", &path, None)
            .await
            .unwrap();
        assert_eq!(token.as_deref(), Some("hsk_usr_live"));
    }

    #[tokio::test]
    async fn resolve_token_is_none_when_the_server_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials.json");
        assert!(
            resolve_token_with("http://elsewhere", &path, None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn first_login_defaults_without_the_flag() {
        assert!(should_default(&Credentials::default(), false));
    }

    #[test]
    fn later_login_defaults_only_with_the_flag() {
        let mut creds = Credentials::default();
        creds.set(
            "http://x",
            ServerCredentials {
                access_token: "a".into(),
                refresh_token: String::new(),
                expires_at: 0,
            },
        );
        assert!(!should_default(&creds, false));
        assert!(should_default(&creds, true));
    }

    #[test]
    fn marker_marks_the_default_server_only() {
        assert_eq!(
            marker_for("http://localhost:3789", Some("http://localhost:3789")),
            "  (default)"
        );
        assert_eq!(
            marker_for("http://localhost:3789", Some("http://localhost:3789/")),
            "  (default)"
        );
        assert_eq!(
            marker_for("http://localhost:3789", Some("https://other.dev")),
            ""
        );
        assert_eq!(marker_for("http://localhost:3789", None), "");
    }
}
