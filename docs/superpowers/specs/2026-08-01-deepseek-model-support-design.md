# DeepSeek model support

## Goal

Make DeepSeek's `deepseek-v4-flash` and `deepseek-v4-pro` first-class models in
horsie: correct catalog entries, working thinking control, and — the only real
blocker — forced tool calls that do not 400.

Model cards also learn a canonical `base_url`, recording where a model is
officially served. It is stored and editable now; consuming it as a prefill in
Settings is deliberately left for later.

## What was already true

horsie can already reach DeepSeek today. A provider with `kind: "openai"` and
`base_url: "https://api.deepseek.com"` resolves to `/v1/chat/completions`
(`providers/openai/src/lib.rs`), and the wire layer already reads
`reasoning_content` into a `ThinkingPart` — DeepSeek is named in that field's
doc comment (`providers/openai/src/wire.rs`).

Two things that looked like gaps are not, both settled by probing the live API
rather than reading the docs:

- **`openai_effort` is the correct dialect.** The API reference documents effort
  as nested `thinking.reasoning_effort`. That field is silently ignored. Only
  the top-level `reasoning_effort` is honored, which is exactly what
  `ThinkingDialect::OpenAiEffort` already emits.
- **Cache accounting already works.** DeepSeek reports both
  `prompt_cache_hit_tokens` and `prompt_tokens_details.cached_tokens`, and they
  agree. horsie reads the latter, so `cache_read_tokens` is already correct.

## Probe findings

All measured against the live API on 2026-08-01 with `deepseek-v4-flash` unless
noted. These supersede the published documentation wherever they disagree.

### Effort placement

`prompt_tokens` for a fixed prompt is the tell — DeepSeek injects thinking
scaffolding whose size tracks the effort:

| request | `prompt_tokens` |
| --- | --- |
| no thinking control | 125 |
| top-level `reasoning_effort: low` | 46 |
| top-level `reasoning_effort: high` | 125 |
| top-level `reasoning_effort: max` | 138 |
| nested `thinking.reasoning_effort: low` | 125 |
| nested `thinking.reasoning_effort: high` | 125 |
| nested `thinking.reasoning_effort: max` | 125 |

Every nested variant equals the no-control baseline, so nested effort is inert.
With both set (`top=low`, `nested=max`) the result is 46 — top-level wins.

`thinking: {type: "disabled"}` *is* honored, but it is redundant:
`reasoning_effort: "none"` disables thinking just as completely (no
`reasoning_content`, no `completion_tokens_details`).

### Effort vocabulary

An invalid value returns a 400 that enumerates the accepted set:

```
reasoning_effort: unknown variant `bogus`,
expected one of `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`
```

That is horsie's `ThinkingEffort` ladder exactly. The docs' claim of
"low/high/max only" is stale. On `deepseek-v4-pro`, `low` and `high` are
currently indistinguishable (both yield `prompt_tokens=46`); DeepSeek's docs say
full support arrives early August 2026. All seven are accepted by both models.

### Forced tool choice conflicts with thinking

The blocker. `deepseek-v4-flash`, tools present:

| `tool_choice` | thinking on | `reasoning_effort: "none"` or `thinking` disabled |
| --- | --- | --- |
| `auto` | OK | OK |
| `none` | OK | OK |
| `required` | **400** | OK |
| named function | **400** | OK |

The 400 body is `Thinking mode does not support this tool_choice`.

horsie maps `ToolChoice::Any` to `"required"` and `ToolChoice::Required(name)`
to a named function (`providers/openai/src/lib.rs`). `agentcore/src/agent.rs`
selects `ToolChoice::Any` for every turn of a **forced-handoff agent**, with the
comment "Honoring `tool_choice` is a hard requirement of every provider." So
handoff-style sub-agents hard-fail on DeepSeek whenever thinking is enabled —
which is the default, since DeepSeek thinks unless told not to.

### Limits

- Context window: **1,048,576** tokens. A 3M-token request returns
  `This model's maximum context length is 1048576 tokens`.
- `max_tokens`: valid range **[1, 393216]**, identical on both models.
- Streaming is clean: zero unparseable SSE frames across every probe. Tool calls
  stream normally alongside `reasoning_content`.

