# Thinking signatures and per-model thinking efforts

Closes #51 (thinking signatures bloat the journal) and adds per-model thinking
effort configuration.

## Problem

Two related problems, both rooted in horsie handling extended thinking as a
receive-only concern.

### 1. Thinking signatures are persisted and shipped, and nothing reads them

Measured against the live homelab history API, 5 sessions / 146 thinking blocks:

| session | blocks | thinking text | signature bytes | signature share of response |
|---|---|---|---|---|
| 1191f4fd | 40 | 63.0 KB | 173.6 KB | 37% |
| 3d935209 | 33 | 27.3 KB | 143.2 KB | 46% |
| 75473f5c | 40 | 15.7 KB | 173.6 KB | 46% |
| de4ff5de | 19 | 17.3 KB | 82.5 KB | 44% |
| f521dc31 | 14 | 16.7 KB | 60.8 KB | 40% |

Every signature was exactly 4340 chars regardless of thinking length — a
fixed-size opaque blob at 2.8x-11x the size of the text it accompanies.

The signature reaches three sinks:

- **journal** — `AgentDomainEvent::MessageComplete { message }`
  (`workflow/src/agent_actor.rs:1278`), written through
  `actor/src/file_journal.rs:53-59` as `base64(JSON([base64(event_json)]))`,
  ~1.78x inflation on disk. `snapshot()` is a no-op (`file_journal.rs:83-95`),
  so it never compacts.
- **history API** — `to_wire_history` (`server/src/http/handlers.rs:280-301`)
  moves messages verbatim, no filtering.
- **SSE** — `server/src/sessions/events.rs:130-136` maps `MessageComplete` whole,
  serialized at `server/src/http/sse.rs:40-55`.

The web client never reads it: `clients/web/src/components/ThinkingBlock.tsx:5`
destructures `{ text }` only.

Issue #51 reported only the journal and history costs, and attributed the
signatures to a provider that "doesn't echo them back". Both are wrong — SSE
carries them too, the on-disk cost is ~1.78x the reported figure, and the
signatures are genuine Anthropic-protocol artifacts (kimi is configured as
`kind: "anthropic"` against `https://api.kimi.com/coding/`).

### 2. Thinking is never requested, and cannot be configured

Thinking is plumbed end-to-end on the *response* side but never on the request
side:

- `async-llm/src/types.rs:78-84` has only
  `ThinkingConfig::{Enabled{budget_tokens}, Disabled}` — the pre-4.6 Anthropic
  shape. No `output_config`, no `reasoning_effort`.
- `AnthropicProvider::with_thinking` (`providers/anthropic/src/lib.rs:178`) is
  **dead code with zero callers** workspace-wide.
- The OpenAI wire's `ChatRequest` (`providers/openai/src/wire.rs:65-76`) has no
  reasoning field at all.
- No DB column, config.json key, model-card attribute, session parameter, or UI
  control exists. The only UI knob is the `showThinking` display boolean.

## Evidence: empirical probe of `api.kimi.com/coding/` (model `k3`)

Run 2026-07-27 against the live Anthropic-compatible endpoint.

**Signatures are not validated.** Six tamper variants, in both a plain
multi-turn exchange and a tool-use loop (the case where genuine Anthropic
enforces). All returned 200 with correct answers:

| variant | plain turn | tool-use loop |
|---|---|---|
| signature intact | 200 | 200 |
| signature key omitted | 200 | 200 |
| signature `""` | 200 | 200 |
| signature altered | 200 | 200 |
| thinking block removed | 200 | 200 |
| thinking text tampered | 200 | — |

**`reasoning_effort` is silently ignored on this wire.** n=6 per arm, hard
prompt, comparing `thinking_tokens`:

```
bogus(control)   median 131   range 110-581
low              median 160   range 104-321
max              median 169   range  92-431
```

Fully overlapping. A bogus value returned 200, not the documented 400 — the
Anthropic shim appears to drop unknown top-level keys. Kimi's own Claude Code
integration sets effort via a `CLAUDE_CODE_EFFORT_LEVEL` env var, never a body
field, which is consistent.

**`thinking:{type:"disabled"}` zeroes thinking tokens — but is not trustworthy.**
Kimi Code docs state that disabling thinking on k3 *routes the request to K2.6*,
a different and weaker model. The response echoes `model: "k3"` either way, so
there is no response-level signal to detect the swap. Treated here as: do not
offer `none` for k3.

## Design

Four parts. Parts 1 and 2 close #51; parts 3 and 4 add effort configuration.

### Part 1 — stop creating the bloat at the source

`AnthropicProvider` gains a `keep_thinking_signature: bool`, set at construction
from the provider row. When false, the ingest path
(`providers/anthropic/src/lib.rs:553-563`) emits `signature: None`, so no
signature ever reaches the journal, the history API, or SSE.

