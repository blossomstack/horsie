# Routines CLI commands — design

Date: 2026-08-03 · Status: approved

## Problem

The server exposes a full routines API (`GET /api/routines`, `GET /api/routines/:name`,
`POST /api/routines/:name/run`), and the web UI has a routines page, but the `horsie`
CLI has no way to list routines or trigger a run from the terminal. The `agent`
command already shows the pattern (list / get / invoke against a session server);
routines should get the same treatment.

## Scope

Add three subcommands under a new top-level `horsie routines` command:

```
horsie routines list              → GET /api/routines
horsie routines get <name>        → GET /api/routines/:name
horsie routines invoke <name>     → POST /api/routines/:name/run
```

Each takes the standard `--server` flag (omitted → configured default server, else
`https://auth.horsie.dev`), resolved via the existing `horsie::config::resolve_server`.

Out of scope (YAGNI): create / update / delete routines, run-history listing
(`GET /api/routines/:name/sessions`), and message overrides on invoke — the run
endpoint takes no body and the routine's prompt is fixed. The web UI owns editing.

## Implementation

### `cli/src/server_client.rs`

Three new methods on `ServerClient`, using the existing `send` helper:

- `list_routines(&self) -> Result<Vec<RoutineView>, CliError>` — `GET /api/routines`.
- `get_routine(&self, name: &str) -> Result<RoutineView, CliError>` — `GET /api/routines/{name}`.
- `run_routine(&self, name: &str) -> Result<RoutineRunResponse, CliError>` — `POST /api/routines/{name}/run` with no body.

Wire types come from `horsie_models::routines::{RoutineView, RoutineRunResponse}` —
already generated from `models/fluorite/routines.fl`; no model changes.

### `cli/src/routines.rs` (new module, mirrors `agent.rs`)

- `pub async fn list(server: &str)` — prints a table: columns
  `NAME, AGENT, SCHEDULE, ENABLED, NEXT RUN, DESCRIPTION`, using
  `crate::agent::truncate` for long cells and `crate::session::relative` (made
  `pub(crate)`) for the next-run time; a routine with no `next_run_at_ms` (manual,
  paused, or spent once) shows `-`. Empty → `"no routines\n"`.
- `pub async fn get(server: &str, name: &str)` — prints a detail block:
  `name`, `description`, `agent`, `schedule`, `enabled`, `next run`, `last run`
  (the two run times always render, `-` when absent), `last session` (omitted when
  absent), `error` (only when `last_error` is set), and `prompt` last (may be
  long / multi-line).
- `pub async fn invoke(server: &str, name: &str)` — triggers a run and prints
  `session <id>` then `<base>/sessions/<id>` on the next line — the exact two-line
  format `agent invoke` uses.
- `fn schedule_label(&RoutineSchedule) -> String` — exhaustive match over the union:
  `Manual` → `"manual"`, `Every { interval_secs }` → `"every {interval_secs}s"`,
  `Once` → `"once"`.
- `fn enabled_label(bool) -> &'static str` — `"yes"` / `"no"`.

### `cli/src/main.rs`

- New `Command::Routine { action: RoutineAction }` variant, doc comment
  "List routines and trigger runs on a session server."
- `RoutineAction` subcommand enum: `List { server }`, `Get { name, server }`,
  `Invoke { name, server }` — clap shape identical to `AgentAction`.
- Dispatch arm calling `horsie::routines::{list, get, invoke}`.

### Tests

In `cli/src/routines.rs` `#[cfg(test)]`, mirroring `agent.rs`:

- empty table prints `"no routines\n"`;
- table has header + one row per routine;
- `schedule_label` renders all three union arms;
- detail omits absent optionals (`last session`, `error`) and includes `prompt`;
- invoke output is `session <id>\n<base>/sessions/<id>\n`.

## Error handling

Reuses `CliError` and `ServerClient::send`: non-2xx → server's `ApiError` message
(e.g. unknown routine name → 404 message), transport failure → "cannot reach server"
naming the base URL. No new error variants.

## Verification

- `cargo clippy --all-targets --all-features -- -D warnings` (from workspace root)
- `cargo fmt --check`
- `cargo test --workspace` (unit tests in `cli/src/routines.rs`)
