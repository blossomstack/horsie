# ChatGPT Plan Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let horsie spend a ChatGPT subscription by adding a Responses-API provider that accepts either an API key or a ChatGPT OAuth credential.

**Architecture:** One new crate, `providers/openai-responses`, implements `LlmProvider` over the Responses API and is parameterised by a `Credential` — `ApiKey` posts to `{base_url}/responses`, `ChatGpt` posts to `https://chatgpt.com/backend-api/codex/responses` with a bearer access token and `ChatGPT-Account-ID`. Tokens live in a new `provider_oauth` table and refresh from inside the provider, because `build_registry` is synchronous and swaps the whole registry at once. Login is OAuth device code, driven from the server so a hosted horsie needs no callback URL.

**Tech Stack:** Rust, axum, reqwest + reqwest-eventsource, sqlx (SQLite + Postgres), fluorite codegen, React/Tailwind for the settings panel.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-04-chatgpt-plan-provider-design.md`.
- `originator` is `horsie`. Never send `x-oai-attestation`, and never fabricate one.
- Requests always set `stream: true`, `store: false`, `include: ["reasoning.encrypted_content"]`.
- OAuth client id is `app_EMoamEEZ73f0CkXaXp7hrann`; issuer `https://auth.openai.com`.
- Only a **4xx** from the token endpoint invalidates a stored credential (#164).
- Provider `kind` stays a validated `String` in `store.rs` — never a fluorite enum (stored rows are lowercase; fluorite serialises PascalCase).
- Timestamps in `provider_oauth` are INTEGER epoch seconds, matching `0014_auth.sql`.
- Migrations must be written for **both** `server/migrations/sqlite/` and `server/migrations/postgres/`.
- After any `models/fluorite/*.fl` edit, regenerate **both** `clients/ts/src/generated` and `clients/web/src/generated`, then run `make ts-types`.
- Verify with `cargo test -p <crate> --lib` while iterating; run the full workspace suite once before pushing (never twice in one command).
- Clippy any crate with a `test-util` feature as `cargo clippy -p <crate> --all-targets --features test-util -- -D warnings`.

---

### Task 1: `openai-responses` crate — wire types and history mapping

**Files:**
- Create: `providers/openai-responses/Cargo.toml`, `providers/openai-responses/src/lib.rs`, `providers/openai-responses/src/wire.rs`
- Modify: `Cargo.toml` (workspace `members`)

**Interfaces:**
- Consumes: `horsie_models::agent::{ContentPart, Message, Role, ThinkingPart, Usage}`, `horsie_agentcore::{CompletionRequest, ToolChoice}`.
- Produces: `wire::to_input_items(&[Message]) -> Vec<serde_json::Value>`, `wire::ReasoningRef { id, encrypted }` with `ReasoningRef::from_signature(&str) -> Option<Self>` and `ReasoningRef::to_signature(&self) -> String`, `wire::ResponsesRequest`, `wire::FunctionTool`.

- [ ] **Step 1: Write the failing tests in `wire.rs`**

Cover: user text → `input_text`; assistant text → `output_text`; `ToolCallPart` → `function_call` with `call_id` and a JSON **string** `arguments`; `ToolResultPart` → `function_call_output`; `SubAgentResultPart` flattened via `to_wire_text()`; a `ThinkingPart` whose signature round-trips through `ReasoningRef` → `reasoning` item carrying `encrypted_content`; a `ThinkingPart` with no/garbage signature → dropped entirely; a turn that produces nothing → no item.

- [ ] **Step 2: Run and watch them fail** — `cargo test -p horsie-openai-responses --lib`, expected: does not compile.

- [ ] **Step 3: Implement `wire.rs`**

Item shapes: `{"type":"message","role":"user","content":[{"type":"input_text","text":…}]}`, assistant the same with `output_text`, `{"type":"function_call","call_id":…,"name":…,"arguments":"<json string>"}`, `{"type":"function_call_output","call_id":…,"output":…}`, `{"type":"reasoning","id":…,"encrypted_content":…,"summary":[]}`. Tools are **flat**: `{"type":"function","name":…,"description":…,"parameters":…}` — not the chat wire's nested `function` object.

`ReasoningRef` serialises as `{"id":…,"enc":…}` into `ThinkingPart.signature`.

- [ ] **Step 4: Tests pass** — `cargo test -p horsie-openai-responses --lib`.
- [ ] **Step 5: Commit** — `feat(openai-responses): wire types and history mapping`.

---

### Task 2: `mock-llm` `/responses` route

**Files:**
- Create: `providers/mock-llm/src/responses.rs`
- Modify: `providers/mock-llm/src/server.rs` (add the route to the router next to `/v1/chat/completions`), `providers/mock-llm/src/lib.rs` (`mod responses;`)

**Interfaces:**
- Consumes: `crate::server::{MockResponse, MockState, ResponseKind, sse_from_pairs}`.
- Produces: `pub(crate) async fn handle_responses(State<Arc<MockState>>, HeaderMap, Json<Value>) -> ResponseKind`, served at `/responses` **and** `/v1/responses`.

- [ ] **Step 1: Write failing route tests** mirroring `openai.rs`'s: queued text streams `response.output_text.delta` then `response.completed`; a queued tool call streams `response.output_item.added` with a `function_call` item plus `response.function_call_arguments.delta`; `queue_error(429, …)` returns HTTP 429; `queue_truncated` emits `response.incomplete` with `incomplete_details.reason = "max_output_tokens"`; `queue_cut_stream` omits `response.completed`; `MockResponse::Reasoning` emits a `reasoning` item with `encrypted_content` and `response.reasoning_summary_text.delta`.
- [ ] **Step 2: Run and watch them fail** — `cargo test -p horsie-mock-llm --lib`.
- [ ] **Step 3: Implement the route**, one SSE frame per event with `event:` names matching the Responses API. Usage rides on `response.completed` as `{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":5}`.
- [ ] **Step 4: Tests pass.**
- [ ] **Step 5: Commit** — `test(mock-llm): serve the Responses wire`.

---

### Task 3: Provider `complete()` — streaming, retries, usage

**Files:**
- Modify: `providers/openai-responses/src/lib.rs`
- Test: same file, `#[cfg(test)] mod tests` against `MockLlmServer`

**Interfaces:**
- Consumes: Task 1's `wire`, Task 2's mock route.
- Produces: `ResponsesProvider` with `new()`, `with_api_key(impl Into<Secret>)`, `with_model`, `with_base_url`, `with_max_tokens`, `with_thinking_effort_dialect(ThinkingDialect)`, `with_retry_delay_secs`, `with_read_timeout_secs`, and `impl LlmProvider`.

- [ ] **Step 1: Write failing provider tests** — text turn returns one `TextPart` and `StopReason::EndTurn`; a tool call returns a `ToolCallPart` with parsed input and `StopReason::ToolUse`; a reasoning turn returns a `ThinkingPart` whose signature parses back to a `ReasoningRef`; `response.incomplete` → `StopReason::MaxTokens`; a cut stream is an `Err`, not an empty success; usage maps `input_tokens`/`cached_tokens`/`output_tokens`.
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Implement.** Copy the retry/backoff/`saw_terminal` structure from `providers/openai/src/lib.rs` — including its rule that a retry only happens when nothing has been emitted, and that a stream ending without a terminal frame is an error. Event mapping is the table in the spec's §3.
- [ ] **Step 4: Tests pass** — `cargo test -p horsie-openai-responses`.
- [ ] **Step 5: Commit** — `feat(openai-responses): stream the Responses wire`.

---

### Task 4: Conformance coverage

**Files:**
- Modify: `tests/tests/provider_conformance.rs`

- [ ] **Step 1:** Add `ProviderKind::OpenaiResponses` to the enum, to `KINDS`, and to `build_provider`. Add a `400 → ApiError` test alongside the existing per-provider ones.
- [ ] **Step 2: Run** — `cargo test -p horsie-tests --test provider_conformance`. Every existing conformance case must pass for the new kind unchanged.
- [ ] **Step 3: Commit** — `test: run the conformance suite against the Responses wire`.

---

### Task 5: `ChatGptTokens` — device-code login and refresh

**Files:**
- Create: `providers/openai-responses/src/chatgpt.rs`
- Modify: `providers/openai-responses/src/lib.rs` (`Credential` enum; endpoint and header selection)

**Interfaces:**
- Produces:
  - `pub struct StoredTokens { pub access: String, pub refresh: String, pub expires_at: i64, pub account_id: String }`
  - `pub trait TokenStore: Send + Sync { fn save(&self, t: &StoredTokens); }`
  - `pub struct ChatGptTokens` with `new(StoredTokens, Arc<dyn TokenStore>, issuer: String)` and `async fn access_token(&self) -> Result<(String, String), LlmError>` returning `(access, account_id)`, refreshing when within 60s of expiry.
  - `pub async fn start_device_login(&Client, issuer) -> Result<DeviceLogin, LlmError>` and `pub async fn poll_device_login(&Client, issuer, &DeviceLogin) -> Result<Option<StoredTokens>, LlmError>`.
  - `pub fn account_id_from_id_token(&str) -> Option<String>` — claims `chatgpt_account_id`, then `https://api.openai.com/auth`.`chatgpt_account_id`, then `organizations[0].id`.

- [ ] **Step 1: Write failing tests** against a fake issuer built the way `server/src/mcp/oauth.rs`'s `mock_as()` does (axum on port 0): a non-expired token is returned without a refresh call; an expired one refreshes and calls `TokenStore::save`; a **500** from the token endpoint returns `Err` and does **not** save or clear; a **400** marks the credential invalid; device polling returns `Ok(None)` until the fake issuer flips to approved, then exchanges and yields tokens; `account_id_from_id_token` handles all three claim shapes.
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Implement**, including the `Credential` split in `lib.rs`: `ChatGpt` targets `https://chatgpt.com/backend-api/codex/responses` and sets `Authorization` + `ChatGPT-Account-ID`; a 401 refreshes once and retries once. Build the reqwest client with a cookie store so Cloudflare cookies persist.
- [ ] **Step 4: Tests pass.**
- [ ] **Step 5: Commit** — `feat(openai-responses): ChatGPT device login and token refresh`.

---

### Task 6: Persistence and registry wiring

**Files:**
- Create: `server/migrations/sqlite/0021_provider_oauth.sql`, `server/migrations/postgres/0021_provider_oauth.sql`
- Modify: `server/src/config/store.rs`, `cli/src/config.rs`

**Interfaces:**
- Consumes: Task 5's `StoredTokens`/`TokenStore`.
- Produces: `ConfigStore::provider_oauth(&self, provider: &str) -> Option<StoredTokens>`, `::save_provider_oauth`, `::delete_provider_oauth`; `build_registry` accepts the loaded rows and constructs `chatgpt` providers.

- [ ] **Step 1: Write failing store tests** — round-trip a row; `build_registry` errors for a `chatgpt` provider with no credential row; both new kinds build; an unknown kind still errors.
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Implement.** Table per the spec §4. `build_registry` gains a `&HashMap<String, StoredTokens>` argument, loaded by the caller before the registry is built, so the function stays synchronous. `cli/src/config.rs` gains an `Openairesponses` arm (API key only).
- [ ] **Step 4: Tests pass** — `cargo test -p horsie-server --lib config::`.
- [ ] **Step 5: Commit** — `feat(server): store ChatGPT credentials and build both new provider kinds`.

---

### Task 7: Device-login HTTP endpoints

**Files:**
- Modify: `server/src/http/mod.rs`, `server/src/config/mod.rs` (handlers)

**Interfaces:**
- Produces: `POST /api/admin/providers/:name/chatgpt/login` → `{userCode, verificationUrl, intervalSecs}`; `POST /api/admin/providers/:name/chatgpt/poll` → `{status: "pending"|"complete", accountId?}`; `DELETE /api/admin/providers/:name/chatgpt/login` → 204.

- [ ] **Step 1: Write failing handler tests** in `http/mod.rs`'s existing suite: login against an unknown provider is 404; login against a non-`chatgpt` provider is 422; poll before approval is `pending`; delete removes the row. Point the issuer at the fake from Task 5 via the existing test config seam.
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Implement**, holding the in-flight `DeviceLogin` in server state keyed by provider name. On completion, save the row **and** swap the registry so the provider is usable without a restart.
- [ ] **Step 4: Tests pass.**
- [ ] **Step 5: Commit** — `feat(server): ChatGPT device-login endpoints`.

---

### Task 8: Settings panel

**Files:**
- Modify: `clients/web/src/` settings provider components; `models/fluorite/settings.fl` only if a wire type is genuinely needed.

- [ ] **Step 1:** Add `openai-responses` and `chatgpt` to the provider-kind selector.
- [ ] **Step 2:** For a `chatgpt` provider, render a "Sign in with ChatGPT" button that calls `/login`, displays the user code and the `auth.openai.com/codex/device` link, polls until complete, then shows the signed-in account id with a sign-out control.
- [ ] **Step 3:** If any `.fl` file changed, regenerate both type trees and run `make ts-types`.
- [ ] **Step 4:** `cd clients/web && bun run typecheck` (bun, not npm).
- [ ] **Step 5: Commit** — `feat(web): ChatGPT sign-in for provider settings`.

---

### Task 9: Docs and final gate

**Files:**
- Modify: `docs/guide/settings-reference.md`

- [ ] **Step 1:** Document both kinds, the device-login flow, that ChatGPT usage draws on the operator's plan limits, and the two standing rules (`originator: horsie`, no attestation header).
- [ ] **Step 2:** Run `cargo fmt --all`, then `make check`, then `make ts-types`.
- [ ] **Step 3: Commit and open the PR.**

## Self-review

Spec coverage: §1 → Tasks 1/3/5; §2 → Task 1; §3 → Tasks 2/3; §4 → Tasks 5/6; §5 → Tasks 5/7/8; §6 → Tasks 6/8/9; testing → Tasks 2/3/4/5/6/7. No spec section is unclaimed.

Naming is consistent across tasks: `ReasoningRef`, `StoredTokens`, `TokenStore`, `ChatGptTokens`, `ResponsesProvider`, `to_input_items`.
