# ChatGPT plan support: a Responses-API provider with two credential sources

Status: design, approved 2026-08-04. Branch `feat/chatgpt-plan-provider`.

## Problem

horsie can talk to a model two ways: Anthropic's wire, and OpenAI-compatible
`/v1/chat/completions`. Neither can spend a **ChatGPT subscription**. Codex plan
inference is served from `https://chatgpt.com/backend-api/codex/responses`, which
accepts the **Responses API** and nothing else, authenticated with an OAuth access
token rather than an API key.

Two gaps, then: horsie has no Responses wire, and no provider credential that
expires. This spec closes both.

## What the code actually says

- `providers/openai` is chat-completions only — its `wire.rs` maps tool results
  onto `role: "tool"` messages and deliberately drops thinking on the way out.
  Neither is true on the Responses wire.
- `build_registry` (`server/src/config/store.rs:355`) is a synchronous function
  over `providers` rows that builds `Arc<dyn LlmProvider>` values, and the whole
  registry is swapped on config edits. Nothing in it can await, so a credential
  that needs refreshing must refresh itself from inside the provider.
- The `providers` table holds `name`/`kind`/`base_url`/`api_key`. `api_key` is
  write-only from the UI and never rotates on its own.
- `server/src/mcp/oauth.rs` already implements PKCE S256 generation, an
  authorization-code exchange and a refresh call, with tests against a fake
  issuer. It is reusable as-is.
- `0014_auth.sql` gives horsie one admin account (`auth_users`) and opaque
  `auth_tokens`. **No resource carries an owner column** — providers, models,
  agents and sessions are global admin config.

## Scope

In:

- A `providers/openai-responses` crate implementing `LlmProvider` over the
  Responses API, usable with an API key *or* a ChatGPT OAuth credential.
- ChatGPT device-code login, token storage, and refresh-on-expiry.
- Config + web UI surface for both new provider kinds.
- `mock-llm` `/responses` route and conformance coverage.

Out, deliberately:

- **Per-tenant BYOK.** horsie has no tenant concept today; the ChatGPT login is
  admin-configured global config exactly like every other provider. Per-user
  credentials are future work and are not designed here.
- **CLI ChatGPT login.** `cli/src/config.rs` gains the `openai-responses` arm
  (API key) only. A local CLI agent cannot sign into ChatGPT in this work.
- **`x-oai-attestation`.** See "Position on OpenAI's terms" below.

## Position on OpenAI's terms

The endpoint, the OAuth constants and the device flow are all taken from
OpenAI's own open-source `codex` CLI. OpenAI neither documents permission for
third-party clients nor prohibits them; opencode and OpenClaw both ship this
unblocked today.

Two rules follow, and they are requirements, not preferences:

1. **`originator` is `horsie`.** opencode sends its own name and is not blocked.
   We identify ourselves honestly rather than impersonating `codex_cli_rs`.
2. **We never send `x-oai-attestation`.** That header's value is minted by a
   first-party OpenAI client (`codex-rs/app-server/src/attestation.rs` asks the
   connected IDE/desktop client for it over RPC and wraps it in
   `{"v":1,"s":<status>,"t":<token>}`). We cannot produce a valid one, and
   fabricating the envelope would turn a tolerated client into one evading an
   integrity control. If the endpoint ever starts requiring it, that is OpenAI
   closing the door and we stop — we do not work around it.

## Design

### 1. One crate, two credentials

```rust
pub enum Credential {
    ApiKey(Secret),
    ChatGpt(Arc<ChatGptTokens>),
}
```

`ApiKey` posts to `{base_url}/responses` (default `https://api.openai.com/v1`).
`ChatGpt` posts to `https://chatgpt.com/backend-api/codex/responses` with
`Authorization: Bearer <access>` and `ChatGPT-Account-ID: <account_id>`.

Everything else — request building, SSE parsing, retries — is credential-blind.
The client keeps a Cloudflare-only cookie jar for `chatgpt.com`, as codex does
(`codex-rs/http-client/src/chatgpt_cloudflare_cookies.rs`); no account or session
cookie is ever stored.

