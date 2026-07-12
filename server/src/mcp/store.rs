//! SQLite storage for configured remote MCP servers (`mcp_servers`). One row
//! per server, keyed by `name`. A bearer secret is stored plaintext (the DB
//! file is the trust boundary) and wrapped in [`Secret`] in memory; write-only
//! inputs follow the settings store's keep/clear/set convention (`None` keeps,
//! `""` clears, a value sets). `github_app` servers store no token — it is
//! minted from the GitHub App connection at use time.

use horsie_agentcore::Secret;
use horsie_models::mcp::{McpAuthInput, McpServerInput};
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
    /// GitHub MCP over the existing App connection; token minted at use.
    GithubApp,
}

impl StoredAuth {
    /// The `auth_kind` discriminant persisted in the row.
    fn kind(&self) -> &'static str {
        match self {
            StoredAuth::None => "none",
            StoredAuth::Bearer(_) => "bearer",
            StoredAuth::GithubApp => "github_app",
        }
    }

    /// The bearer secret to persist, if any.
    fn bearer(&self) -> Option<&Secret> {
        match self {
            StoredAuth::Bearer(s) => s.as_ref(),
            StoredAuth::None | StoredAuth::GithubApp => None,
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
            "SELECT name, url, enabled, auth_kind, bearer_token, tool_count, last_error \
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
            "SELECT name, url, enabled, auth_kind, bearer_token, tool_count, last_error \
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
             (name, url, enabled, auth_kind, bearer_token, tool_count, last_error, created_at, updated_at) \
             VALUES (?, ?, 0, ?, ?, NULL, NULL, ?, ?) \
             ON CONFLICT(name) DO UPDATE SET \
             url = excluded.url, auth_kind = excluded.auth_kind, \
             bearer_token = excluded.bearer_token, enabled = 0, \
             tool_count = NULL, last_error = NULL, updated_at = excluded.updated_at",
        )
        .bind(name)
        .bind(url)
        .bind(auth.kind())
        .bind(auth.bearer().map(|s| s.expose().to_string()))
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
}

/// Build the stored auth for an upsert, resolving the bearer secret against the
/// existing row (keep/clear/set).
fn auth_from_input(input: &McpAuthInput, existing: Option<&McpServerRow>) -> StoredAuth {
    match input {
        McpAuthInput::None(_) => StoredAuth::None,
        McpAuthInput::GithubApp(_) => StoredAuth::GithubApp,
        McpAuthInput::Bearer(b) => {
            let prior = existing.and_then(|r| match &r.auth {
                StoredAuth::Bearer(s) => s.as_ref(),
                StoredAuth::None | StoredAuth::GithubApp => None,
            });
            StoredAuth::Bearer(resolve_secret(&b.token, prior))
        }
    }
}

fn row_to_server(row: &SqliteRow) -> Result<McpServerRow, String> {
    let auth_kind: String = row.try_get("auth_kind").map_err(|e| e.to_string())?;
    let auth = match auth_kind.as_str() {
        "none" => StoredAuth::None,
        "github_app" => StoredAuth::GithubApp,
        "bearer" => StoredAuth::Bearer(opt_secret(row, "bearer_token")?),
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

    #[tokio::test]
    async fn rejects_empty_name_and_url() {
        let (s, _t) = store().await;
        assert!(s.upsert(&bearer_input("  ", Some("t"))).await.is_err());
        let mut inp = bearer_input("a", Some("t"));
        inp.url = "  ".into();
        assert!(s.upsert(&inp).await.is_err());
    }
}
