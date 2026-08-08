//! Validation, timestamps, and row↔wire mapping over `EnvironmentStore`.
//! Save-time validation covers only what's stable at save: the name slug and
//! the vendor rule (required, never "local"). Whether the named vendor is
//! connected is a runtime concern — an environment can outlive the vendor it
//! names.

use crate::environments::store::{
    EnvironmentEnvVar, EnvironmentProvisionStep, EnvironmentRepo, EnvironmentRow,
    EnvironmentStepParam, EnvironmentStore,
};
use horsie_models::environments::{EnvironmentInput, EnvironmentView};
use horsie_models::executor::{EnvVar, ProvisionStep, StepParam};
use horsie_models::session_api::RepoConfig;

/// Typed service errors so the HTTP layer can pick a status without string
/// matching: NotFound → 404, Conflict → 409, Invalid → 422, Internal → 500.
#[derive(Debug)]
pub enum EnvironmentError {
    NotFound(String),
    Conflict(String),
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for EnvironmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Conflict(m) | Self::Invalid(m) | Self::Internal(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for EnvironmentError {}

pub struct EnvironmentService {
    store: EnvironmentStore,
}

impl EnvironmentService {
    pub fn new(store: EnvironmentStore) -> Self {
        Self { store }
    }

    pub async fn list(&self) -> Result<Vec<EnvironmentView>, EnvironmentError> {
        Ok(self
            .store
            .list()
            .await
            .map_err(EnvironmentError::Internal)?
            .iter()
            .map(environment_view)
            .collect())
    }

    pub async fn get(&self, name: &str) -> Result<EnvironmentView, EnvironmentError> {
        self.store
            .get(name)
            .await
            .map_err(EnvironmentError::Internal)?
            .as_ref()
            .map(environment_view)
            .ok_or_else(|| EnvironmentError::NotFound(format!("unknown environment '{name}'")))
    }

    pub async fn create(
        &self,
        input: EnvironmentInput,
    ) -> Result<EnvironmentView, EnvironmentError> {
        let vendor = validate(&input)?;
        if self
            .store
            .get(&input.name)
            .await
            .map_err(EnvironmentError::Internal)?
            .is_some()
        {
            return Err(EnvironmentError::Conflict(format!(
                "environment '{}' already exists",
                input.name
            )));
        }
        let now = now_secs();
        let row = row_from_input(input, vendor, now.clone(), now);
        self.store
            .insert(&row)
            .await
            .map_err(EnvironmentError::Internal)?;
        self.get(&row.name).await
    }

    /// Full replace. The path name is the id of record: a body naming a
    /// different environment is invalid rather than a rename.
    pub async fn replace(
        &self,
        name: &str,
        input: EnvironmentInput,
    ) -> Result<EnvironmentView, EnvironmentError> {
        if input.name != name {
            return Err(EnvironmentError::Invalid(
                "environment name is immutable; the path is the id of record".to_string(),
            ));
        }
        let existing = self
            .store
            .get(name)
            .await
            .map_err(EnvironmentError::Internal)?
            .ok_or_else(|| EnvironmentError::NotFound(format!("unknown environment '{name}'")))?;
        let vendor = validate(&input)?;
        let row = row_from_input(input, vendor, existing.created_at, now_secs());
        self.store
            .replace(&row)
            .await
            .map_err(EnvironmentError::Internal)?;
        self.get(name).await
    }

    pub async fn delete(&self, name: &str) -> Result<(), EnvironmentError> {
        if self
            .store
            .delete(name)
            .await
            .map_err(EnvironmentError::Internal)?
        {
            Ok(())
        } else {
            Err(EnvironmentError::NotFound(format!(
                "unknown environment '{name}'"
            )))
        }
    }
}

/// Save-time validation; returns the trimmed vendor to store.
fn validate(input: &EnvironmentInput) -> Result<String, EnvironmentError> {
    crate::memory::validate_slug(&input.name).map_err(EnvironmentError::Invalid)?;
    let vendor = input.vendor.trim();
    if vendor.is_empty() {
        return Err(EnvironmentError::Invalid(
            "vendor must not be empty: an environment names the runtime it runs on".to_string(),
        ));
    }
    if vendor == "local" {
        return Err(EnvironmentError::Invalid(
            "vendor 'local' is not supported: environments target vendor-managed runtimes"
                .to_string(),
        ));
    }
    Ok(vendor.to_string())
}

fn row_from_input(
    input: EnvironmentInput,
    vendor: String,
    created_at: String,
    updated_at: String,
) -> EnvironmentRow {
    EnvironmentRow {
        name: input.name,
        description: input.description.unwrap_or_default(),
        vendor,
        repos: input
            .repos
            .unwrap_or_default()
            .into_iter()
            .map(|r| EnvironmentRepo {
                url: r.url,
                git_ref: r.git_ref,
                dir: r.dir,
            })
            .collect(),
        env_vars: input
            .env_vars
            .unwrap_or_default()
            .into_iter()
            .map(|v| EnvironmentEnvVar {
                name: v.name,
                value: v.value,
            })
            .collect(),
        provision: input
            .provision
            .unwrap_or_default()
            .into_iter()
            .map(|p| EnvironmentProvisionStep {
                name: p.name,
                uses: p.uses,
                with: p
                    .with
                    .into_iter()
                    .map(|w| EnvironmentStepParam {
                        key: w.key,
                        value: w.value,
                    })
                    .collect(),
            })
            .collect(),
        created_at,
        updated_at,
    }
}

fn environment_view(row: &EnvironmentRow) -> EnvironmentView {
    EnvironmentView {
        name: row.name.clone(),
        description: row.description.clone(),
        vendor: row.vendor.clone(),
        repos: row
            .repos
            .iter()
            .map(|r| RepoConfig {
                url: r.url.clone(),
                git_ref: r.git_ref.clone(),
                dir: r.dir.clone(),
            })
            .collect(),
        env_vars: row
            .env_vars
            .iter()
            .map(|v| EnvVar {
                name: v.name.clone(),
                value: v.value.clone(),
            })
            .collect(),
        provision: row
            .provision
            .iter()
            .map(|p| ProvisionStep {
                name: p.name.clone(),
                uses: p.uses.clone(),
                with: p
                    .with
                    .iter()
                    .map(|w| StepParam {
                        key: w.key.clone(),
                        value: w.value.clone(),
                    })
                    .collect(),
            })
            .collect(),
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
    }
}

fn now_secs() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    async fn service() -> EnvironmentService {
        EnvironmentService::new(EnvironmentStore::new(
            crate::db::testing::db().await,
            crate::auth::UserId::new("1"),
        ))
    }

    fn input(name: &str, vendor: &str) -> EnvironmentInput {
        EnvironmentInput {
            name: name.into(),
            description: Some("d".into()),
            vendor: vendor.into(),
            repos: None,
            env_vars: None,
            provision: None,
        }
    }

    #[tokio::test]
    async fn create_returns_a_view_with_defaults_and_timestamps() {
        let s = service().await;
        let v = s.create(input("a", "fly")).await.unwrap();
        assert_eq!(v.name, "a");
        assert_eq!(v.vendor, "fly");
        assert!(v.repos.is_empty() && v.env_vars.is_empty() && v.provision.is_empty());
        assert!(!v.created_at.is_empty());
        assert_eq!(v.created_at, v.updated_at);
    }

    #[tokio::test]
    async fn create_validates_slug_and_vendor() {
        let s = service().await;
        assert!(matches!(
            s.create(input("Not A Slug", "fly")).await.unwrap_err(),
            EnvironmentError::Invalid(_)
        ));
        for bad_vendor in ["", "   ", "local"] {
            let err = s.create(input("a", bad_vendor)).await.unwrap_err();
            assert!(
                matches!(err, EnvironmentError::Invalid(ref m) if m.contains("vendor")),
                "{bad_vendor:?}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn vendor_is_stored_trimmed() {
        let s = service().await;
        let v = s.create(input("a", "  fly  ")).await.unwrap();
        assert_eq!(v.vendor, "fly");
    }

    #[tokio::test]
    async fn duplicate_create_conflicts() {
        let s = service().await;
        s.create(input("a", "fly")).await.unwrap();
        assert!(matches!(
            s.create(input("a", "fly")).await.unwrap_err(),
            EnvironmentError::Conflict(_)
        ));
    }

    #[tokio::test]
    async fn replace_swaps_fields_and_keeps_created_at() {
        let s = service().await;
        let v = s.create(input("a", "fly")).await.unwrap();
        let mut upd = input("a", "docker");
        upd.description = Some("new".into());
        let got = s.replace("a", upd).await.unwrap();
        assert_eq!(got.vendor, "docker");
        assert_eq!(got.description, "new");
        assert_eq!(got.created_at, v.created_at);
        // Rename via body → invalid; unknown → not found.
        assert!(matches!(
            s.replace("a", input("b", "fly")).await.unwrap_err(),
            EnvironmentError::Invalid(_)
        ));
        assert!(matches!(
            s.replace("ghost", input("ghost", "fly")).await.unwrap_err(),
            EnvironmentError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn delete_and_get_report_unknown_names() {
        let s = service().await;
        assert!(matches!(
            s.get("ghost").await.unwrap_err(),
            EnvironmentError::NotFound(_)
        ));
        assert!(matches!(
            s.delete("ghost").await.unwrap_err(),
            EnvironmentError::NotFound(_)
        ));
        s.create(input("a", "fly")).await.unwrap();
        s.delete("a").await.unwrap();
        assert!(matches!(
            s.get("a").await.unwrap_err(),
            EnvironmentError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn list_is_ordered_by_name() {
        let s = service().await;
        s.create(input("b", "fly")).await.unwrap();
        s.create(input("a", "fly")).await.unwrap();
        let names: Vec<String> = s
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn repos_env_vars_and_provision_round_trip_through_the_mapping() {
        let s = service().await;
        let mut i = input("a", "fly");
        i.repos = Some(vec![RepoConfig {
            url: "https://github.com/o/api".into(),
            git_ref: Some("dev".into()),
            dir: None,
        }]);
        i.env_vars = Some(vec![EnvVar {
            name: "RUST_LOG".into(),
            value: "debug".into(),
        }]);
        i.provision = Some(vec![ProvisionStep {
            name: "setup".into(),
            uses: "run".into(),
            with: vec![StepParam {
                key: "cmd".into(),
                value: "make setup".into(),
            }],
        }]);
        let v = s.create(i).await.unwrap();
        assert_eq!(v.repos[0].git_ref.as_deref(), Some("dev"));
        assert_eq!(v.env_vars[0].name, "RUST_LOG");
        assert_eq!(v.provision[0].with[0].value, "make setup");
    }
}