## Design

### Where the capability lives

The conflict is a per-model fact, so it is stored as data on the model card
rather than inferred. This follows the principle migration 0011 already states
for `thinking_dialect`: two models on one provider kind can need different
request shapes, "so this is data, not inference".

Two alternatives were rejected:

- **Infer from `model_id.contains("deepseek")`.** Zero schema change, but it is
  the inference the codebase deliberately rejected, and it breaks for any proxy
  that renames the model (OpenRouter serves it as `deepseek/deepseek-v4-flash`).
- **A new `ThinkingDialect` variant.** Avoids a column, but conflates two
  orthogonal axes — how effort is encoded versus whether forced tools tolerate
  thinking — turning the enum into a cross-product the moment a second provider
  shares the quirk.

### Data model

Migration `0013_deepseek_v4_and_card_base_url.sql`:

```sql
ALTER TABLE model_cards ADD COLUMN base_url                      TEXT;
ALTER TABLE model_cards ADD COLUMN forced_tools_disable_thinking INTEGER NOT NULL DEFAULT 0;
ALTER TABLE models      ADD COLUMN forced_tools_disable_thinking INTEGER NOT NULL DEFAULT 0;
```

`models` deliberately gets no `base_url`: the endpoint is a property of the
provider, and `providers.base_url` already holds it. On the card it is a
*prefill hint* — see "A card now prefills two different rows" below.

`deepseek-chat` is deleted — it is superseded, and its seeded limits
(128k/8192) were never right for the v4 models. Card seeding is
insert-if-missing and so can never correct an existing row, which is why the two
v4 cards are written with an upsert in the migration rather than left to the
seed file. Migration 0012 set both precedents.

Card values for `deepseek-v4-flash` and `deepseek-v4-pro` (identical):

| field | value |
| --- | --- |
| `base_url` | `https://api.deepseek.com` |
| `context_window` | 1048576 |
| `max_tokens` | 393216 |
| `thinking_dialect` | `openai_effort` |
| `thinking_efforts` | `none, minimal, low, medium, high, xhigh, max` |
| `default_thinking_effort` | `high` |
| `forced_tools_disable_thinking` | true |

Existing cards keep `base_url` NULL. Backfilling canonical endpoints for the
Anthropic, OpenAI, Kimi and GLM cards is a separate, mechanical change and is
not part of this work.

`default_context_window` in `server/src/config/store.rs` maps `"deepseek"` to
128,000; it becomes 1,048,576.

### What `base_url` on a card means

"The model's canonical first-party endpoint" — reference data, nothing more. The
same model id is legitimately served by OpenRouter, a local vLLM, or any proxy,
so the value is a hint and never authoritative.

Nothing reads it in this change. No server code consults it, and the Settings
form does not prefill from it: cards remain prefill templates that are never
linked to configured models, and request routing still reads
`providers.base_url` alone. This step only gives the catalog somewhere to record
the endpoint, and gives the admin UI a way to edit it.

Note the shape it will eventually have, since it is unlike every existing card
field: all of those prefill the *model* row, whereas `base_url` corresponds to
the *provider* row. Wiring that up means reaching across two sections of the
Settings page, which is exactly why it is a separate piece of work.

### Provider behaviour

`OpenAiProvider` gains `with_forced_tools_disable_thinking(bool)`. In
`build_body`, once `tool_choice` has been computed:

```rust
// `tool_choice` is Some only when tools exist AND the choice is not Auto —
// exactly the cases DeepSeek rejects under thinking.
reasoning_effort: if self.forced_tools_disable_thinking && tool_choice.is_some() {
    Some("none".to_string())
} else {
    match (self.thinking_dialect, request.thinking_effort) { /* unchanged */ }
}
```

`tool_choice.is_some()` is the whole condition, and it is worth being precise
about why: the existing code already yields `None` both when `tools` is empty
and when the choice is `ToolChoice::Auto`, leaving `Some` for precisely
`ToolChoice::Any` (`"required"`) and `ToolChoice::Required(name)` (a named
function) — the two rows that 400. No separate `Auto` check is needed, and
adding one would be dead logic.

