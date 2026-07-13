//! SQLite storage for configured remote MCP servers (`mcp_servers`). One row
//! per server, keyed by `name`. A bearer secret is stored plaintext (the DB
//! file is the trust boundary) and wrapped in [`Secret`] in memory; write-only
//! inputs follow the settings store's keep/clear/set convention (`None` keeps,
//! `""` clears, a value sets). `github_app` servers store no token — it is
//! minted from the GitHub App connection at use time.

use horsie_agentcore::Secret;
use horsie_models::mcp::{McpAuthInput, McpOAuthInput, McpServerInput};
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};
use std::time::{SystemTime, UNIX_EPOCH};

/// How horsie authenticates to a server, as stored (the bearer secret rides
/// along for the `Bearer` variant).
#[derive(Debug, Clone, PartialEq)]
pub enum StoredAuth {
    /// Public server, no credentials.
    None,
    /// Static bearer token (absent until set).
    Bearer(Option<Secret>),
    /// OAuth 2.1: the (optionally DCR-registered) client, the current token
    /// pair, and a JSON cache of the discovered AS endpoints.
    Oauth(OauthState),
    /// GitHub MCP over the existing App connection; token minted at use.
    GithubApp,
}

/// OAuth 2.1 state as stored (see [`StoredAuth::Oauth`]).
#[derive(Debug, Clone, PartialEq)]
pub struct OauthState {
    pub client_id: Option<String>,
    pub client_secret: Option<Secret>,
    pub access_token: Option<Secret>,
    pub refresh_token: Option<Secret>,
    pub expires_at: Option<String>,
    pub meta: Option<String>,
}

impl StoredAuth {
    /// The `auth_kind` discriminant persisted in the row.
    fn kind(&self) -> &'static str {
        match self {
            StoredAuth::None => "none",
            StoredAuth::Bearer(_) => "bearer",
            StoredAuth::Oauth(_) => "oauth",
            StoredAuth::GithubApp => "github_app",
        }
    }

    /// The bearer secret to persist, if any. OAuth's bearer is minted at use,
    /// not stored in `bearer_token`.
    fn bearer(&self) -> Option<&Secret> {
        match self {
            StoredAuth::Bearer(s) => s.as_ref(),
            StoredAuth::None | StoredAuth::GithubApp | StoredAuth::Oauth(_) => None,
        }
    }
}

/// One configured MCP server row.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerRow {
    pub name: String,
    pub url: String,
    pub enabled: bool,
    pub auth: StoredAuth,
    pub tool_count: Option<u32>,
    pub last_error: Option<String>,
}

pub struct McpStore {
    pool: SqlitePool,
}

