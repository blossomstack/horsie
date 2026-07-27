# Thinking Signature Bloat Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop shipping and storing thinking-block signatures that no client reads, closing horsie#51.

**Architecture:** Two independent layers. A wire-boundary redactor strips `signature` from every `Message` leaving the server (HTTP history + SSE), which fixes sessions whose journals already contain signatures. Separately, `AnthropicProvider` gains an opt-in `keep_thinking_signature` flag so signatures are never captured at all for endpoints that don't validate them — the default — while genuine Anthropic deployments opt back in.

**Tech Stack:** Rust (axum, sqlx/SQLite, tokio), fluorite-codegen wire models, React/TypeScript web client.

## Global Constraints

- Worktree: `/Users/xiaoguang/works/repos/bloomstack/october/horsie/.horsie/worktrees/thinking-config`, branch `feat/thinking-efforts`.
- Canonical config values stay `String`/scalar, never fluorite enums — PascalCase-ing an existing string column breaks the live DB (`providers.kind` precedent).
- New DB columns must be `ALTER TABLE ... ADD COLUMN` with a default, so migration against a populated homelab DB is non-destructive.
- Default for `keep_thinking_signature` is **off (0)**. Empirically verified 2026-07-27 against `https://api.kimi.com/coding/`: omitted, empty, altered, and wholly removed signatures were all accepted with 200, including inside tool-use loops.
- Never list Claude as author/co-author on commits. Commit subjects short, no body unless the diff hides context.
- Known limitation carried by this plan: `providers/anthropic/src/lib.rs:231` replays `signature.clone().unwrap_or_default()`, and async-llm's `Thinking.signature` is a plain `String` (`async-llm/src/types.rs:58-64`), so a stripped signature serializes as `"signature": ""` rather than being omitted. This is proven-safe for flag-off providers and never arises for flag-on ones. Making it `Option<String>` belongs to the async-llm plan (Plan B).

---

### Task 1: Strip thinking signatures at the wire boundary

Fixes existing sessions immediately — no schema change, no provider change.

**Files:**
- Create: `server/src/wire_redact.rs`
- Modify: `server/src/lib.rs:1-13` (add module declaration)
- Modify: `server/src/http/handlers.rs:231-247` (`to_wire_history`)
- Modify: `server/src/sessions/events.rs:130-135` (`wire_event`)

**Interfaces:**
- Consumes: `horsie_models::agent::{ContentPart, Message, ThinkingPart}`
- Produces: `crate::wire_redact::strip_thinking_signatures(&mut [Message])` and `crate::wire_redact::strip_message_signature(&mut Message)`

- [ ] **Step 1: Write the failing test**

Create `server/src/wire_redact.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use horsie_models::agent::{ContentPart, Message, Role, TextPart, ThinkingPart};

    fn assistant_with_thinking(signature: Option<&str>) -> Message {
        Message {
            id: "m1".into(),
            role: Role::Assistant,
            parts: vec![
                ContentPart::Thinking(ThinkingPart {
                    text: "step by step".into(),
                    signature: signature.map(Into::into),
                }),
                ContentPart::Text(TextPart {
                    text: "the answer".into(),
                }),
            ],
        }
    }

    #[test]
    fn strips_signature_and_keeps_thinking_text() {
        let mut msgs = vec![assistant_with_thinking(Some("opaque-blob"))];
        strip_thinking_signatures(&mut msgs);
        match &msgs[0].parts[0] {
            ContentPart::Thinking(th) => {
                assert_eq!(th.signature, None);
                assert_eq!(th.text, "step by step");
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn leaves_non_thinking_parts_untouched() {
        let mut msgs = vec![assistant_with_thinking(Some("opaque-blob"))];
        strip_thinking_signatures(&mut msgs);
        match &msgs[0].parts[1] {
            ContentPart::Text(t) => assert_eq!(t.text, "the answer"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn tolerates_already_absent_signature() {
        let mut msgs = vec![assistant_with_thinking(None)];
        strip_thinking_signatures(&mut msgs);
        match &msgs[0].parts[0] {
            ContentPart::Thinking(th) => assert_eq!(th.signature, None),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn single_message_variant_strips() {
        let mut msg = assistant_with_thinking(Some("opaque-blob"));
        strip_message_signature(&mut msg);
        match &msg.parts[0] {
            ContentPart::Thinking(th) => assert_eq!(th.signature, None),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }
}
```

