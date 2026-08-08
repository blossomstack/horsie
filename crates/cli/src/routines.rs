//! `horsie routines …` commands: list routines, show one, and trigger a run.
//! A routine is an agent preset plus a fixed prompt and a trigger; the server
//! owns the schedule and the run endpoint.

use crate::agent::truncate;
use crate::error::CliError;
use crate::server_client::ServerClient;
use crate::session::relative;
use horsie_models::now_ms;
use horsie_models::routines::{RoutineRunResponse, RoutineSchedule, RoutineView, Weekday};

pub async fn list(server: &str) -> Result<(), CliError> {
    let routines = ServerClient::new(server).await?.list_routines().await?;
    print!("{}", render_routine_table(&routines, now_ms()));
    Ok(())
}

pub async fn get(server: &str, name: &str) -> Result<(), CliError> {
    let routine = ServerClient::new(server).await?.get_routine(name).await?;
    print!("{}", render_routine_detail(&routine, now_ms()));
    Ok(())
}

/// Trigger a run now, whatever the schedule says; print the new session's id
/// and web link — the same two-line shape as `horsie agent invoke`.
pub async fn invoke(server: &str, name: &str) -> Result<(), CliError> {
    let client = ServerClient::new(server).await?;
    let RoutineRunResponse { session } = client.run_routine(name).await?;
    print!("{}", render_invoke(client.base(), &session.id));
    Ok(())
}

/// One label per schedule arm: "manual", "every 3600s", "once",
/// "daily 09:05 Asia/Shanghai", "weekly mon,wed,fri 09:00 …".
fn schedule_label(schedule: &RoutineSchedule) -> String {
    match schedule {
        RoutineSchedule::Manual(_) => "manual".to_string(),
        RoutineSchedule::Every(e) => format!("every {}s", e.interval_secs),
        RoutineSchedule::Once(_) => "once".to_string(),
        RoutineSchedule::Daily(d) => {
            format!("daily {:02}:{:02} {}", d.hour, d.minute, d.timezone)
        }
        RoutineSchedule::Weekly(w) => {
            let days = w
                .weekdays
                .iter()
                .map(weekday_abbr)
                .collect::<Vec<_>>()
                .join(",");
            format!("weekly {days} {:02}:{:02} {}", w.hour, w.minute, w.timezone)
        }
        RoutineSchedule::Monthly(m) => format!(
            "monthly {}th {:02}:{:02} {}",
            m.day_of_month, m.hour, m.minute, m.timezone
        ),
        RoutineSchedule::Yearly(y) => format!(
            "yearly {}-{} {:02}:{:02} {}",
            month_abbr(y.month),
            y.day_of_month,
            y.hour,
            y.minute,
            y.timezone
        ),
    }
}

fn weekday_abbr(d: &Weekday) -> &'static str {
    match d {
        Weekday::Mon => "mon",
        Weekday::Tue => "tue",
        Weekday::Wed => "wed",
        Weekday::Thu => "thu",
        Weekday::Fri => "fri",
        Weekday::Sat => "sat",
        Weekday::Sun => "sun",
    }
}

fn month_abbr(month: u32) -> &'static str {
    [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ][(month - 1) as usize]
}

fn enabled_label(enabled: bool) -> &'static str {
    if enabled { "yes" } else { "no" }
}

fn render_routine_table(routines: &[RoutineView], now: u64) -> String {
    if routines.is_empty() {
        return "no routines\n".to_string();
    }
    let mut out = format!(
        "{:<20} {:<14} {:<12} {:<7} {:<10} DESCRIPTION\n",
        "NAME", "AGENT", "SCHEDULE", "ENABLED", "NEXT RUN"
    );
    for r in routines {
        out.push_str(&format!(
            "{:<20} {:<14} {:<12} {:<7} {:<10} {}\n",
            truncate(&r.name, 20),
            truncate(&r.agent, 14),
            truncate(&schedule_label(&r.schedule), 12),
            enabled_label(r.enabled),
            r.next_run_at_ms
                .map(|t| relative(now, t))
                .unwrap_or_else(|| "-".to_string()),
            truncate(&r.description, 60),
        ));
    }
    out
}

