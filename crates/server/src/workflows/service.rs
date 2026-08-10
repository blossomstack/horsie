//! Validation, timestamps, and row↔wire mapping over [`WorkflowStore`].
//!
//! Save-time validation covers what is stable at save: the name slug, and that
//! the graph is well formed — a real start step, no duplicate step names, every
//! transition pointing somewhere, a schema wherever a condition needs one, and
//! every referenced agent preset existing. Whether a preset's model is still
//! configured is live state, resolved when a run is created.

use crate::agents::AgentService;
use crate::workflows::store::{WorkflowRow, WorkflowStore};
use horsie_models::workflow::{WorkflowInput, WorkflowStepDef, WorkflowView};
use std::collections::HashSet;
use std::sync::Arc;

/// Typed service errors so the HTTP layer can pick a status without string
/// matching: NotFound → 404, Conflict → 409, Invalid → 422, Internal → 500.
#[derive(Debug)]
pub enum WorkflowError {
    NotFound(String),
    Conflict(String),
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Conflict(m) | Self::Invalid(m) | Self::Internal(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for WorkflowError {}

pub struct WorkflowService {
    store: WorkflowStore,
    agents: Arc<AgentService>,
}

impl WorkflowService {
    pub fn new(store: WorkflowStore, agents: Arc<AgentService>) -> Self {
        Self { store, agents }
    }

    pub async fn list(&self) -> Result<Vec<WorkflowView>, WorkflowError> {
        Ok(self
            .store
            .list()
            .await
            .map_err(WorkflowError::Internal)?
            .into_iter()
            .map(to_view)
            .collect())
    }

    pub async fn get(&self, name: &str) -> Result<WorkflowView, WorkflowError> {
        self.row(name).await.map(to_view)
    }

    /// The stored row, for callers that need the graph itself rather than its
    /// wire projection — a run snapshots it.
    pub async fn row(&self, name: &str) -> Result<WorkflowRow, WorkflowError> {
        self.store
            .get(name)
            .await
            .map_err(WorkflowError::Internal)?
            .ok_or_else(|| WorkflowError::NotFound(format!("unknown workflow '{name}'")))
    }

    pub async fn create(
        &self,
        input: WorkflowInput,
        now_secs: u64,
    ) -> Result<WorkflowView, WorkflowError> {
        self.validate(&input).await?;
        if self
            .store
            .get(&input.name)
            .await
            .map_err(WorkflowError::Internal)?
            .is_some()
        {
            return Err(WorkflowError::Conflict(format!(
                "workflow '{}' already exists",
                input.name
            )));
        }
        let row = WorkflowRow {
            name: input.name,
            description: input.description.unwrap_or_default(),
            start: input.start,
            steps: input.steps,
            max_steps: input.max_steps,
            created_at: now_secs.to_string(),
            updated_at: now_secs.to_string(),
        };
        self.store
            .insert(&row)
            .await
            .map_err(WorkflowError::Internal)?;
        Ok(to_view(row))
    }

    pub async fn replace(
        &self,
        name: &str,
        input: WorkflowInput,
        now_secs: u64,
    ) -> Result<WorkflowView, WorkflowError> {
        if input.name != name {
            return Err(WorkflowError::Invalid(
                "the name in the body must match the one in the path".to_string(),
            ));
        }
        self.validate(&input).await?;
        let existing = self.row(name).await?;
        let row = WorkflowRow {
            name: input.name,
            description: input.description.unwrap_or_default(),
            start: input.start,
            steps: input.steps,
            max_steps: input.max_steps,
            created_at: existing.created_at,
            updated_at: now_secs.to_string(),
        };
        if !self
            .store
            .replace(&row)
            .await
            .map_err(WorkflowError::Internal)?
        {
            return Err(WorkflowError::NotFound(format!(
                "unknown workflow '{name}'"
            )));
        }
        Ok(to_view(row))
    }

    /// Delete a definition.
    ///
    /// Runs are deliberately untouched, in flight or not — including the one
    /// this deletes out from under. A run snapshots the whole graph at creation,
    /// with every step's preset already resolved, so it neither reads this row
    /// again nor needs it to stay readable afterwards. (An earlier comment here
    /// claimed the caller refuses while a run is active. It does not, and it
    /// should not: there is nothing for the run to lose.)
    pub async fn delete(&self, name: &str) -> Result<(), WorkflowError> {
        if !self
            .store
            .delete(name)
            .await
            .map_err(WorkflowError::Internal)?
        {
            return Err(WorkflowError::NotFound(format!(
                "unknown workflow '{name}'"
            )));
        }
        Ok(())
    }

    async fn validate(&self, input: &WorkflowInput) -> Result<(), WorkflowError> {
        crate::memory::validate_slug(&input.name).map_err(WorkflowError::Invalid)?;
        if input.steps.is_empty() {
            return Err(WorkflowError::Invalid(
                "a workflow needs at least one step".to_string(),
            ));
        }
        // A budget of zero would fail every run before its first step, which is
        // never what anyone means. Absent is how you ask for the default.
        if input.max_steps == Some(0) {
            return Err(WorkflowError::Invalid(
                "max_steps must be at least 1, or absent for the default".to_string(),
            ));
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for step in &input.steps {
            if step.name.trim().is_empty() {
                return Err(WorkflowError::Invalid(
                    "every step needs a name".to_string(),
                ));
            }
            if !seen.insert(step.name.as_str()) {
                return Err(WorkflowError::Invalid(format!(
                    "two steps are named '{}'; transitions address steps by name",
                    step.name
                )));
            }
            if step.prompt.trim().is_empty() {
                return Err(WorkflowError::Invalid(format!(
                    "step '{}': prompt must not be empty — it is the step's whole instruction",
                    step.name
                )));
            }
        }
        if !seen.contains(input.start.as_str()) {
            return Err(WorkflowError::Invalid(format!(
                "start step '{}' is not one of the steps",
                input.start
            )));
        }
        for step in &input.steps {
            for t in step.transitions.iter().flatten() {
                if !seen.contains(t.to.as_str()) {
                    return Err(WorkflowError::Invalid(format!(
                        "step '{}' transitions to '{}', which is not a step",
                        step.name, t.to
                    )));
                }
                // A condition reads `output`; with no schema the step ends its
                // turn with plain text and there is nothing to read.
                if let Some(condition) = &t.condition {
                    if step.output_schema.is_none() {
                        return Err(WorkflowError::Invalid(format!(
                            "step '{}' has a conditional transition but no output schema — \
                             a condition reads the step's structured output",
                            step.name
                        )));
                    }
                    // Parseability only: whether it is *true* depends on output
                    // this workflow has not produced yet. Worth checking here
                    // because an unparseable expression unwinds the evaluator,
                    // and catching it at save beats failing a run halfway.
                    if let Err(e) =
                        crate::sessions::workflow::eval_condition(condition, &serde_json::json!({}))
                        && e.contains("not a valid expression")
                    {
                        return Err(WorkflowError::Invalid(format!("step '{}': {e}", step.name)));
                    }
                }
            }
            // Checked here so a workflow cannot be saved broken; resolved
            // again when a run is created, because presets are editable.
            self.agents.get(&step.agent).await.map_err(|_| {
                WorkflowError::Invalid(format!(
                    "step '{}': unknown agent preset '{}'",
                    step.name, step.agent
                ))
            })?;
        }
        Ok(())
    }
}

fn to_view(row: WorkflowRow) -> WorkflowView {
    WorkflowView {
        name: row.name,
        description: row.description,
        start: row.start,
        steps: row.steps,
        max_steps: row.max_steps,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// The definition's steps, indexed by name.
pub fn step_named<'a>(steps: &'a [WorkflowStepDef], name: &str) -> Option<&'a WorkflowStepDef> {
    steps.iter().find(|s| s.name == name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::agents::AgentPresetInput;
    use horsie_models::workflow::WorkflowTransition;

    /// A service over a real database, with the three presets the fixtures
    /// reference already created. The agent service validates a preset's model
    /// against the config store, so that has to exist and know one model.
    async fn service() -> WorkflowService {
        use horsie_models::settings::{ModelInput, ProviderInput};
        let db = crate::db::testing::db().await;
        let opened = crate::config::DbConfigStore::open_on(
            db.clone(),
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
                vec![ModelInput {
                    alias: "sonnet".into(),
                    provider: "p".into(),
                    model_id: "claude-sonnet-4-6".into(),
                    max_tokens: None,
                    context_window: None,
                    thinking_efforts: None,
                    thinking_effort: None,
                    thinking_dialect: None,
                    forced_tools_disable_thinking: None,
                }],
            )
            .await
            .unwrap();
        let agents = Arc::new(AgentService::new(
            crate::agents::AgentStore::new(db.clone(), crate::auth::UserId::new("1")),
            opened.store.clone(),
        ));
        for name in ["bug-triager", "coder", "writer"] {
            agents
                .create(AgentPresetInput {
                    name: name.into(),
                    description: None,
                    instructions: None,
                    model: "sonnet".into(),
                    plugins: None,
                    mcp_servers: None,
                    memory_spaces: None,
                    thinking_effort: None,
                })
                .await
                .unwrap();
        }
        WorkflowService::new(
            WorkflowStore::new(db, crate::auth::UserId::new("1")),
            agents,
        )
    }

    fn step(name: &str, agent: &str) -> WorkflowStepDef {
        WorkflowStepDef {
            name: name.into(),
            agent: agent.into(),
            prompt: "do it".into(),
            output_schema: None,
            transitions: None,
            max_iterations: None,
            max_retries: None,
        }
    }

    fn input(name: &str) -> WorkflowInput {
        WorkflowInput {
            name: name.into(),
            description: None,
            start: "triage".into(),
            steps: vec![step("triage", "bug-triager"), step("fix", "coder")],
            max_steps: None,
        }
    }

    #[tokio::test]
    async fn create_then_get_round_trips_and_stamps_both_times() {
        let s = service().await;
        let v = s.create(input("fix-bug"), 1_000).await.unwrap();
        assert_eq!(v.name, "fix-bug");
        assert_eq!(v.description, "");
        assert_eq!(v.created_at, v.updated_at);
        assert_eq!(s.get("fix-bug").await.unwrap().steps.len(), 2);
    }

    #[tokio::test]
    async fn a_second_create_conflicts() {
        let s = service().await;
        s.create(input("a"), 1).await.unwrap();
        assert!(matches!(
            s.create(input("a"), 1).await,
            Err(WorkflowError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn replace_keeps_created_at_and_moves_updated_at() {
        let s = service().await;
        s.create(input("a"), 1_000).await.unwrap();
        let v = s.replace("a", input("a"), 2_000).await.unwrap();
        assert_eq!(v.created_at, "1000");
        assert_eq!(v.updated_at, "2000");
    }

    #[tokio::test]
    async fn a_start_that_names_no_step_is_refused() {
        let s = service().await;
        let mut i = input("a");
        i.start = "nowhere".into();
        assert!(matches!(
            s.create(i, 1).await,
            Err(WorkflowError::Invalid(m)) if m.contains("start step 'nowhere'")
        ));
    }

    #[tokio::test]
    async fn a_transition_to_nowhere_is_refused() {
        let s = service().await;
        let mut i = input("a");
        i.steps[0].transitions = Some(vec![WorkflowTransition {
            to: "reviw".into(),
            condition: None,
        }]);
        assert!(matches!(
            s.create(i, 1).await,
            Err(WorkflowError::Invalid(m)) if m.contains("'reviw'")
        ));
    }

    /// A condition reads `output`. Without a schema the step ends its turn with
    /// plain text, so the condition could only ever fail to evaluate — which is
    /// a run failure, and much better caught here.
    #[tokio::test]
    async fn a_condition_without_an_output_schema_is_refused() {
        let s = service().await;
        let mut i = input("a");
        i.steps[0].transitions = Some(vec![WorkflowTransition {
            to: "fix".into(),
            condition: Some("output.severity == \"p0\"".into()),
        }]);
        assert!(matches!(
            s.create(i, 1).await,
            Err(WorkflowError::Invalid(m)) if m.contains("no output schema")
        ));
    }

    /// An unparseable expression unwinds the evaluator at run time. Catching
    /// it at save turns a run that dies halfway into a 422 on the form.
    #[tokio::test]
    async fn an_unparseable_condition_is_refused() {
        let s = service().await;
        let mut i = input("a");
        i.steps[0].output_schema = Some(serde_json::json!({"type": "object"}));
        i.steps[0].transitions = Some(vec![WorkflowTransition {
            to: "fix".into(),
            condition: Some("!!!".into()),
        }]);
        assert!(matches!(
            s.create(i, 1).await,
            Err(WorkflowError::Invalid(m)) if m.contains("not a valid expression")
        ));
    }

    /// A condition that merely reads a field the probe has no value for is
    /// fine: whether it holds depends on output no run has produced yet.
    #[tokio::test]
    async fn a_condition_reading_an_absent_field_is_allowed() {
        let s = service().await;
        let mut i = input("a");
        i.steps[0].output_schema = Some(serde_json::json!({"type": "object"}));
        i.steps[0].transitions = Some(vec![WorkflowTransition {
            to: "fix".into(),
            condition: Some("output.severity == \"p0\"".into()),
        }]);
        assert!(s.create(i, 1).await.is_ok());
    }

    /// The budget is what stops a loop whose condition never flips, and it is a
    /// property of the graph — so a workflow that legitimately loops far can say
    /// so, and a run snapshots whatever it said.
    #[tokio::test]
    async fn a_definition_carries_its_own_step_budget() {
        let s = service().await;
        let mut i = input("a");
        i.max_steps = Some(12);
        assert_eq!(s.create(i, 1).await.unwrap().max_steps, Some(12));
        assert_eq!(s.get("a").await.unwrap().max_steps, Some(12));
    }

    /// Zero would fail every run before its first step. Absent is how you ask
    /// for the default.
    #[tokio::test]
    async fn a_step_budget_of_zero_is_refused() {
        let s = service().await;
        let mut i = input("a");
        i.max_steps = Some(0);
        assert!(matches!(
            s.create(i, 1).await,
            Err(WorkflowError::Invalid(m)) if m.contains("at least 1")
        ));
    }

    #[tokio::test]
    async fn duplicate_step_names_are_refused() {
        let s = service().await;
        let mut i = input("a");
        i.steps.push(step("triage", "coder"));
        assert!(matches!(
            s.create(i, 1).await,
            Err(WorkflowError::Invalid(m)) if m.contains("two steps are named")
        ));
    }

    #[tokio::test]
    async fn an_unknown_preset_is_refused_naming_the_step() {
        let s = service().await;
        let mut i = input("a");
        i.steps[1].agent = "ghost".into();
        assert!(matches!(
            s.create(i, 1).await,
            Err(WorkflowError::Invalid(m)) if m.contains("step 'fix'") && m.contains("'ghost'")
        ));
    }

    #[tokio::test]
    async fn an_empty_prompt_is_refused() {
        let s = service().await;
        let mut i = input("a");
        i.steps[0].prompt = "  ".into();
        assert!(matches!(
            s.create(i, 1).await,
            Err(WorkflowError::Invalid(m)) if m.contains("prompt must not be empty")
        ));
    }

    #[tokio::test]
    async fn get_and_delete_report_a_missing_workflow() {
        let s = service().await;
        assert!(matches!(
            s.get("ghost").await,
            Err(WorkflowError::NotFound(_))
        ));
        assert!(matches!(
            s.delete("ghost").await,
            Err(WorkflowError::NotFound(_))
        ));
    }
}
