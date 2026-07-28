# Thinking Efforts — UI and Catalog Reseed (Plan D)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make thinking effort selectable in the browser, backed by a current, correct model catalog.

**Architecture:** A data migration corrects and prunes the existing eight cards (seeding is insert-if-missing and will never fix an existing row, so corrections must be a migration). The bundled seed then supplies the current catalog with efforts, defaults and dialects. The settings model form gains the three fields with card prefill; the session config bar gains a control populated from the selected model.

**Tech Stack:** SQLite migrations, Rust, React/TypeScript.

## Global Constraints

- Depends on **Plan C** (schema + session field must exist).
- Card seeding is **insert-if-missing** (`server/src/config/model_cards.rs`, `seed`). Do **not** convert it to an upsert — cards are operator-editable via `/api/admin/model-cards`, and an upsert would clobber operator edits on every restart. Corrections to existing rows belong in the migration.
- Cards are prefill templates only and are never linked to configured models (`0008_model_cards.sql`), so deleting a card cannot break a running config.
- Editing any `.fl` requires `cd clients/web && npm run generate-types`.
- `clients/web` has **no `lint` script**; the typecheck gate is `npx tsc -b` (or `npm run build`).
- Do not commit `clients/web/package-lock.json` — the repo does not track it.
- Never list Claude as author/co-author on commits.

---

### Task 1: Correct and reseed the model catalog

**Files:**
- Create: `server/migrations/0012_model_card_corrections.sql`
- Modify: `server/src/config/model_cards_seed.json`

**Interfaces:**
- Consumes: the `thinking_efforts` / `default_thinking_effort` / `thinking_dialect` columns from Plan C's migration `0011`.
- Produces: a corrected, current card catalog.

- [ ] **Step 1: Write the correction migration**

The existing seed shipped wrong numbers that prefill into model config. Create `server/migrations/0012_model_card_corrections.sql`:

```sql
-- The original bundled catalog shipped incorrect limits for two current models
-- and four entries that are superseded. Card seeding is insert-if-missing, so
-- it can never fix a row that already exists — corrections have to happen here.
--
-- Cards are prefill templates and are never linked to configured models
-- (0008_model_cards.sql), so deleting one cannot break a running deployment.

-- Wrong limits: both are 1M context / 128K output, not 200K / 16K and 200K / 8K.
UPDATE model_cards
   SET context_window = 1000000, max_tokens = 128000, updated_at = datetime('now')
 WHERE model_id = 'claude-sonnet-4-6';

UPDATE model_cards
   SET context_window = 200000, max_tokens = 64000, updated_at = datetime('now')
 WHERE model_id = 'claude-haiku-4-5';

-- Superseded entries. Removed so the prefill menu reflects what is current;
-- an operator who still wants one can re-add it via /api/admin/model-cards.
DELETE FROM model_cards WHERE model_id IN ('o1', 'claude-opus-4-1', 'gpt-4o');
```

`o3` is deliberately retained — it is still served (retires Dec 2026) and gets thinking metadata in the seed below.

- [ ] **Step 2: Write the failing test**

Append to `mod tests` in `server/src/config/model_cards.rs`:

