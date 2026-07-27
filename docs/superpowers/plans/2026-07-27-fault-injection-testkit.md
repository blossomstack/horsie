# Fault-Injection Testkit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every failure at three seams — LLM wire, runtime transport, journal — expressible by a test, and land the findings those seams unlock as named, ignored, red tests.

**Architecture:** Each fault lives in the crate that owns the trait it corrupts, behind that crate's `test-util` feature. One shared `Script<T>` type in `agentcore::testkit` gives ordered, strictly-exhausting programmed outcomes to both `MockProvider` and `MockTransport`. No central fault enum, no new crate.

**Tech Stack:** Rust edition 2024, tokio, async-trait, axum (mock-llm), serde_json, tempfile.

## Global Constraints

- **Base:** worktree `/Users/xiaoguang/works/repos/bloomstack/october/horsie-testkit`, branch `feat/fault-injection-testkit`, off `origin/main` @ `ce06849`.
- **Fix nothing.** This plan adds capability and red tests only. If a task's test unexpectedly passes, stop and report — do not adjust the assertion to make it red.
- **Workspace lints** deny `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm` in non-test code. Code behind `test-util` **is** non-test code and must obey them. Test modules opt out with exactly this block:
  ```rust
  #[cfg(test)]
  #[allow(
      clippy::unwrap_used,
      clippy::expect_used,
      clippy::panic,
      clippy::wildcard_enum_match_arm
  )]
  mod tests {
  ```
- **Red-test convention:** `#[ignore = "red: #61 item N — <one-line invariant>"]`, and every red test body wrapped in `tokio::time::timeout` (several findings *are* hangs; an unbounded red test wedges CI).
- **Pre-PR checks:** `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `cargo test --workspace`.
- **Commits:** short imperative subject, no body unless the diff hides context. No AI attribution or co-author trailers.

## Deviations from the spec (deliberate, discovered while planning)

1. **No new `horsie-testkit` crate.** `runtime-client/Cargo.toml:10` already depends on `horsie-agentcore`, so `Script<T>` goes in `agentcore::testkit` and `runtime-client`'s `test-util` feature enables `horsie-agentcore/test-util`. `actor` never needs `Script`. The "copy into three crates" alternative the spec weighed against does not exist.
2. **Journal conformance uses plain async helper fns + two `mod` blocks, not `macro_rules!`.** Same outcome (per-backend, individually ignorable tests), no macro. The spec's concern — that `provider_conformance`'s inside-the-test loop cannot express a per-backend ignore — still drives the shape.
3. **`Script::then_repeating` needs a factory internally.** `Result<CompletionResponse, LlmError>` is *not* `Clone` (`LlmError::Network` holds a `Box<dyn Error>`), so the repeating value is stored as `Box<dyn Fn() -> T>`. `then_repeating(v)` is sugar over `then_repeating_with(move || v.clone())` for `T: Clone`.

## File Structure

**Created:**
- `agentcore/src/testkit/script.rs` — `Script<T>`, `ScriptExhausted`. No knowledge of providers.
- `actor/src/testkit.rs` — `FaultyJournal`, `write_corrupt_journal`. Journal faults only.
- `actor/tests/journal_conformance.rs` — the `Journal` contract, run against both backends.
- `actor/tests/journal_corruption.rs` — the item 13 red test.
- `runtime-client/src/testkit.rs` — `TransportOutcome`, `BlockHandle`, `TransportProbe`, and `MockTransport`'s fault modes.

Red tests for items 2 and 23 are **appended to `tests/tests/session_server_e2e.rs`** rather than given their own file: the harness helpers (`start_server`, `create_session`, `send_message`, `wait_status`) are module-private there, and a second copy would be a second harness to keep in sync.

**Modified:**
- `agentcore/src/testkit.rs` — becomes `testkit/mod.rs`; `MockProvider` rebuilt on `Script`, gains `requests()`.
- `agentcore/Cargo.toml`, `actor/Cargo.toml`, `runtime-client/Cargo.toml` — feature declarations.
- `runtime-client/src/transport.rs` — `MockTransport` moves its fault behaviour to `testkit.rs`; `lib.rs:7` stops exporting it unconditionally.
- `server/src/vendor/mock.rs` — `MockVendor::with_transport`, `disconnect_runtime_after`.
- `providers/mock-llm/src/server.rs` — `CutStream`, `CutToolCallStream`, `AbortBody`, `Delayed`; `Infallible` relaxed.
- `providers/mock-llm/src/openai.rs` — same four variants on the OpenAI wire.
- `tests/tests/provider_conformance.rs` — fault cases for items 1a, 1b, 5a, 6.
- `.github/workflows/ci.yml` — non-blocking red-catalogue job.

---

### Task 1: `Script<T>` — ordered outcomes that error on exhaustion

**Files:**
- Create: `agentcore/src/testkit/script.rs`
- Modify: `agentcore/src/testkit.rs` → move to `agentcore/src/testkit/mod.rs`, add `pub mod script; pub use script::{Script, ScriptExhausted};`

**Interfaces:**
- Consumes: nothing.
- Produces: `Script<T>` with `of(impl IntoIterator<Item = T>) -> Self`, `once(T) -> Self`, `labelled(&'static str) -> Self`, `then_repeating(T) -> Self where T: Clone + Send + Sync + 'static`, `then_repeating_with(impl Fn() -> T + Send + Sync + 'static) -> Self`, `next_step(&self) -> Result<T, ScriptExhausted>`, `taken(&self) -> usize`. `ScriptExhausted { label: &'static str, taken: usize }` implements `std::error::Error`.

- [ ] **Step 1: Move the testkit module to a directory**

```bash
cd /Users/xiaoguang/works/repos/bloomstack/october/horsie-testkit
mkdir -p agentcore/src/testkit
git mv agentcore/src/testkit.rs agentcore/src/testkit/mod.rs
```

- [ ] **Step 2: Write the failing test**

Create `agentcore/src/testkit/script.rs` containing only this test module (the implementation comes in step 4):

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    #[test]
    fn returns_steps_in_order() {
        let s = Script::of([1, 2, 3]);
        assert_eq!(s.next_step().unwrap(), 1);
        assert_eq!(s.next_step().unwrap(), 2);
        assert_eq!(s.next_step().unwrap(), 3);
    }

    #[test]
    fn errors_instead_of_cycling_when_exhausted() {
        let s = Script::of([1]).labelled("counter");
        assert_eq!(s.next_step().unwrap(), 1);
        let err = s.next_step().unwrap_err();
        assert_eq!(err.label, "counter");
        assert_eq!(err.taken, 1);
    }

    #[test]
    fn then_repeating_serves_the_steady_value_forever() {
        let s = Script::of([1, 2]).then_repeating(9);
        assert_eq!(s.next_step().unwrap(), 1);
        assert_eq!(s.next_step().unwrap(), 2);
        assert_eq!(s.next_step().unwrap(), 9);
        assert_eq!(s.next_step().unwrap(), 9);
    }

    #[test]
    fn then_repeating_with_supports_non_clone_values() {
        // The real motivator: `Result<_, LlmError>` is not Clone.
        let s: Script<Result<u8, String>> =
            Script::of([Ok(1)]).then_repeating_with(|| Err("boom".to_string()));
        assert_eq!(s.next_step().unwrap(), Ok(1));
        assert_eq!(s.next_step().unwrap(), Err("boom".to_string()));
        assert_eq!(s.next_step().unwrap(), Err("boom".to_string()));
    }

    #[test]
    fn taken_counts_consumed_steps() {
        let s = Script::of([1, 2, 3]);
        let _ = s.next_step();
        let _ = s.next_step();
        assert_eq!(s.taken(), 2);
    }
}
```

- [ ] **Step 3: Run it to make sure it fails**

Run: `cargo test -p horsie-agentcore --lib script`
Expected: FAIL — `cannot find type Script in this scope` (the module is not yet declared, so also expect an unresolved-module error until step 5).

- [ ] **Step 4: Write the implementation**

Prepend to `agentcore/src/testkit/script.rs`:

```rust
//! Ordered, strictly-exhausting programmed outcomes for test doubles.
//!
//! The point is the exhaustion behaviour: running past the end is an error, never
//! a wrap-around. A double that cycles turns "my test over-ran its script" into a
//! silent repeated response, which is exactly how iteration-count bugs hide.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// Returned when a [`Script`] is asked for a step it does not have.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("script '{label}' exhausted after {taken} step(s)")]
pub struct ScriptExhausted {
    pub label: &'static str,
    pub taken: usize,
}

/// An ordered list of programmed outcomes, consumed once.
pub struct Script<T> {
    label: &'static str,
    steps: Mutex<VecDeque<T>>,
    repeating: Option<Box<dyn Fn() -> T + Send + Sync>>,
    taken: AtomicUsize,
}

impl<T> Script<T> {
    /// A script that yields `steps` in order, then errors.
    pub fn of(steps: impl IntoIterator<Item = T>) -> Self {
        Self {
            label: "script",
            steps: Mutex::new(steps.into_iter().collect()),
            repeating: None,
            taken: AtomicUsize::new(0),
        }
    }

    /// A one-step script.
    pub fn once(step: T) -> Self {
        Self::of([step])
    }

    /// Name the script so `ScriptExhausted` says which one ran out.
    #[must_use]
    pub fn labelled(mut self, label: &'static str) -> Self {
        self.label = label;
        self
    }

    /// After the scripted steps, keep yielding values built by `f`. Opting into a
    /// steady state has to be said out loud — it is not the default.
    #[must_use]
    pub fn then_repeating_with(mut self, f: impl Fn() -> T + Send + Sync + 'static) -> Self {
        self.repeating = Some(Box::new(f));
        self
    }

    /// Take the next programmed outcome.
    pub fn next_step(&self) -> Result<T, ScriptExhausted> {
        let next = self
            .steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        match next {
            Some(step) => {
                self.taken.fetch_add(1, Ordering::Relaxed);
                Ok(step)
            }
            None => match &self.repeating {
                Some(f) => {
                    self.taken.fetch_add(1, Ordering::Relaxed);
                    Ok(f())
                }
                None => Err(ScriptExhausted {
                    label: self.label,
                    taken: self.taken.load(Ordering::Relaxed),
                }),
            },
        }
    }

    /// How many steps have been consumed.
    pub fn taken(&self) -> usize {
        self.taken.load(Ordering::Relaxed)
    }
}

impl<T: Clone + Send + Sync + 'static> Script<T> {
    /// Sugar over [`Script::then_repeating_with`] for cloneable values.
    #[must_use]
    pub fn then_repeating(self, steady: T) -> Self {
        self.then_repeating_with(move || steady.clone())
    }
}
```

- [ ] **Step 5: Declare the module**

At the top of `agentcore/src/testkit/mod.rs`, below the existing `//!` doc comment:

```rust
pub mod script;
pub use script::{Script, ScriptExhausted};
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p horsie-agentcore --lib script`
Expected: PASS, 5 tests.

- [ ] **Step 7: Lint and commit**

```bash
cargo clippy -p horsie-agentcore --all-targets --all-features -- -D warnings
cargo fmt
git add agentcore/src/testkit
git commit -m "testkit: add Script<T> with strict exhaustion"
```

---

### Task 2: `MockProvider` on `Script`, with request capture

**Files:**
- Modify: `agentcore/src/testkit/mod.rs:22-87` (the `MockProvider` struct and its `LlmProvider` impl)
- Test: same file, in its `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `Script<T>` from Task 1.
- Produces: `MockProvider::scripted(Script<Result<CompletionResponse, LlmError>>) -> Arc<Self>`, `MockProvider::failing(LlmError) -> Arc<Self>`, `calls(&self) -> usize`, `requests(&self) -> Vec<RequestSummary>`. `RequestSummary { message_count: usize, roles: Vec<Role>, tool_call_ids: Vec<String>, tool_result_ids: Vec<String> }` is `Debug + Clone + PartialEq`. Existing `text()` / `tool_then_text()` keep their signatures and behaviour.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `agentcore/src/testkit/mod.rs` (create the block if absent, using the standard allow header):

```rust
use crate::provider::{CompletionRequest, ToolChoice};
use crate::events::EventSink;
use horsie_models::agent::{Message, Role};

fn user_msg(id: &str, text: &str) -> Message {
    Message {
        id: id.to_string(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart { text: text.to_string() })],
    }
}

