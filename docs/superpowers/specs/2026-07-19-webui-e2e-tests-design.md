# Systematic Web-UI E2E Tests for the Horsie Session Server

**Date:** 2026-07-19
**Status:** design approved (driving to an initial PR)
**Branch:** `test/webui-e2e-suite`

## Goal

Give the horsie session server a systematic, browser-driven end-to-end test
suite that exercises its **core features through the actual web UI**, against a
realistic-but-deterministic system:

- **Real `horsie-server`** — the production binary, unmodified, serving the
  built web UI same-origin (`--web dist`) plus the HTTP + SSE API.
- **Mock LLM** — the existing `horsie-mock-llm` crate run as a real process,
  emulating the Anthropic Messages API with *controlled, deterministic*
  responses programmed per test.
- **Real local runtime vendor** — a real `horsie-runtime` daemon the harness
  launches, dialing back into the server's single HTTP port
  (`/api/runtime/connect?register=local`) and registering itself as a
  selectable vendor. Tool calls execute for real in a scratch workspace.

Only the LLM is a double. Everything else — actors, journal, SSE, the runtime,
tool execution — is real. This mirrors and complements the existing in-process
Rust `tests/tests/session_server_e2e.rs` (which doubles the vendor with
`MockVendor`); here the vendor is real and the driver is a real browser.

## System Under Test — process topology

```
 Playwright (Chromium)
        │  HTTP + SSE (same origin)
        ▼
 horsie-server  ──DB seed (PUT /api/config)──►  SQLite settings DB (temp)
   :PORT         provider "mock" → mock LLM url
   --web dist    model alias      → provider "mock"
   local_runtime=true
        ▲  ws://…/api/runtime/connect?register=local
        │
 horsie-runtime daemon  (--runtime-id e2e --workspace main=<scratch>)
        │  Anthropic Messages API
        ▼
 horsie-mock-llm  :MOCKPORT   (control plane: /queue, /reset, /scenarios/*)
```

All four run on `127.0.0.1` on ephemeral/allocated ports, in a temp state dir,
torn down after the run.

## Components

### 1. `horsie-mock-llm` binary (new)

The crate is library-only today (`MockLlmServer::builder()…build().await`),
with no way to run as a standalone process. Add a thin `[[bin]]`:

- `providers/mock-llm/src/bin/horsie-mock-llm/main.rs` (or `src/main.rs`).
- Reads a port from `--port`/`PORT` (default: ephemeral, prints the chosen
  port + url to stdout so the harness can capture it).
- Starts `MockLlmServer` and parks forever.

The existing control-plane HTTP routes are reused as-is:
`POST /v1/messages` (the Anthropic surface), `POST /queue` (append one
`MockResponse`), `POST /scenarios/load`, `GET /scenarios`,
`POST /scenarios/:name/register/:session_id`.

Add one small route for test isolation: **`POST /reset`** — clears the FIFO
queue and per-session scenario state. Tests call it in `beforeEach` so no
queued response leaks across cases.

`MockResponse` variants (tagged `type`, snake_case) the tests use:
`text {content}`, `text_stream {chunks}`, `tool_call {name, input}`,
`error {status, message}`, `thinking {text, signature}`.

### 2. Determinism strategy

The mock has two mechanisms: a global **FIFO queue** and **per-session
scenarios** keyed on the `X-Session-Id` header. The real server does *not*
construct a per-session provider, so it never sets `X-Session-Id` — the
scenario path won't fire against a real server. Therefore:

- Tests use the **FIFO queue**, and the suite runs **serially**
  (`workers: 1`, `fullyParallel: false`). Each test resets the queue, enqueues
  exactly the responses its turn(s) need, then drives the UI. One agent turn
  may call the LLM more than once (e.g. `tool_call` then final `text`), so the
  queue is loaded in call order.
- **Follow-up (out of scope now):** propagate the horsie session id to the
  provider as `X-Session-Id` so scenario-bound, parallel-safe tests become
  possible. Tracked in the issue, not built here.

### 3. `data-testid` selectors

The web UI currently has zero `data-testid` attributes and several icon-only or
duplicated controls (two "Stop" buttons, icon-only delete). Add stable
`data-testid` props to the load-bearing controls only — a small, low-risk diff:

- Sidebar: `new-session-button`, `session-search`, `session-row` (+ the id).
- NewSessionModal: `session-name-input`, `model-select`, `vendor-select`,
  `create-session-submit`.
- Composer: `composer-input`, `composer-send`, `composer-stop`,
  `ask-question-banner`.
- SessionView header: `session-stop`, `session-delete`.
- Transcript/messages: `message` (+ role), `assistant-text`.
- ToolCallCard: `tool-call-card` (+ tool name), `tool-call-output`.
- AskUserCard: `ask-user-card`, and its choice chips.
- StatusBadge: `status-badge` (asserts on the visible label text).

Delete uses a native `confirm()`; the tests auto-accept the dialog.

### 4. Playwright harness + orchestration