This must be provider-scoped rather than message-scoped because `Message`
(`models/fluorite/agent.fl:44-48`) carries no provider identity — the model is
session-level config resolved per turn
(`server/src/sessions/session_actor.rs:452-468`). The provider adapter is the
only place that knows which endpoint produced a block.

Default: **false**. Genuine Anthropic deployments set it true. This is safe by
default for the common self-host case and explicit for the case that needs it.

Config surface: a `keep_thinking_signature` column on `providers`, exposed on
`ProviderView`/`ProviderInput` in `models/fluorite/settings.fl`.

Note `providers/anthropic/src/lib.rs:231` currently replays
`signature.clone().unwrap_or_default()`, turning a `None` into `signature: ""`.
When the flag is off the field must be **omitted**, not emptied.

### Part 2 — strip at the client boundary regardless

Drop `signature` in `to_wire_history` (`server/src/http/handlers.rs:280-301`)
and in the SSE mapper (`server/src/sessions/events.rs:130-136`).

Part 1 does not help sessions whose journals already contain signatures; Part 2
does, and it protects any future provider. The client has no use for the field
under any configuration.

### Part 3 — per-model thinking efforts

#### Canonical vocabulary

```
none | minimal | low | medium | high | xhigh | max
```

Covers all four vendors surveyed. Stored as `String`, not a fluorite enum —
following the `providers.kind` precedent, where PascalCase-ing an existing
string column would break the live DB.

Selection semantics:

- **Absent / unset** — send no thinking control; provider default applies.
- **`none`** — disable thinking.
- **any other value** — set that effort via the model's dialect.

This lets toggle-only models (most GLM) be expressed as `["none"]`: the only
explicit choice is off, and omitting the setting means on.

#### Dialects

The value is portable; the encoding is not. Two models on the *same* provider
kind can need different encodings, and one model can need a different encoding
than its provider kind implies:

| dialect | encoding | models |
|---|---|---|
| `anthropic_effort` | `output_config.effort` + `thinking:{type:"adaptive"}`; `none` -> `thinking:{type:"disabled"}` | Opus 4.6/4.7/4.8, Sonnet 5, Sonnet 4.6 |
| `anthropic_always_on` | `output_config.effort` only, `thinking` omitted; **no `none`** | Fable 5 |
| `anthropic_budget` | thinking enabled via `thinking:{type:"enabled",budget_tokens:N}`, disabled via `{type:"disabled"}`; `output_config.effort` sent only when the model's effort list is non-empty | Opus 4.5, Sonnet 4.5, Haiku 4.5 |
| `openai_effort` | top-level `reasoning_effort` | GPT-5.x, o-series, GLM-5.2, kimi-k3 (OpenAI wire) |
| `zai_thinking` | `thinking:{type,clear_thinking}` | GLM-4.5 through GLM-5.1 |
| `kimi_thinking` | `thinking:{type:"enabled",keep:"all"}` | kimi-k2.6, kimi-k2.7-code |
| `none` | no thinking control | gpt-4o, gpt-4.1, moonshot-v1, kimi k3 on the Anthropic wire |

`kimi/k3` is the case that forces this to be data rather than inference: its
dialect is `openai_effort` on `/coding/v1` and `none` on `/coding/`. The card
seeds the documented value; the configured model row overrides it.

#### Schema

```sql
-- 0010_model_thinking.sql
ALTER TABLE model_cards ADD COLUMN thinking_efforts        TEXT;  -- JSON array, ordered
ALTER TABLE model_cards ADD COLUMN default_thinking_effort TEXT;
ALTER TABLE model_cards ADD COLUMN thinking_dialect        TEXT;
ALTER TABLE models      ADD COLUMN thinking_efforts        TEXT;  -- menu offered to sessions
ALTER TABLE models      ADD COLUMN thinking_effort         TEXT;  -- DEFAULT only; sessions override
ALTER TABLE models      ADD COLUMN thinking_dialect        TEXT;  -- prefilled from card, editable
ALTER TABLE providers   ADD COLUMN keep_thinking_signature INTEGER NOT NULL DEFAULT 0;
```

JSON-in-TEXT follows the `vendors.config` precedent (`0001_init.sql:27`).

Model cards are reference data (what the provider actually supports); the
`models` row is the deployment's editable copy, prefilled from the card at
creation, exactly as `context_window`/`max_tokens` work today.

#### Flow

Thinking level is chosen **per session, at session creation**. The model config
contributes the menu and the default; it is not the effective value.

1. Card seeds `thinking_efforts` + `default_thinking_effort` + `thinking_dialect`.
2. Settings model form prefills from the card on `model_id` match; operator may
   narrow the offered list or override the dialect.
3. **Session creation selects one value** from the configured model's
   `thinking_efforts`. Unset falls back to the model's `thinking_effort`, which
   falls back to the card default, which falls back to "send no thinking
   control".
