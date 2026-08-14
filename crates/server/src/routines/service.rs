//! Validation, timestamps, and row↔wire mapping over [`RoutineStore`].
//!
//! Save-time validation covers what is stable at save: the name slug, the
//! prompt, the interval floor, and that the named agent preset exists. The
//! agent's own contents — vendor connectivity, whether its model is still
//! configured — are live state, re-checked by the runner at every trigger.

use crate::agents::AgentService;
use crate::routines::store::{RoutineRow, RoutineStore, RunOutcome};
use horsie_models::environments::EnvironmentSpec;
use horsie_models::routines::{
    ManualSchedule, RoutineInput, RoutineSchedule, RoutineView, Weekday,
};
use jiff::tz::TimeZone;
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
/// The calendar arms delegate to [`crate::routines::recurrence::next_occurrence`].
pub fn next_run_at(schedule: &RoutineSchedule, enabled: bool, now_ms: u64) -> Option<u64> {
    if !enabled {
        return None;
    }
    match schedule {
        RoutineSchedule::Manual(_) => None,
        RoutineSchedule::Every(e) => Some(now_ms.saturating_add(e.interval_secs * 1_000)),
        RoutineSchedule::Once(o) => (o.at_ms > now_ms).then_some(o.at_ms),
        s @ (RoutineSchedule::Daily(_)
        | RoutineSchedule::Weekly(_)
        | RoutineSchedule::Monthly(_)
        | RoutineSchedule::Yearly(_)) => crate::routines::recurrence::next_occurrence(s, now_ms),
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
    async fn validate(
        &self,
        input: &RoutineInput,
    ) -> Result<(RoutineSchedule, bool), RoutineError> {
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
        // Only what is stable at save. Whether the named vendor is connected,
        // or the named environment still exists, is a run-time fact — a routine
        // outlives both, and reports a broken one through `last_error`.
        if matches!(&input.environment, EnvironmentSpec::Runtime(r) if r.vendor.trim().is_empty()) {
            return Err(RoutineError::Invalid(
                "environment names no runtime vendor".to_string(),
            ));
        }
        let schedule = input
            .schedule
            .clone()
            .unwrap_or(RoutineSchedule::Manual(ManualSchedule {}));
        validate_schedule(&schedule)?;
        Ok((schedule, input.enabled.unwrap_or(true)))
    }
}

/// Save-time validation for a schedule. Covers what is stable at save: the
/// interval floor, the IANA timezone name, and that the calendar fields are
/// in range. The agent's own contents are live state, re-checked by the
/// runner at every trigger.
fn validate_schedule(schedule: &RoutineSchedule) -> Result<(), RoutineError> {
    match schedule {
        RoutineSchedule::Every(e) if e.interval_secs < MIN_INTERVAL_SECS => {
            Err(RoutineError::Invalid(format!(
                "interval must be at least {MIN_INTERVAL_SECS} seconds"
            )))
        }
        RoutineSchedule::Daily(d) => validate_clock(&d.timezone, d.hour, d.minute),
        RoutineSchedule::Weekly(w) => {
            validate_clock(&w.timezone, w.hour, w.minute)?;
            if w.weekdays.is_empty() {
                return Err(RoutineError::Invalid(
                    "weekly schedule needs at least one weekday".to_string(),
                ));
            }
            if w.weekdays
                .windows(2)
                .any(|p| weekday_rank(&p[0]) >= weekday_rank(&p[1]))
            {
                return Err(RoutineError::Invalid(
                    "weekdays must be unique and in Mon–Sun order".to_string(),
                ));
            }
            Ok(())
        }
        RoutineSchedule::Monthly(m) => {
            validate_clock(&m.timezone, m.hour, m.minute)?;
            validate_day_of_month(m.day_of_month)
        }
        RoutineSchedule::Yearly(y) => {
            validate_clock(&y.timezone, y.hour, y.minute)?;
            if !(1..=12).contains(&y.month) {
                return Err(RoutineError::Invalid(format!(
                    "month must be 1–12, got {}",
                    y.month
                )));
            }
            validate_day_of_month(y.day_of_month)
        }
        RoutineSchedule::Manual(_) | RoutineSchedule::Every(_) | RoutineSchedule::Once(_) => Ok(()),
    }
}

fn validate_clock(timezone: &str, hour: u32, minute: u32) -> Result<(), RoutineError> {
    TimeZone::get(timezone)
        .map_err(|_| RoutineError::Invalid(format!("unknown timezone '{timezone}'")))?;
    if hour > 23 {
        return Err(RoutineError::Invalid(format!(
            "hour must be 0–23, got {hour}"
        )));
    }
    if minute > 59 {
        return Err(RoutineError::Invalid(format!(
            "minute must be 0–59, got {minute}"
        )));
    }
    Ok(())
}

fn validate_day_of_month(day: u32) -> Result<(), RoutineError> {
    if !(1..=31).contains(&day) {
        return Err(RoutineError::Invalid(format!(
            "day of month must be 1–31, got {day}"
        )));
    }
    Ok(())
}

fn weekday_rank(d: &Weekday) -> u8 {
    match d {
        Weekday::Mon => 1,
        Weekday::Tue => 2,
        Weekday::Wed => 3,
        Weekday::Thu => 4,
        Weekday::Fri => 5,
        Weekday::Sat => 6,
        Weekday::Sun => 7,
    }
}

fn row_from_input(
    input: RoutineInput,
    schedule: RoutineSchedule,
    enabled: bool,
    now_ms: u64,
    created_at: String,
    updated_at: String,
) -> RoutineRow {
    RoutineRow {
        name: input.name,
        description: input.description.unwrap_or_default(),
        agent: input.agent,
        environment: input.environment,
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
        environment: row.environment.clone(),
        prompt: row.prompt.clone(),
        schedule: row.schedule.clone(),
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
    use horsie_models::environments::RuntimeEnvironment;
    use horsie_models::routines::{
        DailySchedule, EverySchedule, ManualSchedule, MonthlySchedule, OnceSchedule, Weekday,
        WeeklySchedule, YearlySchedule,
    };
    use horsie_models::settings::{ModelInput, ProviderInput};

    /// Everything a routine test needs, over one temp DB: a config store with
    /// one model ("sonnet"), one agent preset ("reviewer"), and the routine
    /// service itself. Shared with the runner and scheduler tests, which build
    /// a supervisor on top of it.
    pub(crate) struct Fixture {
        pub routines: Arc<RoutineService>,
        pub agents: Arc<AgentService>,
        pub environments: Arc<crate::environments::EnvironmentService>,
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
            AgentStore::new(opened.db.clone(), crate::auth::UserId::new("1")),
            opened.store.clone(),
        ));
        agents
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
            control_plane: None,
            })
            .await
            .unwrap();
        Fixture {
            routines: Arc::new(RoutineService::new(
                RoutineStore::new(opened.db.clone(), crate::auth::UserId::new("1")),
                agents.clone(),
            )),
            agents,
            environments: Arc::new(crate::environments::EnvironmentService::new(
                crate::environments::EnvironmentStore::new(
                    opened.db.clone(),
                    crate::auth::UserId::new("1"),
                ),
            )),
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
            environment: EnvironmentSpec::Runtime(RuntimeEnvironment {
                vendor: "mock".into(),
                repos: None,
            }),
            prompt: "triage the inbox".into(),
            schedule,
            enabled: None,
        }
    }

    #[test]
    fn next_run_measures_an_interval_from_now_and_never_re_arms_a_once() {
        assert_eq!(
            next_run_at(&RoutineSchedule::Manual(ManualSchedule {}), true, 1_000),
            None
        );
        assert_eq!(
            next_run_at(
                &RoutineSchedule::Every(EverySchedule { interval_secs: 60 }),
                true,
                1_000
            ),
            Some(61_000)
        );
        assert_eq!(
            next_run_at(
                &RoutineSchedule::Once(OnceSchedule { at_ms: 5_000 }),
                true,
                1_000
            ),
            Some(5_000)
        );
        // Already past, so it never fires; and a paused routine never fires.
        assert_eq!(
            next_run_at(
                &RoutineSchedule::Once(OnceSchedule { at_ms: 500 }),
                true,
                1_000
            ),
            None
        );
        assert_eq!(
            next_run_at(
                &RoutineSchedule::Every(EverySchedule { interval_secs: 60 }),
                false,
                1_000
            ),
            None
        );
    }

    #[test]
    fn next_run_arms_a_calendar_schedule_to_its_next_occurrence() {
        // 1970-01-01T00:00:01Z is a Thursday: daily 09:00 UTC is ~9h away.
        assert_eq!(
            next_run_at(
                &RoutineSchedule::Daily(DailySchedule {
                    timezone: "UTC".into(),
                    hour: 9,
                    minute: 0,
                }),
                true,
                1_000
            ),
            // 09:00:00 UTC on 1970-01-01, which is 9h after midnight.
            Some(9 * 3_600 * 1_000)
        );
        // A paused calendar routine never fires.
        assert_eq!(
            next_run_at(
                &RoutineSchedule::Daily(DailySchedule {
                    timezone: "UTC".into(),
                    hour: 9,
                    minute: 0,
                }),
                false,
                1_000
            ),
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

        let bad_zone = input(
            "a",
            Some(RoutineSchedule::Daily(DailySchedule {
                timezone: "Not/AZone".into(),
                hour: 9,
                minute: 0,
            })),
        );
        assert!(matches!(
            s.create(bad_zone, 0).await.unwrap_err(),
            RoutineError::Invalid(m) if m.contains("timezone")
        ));

        let bad_hour = input(
            "a",
            Some(RoutineSchedule::Daily(DailySchedule {
                timezone: "UTC".into(),
                hour: 24,
                minute: 0,
            })),
        );
        assert!(matches!(
            s.create(bad_hour, 0).await.unwrap_err(),
            RoutineError::Invalid(m) if m.contains("hour")
        ));

        let no_days = input(
            "a",
            Some(RoutineSchedule::Weekly(WeeklySchedule {
                timezone: "UTC".into(),
                hour: 9,
                minute: 0,
                weekdays: vec![],
            })),
        );
        assert!(matches!(
            s.create(no_days, 0).await.unwrap_err(),
            RoutineError::Invalid(m) if m.contains("weekday")
        ));

        let dup_days = input(
            "a",
            Some(RoutineSchedule::Weekly(WeeklySchedule {
                timezone: "UTC".into(),
                hour: 9,
                minute: 0,
                weekdays: vec![Weekday::Mon, Weekday::Mon],
            })),
        );
        assert!(matches!(
            s.create(dup_days, 0).await.unwrap_err(),
            RoutineError::Invalid(_)
        ));

        let bad_month_day = input(
            "a",
            Some(RoutineSchedule::Monthly(MonthlySchedule {
                timezone: "UTC".into(),
                hour: 9,
                minute: 0,
                day_of_month: 32,
            })),
        );
        assert!(matches!(
            s.create(bad_month_day, 0).await.unwrap_err(),
            RoutineError::Invalid(m) if m.contains("day of month")
        ));

        let bad_year = input(
            "a",
            Some(RoutineSchedule::Yearly(YearlySchedule {
                timezone: "UTC".into(),
                hour: 9,
                minute: 0,
                month: 13,
                day_of_month: 1,
            })),
        );
        assert!(matches!(
            s.create(bad_year, 0).await.unwrap_err(),
            RoutineError::Invalid(m) if m.contains("month")
        ));
    }

    #[tokio::test]
    async fn create_arms_a_weekly_schedule_to_its_next_weekday() {
        let (s, _t) = service().await;
        // 1970-01-01T00:00:01Z is a Thursday; Mon/Wed/Fri 09:00 → Friday 09:00.
        let v = s
            .create(
                input(
                    "triages",
                    Some(RoutineSchedule::Weekly(WeeklySchedule {
                        timezone: "UTC".into(),
                        hour: 9,
                        minute: 0,
                        weekdays: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
                    })),
                ),
                1_000,
            )
            .await
            .unwrap();
        assert_eq!(
            v.next_run_at_ms,
            // Friday 09:00:00 UTC = Thursday 00:00:00 + 24h + 9h.
            Some((24 + 9) * 3_600 * 1_000),
            "the next firing is Friday 09:00"
        );
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