Add Playwright to `clients/web` (devDependency + `playwright.config.ts` +
`e2e/`). Playwright's single-command `webServer` can't orchestrate four
processes, so use **`globalSetup` / `globalTeardown`**:

**globalSetup**
1. Build binaries: `cargo build -p horsie-server -p horsie-runtime -p
   horsie-mock-llm` (idempotent/fast when warm).
2. Build web assets: `bun run build` → `clients/web/dist`.
3. Create a temp root: state dir, data dir, scratch workspace, `config.json`
   (`{ local_runtime: true, storage:{…}, database:{ url: sqlite://…temp } }`).
4. Spawn `horsie-mock-llm`, capture its url.
5. Spawn `horsie-server --config … --addr 127.0.0.1:PORT --web …/dist`; wait
   for `GET /api/health`.
6. Seed the DB: `PUT /api/config` with a provider `mock` (kind anthropic,
   `baseUrl` = mock url, no key) and a model alias → provider `mock`.
7. Spawn `horsie-runtime --endpoint
   ws://127.0.0.1:PORT/api/runtime/connect?register=local --runtime-id e2e
   --workspace main=<scratch>`; poll `GET /api/config` until vendor `e2e` is
   `active`.
8. `PUT /api/config { defaultVendor: "e2e" }` (valid now that it's connected).
9. Write process handles + baseURL + mock control url to a JSON file for
   teardown and tests.

**globalTeardown** kills all spawned processes and removes the temp root.

**Test fixture / helpers** (`e2e/fixtures.ts`): `baseURL` (Playwright config),
a `mock` helper (`reset()`, `queueText()`, `queueToolCall()`,
`queueTextStream()`, `queueError()`), and small UI helpers
(`createSession({model})`, `sendMessage(text)`).

The UI reaches the daemon vendor with no manual picking: `e2e` is the only
active vendor and the default, so the New Session modal auto-selects it.

## Test cases (initial core scope — all four groups)

Grouped as the tracking issue's checkboxes. All run against the SUT above.

**A. Turn basics + streaming**
- A1 Create a session (default model + `e2e` vendor) → reaches Idle/Ready,
  appears in the sidebar, routes to `/sessions/:id`.
- A2 Text turn: send a message; mock returns `text`; the assistant message
  renders with the exact mock content; status returns to Idle.
- A3 Streaming: mock returns `text_stream` chunks; deltas accumulate into the
  final assistant message.
- A4 Thinking: mock returns a `thinking` block + text; the thinking block
  renders and the final text follows. *(nice-to-have; include if cheap.)*

**B. Agent tool-call via the real runtime**
- B1 Mock returns `tool_call` (bash writing/echoing in the scratch workspace);
  the **real** runtime executes it; the tool-call card shows the command and
  the real output; mock then returns final `text`; turn completes.
- B2 Tool error surfaced: a bash command that exits non-zero renders an error
  state in the tool-call card, and the turn still completes with the follow-up
  text.

**C. `ask_user` clarify flow**
- C1 Mock returns an `ask_user` tool call → question card renders, status
  `Awaiting input`; user types an answer in the composer → turn resumes using
  the answer (mock's next `text` references it) → Idle.

**D. Lifecycle + resilience**
- D1 Stop: while a turn is running (gated mock response), click Stop → status
  Stopped; a follow-up message reattaches and completes.
- D2 Delete: delete a session (auto-accept confirm) → removed from sidebar,
  navigates away.
- D3 Multi-session: create two sessions, switch between them in the sidebar;
  each shows its own transcript; sidebar status dots reflect live status.
- D4 LLM error: mock returns `error {status, message}` → the session/turn
  surfaces the error text (no silent hang).
- D5 Journal replay on reload: after a completed turn, reload the page → the
  transcript is restored from the durable journal (SSE replay), not lost.

## Out of scope (now)

Settings CRUD, GitHub connect/repo provisioning, MCP servers, skills/plugins,
the velos vendor, provisioning workspaces (the local daemon announces
`supports_provisioning: false`), multi-turn conversational depth beyond what a
flow needs, and `X-Session-Id` per-session scenarios. Each is a follow-up.

## Follow-ups (tracked in the issue, not built here)

- `X-Session-Id` propagation → scenario-bound, parallelizable tests.
- CI wiring (GitHub Actions job running the suite headless).
- Settings/GitHub/MCP/skills UI coverage.
- Visual/screenshot assertions if desired later.

## Risks & mitigations

- **Four-process orchestration flakiness** → health/readiness polling at each
  step (server health, vendor active) with generous timeouts; JSON handoff
  file; robust teardown that always kills children.
- **Queue bleed across tests** → `POST /reset` in `beforeEach`; serial workers.
- **SSE buffering** → served same-origin from `horsie-server` (no proxy);
  assert on final message/status transitions, not transient deltas (except A3,
  which explicitly asserts accumulation).
- **Runtime sandbox** → launch the daemon with **no** `--sandbox-caps` (no
  landlock/nono needed); scratch workspace is a temp dir.
```
