# Routines — design

A **routine** is a saved agent preset plus a fixed prompt and a trigger. Run it
from the web UI, from the API, or on a timer; every run creates a session that
works unattended and reports back on the routine's own page.

## Why

Agent presets already answer "what configuration should this session have?" What
they don't answer is "what should it do, and when?" — every invocation still
needs a human to type a message. A routine closes that gap: the prompt is part of
the definition, so a trigger (a button, a POST, a timer) is enough to start real
work.

## Shape

A routine is deliberately **not** a second copy of an agent preset. It composes
one:

```
routine = agent preset (vendor, model, repos, skills, MCP, memory, effort)
        + prompt        (the fixed first message)
        + schedule      (manual | every N seconds | once at T)
        + enabled       (pause the schedule without losing it)
```

Duplicating the preset's config fields would mean a second store, a second
validator, and a second copy of the config form — for a type that means the same
thing. Referencing one keeps a single place to answer "how does this agent run?".
The cost is a dependency: a routine whose agent is deleted cannot run. That is
enforced at both ends — saving a routine validates the agent exists, and deleting
an agent a routine references is a 409.

## Data model

### `routines` table (settings DB, alongside `agents`)

| column           | type    | notes                                             |
| ---------------- | ------- | ------------------------------------------------- |
| `name`           | TEXT PK | slug; the id of record, immutable                 |
| `description`    | TEXT    | defaults `''`                                     |
| `agent`          | TEXT    | agent-preset name (validated at save, not a FK)   |
| `prompt`         | TEXT    | the message every run queues                      |
| `schedule_kind`  | TEXT    | `manual` \| `every` \| `once`                     |
| `interval_secs`  | INTEGER | non-NULL iff `every`                              |
| `at_ms`          | INTEGER | non-NULL iff `once`                               |
| `enabled`        | INTEGER | 0/1; pauses the timer, never the manual button    |
| `next_run_at_ms` | INTEGER | NULL → nothing scheduled (manual, paused, spent)  |
| `last_run_at_ms` | INTEGER | last trigger attempt                              |
| `last_session_id`| TEXT    | the session the last successful trigger created   |
| `last_error`     | TEXT    | why the last trigger failed to create a session   |
| `created_at`     | TEXT    | unix epoch seconds (matches `agents`)             |
| `updated_at`     | TEXT    | unix epoch seconds                                |

The storage schedule is a Rust enum (`Manual` / `Every { interval_secs }` /
`Once { at_ms }`) projected onto those columns; a row with `every` and no
interval is a load error, not a silently-defaulted value.

### Session origin

`SessionSpec` gains:

```rust
#[serde(default)]
pub origin: SessionOrigin,   // User | Routine { routine: String }
```

`#[serde(default)]` so every existing journal row loads as `User`. This one field
carries all three behaviours the requirements ask for:

- `GET /api/sessions` returns only `User`-origin sessions.
- `GET /api/routines/:name/sessions` returns the sessions that name it.
- A non-`User` session is **unattended**: it gets no `ask_user` tool and a system
  prompt suffix that says so.

Making unattendedness a property of the session's origin, rather than a separate
`allow_ask_user` flag, means the two can never disagree — a routine session with
a working `ask_user` would park forever with nobody to answer it.

## Triggers

All three paths converge on one `RoutineRunner::run(name)`:

1. resolve the routine, then its agent preset;
2. check the agent's vendor is connected and its model still configured (the same
   liveness checks `POST /api/agents/:name/invoke` already makes);
3. build a `SessionSpec` from the preset with `origin = Routine { name }`;
4. `Create` the session, then queue the prompt as its first message;
5. record `last_run_at_ms` + `last_session_id`, or `last_error` on failure.

- **Web UI** — "Run now" on the routine's page.
- **API** — `POST /api/routines/:name/run`, behind the same auth as everything
  else under `/api`. There is no separate webhook secret; a machine token is how
  a machine calls horsie.
- **Timer** — `RoutineScheduler`, a background task ticking every 15s. Each tick
  asks the store for enabled routines with `next_run_at_ms <= now`, advances the
  schedule *before* running (so a slow run cannot double-fire), and runs each.

Schedule advancement is deliberately "next = now + interval", not
"last + interval": a server that was down for a day resumes with one run, not a
day's backlog. Minimum interval is 60s.

Overlapping runs are **not** prevented: a routine on a 5-minute timer whose runs
take 10 minutes will have two sessions in flight. Detecting the overlap would
mean loading the previous session to ask its status (the supervisor deliberately
does not persist status), which costs a runtime wake-up per tick. The 60s floor
plus a visible run list is the mitigation.

## Deleting

Deleting a routine deletes its sessions. Its detail page is the only place they
are listed, so keeping them would leave sessions that are unreachable from the UI
but still hold vendor runtimes. The confirm dialog says how many will go.

## HTTP surface

```
GET    /api/routines                 → RoutineView[]
POST   /api/routines                 → 201 RoutineView
GET    /api/routines/:name           → RoutineView
PUT    /api/routines/:name           → RoutineView          (full replace)
DELETE /api/routines/:name           → 204                  (+ its sessions)
POST   /api/routines/:name/run       → 201 RoutineRunResponse
GET    /api/routines/:name/sessions  → RoutineSessionsResponse
```

Wire types live in `models/fluorite/routines.fl`. `RoutineSchedule` is a fluorite
union (`Manual` / `Every` / `Once`), so a client cannot express "every, with no
interval" either.

## Web UI

- **Sidebar** — a `Routines` entry immediately after `Agents`.
- **`/routines`** — one row per routine: name, agent, schedule, next run, last
  outcome. Delete from the row.
- **`/routines/new`, `/routines/:name/edit`** — name, description, agent picker
  (from the existing agent list), prompt textarea, schedule picker, enabled.
- **`/routines/:name`** — the detail page: definition summary, "Run now", and the
  run list (each row links to the normal session view, which works fine for a
  session the sidebar does not list).

## Server module layout

```
server/src/routines/
  store.rs      RoutineRow + SQLite CRUD + due(now) + record_run
  service.rs    validation, schedule↔columns, row↔wire, next-run computation
  runner.rs     routine → session (shared by the HTTP route and the scheduler)
  scheduler.rs  tick(now) → due routines → runner; the 15s background loop
```

`build_session_spec` moves out of `http::handlers` into `sessions::builder` with
a typed `SpecError`, because it now has three callers (create-session, agent
invoke, routine run) and the runner is not an HTTP concern.

## Testing

- **store** — round-trip including every schedule shape; `due()` respects
  `enabled` and the timestamp; a malformed schedule row is a load error.
- **service** — slug/agent/prompt/interval validation; next-run computation on
  save; replace keeps `created_at`.
- **runner** — creates a session with the routine origin and queues the prompt;
  records `last_error` when the vendor is not connected.
- **scheduler** — a `tick(now)` before the due time does nothing; after it, runs
  once and re-arms; a `once` routine never re-arms.
- **origin** — the session list omits routine sessions; the routine's own list
  shows them; an unattended session's toolbox has no `ask_user`.
- **HTTP** — CRUD + run + sessions, and the 409 on deleting a referenced agent.
- **web** — Vitest for the list page; a Playwright spec covering create → run →
  the run appearing on the detail page and *not* in the sidebar.