```rust
    #[tokio::test]
    async fn bundled_seed_carries_thinking_metadata() {
        let cards = bundled_seed().expect("bundled seed parses");

        let opus = cards
            .iter()
            .find(|c| c.model_id == "claude-opus-4-8")
            .expect("catalog includes claude-opus-4-8");
        assert_eq!(opus.context_window, Some(1_000_000));
        assert_eq!(opus.max_tokens, Some(128_000));
        assert_eq!(opus.thinking_dialect.as_deref(), Some("anthropic_effort"));
        assert_eq!(opus.default_thinking_effort.as_deref(), Some("high"));
        let efforts = opus.thinking_efforts.as_ref().expect("efforts listed");
        assert!(efforts.contains(&"xhigh".to_string()));
        assert!(efforts.contains(&"none".to_string()));

        // Fable 5 cannot disable thinking — offering `none` would produce a 400.
        let fable = cards
            .iter()
            .find(|c| c.model_id == "claude-fable-5")
            .expect("catalog includes claude-fable-5");
        assert_eq!(fable.thinking_dialect.as_deref(), Some("anthropic_always_on"));
        assert!(
            !fable
                .thinking_efforts
                .as_ref()
                .expect("efforts listed")
                .contains(&"none".to_string()),
            "Fable 5 must not offer `none`"
        );

        // Opus 4.6 predates xhigh.
        let o46 = cards
            .iter()
            .find(|c| c.model_id == "claude-opus-4-6")
            .expect("catalog includes claude-opus-4-6");
        assert!(
            !o46.thinking_efforts
                .as_ref()
                .expect("efforts listed")
                .contains(&"xhigh".to_string()),
            "xhigh arrived with Opus 4.7"
        );
    }

    #[tokio::test]
    async fn bundled_seed_efforts_and_dialects_are_canonical() {
        for c in bundled_seed().expect("bundled seed parses") {
            if let Some(d) = c.thinking_dialect.as_deref() {
                assert!(
                    horsie_agentcore::ThinkingDialect::parse(d).is_some(),
                    "{}: unknown dialect {d}",
                    c.model_id
                );
            }
            let efforts = c.thinking_efforts.clone().unwrap_or_default();
            for e in &efforts {
                assert!(
                    horsie_agentcore::ThinkingEffort::parse(e).is_some(),
                    "{}: unknown effort {e}",
                    c.model_id
                );
            }
            if let Some(def) = c.default_thinking_effort.as_deref() {
                assert!(
                    efforts.iter().any(|e| e == def),
                    "{}: default {def} not among offered efforts",
                    c.model_id
                );
            }
            if let (Some(d), false) = (c.thinking_dialect.as_deref(), efforts.is_empty()) {
                let dialect = horsie_agentcore::ThinkingDialect::parse(d).expect("checked above");
                for e in &efforts {
                    let effort = horsie_agentcore::ThinkingEffort::parse(e).expect("checked above");
                    assert!(
                        dialect.supports(effort),
                        "{}: dialect {d} cannot express effort {e}",
                        c.model_id
                    );
                }
            }
        }
    }
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p horsie-server bundled_seed`
Expected: FAIL — `claude-opus-4-8` is not in the catalog.

- [ ] **Step 4: Replace the seed**

Replace `server/src/config/model_cards_seed.json` entirely. Values are from vendor documentation as of 2026-07-27; entries whose numbers could not be confirmed against primary docs are marked in the review notes below rather than in the JSON (the file is data, not prose).