async fn call(provider: &MockProvider, messages: &[Message]) -> Result<CompletionResponse, LlmError> {
    let sink = CollectingEventSink::new();
    provider
        .complete(
            CompletionRequest {
                messages,
                system: None,
                tools: vec![],
                tool_choice: ToolChoice::Auto,
                max_tokens: None,
            },
            "msg-1",
            &sink as &dyn EventSink,
        )
        .await
}

#[tokio::test]
async fn scripted_provider_yields_then_errors_on_exhaustion() {
    let p = MockProvider::scripted(Script::of([
        Ok(CompletionResponse {
            parts: vec![ContentPart::Text(TextPart { text: "one".into() })],
            stop_reason: StopReason::EndTurn,
            usage: Usage::without_cache(1, 1),
        }),
        Err(LlmError::Overloaded),
    ]));
    let msgs = vec![user_msg("m1", "hi")];

    assert!(call(&p, &msgs).await.is_ok());
    assert!(matches!(call(&p, &msgs).await, Err(LlmError::Overloaded)));
    // Third call: the script is spent. A cycling double would silently repeat.
    assert!(matches!(
        call(&p, &msgs).await,
        Err(LlmError::ApiError { status: 500, .. })
    ));
}

#[tokio::test]
async fn failing_provider_always_errors() {
    let p = MockProvider::failing(LlmError::ApiError {
        status: 400,
        message: "context length exceeded".into(),
    });
    let msgs = vec![user_msg("m1", "hi")];
    for _ in 0..3 {
        assert!(matches!(
            call(&p, &msgs).await,
            Err(LlmError::ApiError { status: 400, .. })
        ));
    }
}

#[tokio::test]
async fn records_what_each_call_was_asked() {
    let p = MockProvider::text("ok");
    let first = vec![user_msg("m1", "hi")];
    let second = vec![
        user_msg("m1", "hi"),
        Message {
            id: "m2".into(),
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "call-1".into(),
                name: "echo".into(),
                input: json!({}),
            })],
        },
        Message {
            id: "m3".into(),
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResultPart {
                tool_call_id: "call-1".into(),
                output: "done".into(),
                is_error: false,
            })],
        },
    ];

    let _ = call(&p, &first).await;
    let _ = call(&p, &second).await;

    let seen = p.requests();
    assert_eq!(p.calls(), 2);
    assert_eq!(seen[0].message_count, 1);
    assert_eq!(seen[0].roles, vec![Role::User]);
    assert_eq!(seen[1].message_count, 3);
    assert_eq!(seen[1].tool_call_ids, vec!["call-1".to_string()]);
    assert_eq!(seen[1].tool_result_ids, vec!["call-1".to_string()]);
}

#[tokio::test]
async fn existing_constructors_still_repeat() {
    // `text()` is used by tests that call it more than once; it must not become
    // strict, or migrating the suite becomes a rewrite.
    let p = MockProvider::text("hello");
    let msgs = vec![user_msg("m1", "hi")];
    assert!(call(&p, &msgs).await.is_ok());
    assert!(call(&p, &msgs).await.is_ok());
}
```

Add to the test module's imports: `use horsie_models::agent::{ToolCallPart, ToolResultPart};` and `use crate::provider::StopReason;` as needed.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p horsie-agentcore --lib testkit`
Expected: FAIL — `no function or associated item named 'scripted' found`.

- [ ] **Step 3: Rewrite `MockProvider`**

Replace lines 22-87 of `agentcore/src/testkit/mod.rs` with:

```rust
/// A summary of one `CompletionRequest`, captured because the request itself is
/// borrowed and cannot be stored. Enough to assert *what the model was asked* —
/// which is the only way to catch a retry that rebuilds history from the wrong
/// place (#61 item 21).
#[derive(Debug, Clone, PartialEq)]
pub struct RequestSummary {
    pub message_count: usize,
    pub roles: Vec<horsie_models::agent::Role>,
    pub tool_call_ids: Vec<String>,
    pub tool_result_ids: Vec<String>,
}

impl RequestSummary {
    fn of(messages: &[horsie_models::agent::Message]) -> Self {
        let mut tool_call_ids = Vec::new();
        let mut tool_result_ids = Vec::new();
        for message in messages {
            for part in &message.parts {
                match part {
                    ContentPart::ToolCall(c) => tool_call_ids.push(c.id.clone()),
                    ContentPart::ToolResult(r) => tool_result_ids.push(r.tool_call_id.clone()),
                    ContentPart::Text(_) | ContentPart::Thinking(_) => {}
                }
            }
        }
        Self {
            message_count: messages.len(),
            roles: messages.iter().map(|m| m.role.clone()).collect(),
            tool_call_ids,
            tool_result_ids,
        }
    }
}

/// An `LlmProvider` that replays a [`Script`] of programmed outcomes and records
/// what it was asked.
pub struct MockProvider {
    script: Script<Result<CompletionResponse, LlmError>>,
    requests: Mutex<Vec<RequestSummary>>,
}

impl MockProvider {
    /// Replay `script`. When it runs out, every further call returns
    /// `LlmError::ApiError { status: 500 }` naming the exhausted script — a loud,
    /// attributable failure rather than a silent repeat.
    pub fn scripted(script: Script<Result<CompletionResponse, LlmError>>) -> Arc<Self> {
        Arc::new(Self {
            script,
            requests: Mutex::new(Vec::new()),
        })
    }

    /// Fail every call with `err`.
    pub fn failing(err: LlmError) -> Arc<Self> {
        let message = err.to_string();
        let status = match err {
            LlmError::ApiError { status, .. } => status,
            LlmError::RateLimit { .. } => 429,
            LlmError::Overloaded => 529,
            LlmError::Network(_) | LlmError::EventSink(_) => 502,
        };
        Self::scripted(
            Script::of([]).then_repeating_with(move || {
                Err(LlmError::ApiError {
                    status,
                    message: message.clone(),
                })
            }),
        )
    }

    /// A provider that answers `text` on every call.
    pub fn text(text: &str) -> Arc<Self> {
        let response = CompletionResponse {
            parts: vec![ContentPart::Text(TextPart {
                text: text.to_string(),
            })],
            stop_reason: StopReason::EndTurn,
            usage: Usage::without_cache(10, 5),
        };
        Self::scripted(Script::of([]).then_repeating_with(move || Ok(response.clone())))
    }

    /// One tool call, then `reply` on every later call.
    pub fn tool_then_text(tool_id: &str, tool_name: &str, input: Value, reply: &str) -> Arc<Self> {
        let first = CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: tool_id.to_string(),
                name: tool_name.to_string(),
                input,
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(20, 10),
        };
        let steady = CompletionResponse {
            parts: vec![ContentPart::Text(TextPart {
                text: reply.to_string(),
            })],
            stop_reason: StopReason::EndTurn,
            usage: Usage::without_cache(30, 8),
        };
        Self::scripted(
            Script::of([Ok(first)]).then_repeating_with(move || Ok(steady.clone())),
        )
    }

    /// How many completions have been requested.
    pub fn calls(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// A summary of every request, in order.
    pub fn requests(&self) -> Vec<RequestSummary> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn model_id(&self) -> &str {
        "mock-model"
    }

    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        _message_id: &str,
        _events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(RequestSummary::of(request.messages));
        match self.script.next_step() {
            Ok(outcome) => outcome,
            Err(exhausted) => Err(LlmError::ApiError {
                status: 500,
                message: exhausted.to_string(),
            }),
        }
    }
}
```

Update the file's imports to add `use crate::provider::CompletionRequest;` and `use horsie_models::agent::ToolResultPart;` if not already present.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p horsie-agentcore --lib testkit`
Expected: PASS, 4 new tests plus the existing ones.

- [ ] **Step 5: Run every consumer of the testkit**

Run: `cargo test --workspace --all-features`
Expected: PASS. `text()` and `tool_then_text()` kept their repeating behaviour, so no existing test should change. If one fails with an `ApiError { status: 500 }` mentioning an exhausted script, that test was relying on cycling *past* its script — report it rather than adding `then_repeating`.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt
git add agentcore/src/testkit
git commit -m "testkit: MockProvider replays a Script and records requests"
```

---

### Task 3: `actor` gains a `test-util` feature and `FaultyJournal`

Why this is an unlock, not cleanup: `FailingPersistJournal` currently lives inside `actor/src/runtime.rs`'s `#[cfg(test)] mod tests` (`:376-427`), so `server` and `workflow` cannot reach it. That is the mechanical reason no journal-failure test exists at the `SessionActor` / `AgentActor` layer.

**Files:**
- Create: `actor/src/testkit.rs`
- Modify: `actor/Cargo.toml` (add `test-util` feature), `actor/src/lib.rs` (declare + re-export), `actor/src/runtime.rs:376-447` (delete `FailingPersistJournal`, point its test at `FaultyJournal`)

**Interfaces:**
- Consumes: `Journal`, `JournalResult`, `JournalError`, `PersistenceId` from this crate.
- Produces: `FaultyJournal<J>` with `wrapping(J) -> Self`, `fail_persist_after(usize) -> Self`, `fail_snapshot() -> Self`, `fail_replay_at(u64) -> Self`; implements `Journal` when `J: Journal`.

- [ ] **Step 1: Add the feature**

In `actor/Cargo.toml`, under `[features]`:

```toml
# Exposes `testkit` (FaultyJournal, corrupt-journal fixture) to other crates.
# actor's own unit tests get it via `cfg(test)` without the feature.
test-util = []
```

- [ ] **Step 2: Write the failing test**

Create `actor/src/testkit.rs` with only this test module:

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::journal::InMemoryJournal;

    fn pid() -> PersistenceId {
        PersistenceId::new("t", "a")
    }

    #[tokio::test]
    async fn fail_persist_after_lets_the_first_n_through() {
        let j = FaultyJournal::wrapping(InMemoryJournal::new()).fail_persist_after(1);
        assert!(j.persist(&pid(), &[vec![1]]).await.is_ok());
        assert!(j.persist(&pid(), &[vec![2]]).await.is_err());
        assert!(j.persist(&pid(), &[vec![3]]).await.is_err());
    }

    #[tokio::test]
    async fn fail_persist_after_zero_fails_immediately() {
        let j = FaultyJournal::wrapping(InMemoryJournal::new()).fail_persist_after(0);
        assert!(j.persist(&pid(), &[vec![1]]).await.is_err());
    }

    #[tokio::test]
    async fn healthy_by_default_delegates_to_inner() {
        let j = FaultyJournal::wrapping(InMemoryJournal::new());
        j.persist(&pid(), &[vec![7]]).await.unwrap();
        let mut s = j.replay(&pid(), 0).await;
        assert_eq!(s.next().await.unwrap().unwrap(), vec![7]);
    }

    #[tokio::test]
    async fn fail_snapshot_rejects_saves_but_not_persists() {
        let j = FaultyJournal::wrapping(InMemoryJournal::new()).fail_snapshot();
        assert!(j.persist(&pid(), &[vec![1]]).await.is_ok());
        assert!(j.save_snapshot(&pid(), vec![9], 1).await.is_err());
    }

    #[tokio::test]
    async fn fail_replay_at_truncates_the_stream_with_an_error() {
        let j = FaultyJournal::wrapping(InMemoryJournal::new()).fail_replay_at(2);
        j.persist(&pid(), &[vec![1], vec![2], vec![3]]).await.unwrap();
        let mut s = j.replay(&pid(), 0).await;
        assert!(s.next().await.unwrap().is_ok()); // seq 1
        assert!(s.next().await.unwrap().is_err()); // seq 2 → injected failure
    }
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p horsie-actor --lib testkit`
Expected: FAIL — unresolved module `testkit` / `cannot find type FaultyJournal`.

- [ ] **Step 4: Write the implementation**

Prepend to `actor/src/testkit.rs`:

```rust
//! Fault-injecting [`Journal`] wrappers and on-disk fixtures.
//!
//! Gated behind `cfg(any(test, feature = "test-util"))`: available to the actor
//! crate's own tests unconditionally, and to `server` / `workflow` when they
//! enable `horsie-actor/test-util`.