Two provider `kind` values map onto this one crate: **`openai-responses`**
(API key) and **`chatgpt`** (OAuth). Explicit in config and in the UI, and it
avoids inferring auth mode from the presence of a token row. `kind` stays a
validated `String` in `store.rs`, per the precedent set by the OpenAI provider.

### 2. Request shape

```json
{
  "model": "<model_id>",
  "instructions": "<system prompt>",
  "input": [ /* items, below */ ],
  "tools": [{"type": "function", "name": "...", "description": "...", "parameters": {}}],
  "tool_choice": "auto" | "required" | {"type": "function", "name": "..."},
  "max_output_tokens": 32000,
  "reasoning": {"effort": "medium", "summary": "auto"},
  "store": false,
  "stream": true,
  "include": ["reasoning.encrypted_content"]
}
```

Note the tool shape is **flat** — `{type, name, parameters}` — not the chat
wire's `{type, function: {...}}`. `store: false` is mandatory on the ChatGPT
endpoint and correct everywhere else: horsie owns conversation state.

`ContentPart` → input item:

| horsie part | Responses item |
| --- | --- |
| `Text` (user) | `{"type":"message","role":"user","content":[{"type":"input_text",…}]}` |
| `Text` (assistant) | `{"type":"message","role":"assistant","content":[{"type":"output_text",…}]}` |
| `ToolCall` | `{"type":"function_call","call_id":…,"name":…,"arguments":"<json>"}` |
| `ToolResult` | `{"type":"function_call_output","call_id":…,"output":…}` |
| `Thinking` | `{"type":"reasoning","id":…,"encrypted_content":…,"summary":[]}` |
| `SubAgentResult` | flattened to a text block, as every other provider does |

**Reasoning is replayed here, unlike on the chat wire.** With `store: false` the
model holds no server-side state, so its own reasoning survives to the next turn
only if we echo the encrypted item back. The provider packs
`{"id":…,"enc":…}` as JSON into the existing `ThinkingPart.signature` — already
the opaque provider-bytes field Anthropic uses — so no schema change is needed.
A `ThinkingPart` without a parseable signature is dropped rather than sent as
plain text.

A new `ThinkingDialect::Responses` maps horsie's canonical effort onto
`reasoning.effort`, with `summary: "auto"` so the UI has something to show.

### 3. Stream parsing

| event | effect |
| --- | --- |
| `response.output_item.added` | open a block: `message` → text, `function_call` → tool call, `reasoning` → thinking |
| `response.output_text.delta` | `TextChunkEvent` |
| `response.reasoning_summary_text.delta` | `ThinkingChunkEvent` |
| `response.function_call_arguments.delta` | `ToolCallInputDeltaEvent` |
| `response.output_item.done` | finalise the part; capture `arguments` / `encrypted_content` |
| `response.completed` | usage + `StopReason` |
| `response.incomplete` (`max_output_tokens`) | `StopReason::MaxTokens` → `AgentError::Truncated` |
| `response.failed`, `error` | `LlmError` |

Unrecognised frames are skipped, not fatal — same forgiving stance as the chat
provider, for the same reason.

