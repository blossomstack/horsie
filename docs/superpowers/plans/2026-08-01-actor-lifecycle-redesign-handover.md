# Actor lifecycle redesign — handover

Written 2026-08-01, at the point the server half is complete and green. Read this before touching the branch; it is the context that is *not* in the diff.

## Orientation

| | |
|---|---|
| Branch | `feat/actor-lifecycle-redesign` |
| Worktree | `october/horsie-actor-redesign` |
| Base | `origin/main` @ `2c19f57` |
| PR | blossomstack/horsie#101 (draft) |
| Design | `docs/superpowers/specs/2026-08-01-actor-lifecycle-redesign-design.md` |
| Plan + status | `docs/superpowers/plans/2026-08-01-actor-lifecycle-redesign.md` |

Commits, oldest first: `933017f` design → `2be6cd5` plan → `6f86867` vendor protocol → `d23682f` RuntimeManager → `3e9d76f` session + agent + supervisor → `5932666` session fold tests → `0a91566` plan status.

**Verify before you change anything:**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets && cargo test --workspace
```

All three are green as of `0a91566`. One e2e test is `#[ignore]`d — see "Coverage regressions" below, it is not the pre-existing one it claims to be.

## What the design is, in six sentences

The supervisor persists **existence only** (created / named / deleted) and keeps status in an in-memory cache, so a restart loads nothing and honestly reports "unknown" until a session is opened. A session actor loads on any command addressed to it, owns its own status in its own journal, and is unloaded after an idle timeout; unloading hibernates its runtime. The agent is **resident** for the session's loaded lifetime, so history and usage reads spawn nothing and acquire no runtime. A user message is **always accepted** into a durable inbox and answered at the next turn boundary — there is no `409`, and stop cancels the turn without discarding the promise. All runtime lifecycle sits behind a server-level `RuntimeManager`; no actor ever makes a vendor call on its mailbox. The runtime is created **exactly once**, at session creation, by exactly one call site — and `get` can never provision, which is what makes a silent workspace rebuild impossible.

## Map of what exists now

| File | Role |
|---|---|
| `server/src/runtime_manager.rs` | `RuntimeManager` (create/get/hibernate/delete), `RuntimeClientProvider`, `RuntimeError{Unavailable,Gone,Provision}`, all spec assembly + credential minting. `test_runtime_manager` is the `pub(crate)` fixture other test modules use. |
| `server/src/sessions/session_actor.rs` | The state machine: `SessionCommand`, `SessionDomainEvent`, `SessionState{status,pending_ask,inbox,…}`, `drain()`, `cancel_run()`, `stop_agents()`, `SessionContextProvider`. Tests at the bottom are pure fold tests. |
| `server/src/sessions/supervisor.rs` | Registry, `ensure_loaded`, `offload_idle`, `SupervisorConfig{clock,idle_timeout,tick_interval}`. Tests use `manual_config` (no background ticker) so nothing races. |
| `server/src/sessions/clock.rs` | `Clock` / `SystemClock` / `TestClock`. The idle timer must go through this — never `tokio::time::sleep` in a lifecycle test. |
| `server/src/runtime_vendor/{mod,link,fake}.rs` | `VendorError{Provision,Gone,Unavailable}`, `RuntimeVendorLink::{create,get,hibernate,delete}`, fake knobs `gone_on_get` / `block_creates` / `release_creates`. |
| `runtime-vendor/src/vendor.rs` | The real agent loop: `GetRuntime` is a liveness check, `HibernateRuntime` is a declined no-op, `lifecycle_locks` serialize per runtime id. |
| `models/fluorite/{runtime_vendor,session,session_api}.fl` | Wire truth. Regenerate with `cd clients/web && bun run generate-types` **and** `cd clients/ts && npm run generate-types`; Rust regenerates via `models/build.rs` on `cargo check`. |

## Locked decisions — do not re-litigate

These were settled with the user during brainstorming. Each was chosen over a specific alternative that was rejected on purpose.