```json
[
  { "modelId": "claude-fable-5", "name": "Claude Fable 5", "contextWindow": 1000000, "maxTokens": 128000, "thinkingEfforts": ["low","medium","high","xhigh","max"], "defaultThinkingEffort": "high", "thinkingDialect": "anthropic_always_on" },
  { "modelId": "claude-opus-4-8", "name": "Claude Opus 4.8", "contextWindow": 1000000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","xhigh","max"], "defaultThinkingEffort": "high", "thinkingDialect": "anthropic_effort" },
  { "modelId": "claude-opus-4-7", "name": "Claude Opus 4.7", "contextWindow": 1000000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","xhigh","max"], "defaultThinkingEffort": "high", "thinkingDialect": "anthropic_effort" },
  { "modelId": "claude-opus-4-6", "name": "Claude Opus 4.6", "contextWindow": 1000000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","max"], "defaultThinkingEffort": "high", "thinkingDialect": "anthropic_effort" },
  { "modelId": "claude-sonnet-5", "name": "Claude Sonnet 5", "contextWindow": 1000000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","xhigh","max"], "defaultThinkingEffort": "high", "thinkingDialect": "anthropic_effort" },
  { "modelId": "claude-sonnet-4-6", "name": "Claude Sonnet 4.6", "contextWindow": 1000000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","max"], "defaultThinkingEffort": "high", "thinkingDialect": "anthropic_effort" },
  { "modelId": "claude-opus-4-5", "name": "Claude Opus 4.5", "contextWindow": 200000, "maxTokens": 64000, "thinkingEfforts": ["none","low","medium","high"], "defaultThinkingEffort": "high", "thinkingDialect": "anthropic_budget" },
  { "modelId": "claude-sonnet-4-5", "name": "Claude Sonnet 4.5", "contextWindow": 200000, "maxTokens": 64000, "thinkingEfforts": ["none"], "thinkingDialect": "anthropic_budget" },
  { "modelId": "claude-haiku-4-5", "name": "Claude Haiku 4.5", "contextWindow": 200000, "maxTokens": 64000, "thinkingEfforts": ["none"], "thinkingDialect": "anthropic_budget" },

  { "modelId": "gpt-5.6", "name": "GPT-5.6", "contextWindow": 1050000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","xhigh","max"], "defaultThinkingEffort": "medium", "thinkingDialect": "openai_effort" },
  { "modelId": "gpt-5.6-sol", "name": "GPT-5.6 Sol", "contextWindow": 1050000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","xhigh","max"], "defaultThinkingEffort": "medium", "thinkingDialect": "openai_effort" },
  { "modelId": "gpt-5.6-terra", "name": "GPT-5.6 Terra", "contextWindow": 1050000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","xhigh","max"], "defaultThinkingEffort": "medium", "thinkingDialect": "openai_effort" },
  { "modelId": "gpt-5.6-luna", "name": "GPT-5.6 Luna", "contextWindow": 1050000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","xhigh","max"], "defaultThinkingEffort": "medium", "thinkingDialect": "openai_effort" },
  { "modelId": "gpt-5.5", "name": "GPT-5.5", "contextWindow": 1050000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","xhigh"], "defaultThinkingEffort": "medium", "thinkingDialect": "openai_effort" },
  { "modelId": "gpt-5.4", "name": "GPT-5.4", "contextWindow": 1050000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","xhigh"], "defaultThinkingEffort": "none", "thinkingDialect": "openai_effort" },
  { "modelId": "gpt-5.4-mini", "name": "GPT-5.4 mini", "contextWindow": 400000, "maxTokens": 128000, "thinkingEfforts": ["none","low","medium","high","xhigh"], "defaultThinkingEffort": "none", "thinkingDialect": "openai_effort" },
  { "modelId": "o3", "name": "o3", "contextWindow": 200000, "maxTokens": 100000, "thinkingEfforts": ["low","medium","high"], "defaultThinkingEffort": "medium", "thinkingDialect": "openai_effort" },
  { "modelId": "o4-mini", "name": "o4-mini", "contextWindow": 200000, "maxTokens": 100000, "thinkingEfforts": ["low","medium","high"], "defaultThinkingEffort": "medium", "thinkingDialect": "openai_effort" },
  { "modelId": "gpt-4.1", "name": "GPT-4.1", "contextWindow": 1000000, "maxTokens": 32768, "thinkingDialect": "none" },

  { "modelId": "k3", "name": "Kimi K3 (Kimi Code)", "contextWindow": 1000000, "maxTokens": 128000, "thinkingDialect": "none" },
  { "modelId": "k3-256k", "name": "Kimi K3 256k (Kimi Code)", "contextWindow": 256000, "maxTokens": 128000, "thinkingDialect": "none" },
  { "modelId": "kimi-k3", "name": "Kimi K3 (platform)", "contextWindow": 1000000, "maxTokens": 131072, "thinkingEfforts": ["low","high","max"], "thinkingDialect": "openai_effort" },
  { "modelId": "kimi-for-coding", "name": "Kimi K2.7 Code (Kimi Code)", "contextWindow": 256000, "maxTokens": 32768, "thinkingDialect": "none" },
  { "modelId": "kimi-for-coding-highspeed", "name": "Kimi K2.7 Code HighSpeed (Kimi Code)", "contextWindow": 256000, "maxTokens": 32768, "thinkingDialect": "none" },
  { "modelId": "kimi-k2.7-code", "name": "Kimi K2.7 Code (platform)", "contextWindow": 256000, "maxTokens": 32768, "thinkingDialect": "kimi_thinking" },
  { "modelId": "kimi-k2.6", "name": "Kimi K2.6", "contextWindow": 256000, "maxTokens": 32768, "thinkingEfforts": ["none"], "thinkingDialect": "kimi_thinking" },

  { "modelId": "glm-5.2", "name": "GLM-5.2", "contextWindow": 1000000, "maxTokens": 128000, "thinkingEfforts": ["none","minimal","low","medium","high","xhigh","max"], "defaultThinkingEffort": "max", "thinkingDialect": "openai_effort" },
  { "modelId": "glm-5.1", "name": "GLM-5.1", "contextWindow": 200000, "maxTokens": 128000, "thinkingEfforts": ["none"], "thinkingDialect": "zai_thinking" },
  { "modelId": "glm-5", "name": "GLM-5", "contextWindow": 200000, "maxTokens": 128000, "thinkingEfforts": ["none"], "thinkingDialect": "zai_thinking" },
  { "modelId": "glm-5-turbo", "name": "GLM-5-Turbo", "contextWindow": 200000, "maxTokens": 128000, "thinkingEfforts": ["none"], "thinkingDialect": "zai_thinking" },
  { "modelId": "glm-4.7", "name": "GLM-4.7", "contextWindow": 200000, "maxTokens": 128000, "thinkingEfforts": ["none"], "thinkingDialect": "zai_thinking" },
  { "modelId": "glm-4.6", "name": "GLM-4.6", "contextWindow": 200000, "maxTokens": 128000, "thinkingEfforts": ["none"], "thinkingDialect": "zai_thinking" },

  { "modelId": "deepseek-chat", "name": "DeepSeek Chat", "contextWindow": 128000, "maxTokens": 8192, "thinkingDialect": "none" }
]
```

