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
use crate::sessions::addressing::SupervisorRef;
use crate::sessions::builder::{AgentChoice, SpecError, build_session_spec, build_workflow_spec};
use crate::sessions::session_actor::NewSessionMessage;
use crate::sessions::spec::{SessionOrigin, SessionStatus, status_kind, status_reason};
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::workflow::{ResolveRunError, resolve_run_with};
use crate::sessions::{CreateSessionError, UserMessageError};
use horsie_models::routines::RoutineTarget;
use horsie_models::session::{AgentSettings as WireAgentSettings, SessionSummary};
use std::sync::Arc;

pub struct RoutineRunner {
    routines: Arc<RoutineService>,
    agents: Arc<AgentService>,
    workflows: Arc<crate::workflows::WorkflowService>,
    environments: Arc<EnvironmentService>,
    config: Arc<dyn ConfigStore>,
    vendors: Arc<RuntimeVendorRegistry>,
    supervisor: SupervisorRef,
}

impl RoutineRunner {
    pub fn new(
        routines: Arc<RoutineService>,
        agents: Arc<AgentService>,
        workflows: Arc<crate::workflows::WorkflowService>,
        environments: Arc<EnvironmentService>,
        config: Arc<dyn ConfigStore>,
        vendors: Arc<RuntimeVendorRegistry>,
        supervisor: SupervisorRef,
    ) -> Self {
        Self {
            routines,
            agents,
            workflows,
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
        // Re-resolved every run, whichever it is: presets and workflows are
        // both editable, and a routine saved against one that has since been
        // deleted must fail here, visibly, rather than as a turn error inside
        // a session nobody is watching.
        match routine.target.clone() {
            RoutineTarget::Agent(a) => self.start_agent(&routine, &a.agent, now_ms).await,
            RoutineTarget::Workflow(w) => self.start_workflow(&routine, &w.workflow, now_ms).await,
        }
    }

    /// A routine that runs an agent preset: one ordinary session, with the
    /// routine's prompt as its first user message.
    async fn start_agent(
        &self,
        routine: &crate::routines::store::RoutineRow,
        preset: &str,
        now_ms: u64,
    ) -> Result<SessionSummary, RoutineError> {
        let agent = self
            .agents
            .get(preset)
            .await
            .map_err(|_| RoutineError::Invalid(format!("unknown agent preset '{preset}'")))?;

        let view = self.config.view().await.map_err(RoutineError::Internal)?;
        if !view.models.iter().any(|m| m.alias == agent.model) {
            return Err(RoutineError::Invalid(format!(
                "model '{}' is no longer configured",
                agent.model
            )));
        }

        let wire = WireAgentSettings {
            model: agent.model.clone(),
            // A routine is how you schedule control-plane work — "prune last
            // week's sessions every Monday" — so the preset's selection carries
            // into the sessions it fires, grant included, exactly as an
            // interactive invoke does.
            allowed_tools: agent.allowed_tools.clone(),
            use_plugins: None,
            max_iterations: None,
            max_retries: None,
            mcp_servers: Some(agent.mcp_servers.clone()),
            memory_spaces: Some(agent.memory_spaces.clone()),
            thinking_effort: agent.thinking_effort.clone(),
            max_concurrent_subagents: None,
            allow_recursive_delegation: None,
            instructions: agent.instructions.clone(),
            auto_compact: agent.auto_compact,
        };
        let spec = build_session_spec(
            &self.config,
            &self.environments,
            // A routine *is* a scheduled invoke of a preset, so its runs are
            // findable under that preset alongside the interactive ones — which
            // is exactly the history a tuning routine reads.
            AgentChoice::from_preset(wire, agent.name.clone()),
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
        // A routine whose environment asks for no runtime names no vendor, so
        // there is nothing to re-resolve.
        if let Some(vendor) = spec.vendor()
            && !self.vendors.connected_names().iter().any(|v| v == vendor)
        {
            return Err(RoutineError::Invalid(format!(
                "runtime vendor '{vendor}' is not connected"
            )));
        }

        // The prompt travels with the create. Two asks would be addressed
        // separately, and a shard that moved between them left the second one
        // talking to a supervisor that had never heard of the session.
        let id = self
            .supervisor
            .ask(|reply| SessionSupervisorCommand::Create {
                spec: spec.clone(),
                // Named for the routine so a run is recognisable before the
                // agent titles it; the agent may retitle it from what it
                // actually did.
                name: Some(routine.name.clone()),
                created_at: now_ms,
                message: Some(NewSessionMessage::text(routine.prompt.clone())),
                reply,
            })
            .await
            .map_err(|_| RoutineError::Internal("session supervisor unavailable".into()))?
            .map_err(|e| match e {
                CreateSessionError::NotRecorded(m) => RoutineError::Internal(m),
                CreateSessionError::Message(UserMessageError::NotFound) => {
                    RoutineError::Internal("the session vanished before its prompt".into())
                }
                CreateSessionError::Message(
                    // `Rejected` means "this session does not take messages",
                    // which is a workflow run. This arm creates a plain
                    // session, and the workflow arm creates its run with no
                    // message at all — so neither can reach it. Still matched
                    // rather than ignored: it carries a reason worth reporting
                    // if the shape of a session ever changes under it.
                    UserMessageError::Unrecoverable(why) | UserMessageError::Rejected(why),
                ) => RoutineError::Conflict(why),
            })?
            .id;

        let status = SessionStatus::Idle;
        Ok(SessionSummary {
            id,
            name: Some(routine.name.clone()),
            status: status_kind(&status),
            created_at: now_ms,
            last_error: status_reason(&status),
            // This arm invokes an agent preset; the workflow arm names its
            // definition here.
            workflow: None,
            // A run's session was just created; it has no annotations yet.
            annotations: vec![],
            // Nor sub sessions: nobody has had a session in it to branch.
            sub_sessions: vec![],
        })
    }

    /// A routine that runs a workflow: one run, with the routine's prompt as
    /// the input its start step is handed.
    ///
    /// No message is queued. A run is started by being created — the session
    /// actor asks the orchestrator what to do at load, and a pending run's
    /// answer is its first step — and a run refuses user messages outright.
    async fn start_workflow(
        &self,
        routine: &crate::routines::store::RoutineRow,
        workflow: &str,
        now_ms: u64,
    ) -> Result<SessionSummary, RoutineError> {
        // Every step's preset resolved once, here, exactly as an interactive
        // run does it. After this the run is self-contained, so a preset edited
        // between the trigger and a later step cannot change that step.
        let resolved = resolve_run_with(
            &self.workflows,
            &self.agents,
            &self.config,
            workflow,
            &routine.prompt,
        )
        .await
        .map_err(|e| match e {
            ResolveRunError::NotFound(m) => RoutineError::Invalid(m),
            ResolveRunError::Invalid(m) => RoutineError::Invalid(m),
            ResolveRunError::Internal(m) => RoutineError::Internal(m),
        })?;
        let spec = build_workflow_spec(
            &self.environments,
            routine.environment.clone(),
            resolved.plugins,
            resolved.run,
            // What keeps a routine's runs out of the session list, the same as
            // its agent sessions.
            SessionOrigin::Routine {
                routine: routine.name.clone(),
            },
        )
        .await
        .map_err(|e| match e {
            SpecError::Invalid(m) => RoutineError::Invalid(m),
            SpecError::Internal(m) => RoutineError::Internal(m),
        })?;
        if let Some(vendor) = spec.vendor()
            && !self.vendors.connected_names().iter().any(|v| v == vendor)
        {
            return Err(RoutineError::Invalid(format!(
                "runtime vendor '{vendor}' is not connected"
            )));
        }

        let id = self
            .supervisor
            .ask(|reply| SessionSupervisorCommand::Create {
                spec: spec.clone(),
                name: Some(routine.name.clone()),
                created_at: now_ms,
                // A run carries its input in its snapshot, not in an inbox.
                message: None,
                reply,
            })
            .await
            .map_err(|_| RoutineError::Internal("session supervisor unavailable".into()))?
            .map_err(|e| match e {
                CreateSessionError::NotRecorded(m) => RoutineError::Internal(m),
                CreateSessionError::Message(
                    UserMessageError::NotFound
                    | UserMessageError::Unrecoverable(_)
                    | UserMessageError::Rejected(_),
                ) => RoutineError::Internal(
                    "a run was created with no message and still reported one".into(),
                ),
            })?
            .id;

        let status = SessionStatus::Idle;
        Ok(SessionSummary {
            id,
            name: Some(routine.name.clone()),
            status: status_kind(&status),
            created_at: now_ms,
            last_error: status_reason(&status),
            // What makes the session recognisable as a run wherever one is
            // listed or opened — including the run page the whole session view
            // now switches to.
            workflow: Some(workflow.to_string()),
            annotations: vec![],
            sub_sessions: vec![],
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
    use crate::sessions::supervisor::SupervisorConfig;
    use horsie_models::routines::{EverySchedule, ManualSchedule, RoutineSchedule};
    use std::collections::HashMap;
    use std::time::Duration;

    pub(crate) struct RunnerFixture {
        pub runner: Arc<RoutineRunner>,
        pub routines: Arc<RoutineService>,
        pub supervisor: SupervisorRef,
        /// Kept alive: dropping it closes the fake vendor's transport.
        pub _vendor: FakeRuntimeVendor,
        pub _fixture: Fixture,
        /// Kept alive: the registry the supervisor resolves its account
        /// through is reached weakly, so letting this go would take the
        /// account with it.
        pub _node: crate::testing::Deployment,
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
            artifacts: None,
            project: crate::projects::ProjectId::new("p-test"),
            runtimes: crate::runtime_manager::test_runtime_manager(&vendors),
            provider_registry: f.provider_registry.clone(),
            vendors,
            github_tokens: None,
            mcp: None,
            plugins: None,
            memory: None,
        };
        let node = crate::testing::Deployment::new(
            deps,
            SupervisorConfig {
                // No background ticker: nothing here is about going idle, and a
                // sweep nobody asked for is a race.
                tick_interval: None,
                ..SupervisorConfig::default()
            },
        )
        .await;
        let supervisor = node.supervisor().await;
        let runner = Arc::new(RoutineRunner::new(
            f.routines.clone(),
            f.agents.clone(),
            f.workflows.clone(),
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
            _node: node,
        }
    }

    /// Every session the supervisor knows, with its spec.
    pub(crate) async fn sessions(sup: &SupervisorRef) -> Vec<(String, SessionSpec)> {
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
    async fn prompt_reached_the_agent(sup: &SupervisorRef, id: &str, prompt: &str) -> bool {
        for _ in 0..100 {
            let page = sup
                .ask(|reply| SessionSupervisorCommand::PageLog {
                    id: id.to_string(),
                    agent_id: None,
                    anchor: crate::agent_loop::Anchor::Tail,
                    max: 50,
                    filter: crate::agent_loop::LogFilter::everything(),
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