Add the module declaration to `server/src/lib.rs`, keeping the list alphabetical — insert after `pub mod velos;` is wrong; it belongs after `pub mod vendor;` alphabetically but the existing list is already alphabetical, so add `mod wire_redact;` as the final line:

```rust
pub mod vendor;
mod wire_redact;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p horsie-server wire_redact`
Expected: FAIL to compile — `cannot find function 'strip_thinking_signatures' in this scope`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `server/src/wire_redact.rs`, above the test module:

```rust
//! Wire-boundary redaction: fields that exist only for provider replay and are
//! meaningless to API clients.

use horsie_models::agent::{ContentPart, Message};

/// Drop thinking-block signatures from messages on their way to a client.
///
/// Signatures are opaque provider-replay artifacts — 4-13 KB each, and 37-46%
/// of a typical history response — that no client reads: the web transcript
/// renders `text` only (`clients/web/src/components/ThinkingBlock.tsx`). They
/// stay in the agent's in-memory state and journal, where provider replay needs
/// them; this strips only the copies handed to HTTP and SSE clients.
pub fn strip_thinking_signatures(messages: &mut [Message]) {
    for message in messages.iter_mut() {
        strip_message_signature(message);
    }
}

/// Single-message variant, for the SSE path.
pub fn strip_message_signature(message: &mut Message) {
    for part in message.parts.iter_mut() {
        if let ContentPart::Thinking(thinking) = part {
            thinking.signature = None;
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p horsie-server wire_redact`
Expected: PASS, 4 tests.

- [ ] **Step 5: Wire into the history API**

In `server/src/http/handlers.rs`, replace the body of `to_wire_history` (line 231):

```rust
fn to_wire_history(page: AgentHistoryPage) -> HistoryPage {
    let mut messages = page.messages;
    crate::wire_redact::strip_thinking_signatures(&mut messages);
    HistoryPage {
        messages,
        has_more: page.has_more,
        tasks: page.tasks.map(|tasks| {
            tasks
                .into_iter()
                .map(|t| TaskItem {
                    id: t.id,
                    content: t.content,
                    status: wire_task_status(t.status),
                })
                .collect()
        }),
        usage: page.usage.map(to_wire_usage),
    }
}
```

- [ ] **Step 6: Wire into the SSE path**

In `server/src/sessions/events.rs`, replace the first match arm of `wire_event` (line 132):

```rust
        AgentDomainEvent::InputMessage { mut message }
        | AgentDomainEvent::MessageComplete { mut message } => {
            crate::wire_redact::strip_message_signature(&mut message);
            Some(SessionEvent::Message(MessageEvent { message }))
        }
```

- [ ] **Step 7: Add an SSE regression test**

Append to the existing `mod tests` in `server/src/sessions/events.rs`:

```rust
    #[test]
    fn wire_event_strips_thinking_signature() {
        use horsie_models::agent::{ContentPart, Message, Role, ThinkingPart};

        let message = Message {
            id: "m1".into(),
            role: Role::Assistant,
            parts: vec![ContentPart::Thinking(ThinkingPart {
                text: "reasoning".into(),
                signature: Some("opaque-blob".into()),
            })],
        };
        let wired = wire_event(AgentDomainEvent::MessageComplete { message })
            .expect("MessageComplete should surface");
        match wired {
            SessionEvent::Message(m) => match &m.message.parts[0] {
                ContentPart::Thinking(th) => {
                    assert_eq!(th.signature, None);
                    assert_eq!(th.text, "reasoning");
                }
                other => panic!("expected Thinking, got {other:?}"),
            },
            other => panic!("expected Message, got {other:?}"),
        }
    }
```

- [ ] **Step 8: Run the full server test suite**

Run: `cargo test -p horsie-server`
Expected: PASS, including the new `wire_event_strips_thinking_signature`.

- [ ] **Step 9: Commit**

```bash
git add server/src/wire_redact.rs server/src/lib.rs server/src/http/handlers.rs server/src/sessions/events.rs
git commit -m "fix: strip thinking signatures from history and SSE responses"
```

---

### Task 2: Gate signature capture in AnthropicProvider

