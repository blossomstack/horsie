# Routines CLI Commands Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `horsie routines list`, `horsie routines get <name>`, and `horsie routines invoke <name>` to the CLI, backed by the server's existing `/api/routines` endpoints.

**Architecture:** Mirror the existing `agent` command end to end: one `ServerClient` method per endpoint (`cli/src/server_client.rs`), a new `cli/src/routines.rs` module holding the async command functions plus pure render helpers, and a `Command::Routine` variant wired into `main.rs`. Wire types come from `horsie_models::routines` (already generated from `models/fluorite/routines.fl`) — no model or server changes.

**Tech Stack:** Rust 2024 edition, clap 4 (derive), tokio, reqwest, `horsie-models` (fluorite-generated types), `horsie` CLI crate.

## Global Constraints

- The subcommand must appear as **`routines` (plural)**. clap would derive `routine` from a `Routine` variant, so the variant carries `#[command(name = "routines")]`.
- Every subcommand takes `--server: Option<String>` and resolves it with `horsie::config::resolve_server(server, None)?`.
- Wire types only from `horsie_models::routines::{RoutineView, RoutineRunResponse, RoutineSchedule, ManualSchedule, EverySchedule, OnceSchedule}` — no hand-rolled JSON.
- Reuse `crate::agent::truncate` (already `pub`) and `crate::session::relative` (Task 2 makes it `pub(crate)`).
- Output formats exactly as the spec:
  - `list` table columns: `NAME, AGENT, SCHEDULE, ENABLED, NEXT RUN, DESCRIPTION`; empty → `"no routines\n"`; absent `next_run_at_ms` → `-`.
  - `get` detail block fields: `name`, `description`, `agent`, `schedule`, `enabled`, `next run`, `last run` (always present, `-` when absent), `last session` (omitted when absent), `error` (only when `last_error` set), `prompt` last.
  - `invoke` prints exactly `session <id>\n<base>/sessions/<id>\n`.
- Workspace lints deny `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm` in production code; test modules use the standard opt-out block (see Task 2).
- Format with the **stable** toolchain only. Final gate: `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `cargo test --workspace` (single-crate `-p horsie-server` tests fail on feature gating; `-p horsie` is fine for per-task checks).
- All work happens in the worktree at `.horsie/worktrees/routines-cli` (branch `routines-cli`), based on `origin/main` (05858df).

---

### Task 1: ServerClient methods for the routines endpoints

**Files:**
- Modify: `cli/src/server_client.rs` (imports at top; methods appended after `get_session` at the end of the `impl ServerClient` block)

**Interfaces:**
- Consumes: the existing private `send<B, T>` helper, `CliError`, `reqwest`.
- Produces (used by Task 2):
  - `pub async fn list_routines(&self) -> Result<Vec<RoutineView>, CliError>`
  - `pub async fn get_routine(&self, name: &str) -> Result<RoutineView, CliError>`
  - `pub async fn run_routine(&self, name: &str) -> Result<RoutineRunResponse, CliError>`

- [ ] **Step 1: Add the import**

Add to the existing `use` block at the top of `cli/src/server_client.rs` (after the `horsie_models::session_api` import):

```rust
use horsie_models::routines::{RoutineRunResponse, RoutineView};
```

- [ ] **Step 2: Add the three methods**

Append to the end of `impl ServerClient` in `cli/src/server_client.rs` (after `get_session`):

```rust
    pub async fn list_routines(&self) -> Result<Vec<RoutineView>, CliError> {
        self.send(reqwest::Method::GET, "/api/routines", None::<&str>)
            .await
    }

    pub async fn get_routine(&self, name: &str) -> Result<RoutineView, CliError> {
        self.send(
            reqwest::Method::GET,
            &format!("/api/routines/{name}"),
            None::<&str>,
        )
        .await
    }

    pub async fn run_routine(&self, name: &str) -> Result<RoutineRunResponse, CliError> {
        self.send(
            reqwest::Method::POST,
            &format!("/api/routines/{name}/run"),
            None::<&str>,
        )
        .await
    }