1. **Session journal is the only durable status truth.** The supervisor's status map is a cache; it does not persist status. Rejected: supervisor-owned status, and a split by meaning.
2. **Nothing loads at boot and nothing auto-resumes.** Rejected: recreating `Running` sessions, and a boot-time repair pass. A session parked on `pending_ask` stays parked forever until the user answers.
3. **One idle timer, hot/warm → cold in a single step.** Rejected: separate hot→warm and warm→cold clocks.
4. **Hibernate is advisory and vendor-decided.** A vendor that cannot suspend keeps the runtime. The server never reasons about workspace durability.
5. **Resume never re-provisions.** A vendor that cannot resume errors; the session becomes terminally `Unrecoverable`. Rejected: `Failed{RuntimeGone}` with automatic re-provision on the next message, and an explicit "new runtime" button.
6. **"Created once" is structural, not bookkept.** One call site calls `create`. Rejected outright: a `session_runtimes` SQLite table remembering whether a session was provisioned.
7. **Single-flight is the vendor's obligation**, not server logic. `get` must not answer before an in-flight `create` for the same id resolves.
8. **Stop cancels the turn only.** The inbox survives; queued messages start the next turn immediately.
9. **Two actors, not one.** `main_agent` field + `sub_agents` map (not one map) — kept to preserve the sub-agent axis.
10. **No backward compatibility, no migration.** Event and wire enums took the names they deserve; existing state dirs must be wiped on deploy. Never add a deprecated variant "so old journals load".
11. **Journal snapshotting is out of scope** — it is `FileJournal`'s own decision behind the `Journal` trait.

## Deliberately deleted — do not restore

`Provisioning` / `Stopped` / `Interrupted` / `RecoveryFailed` statuses; `WakeMode`; `attach`; `ensure_runtime` / `ensure_agent` / `wake` / `halt`; the generation fence on both sides; `is_connected` above the manager; `NoContextProvider` and both transient-reader spawn sites; boot-time session re-spawn; `SessionCommand::Provision`; the `409 TurnInFlight` path; `UserMessageError::{Provisioning,TurnInFlight,RecoveryFailed}`.

## Remaining work, in the order I would do it

### 1. Finish the wire so the client can be built (blocks everything below)

`SessionDetail` needs `inbox: Vec<QueuedMessage { id, text, at_ms }>`, and the SSE union needs `InboxChanged { queued }` emitted by the session on `MessageQueued` and `TurnBegan`. Also delete the dead `SessionEvent::Asked` variant while you are in there. Then regenerate both TS trees.

*Done when:* `GET /api/sessions/{id}` returns queued messages, a second browser tab sees a queued message appear without reloading, and `session_server_e2e` asserts both.

### 2. Web client (`clients/web`)

Render queued inbox messages as unread; render `Unrecoverable` as a read-only banner with its reason; show an em dash for a session whose status is `null`; delete the `409` handling and the `askLocked` composer latch. Update `clients/web/e2e/` specs that assert old statuses.

*Watch for:* the composer must stay enabled while `Running` — queueing is the point. The unread markers are load-bearing, not decorative: without them, "stop, then a queued message immediately starts a new turn" reads as a bug.

### 3. Replace the two deleted e2e tests

I deleted `turn_in_flight_conflicts` and `answering_an_ask_marks_the_session_running_and_rejects_a_concurrent_message` because their premise (the `409` path) no longer exists. Their replacements are owed:

- a message sent during a run is `202`, and the next turn carries both texts merged with a blank line;
- a crash with a non-empty inbox reloads with the inbox intact and **nothing running**;
- `Gone` is terminal (`Unrecoverable`, further messages rejected) while `Unavailable` is retryable (`Failed`, next message succeeds once the vendor is back) — drive with `gone_on_get(true)` and by dropping the fake agent respectively;
- the idle clock through the full HTTP stack: advance, tick, assert `hibernate:<id>`, then a message reloads and emits `get:<id>` and never a second `create:`.

### 4. Agent-side gaps from Task 4

Two items I did not reach:

- **`run_id`**: each run should carry an id, `RunFinished` should carry it, and a report whose id is stale should be dropped inside the agent. Today the agent still relies on `self.running` alone. This matters now that the agent is resident and cancel is cooperative.
- **Journal the tool-call repair once.** `sanitize_for_resume` / `sanitize_answering` in `workflow/src/agent_actor.rs` still run on a clone at every turn start and are never persisted, so "what the model saw" exists nowhere on disk. Rename to `repair_unanswered_tool_calls` / `…_except`, and persist the synthetic `tool_result`s on `RunCancelled` and on recovery-after-interruption instead.

### 5. Vendor conformance against the real agent loop

The four conformance tests run against `FakeRuntimeVendor`. The `lifecycle_locks` I added to `runtime-vendor/src/vendor.rs` — the thing that actually keeps the contract — are **unexercised**. Needs a test harness in the `runtime-vendor` crate: a WS pair, a `RuntimeProvider` double that blocks on a barrier, and the same four assertions.

## Warts I introduced — fix them rather than build on them

- **`RUNTIME_GONE_PREFIX` is a string marker.** `SessionContextProvider::provide` prefixes its error string, and `on_agent_outcome` string-matches it to decide terminal-vs-retryable. It works and it is tested, but classifying a failure by grepping its message is exactly the pattern #73 removed from the Anthropic provider. Replace with a typed failure carried through `AgentOutcome::Failed` (e.g. a `terminal: bool` or a small enum).
- **`SessionAck { message_id }` is reused for stop and delete with an empty string.** Those endpoints should have their own empty ack type.
- **`sub_agents` is a dead field** today (declared, never populated). That is intentional per decision 9, but a reviewer will ask.
- **`SessionSupervisorCommand::Shutdown` drives `PrepareOffload`** on each child, which means shutdown and idle-offload share a path. Fine, but note `main.rs` still never calls `Shutdown` — `axum::serve` has no shutdown signal wired, so a real restart always goes through the interrupted-turn path.

## Coverage regressions — the honest list

- **Cancel/stop has no direct test any more.** My rewrite of `session_actor.rs` dropped the old test module wholesale, including `stop_waits_for_the_cancelled_run_to_unwind`. The `#[ignore]`d e2e test `stopping_a_turn_cancels_the_in_flight_tool_call` still says in its comment that cancel is "still covered end-to-end by that unit test" — **that is now false**. Either restore an equivalent test against `cancel_run()` or fix the comment; do not read the ignore marker as "already covered".
- The new session tests are **pure fold tests**. `drain()`, `cancel_run()` and `PrepareOffload`'s refuse-if-running branch have no unit coverage; the supervisor tests cover the offload handshake only from the outside.
- `create_assembles_env_fresh_each_time` from the plan (Task 2, step 3) was never written — credential freshness on every create is currently unpinned, which is also what #96 is about.

## Traps found the hard way

- **The vendor layer changed under this design mid-flight** (upstream #87/#91/#93). Vendors are external agent processes now. The old fact "attach ≡ create because `AttachRuntime` calls `do_create`" is dead; attach's real semantics were "provision a fresh instance against the same spec", which is precisely what this design bans. Re-read `runtime_vendor.fl` before trusting any older note.
- **The fake agent's command loop is sequential**, so a blocked create genuinely blocks the socket read. That is why `get_during_an_in_flight_create_waits_for_it` passes against the fake without any locking — do not mistake it for evidence about the real agent.
- **`FakeRuntimeVendor` must answer `ScanWorkspace` and `SessionStart`** or session provisioning hangs with zero output. This is already handled in `fake.rs`; do not "simplify" it away.
- **A session's status cache is seeded at `Create`** (`self.status.insert(id, Idle)`), which is why the e2e helper can wait for `Idle` immediately after creating. After a restart the same session reports `Unknown` — that asymmetry is intended, not a bug.
- **`cargo fmt` prints a wall of nightly-only warnings** on this repo. Ignore them; they are not failures.