fn render_routine_detail(r: &RoutineView, now: u64) -> String {
    let mut out = format!(
        "name        {}\ndescription {}\nagent       {}\nschedule    {}\nenabled     {}\nnext run    {}\nlast run    {}\n",
        r.name,
        r.description,
        r.agent,
        schedule_label(&r.schedule),
        enabled_label(r.enabled),
        r.next_run_at_ms
            .map(|t| relative(now, t))
            .unwrap_or_else(|| "-".to_string()),
        r.last_run_at_ms
            .map(|t| relative(now, t))
            .unwrap_or_else(|| "-".to_string()),
    );
    if let Some(id) = r.last_session_id.as_deref() {
        out.push_str(&format!("last session {id}\n"));
    }
    if let Some(err) = r.last_error.as_deref() {
        out.push_str(&format!("error       {err}\n"));
    }
    out.push_str(&format!("prompt      {}\n", r.prompt));
    out
}

/// Two lines: the bare id (script-friendly) and the clickable web link.
fn render_invoke(base: &str, session_id: &str) -> String {
    format!("session {session_id}\n{base}/sessions/{session_id}\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::routines::{
        DailySchedule, EverySchedule, ManualSchedule, MonthlySchedule, OnceSchedule, Weekday,
        WeeklySchedule, YearlySchedule,
    };

    fn routine(name: &str) -> RoutineView {
        RoutineView {
            name: name.into(),
            description: "nightly review".into(),
            agent: "reviewer".into(),
            prompt: "Review open PRs.".into(),
            schedule: RoutineSchedule::Every(EverySchedule {
                interval_secs: 3600,
            }),
            enabled: true,
            next_run_at_ms: Some(1_000),
            last_run_at_ms: None,
            last_session_id: None,
            last_error: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[test]
    fn empty_table_says_no_routines() {
        assert_eq!(render_routine_table(&[], 0), "no routines\n");
    }

    #[test]
    fn table_has_header_and_one_row_per_routine() {
        let out = render_routine_table(&[routine("nightly"), routine("weekly")], 1_000_000);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("NAME"));
        assert!(lines[0].contains("NEXT RUN"));
        assert!(lines[1].contains("nightly"));
        assert!(lines[1].contains("every 3600s"));
        assert!(lines[1].contains("yes"));
        assert!(lines[2].contains("weekly"));
    }

    #[test]
    fn schedule_label_covers_all_arms() {
        assert_eq!(
            schedule_label(&RoutineSchedule::Manual(ManualSchedule {})),
            "manual"
        );
        assert_eq!(
            schedule_label(&RoutineSchedule::Every(EverySchedule {
                interval_secs: 3600
            })),
            "every 3600s"
        );
        assert_eq!(
            schedule_label(&RoutineSchedule::Once(OnceSchedule { at_ms: 0 })),
            "once"
        );
        assert_eq!(
            schedule_label(&RoutineSchedule::Daily(DailySchedule {
                timezone: "Asia/Shanghai".into(),
                hour: 9,
                minute: 5,
            })),
            "daily 09:05 Asia/Shanghai"
        );
        assert_eq!(
            schedule_label(&RoutineSchedule::Weekly(WeeklySchedule {
                timezone: "Asia/Shanghai".into(),
                hour: 9,
                minute: 0,
                weekdays: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
            })),
            "weekly mon,wed,fri 09:00 Asia/Shanghai"
        );
        assert_eq!(
            schedule_label(&RoutineSchedule::Monthly(MonthlySchedule {
                timezone: "UTC".into(),
                hour: 9,
                minute: 0,
                day_of_month: 15,
            })),
            "monthly 15th 09:00 UTC"
        );
        assert_eq!(
            schedule_label(&RoutineSchedule::Yearly(YearlySchedule {
                timezone: "UTC".into(),
                hour: 9,
                minute: 0,
                month: 2,
                day_of_month: 15,
            })),
            "yearly feb-15 09:00 UTC"
        );
    }

    #[test]
    fn detail_omits_absent_optionals_and_ends_with_prompt() {
        let out = render_routine_detail(&routine("nightly"), 1_000_000);
        assert!(out.contains("name        nightly"));
        assert!(out.contains("schedule    every 3600s"));
        assert!(out.contains("next run    "));
        assert!(!out.contains("last session"), "absent last session: {out}");
        assert!(!out.contains("error"), "absent last error: {out}");
        assert!(out.trim_end().ends_with("prompt      Review open PRs."));
    }

    #[test]
    fn invoke_output_is_id_then_link() {
        let out = render_invoke("http://127.0.0.1:3789", "abc-123");
        assert_eq!(
            out,
            "session abc-123\nhttp://127.0.0.1:3789/sessions/abc-123\n"
        );
    }
}
