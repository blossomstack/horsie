# Provider independence: a second wire protocol + a conformance suite

Issue: [#16](https://github.com/blossomstack/horsie/issues/16)
Date: 2026-07-20
Status: approved, ready for planning

## Problem

horsie claims provider independence but has never demonstrated it. A "second
backend" is reachable today without code changes — `examples/README.md:26-31`
points at Moonshot Kimi via `{"type": "anthropic", "base_url": "..."}` — but
that is a URL swap on the same wire format. It proves nothing about the seam.

Independence becomes a real property only when a core flow runs end to end on a
genuinely different wire protocol. The first target is OpenAI-compatible
`/v1/chat/completions`, because one implementation covers Ollama, vLLM,
llama.cpp, OpenRouter and DeepSeek.

## What the code actually says

Verified against the tree at `origin/main` (4b9418a):

- **The seam is clean.** `LlmProvider` (`agentcore/src/provider.rs:35-46`) is
  provider-neutral and dispatched as `Arc<dyn LlmProvider>` throughout. A new
  backend is a new crate, not a refactor.
- **`async-llm` is Anthropic-wire only.** Our fork (pinned by git rev at
  `Cargo.toml:36`) exposes `messages.rs` plus Anthropic-shaped `types.rs`. There
  is no `chat/completions` support to reuse, and `errors.rs` is
  `AnthropicError`-shaped throughout.
- **`providers/mock-llm` is an HTTP server, not a mock provider.** It serves the
  Anthropic wire at `/v1/messages` from a `MockResponse` queue
  (`server.rs:17`). It is a different thing from `MockProvider`, which is
  `#[cfg(test)]`-private inside `agent.rs:559`. Both are load-bearing here.
- **Provider kind is hardcoded in five places** — the four the issue lists
  (`server/src/config/store.rs:356`, `:613`, `:631`;
  `clients/web/src/pages/SettingsPage.tsx:214`) plus `ProviderInput.kind:
  String` in `models/fluorite/settings.fl:112`.
- **CI is one `check` job**, no secrets, running `cargo test --locked
  --workspace --all-features`.

## Scope

In scope: all six acceptance criteria from #16, plus the `stop_reason` bug.

Out of scope, deliberately:

- **Context-window management.** Unbounded history growth (`agent.rs:331`) is
  real, but it is not in the issue's acceptance criteria and is a design project
  of its own: which trimming policy, does summarization call the model, and how
  does any of it interact with the event-sourced session journal whose sequence
  numbers are the SSE cursor space and must stay stable
  (`workflow/src/agent_actor.rs:58`). Gets its own issue.
- **A live-backend CI job.** The conformance suite runs against `mock-llm` only.
  Live-backend verification happens manually once the change is ready, so no
  repo secret and no fork-PR secret convention is introduced here.
- **A third backend.** Follow-up once the suite and the kind enum exist.
- **Pointing the Playwright suite at a live backend.** Its design assumes a
  global FIFO of exact programmed responses (`clients/web/playwright.config.ts:10-11`).

## Design

### 1. Shared test kit

`MockProvider`, `CollectingEventSink` and `MockToolbox` are trapped inside
`#[cfg(test)] mod tests` in `agent.rs`, so no other crate can reuse them. Move
them to a `testkit` module behind a feature, matching the existing precedent at
`server/Cargo.toml:11`:

```rust
#[cfg(any(test, feature = "test-util"))]
pub mod testkit;
```

The `any(test, ...)` guard is required: it keeps the kit available to
agentcore's own unit tests without the feature, so `cargo test -p
horsie-agentcore` continues to pass unfeatured.

### 2. A two-wire mock, and the conformance suite

`providers/mock-llm` grows a `/v1/chat/completions` route serving the **same**
`MockResponse` queue in OpenAI SSE format. The enum is already wire-neutral:
`Text`, `ToolCall`, `Error`, `TextStream` and `ToolCallStream` all map cleanly.

`Thinking` is the one exception — it has no OpenAI equivalent, so the OpenAI
route emits nothing for it.

The suite lives at `tests/tests/provider_conformance.rs` in the existing
`integration-tests` crate, which already dev-deps `horsie-anthropic` and
`horsie-mock-llm`. It runs the same agent-loop assertions against each provider
pointed at its own wire on the same mock server.

Shared assertions are the wire-neutral subset:

- a plain text turn
- tool call → tool result → final text
- multi-turn history replay
- error classification (429 → `RateLimit`, 5xx → `Overloaded`)
- `max_tokens` truncation surfaces as truncation

Thinking-block replay stays an **Anthropic-only** test. Asserting it on both
backends would be asserting a fiction.

**This satisfies the CI criterion with no `ci.yml` change.** `integration-tests`
is a workspace member and CI already runs `--workspace --all-features`, so the
suite runs on every PR: both kinds, no secrets, deterministic.

### 3. The OpenAI provider

A new `providers/openai` crate: hand-rolled `reqwest` + `reqwest-eventsource`,
with its own types for the `/v1/chat/completions` subset.

Hand-rolled rather than `async-openai` because "OpenAI-compatible" is a family
of near-misses, and the value here is one implementation covering five backends.
That needs deliberately forgiving parsing we control — `#[serde(default)]`
throughout, tolerate missing `usage`, tolerate unknown fields. A crate typed
against real OpenAI fights that goal. Rather than extending the `async-llm`
fork, because every iteration would become a cross-repo change plus a new pinned
git rev, during exactly the phase where compat quirks are being discovered.

Mapping concerns that stay **inside** the crate, needing no cross-cutting change:

- **Role expansion.** `Role::Tool` becomes a distinct `role: "tool"` message per
  result, rather than Anthropic's collapse of both `User` and `Tool` into `user`
  (`providers/anthropic/src/lib.rs:200-207`).
- **Thinking parts are dropped** on history replay — no equivalent exists.
- **Error classification** against OpenAI-shaped bodies, so a 429 is a
  `RateLimit` rather than falling through to `Network` and silently consuming
  the retry budget.
- **No `cache_control`** is ever emitted.

### 4. Capability surface: one field

Four of the five breakages the issue lists need no flag — they are already
provider-local, which is the trait doing its job. `cache_control` lives entirely
in `providers/anthropic` (`:240`); role collapsing, error string-matching and
thinking replay are all per-provider mapping. Building config for them would be
inventing fields with no reader.

Prompt caching stays **unconditional** in the Anthropic provider, exactly as
today. The scenario that would justify a toggle — an Anthropic-wire backend
rejecting `cache_control` — is hypothetical, not observed. Add the knob when a
real backend is seen rejecting it.

Streaming likewise stays mandatory: every backend in the target set supports SSE
on `chat/completions`. Documented as a known limit.

So the capability surface is exactly one field:

```rust
pub struct ProviderCapabilities {
    pub supports_tool_choice: bool,   // Ollama and llama.cpp may ignore or reject it
}

// on LlmProvider, with a fully-capable default impl
fn capabilities(&self) -> ProviderCapabilities { ProviderCapabilities::default() }
```

Its one consumer is the forced-handoff path.

### 5. Two bug fixes at the provider boundary

**Forced `tool_choice` in workflows.** `workflow/src/agent_actor.rs:240-242` sets
`force_handoff_choice: true` whenever `optional_handoff_tool` is `None`, which
`from_def` always sets (`:52`). All four agents in `examples/dev-workflow.json`
have an `outputSchema` (verified: 4 agents, 4 schemas), so every one sends
`tool_choice: {"type":"any"}`. This path consults `capabilities()` and falls back
to prompt-level instruction when the backend can't force a choice.

**`stop_reason` is never read in production.** It is computed at
`providers/anthropic/src/lib.rs:486`, but every consumer in `agent.rs` is
`#[cfg(test)]`; the live loop branches only on whether tool calls are empty
(`agent.rs:335`). A `max_tokens` truncation is silently treated as a normal
end of turn. The loop reads it: `MaxTokens` with no tool calls becomes a real
error rather than a silent success.

### 6. Configuration

Two paths, treated differently on purpose.

**`cli/src/config.rs:73`** is already `#[serde(tag = "type")]` with a lone
`Anthropic` variant. Add an `Openai` variant. Genuinely a tagged union.

**Settings / fluorite / DB**: `kind` becomes a closed `ProviderKind` enum
(`Anthropic | Openai`) replacing the free-form `String`, keeping the flat field
set.

This is a deliberate deviation from the issue's wording, which asks for a tagged
union mirroring `VendorConfigInput` (`settings.fl:136`). That union earns its
keep because velos carries ~10 fields no other vendor has. The Anthropic and
OpenAI providers have *identical* config — `name`, `base_url`, `api_key` — so a
union there would be two variants with the same payload plus a DB migration,
buying nothing. The enum delivers what the criterion actually wants: `kind` stops
being a free string, both `store.rs` guards validate a closed set, and
construction dispatches on it. Widen to a union when a backend with genuinely
different config lands (Bedrock: region, profile).

`SettingsPage.tsx:214` gets a kind selector, replacing the hardcoded
`kind: "anthropic"`.

## Delivery

Four stacked PRs off `feat/openai-provider`:

1. Test kit — `testkit` module behind `test-util`, plus the conformance suite
   running Anthropic-only against `mock-llm`. Establishes the harness.
2. OpenAI wire — `/v1/chat/completions` route on `mock-llm`, the
   `providers/openai` crate, suite green on both kinds.
3. Capability flag + the two bug fixes (workflow forced `tool_choice`,
   `stop_reason`).
4. Config kind enum (fluorite + `store.rs` + cli), Settings UI selector, docs.

## Acceptance

- [ ] A second `LlmProvider` for a different wire protocol exists.
- [ ] `kind` is a closed set on both config paths; both `store.rs` guards accept
      both kinds; construction dispatches; the Settings UI has a selector.
- [ ] A conformance suite both implementations pass, with the test kit promoted
      out of `#[cfg(test)]`.
- [ ] Anthropic-specific behavior is provider-local or behind
      `supports_tool_choice`; the workflow forced-`tool_choice` bug is fixed.
- [ ] One command runs the suite green on both backends.
- [ ] CI runs it for both kinds on every PR, no secrets — via the existing
      `--workspace --all-features` job.
- [ ] `make check` green.