**Files:**
- Modify: `providers/anthropic/src/lib.rs:76-85` (struct fields)
- Modify: `providers/anthropic/src/lib.rs:120-150` (both constructors)
- Modify: `providers/anthropic/src/lib.rs:178-181` (add builder next to `with_thinking`)
- Modify: `providers/anthropic/src/lib.rs:553-563` (ingest assembly)
- Test: `providers/anthropic/src/lib.rs:594` (existing inline `mod tests`)

**Interfaces:**
- Consumes: nothing from Task 1 — independent.
- Produces: `AnthropicProvider::with_keep_thinking_signature(bool) -> Self` (consumed by Task 3), and private `AnthropicProvider::thinking_part(&str, &str, bool) -> Option<ContentPart>`.

- [ ] **Step 1: Write the failing test**

Append to the existing `mod tests` in `providers/anthropic/src/lib.rs`:

```rust
    #[test]
    fn thinking_part_keeps_signature_when_enabled() {
        let part = AnthropicProvider::thinking_part("reasoning", "sig-blob", true)
            .expect("non-empty thinking yields a part");
        match part {
            ContentPart::Thinking(th) => {
                assert_eq!(th.text, "reasoning");
                assert_eq!(th.signature.as_deref(), Some("sig-blob"));
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn thinking_part_drops_signature_when_disabled() {
        let part = AnthropicProvider::thinking_part("reasoning", "sig-blob", false)
            .expect("non-empty thinking yields a part");
        match part {
            ContentPart::Thinking(th) => {
                assert_eq!(th.text, "reasoning");
                assert_eq!(th.signature, None);
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn thinking_part_normalizes_empty_signature_to_none() {
        let part = AnthropicProvider::thinking_part("reasoning", "", true)
            .expect("non-empty thinking yields a part");
        match part {
            ContentPart::Thinking(th) => assert_eq!(th.signature, None),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn thinking_part_skips_empty_thinking() {
        assert!(AnthropicProvider::thinking_part("", "sig-blob", true).is_none());
    }

    #[test]
    fn keep_thinking_signature_defaults_off() {
        let p = AnthropicProvider::new().expect("provider builds without a key");
        assert!(!p.keep_thinking_signature);
    }

    #[test]
    fn with_keep_thinking_signature_enables_retention() {
        let p = AnthropicProvider::new()
            .expect("provider builds without a key")
            .with_keep_thinking_signature(true);
        assert!(p.keep_thinking_signature);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p horsie-provider-anthropic thinking_part`
Expected: FAIL to compile — `no function or associated item named 'thinking_part'`.

- [ ] **Step 3: Add the struct field and constructor defaults**

In `providers/anthropic/src/lib.rs`, add the field to `pub struct AnthropicProvider` after `thinking_budget`:

```rust
    thinking_budget: Option<u32>,
    /// Retain provider thinking-block signatures captured from this endpoint.
    keep_thinking_signature: bool,
```

Add `keep_thinking_signature: false,` immediately after `thinking_budget: None,` in **both** `new()` and `with_api_key()`.

- [ ] **Step 4: Add the builder**

Insert after the existing `with_thinking` method (line 181):

```rust
    /// Retain provider thinking-block signatures on captured thinking parts.
    ///
    /// Genuine Anthropic validates these on replay, so real Anthropic providers
    /// must enable this. Anthropic-compatible endpoints do not: verified
    /// 2026-07-27 against `https://api.kimi.com/coding/` (model `k3`), where
    /// omitted, empty, altered, and wholly removed signatures were all accepted
    /// with 200 — including inside tool-use loops. Default off, because the
    /// blobs run 4-13 KB each and no client reads them.
    pub fn with_keep_thinking_signature(mut self, keep: bool) -> Self {
        self.keep_thinking_signature = keep;
        self
    }
```

- [ ] **Step 5: Add the testable assembly helper**

Insert next to the other private helpers, immediately above `fn parts_to_api_content` (line 209):

```rust
    /// Build the thinking part for one assembled block, honoring the
    /// signature-retention policy. `None` when the block carried no text.
    fn thinking_part(text: &str, signature: &str, keep_signature: bool) -> Option<ContentPart> {
        if text.is_empty() {
            return None;
        }
        Some(ContentPart::Thinking(ThinkingPart {
            text: text.to_string(),
            signature: if keep_signature && !signature.is_empty() {
                Some(signature.to_string())
            } else {
                None
            },
        }))
    }
