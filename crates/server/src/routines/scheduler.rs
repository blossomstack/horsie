//! The routine timer: one clock for the whole deployment.
//!
//! It owns no schedule logic of its own — it asks which routines have come due,
//! works out where each one's next firing lands, and hands both to that
//! routine's owner. Ticking is a plain method taking `now_ms` so tests drive
//! time rather than sleep through it.
//!
//! One timer rather than one per account, deliberately: a timer per dormant
//! account is exactly what building account services lazily is for avoiding.
//! So the read is [`RoutineStore::due_across_all_users`], which is unscoped and
//! on the scope audit's allowlist for this reason, and each routine is then
//! armed and run *as its owner*.

use crate::db::Db;
use crate::projects::ProjectId;
use crate::projects::ProjectRegistry;
use crate::routines::service::next_run_at;
use crate::routines::store::{RoutineRow, RoutineStore};
use std::sync::Arc;
use std::time::Duration;

/// How often the timer looks for due routines. Well under the 60s minimum
/// interval, so a routine fires within a tick of when it was due.
pub const TICK_INTERVAL: Duration = Duration::from_secs(15);

pub struct RoutineScheduler {
    db: Db,
    projects: Arc<ProjectRegistry>,
}

impl RoutineScheduler {
    #[must_use]
    pub fn new(db: Db, projects: Arc<ProjectRegistry>) -> Self {
        Self { db, projects }
    }

    /// Fire every routine due at `now_ms`, whoever owns it.
    ///
    /// Each one is *claimed first* — its timer moved to the next firing before
    /// the run is started. Two things follow, both deliberate: a run that takes
    /// longer than a tick cannot be found still-due and started twice, and a
    /// routine whose vendor is offline waits out its interval instead of being
    /// retried on every tick.
    ///
    /// One account's failure never costs another its tick: resolving a bundle,
    /// arming, and running are all per-routine, and every failure continues.
    pub async fn tick(&self, now_ms: u64) {
        let due = match RoutineStore::due_across_all_users(&self.db, now_ms).await {
            Ok(due) => due,
            Err(e) => {
                tracing::error!(error = %e, "reading due routines failed");
                return;
            }
        };
        for (owner, routine) in due {
            self.fire(&owner, routine, now_ms).await;
        }
    }

