//! Turning a routine into a running session.
//!
//! Every trigger — the run endpoint, the web UI button, the scheduler's timer —
//! goes through [`RoutineRunner::run`], so all three make the same liveness
//! checks and produce the same kind of session. It is not an HTTP type: the
//! scheduler has no request to answer.

use crate::agents::AgentService;
use crate::config::ConfigStore;
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
    config: Arc<dyn ConfigStore>,
    vendors: Arc<RuntimeVendorRegistry>,
    supervisor: ActorRef<SessionSupervisorCommand>,
}

impl RoutineRunner {
    pub fn new(
        routines: Arc<RoutineService>,
        agents: Arc<AgentService>,
        config: Arc<dyn ConfigStore>,
        vendors: Arc<RuntimeVendorRegistry>,
        supervisor: ActorRef<SessionSupervisorCommand>,
    ) -> Self {
        Self {
            routines,
            agents,
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

        // A preset names no vendor, so every run resolves the server default.
        // Deliberately not a per-routine pin: a routine is unattended, so a pin
        // that goes stale fails every interval with nobody watching, and
        // `next_run` waits the interval out rather than disabling the routine.
        let vendor = self.config.default_vendor();
        if !self.vendors.connected_names().contains(&vendor) {
            return Err(RoutineError::Invalid(format!(
                "runtime vendor '{vendor}' is not connected"
            )));
        }
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
        };
        let spec = build_session_spec(
            &self.config,
            // Named for the routine so a run is recognisable before the agent
            // titles it; the agent may retitle it from what it actually did.
            Some(routine.name.clone()),
            wire,
            Some(vendor),
            agent.repos.clone(),
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
            status: Some(status_kind(&status)),
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
    use crate::routines::scheduler::RoutineScheduler;
    use crate::routines::service::tests::{Fixture, fixture, input};
    use crate::routines::store::Schedule;
    use crate::runtime_vendor::RuntimeVendorLink;
    use crate::runtime_vendor::fake::FakeRuntimeVendor;
    use crate::sessions::spec::{ServerDeps, SessionSpec};
    use crate::sessions::supervisor::SessionSupervisor;
    use horsie_actor::{InMemoryJournal, Journal, spawn_root};
    use horsie_models::routines::{EverySchedule, OnceSchedule, RoutineSchedule};
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
        let mut map: HashMap<String, Arc<RuntimeVendorLink>> = HashMap::new();
        if connected {
            map.insert("mock".into(), vendor.link());
        }
        let vendors = Arc::new(std::sync::RwLock::new(map));
        let registry = Arc::new(RuntimeVendorRegistry::new(vendors.clone()));
        let deps = ServerDeps {
            runtimes: crate::runtime_manager::test_runtime_manager(&vendors, f.tmp.path()),
            provider_registry: f.provider_registry.clone(),
            vendors,
            state_dir: f.tmp.path().to_path_buf(),
            github_tokens: None,
            mcp: None,
            plugins: None,
            memory: None,
        };
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let (gtx, _) = tokio::sync::broadcast::channel(64);
        let supervisor = spawn_root(SessionSupervisor::new(deps, gtx), journal.clone());
        let runner = Arc::new(RoutineRunner::new(
            f.routines.clone(),
            f.agents.clone(),
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
    async fn sessions(sup: &ActorRef<SessionSupervisorCommand>) -> Vec<(String, SessionSpec)> {
        sup.ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap()
            .into_iter()
            .map(|(id, rec, _)| (id, rec.spec))
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
                .ask(|reply| SessionSupervisorCommand::History {
                    id: id.to_string(),
                    agent_id: None,
                    query: horsie_workflow::HistoryQuery {
                        before: None,
                        after: None,
                        limit: 50,
                    },
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

    #[tokio::test]
    async fn the_scheduler_fires_only_once_due_and_re_arms_the_interval() {
        let f = runner_fixture(true).await;
        let scheduler = RoutineScheduler::new(f.runner.clone(), f.routines.clone());
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
        // Armed for now + 1h.
        assert_eq!(
            f.routines.get("hourly").await.unwrap().next_run_at_ms,
            Some(3_601_000)
        );

        scheduler.tick(3_600_999).await;
        assert!(sessions(&f.supervisor).await.is_empty(), "not due yet");

        scheduler.tick(3_601_000).await;
        assert_eq!(sessions(&f.supervisor).await.len(), 1);
        let view = f.routines.get("hourly").await.unwrap();
        assert_eq!(
            view.next_run_at_ms,
            Some(3_601_000 + 3_600_000),
            "the next firing is measured from this one, not from the last due time"
        );

        // A second tick at the same instant must not fire it again.
        scheduler.tick(3_601_000).await;
        assert_eq!(sessions(&f.supervisor).await.len(), 1);
    }

    #[tokio::test]
    async fn a_once_routine_never_re_arms() {
        let f = runner_fixture(true).await;
        let scheduler = RoutineScheduler::new(f.runner.clone(), f.routines.clone());
        f.routines
            .create(
                input(
                    "launch",
                    Some(RoutineSchedule::Once(OnceSchedule { at_ms: 5_000 })),
                ),
                1_000,
            )
            .await
            .unwrap();

        scheduler.tick(5_000).await;
        assert_eq!(sessions(&f.supervisor).await.len(), 1);
        let view = f.routines.get("launch").await.unwrap();
        assert_eq!(view.next_run_at_ms, None);
        assert_eq!(
            view.schedule,
            RoutineSchedule::Once(OnceSchedule { at_ms: 5_000 })
        );

        scheduler.tick(9_999).await;
        assert_eq!(sessions(&f.supervisor).await.len(), 1);
    }

    #[tokio::test]
    async fn a_failed_run_still_advances_the_schedule() {
        // Otherwise a routine whose vendor is offline is retried on every
        // 15-second tick, which is a hot loop rather than a schedule.
        let f = runner_fixture(false).await;
        let scheduler = RoutineScheduler::new(f.runner.clone(), f.routines.clone());
        f.routines
            .create(
                input(
                    "hourly",
                    Some(RoutineSchedule::Every(EverySchedule {
                        interval_secs: 3_600,
                    })),
                ),
                0,
            )
            .await
            .unwrap();

        scheduler.tick(3_600_000).await;
        let view = f.routines.get("hourly").await.unwrap();
        assert!(view.last_error.is_some());
        assert_eq!(view.next_run_at_ms, Some(7_200_000));
        assert!(f.routines.due(3_600_001).await.unwrap().is_empty());
    }

    #[test]
    fn a_manual_schedule_is_never_due() {
        assert_eq!(
            crate::routines::next_run_at(&Schedule::Manual, true, 1_000),
            None
        );
    }
}
