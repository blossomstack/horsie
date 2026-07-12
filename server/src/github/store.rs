//! SQLite storage for the deployment-global GitHub connection: the App config
//! (single row) and the connected account's OAuth credentials (single row).
//! Secrets are stored plaintext (the DB file is the trust boundary) and wrapped
//! in [`Secret`] in memory; write-only inputs follow the settings store's
//! keep/clear/set convention (`None` keeps, `""` clears, a value sets).

use horsie_agentcore::Secret;
use horsie_models::github::GitHubAppConfigInput;
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};

/// The GitHub App config row (`github_app`, id = 1).
pub struct AppConfigRow {
    pub client_id: String,
    pub client_secret: Option<Secret>,
    pub app_id: Option<u64>,
    pub private_key: Option<Secret>,
    pub app_slug: Option<String>,
    pub callback_base: Option<String>,
}

/// The connected account's OAuth credentials (`github_credentials`, id = 1).
pub struct CredentialsRow {
    pub login: String,
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
    pub expires_at: Option<String>,
    pub installation_id: Option<u64>,
}

pub struct GithubStore {
    pool: SqlitePool,
}

impl GithubStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn app_config(&self) -> Result<Option<AppConfigRow>, String> {
        let row = sqlx::query(
            "SELECT client_id, client_secret, app_id, private_key, app_slug, callback_base \
             FROM github_app WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(AppConfigRow {
            client_id: row
                .try_get::<String, _>("client_id")
                .map_err(|e| e.to_string())?,
            client_secret: opt_secret(&row, "client_secret")?,
            app_id: opt_u64(&row, "app_id")?,
            private_key: opt_secret(&row, "private_key")?,
            app_slug: opt_string(&row, "app_slug")?,
            callback_base: opt_string(&row, "callback_base")?,
        }))
    }

    /// Persist the App config, honoring write-only secret semantics for
    /// `client_secret` and `private_key`.
    pub async fn save_app_config(
        &self,
        input: &GitHubAppConfigInput,
    ) -> Result<AppConfigRow, String> {
        let existing = self.app_config().await?;
        let client_secret = resolve_secret(
            &input.client_secret,
            existing.as_ref().and_then(|e| e.client_secret.as_ref()),
        );
        let private_key = resolve_secret(
            &input.private_key,
            existing.as_ref().and_then(|e| e.private_key.as_ref()),
        );
        sqlx::query(
            "INSERT INTO github_app (id, client_id, client_secret, app_id, private_key, app_slug, callback_base) \
             VALUES (1, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET client_id = excluded.client_id, \
             client_secret = excluded.client_secret, app_id = excluded.app_id, \
             private_key = excluded.private_key, app_slug = excluded.app_slug, \
             callback_base = excluded.callback_base",
        )
        .bind(input.client_id.trim())
        .bind(client_secret.as_ref().map(|s| s.expose().to_string()))
        .bind(input.app_id.map(|v| v as i64))
        .bind(private_key.as_ref().map(|s| s.expose().to_string()))
        .bind(trimmed(&input.app_slug))
        .bind(trimmed(&input.callback_base))
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        self.app_config()
            .await?
            .ok_or_else(|| "github app config missing after save".to_string())
    }

    pub async fn credentials(&self) -> Result<Option<CredentialsRow>, String> {
        let row = sqlx::query(
            "SELECT login, access_token, refresh_token, expires_at, installation_id \
             FROM github_credentials WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(CredentialsRow {
            login: row
                .try_get::<String, _>("login")
                .map_err(|e| e.to_string())?,
            access_token: Secret::from(
                row.try_get::<String, _>("access_token")
                    .map_err(|e| e.to_string())?,
            ),
            refresh_token: opt_secret(&row, "refresh_token")?,
            expires_at: opt_string(&row, "expires_at")?,
            installation_id: opt_u64(&row, "installation_id")?,
        }))
    }

    pub async fn save_credentials(&self, row: &CredentialsRow) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO github_credentials (id, login, access_token, refresh_token, expires_at, installation_id) \
             VALUES (1, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET login = excluded.login, \
             access_token = excluded.access_token, refresh_token = excluded.refresh_token, \
             expires_at = excluded.expires_at, installation_id = excluded.installation_id",
        )
        .bind(row.login.trim())
        .bind(row.access_token.expose().to_string())
        .bind(row.refresh_token.as_ref().map(|s| s.expose().to_string()))
        .bind(row.expires_at.clone())
        .bind(row.installation_id.map(|v| v as i64))
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn clear_credentials(&self) -> Result<(), String> {
        sqlx::query("DELETE FROM github_credentials")
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
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

/// A `u64` column round-tripped through SQLite's signed `INTEGER`.
fn opt_u64(row: &SqliteRow, col: &str) -> Result<Option<u64>, String> {
    let v = row
        .try_get::<Option<i64>, _>(col)
        .map_err(|e| e.to_string())?;
    Ok(v.and_then(|n| u64::try_from(n).ok()))
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

/// A trimmed, non-empty value, else `None`.
fn trimmed(v: &Option<String>) -> Option<String> {
    v.as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
    use horsie_models::github::GitHubAppConfigInput;
    use std::str::FromStr;

    async fn store() -> (GithubStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (GithubStore::new(pool), tmp)
    }

    fn input(secret: Option<&str>, key: Option<&str>) -> GitHubAppConfigInput {
        GitHubAppConfigInput {
            client_id: "cid".into(),
            client_secret: secret.map(str::to_string),
            app_id: Some(7),
            private_key: key.map(str::to_string),
            app_slug: Some("horsie".into()),
            callback_base: None,
        }
    }

    #[tokio::test]
    async fn app_config_round_trips_and_keeps_omitted_secrets() {
        let (s, _t) = store().await;
        assert!(s.app_config().await.unwrap().is_none());
        s.save_app_config(&input(Some("sec"), Some("PEM")))
            .await
            .unwrap();
        // Omitted secrets keep the stored values.
        let row = s.save_app_config(&input(None, None)).await.unwrap();
        assert_eq!(
            row.client_secret.as_ref().map(|s| s.expose().to_string()),
            Some("sec".into())
        );
        assert_eq!(
            row.private_key.as_ref().map(|s| s.expose().to_string()),
            Some("PEM".into())
        );
        assert_eq!(row.app_id, Some(7));
        // Empty string clears.
        let row = s.save_app_config(&input(Some(""), None)).await.unwrap();
        assert!(row.client_secret.is_none());
    }

    #[tokio::test]
    async fn credentials_save_read_clear() {
        let (s, _t) = store().await;
        assert!(s.credentials().await.unwrap().is_none());
        s.save_credentials(&CredentialsRow {
            login: "octo".into(),
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: None,
            installation_id: Some(42),
        })
        .await
        .unwrap();
        let c = s.credentials().await.unwrap().unwrap();
        assert_eq!(c.login, "octo");
        assert_eq!(c.installation_id, Some(42));
        assert_eq!(c.access_token.expose(), "tok");
        s.clear_credentials().await.unwrap();
        assert!(s.credentials().await.unwrap().is_none());
    }
}