The override sits outside the dialect match on purpose. It must fire even for
`ThinkingDialect::NoControl`, because DeepSeek's default is thinking *on* —
sending no thinking control at all still 400s on a forced tool call. A fix
placed inside the dialect match would miss exactly that case.

A consequence to keep in mind while implementing: this reads `tool_choice`
after it is built, so the assignment has to move below that binding in
`build_body`.

### Wiring

Wire types are fluorite-generated, so each new field is declared once in
`models/fluorite/` and the TypeScript follows:

- `model_cards.fl` — `base_url` and `forced_tools_disable_thinking` on
  `ModelCard`, `ModelCardInput` and `ModelCardUpdate`.
- `settings.fl` — `forced_tools_disable_thinking` on `ModelInput` (and the
  model view). `ProviderInput` is untouched; it already carries `base_url`.

`ModelRow` in `server/src/config/store.rs` gains the flag; the `COLUMNS`
constants and queries in `store.rs` and `server/src/config/model_cards.rs`
extend to carry the new columns; `build_registry` passes the flag to
`build_openai`. `build_anthropic` ignores it — the Anthropic wire has no such
conflict — noted at the call site.

The seed file `model_cards_seed.json` replaces its `deepseek-chat` line with the
two v4 entries.

### Web UI

`ModelCardsPage` (admin) gains a base-URL text input and a
"forced tools disable thinking" checkbox, alongside the existing card fields.
This is the only surface that reads or writes `base_url`.

In `ModelsSettings`, `ModelDraft` gains `forcedToolsDisableThinking`, prefilled
by `pick(card)` like every other card-backed field and editable in `ModelRow` as
a checkbox next to the thinking controls. `pick(card)` is otherwise unchanged —
it still patches only the model draft, and ignores the card's `base_url`.

### Consequence worth stating plainly

A forced-handoff agent on DeepSeek runs with thinking off for every turn, since
every turn of that loop uses `ToolChoice::Any`. That is the only combination
DeepSeek permits. It makes DeepSeek a poor choice for handoff-style sub-agents,
and the guide says so rather than leaving operators to discover it.

## Testing

- `providers/openai` unit tests over `build_body`:
  - forced tool choice + flag set → `reasoning_effort: "none"`
  - forced tool choice + flag unset → dialect value, unchanged
  - `ToolChoice::Auto` + flag set → dialect value, unchanged
  - `ThinkingDialect::NoControl` + forced + flag → `"none"` (the case a
    dialect-local fix would miss)
  - no tools + flag set → no `tool_choice`, no `reasoning_effort` change
- Model-card store: both new columns round-trip through create/read/update; the
  migration removes `deepseek-chat` and seeds both v4 cards with the values
  above.
- Web UI, in the existing Playwright suite (`clients/web/e2e/`):
  `forcedToolsDisableThinking` prefills from the card and survives a save, and
  picking a card with a `base_url` leaves the provider rows untouched.
- Provider conformance suite is unchanged. DeepSeek is `kind = "openai"` and is
  already covered by the OpenAI arm.
- An `#[ignore]`d live smoke test gated on `DEEPSEEK_API_KEY` asserting that a
  forced tool call against `deepseek-v4-flash` succeeds.

## Docs

A guide section on adding DeepSeek: `kind: "openai"`,
`base_url: "https://api.deepseek.com"`, the full effort ladder, and the
forced-tool consequence above.

## Out of scope

- A distinct `deepseek` provider kind. The wire is OpenAI-compatible; a separate
  kind would add store validation, settings UI and docs surface for no
  behavioural difference.
- A `deepseek_thinking` dialect. `openai_effort` is already the correct
  encoding, as measured.
- Any change to cache or usage parsing. Already correct, as measured.
- Backfilling `base_url` onto the existing Anthropic, OpenAI, Kimi and GLM
  cards. Mechanical, and easier to review on its own.
- Any server-side use of a card's `base_url`. Cards stay prefill templates that
  are never linked to configured models; request routing still reads
  `providers.base_url` alone.
- Prefilling a provider's base URL from a card in the Settings form. Deferred by
  choice: it reaches across the page's two sections, and the column is useful on
  its own before that exists.