use crate::error::JournalError;
use crate::journal::{Journal, JournalResult};
use crate::persistence_id::PersistenceId;
use async_trait::async_trait;
use futures_util::stream::{self, BoxStream, StreamExt};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Wraps any [`Journal`], failing selected operations on demand.
pub struct FaultyJournal<J> {
    inner: J,
    /// Number of `persist` calls to allow before failing; `None` = never fail.
    persist_budget: Option<usize>,
    persists: AtomicUsize,
    fail_snapshot: bool,
    /// Sequence number at which `replay` yields an error instead of the event.
    replay_fails_at: Option<u64>,
}

impl<J> FaultyJournal<J> {
    /// A healthy wrapper — every call delegates until a fault is configured.
    pub fn wrapping(inner: J) -> Self {
        Self {
            inner,
            persist_budget: None,
            persists: AtomicUsize::new(0),
            fail_snapshot: false,
            replay_fails_at: None,
        }
    }

    /// Allow `n` successful persists, then fail every one after.
    #[must_use]
    pub fn fail_persist_after(mut self, n: usize) -> Self {
        self.persist_budget = Some(n);
        self
    }

    /// Fail every `save_snapshot`.
    #[must_use]
    pub fn fail_snapshot(mut self) -> Self {
        self.fail_snapshot = true;
        self
    }

    /// Yield an error in place of the event at `seq`, ending the replay there.
    #[must_use]
    pub fn fail_replay_at(mut self, seq: u64) -> Self {
        self.replay_fails_at = Some(seq);
        self
    }
}

#[async_trait]
impl<J: Journal> Journal for FaultyJournal<J> {
    async fn persist(&self, pid: &PersistenceId, events: &[Vec<u8>]) -> JournalResult<()> {
        if let Some(budget) = self.persist_budget {
            if self.persists.fetch_add(1, Ordering::Relaxed) >= budget {
                return Err(JournalError::Backend("injected persist failure".into()));
            }
        }
        self.inner.persist(pid, events).await
    }

    async fn replay(
        &self,
        pid: &PersistenceId,
        after_seq: u64,
    ) -> BoxStream<'_, JournalResult<Vec<u8>>> {
        let Some(fail_at) = self.replay_fails_at else {
            return self.inner.replay(pid, after_seq).await;
        };
        let mut out: Vec<JournalResult<Vec<u8>>> = Vec::new();
        let mut seq = after_seq;
        let mut inner = self.inner.replay(pid, after_seq).await;
        while let Some(item) = inner.next().await {
            seq += 1;
            if seq >= fail_at {
                out.push(Err(JournalError::Backend("injected replay failure".into())));
                break;
            }
            out.push(item);
        }
        stream::iter(out).boxed()
    }

    async fn save_snapshot(
        &self,
        pid: &PersistenceId,
        state: Vec<u8>,
        seq_nr: u64,
    ) -> JournalResult<()> {
        if self.fail_snapshot {
            return Err(JournalError::Backend("injected snapshot failure".into()));
        }
        self.inner.save_snapshot(pid, state, seq_nr).await
    }

    async fn latest_snapshot(&self, pid: &PersistenceId) -> JournalResult<Option<(Vec<u8>, u64)>> {
        self.inner.latest_snapshot(pid).await
    }

    async fn delete_events_before(&self, pid: &PersistenceId, seq_nr: u64) -> JournalResult<()> {
        self.inner.delete_events_before(pid, seq_nr).await
    }

    async fn copy_snapshot(&self, from: &PersistenceId, to: &PersistenceId) -> JournalResult<()> {
        self.inner.copy_snapshot(from, to).await
    }

    async fn clear(&self, pid: &PersistenceId) -> JournalResult<()> {
        self.inner.clear(pid).await
    }
}
```

- [ ] **Step 5: Declare the module**

In `actor/src/lib.rs`, alongside the existing module declarations:

```rust
#[cfg(any(test, feature = "test-util"))]
pub mod testkit;
```

- [ ] **Step 6: Retire `FailingPersistJournal`**

Delete `actor/src/runtime.rs:376-427` (the `FailingPersistJournal` struct and its `Journal` impl). In the surviving test `ask_with_persist_and_ack_reports_journal_failure` (`:429-447`), replace the journal construction:

```rust
let journal = Arc::new(
    crate::testkit::FaultyJournal::wrapping(InMemoryJournal::new()).fail_persist_after(0),
);
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p horsie-actor --all-features`
Expected: PASS — 5 new testkit tests, and `ask_with_persist_and_ack_reports_journal_failure` still passing against the replacement.

- [ ] **Step 8: Lint and commit**

```bash
cargo clippy -p horsie-actor --all-targets --all-features -- -D warnings
cargo fmt
git add actor/
git commit -m "actor: add test-util feature with FaultyJournal"
```

---

### Task 4: Corrupt-journal fixture and the item 13 red test

**Files:**
- Modify: `actor/src/testkit.rs` (add `write_corrupt_journal`)
- Test: `actor/src/testkit.rs` tests module (fixture self-test), `actor/tests/journal_corruption.rs` (the red test)

**Interfaces:**
- Consumes: `FileJournal`'s on-disk format — one line per batch, `base64(JSON([base64(event0), ...]))` (`actor/src/file_journal.rs:55-60`).
- Produces: `write_corrupt_journal(root: &Path, pid: &PersistenceId, batches: &[Vec<Vec<u8>>], corrupt_at: usize) -> std::io::Result<()>` — writes `batches` as valid lines, replacing line index `corrupt_at` with undecodable bytes.

- [ ] **Step 1: Write the fixture's self-test**

Add to `actor/src/testkit.rs`'s `mod tests`:

```rust
#[cfg(feature = "file-journal")]
#[tokio::test]
async fn corrupt_fixture_produces_a_file_that_stops_decoding_midway() {
    let dir = tempfile::tempdir().unwrap();
    let pid = PersistenceId::new("t", "corrupt");
    write_corrupt_journal(
        dir.path(),
        &pid,
        &[vec![vec![1]], vec![vec![2]], vec![vec![3]]],
        1,
    )
    .unwrap();

    let j = crate::file_journal::FileJournal::new(dir.path());
    let mut s = j.replay(&pid, 0).await;
    let first = s.next().await.unwrap().unwrap();
    assert_eq!(first, vec![1]);
    // Everything past the corrupt line is unreachable — that is the fixture working,
    // and separately it is the bug (#61 item 13), asserted in tests/journal_corruption.rs.
    assert!(s.next().await.is_none());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p horsie-actor --all-features --lib corrupt_fixture`
Expected: FAIL — `cannot find function write_corrupt_journal`.

- [ ] **Step 3: Write the fixture**

Add to `actor/src/testkit.rs` (gated, because the encoding is `file-journal`'s):

```rust
/// Write a `FileJournal`-format log for `pid` under `root`, replacing the line at
/// index `corrupt_at` with bytes that cannot be base64-decoded.
///
/// Mirrors `FileJournal::persist`'s framing exactly: one line per batch, each line
/// `base64(JSON([base64(event0), base64(event1), ...]))`.
#[cfg(any(test, feature = "file-journal"))]
pub fn write_corrupt_journal(
    root: &std::path::Path,
    pid: &PersistenceId,
    batches: &[Vec<Vec<u8>>],
    corrupt_at: usize,
) -> std::io::Result<()> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use std::io::Write;

    let path = root
        .join("actors")
        .join(&pid.kind)
        .join(&pid.id)
        .join("journal.jsonl");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(&path)?;
    for (index, batch) in batches.iter().enumerate() {
        let line = if index == corrupt_at {
            "!!!not-base64!!!".to_string()
        } else {
            let encoded: Vec<String> = batch.iter().map(|e| STANDARD.encode(e)).collect();
            let json = serde_json::to_vec(&encoded)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            STANDARD.encode(&json)
        };
        writeln!(file, "{line}")?;
    }
    file.flush()
}
```

Add `serde_json` is already a dependency; add `base64` usage requires the `file-journal` feature, which the `cfg` above enforces.

- [ ] **Step 4: Run the fixture test**

Run: `cargo test -p horsie-actor --all-features --lib corrupt_fixture`
Expected: PASS.

- [ ] **Step 5: Write the red test**

Create `actor/tests/journal_corruption.rs`:

```rust
//! #61 item 13: mid-file journal corruption silently truncates replay.
//!
//! `decode_after` treats an undecodable line as a stop condition and `break`s
//! (`actor/src/file_journal.rs:136-146`), returning a short-but-clean stream.
//! `recover` cannot distinguish that from a genuinely short log, so it adopts the
//! truncated prefix as the true state — while the surviving tail events are still
//! in the file, and the actor then appends *after* them. Permanent split-brain.

#![cfg(feature = "file-journal")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use futures_util::StreamExt;
use horsie_actor::testkit::write_corrupt_journal;
use horsie_actor::{FileJournal, Journal, PersistenceId};
use std::time::Duration;

#[tokio::test]
#[ignore = "red: #61 item 13 — corrupt journal line truncates replay silently instead of erroring"]
async fn replay_surfaces_an_error_when_a_journal_line_is_corrupt() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let dir = tempfile::tempdir().unwrap();
        let pid = PersistenceId::new("t", "corrupt");
        // Three batches; the middle line is unreadable, the third is intact.
        write_corrupt_journal(
            dir.path(),
            &pid,
            &[vec![vec![1]], vec![vec![2]], vec![vec![3]]],
            1,
        )
        .unwrap();

        let journal = FileJournal::new(dir.path());
        let items: Vec<_> = journal.replay(&pid, 0).await.collect().await;

        assert!(
            items.iter().any(std::result::Result::is_err),
            "corruption must surface as an error, got a clean {}-event stream: {items:?}",
            items.len()
        );
    })
    .await
    .expect("test timed out");
}
```

- [ ] **Step 6: Verify it is red, and red for the right reason**

Run: `cargo test -p horsie-actor --all-features --test journal_corruption -- --ignored`
Expected: FAIL with "corruption must surface as an error, got a clean 1-event stream". A *timeout* failure or a compile error means something else is wrong — fix that before moving on.

- [ ] **Step 7: Confirm the default run stays green**

Run: `cargo test -p horsie-actor --all-features`
Expected: PASS — the red test is skipped, reported as `1 ignored`.

- [ ] **Step 8: Commit**

```bash
cargo clippy -p horsie-actor --all-targets --all-features -- -D warnings
cargo fmt
git add actor/
git commit -m "actor: corrupt-journal fixture and red test for #61 item 13"
```

---

### Task 5: `Journal` conformance suite

**Files:**
- Create: `actor/tests/journal_conformance.rs`

**Interfaces:**
- Consumes: `Journal`, `InMemoryJournal`, `FileJournal`, `PersistenceId`, `spawn_root`.
- Produces: nothing other crates depend on.

Ten assertions, each a plain `async fn` taking `&dyn Journal`, called from two modules so every backend/assertion pair is an individually-ignorable test. Five are red on `FileJournal`, following mechanically from its four `Ok(())` no-ops (`file_journal.rs:85-104`): snapshot roundtrip, compaction, `copy_snapshot` seeding, `copy_snapshot` erroring with no source, and recovery-after-compaction — the last **only because its assertion covers both halves**. `FileJournal` recovers the right *value* (a full replay from event 0 reaches the same state), so the test must also assert the log was compacted, mirroring `runtime.rs:499-511`. Without that half it would pass and hide the bug.

- [ ] **Step 1: Write the suite**

Create `actor/tests/journal_conformance.rs`:

```rust
//! Journal conformance suite.
//!
//! The same contract assertions run against every `Journal` implementation. The
//! assertions come from the trait's own doc comments (`actor/src/journal.rs:18-53`),
//! which are the real spec — they are behavioural, never about storage layout,
//! which is what makes them portable across backends.
//!
//! Deliberately shaped differently from `tests/tests/provider_conformance.rs`:
//! that suite loops over backends *inside* each test, which cannot express "this
//! assertion is red for one backend only". Five of these ten are red on
//! `FileJournal` (#61 item 9), so each backend gets its own test function.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use futures_util::StreamExt;
use horsie_actor::{InMemoryJournal, Journal, PersistenceId};

