//! Validation, timestamps, and row↔wire mapping over `AgentStore`. Save-time
//! validation covers only what's stable at save: the name slug, the model
//! alias, and the thinking effort the model offers. Plugins, MCP servers, and
//! memory spaces are live/external rosters — validated at invoke.

use crate::agents::store::{AgentRow, AgentStore};
use crate::config::ConfigStore;
use horsie_models::agents::{AgentPresetInput, AgentView};
use std::sync::Arc;

/// Longest set of preset instructions, in characters. They ride in every prompt
/// this preset's agent sends, so the bound is a cost bound as much as a
/// validation one.
const MAX_INSTRUCTIONS_CHARS: usize = 8_000;

/// Typed service errors so the HTTP layer can pick a status without string
/// matching: NotFound → 404, Conflict → 409, Invalid → 422, Internal → 500.
#[derive(Debug)]
pub enum AgentError {
    NotFound(String),
    Conflict(String),
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Conflict(m) | Self::Invalid(m) | Self::Internal(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for AgentError {}

pub struct AgentService {
    store: AgentStore,
    config: Arc<dyn ConfigStore>,
}

impl AgentService {
    pub fn new(store: AgentStore, config: Arc<dyn ConfigStore>) -> Self {
        Self { store, config }
    }

    pub async fn list(&self) -> Result<Vec<AgentView>, AgentError> {
        Ok(self
            .store
            .list()
            .await
            .map_err(AgentError::Internal)?
            .iter()
            .map(agent_view)
            .collect())
    }

    pub async fn get(&self, name: &str) -> Result<AgentView, AgentError> {
        self.store
            .get(name)
            .await
            .map_err(AgentError::Internal)?
            .as_ref()
            .map(agent_view)
            .ok_or_else(|| AgentError::NotFound(format!("unknown agent '{name}'")))
    }

    pub async fn create(&self, input: AgentPresetInput) -> Result<AgentView, AgentError> {
        self.validate(&input).await?;
        if self
            .store
            .get(&input.name)
            .await
            .map_err(AgentError::Internal)?
            .is_some()
        {
            return Err(AgentError::Conflict(format!(
                "agent '{}' already exists",
                input.name
            )));
        }
        let now = now_secs();
        let row = row_from_input(input, now.clone(), now);
        self.store
            .insert(&row)
            .await
            .map_err(AgentError::Internal)?;
        self.get(&row.name).await
    }

    /// Full replace. The path name is the id of record: a body naming a
    /// different agent is invalid rather than a rename.
    pub async fn replace(
        &self,
        name: &str,
        input: AgentPresetInput,
    ) -> Result<AgentView, AgentError> {
        if input.name != name {
            return Err(AgentError::Invalid(
                "agent name is immutable; the path is the id of record".to_string(),
            ));
        }
        let existing = self
            .store
            .get(name)
            .await
            .map_err(AgentError::Internal)?
            .ok_or_else(|| AgentError::NotFound(format!("unknown agent '{name}'")))?;
        self.validate(&input).await?;
        let row = row_from_input(input, existing.created_at, now_secs());
        self.store
            .replace(&row)
            .await
            .map_err(AgentError::Internal)?;
        self.get(name).await
    }

    pub async fn delete(&self, name: &str) -> Result<(), AgentError> {
        if self
            .store
            .delete(name)
            .await
            .map_err(AgentError::Internal)?
        {
            Ok(())
        } else {
            Err(AgentError::NotFound(format!("unknown agent '{name}'")))
        }
    }

    /// Save-time validation: slug, configured model, offered thinking effort.
    async fn validate(&self, input: &AgentPresetInput) -> Result<(), AgentError> {
        crate::memory::validate_slug(&input.name).map_err(AgentError::Invalid)?;
        // Instructions ride in every one of this agent's prompts, so an
        // accidental paste is a bill rather than a typo. The cap is generous
        // enough for a page of guidance and nothing like a 51 KB document.
        if let Some(instructions) = input.instructions.as_deref()
            && instructions.chars().count() > MAX_INSTRUCTIONS_CHARS
        {
            return Err(AgentError::Invalid(format!(
                "instructions must be at most {MAX_INSTRUCTIONS_CHARS} characters"
            )));
        }
        let view = self.config.view().await.map_err(AgentError::Internal)?;
        let model = view
            .models
            .iter()
            .find(|m| m.alias == input.model)
            .ok_or_else(|| AgentError::Invalid(format!("unknown model '{}'", input.model)))?;
        if let Some(effort) = input.thinking_effort.as_deref() {
            let offered = model.thinking_efforts.clone().unwrap_or_default();
            if !offered.iter().any(|e| e == effort) {
                return Err(AgentError::Invalid(format!(
                    "model '{}' does not offer thinking effort '{effort}'",
                    input.model
                )));
            }
        }
        Ok(())
    }
}

fn row_from_input(input: AgentPresetInput, created_at: String, updated_at: String) -> AgentRow {
    AgentRow {
        name: input.name,
        description: input.description.unwrap_or_default(),
        // Empty is absent: a textarea someone opened and closed must not turn
        // into a blank section in the system prompt.
        instructions: input
            .instructions
            .map(|i| i.trim().to_string())
            .filter(|i| !i.is_empty()),
        model: input.model,
        plugins: input.plugins.unwrap_or_default(),
        mcp_servers: input.mcp_servers.unwrap_or_default(),
        memory_spaces: input.memory_spaces.unwrap_or_default(),
        thinking_effort: input.thinking_effort,
        created_at,
        updated_at,
    }
}

fn agent_view(row: &AgentRow) -> AgentView {
    AgentView {
        name: row.name.clone(),
        description: row.description.clone(),
        instructions: row.instructions.clone(),
        model: row.model.clone(),
        plugins: row.plugins.clone(),
        mcp_servers: row.mcp_servers.clone(),
        memory_spaces: row.memory_spaces.clone(),
        thinking_effort: row.thinking_effort.clone(),
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
    use horsie_models::settings::{ModelInput, ProviderInput};

    /// A service on a temp DB with one provider ("p") and two models:
    /// "sonnet" (offers thinking efforts) and "haiku" (none).
    async fn service() -> (AgentService, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("config.db");
        let opened = crate::config::DbConfigStore::open(
            &format!("sqlite://{}", db.display()),
            crate::config::StoreDeps {
                info: horsie_models::settings::ServerInfo {
                    config_path: String::new(),
                    database: String::new(),
                    state_dir: String::new(),
                    data_dir: String::new(),
                    plugins_dir: String::new(),
                    version: "test".into(),
                },
            },
            crate::auth::UserId::new("1"),
        )
        .await
        .unwrap();
        opened
            .store
            .seed(
                vec![ProviderInput {
                    name: "p".into(),
                    kind: "anthropic".into(),
                    base_url: Some("http://localhost:1".into()),
                    api_key: Some("sk-x".into()),
                    keep_thinking_signature: None,
                }],
                vec![
                    ModelInput {
                        alias: "sonnet".into(),
                        provider: "p".into(),
                        model_id: "claude-sonnet-4-6".into(),
                        max_tokens: None,
                        context_window: None,
                        thinking_efforts: Some(vec!["low".into(), "high".into()]),
                        thinking_effort: None,
                        thinking_dialect: None,
                        forced_tools_disable_thinking: None,
                    },
                    ModelInput {
                        alias: "haiku".into(),
                        provider: "p".into(),
                        model_id: "claude-haiku-4-5".into(),
                        max_tokens: None,
                        context_window: None,
                        thinking_efforts: None,
                        thinking_effort: None,
                        thinking_dialect: None,
                        forced_tools_disable_thinking: None,
                    },
                ],
            )
            .await
            .unwrap();
        (
            AgentService::new(
                AgentStore::new(opened.db.clone(), crate::auth::UserId::new("1")),
                opened.store.clone(),
            ),
            tmp,
        )
    }

    fn input(name: &str, model: &str) -> AgentPresetInput {
        AgentPresetInput {
            name: name.into(),
            description: Some("d".into()),
            instructions: None,
            model: model.into(),
            plugins: None,
            mcp_servers: None,
            memory_spaces: None,
            thinking_effort: None,
        }
    }

    #[tokio::test]
    async fn create_returns_a_view_with_defaults_and_timestamps() {
        let (s, _t) = service().await;
        let v = s.create(input("a", "sonnet")).await.unwrap();
        assert_eq!(v.name, "a");
        assert_eq!(v.description, "d");
        assert!(v.plugins.is_empty());
        assert!(!v.created_at.is_empty());
        assert_eq!(v.created_at, v.updated_at);
    }

    #[tokio::test]
    async fn create_validates_slug_model_and_thinking_effort() {
        let (s, _t) = service().await;
        let mut bad = input("Not A Slug", "sonnet");
        assert!(matches!(
            s.create(bad.clone()).await.unwrap_err(),
            AgentError::Invalid(_)
        ));
        bad = input("a", "ghost-model");
        let err = s.create(bad.clone()).await.unwrap_err();
        assert!(matches!(err, AgentError::Invalid(m) if m.contains("ghost-model")));
        bad = input("a", "haiku");
        bad.thinking_effort = Some("high".into());
        let err = s.create(bad).await.unwrap_err();
        assert!(matches!(err, AgentError::Invalid(m) if m.contains("haiku")));
        // An offered effort passes.
        let mut ok = input("a", "sonnet");
        ok.thinking_effort = Some("high".into());
        assert!(s.create(ok).await.is_ok());
    }

    /// The field that makes two presets on one model different agents. It is
    /// trimmed, empty means absent, and it is bounded because it rides in every
    /// prompt the preset's agent sends.
    #[tokio::test]
    async fn instructions_round_trip_trimmed_and_bounded() {
        let (s, _t) = service().await;
        let mut with = input("reviewer", "sonnet");
        with.instructions = Some("  Always cite file:line.  ".into());
        assert_eq!(
            s.create(with).await.unwrap().instructions.as_deref(),
            Some("Always cite file:line.")
        );

        let mut blank = input("blank", "sonnet");
        blank.instructions = Some("   ".into());
        assert_eq!(
            s.create(blank).await.unwrap().instructions,
            None,
            "a textarea somebody opened and closed is not an instruction"
        );

        let mut huge = input("huge", "sonnet");
        huge.instructions = Some("x".repeat(MAX_INSTRUCTIONS_CHARS + 1));
        assert!(matches!(
            s.create(huge).await.unwrap_err(),
            AgentError::Invalid(m) if m.contains("at most")
        ));
    }

    #[tokio::test]
    async fn duplicate_create_conflicts() {
        let (s, _t) = service().await;
        s.create(input("a", "sonnet")).await.unwrap();
        assert!(matches!(
            s.create(input("a", "sonnet")).await.unwrap_err(),
            AgentError::Conflict(_)
        ));
    }

    #[tokio::test]
    async fn replace_swaps_fields_and_keeps_created_at() {
        let (s, _t) = service().await;
        let v = s.create(input("a", "sonnet")).await.unwrap();
        let mut upd = input("a", "haiku");
        upd.description = Some("new".into());
        let got = s.replace("a", upd).await.unwrap();
        assert_eq!(got.model, "haiku");
        assert_eq!(got.description, "new");
        assert_eq!(got.created_at, v.created_at);
        // Rename via body → invalid; unknown → not found.
        assert!(matches!(
            s.replace("a", input("b", "sonnet")).await.unwrap_err(),
            AgentError::Invalid(_)
        ));
        assert!(matches!(
            s.replace("ghost", input("ghost", "sonnet"))
                .await
                .unwrap_err(),
            AgentError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn delete_and_get_report_unknown_names() {
        let (s, _t) = service().await;
        assert!(matches!(
            s.get("ghost").await.unwrap_err(),
            AgentError::NotFound(_)
        ));
        assert!(matches!(
            s.delete("ghost").await.unwrap_err(),
            AgentError::NotFound(_)
        ));
        s.create(input("a", "sonnet")).await.unwrap();
        s.delete("a").await.unwrap();
        assert!(matches!(
            s.get("a").await.unwrap_err(),
            AgentError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn list_is_ordered_by_name() {
        let (s, _t) = service().await;
        s.create(input("b", "sonnet")).await.unwrap();
        s.create(input("a", "haiku")).await.unwrap();
        let names: Vec<String> = s
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }
}
