//! Turning a routine into a running session.
//!
//! Every trigger — the run endpoint, the web UI button, the scheduler's timer —
//! goes through [`RoutineRunner::run`], so all three make the same liveness
//! checks and produce the same kind of session. It is not an HTTP type: the
//! scheduler has no request to answer.

use crate::agents::AgentService;
use crate::config::ConfigStore;
use crate::environments::EnvironmentService;
use crate::routines::service::{RoutineError, RoutineService};
use crate::routines::store::RunOutcome;
use crate::runtime_vendor::RuntimeVendorRegistry;
use crate::sessions::UserMessageError;
use crate::sessions::builder::{SpecError, build_session_spec};
use crate::sessions::spec::{SessionOrigin, SessionStatus, status_kind, status_reason};
use crate::sessions::supervisor::SessionSupervisorCommand;
use horsie_actor::ActorRef;
use horsie_models::session::{AgentSettings as WireAgentSettings, SessionSummary};
use std::sync::Arc;

pub struct RoutineRunner {
    routines: Arc<RoutineService>,
    agents: Arc<AgentService>,
    environments: Arc<EnvironmentService>,
    config: Arc<dyn ConfigStore>,
    vendors: Arc<RuntimeVendorRegistry>,
    supervisor: ActorRef<SessionSupervisorCommand>,
}

impl RoutineRunner {
    pub fn new(
        routines: Arc<RoutineService>,
        agents: Arc<AgentService>,
        environments: Arc<EnvironmentService>,
        config: Arc<dyn ConfigStore>,
        vendors: Arc<RuntimeVendorRegistry>,
        supervisor: ActorRef<SessionSupervisorCommand>,
    ) -> Self {
        Self {
            routines,
            agents,
            environments,
            config,
            vendors,
            supervisor,
        }
    }

    /// Trigger `name`: create its session and queue its prompt.
    ///
    /// The routine's timer is not touched — arming it is the scheduler's
    /// business, taken as a claim before the run starts, so pressing the run
    /// button never moves the next scheduled firing.
    ///
    /// Returns as soon as both the session and the message are accepted — the
    /// turn itself runs in the background and reports through the session.
    pub async fn run(&self, name: &str, now_ms: u64) -> Result<SessionSummary, RoutineError> {
        match self.start(name, now_ms).await {
            Ok(summary) => {
                self.record(name, now_ms, RunOutcome::Started(summary.id.clone()))
                    .await;
                Ok(summary)
            }
            Err(e) => {
                // Only a failure to *start* is recorded here. A run that began
                // and then failed reports through its own session.
                self.record(name, now_ms, RunOutcome::Failed(e.to_string()))
                    .await;
                Err(e)
            }
        }
    }

    async fn record(&self, name: &str, now_ms: u64, outcome: RunOutcome) {
        if let Err(e) = self.routines.record_run(name, now_ms, &outcome).await {
            // The run already happened; losing its bookkeeping must not undo
            // it or stop the next one.
            tracing::error!(routine = %name, error = %e, "recording a routine run failed");
        }
    }

