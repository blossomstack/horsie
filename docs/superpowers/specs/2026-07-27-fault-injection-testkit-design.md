# Fault-injection testkit — make failure expressible before fixing it

Status: approved design, 2026-07-27

## Problem

[horsie#61](https://github.com/blossomstack/horsie/issues/61) is an audit of the
session → `SessionActor` → `AgentActor` → runtime path that found 41 numbered
items across P0–P4. Its root-cause section names six structural causes; the last
one, **R6**, explains why the list is 41 items and not 15:

> Test doubles that cannot fail, and a test backend with different semantics than
> production. Nothing above could be *caught*.

Concretely, today:

- `MockProvider` cannot return an error at all, and **cycles** its response queue
  when exhausted (`agentcore/src/testkit.rs:79-86`), so a test that over-runs its
  script silently gets a repeat instead of failing.
- `MockVendor`'s runtime is hardwired to `RuntimeClient::new(MockTransport::ok(""))`
  (`server/src/vendor/mock.rs:88`), and `MockTransport`
  (`runtime-client/src/transport.rs:41-135`) has no failure, disconnect, or hang
  mode. The entire tool-failure surface is therefore untestable.
- `mock-llm` can return an error status and can block before responding, but
  cannot end a stream without its terminal event — the exact shape of item 1.
- `FailingPersistJournal` exists but is trapped inside `runtime.rs`'s
  `#[cfg(test)] mod tests` (`actor/src/runtime.rs:376-427`), so no other crate can
  reach it. That is the mechanical reason there is no journal-failure test at the
  `SessionActor` / `AgentActor` layer.
- Sharpest instance: **`InMemoryJournal` correctly implements the four operations
  `FileJournal` no-ops** (`actor/src/journal.rs:108-149` vs
  `actor/src/file_journal.rs:85-104`). The suite does not merely miss the fork
  bug — it actively certifies behaviour production does not have.

This project builds the missing fault capability and lands a catalogue of red
tests for the findings it unlocks. It fixes none of them.

## Goals

1. Every failure at three seams — LLM wire, runtime transport, journal — is
   expressible by a test. Five doubles are involved: the seams' own doubles plus
   the provider and vendor wrappers that sit in front of two of them.
2. The nine findings those seams unlock (eleven probes — items 1 and 5 each have
   two distinct shapes) exist as real, named, ignored tests, so the gap is durable
   and greppable rather than living only in an issue.
3. A conformance suite exists for `Journal`, generalizing the pattern
   `tests/tests/provider_conformance.rs` already invented for `LlmProvider`.
4. No test-only code ships in a production build.

## Non-goals

- **No bug fixes.** Every finding this project can express stays red. Fixes land
  in their own PRs, each deleting exactly one `#[ignore]`.
- **No shared fault vocabulary.** A central `Fault` enum consumed by every double
  was considered and rejected: the three seams have genuinely different failure
  alphabets (an HTTP server half-closing a chunked body, a trait returning
  `TransportError::Disconnected`, a journal returning `JournalError`), so the enum
  ends up generic over the error type with per-seam variants anyway.
- **No process-level fault injection** — no TCP proxy, no permission revocation.
  The one exception is the real-daemon disconnect e2e, where
  `tests/tests/session_server_e2e.rs` already kills a real daemon.
- **Not covered, deliberately:** session-concurrency scenarios (#61 items 3, 11,
  14, two-client), MCP unreachable (24), and context-window overflow (25). These
  need no new double — only test authoring — and stay in #61 for their own PRs.

## Architecture

**Principle: each fault lives in the crate that owns the trait it corrupts,
behind that crate's `test-util` feature.** No fault framework, no central
registry. The only shared thing is a script type.

| Seam | Crate (feature) | Double | New capability |
|---|---|---|---|
| LLM wire | `providers/mock-llm` | `MockLlmServer` queue | stream ending without its terminal event; body aborting mid-chunk; per-response delay |
| Provider trait | `agentcore` (`test-util`) | `MockProvider` | scripted `Result<CompletionResponse, LlmError>`; strict exhaustion; request capture |
| Runtime transport | `runtime-client` (**new** `test-util`) | `MockTransport` | scripted per-call outcome incl. `Disconnected` and hang; records `cancel()` |
| Vendor | `server` (`test-util`) | `MockVendor` | accepts a caller-supplied transport factory |
| Journal | `actor` (**new** `test-util`) | `FaultyJournal` + corrupt-file fixture | fail any operation; a mid-file-corrupted `FileJournal` on disk |

### The one shared type

A new zero-dependency leaf crate `testkit/` (`horsie-testkit`) holding `Script<T>`:
an ordered list of programmed outcomes, consumed once, where **running past the
end is an error, not a wrap-around**.

```rust
pub struct Script<T> { /* consumed once, ordered */ }

impl<T> Script<T> {
    pub fn of(steps: impl IntoIterator<Item = T>) -> Self;
    pub fn once(step: T) -> Self;
    /// Opt in, out loud, to the old cycling behaviour.
    pub fn then_repeating(self, steady: T) -> Self;
    /// `Err(ScriptExhausted { label, taken })` when the script runs out.
    pub fn next_step(&self) -> Result<T, ScriptExhausted>;
}
```

Exhaustion-is-an-error is the point: it converts "my test over-ran its script and
silently got a repeat" into a failure. `MockProvider` and `MockTransport` both
take one. Existing `MockProvider::text()` / `tool_then_text()` are reimplemented
over `.then_repeating(...)`, so today's tests keep passing while the *default*
becomes strict.

A separate crate for ~40 lines is a judgement call; the alternative is copying it
into three crates, which is worse in a repo whose CLAUDE.md leads with "narrow
interface, deep implementation."

### Two feature gates that are unlocks, not cleanup

- `actor` gains `test-util`, exporting the journal doubles. This is what makes
  journal-failure tests at the `SessionActor` / `AgentActor` layer possible at all.
- `runtime-client` gains `test-util`. `MockTransport` is currently exported
  unconditionally (`runtime-client/src/lib.rs:7`), so it ships in production
  binaries today.

Both follow the pattern `agentcore` and `server` already use. CI already runs
`cargo test --locked --workspace --all-features` (`.github/workflows/ci.yml:50`),
so the new features are compiled and their self-tests run without CI changes.

## Components

### `MockProvider` (`agentcore`, `test-util`)

```rust
pub fn scripted(script: Script<Result<CompletionResponse, LlmError>>) -> Arc<Self>;
pub fn failing(err: LlmError) -> Arc<Self>;
pub fn calls(&self) -> usize;
pub fn requests(&self) -> Vec<RequestSummary>;  // roles, message count, tool_call ids
```

`requests()` is not incidental. #61 item 21 is "the retry restarts the whole turn
from the original `history`, but the failed attempt's events were already
persisted" — the only way to catch that is to inspect what the provider was
*asked* on attempt 2. `CompletionRequest<'_>` is borrowed, so the double snapshots
a summary rather than cloning the request.

### `MockTransport` (`runtime-client`, new `test-util`)

```rust
pub enum TransportOutcome { Ok(ToolResult), Err(TransportError), Hang }

pub fn scripted(script: Script<TransportOutcome>) -> Self;
pub fn disconnect_after(n: usize) -> Self;      // n oks, then Disconnected forever
pub fn hanging() -> (Self, BlockHandle);
pub fn cancels(&self) -> Vec<String>;           // call_ids passed to cancel()
pub fn invocations(&self) -> Vec<ToolCall>;
```

`cancels()` is a direct probe for item 23: stop a turn mid-`bash` and assert the
list is non-empty. Today it is always empty — `RuntimeClient::cancel` has no
caller outside the executor's own inbound handler.

`hanging()` returns the same `BlockHandle` idiom `mock-llm` already uses
(`providers/mock-llm/src/server.rs:74-88`), so the repo has one hang vocabulary
rather than two.

### `MockVendor` (`server`, `test-util`)

```rust
pub fn with_transport(self, make: impl Fn(&str) -> MockTransport + Send + Sync + 'static) -> Self;
pub fn disconnect_runtime_after(self, n: usize) -> Self;   // sugar over the above
```

A factory keyed by runtime id, not a single transport — so `create` can hand out a
doomed transport and a later `attach` a healthy one. That asymmetry is exactly the
recovery path item 2 should be able to reach and currently cannot. `signals()`,
`fail_create()` and `fail_attach_times()` are untouched.

### Journal doubles (`actor`, new `test-util`)

```rust
FaultyJournal::wrapping(inner)
    .fail_persist_after(n)
    .fail_snapshot()
    .fail_replay_at(seq);

// fixture, not a double:
file_journal::testkit::write_corrupt_journal(dir, pid, events, corrupt_at);
```

Today's `FailingPersistJournal` becomes
`FaultyJournal::wrapping(InMemoryJournal::new()).fail_persist_after(0)`, keeping
its one existing test working while making the capability reachable from `server`
and `workflow` for the first time.

### `mock-llm` additions

```rust
MockResponse::CutStream { chunks, after }                        // clean early end
MockResponse::CutToolCallStream { name, id, partial_input_json }
MockResponse::AbortBody { after_events }                         // genuine transport error
MockResponse::Delayed { inner, delay }
```

with matching `queue_*` methods on `MockLlmServer`.

Item 1 has two distinct shapes needing different mechanisms:

- **Clean early end, no terminal event** reproduces item 1 exactly — OpenAI's
  `Err(StreamEnded) => break` (`providers/openai/src/lib.rs:392`) and Anthropic's
  `while let` exiting with `last_error: None` (`providers/anthropic/src/lib.rs:510`).
  Trivial: yield a prefix of events, end the stream.
- **Aborted body mid-chunk** produces a genuine transport error on the client, a
  different branch. `mock-llm` streams via
  `Sse<BoxStream<'static, Result<Event, Infallible>>>` (`server.rs:382`);
  `Infallible` means the body *cannot* fail, so this requires relaxing that to a
  real error type.

`CutToolCallStream` is the one that proves the fabricated-input bug: emit the
tool_use start plus a *partial* `input_json_delta`, then end, and watch OpenAI
substitute `json!({})` (`providers/openai/src/lib.rs:442-453`) and dispatch it.

Error statuses (401/403/400/5xx) need no new machinery — `queue_error(status, msg)`
already covers them; only the tests are missing.

## Conformance suites

**`actor/tests/journal_conformance.rs`** — ten assertions taken from the `Journal`
trait's own doc comments, which are the real spec: ordered replay, `after_seq`
filtering, kind namespacing, snapshot roundtrip, compaction, numbering continuity
after compaction, `copy_snapshot` seeding, `copy_snapshot` erroring with no source,
`clear`, and one end-to-end recovery through `spawn_root` after
snapshot+compaction.

Exactly five of the ten are red on `FileJournal`, and which five follows
mechanically from its four `Ok(())` no-ops (`file_journal.rs:85-104`):

- **snapshot roundtrip** — `latest_snapshot` returns `None` after a `save_snapshot`.
- **compaction** — `delete_events_before` removes nothing, so replay is unchanged.
- **`copy_snapshot` seeds a new pid** — the destination has no snapshot.
- **`copy_snapshot` with no source errors** — it returns `Ok(())` instead.
- **end-to-end recovery after snapshot+compaction** — *only because the assertion
  covers both halves*: the recovered value is correct on `FileJournal` (a full
  replay from event 0 reaches the same state), so this test must also assert the
  log was compacted, mirroring `runtime.rs:499-511`. Without that half it would
  pass and hide the bug.

The remaining five — ordered replay, `after_seq` filtering, kind namespacing,
numbering continuity after compaction, and `clear` — pass on both backends today.

**Parameterization differs deliberately from `provider_conformance`.** That suite
loops `for &kind in KINDS` *inside* each `#[tokio::test]`
(`tests/tests/provider_conformance.rs:73-75`), which cannot express "this
assertion is red for one backend only". The journal suite therefore uses a
`macro_rules!` that instantiates the body once per backend, producing
separately-ignorable
`file_copy_snapshot_seeds_new_id` / `in_memory_copy_snapshot_seeds_new_id`
functions.

**`tests/tests/provider_conformance.rs`** gains the fault cases (cut stream, cut
tool-call stream, error statuses, delay) as new `for &kind in KINDS` tests,
matching the existing style.

## The red catalogue

One convention, greppable — `rg 'ignore = "red:'` is the live gap list:

```rust
#[ignore = "red: #61 item 9 — FileJournal::copy_snapshot returns Ok having copied nothing"]
```

Each fix PR deletes exactly one attribute. The findings this project makes
assertable:

| #61 item | Probe |
|---|---|
| 1a | `CutStream` → provider returns `Err`, not `Ok(EndTurn)` with empty text (both wires) |
| 1b | `CutToolCallStream` with unparseable partial input → toolbox records **zero** calls |
| 2 | `disconnect_runtime_after(1)` → turn 1 fails, turn 2 succeeds via a fresh attach |
| 5a | `Delayed` past a deadline → provider errors instead of awaiting forever |
| 5b | `MockTransport::hanging()` stalls `scan_workspace` / `run_session_start` inside `provide()`, then Stop → session leaves `Running` within the halt timeout |
| 6 | `queue_error(400)` on Anthropic → `ApiError { status: 400 }`, `recoverable: false` |
| 9 | five journal-conformance assertions on `FileJournal` |
| 13 | corrupt-file fixture → recovery `Err`s instead of adopting a truncated prefix |
| 21 | `max_retries=1`, script `[Err, Ok]` → attempt 2's request matches what was journaled |
| 22 | `FaultyJournal` persist failure inside `complete()` → run aborts, not retried at the LLM |
| 23 | Stop mid-`bash` → `transport.cancels()` is non-empty |

**Every red test is wrapped in `tokio::time::timeout`.** Several of these findings
*are* hangs (5a, 5b); an unbounded red test wedges CI for its full ceiling instead
of failing in seconds. The timeout is part of the assertion, not a safety net.

## CI

One added job, non-blocking:

```yaml
- name: Red catalogue (#61)
  continue-on-error: true
  run: cargo test --locked --workspace --all-features -- --ignored
```

Red tests failing there is the expected state. The job's only purpose is to
surface a red test that has quietly gone *green* — a finding fixed as a side
effect, which should then be closed deliberately rather than left ignored and
rotting. The main test job is unaffected: `cargo test` skips ignored tests, so the
catalogue costs nothing in the required checks.

## Testing the testkit

Every new fault mode gets a unit test proving it fires: `disconnect_after(1)`
really yields `Ok` then `Disconnected`; `Script` really errors on exhaustion;
`CutStream` really omits the terminal event; `write_corrupt_journal` really
produces a file whose middle line fails to decode. Shipping unverified test
infrastructure would be the same meta-failure R6 describes.

## Sequencing

The work splits into four independently landable PRs:

1. `horsie-testkit` crate + `Script<T>`, and `MockProvider` rebuilt on it
   (including `requests()`), with existing call sites migrated to
   `.then_repeating(...)`.
2. `actor` `test-util` feature: `FaultyJournal`, the corrupt-file fixture, and
   `actor/tests/journal_conformance.rs` with its five red `FileJournal` cases.
3. `runtime-client` `test-util` feature: `MockTransport` fault modes and
   `cancels()`; `MockVendor::with_transport`; the item 2 and 23 red tests.
4. `mock-llm` stream faults (including the `Infallible` relaxation), the provider
   conformance fault cases, and the CI job.

Order matters only for 1 before 3; 2 and 4 are independent.