```

`POST /api/routines/:name/run` takes no body, so `None::<&str>` is correct — same shape as the existing `list_agents`/`get_agent` calls.

- [ ] **Step 3: Verify it compiles**

Run (from the worktree root): `cargo check -p horsie`
Expected: no errors. `horsie` is the CLI crate name (`cli/Cargo.toml`).

- [ ] **Step 4: Run the existing CLI tests**

Run: `cargo test -p horsie`
Expected: all pass — this task changes no behavior, only adds methods.

- [ ] **Step 5: Commit**

```bash
git add cli/src/server_client.rs
git commit -m "feat(cli): ServerClient methods for the routines endpoints"
```

---

### Task 2: `routines` module with render helpers and unit tests

**Files:**
- Modify: `cli/src/lib.rs` — register the module
- Modify: `cli/src/session.rs:289` — widen `relative` visibility
- Create: `cli/src/routines.rs`

**Interfaces:**
- Consumes: `ServerClient::{list_routines, get_routine, run_routine}` (Task 1), `crate::agent::truncate`, `crate::session::relative`, `horsie_models::now_ms()`.
- Produces (used by Task 3):
  - `pub async fn list(server: &str) -> Result<(), CliError>`
  - `pub async fn get(server: &str, name: &str) -> Result<(), CliError>`
  - `pub async fn invoke(server: &str, name: &str) -> Result<(), CliError>`
  - private `schedule_label`, `enabled_label`, `render_routine_table`, `render_routine_detail`, `render_invoke`

- [ ] **Step 1: Register the module and widen `relative`**

In `cli/src/lib.rs`, add `pub mod routines;` between `pub mod plugins;` and `pub mod server_client;` (alphabetical order):

```rust
pub mod plugins;
pub mod routines;
pub mod server_client;
```

In `cli/src/session.rs`, change line 289 from `fn relative(now_ms: u64, then_ms: u64) -> String {` to:

```rust
pub(crate) fn relative(now_ms: u64, then_ms: u64) -> String {
```

- [ ] **Step 2: Write the failing tests**

Create `cli/src/routines.rs` with stub render helpers (returning empty strings) plus the full test module, so the tests fail for the right reason:

```rust
//! `horsie routines …` commands: list routines, show one, and trigger a run.
//! A routine is an agent preset plus a fixed prompt and a trigger; the server
//! owns the schedule and the run endpoint.

use horsie_models::routines::{RoutineSchedule, RoutineView};

fn schedule_label(_schedule: &RoutineSchedule) -> String {
    String::new()
}

fn enabled_label(_enabled: bool) -> &'static str {
    ""
}

fn render_routine_table(_routines: &[RoutineView], _now: u64) -> String {
    String::new()
}

fn render_routine_detail(_r: &RoutineView, _now: u64) -> String {
    String::new()
}

fn render_invoke(_base: &str, _session_id: &str) -> String {
    String::new()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::routines::{EverySchedule, ManualSchedule, OnceSchedule};

    fn routine(name: &str) -> RoutineView {
        RoutineView {
            name: name.into(),
            description: "nightly review".into(),
            agent: "reviewer".into(),
            prompt: "Review open PRs.".into(),
            schedule: RoutineSchedule::Every(EverySchedule { interval_secs: 3600 }),
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
            schedule_label(&RoutineSchedule::Every(EverySchedule { interval_secs: 3600 })),
            "every 3600s"
        );
        assert_eq!(
            schedule_label(&RoutineSchedule::Once(OnceSchedule { at_ms: 0 })),
            "once"
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
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p horsie routines::tests`
Expected: the five tests FAIL (each `assert_eq!`/`assert!` against an empty string). Compilation must succeed — the stubs exist solely so the file type-checks.

- [ ] **Step 4: Implement the module**

Replace the five stub functions in `cli/src/routines.rs` with the real implementations (keep the test module unchanged, and update the `use` block at the top):

```rust
use crate::agent::truncate;
use crate::error::CliError;
use crate::server_client::ServerClient;
use crate::session::relative;
use horsie_models::now_ms;
use horsie_models::routines::{RoutineRunResponse, RoutineSchedule, RoutineView};

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

/// One label per schedule arm: "manual", "every 3600s", "once".
fn schedule_label(schedule: &RoutineSchedule) -> String {
    match schedule {
        RoutineSchedule::Manual(_) => "manual".to_string(),
        RoutineSchedule::Every(e) => format!("every {}s", e.interval_secs),
        RoutineSchedule::Once(_) => "once".to_string(),
    }
}

fn enabled_label(enabled: bool) -> &'static str {
    if enabled {
        "yes"
    } else {
        "no"
    }
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
```

Notes:
- `relative()` treats a timestamp in the future as "just now" (it does `now - then` with saturating subtraction). That is the approved spec behavior for the `NEXT RUN` column; do not change `relative`.
- The `schedule_label` match spells out all three arms — `wildcard_enum_match_arm` is denied at the workspace level.
- `RoutineSchedule::Manual(ManualSchedule {})` constructs the empty `ManualSchedule` struct with its literal syntax (generated type has an empty-brace body).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie routines::tests`
Expected: all five tests PASS.

- [ ] **Step 6: Run the full CLI test suite**

Run: `cargo test -p horsie`
Expected: all tests pass, including the pre-existing `agent`, `session`, and `config_key` tests.

- [ ] **Step 7: Commit**

```bash
git add cli/src/lib.rs cli/src/session.rs cli/src/routines.rs
git commit -m "feat(cli): routines list/get/invoke commands"
```

---

### Task 3: Wire the `routines` command into the CLI

**Files:**
- Modify: `cli/src/main.rs` — `Command` enum, `RoutineAction` enum, dispatch match

**Interfaces:**
- Consumes: `horsie::routines::{list, get, invoke}` (Task 2), `horsie::config::resolve_server`.
- Produces: `horsie routines list`, `horsie routines get <name>`, `horsie routines invoke <name>`.

- [ ] **Step 1: Add the `Command` variant**

In `cli/src/main.rs`, add a variant to the `Command` enum right after the `Agent` variant. Note the explicit `name = "routines"` — without it clap would expose the subcommand as `routine`:

```rust
    /// List routines and trigger runs on a session server.
    #[command(name = "routines")]
    Routine {
        #[command(subcommand)]
        action: RoutineAction,
    },
```

- [ ] **Step 2: Add the `RoutineAction` enum**

Add this enum after `AgentAction` (before `ConfigAction`):

```rust
#[derive(Subcommand)]
enum RoutineAction {
    /// List routines.
    List {
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
    /// Show one routine.
    Get {
        /// Routine name.
        name: String,
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
    /// Trigger a routine run now, creating an unattended session.
    Invoke {
        /// Routine name.
        name: String,
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
}
```

- [ ] **Step 3: Add the dispatch arm**

In the `dispatch` function's match over `command`, add this arm after the `Command::Agent { .. }` arm (the compiler will reject the build until the match is exhaustive):

```rust
        Command::Routine { action } => match action {
            RoutineAction::List { server } => {
                let server = horsie::config::resolve_server(server, None)?;
                horsie::routines::list(&server).await?;
                Ok(0)
            }
            RoutineAction::Get { name, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                horsie::routines::get(&server, &name).await?;
                Ok(0)
            }
            RoutineAction::Invoke { name, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                horsie::routines::invoke(&server, &name).await?;
                Ok(0)
            }
        },
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p horsie`
Expected: no errors.

- [ ] **Step 5: Smoke-test the help output**

Run: `cargo run -p horsie -- routines --help`
Expected: help shows `routines` with `list`, `get`, and `invoke` subcommands (proves the `name = "routines"` attribute worked — the heading must say `horsie-routines` / `routines`).

Run: `cargo run -p horsie -- --help`
Expected: the top-level command list includes `routines`.

- [ ] **Step 6: Run the CLI tests**

Run: `cargo test -p horsie`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add cli/src/main.rs
git commit -m "feat(cli): wire the routines command into the CLI"
```

---

### Task 4: Final verification gate

**Files:**
- None expected to change; fix any issue the checks surface.

- [ ] **Step 1: Clippy, all targets and features, warnings denied**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings. The three new match/format sites (`schedule_label`, `render_routine_table`, `render_routine_detail`) must not trigger anything — all enum arms spelled out, no `unwrap` in production code.

- [ ] **Step 2: Format check with the stable toolchain**

Run: `cargo fmt --check`
Expected: no diff. If it reports diffs, run `cargo fmt` (stable, NOT nightly — nightly produces spurious diffs on this repo) and re-check.

- [ ] **Step 3: Full workspace test suite**

Run: `cargo test --workspace`
Expected: all tests pass. (Use `--workspace`, not `-p horsie-server` — that crate's testkit modules are feature-gated and fail when built alone.)

- [ ] **Step 4: Live smoke test against a running server (optional, if one is available)**

```bash
horsie routines list --server http://127.0.0.1:3789
horsie routines get <some-routine> --server http://127.0.0.1:3789
horsie routines invoke <some-routine> --server http://127.0.0.1:3789
```

Expected: `list` prints the table, `get` the detail block, `invoke` a `session <id>` line plus a `<base>/sessions/<id>` link.

- [ ] **Step 5: Final commit (only if any fix was needed)**

```bash
git add -A
git commit -m "fix(cli): address review gate findings"
```

If Step 1–3 all passed cleanly, no commit is needed here — Tasks 1–3 already carry the changes.