4. `CompletionRequest` (`agentcore/src/provider.rs:5-11`) gains an optional
   effort field — it currently has no channel for one.
5. The provider adapter translates canonical value + dialect into wire fields.

#### Session surface

The effort is fixed for the session's lifetime. Both Kimi and Anthropic warn
that changing effort mid-conversation invalidates the prompt cache, so it is a
creation-time choice, not a per-turn one.

Wire and storage:

- `AgentSettings` (`models/fluorite/session.fl:21-33`) gains
  `thinking_effort: Option<String>`.
- Its storage twin (`server/src/sessions/spec.rs:32-48`) gains the same field
  with `#[serde(default)]`, so pre-existing journal rows deserialize unchanged —
  the same pattern `mcp_servers` and `memory_spaces` already use.
- The create-session mapping (`server/src/http/handlers.rs:60-70`) carries it
  through and validates it against the resolved model's `thinking_efforts`,
  rejecting a value the model does not offer rather than passing it to the
  provider.

Web client:

- `SessionConfigBar.tsx` gains a thinking-level control alongside the existing
  runtime / model / repos / skills / MCP / memory controls. It is populated from
  the selected model's `thinking_efforts` and **reacts to model changes** — the
  menu and the default both belong to the model, so switching model must
  re-derive them and drop a now-invalid selection.
- A model whose `thinking_efforts` is empty renders no control.
- `SessionDraft` + `buildRequest` (`clients/web/src/hooks/useSessionDraft.ts:22-41`)
  gain the field. Drafts persist to localStorage, so a restored draft naming an
  effort the model no longer offers must fall back to the default rather than
  submitting an invalid value.

#### async-llm change

`async-llm` must gain an `output_config: Option<OutputConfig>` with an `effort`
field, and the OpenAI wire's `ChatRequest` must gain `reasoning_effort`. The
existing `ThinkingConfig` covers only the `anthropic_budget` dialect. This makes
the work a cross-repo stack: async-llm first, then horsie.

### Part 4 — model card reseed

`server/src/config/model_cards_seed.json` currently holds 8 cards that are both
stale and **wrong**: `claude-sonnet-4-6` is listed at 200K context / 16K output
when it is 1M / 128K, and `claude-haiku-4-5` at 8K output when it is 64K. Those
values prefill into model config, so the errors propagate.

Full refresh: drop retired/superseded entries, correct the retained ones, and
add current Anthropic / OpenAI / Kimi / z.ai models with efforts, default and
dialect. Seeding is startup-idempotent, so this is safe to re-run.

Values below are from vendor documentation. Rows marked (!) could not be
verified against primary docs and are seeded on best available information.

**Anthropic** — `anthropic_effort` unless noted.

| model_id | ctx | max out | efforts | default | dialect |
|---|---|---|---|---|---|
| claude-fable-5 | 1M | 128K | low,medium,high,xhigh,max | high | `anthropic_always_on` |
| claude-opus-4-8 | 1M | 128K | none,low,medium,high,xhigh,max | high | `anthropic_effort` |
| claude-opus-4-7 | 1M | 128K | none,low,medium,high,xhigh,max | high | `anthropic_effort` |
| claude-opus-4-6 | 1M | 128K | none,low,medium,high,max | high | `anthropic_effort` |
| claude-sonnet-5 | 1M | 128K | none,low,medium,high,xhigh,max | high | `anthropic_effort` |
| claude-sonnet-4-6 | 1M | 128K | none,low,medium,high,max | high | `anthropic_effort` |
| claude-opus-4-5 | 200K | 64K | none,low,medium,high | high | `anthropic_budget` |
| claude-sonnet-4-5 | 200K | 64K | none | — | `anthropic_budget` |
| claude-haiku-4-5 | 200K | 64K | none | — | `anthropic_budget` |

Opus 4.5 is the mixed case the `anthropic_budget` dialect exists to cover:
thinking is enabled via `budget_tokens`, but `output_config.effort` is
independently honored (low/medium/high only). Sonnet 4.5 and Haiku 4.5 reject
`effort` outright, which is why their effort lists are `none`-only.

`claude-mythos-5` is omitted — Project Glasswing only, not generally reachable.

**OpenAI** — `openai_effort` unless noted.

