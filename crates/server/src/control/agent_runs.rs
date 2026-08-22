//! The agent-runs resource: which agents have run, and under which preset.
//!
//! The read side of the index in [`crate::agent_runs`]. There is no write side
//! — every row is derived by the session that owns the agent, so a caller that
//! could write one could make the index disagree with the transcripts it points
//! at.
//!
//! This is the entry point for tuning: find the runs of a preset, read what
//! they did, then edit the preset through `horsie_agents`. Without it that first
//! step is a sweep of every session in the project asking each what it hosted.

use crate::agent_runs::AgentRunFilter;
use crate::control::{ControlError, Expose, Method, Operation, Resource, op};
use crate::projects::ProjectServices;
use horsie_models::agents::{AgentRunView, AgentRunsPage};
use std::sync::Arc;

/// What a model gets by default, and the most it can ask for.
///
/// A run is a few short strings, so these are far above the transcript read's
/// 20/100: the point of a listing is to see the shape of a preset's history
/// before deciding which runs to spend context on.
const PAGE_DEFAULT: usize = 50;
const PAGE_MAX: usize = 200;

#[derive(serde::Deserialize, schemars::JsonSchema, Default)]
pub struct ListAgentRuns {
    /// Only runs of this saved preset. This is the question the index exists
    /// for: "what has this agent actually been doing".
    pub agent: Option<String>,
    /// Only runs inside this session.
    pub session_id: Option<String>,
    /// Only runs that reached this status: "running", "idle", "completed",
    /// "failed", "cancelled", "awaiting_input", "provisioning". Pair with
    /// `agent` to find what a preset gets wrong.
    pub status: Option<String>,
    /// Only runs that started at or after this Unix epoch-ms stamp. How a
    /// scheduled job reads what has happened since it last looked.
    pub since_ms: Option<u64>,
    /// How many runs, at most 200. Defaults to 50.
    pub max: Option<usize>,
    /// Skip this many. With `max`, how you page a long history.
    pub offset: Option<usize>,
}

/// Agent runs: one row per agent that has run, indexed by the preset it ran
/// under.
pub struct AgentRuns;

impl Resource for AgentRuns {
    fn name(&self) -> &'static str {
        "agent-runs"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![op(
            "list",
            Method::Get,
            "/agent-runs",
            "Find agent runs, newest first. Filter by `agent` to get every run \
             of one saved preset — across sessions, workflow steps and \
             subagents alike — then read one with `horsie_sessions` using the \
             `session_id` and `agent_id` this answers with.",
            Expose::ApiAndTool,
            |s: Arc<ProjectServices>, i: ListAgentRuns| async move { list(&s, i).await },
        )]
    }
}

async fn list(
    services: &ProjectServices,
    input: ListAgentRuns,
) -> Result<AgentRunsPage, ControlError> {
    let filter = AgentRunFilter {
        preset: input.agent,
        session_id: input.session_id,
        status: input.status,
        since_ms: input.since_ms.map(|t| i64::try_from(t).unwrap_or(i64::MAX)),
        limit: input.max.unwrap_or(PAGE_DEFAULT).clamp(1, PAGE_MAX),
        offset: input.offset.unwrap_or(0),
    };
    let runs = services
        .agent_runs
        .list(&filter)
        .await
        .map_err(ControlError::Internal)?
        .into_iter()
        .map(|r| AgentRunView {
            session_id: r.session_id,
            agent_id: r.agent_id,
            preset: r.preset,
            status: r.status,
            started_at_ms: u64::try_from(r.started_at).unwrap_or(0),
            ended_at_ms: r.ended_at.map(|t| u64::try_from(t).unwrap_or(0)),
        })
        .collect();
    Ok(AgentRunsPage { runs })
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

    fn operations() -> Vec<Operation> {
        AgentRuns.operations()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        assert_eq!(actions, ["list"]);
        assert_eq!(AgentRuns.name(), "agent-runs");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }

    /// The index is derived, so nothing here may write to it. A row a caller
    /// could invent would point at a transcript that never existed.
    #[test]
    fn the_index_is_read_only() {
        assert!(
            operations().iter().all(|o| o.method == Method::Get),
            "agent-runs is a projection; a write here would let the index \
             disagree with the sessions it indexes"
        );
    }

    async fn services() -> (Arc<ProjectServices>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::testing::state(dir.path()).build().await;
        let services = state.services().await;
        (services, dir)
    }

    fn row(session: &str, agent: &str, preset: &str) -> crate::agent_runs::AgentRunRow {
        crate::agent_runs::AgentRunRow {
            session_id: session.into(),
            agent_id: agent.into(),
            preset: Some(preset.into()),
            status: "completed".into(),
            started_at: 1_000,
            ended_at: Some(2_000),
        }
    }

    #[tokio::test]
    async fn listing_by_agent_answers_addresses_a_transcript_read_can_use() {
        let (services, _dir) = services().await;
        services
            .agent_runs
            .record(&[row("s1", "main", "reviewer"), row("s2", "main", "deployer")])
            .await
            .unwrap();

        let out = operations()
            .into_iter()
            .find(|o| o.action == "list")
            .unwrap()
            .run(services, serde_json::json!({"agent": "reviewer"}))
            .await
            .unwrap();
        assert_eq!(out["runs"].as_array().unwrap().len(), 1);
        // The two fields that make this useful: they are exactly `sessions.read`'s
        // `id` and `aid`, so the next call needs no translation.
        assert_eq!(out["runs"][0]["sessionId"], "s1");
        assert_eq!(out["runs"][0]["agentId"], "main");
        assert_eq!(out["runs"][0]["preset"], "reviewer");
    }

    #[tokio::test]
    async fn the_page_size_is_clamped_rather_than_honoured() {
        let (services, _dir) = services().await;
        let rows: Vec<crate::agent_runs::AgentRunRow> = (0..PAGE_MAX + 10)
            .map(|i| row(&format!("s{i}"), "main", "reviewer"))
            .collect();
        services.agent_runs.record(&rows).await.unwrap();

        let out = operations()
            .into_iter()
            .find(|o| o.action == "list")
            .unwrap()
            .run(services, serde_json::json!({"max": 10_000}))
            .await
            .unwrap();
        assert_eq!(out["runs"].as_array().unwrap().len(), PAGE_MAX);
    }
}
