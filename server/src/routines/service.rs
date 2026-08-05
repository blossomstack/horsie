//! Validation, timestamps, and row↔wire mapping over [`RoutineStore`].
//!
//! Save-time validation covers what is stable at save: the name slug, the
//! prompt, the interval floor, and that the named agent preset exists. The
//! agent's own contents — vendor connectivity, whether its model is still
//! configured — are live state, re-checked by the runner at every trigger.

use crate::agents::AgentService;
use crate::routines::store::{RoutineRow, RoutineStore, RunOutcome, Schedule};
use horsie_models::routines::{
    EverySchedule, ManualSchedule, OnceSchedule, RoutineInput, RoutineSchedule, RoutineView,
};
use std::sync::Arc;

/// The shortest interval a recurring routine may use. A floor rather than a
/// concurrency guard: runs are not prevented from overlapping, so the cadence
/// has to leave room for one to finish.
pub const MIN_INTERVAL_SECS: u64 = 60;

/// Typed service errors so the HTTP layer can pick a status without string
/// matching: NotFound → 404, Conflict → 409, Invalid → 422, Internal → 500.
#[derive(Debug)]
pub enum RoutineError {
    NotFound(String),
    Conflict(String),
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for RoutineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Conflict(m) | Self::Invalid(m) | Self::Internal(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for RoutineError {}

/// When a schedule should next fire, given the moment it was armed.
///
/// `Every` is measured from `now`, not from a fixed origin: a server that was
/// down for a day resumes with one run rather than a day of backlog. `Once`
/// only fires if its instant is still ahead; a paused routine never fires.
pub fn next_run_at(schedule: &Schedule, enabled: bool, now_ms: u64) -> Option<u64> {
    if !enabled {
        return None;
    }
    match schedule {
        Schedule::Manual => None,
        Schedule::Every { interval_secs } => Some(now_ms.saturating_add(interval_secs * 1_000)),
        Schedule::Once { at_ms } => (*at_ms > now_ms).then_some(*at_ms),
    }
}

pub struct RoutineService {
    store: RoutineStore,
    agents: Arc<AgentService>,
}

impl RoutineService {
    pub fn new(store: RoutineStore, agents: Arc<AgentService>) -> Self {
        Self { store, agents }
    }

    pub async fn list(&self) -> Result<Vec<RoutineView>, RoutineError> {
        Ok(self
            .store
            .list()
            .await
            .map_err(RoutineError::Internal)?
            .iter()
            .map(routine_view)
            .collect())
    }

    pub async fn get(&self, name: &str) -> Result<RoutineView, RoutineError> {
        self.row(name).await.map(|r| routine_view(&r))
    }

    /// The storage row, for callers that need more than the wire view (the
    /// runner needs the prompt and the schedule).
    pub async fn row(&self, name: &str) -> Result<RoutineRow, RoutineError> {
        self.store
            .get(name)
            .await
            .map_err(RoutineError::Internal)?
            .ok_or_else(|| RoutineError::NotFound(format!("unknown routine '{name}'")))
    }

    pub async fn create(
        &self,
        input: RoutineInput,
        now_ms: u64,
    ) -> Result<RoutineView, RoutineError> {
        let (schedule, enabled) = self.validate(&input).await?;
        if self
            .store
            .get(&input.name)
            .await
            .map_err(RoutineError::Internal)?
            .is_some()
        {
            return Err(RoutineError::Conflict(format!(
                "routine '{}' already exists",
                input.name
            )));
        }
        let now = now_secs();
        let row = row_from_input(input, schedule, enabled, now_ms, now.clone(), now);
        self.store
            .insert(&row)
            .await
            .map_err(RoutineError::Internal)?;
        self.get(&row.name).await
    }

    /// Full replace. The path name is the id of record: a body naming a
    /// different routine is invalid rather than a rename. Re-arms the schedule
    /// from `now_ms`; run history is left alone.
    pub async fn replace(
        &self,
        name: &str,
        input: RoutineInput,
        now_ms: u64,
    ) -> Result<RoutineView, RoutineError> {
        if input.name != name {
            return Err(RoutineError::Invalid(
                "routine name is immutable; the path is the id of record".to_string(),
            ));
        }
        let existing = self.row(name).await?;
        let (schedule, enabled) = self.validate(&input).await?;
        let row = row_from_input(
            input,
            schedule,
            enabled,
            now_ms,
            existing.created_at,
            now_secs(),
        );
        self.store
            .replace(&row)
            .await
            .map_err(RoutineError::Internal)?;
        self.get(name).await
    }

    pub async fn delete(&self, name: &str) -> Result<(), RoutineError> {
        if self
            .store
            .delete(name)
            .await
            .map_err(RoutineError::Internal)?
        {
            Ok(())
        } else {
            Err(RoutineError::NotFound(format!("unknown routine '{name}'")))
        }
    }

    /// Enabled routines that have come due.
    pub async fn due(&self, now_ms: u64) -> Result<Vec<RoutineRow>, RoutineError> {
        self.store.due(now_ms).await.map_err(RoutineError::Internal)
    }

    /// Names of the routines that would run a given agent preset.
    pub async fn using_agent(&self, agent: &str) -> Result<Vec<String>, RoutineError> {
        self.store
            .using_agent(agent)
            .await
            .map_err(RoutineError::Internal)
    }

    /// Move the timer to its next firing (the scheduler's claim, taken before
    /// a run starts).
    pub async fn arm(&self, name: &str, next_run_at_ms: Option<u64>) -> Result<(), RoutineError> {
        self.store
            .arm(name, next_run_at_ms)
            .await
            .map_err(RoutineError::Internal)
    }

    pub async fn record_run(
        &self,
        name: &str,
        at_ms: u64,
        outcome: &RunOutcome,
    ) -> Result<(), RoutineError> {
        self.store
            .record_run(name, at_ms, outcome)
            .await
            .map_err(RoutineError::Internal)
    }

    /// Save-time validation, returning the resolved schedule and enabled flag.
    async fn validate(&self, input: &RoutineInput) -> Result<(Schedule, bool), RoutineError> {
        crate::memory::validate_slug(&input.name).map_err(RoutineError::Invalid)?;
        if input.prompt.trim().is_empty() {
            return Err(RoutineError::Invalid(
                "prompt must not be empty — it is the whole instruction a run gets".to_string(),
            ));
        }
        // The reference is checked here so a routine cannot be saved broken;
        // it is checked again at every trigger, because presets are editable.
        self.agents.get(&input.agent).await.map_err(|_| {
            RoutineError::Invalid(format!("unknown agent preset '{}'", input.agent))
        })?;
        let schedule = storage_schedule(input.schedule.as_ref());
        if let Schedule::Every { interval_secs } = &schedule
            && *interval_secs < MIN_INTERVAL_SECS
        {
            return Err(RoutineError::Invalid(format!(
                "interval must be at least {MIN_INTERVAL_SECS} seconds"
            )));
        }
        Ok((schedule, input.enabled.unwrap_or(true)))
    }
}

/// Wire schedule → storage schedule; an absent schedule means manual.
fn storage_schedule(wire: Option<&RoutineSchedule>) -> Schedule {
    match wire {
        None | Some(RoutineSchedule::Manual(_)) => Schedule::Manual,
        Some(RoutineSchedule::Every(EverySchedule { interval_secs })) => Schedule::Every {
            interval_secs: *interval_secs,
        },
        Some(RoutineSchedule::Once(OnceSchedule { at_ms })) => Schedule::Once { at_ms: *at_ms },
    }
}

fn wire_schedule(schedule: &Schedule) -> RoutineSchedule {
    match schedule {
        Schedule::Manual => RoutineSchedule::Manual(ManualSchedule {}),
        Schedule::Every { interval_secs } => RoutineSchedule::Every(EverySchedule {
            interval_secs: *interval_secs,
        }),
        Schedule::Once { at_ms } => RoutineSchedule::Once(OnceSchedule { at_ms: *at_ms }),
    }
}

fn row_from_input(
    input: RoutineInput,
    schedule: Schedule,
    enabled: bool,
    now_ms: u64,
    created_at: String,
    updated_at: String,
) -> RoutineRow {
    RoutineRow {
        name: input.name,
        description: input.description.unwrap_or_default(),
        agent: input.agent,
        prompt: input.prompt,
        next_run_at_ms: next_run_at(&schedule, enabled, now_ms),
        schedule,
        enabled,
        // Never carried from the input: run history belongs to the runs.
        last_run_at_ms: None,
        last_session_id: None,
        last_error: None,
        created_at,
        updated_at,
    }
}

fn routine_view(row: &RoutineRow) -> RoutineView {
    RoutineView {
        name: row.name.clone(),
        description: row.description.clone(),
        agent: row.agent.clone(),
        prompt: row.prompt.clone(),
        schedule: wire_schedule(&row.schedule),
        enabled: row.enabled,
        next_run_at_ms: row.next_run_at_ms,
        last_run_at_ms: row.last_run_at_ms,
        last_session_id: row.last_session_id.clone(),
        last_error: row.last_error.clone(),
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
pub(crate) mod tests {
    use super::*;
    use crate::agents::AgentStore;
    use crate::config::ConfigStore;
    use horsie_models::agents::AgentPresetInput;
    use horsie_models::settings::{ModelInput, ProviderInput, SettingsUpdate};

    /// Everything a routine test needs, over one temp DB: a config store with
    /// one model ("sonnet"), one agent preset ("reviewer"), and the routine
    /// service itself. Shared with the runner and scheduler tests, which build
    /// a supervisor on top of it.
    pub(crate) struct Fixture {
        pub routines: Arc<RoutineService>,
        pub agents: Arc<AgentService>,
        pub config: Arc<dyn ConfigStore>,
        pub provider_registry: crate::sessions::spec::SharedProviderRegistry,
        pub tmp: tempfile::TempDir,
    }

    pub(crate) async fn fixture() -> Fixture {
        // The temp dir is still handed back for callers that write files next
        // to the fixture; the database itself comes from `db::testing`, so
        // these tests run on whichever backend the run selected.
        let tmp = tempfile::tempdir().unwrap();
        let opened = crate::config::DbConfigStore::open_on(
            crate::db::testing::db().await,
            crate::config::StoreDeps {
                info: horsie_models::settings::ServerInfo {
                    config_path: String::new(),
                    database: String::new(),
                    state_dir: String::new(),
                    data_dir: String::new(),
                    plugins_dir: String::new(),
                    version: "test".into(),
                    journal_backend: "file".into(),
                },
            },
            crate::auth::UserId::new("1"),
        )
        .await
        .unwrap();
        opened
            .store
            .update(SettingsUpdate {
                providers: Some(vec![ProviderInput {
                    name: "p".into(),
                    kind: "anthropic".into(),
                    base_url: Some("http://localhost:1".into()),
                    api_key: Some("sk-x".into()),
                    keep_thinking_signature: None,
                }]),
                models: Some(vec![ModelInput {
                    alias: "sonnet".into(),
                    provider: "p".into(),
                    model_id: "claude-sonnet-4-6".into(),
                    max_tokens: None,
                    context_window: None,
                    thinking_efforts: None,
                    thinking_effort: None,
                    thinking_dialect: None,
                    forced_tools_disable_thinking: None,
                }]),
                default_vendor: Some("mock".into()),
            })
            .await
            .unwrap();
        let agents = Arc::new(AgentService::new(
            AgentStore::new(opened.db.clone()),
            opened.store.clone(),
        ));
        agents
            .create(AgentPresetInput {
                name: "reviewer".into(),
                description: None,
                model: "sonnet".into(),
                repos: None,
                plugins: None,
                mcp_servers: None,
                memory_spaces: None,
                thinking_effort: None,
            })
            .await
            .unwrap();
        Fixture {
            routines: Arc::new(RoutineService::new(
                RoutineStore::new(opened.db.clone()),
                agents.clone(),
            )),
            agents,
            config: opened.store.clone(),
            provider_registry: opened.registry.clone(),
            tmp,
        }
    }

    /// The routine service alone, for tests that need nothing else.
    async fn service() -> (Arc<RoutineService>, tempfile::TempDir) {
        let f = fixture().await;
        (f.routines, f.tmp)
    }

    pub(crate) fn input(name: &str, schedule: Option<RoutineSchedule>) -> RoutineInput {
        RoutineInput {
            name: name.into(),
            description: Some("d".into()),
            agent: "reviewer".into(),
            prompt: "triage the inbox".into(),
            schedule,
            enabled: None,
        }
    }

    #[test]
    fn next_run_measures_an_interval_from_now_and_never_re_arms_a_once() {
        assert_eq!(next_run_at(&Schedule::Manual, true, 1_000), None);
        assert_eq!(
            next_run_at(&Schedule::Every { interval_secs: 60 }, true, 1_000),
            Some(61_000)
        );
        assert_eq!(
            next_run_at(&Schedule::Once { at_ms: 5_000 }, true, 1_000),
            Some(5_000)
        );
        // Already past, so it never fires; and a paused routine never fires.
        assert_eq!(
            next_run_at(&Schedule::Once { at_ms: 500 }, true, 1_000),
            None
        );
        assert_eq!(
            next_run_at(&Schedule::Every { interval_secs: 60 }, false, 1_000),
            None
        );
    }

    #[tokio::test]
    async fn create_defaults_to_a_manual_enabled_routine() {
        let (s, _t) = service().await;
        let v = s.create(input("nightly", None), 1_000).await.unwrap();
        assert_eq!(v.name, "nightly");
        assert_eq!(v.agent, "reviewer");
        assert_eq!(v.schedule, RoutineSchedule::Manual(ManualSchedule {}));
        assert!(v.enabled);
        assert_eq!(v.next_run_at_ms, None);
        assert_eq!(v.last_run_at_ms, None);
        assert_eq!(v.created_at, v.updated_at);
    }

    #[tokio::test]
    async fn create_arms_a_recurring_schedule_from_now() {
        let (s, _t) = service().await;
        let v = s
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
        assert_eq!(v.next_run_at_ms, Some(3_601_000));
    }

    #[tokio::test]
    async fn create_validates_the_slug_prompt_agent_and_interval() {
        let (s, _t) = service().await;
        assert!(matches!(
            s.create(input("Not A Slug", None), 0).await.unwrap_err(),
            RoutineError::Invalid(_)
        ));

        let mut empty = input("a", None);
        empty.prompt = "   ".into();
        assert!(matches!(
            s.create(empty, 0).await.unwrap_err(),
            RoutineError::Invalid(m) if m.contains("prompt")
        ));

        let mut ghost = input("a", None);
        ghost.agent = "ghost".into();
        assert!(matches!(
            s.create(ghost, 0).await.unwrap_err(),
            RoutineError::Invalid(m) if m.contains("ghost")
        ));

        let fast = input(
            "a",
            Some(RoutineSchedule::Every(EverySchedule { interval_secs: 30 })),
        );
        assert!(matches!(
            s.create(fast, 0).await.unwrap_err(),
            RoutineError::Invalid(m) if m.contains("60")
        ));
    }

    #[tokio::test]
    async fn duplicate_create_conflicts() {
        let (s, _t) = service().await;
        s.create(input("a", None), 0).await.unwrap();
        assert!(matches!(
            s.create(input("a", None), 0).await.unwrap_err(),
            RoutineError::Conflict(_)
        ));
    }

    #[tokio::test]
    async fn replace_re_arms_the_schedule_and_keeps_created_at() {
        let (s, _t) = service().await;
        let created = s.create(input("a", None), 1_000).await.unwrap();
        s.record_run("a", 2_000, &RunOutcome::Started("sess-1".into()))
            .await
            .unwrap();

        let mut upd = input(
            "a",
            Some(RoutineSchedule::Every(EverySchedule { interval_secs: 600 })),
        );
        upd.description = Some("new".into());
        let got = s.replace("a", upd, 5_000).await.unwrap();
        assert_eq!(got.description, "new");
        assert_eq!(got.next_run_at_ms, Some(605_000));
        assert_eq!(got.created_at, created.created_at);
        assert_eq!(
            got.last_session_id.as_deref(),
            Some("sess-1"),
            "editing a routine must not erase what it has already run"
        );

        assert!(matches!(
            s.replace("a", input("b", None), 0).await.unwrap_err(),
            RoutineError::Invalid(_)
        ));
        assert!(matches!(
            s.replace("ghost", input("ghost", None), 0)
                .await
                .unwrap_err(),
            RoutineError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn a_disabled_routine_is_never_armed() {
        let (s, _t) = service().await;
        let mut paused = input(
            "a",
            Some(RoutineSchedule::Every(EverySchedule { interval_secs: 600 })),
        );
        paused.enabled = Some(false);
        let v = s.create(paused, 1_000).await.unwrap();
        assert!(!v.enabled);
        assert_eq!(v.next_run_at_ms, None);
        assert!(s.due(u64::MAX).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_and_get_report_unknown_names() {
        let (s, _t) = service().await;
        assert!(matches!(
            s.get("ghost").await.unwrap_err(),
            RoutineError::NotFound(_)
        ));
        assert!(matches!(
            s.delete("ghost").await.unwrap_err(),
            RoutineError::NotFound(_)
        ));
        s.create(input("a", None), 0).await.unwrap();
        s.delete("a").await.unwrap();
        assert!(matches!(
            s.get("a").await.unwrap_err(),
            RoutineError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn using_agent_reports_the_routines_that_would_break() {
        let (s, _t) = service().await;
        s.create(input("a", None), 0).await.unwrap();
        assert_eq!(s.using_agent("reviewer").await.unwrap(), vec!["a"]);
        assert!(s.using_agent("fixer").await.unwrap().is_empty());
    }
}