fn pid(id: &str) -> PersistenceId {
    PersistenceId::new("conformance", id)
}

async fn drain(j: &dyn Journal, id: &str, after: u64) -> Vec<Vec<u8>> {
    let mut s = j.replay(&pid(id), after).await;
    let mut out = Vec::new();
    while let Some(item) = s.next().await {
        out.push(item.unwrap());
    }
    out
}

// ── the contract ─────────────────────────────────────────────────────────────

async fn persist_then_replay_returns_events_in_order(j: &dyn Journal) {
    j.persist(&pid("order"), &[vec![1], vec![2], vec![3]])
        .await
        .unwrap();
    assert_eq!(
        drain(j, "order", 0).await,
        vec![vec![1], vec![2], vec![3]],
        "replay must return events in ascending sequence order"
    );
}

async fn replay_skips_events_at_or_before_after_seq(j: &dyn Journal) {
    j.persist(&pid("skip"), &[vec![1], vec![2], vec![3]])
        .await
        .unwrap();
    assert_eq!(
        drain(j, "skip", 1).await,
        vec![vec![2], vec![3]],
        "replay(after_seq) must yield strictly-greater sequence numbers only"
    );
}

async fn logs_are_namespaced_by_kind(j: &dyn Journal) {
    j.persist(&PersistenceId::new("workflow", "shared"), &[vec![1]])
        .await
        .unwrap();
    j.persist(&PersistenceId::new("agent", "shared"), &[vec![2]])
        .await
        .unwrap();
    let mut wf = j.replay(&PersistenceId::new("workflow", "shared"), 0).await;
    let mut ag = j.replay(&PersistenceId::new("agent", "shared"), 0).await;
    assert_eq!(wf.next().await.unwrap().unwrap(), vec![1]);
    assert_eq!(ag.next().await.unwrap().unwrap(), vec![2]);
}

async fn clear_removes_all_state(j: &dyn Journal) {
    j.persist(&pid("cleared"), &[vec![1]]).await.unwrap();
    j.clear(&pid("cleared")).await.unwrap();
    assert!(drain(j, "cleared", 0).await.is_empty());
}

async fn persist_continues_numbering_after_compaction(j: &dyn Journal) {
    j.persist(&pid("numbering"), &[vec![1], vec![2]])
        .await
        .unwrap();
    j.delete_events_before(&pid("numbering"), 2).await.unwrap();
    j.persist(&pid("numbering"), &[vec![3]]).await.unwrap();
    assert_eq!(
        drain(j, "numbering", 2).await,
        vec![vec![3]],
        "an event's sequence number must be stable across compaction"
    );
}

async fn snapshot_roundtrips_with_seq(j: &dyn Journal) {
    j.save_snapshot(&pid("snap"), vec![9, 9], 5).await.unwrap();
    assert_eq!(
        j.latest_snapshot(&pid("snap")).await.unwrap(),
        Some((vec![9, 9], 5)),
        "a saved snapshot must be readable back with its sequence number"
    );
}

async fn delete_events_before_compacts(j: &dyn Journal) {
    j.persist(&pid("compact"), &[vec![1], vec![2], vec![3]])
        .await
        .unwrap();
    j.delete_events_before(&pid("compact"), 2).await.unwrap();
    assert_eq!(
        drain(j, "compact", 0).await,
        vec![vec![3]],
        "delete_events_before must drop events at or below seq_nr"
    );
}

async fn copy_snapshot_seeds_new_id(j: &dyn Journal) {
    j.persist(&pid("src"), &[vec![1], vec![2]]).await.unwrap();
    j.save_snapshot(&pid("src"), vec![7], 2).await.unwrap();
    j.copy_snapshot(&pid("src"), &pid("dst")).await.unwrap();
    assert_eq!(
        j.latest_snapshot(&pid("dst")).await.unwrap(),
        Some((vec![7], 2)),
        "copy_snapshot must seed the destination with the source snapshot"
    );
    assert!(
        drain(j, "dst", 2).await.is_empty(),
        "the destination must start with an empty event log"
    );
}

async fn copy_snapshot_without_source_errors(j: &dyn Journal) {
    assert!(
        j.copy_snapshot(&pid("missing"), &pid("dst2")).await.is_err(),
        "copying a snapshot that does not exist must fail, not silently succeed"
    );
}

/// Both halves matter. `FileJournal` recovers the correct *value* via full replay
/// even with snapshotting disabled — only the compaction assertion catches it.
async fn snapshot_then_compact_leaves_only_later_events(j: &dyn Journal) {
    j.persist(&pid("e2e"), &[vec![1], vec![2]]).await.unwrap();
    j.save_snapshot(&pid("e2e"), vec![42], 2).await.unwrap();
    j.delete_events_before(&pid("e2e"), 2).await.unwrap();
    j.persist(&pid("e2e"), &[vec![3]]).await.unwrap();

    assert_eq!(
        j.latest_snapshot(&pid("e2e")).await.unwrap(),
        Some((vec![42], 2)),
        "recovery must start from the snapshot"
    );
    assert_eq!(
        drain(j, "e2e", 0).await,
        vec![vec![3]],
        "only post-snapshot events should remain in the log"
    );
}

// ── backends ─────────────────────────────────────────────────────────────────

mod in_memory {
    use super::*;

    fn journal() -> InMemoryJournal {
        InMemoryJournal::new()
    }

    #[tokio::test]
    async fn persist_then_replay_returns_events_in_order() {
        super::persist_then_replay_returns_events_in_order(&journal()).await;
    }
    #[tokio::test]
    async fn replay_skips_events_at_or_before_after_seq() {
        super::replay_skips_events_at_or_before_after_seq(&journal()).await;
    }
    #[tokio::test]
    async fn logs_are_namespaced_by_kind() {
        super::logs_are_namespaced_by_kind(&journal()).await;
    }
    #[tokio::test]
    async fn clear_removes_all_state() {
        super::clear_removes_all_state(&journal()).await;
    }
    #[tokio::test]
    async fn persist_continues_numbering_after_compaction() {
        super::persist_continues_numbering_after_compaction(&journal()).await;
    }
    #[tokio::test]
    async fn snapshot_roundtrips_with_seq() {
        super::snapshot_roundtrips_with_seq(&journal()).await;
    }
    #[tokio::test]
    async fn delete_events_before_compacts() {
        super::delete_events_before_compacts(&journal()).await;
    }
    #[tokio::test]
    async fn copy_snapshot_seeds_new_id() {
        super::copy_snapshot_seeds_new_id(&journal()).await;
    }
    #[tokio::test]
    async fn copy_snapshot_without_source_errors() {
        super::copy_snapshot_without_source_errors(&journal()).await;
    }
    #[tokio::test]
    async fn snapshot_then_compact_leaves_only_later_events() {
        super::snapshot_then_compact_leaves_only_later_events(&journal()).await;
    }
}

#[cfg(feature = "file-journal")]
mod file {
    use super::*;
    use horsie_actor::FileJournal;

    fn journal(dir: &tempfile::TempDir) -> FileJournal {
        FileJournal::new(dir.path())
    }