| model_id | ctx | max out | efforts | default |
|---|---|---|---|---|
| gpt-5.6 | 1.05M | 128K | none,low,medium,high,xhigh,max | medium |
| gpt-5.6-sol | 1.05M | 128K | none,low,medium,high,xhigh,max | medium |
| gpt-5.6-terra | 1.05M | 128K | none,low,medium,high,xhigh,max | medium |
| gpt-5.6-luna | 1.05M | 128K | none,low,medium,high,xhigh,max | medium |
| gpt-5.5 | 1.05M | 128K | none,low,medium,high,xhigh | medium |
| gpt-5.4 | 1.05M | 128K | none,low,medium,high,xhigh | none |
| gpt-5.4-mini | 400K | 128K | none,low,medium,high,xhigh | none |
| gpt-5.4-nano (!) | 400K | 128K | none,low,medium,high,xhigh | none |
| o3 | 200K | 100K | low,medium,high | medium |
| o4-mini | 200K | 100K | low,medium,high | medium |
| gpt-4.1 | 1M | 32K | — (dialect `none`) | — |
| gpt-4o | 128K | 16K | — (dialect `none`) | — |

OpenAI does not publish per-model effort lists; these come from the reasoning
guide and are worth confirming against a live 400 before relying on them.

**Kimi** — two namespaces for the same models.

| model_id | ctx | max out | efforts | dialect |
|---|---|---|---|---|
| k3 | 1M | 128K | — | `none` (Anthropic wire; see above) |
| k3-256k | 256K | 128K | — | `none` |
| kimi-k3 | 1M | 128K | low,high,max | `openai_effort` |
| kimi-for-coding | 256K | 32K | — | `none` |
| kimi-for-coding-highspeed | 256K | 32K | — | `none` |
| kimi-k2.7-code | 256K | 32K | — | `kimi_thinking` |
| kimi-k2.7-code-highspeed | 256K | 32K | — | `kimi_thinking` |
| kimi-k2.6 | 256K | 32K | none | `kimi_thinking` |
| kimi-k2.5 (!) | 256K | 32K | none | `kimi_thinking` |

Kimi Code docs give k3 default effort `high`; platform docs say `max`.
Unresolved; seeded without a default.

**z.ai** — values documented for the OpenAI-compatible wire. The
Anthropic-compatible endpoint (`https://api.z.ai/api/anthropic`) has **no
official parameter reference**; thinking control there is unverified.

| model_id | ctx | max out | efforts | dialect |
|---|---|---|---|---|
| glm-5.2 | 1M | 128K | none,minimal,low,medium,high,xhigh,max | `openai_effort` (default `max`) |
| glm-5.1 | 200K | 128K | none | `zai_thinking` |
| glm-5 | 200K | 128K | none | `zai_thinking` |
| glm-5-turbo | 200K | 128K | none | `zai_thinking` |
| glm-4.7 | 200K | 128K | none | `zai_thinking` |
| glm-4.6 | 200K | 128K | none | `zai_thinking` |
| glm-4.5 (!) | 128K | 96K | none | `zai_thinking` |
| glm-4.5-air (!) | 128K | 96K | none | `zai_thinking` |

## Testing

- Unit: dialect translation — each canonical value x each dialect produces the
  expected request JSON; `none` on `anthropic_always_on` is rejected at config
  time, not sent.
- Unit: `keep_thinking_signature=false` yields `signature: None` at ingest, and
  the replay path omits the field rather than sending `""`.
- Unit: history and SSE serializers drop `signature` even when present in state.
- Unit: session-creation resolution order — explicit session value wins over
  the model default, which wins over the card default, which falls back to
  sending no thinking control.
- Unit: session creation rejects an effort the resolved model does not offer.
- Migration: 0010 applies to a populated DB; existing rows get NULL efforts and
  behave as "provider default" (no behaviour change).
- Storage: a pre-existing session journal row without `thinking_effort`
  deserializes and replays unchanged.
- e2e: selecting a thinking level at session creation reaches the provider
  request; switching model in the config bar re-derives the menu and clears an
  invalid selection; a restored localStorage draft with a stale effort falls
  back to the default.
- Seed: idempotent re-run leaves no duplicates and corrects changed values.
- e2e: existing mock-LLM thinking tests still pass with signatures stripped.

## Out of scope

- Journal framing (`base64(JSON([base64(...)]))`, ~1.78x) and the no-op
  `snapshot()` — real wins, but they belong with the actor-audit work in #61.
- The streamed-signature length discrepancy (12,946 chars observed
  non-streaming vs 4,340 stored by horsie), suggesting the `signature_delta`
  accumulator at `providers/anthropic/src/lib.rs:469-480` may truncate. Moot
  once signatures are dropped; worth its own issue.
- Changing thinking level on an existing session. Creation-time only, because
  switching effort mid-conversation invalidates the prompt cache.
- Pricing columns on model cards.

## Open questions

- OpenAI per-model effort lists are not officially published; confirm against
  live 400s before trusting the seeded lists.
- z.ai Anthropic-wire thinking control is undocumented. If horsie points a
  z.ai provider at the Anthropic endpoint, dialect must be set manually.
- Whether k3's `thinking:{type:"disabled"}` genuinely swaps to K2.6 could not
  be confirmed from the response. Avoided by not offering `none` for k3.