    async fn fire(&self, owner: &ProjectId, routine: RoutineRow, now_ms: u64) {
        let services = match self.projects.get(owner).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    user = %owner,
                    routine = %routine.name,
                    error = %e,
                    "could not resolve the account a due routine belongs to"
                );
                return;
            }
        };
        let next = next_run_at(&routine.schedule, routine.enabled, now_ms);
        if let Err(e) = services.routines.arm(&routine.name, next).await {
            // Unclaimed, so skipped: running it now would leave it due and
            // firing again on the next tick, and every tick after that.
            tracing::error!(routine = %routine.name, error = %e, "arming a routine failed");
            return;
        }
        if let Err(e) = services.routine_runner.run(&routine.name, now_ms).await {
            tracing::warn!(routine = %routine.name, error = %e, "routine run did not start");
        }
    }

    /// Run the timer until the process ends.
    pub fn spawn(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(TICK_INTERVAL);
            ticker.tick().await; // the first tick fires immediately
            loop {
                ticker.tick().await;
                self.tick(horsie_models::now_ms()).await;
            }
        });
    }
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
    use crate::projects::{ProjectServices, Shared};
    use crate::routines::runner::tests::sessions;
    use crate::runtime_vendor::fake::FakeRuntimeVendor;
    use horsie_models::agents::AgentPresetInput;
    use horsie_models::routines::{
        EverySchedule, OnceSchedule, RoutineInput, RoutineSchedule, Weekday, WeeklySchedule,
    };
    use horsie_models::settings::{ModelInput, ProviderInput};

    /// One account, configured far enough that a routine can actually start a
    /// session: a provider, a model, an agent preset, and a `mock` vendor process
    /// published in *its own* map.
    struct Account {
        services: Arc<ProjectServices>,
        /// Kept alive: dropping it closes the fake vendor's transport.
        _vendor: Option<FakeRuntimeVendor>,
    }

    fn info() -> horsie_models::settings::ServerInfo {
        horsie_models::settings::ServerInfo {
            config_path: String::new(),
            database: String::new(),
            state_dir: String::new(),
            data_dir: String::new(),
            plugins_dir: String::new(),
            version: "test".into(),
        }
    }

    /// A project of the bootstrap account, which is what the scheduler fires
    /// for. Through `default_project`, so the id is minted the way production
    /// mints it.
    async fn another_project(reg: &ProjectRegistry, name: &str) -> ProjectId {
        reg.shared()
            .project_service
            .create(&crate::auth::UserId::bootstrap(), name)
            .await
            .expect("a second project is created")
            .id
    }

    async fn a_project(reg: &ProjectRegistry) -> ProjectId {
        reg.shared()
            .project_service
            .default_project(&crate::auth::UserId::bootstrap())
            .await
            .expect("the bootstrap account gets a default project")
            .id
    }

    fn registry(db: Db, tmp: &tempfile::TempDir) -> Arc<ProjectRegistry> {
        let users = Arc::new(ProjectRegistry::new(Arc::new(Shared {
            bus: Arc::new(crate::bus::MemoryBus::new()),
            system: crate::projects::node_system(&db, None),
            serving: None,
            project_service: Arc::new(crate::projects::ProjectService::new(db.clone())),
            db,
            artifacts: Arc::new(crate::plugins::ArtifactStore::new(
                tmp.path().join("artifacts"),
            )),
            info: info(),
            model_card_seed: Arc::new(Vec::new()),
            model_card_seed_marker: crate::config::model_cards::seed_marker(&[]),
            anonymous: crate::auth::UserId::bootstrap(),
            supervisor: crate::sessions::supervisor::SupervisorConfig::default(),
            deps: None,
            fly_api_base: crate::testing::UNREACHABLE_FLY_API.to_string(),
        })));
        crate::projects::register_session_shards(&users).unwrap();
        users
    }

    /// `connected` false leaves the account's vendor map empty, which is how a
    /// routine whose runtime is offline is tested.
    async fn account(users: &ProjectRegistry, user: &ProjectId, connected: bool) -> Account {
        let services = users.get(user).await.unwrap();
        // Through the trait object, so the per-resource calls rather than the
        // concrete store's test seed helper.
        services
            .config_store
            .upsert_provider(ProviderInput {
                name: "p".into(),
                kind: "anthropic".into(),
                base_url: Some("http://localhost:1".into()),
                api_key: Some("sk-x".into()),
                keep_thinking_signature: None,
            })
            .await
            .unwrap();
        services
            .config_store
            .upsert_model(ModelInput {
                alias: "sonnet".into(),
                provider: "p".into(),
                model_id: "claude-sonnet-4-6".into(),
                max_tokens: None,
                context_window: None,
                thinking_efforts: None,
                thinking_effort: None,
                thinking_dialect: None,
                forced_tools_disable_thinking: None,
            })
            .await
            .unwrap();
        services
            .agents
            .create(AgentPresetInput {
                name: "reviewer".into(),
                description: None,
                instructions: None,
                model: "sonnet".into(),
                plugins: None,
                mcp_servers: None,
                memory_spaces: None,
                thinking_effort: None,
                auto_compact: None,
                allowed_tools: None,
                tunable: None,
                expected_revision: None,
            })
            .await
            .unwrap();
        let vendor = if connected {
            let v = FakeRuntimeVendor::builder("mock")
                .serve_in_process()
                .await
                .expect("fake vendor");
            services.connected_vendors.publish(v.link()).unwrap();
            Some(v)
        } else {
            None
        };
        Account {
            services,
            _vendor: vendor,
        }
    }

    fn routine(name: &str, schedule: RoutineSchedule) -> RoutineInput {
        RoutineInput {
            environment: horsie_models::environments::EnvironmentSpec::Runtime(
                horsie_models::environments::RuntimeEnvironment {
                    vendor: "mock".into(),
                    repos: None,
                },
            ),
            name: name.into(),
            description: Some("d".into()),
            agent: "reviewer".into(),
            prompt: "triage the inbox".into(),
            schedule: Some(schedule),
            enabled: None,
        }
    }

    #[tokio::test]
    async fn fires_only_once_due_and_re_arms_the_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::testing::db().await;
        let users = registry(db.clone(), &tmp);
        let a = account(&users, &a_project(&users).await, true).await;
        let scheduler = RoutineScheduler::new(db, users);

        a.services
            .routines
            .create(
                routine(
                    "hourly",
                    RoutineSchedule::Every(EverySchedule {
                        interval_secs: 3_600,
                    }),
                ),
                1_000,
            )
            .await
            .unwrap();
        assert_eq!(
            a.services
                .routines
                .get("hourly")
                .await
                .unwrap()
                .next_run_at_ms,
            Some(3_601_000)
        );

        scheduler.tick(3_600_999).await;
        assert!(
            sessions(&a.services.supervisor).await.is_empty(),
            "not due yet"
        );

        scheduler.tick(3_601_000).await;
        assert_eq!(sessions(&a.services.supervisor).await.len(), 1);
        assert_eq!(
            a.services
                .routines
                .get("hourly")
                .await
                .unwrap()
                .next_run_at_ms,
            Some(3_601_000 + 3_600_000),
            "the next firing is measured from this one, not from the last due time"
        );

        // A second tick at the same instant must not fire it again.
        scheduler.tick(3_601_000).await;
        assert_eq!(sessions(&a.services.supervisor).await.len(), 1);
    }

    #[tokio::test]
    async fn a_once_routine_never_re_arms() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::testing::db().await;
        let users = registry(db.clone(), &tmp);
        let a = account(&users, &a_project(&users).await, true).await;
        let scheduler = RoutineScheduler::new(db, users);

        a.services
            .routines
            .create(
                routine(
                    "launch",
                    RoutineSchedule::Once(OnceSchedule { at_ms: 5_000 }),
                ),
                1_000,
            )
            .await
            .unwrap();

        scheduler.tick(5_000).await;
        assert_eq!(sessions(&a.services.supervisor).await.len(), 1);
        let view = a.services.routines.get("launch").await.unwrap();
        assert_eq!(view.next_run_at_ms, None);

        scheduler.tick(9_999).await;
        assert_eq!(sessions(&a.services.supervisor).await.len(), 1);
    }

    /// Otherwise a routine whose vendor is offline is retried on every
    /// 15-second tick, which is a hot loop rather than a schedule.
    #[tokio::test]
    async fn a_failed_run_still_advances_the_schedule() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::testing::db().await;
        let users = registry(db.clone(), &tmp);
        let a = account(&users, &a_project(&users).await, false).await;
        let scheduler = RoutineScheduler::new(db.clone(), users);

        a.services
            .routines
            .create(
                routine(
                    "hourly",
                    RoutineSchedule::Every(EverySchedule {
                        interval_secs: 3_600,
                    }),
                ),
                0,
            )
            .await
            .unwrap();

        scheduler.tick(3_600_000).await;
        let view = a.services.routines.get("hourly").await.unwrap();
        assert!(view.last_error.is_some());
        assert_eq!(view.next_run_at_ms, Some(7_200_000));
        assert!(
            RoutineStore::due_across_all_users(&db, 3_600_001)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_weekly_routine_fires_once_due_and_re_arms_to_the_next_weekday() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::testing::db().await;
        let users = registry(db.clone(), &tmp);
        let a = account(&users, &a_project(&users).await, true).await;
        let scheduler = RoutineScheduler::new(db, users);

        // 1970-01-01T00:00:01Z is a Thursday; Mon/Wed/Fri 09:00 UTC → Friday 09:00.
        a.services
            .routines
            .create(
                routine(
                    "triages",
                    RoutineSchedule::Weekly(WeeklySchedule {
                        timezone: "UTC".into(),
                        hour: 9,
                        minute: 0,
                        weekdays: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
                    }),
                ),
                1_000,
            )
            .await
            .unwrap();
        let first = a
            .services
            .routines
            .get("triages")
            .await
            .unwrap()
            .next_run_at_ms;
        // Friday 09:00:00 UTC = Thursday 00:00:00 + 24h + 9h.
        assert_eq!(first, Some((24 + 9) * 3_600 * 1_000));

        scheduler.tick(first.unwrap() - 1).await;
        assert!(
            sessions(&a.services.supervisor).await.is_empty(),
            "not due yet"
        );

        scheduler.tick(first.unwrap()).await;
        assert_eq!(sessions(&a.services.supervisor).await.len(), 1);
        let view = a.services.routines.get("triages").await.unwrap();
        // Friday 09:00 fired → re-arms to Monday 09:00, three days later.
        assert_eq!(
            view.next_run_at_ms,
            Some(first.unwrap() + 3 * 24 * 3_600 * 1_000)
        );

        scheduler.tick(first.unwrap()).await;
        assert_eq!(
            sessions(&a.services.supervisor).await.len(),
            1,
            "no double fire"
        );
    }

    /// One timer, two accounts: each routine fires in its owner's scope, on its
    /// owner's runtime. The scoped read this replaced would have seen only one
    /// of these two.
    #[tokio::test]
    async fn every_project_gets_its_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::testing::db().await;
        let users = registry(db.clone(), &tmp);
        let (one, two) = (
            a_project(&users).await,
            another_project(&users, "second").await,
        );
        let a = account(&users, &one, true).await;
        let b = account(&users, &two, true).await;
        let scheduler = RoutineScheduler::new(db, users);

        for acct in [&a, &b] {
            acct.services
                .routines
                .create(
                    routine(
                        "nightly",
                        RoutineSchedule::Once(OnceSchedule { at_ms: 5_000 }),
                    ),
                    1_000,
                )
                .await
                .unwrap();
        }

        scheduler.tick(5_000).await;
        assert_eq!(sessions(&a.services.supervisor).await.len(), 1);
        assert_eq!(sessions(&b.services.supervisor).await.len(), 1);
    }
}