    async fn start(&self, name: &str, now_ms: u64) -> Result<SessionSummary, RoutineError> {
        let routine = self.routines.row(name).await?;
        // Re-resolved every run: presets are editable, and a routine saved
        // against one that has since been deleted must fail here, visibly,
        // rather than as a turn error inside a session nobody is watching.
        let agent = self.agents.get(&routine.agent).await.map_err(|_| {
            RoutineError::Invalid(format!("unknown agent preset '{}'", routine.agent))
        })?;

        let view = self.config.view().await.map_err(RoutineError::Internal)?;
        if !view.models.iter().any(|m| m.alias == agent.model) {
            return Err(RoutineError::Invalid(format!(
                "model '{}' is no longer configured",
                agent.model
            )));
        }

        let wire = WireAgentSettings {
            model: agent.model.clone(),
            allowed_tools: None,
            use_plugins: None,
            max_iterations: None,
            max_retries: None,
            mcp_servers: Some(agent.mcp_servers.clone()),
            memory_spaces: Some(agent.memory_spaces.clone()),
            thinking_effort: agent.thinking_effort.clone(),
            max_concurrent_subagents: None,
            instructions: agent.instructions.clone(),
            auto_compact: agent.auto_compact,
        };
        let spec = build_session_spec(
            &self.config,
            &self.environments,
            // Named for the routine so a run is recognisable before the agent
            // titles it; the agent may retitle it from what it actually did.
            Some(routine.name.clone()),
            wire,
            routine.environment.clone(),
            Some(agent.plugins.clone()),
            SessionOrigin::Routine {
                routine: routine.name.clone(),
            },
        )
        .await
        .map_err(|e| match e {
            SpecError::Invalid(m) => RoutineError::Invalid(m),
            SpecError::Internal(m) => RoutineError::Internal(m),
        })?;
        // The routine's environment is re-resolved every run — an environment
        // deleted since it was saved, or a vendor now offline, fails here and
        // is recorded in `last_error` rather than failing inside a session
        // nobody is watching.
        if !self.vendors.connected_names().contains(&spec.vendor) {
            return Err(RoutineError::Invalid(format!(
                "runtime vendor '{}' is not connected",
                spec.vendor
            )));
        }

        let id = self
            .supervisor
            .ask(|reply| SessionSupervisorCommand::Create {
                spec: spec.clone(),
                created_at: now_ms,
                reply,
            })
            .await
            .map_err(|_| RoutineError::Internal("session supervisor unavailable".into()))?;

        self.supervisor
            .ask(|reply| SessionSupervisorCommand::UserMessage {
                agent_id: None,
                id: id.clone(),
                text: routine.prompt.clone(),
                reply,
            })
            .await
            .map_err(|_| RoutineError::Internal("session supervisor unavailable".into()))?
            .map_err(|e| match e {
                UserMessageError::NotFound => {
                    RoutineError::Internal("the session vanished before its prompt".into())
                }
                UserMessageError::Unrecoverable(reason) => RoutineError::Conflict(reason),
                // A routine invokes an agent preset, never a workflow, so this
                // is unreachable rather than merely unlikely.
                UserMessageError::Rejected(why) => RoutineError::Conflict(why),
            })?;

        let status = SessionStatus::Idle;
        Ok(SessionSummary {
            id,
            name: spec.name.clone(),
            status: status_kind(&status),
            created_at: now_ms,
            last_error: status_reason(&status),
            // A routine invokes an agent preset, never a workflow.
            workflow: None,
            // A run's session was just created; it has no annotations yet.
            annotations: vec![],
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
pub(crate) mod tests {
    use super::*;
    use crate::routines::service::tests::{Fixture, fixture, input};
    use crate::runtime_vendor::fake::FakeRuntimeVendor;
    use crate::sessions::spec::{ServerDeps, SessionSpec};
    use crate::sessions::supervisor::SessionSupervisor;
    use horsie_actor::{ActorSystem, InMemoryJournal, Journal};
    use horsie_models::routines::{EverySchedule, ManualSchedule, RoutineSchedule};
    use std::collections::HashMap;
    use std::time::Duration;

    pub(crate) struct RunnerFixture {
        pub runner: Arc<RoutineRunner>,
        pub routines: Arc<RoutineService>,
        pub supervisor: ActorRef<SessionSupervisorCommand>,
        /// Kept alive: dropping it closes the fake vendor's transport.
        pub _vendor: FakeRuntimeVendor,
        pub _fixture: Fixture,
    }

    /// A runner over a real supervisor with a fake `mock` runtime vendor.
    /// `connected` false leaves the vendor map empty, which is how a routine
    /// whose runtime is offline is tested.
    pub(crate) async fn runner_fixture(connected: bool) -> RunnerFixture {
        let f = fixture().await;
        let vendor = FakeRuntimeVendor::builder("mock")
            .serve_in_process()
            .await
            .expect("fake vendor");
        let mut map: HashMap<String, Arc<dyn crate::runtime_vendor::RuntimeVendor>> =
            HashMap::new();
        if connected {
            map.insert(
                "mock".into(),
                vendor.link() as Arc<dyn crate::runtime_vendor::RuntimeVendor>,
            );
        }
        let vendors = Arc::new(std::sync::RwLock::new(map));
        let registry = Arc::new(RuntimeVendorRegistry::new(vendors.clone()));
        let deps = ServerDeps {
            runtimes: crate::runtime_manager::test_runtime_manager(&vendors),
            provider_registry: f.provider_registry.clone(),
            vendors,
            github_tokens: None,
            mcp: None,
            plugins: None,
            memory: None,
        };
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let (gtx, _) = tokio::sync::broadcast::channel(64);
        let supervisor = crate::testing::spawn_detached(
            &ActorSystem::new(journal.clone()),
            SessionSupervisor::new(crate::auth::UserId::bootstrap(), deps, gtx),
        );
        let runner = Arc::new(RoutineRunner::new(
            f.routines.clone(),
            f.agents.clone(),
            f.environments.clone(),
            f.config.clone(),
            registry,
            supervisor.clone(),
        ));
        RunnerFixture {
            runner,
            routines: f.routines.clone(),
            supervisor,
            _vendor: vendor,
            _fixture: f,
        }
    }

    /// Every session the supervisor knows, with its spec.
    pub(crate) async fn sessions(
        sup: &ActorRef<SessionSupervisorCommand>,
    ) -> Vec<(String, SessionSpec)> {
        sup.ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap()
            .into_iter()
            .map(|(id, rec)| (id, rec.spec))
            .collect()
    }

    /// Whether the prompt reached the session's agent as a user message.
    /// Read through the supervisor (the actor owns its journal), and polled
    /// because the turn starts on its own task.
    async fn prompt_reached_the_agent(
        sup: &ActorRef<SessionSupervisorCommand>,
        id: &str,
        prompt: &str,
    ) -> bool {
        for _ in 0..100 {
            let page = sup
                .ask(|reply| SessionSupervisorCommand::PageLog {
                    id: id.to_string(),
                    agent_id: None,
                    before: None,
                    max: 50,
                    reply,
                })
                .await
                .unwrap();
            if let Some(page) = page
                && page
                    .messages()
                    .any(|m| serde_json::to_string(m).is_ok_and(|json| json.contains(prompt)))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_run_creates_an_unattended_session_carrying_the_prompt() {
        let f = runner_fixture(true).await;
        f.routines
            .create(input("nightly", None), 1_000)
            .await
            .unwrap();

        let summary = f.runner.run("nightly", 2_000).await.unwrap();
        assert_eq!(summary.created_at, 2_000);
        assert_eq!(summary.name.as_deref(), Some("nightly"));

        let sessions = sessions(&f.supervisor).await;
        assert_eq!(sessions.len(), 1);
        let (id, spec) = &sessions[0];
        assert_eq!(id, &summary.id);
        assert_eq!(spec.routine(), Some("nightly"));
        assert!(
            spec.is_unattended(),
            "a routine run has nobody to answer a question"
        );
        assert!(prompt_reached_the_agent(&f.supervisor, id, "triage the inbox").await);

        // And the run is on the record.
        let view = f.routines.get("nightly").await.unwrap();
        assert_eq!(view.last_session_id.as_deref(), Some(summary.id.as_str()));
        assert_eq!(view.last_run_at_ms, Some(2_000));
        assert_eq!(view.last_error, None);
    }

    #[tokio::test]
    async fn a_run_with_an_offline_vendor_records_the_failure_and_creates_nothing() {
        let f = runner_fixture(false).await;
        f.routines.create(input("nightly", None), 0).await.unwrap();

        let err = f.runner.run("nightly", 2_000).await.unwrap_err();
        assert!(matches!(err, RoutineError::Invalid(m) if m.contains("not connected")));
        assert!(sessions(&f.supervisor).await.is_empty());

        let view = f.routines.get("nightly").await.unwrap();
        assert_eq!(view.last_session_id, None);
        assert!(view.last_error.unwrap().contains("not connected"));
    }

    #[tokio::test]
    async fn a_run_against_a_deleted_environment_fails_visibly() {
        let f = runner_fixture(true).await;
        let mut i = input("nightly", None);
        i.environment = horsie_models::environments::EnvironmentSpec::Named(
            horsie_models::environments::NamedEnvironment {
                name: "staging".into(),
            },
        );
        f.routines.create(i, 0).await.unwrap();

        // Never created, so it is already the "deleted since it was saved"
        // case: the routine is re-resolved every run, and reports through
        // `last_error` rather than failing inside a session nobody watches.
        let err = f.runner.run("nightly", 1).await.unwrap_err();
        assert!(
            matches!(err, RoutineError::Invalid(ref m) if m.contains("staging")),
            "{err}"
        );
        assert!(sessions(&f.supervisor).await.is_empty());
        let view = f.routines.get("nightly").await.unwrap();
        assert!(view.last_error.unwrap().contains("staging"));
    }

    #[tokio::test]
    async fn a_run_against_a_deleted_agent_preset_fails_visibly() {
        let f = runner_fixture(true).await;
        f.routines.create(input("nightly", None), 0).await.unwrap();
        f._fixture.agents.delete("reviewer").await.unwrap();

        let err = f.runner.run("nightly", 1).await.unwrap_err();
        assert!(matches!(err, RoutineError::Invalid(m) if m.contains("reviewer")));
        assert!(sessions(&f.supervisor).await.is_empty());
    }

    #[tokio::test]
    async fn a_run_by_hand_leaves_the_timer_where_it_was() {
        // Pressing run is not a reason for the next scheduled firing to move.
        let f = runner_fixture(true).await;
        f.routines
            .create(
                input(
                    "hourly",
                    Some(RoutineSchedule::Every(EverySchedule {
                        interval_secs: 3_600,
                    })),
                ),
                1_000,
            )
            .await
            .unwrap();
        let armed = f.routines.get("hourly").await.unwrap().next_run_at_ms;

        f.runner.run("hourly", 2_000).await.unwrap();
        let after = f.routines.get("hourly").await.unwrap();
        assert_eq!(after.next_run_at_ms, armed);
        assert_eq!(after.last_run_at_ms, Some(2_000));
    }

    #[test]
    fn a_manual_schedule_is_never_due() {
        assert_eq!(
            crate::routines::next_run_at(&RoutineSchedule::Manual(ManualSchedule {}), true, 1_000),
            None
        );
    }
}
