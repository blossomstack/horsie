# Web-UI end-to-end tests

Browser-driven (Playwright) tests that exercise the session server's core
features through the real web UI, against a realistic-but-deterministic system:

- **Real `horsie-server`** — the production binary, serving the built UI
  same-origin (`--web dist`) plus the HTTP + SSE API.
- **Mock LLM** — `horsie-mock-llm` run as a process, with responses programmed
  per test via its control plane (`/queue`, `/reset`).
- **Real local runtime vendor** — a real `horsie-runtime` daemon dialing back
  into the server's HTTP port (`/api/runtime/connect?register=local`) and
  registering as the `e2e` vendor. Tool calls execute for real in a scratch
  workspace.

Only the LLM is a double. Design: `docs/superpowers/specs/2026-07-19-webui-e2e-tests-design.md`.

## Running

```bash
cd clients/web
bun install
bunx playwright install chromium   # one-time
bun run test:e2e
```

`global-setup` builds the three Rust binaries + the web assets, brings the four
processes up on ephemeral ports, seeds the settings DB (a `mock` provider →
model `mock-sonnet`, `defaultVendor = e2e`), and writes the running URLs to
`e2e/.runtime.json`. `global-teardown` kills everything and removes the temp
root.

Prerequisites on PATH: `cargo`, `bun`. Tests run **serially** (`workers: 1`) —
the mock LLM's response queue is global and order-based.

### Environment

- `HORSIE_E2E_SKIP_BUILD=1` — skip the cargo + web build in global-setup (use
  when the binaries and `dist/` are already current; much faster iteration).

## Layout

- `harness.ts` — paths, free-port allocator, poll helper, config-seeding, the
  `.runtime.json` handoff.
- `global-setup.ts` / `global-teardown.ts` — process orchestration + cleanup.
- `fixtures.ts` — `appBase` (server URL) and `mock` (control-plane client).
- `helpers.ts` — `createSession`, `sendMessage`, `expectStatus`.
- `*.spec.ts` — grouped test cases (A: turn basics, B: tool calls, C: ask_user,
  D: lifecycle + resilience).

## Programming the mock

Each test resets and queues exactly the responses its turn(s) need, in LLM-call
order (a tool-call turn is two calls: the `tool_call`, then the final `text`):

```ts
await mock.reset();
await mock.queueToolCall("bash", { command: "echo hi" });
await mock.queueText("done");
```