```

- [ ] **Step 6: Route the ingest path through the helper**

Replace the `else if` branch at line 553:

```rust
            } else if let Some((thinking, signature)) = thinking_blocks.get(&idx)
                && let Some(part) =
                    Self::thinking_part(thinking, signature, self.keep_thinking_signature)
            {
                parts.push(part);
            }
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p horsie-provider-anthropic`
Expected: PASS, including the 6 new tests.

- [ ] **Step 8: Commit**

```bash
git add providers/anthropic/src/lib.rs
git commit -m "feat(anthropic): opt-in thinking signature retention, default off"
```

---

### Task 3: Persist and expose the provider flag

**Files:**
- Create: `server/migrations/0010_provider_thinking_signature.sql`
- Modify: `server/src/config/store.rs:540-545` (`ProviderRow`)
- Modify: `server/src/config/store.rs:365-374` (provider INSERT)
- Modify: `server/src/config/store.rs:966-980` (provider SELECT)
- Modify: `server/src/config/store.rs:662-682` (`build_anthropic`) and its call site at `:637-651`
- Modify: `models/fluorite/settings.fl:8-18` (`ProviderView`, and `ProviderInput` in the same file)
- Modify: `clients/web/src/pages/settings/ModelsSettings.tsx` (provider form)

**Interfaces:**
- Consumes: `AnthropicProvider::with_keep_thinking_signature(bool)` from Task 2.
- Produces: `ProviderRow.keep_thinking_signature: bool`; wire field `keepThinkingSignature: boolean` on `ProviderView`/`ProviderInput`.

- [ ] **Step 1: Write the migration**

Create `server/migrations/0010_provider_thinking_signature.sql`:

```sql
-- Whether to retain thinking-block signatures captured from this provider.
-- Genuine Anthropic validates signatures when thinking blocks are replayed;
-- Anthropic-compatible endpoints (Kimi, z.ai) do not. The signatures are opaque
-- 4-13 KB blobs that no client reads, so the default is off and real Anthropic
-- deployments opt back in.
ALTER TABLE providers ADD COLUMN keep_thinking_signature INTEGER NOT NULL DEFAULT 0;
```

- [ ] **Step 2: Write the failing round-trip test**

First extend the two existing `ProviderInput` builders in that `mod tests` — `provider(name, key)` (around line 29 of the test module) and `provider_kind(name, kind)` — each gains `keep_thinking_signature: false,` as a final field.

Then append the test, following the `open(dir.path())` harness the neighbouring tests use (`SettingsUpdate` spells all four fields explicitly; there is no `Default` impl to spread):

```rust
    #[tokio::test]
    async fn keep_thinking_signature_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;

        // Defaults off for a fresh provider.
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("kimi", Some("sk-test"))]),
                models: Some(vec![model("m", "kimi")]),
                vendors: None,
                default_vendor: None,
            })
            .await
            .expect("update succeeds");
        assert!(!view.providers[0].keep_thinking_signature);

        // Opting in persists and reads back.
        let mut p = provider("real-anthropic", Some("sk-test"));
        p.keep_thinking_signature = true;
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![p]),
                models: Some(vec![model("m", "real-anthropic")]),
                vendors: None,
                default_vendor: None,
            })
            .await
            .expect("update succeeds");
        assert!(view.providers[0].keep_thinking_signature);
    }
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p horsie-server keep_thinking_signature_round_trips`
Expected: FAIL to compile — `ProviderInput` has no field `keep_thinking_signature`.

- [ ] **Step 4: Add the wire field**

In `models/fluorite/settings.fl`, add to `ProviderView` after `has_inline_key`:

```
    /// Retain thinking-block signatures from this provider. Required for
    /// genuine Anthropic (it validates them on replay); off for
    /// Anthropic-compatible endpoints, which do not.
    keep_thinking_signature: bool,