impl McpStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// All configured servers, ordered by name.
    pub async fn list(&self) -> Result<Vec<McpServerRow>, String> {
        let rows = sqlx::query(
            "SELECT name, url, enabled, auth_kind, bearer_token, \
             oauth_client_id, oauth_client_secret, oauth_access_token, oauth_refresh_token, oauth_expires_at, oauth_meta, \
             tool_count, last_error \
             FROM mcp_servers ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_server).collect()
    }

    /// One server by name.
    pub async fn get(&self, name: &str) -> Result<Option<McpServerRow>, String> {
        let row = sqlx::query(
            "SELECT name, url, enabled, auth_kind, bearer_token, \
             oauth_client_id, oauth_client_secret, oauth_access_token, oauth_refresh_token, oauth_expires_at, oauth_meta, \
             tool_count, last_error \
             FROM mcp_servers WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_server).transpose()
    }

    /// Upsert from a wire input. Editing config re-arms the server (clears the
    /// last test result), so a `Test` is required before it is usable again.
    /// The bearer secret honors keep/clear/set against the existing row.
    pub async fn upsert(&self, input: &McpServerInput) -> Result<McpServerRow, String> {
        let name = input.name.trim();
        if name.is_empty() {
            return Err("MCP server name cannot be empty".into());
        }
        let url = input.url.trim();
        if url.is_empty() {
            return Err("MCP server url cannot be empty".into());
        }
        let existing = self.get(name).await?;
        let auth = auth_from_input(&input.auth, existing.as_ref());
        let now = now_secs().to_string();
        sqlx::query(
            "INSERT INTO mcp_servers \
             (name, url, enabled, auth_kind, bearer_token, \
              oauth_client_id, oauth_client_secret, oauth_access_token, oauth_refresh_token, oauth_expires_at, oauth_meta, \
              tool_count, last_error, created_at, updated_at) \
             VALUES (?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?) \
             ON CONFLICT(name) DO UPDATE SET \
             url = excluded.url, auth_kind = excluded.auth_kind, \
             bearer_token = excluded.bearer_token, \
             oauth_client_id = excluded.oauth_client_id, \
             oauth_client_secret = excluded.oauth_client_secret, \
             oauth_access_token = excluded.oauth_access_token, \
             oauth_refresh_token = excluded.oauth_refresh_token, \
             oauth_expires_at = excluded.oauth_expires_at, \
             oauth_meta = excluded.oauth_meta, \
             enabled = 0, tool_count = NULL, last_error = NULL, updated_at = excluded.updated_at",
        )
        .bind(name)
        .bind(url)
        .bind(auth.kind())
        .bind(auth.bearer().map(|s| s.expose().to_string()))
        .bind(oauth_field(&auth, |o| o.client_id.clone()))
        .bind(oauth_secret(&auth, |o| o.client_secret.clone()))
        .bind(oauth_secret(&auth, |o| o.access_token.clone()))
        .bind(oauth_secret(&auth, |o| o.refresh_token.clone()))
        .bind(oauth_field(&auth, |o| o.expires_at.clone()))
        .bind(oauth_field(&auth, |o| o.meta.clone()))
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        self.get(name)
            .await?
            .ok_or_else(|| "mcp server missing after upsert".to_string())
    }

    pub async fn delete(&self, name: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM mcp_servers WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Record the outcome of a connect/test: usability, tool count, and the last
    /// error (mutually: `enabled` ⇒ `last_error` cleared).
    pub async fn set_status(
        &self,
        name: &str,
        enabled: bool,
        tool_count: Option<u32>,
        last_error: Option<&str>,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE mcp_servers SET enabled = ?, tool_count = ?, last_error = ?, updated_at = ? \
             WHERE name = ?",
        )
        .bind(i64::from(enabled))
        .bind(tool_count.map(i64::from))
        .bind(last_error)
        .bind(now_secs().to_string())
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Persist the (discovered/registered) client and the AS endpoint metadata.
    /// Does not touch tokens or the enabled/test status.
    pub async fn save_oauth_client(
        &self,
        name: &str,
        client_id: &str,
        client_secret: Option<&str>,
        meta: &str,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE mcp_servers SET oauth_client_id = ?, oauth_client_secret = ?, oauth_meta = ?, updated_at = ? \
             WHERE name = ?",
        )
        .bind(client_id)
        .bind(client_secret)
        .bind(meta)
        .bind(now_secs().to_string())
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Persist a fresh token pair (after a code exchange or refresh). A `None`
    /// refresh token keeps the existing one (some AS only rotate sometimes).
    pub async fn save_oauth_tokens(
        &self,
        name: &str,
        access: &str,
        refresh: Option<&str>,
        expires_at: Option<&str>,
    ) -> Result<(), String> {
        match refresh {
            Some(rt) => {
                sqlx::query(
                    "UPDATE mcp_servers SET oauth_access_token = ?, oauth_refresh_token = ?, oauth_expires_at = ?, updated_at = ? WHERE name = ?",
                )
                .bind(access)
                .bind(rt)
                .bind(expires_at)
                .bind(now_secs().to_string())
                .bind(name)
                .execute(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
            }
            None => {
                sqlx::query(
                    "UPDATE mcp_servers SET oauth_access_token = ?, oauth_expires_at = ?, updated_at = ? WHERE name = ?",
                )
                .bind(access)
                .bind(expires_at)
                .bind(now_secs().to_string())
                .bind(name)
                .execute(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

/// Build the stored auth for an upsert, resolving secrets against the existing
/// row (keep/clear/set). OAuth keeps its tokens and discovered metadata across a
/// user re-upsert; only client creds / manual endpoints come from the input.
fn auth_from_input(input: &McpAuthInput, existing: Option<&McpServerRow>) -> StoredAuth {
    match input {
        McpAuthInput::None(_) => StoredAuth::None,
        McpAuthInput::GithubApp(_) => StoredAuth::GithubApp,
        McpAuthInput::Bearer(b) => {
            let prior = existing.and_then(|r| match &r.auth {
                StoredAuth::Bearer(s) => s.as_ref(),
                StoredAuth::None | StoredAuth::GithubApp | StoredAuth::Oauth(_) => None,
            });
            StoredAuth::Bearer(resolve_secret(&b.token, prior))
        }
        McpAuthInput::OAuth(o) => {
            let prior = existing.and_then(|r| match &r.auth {
                StoredAuth::Oauth(s) => Some(s.clone()),
                StoredAuth::None | StoredAuth::Bearer(_) | StoredAuth::GithubApp => None,
            });
            let manual_meta = manual_meta_json(o);
            StoredAuth::Oauth(OauthState {
                client_id: o
                    .client_id
                    .clone()
                    .or_else(|| prior.as_ref().and_then(|p| p.client_id.clone())),
                client_secret: resolve_secret(
                    &o.client_secret,
                    prior.as_ref().and_then(|p| p.client_secret.as_ref()),
                ),
                access_token: prior.as_ref().and_then(|p| p.access_token.clone()),
                refresh_token: prior.as_ref().and_then(|p| p.refresh_token.clone()),
                expires_at: prior.as_ref().and_then(|p| p.expires_at.clone()),
                meta: manual_meta.or_else(|| prior.and_then(|p| p.meta)),
            })
        }
    }
}

/// Build an `oauth_meta` JSON from manually-entered endpoints, or `None` if the
/// user entered none (discovery will fill it in).
fn manual_meta_json(o: &McpOAuthInput) -> Option<String> {
    if o.authorization_endpoint.is_none()
        && o.token_endpoint.is_none()
        && o.registration_endpoint.is_none()
    {
        return None;
    }
    let meta = serde_json::json!({
        "authorization_endpoint": o.authorization_endpoint,
        "token_endpoint": o.token_endpoint,
        "registration_endpoint": o.registration_endpoint,
    });
    Some(meta.to_string())
}

/// A plain oauth text column for the `Oauth` variant, else `None`.
fn oauth_field(auth: &StoredAuth, pick: impl Fn(&OauthState) -> Option<String>) -> Option<String> {
    match auth {
        StoredAuth::Oauth(o) => pick(o),
        StoredAuth::None | StoredAuth::Bearer(_) | StoredAuth::GithubApp => None,
    }
}

/// An oauth secret column (exposed for storage) for the `Oauth` variant.
fn oauth_secret(auth: &StoredAuth, pick: impl Fn(&OauthState) -> Option<Secret>) -> Option<String> {
    match auth {
        StoredAuth::Oauth(o) => pick(o).map(|s| s.expose().to_string()),
        StoredAuth::None | StoredAuth::Bearer(_) | StoredAuth::GithubApp => None,
    }
}

fn row_to_server(row: &SqliteRow) -> Result<McpServerRow, String> {
    let auth_kind: String = row.try_get("auth_kind").map_err(|e| e.to_string())?;
    let auth = match auth_kind.as_str() {
        "none" => StoredAuth::None,
        "github_app" => StoredAuth::GithubApp,
        "bearer" => StoredAuth::Bearer(opt_secret(row, "bearer_token")?),
        "oauth" => StoredAuth::Oauth(OauthState {
            client_id: opt_string(row, "oauth_client_id")?,
            client_secret: opt_secret(row, "oauth_client_secret")?,
            access_token: opt_secret(row, "oauth_access_token")?,
            refresh_token: opt_secret(row, "oauth_refresh_token")?,
            expires_at: opt_string(row, "oauth_expires_at")?,
            meta: opt_string(row, "oauth_meta")?,
        }),
        other => return Err(format!("unknown mcp auth_kind '{other}'")),
    };
    Ok(McpServerRow {
        name: row.try_get("name").map_err(|e| e.to_string())?,
        url: row.try_get("url").map_err(|e| e.to_string())?,
        enabled: row
            .try_get::<i64, _>("enabled")
            .map_err(|e| e.to_string())?
            != 0,
        auth,
        tool_count: opt_u32(row, "tool_count")?,
        last_error: opt_string(row, "last_error")?,
    })
}

fn opt_string(row: &SqliteRow, col: &str) -> Result<Option<String>, String> {
    row.try_get::<Option<String>, _>(col)
        .map_err(|e| e.to_string())
}

/// A stored secret column, treating an empty string as absent.
fn opt_secret(row: &SqliteRow, col: &str) -> Result<Option<Secret>, String> {
    Ok(opt_string(row, col)?
        .filter(|s| !s.is_empty())
        .map(Secret::from))
}

/// A `u32` column round-tripped through SQLite's signed `INTEGER`.
fn opt_u32(row: &SqliteRow, col: &str) -> Result<Option<u32>, String> {
    let v = row
        .try_get::<Option<i64>, _>(col)
        .map_err(|e| e.to_string())?;
    Ok(v.and_then(|n| u32::try_from(n).ok()))
}

/// Write-only secret input: `None` keeps the stored value, `Some("")` clears,
/// `Some(v)` sets.
fn resolve_secret(input: &Option<String>, existing: Option<&Secret>) -> Option<Secret> {
    match input {
        None => existing.cloned(),
        Some(v) if !v.is_empty() => Some(Secret::from(v.as_str())),
        Some(_) => None,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use horsie_models::mcp::{McpBearerInput, McpGithubAppAuth, McpNoAuth};
    use std::str::FromStr;

    async fn store() -> (McpStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (McpStore::new(pool), tmp)
    }

    fn bearer_input(name: &str, token: Option<&str>) -> McpServerInput {
        McpServerInput {
            name: name.into(),
            url: "https://mcp.example/".into(),
            auth: McpAuthInput::Bearer(McpBearerInput {
                token: token.map(str::to_string),
            }),
        }
    }

    #[tokio::test]
    async fn upsert_get_list_delete_round_trip() {
        let (s, _t) = store().await;
        assert!(s.list().await.unwrap().is_empty());
        assert!(s.get("a").await.unwrap().is_none());

        let row = s.upsert(&bearer_input("a", Some("tok"))).await.unwrap();
        assert_eq!(row.url, "https://mcp.example/");
        assert!(!row.enabled);
        assert_eq!(row.auth, StoredAuth::Bearer(Some(Secret::from("tok"))));

        s.upsert(&McpServerInput {
            name: "b".into(),
            url: "https://b.example/".into(),
            auth: McpAuthInput::None(McpNoAuth {}),
        })
        .await
        .unwrap();
        let names: Vec<String> = s
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["a", "b"]);

        s.delete("a").await.unwrap();
        assert!(s.get("a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bearer_secret_keeps_clears_sets() {
        let (s, _t) = store().await;
        s.upsert(&bearer_input("a", Some("tok"))).await.unwrap();
        // Omit → keep.
        let row = s.upsert(&bearer_input("a", None)).await.unwrap();
        assert_eq!(row.auth, StoredAuth::Bearer(Some(Secret::from("tok"))));
        // "" → clear.
        let row = s.upsert(&bearer_input("a", Some(""))).await.unwrap();
        assert_eq!(row.auth, StoredAuth::Bearer(None));
        // value → set.
        let row = s.upsert(&bearer_input("a", Some("new"))).await.unwrap();
        assert_eq!(row.auth, StoredAuth::Bearer(Some(Secret::from("new"))));
    }

    #[tokio::test]
    async fn switching_auth_kind_drops_bearer() {
        let (s, _t) = store().await;
        s.upsert(&bearer_input("a", Some("tok"))).await.unwrap();
        let row = s
            .upsert(&McpServerInput {
                name: "a".into(),
                url: "https://mcp.example/".into(),
                auth: McpAuthInput::GithubApp(McpGithubAppAuth {}),
            })
            .await
            .unwrap();
        assert_eq!(row.auth, StoredAuth::GithubApp);
    }

    #[tokio::test]
    async fn set_status_records_test_outcome_and_upsert_rearms() {
        let (s, _t) = store().await;
        s.upsert(&bearer_input("a", Some("tok"))).await.unwrap();
        s.set_status("a", true, Some(5), None).await.unwrap();
        let row = s.get("a").await.unwrap().unwrap();
        assert!(row.enabled);
        assert_eq!(row.tool_count, Some(5));
        assert!(row.last_error.is_none());

        // Editing re-arms: enabled/tool_count/last_error reset.
        let row = s.upsert(&bearer_input("a", None)).await.unwrap();
        assert!(!row.enabled);
        assert_eq!(row.tool_count, None);

        s.set_status("a", false, None, Some("boom")).await.unwrap();
        let row = s.get("a").await.unwrap().unwrap();
        assert_eq!(row.last_error.as_deref(), Some("boom"));
    }

    fn oauth_input(name: &str, client_id: Option<&str>, secret: Option<&str>) -> McpServerInput {
        use horsie_models::mcp::McpOAuthInput;
        McpServerInput {
            name: name.into(),
            url: "https://mcp.example/".into(),
            auth: McpAuthInput::OAuth(McpOAuthInput {
                client_id: client_id.map(str::to_string),
                client_secret: secret.map(str::to_string),
                authorization_endpoint: None,
                token_endpoint: None,
                registration_endpoint: None,
            }),
        }
    }

    #[tokio::test]
    async fn oauth_upsert_keeps_client_and_tokens_across_reupsert() {
        let (s, _t) = store().await;
        // Manual client creds on first upsert.
        let row = s
            .upsert(&oauth_input("o", Some("cid"), Some("csec")))
            .await
            .unwrap();
        let StoredAuth::Oauth(st) = &row.auth else {
            panic!("expected oauth, got {:?}", row.auth)
        };
        assert_eq!(st.client_id.as_deref(), Some("cid"));
        assert_eq!(st.client_secret, Some(Secret::from("csec")));
        assert!(st.access_token.is_none());

        // The flow writes tokens + endpoint metadata out of band.
        s.save_oauth_client(
            "o",
            "cid",
            Some("csec"),
            r#"{"token_endpoint":"https://as/token"}"#,
        )
        .await
        .unwrap();
        s.save_oauth_tokens("o", "at", Some("rt"), Some("9999999999"))
            .await
            .unwrap();
        let row = s.get("o").await.unwrap().unwrap();
        let StoredAuth::Oauth(st) = &row.auth else {
            panic!()
        };
        assert_eq!(st.access_token, Some(Secret::from("at")));
        assert_eq!(st.refresh_token, Some(Secret::from("rt")));
        assert_eq!(st.expires_at.as_deref(), Some("9999999999"));
        assert!(st.meta.as_deref().unwrap().contains("token_endpoint"));

        // A user re-upsert (omit secret → keep) must NOT wipe the stored tokens/meta.
        let row = s
            .upsert(&oauth_input("o", Some("cid"), None))
            .await
            .unwrap();
        let StoredAuth::Oauth(st) = &row.auth else {
            panic!()
        };
        assert_eq!(
            st.client_secret,
            Some(Secret::from("csec")),
            "omit keeps secret"
        );
        assert_eq!(
            st.access_token,
            Some(Secret::from("at")),
            "tokens survive re-upsert"
        );
        assert!(st.meta.is_some(), "endpoint meta survives re-upsert");
    }

    #[tokio::test]
    async fn rejects_empty_name_and_url() {
        let (s, _t) = store().await;
        assert!(s.upsert(&bearer_input("  ", Some("t"))).await.is_err());
        let mut inp = bearer_input("a", Some("t"));
        inp.url = "  ".into();
        assert!(s.upsert(&inp).await.is_err());
    }
}