`k3` and `k3-256k` carry dialect `none` deliberately: on the Anthropic-compatible Kimi Code endpoint, `reasoning_effort` is accepted and **silently ignored** (verified empirically 2026-07-27), and disabling thinking is documented to route the request to K2.6 — a different model, with no response-level signal. `kimi-k3` (the platform id, OpenAI wire) does honor effort.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p horsie-server`
Expected: PASS.

- [ ] **Step 6: Verify the correction migration**

```bash
python3 - <<'PY'
import sqlite3, os
db="/tmp/mig12-check.db"
os.path.exists(db) and os.remove(db)
c=sqlite3.connect(db)
c.executescript(open("server/migrations/0008_model_cards.sql").read())
c.executescript("""
INSERT INTO model_cards (model_id,name,context_window,max_tokens) VALUES
 ('claude-sonnet-4-6','Claude Sonnet 4.6',200000,16384),
 ('claude-haiku-4-5','Claude Haiku 4.5',200000,8192),
 ('o1','o1',200000,32768),
 ('gpt-4o','GPT-4o',128000,16384);
""")
c.executescript(open("server/migrations/0011_thinking_efforts.sql").read())
c.executescript(open("server/migrations/0012_model_card_corrections.sql").read())
c.commit()
print(list(c.execute("SELECT model_id, context_window, max_tokens FROM model_cards ORDER BY model_id")))
PY
```

Expected: sonnet-4-6 at 1000000/128000, haiku-4-5 at 200000/64000, and `o1`/`gpt-4o` gone.

- [ ] **Step 7: Commit**

```bash
git add server/migrations/0012_model_card_corrections.sql server/src/config/model_cards_seed.json server/src/config/model_cards.rs
git commit -m "feat(model-cards): current catalog with thinking efforts and dialects"
```

---

### Task 2: Settings model form

**Files:**
- Modify: `clients/web/src/pages/settings/ModelsSettings.tsx`

**Interfaces:**
- Consumes: `ModelView`/`ModelInput` thinking fields (Plan C), `ModelCard` thinking fields (Task 1).
- Produces: an editable thinking section on each model row, prefilled from the matching card.

- [ ] **Step 1: Extend the draft type**

```tsx
type ModelDraft = {
  alias: string;
  provider: string;
  modelId: string;
  maxTokens: string;
  contextWindow: string;
  thinkingEfforts: string[];
  thinkingEffort: string; // "" = no default
  thinkingDialect: string; // "" = no thinking control
};
```

Extend `toModelDrafts` with `thinkingEfforts: m.thinkingEfforts ?? []`, `thinkingEffort: m.thinkingEffort ?? ""`, `thinkingDialect: m.thinkingDialect ?? ""`, and the "add model" initialiser with `thinkingEfforts: []`, `thinkingEffort: ""`, `thinkingDialect: ""`.

- [ ] **Step 2: Send them on save**

In the `modelInputs` mapping:

```tsx
      thinkingEfforts: m.thinkingEfforts.length ? m.thinkingEfforts : undefined,
      thinkingEffort: m.thinkingEffort || undefined,
      thinkingDialect: m.thinkingDialect || undefined,
```

- [ ] **Step 3: Render the controls**

Add to the model row, below the existing fields:

```tsx
const EFFORTS = ["none", "minimal", "low", "medium", "high", "xhigh", "max"] as const;
const DIALECTS = [
  "", "anthropic_effort", "anthropic_always_on", "anthropic_budget",
  "openai_effort", "zai_thinking", "kimi_thinking", "none",
] as const;
```

```tsx
<div className="col-span-2 border-t pt-3">
  <RowLabel>Thinking</RowLabel>
  <div className="flex flex-wrap gap-3">
    {EFFORTS.map((e) => (
      <label key={e} className="flex items-center gap-1 text-sm">
        <input
          type="checkbox"
          checked={draft.thinkingEfforts.includes(e)}
          onChange={(ev) => {
            const next = ev.target.checked
              ? [...draft.thinkingEfforts, e]
              : draft.thinkingEfforts.filter((x) => x !== e);
            const ordered = EFFORTS.filter((x) => next.includes(x));
            set({
              thinkingEfforts: [...ordered],
              // drop a default that is no longer offered
              thinkingEffort: ordered.includes(draft.thinkingEffort as (typeof EFFORTS)[number])
                ? draft.thinkingEffort
                : "",
            });
          }}
        />
        {e}
      </label>
    ))}
  </div>
  <div className="mt-2 grid grid-cols-2 gap-3">
    <label className="block">
      <RowLabel>Default effort</RowLabel>
      <select
        className="input font-mono"
        value={draft.thinkingEffort}
        onChange={(ev) => set({ thinkingEffort: ev.target.value })}
      >
        <option value="">(none)</option>
        {draft.thinkingEfforts.map((e) => (
          <option key={e} value={e}>{e}</option>
        ))}
      </select>
    </label>
    <label className="block">
      <RowLabel>Dialect</RowLabel>
      <select
        className="input font-mono"
        value={draft.thinkingDialect}
        onChange={(ev) => set({ thinkingDialect: ev.target.value })}
      >
        {DIALECTS.map((d) => (
          <option key={d} value={d}>{d === "" ? "(none)" : d}</option>
        ))}
      </select>
    </label>
  </div>