```

Add the identical field to `ProviderInput` in the same file.

- [ ] **Step 5: Thread it through the store**

`ProviderRow` (line 540) gains `keep_thinking_signature: bool,`.

The INSERT (line 365) becomes:

```rust
                sqlx::query(
                    "INSERT INTO providers (name, kind, base_url, api_key, keep_thinking_signature) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(name)
                .bind(&p.kind)
                .bind(trimmed(&p.base_url))
                .bind(api_key)
                .bind(i64::from(p.keep_thinking_signature))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
```

The SELECT (line 968) becomes:

```rust
    let rows = sqlx::query(
        "SELECT name, kind, base_url, api_key, keep_thinking_signature FROM providers ORDER BY name",
    )
    .fetch_all(ex)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(ProviderRow {
            name: r.try_get("name")?,
            kind: r.try_get("kind")?,
            base_url: r.try_get("base_url")?,
            api_key: r.try_get("api_key")?,
            keep_thinking_signature: r.try_get::<i64, _>("keep_thinking_signature")? != 0,
        });
    }
```

Then populate the new field at every `ProviderView` construction site. Find them with:

```bash
grep -n "ProviderView {" server/src/config/store.rs
```

Each such literal gains `keep_thinking_signature: row.keep_thinking_signature,` (adjust the binding name to whatever that site calls its `ProviderRow`).

- [ ] **Step 6: Pass the flag to the provider**

`build_anthropic` (line 662) gains a parameter and applies it:

```rust
fn build_anthropic(
    base_url: Option<&str>,
    api_key: Option<&str>,
    model_id: &str,
    max_tokens: Option<u32>,
    keep_thinking_signature: bool,
) -> Result<Arc<dyn LlmProvider>, String> {
    let key: Option<Secret> = match api_key {
        Some(k) if !k.is_empty() => Some(Secret::from(k)),
        Some(_) => return Err("inline api_key is empty".into()),
        None => None,
    };
    let mut p = match key {
        Some(k) => AnthropicProvider::with_api_key(k).map_err(|e| e.to_string())?,
        None => AnthropicProvider::new().map_err(|e| e.to_string())?,
    };
    p = p
        .with_model(model_id)
        .with_max_tokens(max_tokens)
        .with_keep_thinking_signature(keep_thinking_signature);
    if let Some(u) = base_url {
        p = p.with_base_url(u);
    }
    Ok(Arc::new(p))
}
```

Update the `"anthropic" =>` dispatch arm (line 637) to pass `provider_row.keep_thinking_signature`.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p horsie-server`
Expected: PASS, including `keep_thinking_signature_round_trips`.

- [ ] **Step 8: Add the web settings control**

In `clients/web/src/pages/settings/ModelsSettings.tsx`, add a checkbox to the provider row, rendered only when `kind === "anthropic"`:

```tsx
{provider.kind === "anthropic" && (
  <label className="flex items-center gap-2 text-sm">
    <input
      type="checkbox"
      checked={provider.keepThinkingSignature}
      onChange={(e) =>
        updateProvider(index, { ...provider, keepThinkingSignature: e.target.checked })
      }
    />
    Keep thinking signatures
    <span className="text-xs opacity-70">
      Required for api.anthropic.com; leave off for Anthropic-compatible endpoints.
    </span>
  </label>
)}
```

Match the surrounding file's existing update-handler name and class conventions rather than introducing new ones — read the neighbouring `base_url` input and mirror it.

- [ ] **Step 9: Verify the client builds and typechecks**

Run: `cd clients/web && npm run build`
Expected: PASS — the regenerated `ProviderView`/`ProviderInput` types carry `keepThinkingSignature`.

- [ ] **Step 10: Verify the migration against a populated DB**

```bash
cp /path/to/a/copy/of/config.db /tmp/mig-check.db
sqlite3 /tmp/mig-check.db < server/migrations/0010_provider_thinking_signature.sql
sqlite3 /tmp/mig-check.db "SELECT name, kind, keep_thinking_signature FROM providers;"
```

Expected: every existing provider reports `0`, no error.

- [ ] **Step 11: Commit**

```bash
git add server/migrations/0010_provider_thinking_signature.sql server/src/config/store.rs models/fluorite/settings.fl clients/web/src/pages/settings/ModelsSettings.tsx
git commit -m "feat(settings): configure thinking signature retention per provider"
```

---

## Verification

- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `cd clients/web && npm run build && npm run lint` pass.
- [ ] Manual: fetch `/api/sessions/<id>/history` for an existing homelab session and confirm no `signature` key appears in the payload, while `thinking` text still renders in the transcript.
