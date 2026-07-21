# Web-UI E2E: runtime context-loading + OpenAI wire

Extends the browser-driven E2E suite (`clients/web/e2e/`, tracking issue #17,
initial suite #18) with two high-priority groups against the same
real-server + mock-LLM + real-runtime system under test:

- **Group F** — the runtime's setup commands (`ScanWorkspace`, `SessionStart`)
  actually load workspace instructions (`AGENTS.md`), workspace skills, shared
  plugin-library skills, and the SessionStart bootstrap **into the agent's
  system prompt**.
- **Group G** — the OpenAI-compatible wire (`/v1/chat/completions`, provider
  `kind: "openai"`) works end-to-end through the real server, for both a plain
  text turn and a tool-call turn on the real runtime.

Only the LLM stays doubled. Everything else (server, runtime daemon, actors,
journal, SSE, provider crates) is real.

## Motivation / what's observable

Both features land in the **system prompt the agent sends to the LLM**
(`compose_system_prompt`, `workflow/src/workspace.rs:253`) or in the **wire
format** the provider speaks. The suite's mock LLM currently discards inbound
requests, so there is no way to assert what reached the agent. Group F needs
that visibility; Group G exercises the second wire the mock already serves.

With the local `e2e` vendor a session's `use_plugins` defaults to `true`
(`session_actor.rs:347`), so on agent spawn the server runs `ScanWorkspace`
(all workspaces, `include_shared = true`) **and** `run_session_start()`, then
folds the results into the system prompt. All four context sources are
therefore reachable through the real UI without any provisioning vendor.

Prompt-assembly facts that shape the assertions
(`workflow/src/workspace.rs`):

- **`AGENTS.md` content is inlined verbatim** under a `## <workspace> — <path>`
  header inside the `# Workspaces` block.
- **Skills contribute only `- <name>: <description>` listing lines**, not their
  bodies (bodies load on demand via the `skill` tool). So Group F asserts on
  the *listing*, not the body.
- Workspace skills appear under `### Skills (… workspace="<name>")`; shared
  plugin skills under a top-level `# Shared skills` block.
- The SessionStart hook output appears first, under `# Session bootstrap`.

## Shared harness changes

### 1. Mock LLM request capture (new capability)

The mock records every inbound request body so tests can assert on the system
prompt, wire-agnostically.

- `providers/mock-llm/src/server.rs`: add `captured: Mutex<Vec<serde_json::Value>>`
  to `MockState`. In `handle_messages` (Anthropic wire) and
  `openai::handle_chat_completions` (OpenAI wire), push a clone of the full
  request JSON before serving the queued response. (`handle_messages` currently
  deserializes only `stream`; switch it to take the raw `serde_json::Value` and
  read `stream` from it.)
- New route `GET /received` → JSON array of captured bodies, most-recent-first.
- `POST /reset` also clears `captured` (so it stays per-test).

Rationale for storing the raw body (not just the extracted system text): a
single substring match over the stringified JSON works for both wires
(Anthropic top-level `system`, OpenAI `messages[].role=="system"`) without
per-wire parsing. Markers are chosen single-line so JSON `\n`-escaping never
splits them.

Fixture client (`clients/web/e2e/fixtures.ts`): add
`MockLlm.received(): Promise<unknown[]>` (GET `/received`) and a small helper
to assert a substring appears in any captured request.

### 2. Global-setup fixtures (`clients/web/e2e/global-setup.ts`)

- **Workspace context** — write into the scratch workspace (`main=<scratch>`):
  - `AGENTS.md` containing a marker line, e.g. `E2E_AGENTS_MARKER: follow house rules.`
  - `.claude/skills/e2e-skill/SKILL.md` with frontmatter
    `name: e2e-skill`, `description: E2E_SKILL_DESC exercise the skill loader`,
    and a body.
- **Shared plugin library** — create a `plugins/` dir with one plugin
  `e2e-plugin/`:
  - `e2e-plugin/skills/e2e-shared-skill/SKILL.md`
    (`name: e2e-shared-skill`, `description: E2E_SHARED_DESC …`).
  - `e2e-plugin/hooks/hooks.json` with a `SessionStart` matcher whose
    `command` is `echo E2E_BOOTSTRAP_MARKER` (raw stdout becomes the injected
    context; no `additionalContext` envelope needed).
  - Launch the runtime daemon with an added `--plugins-dir <pluginsDir>`.
- **Second provider/model for the OpenAI wire** — after the existing `mock`
  provider seed, `PUT /api/config` adding provider
  `{ name: "mock-openai", kind: "openai", baseUrl: mockUrl, apiKey: "test-key" }`
  and model `{ alias: "mock-openai", provider: "mock-openai", modelId: "mock-model", maxTokens: 4096 }`.
  (`baseUrl` is the mock root — the provider appends `/v1/chat/completions`,
  `providers/openai/src/lib.rs:124`.)

These fixtures are additive: existing groups A–D assert on rendered UI, never on
the system prompt, so the extra AGENTS.md / skills / bootstrap and the second
model do not disturb them. The SessionStart hook (`echo`) runs once per session
spawn — negligible.

### 3. Helper (`clients/web/e2e/helpers.ts`)

`createSession(page, appBase, opts)` gains `opts.model?: string`: when set,
`page.getByTestId("model-select").selectOption(model)` (option `value` is the
model alias) before submitting. Used by Group G to pick `mock-openai`.

## Group F — runtime setup commands load context (`f-context-loading.spec.ts`)

All three cases: reset the mock, create a session (default `mock-sonnet` model,
local `e2e` vendor), send a message, queue one text reply, wait for the
assistant text + `Idle`, then `GET /received` and assert on the captured system
prompt of the message turn.

- **F1 — workspace AGENTS.md + workspace skill.** Assert the captured system
  prompt contains `E2E_AGENTS_MARKER` (instructions inlined) **and**
  `- e2e-skill: E2E_SKILL_DESC` (workspace-skill listing).
- **F2 — shared plugin skill.** Assert it contains `# Shared skills` and
  `- e2e-shared-skill: E2E_SHARED_DESC` (shared library discovered via
  `--plugins-dir` and `include_shared = true`).
- **F3 — SessionStart bootstrap.** Assert it contains `# Session bootstrap`
  and `E2E_BOOTSTRAP_MARKER` (the plugin's SessionStart hook ran and its output
  was injected).

(F1–F3 may be one session/one captured request asserted three ways, or three
sessions — three separate cases keeps failures isolated and is cheap.)

## Group G — OpenAI wire end-to-end (`g-openai-wire.spec.ts`)

- **G1 — text turn.** Create a session with `model: "mock-openai"`; queue a
  text reply; send a message; assert the assistant text renders exactly and the
  status reaches `Idle`. Proves the OpenAI provider request/parse path through
  the real server.
- **G2 — tool call.** Same model; queue a `tool_call` (bash) then a text reply;
  the real runtime executes the command in the scratch workspace; assert the
  tool-call card shows the real output and the follow-up text renders → `Idle`.
  Covers tool-calls, the riskiest wire difference between Anthropic and OpenAI.

## Testing / CI

- Run locally with `cd clients/web && bun run test:e2e` (or
  `HORSIE_E2E_SKIP_BUILD=1` when the three bins + `dist` are current). New
  mock-LLM behavior needs a rebuild first.
- The existing `web-e2e` CI job runs the whole `clients/web/e2e/` directory, so
  the two new spec files are picked up with no workflow change.
- Serial execution (`workers: 1`) stays — the mock's FIFO queue and single
  `/received` log are shared, and reset-per-test keeps them clean.

## Out of scope / follow-ups

- Per-worker SUT isolation for parallelism (already tracked in #17).
- Asserting a skill **body** actually loads via the `skill` tool (a deeper
  tool-call test) — the listing assertion already proves discovery/injection.
- Reasoning-model (`delta.reasoning_content`) rendering over the OpenAI wire —
  covered by the Rust provider tests; not re-driven through the UI here.