    #[tokio::test]
    async fn persist_then_replay_returns_events_in_order() {
        let d = tempfile::tempdir().unwrap();
        super::persist_then_replay_returns_events_in_order(&journal(&d)).await;
    }
    #[tokio::test]
    async fn replay_skips_events_at_or_before_after_seq() {
        let d = tempfile::tempdir().unwrap();
        super::replay_skips_events_at_or_before_after_seq(&journal(&d)).await;
    }
    #[tokio::test]
    async fn logs_are_namespaced_by_kind() {
        let d = tempfile::tempdir().unwrap();
        super::logs_are_namespaced_by_kind(&journal(&d)).await;
    }
    #[tokio::test]
    async fn clear_removes_all_state() {
        let d = tempfile::tempdir().unwrap();
        super::clear_removes_all_state(&journal(&d)).await;
    }
    #[tokio::test]
    async fn persist_continues_numbering_after_compaction() {
        let d = tempfile::tempdir().unwrap();
        super::persist_continues_numbering_after_compaction(&journal(&d)).await;
    }

    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::save_snapshot is a no-op returning Ok"]
    async fn snapshot_roundtrips_with_seq() {
        let d = tempfile::tempdir().unwrap();
        super::snapshot_roundtrips_with_seq(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::delete_events_before is a no-op returning Ok"]
    async fn delete_events_before_compacts() {
        let d = tempfile::tempdir().unwrap();
        super::delete_events_before_compacts(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::copy_snapshot returns Ok having copied nothing"]
    async fn copy_snapshot_seeds_new_id() {
        let d = tempfile::tempdir().unwrap();
        super::copy_snapshot_seeds_new_id(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal::copy_snapshot succeeds with no source snapshot"]
    async fn copy_snapshot_without_source_errors() {
        let d = tempfile::tempdir().unwrap();
        super::copy_snapshot_without_source_errors(&journal(&d)).await;
    }
    #[tokio::test]
    #[ignore = "red: #61 item 9 — FileJournal never compacts, so the whole log is replayed forever"]
    async fn snapshot_then_compact_leaves_only_later_events() {
        let d = tempfile::tempdir().unwrap();
        super::snapshot_then_compact_leaves_only_later_events(&journal(&d)).await;
    }
}
```

- [ ] **Step 2: Run the green half**

Run: `cargo test -p horsie-actor --all-features --test journal_conformance`
Expected: PASS — 15 tests run (10 in-memory + 5 file), `5 ignored`.

- [ ] **Step 3: Confirm the red half is red, and for the stated reason**

Run: `cargo test -p horsie-actor --all-features --test journal_conformance -- --ignored`
Expected: 5 FAILED. Check each message matches its `#[ignore]` reason — e.g. `copy_snapshot_without_source_errors` must fail on "copying a snapshot that does not exist must fail, not silently succeed", not on a panic elsewhere. **If any of the five passes, stop and report** — the no-op may have been fixed, and the catalogue entry needs deleting rather than the assertion loosening.

- [ ] **Step 4: Commit**

```bash
cargo clippy -p horsie-actor --all-targets --all-features -- -D warnings
cargo fmt
git add actor/tests/journal_conformance.rs
git commit -m "actor: Journal conformance suite, red on FileJournal (#61 item 9)"
```

---

### Task 6: `runtime-client` gains `test-util`, and `MockTransport` can fail

**Files:**
- Create: `runtime-client/src/testkit.rs`
- Modify: `runtime-client/src/transport.rs:40-135` (remove `MockTransport`), `runtime-client/src/lib.rs:7` (gate the export), `runtime-client/Cargo.toml`

**Interfaces:**
- Consumes: `Script<T>` from Task 1 via `horsie-agentcore/test-util`; `RuntimeTransport`, `TransportError` from this crate.
- Produces: `TransportOutcome { Ok(ToolResult), Err(TransportError), Hang }`; `BlockHandle` with `release(&self)`; `MockTransport` with the existing `ok(impl Into<String>)`, `output(ToolOutput)`, `err(impl Into<String>)`, `with_scan(Vec<WorkspaceScan>)`, `with_shared_skills(Vec<PluginSkill>)`, `with_session_context(impl Into<String>)`, plus new `scripted(Script<TransportOutcome>) -> Self`, `disconnect_after(usize) -> Self`, `hanging_invoke() -> (Self, BlockHandle)`, `hanging_prep() -> (Self, BlockHandle)`, `cancels(&self) -> Vec<String>`, `invocations(&self) -> Vec<ToolCall>`, `observed_by(&TransportProbe) -> Self`. `TransportProbe` with `new()`, `cancels() -> Vec<String>`, `invocations() -> Vec<ToolCall>`, and `Clone`.

`TransportProbe` exists because `MockVendor::with_transport` builds a *fresh* transport per runtime, so a test can never hold the instance the session actually uses. The probe shares the recording buffers, letting the test observe a transport it does not own — without it, the item 23 assertion cannot be written at all.

- [ ] **Step 1: Find every consumer before moving anything**

```bash
cd /Users/xiaoguang/works/repos/bloomstack/october/horsie-testkit
rg 'MockTransport' --type rust -l
```

Every crate listed that is **not** `runtime-client` needs `horsie-runtime-client = { path = "...", features = ["test-util"] }` in its `[dev-dependencies]` (or, if the reference is itself behind that crate's own `test-util` feature — as `server/src/vendor/mock.rs` is — in `[dependencies]` with the feature enabled by that crate's `test-util`). Record the list; step 6 verifies it.

- [ ] **Step 2: Add the feature**

In `runtime-client/Cargo.toml`:

```toml
[features]
# Exposes `testkit` (MockTransport and its fault modes). Pulls in agentcore's
# testkit for `Script`.
test-util = ["horsie-agentcore/test-util"]
```

- [ ] **Step 3: Write the failing tests**

Create `runtime-client/src/testkit.rs` with only this test module:

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use horsie_models::runtime::BashCall;
    use std::time::Duration;

    fn bash(cmd: &str) -> ToolCall {
        ToolCall::Bash(BashCall {
            command: cmd.to_string(),
            timeout_secs: None,
        })
    }

    #[tokio::test]
    async fn disconnect_after_serves_then_fails_forever() {
        let t = MockTransport::disconnect_after(1);
        assert!(t.invoke("c1", bash("echo 1")).await.is_ok());
        assert!(matches!(
            t.invoke("c2", bash("echo 2")).await,
            Err(TransportError::Disconnected)
        ));
        assert!(matches!(
            t.invoke("c3", bash("echo 3")).await,
            Err(TransportError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn records_invocations_and_cancels() {
        let t = MockTransport::ok("done");
        let _ = t.invoke("c1", bash("ls")).await;
        let _ = t.cancel("c1").await;
        assert_eq!(t.invocations().len(), 1);
        assert_eq!(t.cancels(), vec!["c1".to_string()]);
    }

    #[tokio::test]
    async fn hanging_invoke_blocks_until_released() {
        let (t, handle) = MockTransport::hanging_invoke();
        let t = std::sync::Arc::new(t);
        let call = {
            let t = t.clone();
            tokio::spawn(async move { t.invoke("c1", bash("sleep 999")).await })
        };
        // Still pending after a beat.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), async {})
                .await
                .is_ok()
                && !call.is_finished()
        );
        handle.release();
        let result = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("release must unblock the call")
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn hanging_prep_blocks_scan_and_session_start() {
        let (t, handle) = MockTransport::hanging_prep();
        let t = std::sync::Arc::new(t);
        let scan = {
            let t = t.clone();
            tokio::spawn(async move {
                t.scan_workspace("c1", None, vec![], "*.md".into(), false).await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!scan.is_finished(), "scan_workspace must block");
        handle.release();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), scan)
                .await
                .expect("release must unblock the scan")
                .unwrap()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_probe_observes_transports_it_does_not_own() {
        // The shape the item 23 e2e needs: the vendor builds the transport, the
        // test holds only the probe.
        let probe = TransportProbe::new();
        let build = |probe: &TransportProbe| MockTransport::ok("").observed_by(probe);
        let first = build(&probe);
        let second = build(&probe);
        let _ = first.invoke("c1", bash("a")).await;
        let _ = second.cancel("c2").await;
        assert_eq!(probe.invocations().len(), 1);
        assert_eq!(probe.cancels(), vec!["c2".to_string()]);
    }

    #[tokio::test]
    async fn one_gate_releases_every_transport_sharing_it() {
        let gate = BlockHandle::new();
        let t = std::sync::Arc::new(MockTransport::gated_invoke(&gate));
        let call = {
            let t = t.clone();
            tokio::spawn(async move { t.invoke("c1", bash("slow")).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!call.is_finished());
        gate.release();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), call)
                .await
                .expect("release must unblock")
                .unwrap()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn scripted_exhaustion_surfaces_as_a_transport_error() {
        let t = MockTransport::scripted(Script::of([TransportOutcome::Ok(ToolResult::Ok(
            ToolOutput {
                stdout: "one".into(),
                stderr: String::new(),
                exit_code: 0,
            },
        ))]));
        assert!(t.invoke("c1", bash("a")).await.is_ok());
        assert!(t.invoke("c2", bash("b")).await.is_err());
    }
}
```

If `ToolCall`'s bash variant is not `ToolCall::Bash(BashCall { command, timeout_secs })`, run `rg 'union ToolCall' -A 12 models/fluorite/runtime.fl` and use the actual shape — the assertions do not depend on which variant is used.

- [ ] **Step 4: Run to verify failure**

Run: `cargo test -p horsie-runtime-client --all-features --lib testkit`
Expected: FAIL — unresolved module `testkit`.

- [ ] **Step 5: Write the implementation**

Prepend to `runtime-client/src/testkit.rs`:

```rust
//! Fault-capable [`RuntimeTransport`] double.
//!
//! Gated behind `cfg(any(test, feature = "test-util"))`. Before this existed the
//! only double was a transport that always succeeded, which is why the entire
//! tool-failure surface was untestable (#61 R6).

use crate::transport::{RuntimeTransport, TransportError};
use async_trait::async_trait;
use horsie_agentcore::testkit::Script;
use horsie_models::runtime::{
    PluginSkill, ToolCall, ToolError, ToolOutput, ToolResult, WorkspaceScan,
};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::Notify;

/// What a scripted `invoke` does.
pub enum TransportOutcome {
    /// Answer with this result.
    Ok(ToolResult),
    /// Fail at the transport layer — `Disconnected` is the interesting one.
    Err(TransportError),
    /// Never return until the test releases the gate.
    Hang,
}

/// Releases a transport blocked on [`MockTransport::hanging_invoke`] or
/// [`MockTransport::hanging_prep`]. Mirrors `mock-llm`'s handle of the same name so
/// the repo has one hang vocabulary.
#[derive(Clone)]
pub struct BlockHandle {
    gate: Arc<Notify>,
}

impl BlockHandle {
    /// A fresh gate. Needed when several transports must share one gate — e.g. a
    /// vendor factory that builds a new transport per runtime.
    #[must_use]
    pub fn new() -> Self {
        Self {
            gate: Arc::new(Notify::new()),
        }
    }

    /// Unblock every waiter.
    pub fn release(&self) {
        self.gate.notify_waiters();
    }
}

impl Default for BlockHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Mock transport: a canned result by default, a [`Script`] of outcomes on demand.
pub struct MockTransport {
    script: Option<Script<TransportOutcome>>,
    result: ToolResult,
    scan: Vec<WorkspaceScan>,
    shared: Vec<PluginSkill>,
    session_context: String,
    /// When set, `invoke` waits on this gate before answering.
    invoke_gate: Option<Arc<Notify>>,
    /// When set, `scan_workspace` and `run_session_start` wait on this gate.
    prep_gate: Option<Arc<Notify>>,
    cancels: Arc<Mutex<Vec<String>>>,
    invocations: Arc<Mutex<Vec<ToolCall>>>,
}

/// Observes a transport's recorded calls without owning it.
///
/// `MockVendor::with_transport` builds a fresh transport per runtime, so a test can
/// never hold the instance the session actually uses. A probe shares the recording
/// buffers, which is the only way to assert on a transport the server constructed.
#[derive(Clone, Default)]
pub struct TransportProbe {
    cancels: Arc<Mutex<Vec<String>>>,
    invocations: Arc<Mutex<Vec<ToolCall>>>,
}

impl TransportProbe {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every `call_id` passed to `cancel` on any observed transport, in order.
    pub fn cancels(&self) -> Vec<String> {
        self.cancels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every tool call any observed transport was asked to run, in order.
    pub fn invocations(&self) -> Vec<ToolCall> {
        self.invocations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl MockTransport {
    fn base(result: ToolResult) -> Self {
        Self {
            script: None,
            result,
            scan: Vec::new(),
            shared: Vec::new(),
            session_context: String::new(),
            invoke_gate: None,
            prep_gate: None,
            cancels: Arc::new(Mutex::new(Vec::new())),
            invocations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Record this transport's calls into `probe` as well as its own buffers.
    #[must_use]
    pub fn observed_by(mut self, probe: &TransportProbe) -> Self {
        self.cancels = probe.cancels.clone();
        self.invocations = probe.invocations.clone();
        self
    }

    pub fn ok(stdout: impl Into<String>) -> Self {
        Self::base(ToolResult::Ok(ToolOutput {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
        }))
    }

    /// Return a specific [`ToolOutput`] (lets tests exercise stderr / exit codes).
    pub fn output(output: ToolOutput) -> Self {
        Self::base(ToolResult::Ok(output))
    }

    pub fn err(reason: impl Into<String>) -> Self {
        Self::base(ToolResult::Err(ToolError {
            reason: reason.into(),
        }))
    }

    /// Replay `script` for every `invoke`. Exhaustion is a transport error, so a
    /// test that over-runs its script fails loudly.
    pub fn scripted(script: Script<TransportOutcome>) -> Self {
        let mut t = Self::ok("");
        t.script = Some(script);
        t
    }

    /// Answer `n` calls successfully, then report `Disconnected` forever — a
    /// runtime whose socket dropped mid-run.
    pub fn disconnect_after(n: usize) -> Self {
        let oks = (0..n).map(|_| {
            TransportOutcome::Ok(ToolResult::Ok(ToolOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }))
        });
        Self::scripted(
            Script::of(oks)
                .labelled("disconnect_after")
                .then_repeating_with(|| TransportOutcome::Err(TransportError::Disconnected)),
        )
    }

    /// A transport whose `invoke` blocks until `handle` is released. Takes the
    /// handle so many transports can share one gate.
    #[must_use]
    pub fn gated_invoke(handle: &BlockHandle) -> Self {
        let mut t = Self::ok("");
        t.invoke_gate = Some(handle.gate.clone());
        t
    }

    /// A transport whose `scan_workspace` and `run_session_start` block until
    /// `handle` is released — the shape that wedges `provide()` (#61 item 5).
    #[must_use]
    pub fn gated_prep(handle: &BlockHandle) -> Self {
        let mut t = Self::ok("");
        t.prep_gate = Some(handle.gate.clone());
        t
    }

    /// Sugar: a gated-invoke transport and its own fresh handle.
    pub fn hanging_invoke() -> (Self, BlockHandle) {
        let handle = BlockHandle::new();
        (Self::gated_invoke(&handle), handle)
    }

    /// Sugar: a gated-prep transport and its own fresh handle.
    pub fn hanging_prep() -> (Self, BlockHandle) {
        let handle = BlockHandle::new();
        (Self::gated_prep(&handle), handle)
    }

    #[must_use]
    pub fn with_scan(mut self, scan: Vec<WorkspaceScan>) -> Self {
        self.scan = scan;
        self
    }

    #[must_use]
    pub fn with_shared_skills(mut self, shared: Vec<PluginSkill>) -> Self {
        self.shared = shared;
        self
    }

    #[must_use]
    pub fn with_session_context(mut self, context: impl Into<String>) -> Self {
        self.session_context = context.into();
        self
    }

    /// Every `call_id` passed to `cancel`, in order. Empty means cancellation never
    /// reached the sandbox.
    pub fn cancels(&self) -> Vec<String> {
        self.cancels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every tool call this transport was asked to run, in order.
    pub fn invocations(&self) -> Vec<ToolCall> {
        self.invocations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl RuntimeTransport for MockTransport {
    async fn invoke(&self, _call_id: &str, call: ToolCall) -> Result<ToolResult, TransportError> {
        self.invocations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
        if let Some(gate) = &self.invoke_gate {
            gate.notified().await;
        }
        let Some(script) = &self.script else {
            return Ok(self.result.clone());
        };
        match script.next_step() {
            Ok(TransportOutcome::Ok(result)) => Ok(result),
            Ok(TransportOutcome::Err(e)) => Err(e),
            Ok(TransportOutcome::Hang) => {
                std::future::pending::<()>().await;
                unreachable!("pending never resolves")
            }
            Err(exhausted) => Err(TransportError::SendFailed(exhausted.to_string())),
        }
    }

    async fn cancel(&self, call_id: &str) -> Result<(), TransportError> {
        self.cancels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call_id.to_string());
        Ok(())
    }

    async fn scan_workspace(
        &self,
        _call_id: &str,
        _workspace: Option<String>,
        _instruction_candidates: Vec<String>,
        _skills_glob: String,
        include_shared: bool,
    ) -> Result<(Vec<WorkspaceScan>, Vec<PluginSkill>), TransportError> {
        if let Some(gate) = &self.prep_gate {
            gate.notified().await;
        }
        let shared = if include_shared {
            self.shared.clone()
        } else {
            Vec::new()
        };
        Ok((self.scan.clone(), shared))
    }

    async fn run_session_start(&self, _call_id: &str) -> Result<String, TransportError> {
        if let Some(gate) = &self.prep_gate {
            gate.notified().await;
        }
        Ok(self.session_context.clone())
    }
}
```

`unreachable!` is permitted here only because `clippy::panic` does not cover it; if the lint rejects it, replace the arm body with `std::future::pending().await` typed as the return type directly.

- [ ] **Step 6: Delete the old double and re-gate the export**

Delete `runtime-client/src/transport.rs:40-135` (`MockTransport`, `empty_scan`, and its `RuntimeTransport` impl), keeping `TransportError` and the `RuntimeTransport` trait. Then in `runtime-client/src/lib.rs`:

```rust
mod client;
pub mod tools;
mod transport;
#[cfg(any(test, feature = "test-util"))]
pub mod testkit;

pub use client::{RuntimeCallError, RuntimeClient};
pub use tools::add_runtime_tools;
pub use transport::{RuntimeTransport, TransportError};
#[cfg(any(test, feature = "test-util"))]
pub use testkit::{BlockHandle, MockTransport, TransportOutcome, TransportProbe};
```

Then enable the feature for every crate found in step 1.

- [ ] **Step 7: Run the tests**

Run: `cargo test -p horsie-runtime-client --all-features`
Expected: PASS, 5 new tests.

- [ ] **Step 8: Confirm the double no longer ships in release builds**

Run: `cargo build -p horsie-runtime-client --release && cargo tree -p horsie-runtime-client -e features | rg test-util`
Expected: no `test-util` in the default feature set.

- [ ] **Step 9: Whole-workspace check and commit**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo fmt
git add runtime-client/ server/ workflow/ Cargo.toml
git commit -m "runtime-client: test-util feature with a fault-capable MockTransport"
```

---

### Task 7: `MockVendor` can hand out a doomed runtime

**Files:**
- Modify: `server/src/vendor/mock.rs:13-148`
- Test: same file's `mod tests`

**Interfaces:**
- Consumes: `MockTransport` from Task 6.
- Produces: `MockVendor::with_transport(impl Fn(&str) -> MockTransport + Send + Sync + 'static) -> Self` and `MockVendor::disconnect_runtime_after(usize) -> Self`. `signals()`, `last_create_spec()`, `fail_create()`, `fail_attach_times(u32)` keep their current signatures.

The factory is keyed by runtime id rather than a single stored transport so `create` can hand out a doomed transport while a later `attach` hands out a healthy one — that asymmetry is the recovery path #61 item 2 should reach and currently cannot.

- [ ] **Step 1: Write the failing test**

Add to `server/src/vendor/mock.rs`'s `mod tests`:

```rust
#[tokio::test]
async fn with_transport_supplies_the_runtime_client() {
    use horsie_models::runtime::{BashCall, ToolCall};

    let v = MockVendor::new().with_transport(|_id| MockTransport::disconnect_after(0));
    let rt = v.create("s1", &test_spec()).await.unwrap();
    let err = rt
        .runtime_client
        .invoke(ToolCall::Bash(BashCall {
            command: "echo hi".into(),
            timeout_secs: None,
        }))
        .await
        .unwrap_err();
    assert!(
        matches!(err, horsie_runtime_client::RuntimeCallError::Transport(_)),
        "a disconnected transport must surface as a transport error, got {err:?}"
    );
}

#[tokio::test]
async fn disconnect_runtime_after_is_sugar_over_with_transport() {
    let v = MockVendor::new().disconnect_runtime_after(1);
    let rt = v.create("s2", &test_spec()).await.unwrap();
    assert!(rt.runtime_client.invoke(echo_call()).await.is_ok());
    assert!(rt.runtime_client.invoke(echo_call()).await.is_err());
}
```

Add a small `fn echo_call() -> ToolCall` helper to the test module using the same shape as above.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p horsie-server --all-features --lib vendor::mock`
Expected: FAIL — `no method named with_transport`.

- [ ] **Step 3: Implement**

In `server/src/vendor/mock.rs`, add a field and two builders. Replace the struct definition and `runtime()`:

```rust
type TransportFactory = Arc<dyn Fn(&str) -> MockTransport + Send + Sync>;

#[derive(Clone)]
pub struct MockVendor {
    signals: Arc<Mutex<Vec<String>>>,
    last_spec: Arc<Mutex<Option<RuntimeSpec>>>,
    fail_attach: Arc<Mutex<u32>>,
    fail_create: bool,
    /// Builds the transport handed to each runtime, keyed by runtime id. Defaults
    /// to an always-succeeding transport.
    transport: TransportFactory,
}
```

In `MockVendor::new()`, initialise `transport: Arc::new(|_id| MockTransport::ok(""))`. Then:

```rust
/// Supply the transport backing every runtime this vendor hands out. The runtime
/// id is passed so `create` and `attach` can differ — e.g. a doomed runtime that
/// recovers on re-attach.
#[must_use]
pub fn with_transport(
    mut self,
    make: impl Fn(&str) -> MockTransport + Send + Sync + 'static,
) -> Self {
    self.transport = Arc::new(make);
    self
}

/// Every runtime answers `n` tool calls, then reports `Disconnected` forever.
#[must_use]
pub fn disconnect_runtime_after(self, n: usize) -> Self {
    self.with_transport(move |_id| MockTransport::disconnect_after(n))
}
```

and change `runtime()` to use it:

```rust
fn runtime(&self, runtime_id: &str) -> VendorRuntime {
    VendorRuntime {
        runtime_client: RuntimeClient::new((self.transport)(runtime_id)),
        handle: Arc::new(MockHandle {
            signals: self.signals.clone(),
            runtime_id: runtime_id.to_string(),
        }),
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p horsie-server --all-features --lib vendor::mock`
Expected: PASS — 2 new tests plus the 3 existing ones.

- [ ] **Step 5: Commit**

```bash
cargo clippy -p horsie-server --all-targets --all-features -- -D warnings
cargo fmt
git add server/src/vendor/mock.rs
git commit -m "server: MockVendor can hand out a fault-capable runtime transport"
```

---

### Task 8: Red tests for items 2 and 23

These go **into `tests/tests/session_server_e2e.rs`**, not a new file: the harness helpers (`start_server`, `create_session`, `send_message`, `wait_status`, `get_status`) are module-private there, and duplicating them would create a second harness to keep in sync.

**Files:**
- Modify: `tests/tests/session_server_e2e.rs` (append two tests)
- Modify: `tests/Cargo.toml` if `horsie-runtime-client` is not already a dev-dependency with `test-util`

**Interfaces:**
- Consumes: `MockVendor::disconnect_runtime_after` (Task 7), `MockTransport::cancels` (Task 6), and the existing harness helpers.
- Produces: nothing.

- [ ] **Step 1: Write the item 2 red test**

Append to `tests/tests/session_server_e2e.rs`:

```rust
/// #61 item 2: a runtime that disconnects mid-run is never released, so every
/// later turn fails identically.
///
/// `ensure_runtime` short-circuits on `if self.runtime.is_some()`
/// (`server/src/sessions/session_actor.rs:327-330`) and `self.runtime` is cleared
/// only in `halt()` (`:783`). There is no liveness check anywhere —
/// `VelosRuntimeHandle::health_check` exists and the server never calls it. So the
/// session keeps reusing a dead transport until a Stop or a server restart.
#[tokio::test]
#[ignore = "red: #61 item 2 — a disconnected runtime is retained and reused forever"]
async fn a_disconnected_runtime_is_released_so_the_next_turn_recovers() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let mock = MockLlmServer::builder().build().await;
        // Every runtime answers one tool call, then reports Disconnected forever.
        let vendor = Arc::new(MockVendor::new().disconnect_runtime_after(0));
        let server = start_server(dir.path(), vendor.clone(), &mock.url()).await;
        let client = reqwest::Client::new();
        let id = create_session(&client, &server.addr).await;

        // Turn 1: the model calls a tool; the runtime is dead, so the turn fails.
        mock.queue_tool_call("bash", serde_json::json!({ "command": "echo hi" }));
        mock.queue_response("done anyway");
        send_message(&client, &server.addr, &id, "first").await;
        wait_status(&client, &server.addr, &id, "Idle").await;

        // Turn 2: a healthy runtime should be obtained. The invariant under test is
        // that the session did not pin the dead one — assert via the vendor signals,
        // which record every create/attach.
        let before = vendor.signals().len();
        mock.queue_response("second turn ok");
        send_message(&client, &server.addr, &id, "second").await;
        wait_status(&client, &server.addr, &id, "Idle").await;

        let after = vendor.signals();
        assert!(
            after.len() > before,
            "a dead runtime must be released and re-acquired; vendor saw no new \
             create/attach between turns: {after:?}"
        );

        server.shutdown().await;
    })
    .await
    .expect("test timed out");
}
```

- [ ] **Step 2: Write the item 23 red test**

```rust
/// #61 item 23: tool-call cancellation is never propagated to the sandbox.
///
/// On cancel, `Agent::run` drops the in-flight tool futures
/// (`agentcore/src/agent.rs:574-578`), abandoning them locally only.
/// `RuntimeClient::cancel(call_id)` exists, the transport declares it, and the
/// executor WS protocol implements `CancelToolCall` — but nothing outside the
/// executor's own inbound handler ever calls it. Stopping mid-`bash` leaves the
/// command running to completion inside the sandbox.
#[tokio::test]
#[ignore = "red: #61 item 23 — Stop never reaches the sandbox; cancels() stays empty"]
async fn stopping_a_turn_cancels_the_in_flight_tool_call() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let dir = tempfile::tempdir().unwrap();
        let mock = MockLlmServer::builder().build().await;

        // One gate and one probe, shared by every transport the vendor builds:
        // the tool call blocks so Stop lands mid-flight, and the probe lets the
        // test observe a transport the server constructed.
        let gate = BlockHandle::new();
        let probe = TransportProbe::new();
        let vendor = Arc::new(MockVendor::new().with_transport({
            let gate = gate.clone();
            let probe = probe.clone();
            move |_id| MockTransport::gated_invoke(&gate).observed_by(&probe)
        }));
        let server = start_server(dir.path(), vendor, &mock.url()).await;
        let client = reqwest::Client::new();
        let id = create_session(&client, &server.addr).await;

        mock.queue_tool_call("bash", serde_json::json!({ "command": "sleep 999" }));
        send_message(&client, &server.addr, &id, "run something slow").await;
        wait_status(&client, &server.addr, &id, "Running").await;

        client
            .post(format!("http://{}/api/sessions/{id}/stop", server.addr))
            .send()
            .await
            .unwrap();
        wait_status(&client, &server.addr, &id, "Interrupted").await;
        gate.release();

        assert!(
            !probe.cancels().is_empty(),
            "Stop must propagate a cancel to the runtime; the sandbox never heard about it"
        );

        server.shutdown().await;
    })
    .await
    .expect("test timed out");
}
```

Add `use horsie_runtime_client::{BlockHandle, MockTransport, TransportProbe};` to the file's imports.

- [ ] **Step 3: Verify both are red for the stated reason**

Run: `cargo test -p horsie-tests --test session_server_e2e -- --ignored a_disconnected_runtime stopping_a_turn`
Expected: 2 FAILED, each on its own assertion message. A timeout failure means the harness wiring is wrong, not that the finding reproduced — fix the wiring first.

- [ ] **Step 4: Confirm the default run stays green**

Run: `cargo test -p horsie-tests --test session_server_e2e`
Expected: PASS, `2 ignored`.

- [ ] **Step 5: Commit**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt
git add tests/
git commit -m "tests: red cases for #61 items 2 and 23"
```

---

### Task 9: `mock-llm` can end a stream without its terminal event

The load-bearing capability. #61 item 1 is "a stream that terminates without its terminal event is reported as a *successful* turn" — OpenAI's `Err(StreamEnded) => break` (`providers/openai/src/lib.rs:392`) and Anthropic's `while let` exiting with `last_error: None` (`providers/anthropic/src/lib.rs:510`). Today `mock-llm` can only ever end a stream cleanly *with* its terminal event, so the branch has never been exercised.

**Files:**
- Modify: `providers/mock-llm/src/server.rs:17-72` (variants), `:430-434` (Anthropic dispatch), `:280-340` (queue methods)
- Modify: `providers/mock-llm/src/openai.rs:162-201` (OpenAI dispatch)
- Test: both files' `mod tests`

**Interfaces:**
- Produces: `MockResponse::CutStream { chunks: Vec<String>, after: usize }`, `MockResponse::CutToolCallStream { name: String, id: String, partial_input_json: String }`; `MockLlmServer::queue_cut_stream(chunks, after)`, `MockLlmServer::queue_cut_tool_call(name, id, partial_input_json)`.

- [ ] **Step 1: Write the failing tests**

Add to `providers/mock-llm/src/openai.rs`'s `mod tests`:

```rust
#[tokio::test]
async fn cut_stream_omits_the_terminal_frame() {
    let server = MockLlmServer::builder().build().await;
    server.queue_cut_stream(["hel", "lo"], 1);

    let body = post_stream(&server).await.text().await.unwrap();

    assert!(body.contains("hel"), "body was: {body}");
    assert!(
        !body.contains("[DONE]"),
        "a cut stream must not carry its terminator: {body}"
    );
    assert!(
        !body.contains("finish_reason\":\"stop"),
        "a cut stream must not carry a finish_reason: {body}"
    );
}

#[tokio::test]
async fn cut_tool_call_stream_sends_partial_arguments_and_stops() {
    let server = MockLlmServer::builder().build().await;
    server.queue_cut_tool_call("bash", "call_1", "{\"command\": \"ec");

    let body = post_stream(&server).await.text().await.unwrap();

    assert!(body.contains("bash"), "body was: {body}");
    assert!(body.contains("{\\\"command\\\": \\\"ec"), "body was: {body}");
    assert!(!body.contains("[DONE]"), "body was: {body}");
}
```

Add the equivalent pair to `providers/mock-llm/src/server.rs`'s own tests, asserting `!body.contains("message_stop")` instead of `[DONE]`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p horsie-mock-llm cut_`
Expected: FAIL — `no method named queue_cut_stream`.

- [ ] **Step 3: Add the variants**

In `providers/mock-llm/src/server.rs`, add to `enum MockResponse`:

```rust
    /// A stream that ends after `after` events without its terminal frame — the
    /// connection dropped mid-response. Reproduces #61 item 1.
    CutStream { chunks: Vec<String>, after: usize },
    /// A tool call whose arguments are cut off mid-JSON, with no terminal frame.
    /// `partial_input_json` is emitted verbatim and is expected not to parse.
    CutToolCallStream {
        name: String,
        id: String,
        partial_input_json: String,
    },
```

- [ ] **Step 4: Dispatch on the Anthropic wire**

In `handle_messages`, extend the stream-only match at `:430-434`:

```rust
        Some(MockResponse::CutStream { chunks, after }) => {
            let mut pairs = text_stream_sse(&chunks);
            pairs.truncate(after.min(pairs.len()));
            sse_from_pairs(pairs)
        }
        Some(MockResponse::CutToolCallStream {
            name,
            id,
            partial_input_json,
        }) => sse_from_pairs(cut_tool_call_stream_sse(&name, &id, &partial_input_json)),
```

and add the helper beside `tool_call_stream_sse`:

```rust
/// A tool_use block whose `input_json_delta` is truncated mid-JSON, with no
/// `content_block_stop`, no `message_delta` and no `message_stop`.
fn cut_tool_call_stream_sse(name: &str, id: &str, partial: &str) -> Vec<(String, String)> {
    let msg_id = format!("msg_{}", uuid::Uuid::new_v4());
    vec![
        (
            "message_start".into(),
            serde_json::json!({"type":"message_start","message":{"id":msg_id,"type":"message","role":"assistant","content":[],"model":"mock-model","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}).to_string(),
        ),
        (
            "content_block_start".into(),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}).to_string(),
        ),
        (
            "content_block_delta".into(),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":partial}}).to_string(),
        ),
    ]
}
```

Both new variants must also be added to the two `unreachable!()` arms at `:469` and `:497` — extend those patterns to `MockResponse::TextStream { .. } | MockResponse::ToolCallStream { .. } | MockResponse::CutStream { .. } | MockResponse::CutToolCallStream { .. }`.

- [ ] **Step 5: Dispatch on the OpenAI wire**

In `providers/mock-llm/src/openai.rs`'s match:

```rust
        Some(MockResponse::CutStream { chunks, after }) => {
            let mut pairs = text_stream_chunks(&id, &chunks);
            pairs.truncate(after.min(pairs.len()));
            sse_from_pairs(pairs)
        }
        Some(MockResponse::CutToolCallStream {
            name,
            id: tid,
            partial_input_json,
        }) => sse_from_pairs(vec![chunk(
            &id,
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": tid,
                    "type": "function",
                    "function": { "name": name, "arguments": partial_input_json }
                }]
            }),
            None,
        )]),
```

`chunk` is private to `openai.rs`, so no visibility change is needed.

- [ ] **Step 6: Add the queue methods**

Beside `queue_truncated` in `server.rs`:

```rust
    /// Queue a text stream that is cut off after `after` SSE events, with no
    /// terminal frame — a connection dropped mid-response.
    pub fn queue_cut_stream(
        &self,
        chunks: impl IntoIterator<Item = impl Into<String>>,
        after: usize,
    ) {
        self.state
            .queue
            .lock()
            .push(QueueEntry::immediate(MockResponse::CutStream {
                chunks: chunks.into_iter().map(Into::into).collect(),
                after,
            }));
    }

    /// Queue a tool call whose arguments are cut off mid-JSON.
    pub fn queue_cut_tool_call(
        &self,
        name: impl Into<String>,
        id: impl Into<String>,
        partial_input_json: impl Into<String>,
    ) {
        self.state
            .queue
            .lock()
            .push(QueueEntry::immediate(MockResponse::CutToolCallStream {
                name: name.into(),
                id: id.into(),
                partial_input_json: partial_input_json.into(),
            }));
    }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p horsie-mock-llm`
Expected: PASS, 4 new tests.

- [ ] **Step 8: Commit**

```bash
cargo clippy -p horsie-mock-llm --all-targets --all-features -- -D warnings
cargo fmt
git add providers/mock-llm/
git commit -m "mock-llm: streams that end without their terminal event"
```

---

### Task 10: `mock-llm` per-response delay

**Deviation from the spec:** delay is a property of the *queue entry*, not a `MockResponse::Delayed { inner: Box<MockResponse> }` variant. A recursive variant would need every match arm in both wires to unwrap it, and `QueueEntry` already carries per-entry `reached` / `gate` fields that both handlers honour in exactly one place each (`server.rs:415-422`, `openai.rs:150-157`). Same capability, one-line change per wire.

`AbortBody` from the spec is **deferred**: it needs `ResponseKind::Sse`'s `Infallible` relaxed to a fallible error type, and the clean-early-end shape from Task 9 already reproduces item 1. Left in #61 rather than half-built here.

**Files:**
- Modify: `providers/mock-llm/src/server.rs` (`QueueEntry`, `queue_delayed`, the handler), `providers/mock-llm/src/openai.rs` (the handler)

**Interfaces:**
- Produces: `MockLlmServer::queue_delayed(text: impl Into<String>, delay: std::time::Duration)`.

- [ ] **Step 1: Write the failing test**

In `providers/mock-llm/src/openai.rs`'s `mod tests`:

```rust
#[tokio::test]
async fn queued_delay_defers_the_response() {
    let server = MockLlmServer::builder().build().await;
    server.queue_delayed("slow", std::time::Duration::from_millis(300));

    let started = tokio::time::Instant::now();
    let body = post_stream(&server).await.text().await.unwrap();

    assert!(body.contains("slow"), "body was: {body}");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(250),
        "the response arrived too soon: {:?}",
        started.elapsed()
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p horsie-mock-llm queued_delay`
Expected: FAIL — `no method named queue_delayed`.

- [ ] **Step 3: Implement**

Add `delay: Option<std::time::Duration>` to `QueueEntry` (`server.rs:64-72`), set to `None` in `QueueEntry::immediate` and in `blocking_response`'s literal (`:328-339`), which is the only other place that constructs one. Then add the queue method beside `queue_response`:

```rust
    /// Queue a text response that the server holds for `delay` before answering —
    /// a slow peer. With no client-side timeout this is indistinguishable from a
    /// hang (#61 item 5).
    pub fn queue_delayed(&self, text: impl Into<String>, delay: std::time::Duration) {
        self.state.queue.lock().push(QueueEntry {
            response: MockResponse::Text {
                content: text.into(),
            },
            reached: None,
            gate: None,
            delay: Some(delay),
        });
    }
```

In **both** handlers, immediately after the existing gate block:

```rust
    if let Some(e) = &entry {
        if let Some(d) = e.delay {
            tokio::time::sleep(d).await;
        }
    }
```

- [ ] **Step 4: Run and commit**

Run: `cargo test -p horsie-mock-llm`
Expected: PASS.

```bash
cargo clippy -p horsie-mock-llm --all-targets --all-features -- -D warnings
cargo fmt
git add providers/mock-llm/
git commit -m "mock-llm: per-response delay"
```

---

### Task 11: Provider conformance fault cases (items 1a, 1b, 5a, 6)

**Files:**
- Modify: `tests/tests/provider_conformance.rs`

**Interfaces:**
- Consumes: `queue_cut_stream`, `queue_cut_tool_call`, `queue_delayed` (Tasks 9-10); the file's existing `build_provider(kind, base_url)` and `KINDS`.

Items 1a and 5a are red on **both** wires, so they keep the existing `for &kind in KINDS` style. Item 6 is red on Anthropic only (`providers/anthropic/src/lib.rs:58-60` maps `BadRequest` to `LlmError::Network`; `providers/openai/src/lib.rs:40-60` already matches on status), so it gets one test per wire — the same reason the journal suite splits by backend.

- [ ] **Step 1: Write the four red tests**

Append to `tests/tests/provider_conformance.rs`:

```rust
/// #61 item 1a: a stream that ends without its terminal event is currently
/// returned as `Ok(CompletionResponse { stop_reason: EndTurn })` — an empty or
/// truncated assistant answer, journaled and shown to the user as success.
#[tokio::test]
#[ignore = "red: #61 item 1 — a cut stream is reported as a successful turn"]
async fn a_cut_stream_is_an_error_not_an_empty_success() {
    tokio::time::timeout(Duration::from_secs(30), async {
        for &kind in KINDS {
            let server = MockLlmServer::builder().build().await;
            server.queue_cut_stream(["par", "tial"], 3);
            let provider = build_provider(kind, &server.url());
            let sink = CollectingEventSink::new();

            let result = Agent::builder()
                .provider(provider)
                .toolbox(Arc::new(EmptyToolbox))
                .build()
                .unwrap()
                .run(AgentInput::user("hi"), &sink, CancellationToken::new())
                .await;

            assert!(
                matches!(result, Err(AgentError::Provider(_))),
                "{kind:?}: a truncated stream must fail the turn, got {result:?}"
            );
        }
    })
    .await
    .expect("test timed out");
}

/// #61 item 1b: a half-streamed tool call is dispatched anyway with fabricated
/// input — OpenAI substitutes `json!({})` or `Value::Null`
/// (`providers/openai/src/lib.rs:442-453`), Anthropic an empty object
/// (`providers/anthropic/src/lib.rs:537-548`). The tool then fails with a
/// confusing `InvalidInput` instead of the run failing with a provider error.
#[tokio::test]
#[ignore = "red: #61 item 1 — a tool call with unparseable input is dispatched with fabricated arguments"]
async fn a_tool_call_with_unparseable_input_is_never_dispatched() {
    tokio::time::timeout(Duration::from_secs(30), async {
        for &kind in KINDS {
            let server = MockLlmServer::builder().build().await;
            server.queue_cut_tool_call("echo", "call_1", "{\"value\": 4");
            let provider = build_provider(kind, &server.url());
            let sink = CollectingEventSink::new();
            let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let toolbox = MockToolbox::new(
                vec![horsie_agentcore::ToolSpec {
                    name: "echo".into(),
                    description: "echo".into(),
                    input_schema: serde_json::json!({ "type": "object" }),
                }],
                Arc::new(move |name: &str, input: serde_json::Value| {
                    seen.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(name.to_string());
                    Ok(input)
                }),
            );

            let _ = Agent::builder()
                .provider(provider)
                .toolbox(toolbox)
                .build()
                .unwrap()
                .run(AgentInput::user("hi"), &sink, CancellationToken::new())
                .await;

            let dispatched = calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert!(
                dispatched.is_empty(),
                "{kind:?}: a tool call whose input JSON does not parse must not be \
                 dispatched, but the toolbox saw {dispatched:?}"
            );
        }
    })
    .await
    .expect("test timed out");
}

/// #61 item 5a: neither provider sets `.timeout()`, `.connect_timeout()` or
/// `.read_timeout()` (`providers/anthropic/src/lib.rs:93-101`,
/// `providers/openai/src/lib.rs:74-76`), and reqwest's default is unlimited. Every
/// other HTTP client in the repo does set one.
#[tokio::test]
#[ignore = "red: #61 item 5 — no HTTP timeout on either provider; a slow peer waits forever"]
async fn a_slow_provider_fails_rather_than_waiting_forever() {
    tokio::time::timeout(Duration::from_secs(60), async {
        for &kind in KINDS {
            let server = MockLlmServer::builder().build().await;
            server.queue_delayed("eventually", Duration::from_secs(30));
            let provider = build_provider(kind, &server.url());
            let sink = CollectingEventSink::new();

            let started = tokio::time::Instant::now();
            let result = Agent::builder()
                .provider(provider)
                .toolbox(Arc::new(EmptyToolbox))
                .build()
                .unwrap()
                .run(AgentInput::user("hi"), &sink, CancellationToken::new())
                .await;

            assert!(
                started.elapsed() < Duration::from_secs(25),
                "{kind:?}: the provider must give up on a stalled peer, waited {:?}",
                started.elapsed()
            );
            assert!(result.is_err(), "{kind:?}: a timed-out call must be an error");
        }
    })
    .await
    .expect("test timed out");
}

/// #61 item 6: Anthropic maps `BadRequest` to `LlmError::Network`
/// (`providers/anthropic/src/lib.rs:58-60`), discarding the status, so a permanent
/// 400 — context-length exceeded, malformed schema — is reported to the user as a
/// transient network error with `recoverable: true`.
#[tokio::test]
#[ignore = "red: #61 item 6 — Anthropic classifies 400 as a network error"]
async fn anthropic_reports_a_400_as_an_api_error_with_its_status() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let server = MockLlmServer::builder().build().await;
        server.queue_error(400, "context length exceeded");
        let provider = build_provider(ProviderKind::Anthropic, &server.url());
        let sink = CollectingEventSink::new();

        let messages = vec![Message {
            id: "m1".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart { text: "hi".into() })],
        }];
        let result = provider
            .complete(
                CompletionRequest {
                    messages: &messages,
                    system: None,
                    tools: vec![],
                    tool_choice: ToolChoice::Auto,
                    max_tokens: None,
                },
                "msg-1",
                &sink as &dyn horsie_agentcore::EventSink,
            )
            .await;

        assert!(
            matches!(result, Err(LlmError::ApiError { status: 400, .. })),
            "a 400 must keep its status, got {result:?}"
        );
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn openai_reports_a_400_as_an_api_error_with_its_status() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let server = MockLlmServer::builder().build().await;
        server.queue_error(400, "context length exceeded");
        let provider = build_provider(ProviderKind::Openai, &server.url());
        let sink = CollectingEventSink::new();

        let messages = vec![Message {
            id: "m1".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart { text: "hi".into() })],
        }];
        let result = provider
            .complete(
                CompletionRequest {
                    messages: &messages,
                    system: None,
                    tools: vec![],
                    tool_choice: ToolChoice::Auto,
                    max_tokens: None,
                },
                "msg-1",
                &sink as &dyn horsie_agentcore::EventSink,
            )
            .await;

        assert!(
            matches!(result, Err(LlmError::ApiError { status: 400, .. })),
            "a 400 must keep its status, got {result:?}"
        );
    })
    .await
    .expect("test timed out");
}
```

The two item-6 tests call the provider directly rather than through `Agent`, because the assertion is about `LlmError`'s variant — which `Agent` wraps in `AgentError::Provider`. The messages are built inside the async block so `CompletionRequest<'a>` can borrow them; no `'static` gymnastics needed.

Add to the file header: `use std::time::Duration;`, `use horsie_agentcore::{AgentError, CompletionRequest, ToolChoice};`, and `use horsie_models::agent::{ContentPart, Message, Role, TextPart};`. Adjust the import paths if these types are re-exported elsewhere — `rg 'pub use' agentcore/src/lib.rs` shows the crate's public surface.

- [ ] **Step 2: Verify the red/green split**

Run: `cargo test -p horsie-tests --test provider_conformance`
Expected: PASS with `4 ignored` — including the *green* `openai_reports_a_400...`, which proves the assertion is satisfiable and the Anthropic red is a real difference, not a broken test.

Run: `cargo test -p horsie-tests --test provider_conformance -- --ignored`
Expected: 4 FAILED, each on its own assertion message.

- [ ] **Step 3: Commit**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt
git add tests/tests/provider_conformance.rs
git commit -m "tests: provider conformance fault cases for #61 items 1, 5, 6"
```

---

### Task 12: CI job and catalogue close-out

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `docs/superpowers/specs/2026-07-27-fault-injection-testkit-design.md` (record what shipped)

- [ ] **Step 1: Add the non-blocking job**

In `.github/workflows/ci.yml`, after the existing `cargo test` step (`:50`):

```yaml
      - name: Red catalogue (#61)
        continue-on-error: true
        run: cargo test --locked --workspace --all-features -- --ignored
```

Red tests failing here is the expected state. The job's only purpose is to surface a red test that has quietly gone *green* — a finding fixed as a side effect, which should be closed deliberately rather than left ignored and rotting.

- [ ] **Step 2: Verify the required checks stay green**

Run: `cargo test --locked --workspace --all-features`
Expected: PASS. Ignored counts should total the catalogue size: 5 (journal conformance) + 1 (journal corruption) + 2 (session e2e) + 4 (provider conformance) = **12 ignored**.

- [ ] **Step 3: Verify the catalogue is greppable**

```bash
rg 'ignore = "red:' --type rust
```

Expected: 12 matches, each naming a distinct #61 item. Items covered: 1 (two probes), 2, 5a, 6, 9 (five probes), 13, 23.

- [ ] **Step 4: Record what shipped and what did not**

Append a "Shipped" section to the design doc listing the 12 red tests, and note the three probes this plan does **not** deliver, with why:

- **Item 5b** (`provide()` is not cancellable) — needs `MockTransport::gated_prep` wired through a session e2e; the transport capability exists, the test does not.
- **Item 21** (retry rebuilds history from the wrong place) — needs `MockProvider::requests()` asserted against a live `AgentActor` with `max_retries > 0`; `requests()` exists, the test does not.
- **Item 22** (a journal write failure inside `complete()` is retried against the LLM) — needs `FaultyJournal` wired into an `AgentActor`'s `PersistSink`; the journal double exists, the test does not.

All three are blocked only on `AgentActor` construction details, which were not verified while planning. They are a natural second plan, and every double they need is delivered here.

- [ ] **Step 5: Commit and open the PR**

```bash
cargo fmt --check
git add .github/workflows/ci.yml docs/superpowers/specs/
git commit -m "ci: non-blocking red-catalogue job for #61"
git push -u origin feat/fault-injection-testkit
```

PR title: `Fault-injection testkit: make #61's failures expressible`
PR body: one long line per paragraph (no hard wrapping — GitHub renders newlines as literal breaks). Cover: what the testkit adds at each seam, the 12 red tests and how to run them (`cargo test --workspace --all-features -- --ignored`), the convention that each fix PR deletes exactly one `#[ignore]`, and the three deferred probes.