</div>
```

- [ ] **Step 4: Prefill from the card**

Wherever the form already prefills `contextWindow`/`maxTokens` from a selected model card, also set `thinkingEfforts`, `thinkingEffort` and `thinkingDialect` from the card's `thinkingEfforts`, `defaultThinkingEffort` and `thinkingDialect` (defaulting to `[]`/`""`). Find the existing prefill handler by searching for `contextWindow` assignments in the card-selection path and mirror it.

- [ ] **Step 5: Typecheck**

Run: `cd clients/web && npx tsc -b`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add clients/web/src/pages/settings/ModelsSettings.tsx
git commit -m "feat(web): edit per-model thinking efforts in settings"
```

---

### Task 3: Session config bar control

**Files:**
- Modify: `clients/web/src/hooks/useSessionDraft.ts:22-41`
- Modify: `clients/web/src/components/SessionConfigBar.tsx`

**Interfaces:**
- Consumes: `AgentSettings.thinkingEffort` (Plan C Task 4), `ModelView.thinkingEfforts`/`thinkingEffort`.
- Produces: a session-creation thinking control.

- [ ] **Step 1: Extend the draft**

Add `thinkingEffort: string` (`""` = use the model's default) to `SessionDraft` and its initial value, and in `buildRequest` send `thinkingEffort: draft.thinkingEffort || undefined` inside the `agent` object.

- [ ] **Step 2: Render the control, reacting to model changes**

In `SessionConfigBar.tsx`, derive the menu from the selected model and render nothing when it is empty:

```tsx
const model = models.find((m) => m.alias === draft.model);
const efforts = model?.thinkingEfforts ?? [];

// A persisted draft can name an effort the model no longer offers.
useEffect(() => {
  if (draft.thinkingEffort && !efforts.includes(draft.thinkingEffort)) {
    onChange({ ...draft, thinkingEffort: "" });
  }
}, [draft.model, efforts.join(","), draft.thinkingEffort]);

{efforts.length > 0 && (
  <label className="flex items-center gap-1 text-sm">
    Thinking
    <select
      className="input"
      value={draft.thinkingEffort}
      onChange={(e) => onChange({ ...draft, thinkingEffort: e.target.value })}
    >
      <option value="">
        {model?.thinkingEffort ? `default (${model.thinkingEffort})` : "default"}
      </option>
      {efforts.map((e) => (
        <option key={e} value={e}>{e}</option>
      ))}
    </select>
  </label>
)}
```

Match the surrounding control markup in that file rather than introducing new class conventions — read the adjacent model `<select>` and mirror it.

- [ ] **Step 3: Typecheck and build**

Run: `cd clients/web && npx tsc -b && npm run build`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add clients/web/src/hooks/useSessionDraft.ts clients/web/src/components/SessionConfigBar.tsx
git commit -m "feat(web): choose thinking effort when creating a session"
```

---

## Verification

- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace --all-targets` clean.
- [ ] `cd clients/web && npx tsc -b && npm run build` clean.
- [ ] Migrations `0011` and `0012` apply in order to a DB seeded with the original eight cards.
- [ ] `clients/web/package-lock.json` is not staged.

## Review notes — unverified seed values

Flag these in the PR body rather than treating them as settled:

- `gpt-5.4-nano` is **omitted** — its context window and output cap could not be confirmed from primary docs.
- OpenAI does not publish per-model effort lists; the GPT-5.x rows come from the reasoning guide and secondary sources.
- z.ai flash/air variants and the GLM-4.5 family are omitted for the same reason (unconfirmed context windows).
- `kimi-k2.5` is omitted — no official pricing or limits page exists.
- Kimi Code documents k3's default effort as `high` while the platform docs say `max`; both `k3` rows therefore carry no default.