Usage maps `usage.input_tokens` → `input_tokens` (it already includes cached
tokens, which is what `Usage`'s doc comment requires),
`usage.input_tokens_details.cached_tokens` → `cache_read_tokens`, and
`usage.output_tokens` → `output_tokens` (reasoning tokens included).

### 4. Credential storage and refresh

New table, both dialects:

```sql
CREATE TABLE provider_oauth (
    provider   TEXT PRIMARY KEY,   -- providers.name
    access     TEXT NOT NULL,
    refresh    TEXT NOT NULL,
    expires_at INTEGER NOT NULL,   -- epoch seconds, as in 0014_auth.sql
    account_id TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
```

Separate from `providers.api_key` because these rotate on every refresh, while
`api_key` is a write-only field the operator sets by hand.

`ChatGptTokens` is an `RwLock` over that row plus a write-back callback into the
store. It refreshes when the access token is within 60s of expiry, and once more
on a 401. `build_registry` gains the store handle it needs to construct one; it
stays synchronous — the refresh happens on the request path, not at build time.

**Failure rule (learned from #164):** only a **4xx** from
`https://auth.openai.com/oauth/token` invalidates the stored credential. A
network error or a 5xx — a proxy hiccup, a Caddy 502 — fails the turn and leaves
the refresh token untouched. #164 was exactly this bug: a 502 deleted a valid
login.

### 5. Device-code login

Three endpoints under `/api/admin/providers/:name/chatgpt`. The provider row
itself is still edited through the whole-config `PUT /api/config`; signing in is
a side-effecting action against an existing provider, not a config field, so it
sits under the `/api/admin` prefix that `model-cards` established.

- `POST /login` → server calls `POST {issuer}/api/accounts/deviceauth/usercode`,
  returns `{user_code, verification_url, interval}` for the UI to display.
- `POST /poll` → server calls `.../deviceauth/token`; on approval it exchanges
  the returned `authorization_code` + `code_verifier` at `{issuer}/oauth/token`,
  extracts `chatgpt_account_id` from the id_token claims (falling back to
  `https://api.openai.com/auth`.`chatgpt_account_id`, then `organizations[0].id`),
  and writes `provider_oauth`.
- `DELETE /login` → drops the row.

Device code rather than the browser callback because it is the only flow that
works for a hosted horsie: the OAuth client is OpenAI's own
(`app_EMoamEEZ73f0CkXaXp7hrann`, not registrable by us), its redirects are
`http://localhost:1455/auth/callback` and OpenAI's device callback, and a
deployed server cannot receive either. In the device flow every call is outbound
server→OpenAI and the browser is linked to the server only by the user code, so
no public callback URL, no inbound traffic, no proxy change.

Settings → Providers renders the code, the `auth.openai.com/codex/device` link,
a live "waiting for approval" state, and the signed-in account afterwards.

### 6. Configuration surface

`kind` gains `openai-responses` and `chatgpt`, validated in `store.rs` and
offered in the web selector. Models are configured as they are today
(`models.alias` → `model_id`); no catalog fetch. Codex-only model ids
(`gpt-5.x-codex`, and the `sol`/`terra`/`luna` line) are valid only under the
`chatgpt` kind — a wrong pairing surfaces as the backend's own 4xx, which we
pass through rather than second-guessing with a hardcoded allowlist that would
rot as OpenAI ships models.

## Testing

- `mock-llm` gains `/responses`, driven by the same `MockResponse` queue, emitting
  Responses-shaped SSE.
- `tests/tests/provider_conformance.rs` runs a third kind. Everything except the
  OAuth handshake is covered with no account.
- Unit tests for the refresh state machine (expiry, 401-retry, and the 4xx-only
  invalidation rule) and for device-code polling, against a fake issuer — the
  pattern `mcp/oauth.rs` tests already use.
- Reasoning replay gets a provider-level test: a turn whose history contains a
  `ThinkingPart` must put a `reasoning` item back on the wire with its
  `encrypted_content` intact.
- Live ChatGPT verification is manual, as it is for the OpenAI provider. No
  credential-gated CI job.

## Delivery

Three commits on `feat/chatgpt-plan-provider`, each independently green:

1. The `openai-responses` crate + mock `/responses` + conformance. API key only —
   no OAuth, no migration. Useful on its own: encrypted reasoning and the
   Responses wire for API-key users.
2. `provider_oauth` migrations, `ChatGptTokens` refresh, device-code endpoints,
   the `chatgpt` kind.
3. Web UI sign-in panel, settings docs.

## Acceptance

- A `chatgpt` provider signs in by device code and completes a streaming turn
  with tool calls against a real ChatGPT plan.
- A second turn in the same session replays the prior turn's encrypted reasoning.
- An expired access token refreshes without operator action; a 5xx from the token
  endpoint leaves the stored credential in place.
- An `openai-responses` provider with an API key passes the conformance suite.
- `make check` and `make ts-types` are clean.
