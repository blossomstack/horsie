# In-server runtime vendors, and a Fly Machines vendor

Closes the implementation half of #191. Folds in #234 (naming) and #243
(orphaned runtime state).

Diagrams:
[components](https://excalidraw.com/#json=S2_1fgFxP6pcm1U46qdAv,CgiZki3IyWApAFD1aiqrhw),
[create / relay / hibernate / resume](https://excalidraw.com/#json=LNJF4g1GQEMuqZu4meioe,cm13NsRuFID-MCFF3v4N-A).
Both predate the `RuntimeProgress` model below; the component boxes still hold,
the sequence's single-await create does not.

## The problem

Every runtime vendor today is an external process. `horsie connect` dials
`/api/vendor/connect`, announces a name, and thereafter owns runtime lifecycle;
the server's only handle on it is `RuntimeVendorLink`, and the vendor map starts
empty at boot and is never repopulated from the database
(`crates/server/src/config/store.rs`).

That shape is right for a vendor whose work is inherently local — the local
daemon spawns processes on the operator's own machine, and no server can do that
for them. It is wrong for a vendor whose entire job is calling a REST API. A
provider like Fly Machines needs an API token, a region and an image; asking an
operator to run a second process whose only purpose is to hold those three
values and forward JSON is friction with nothing behind it. The operator should
paste the token into settings and get sandboxes.

## Design intent

**`RuntimeVendor` and `RuntimeHandle` stay small and substrate-agnostic, so
vendor number four costs no API change.** This is the governing constraint, and
it outranks local convenience everywhere below.

Two rules follow from it:

- **A capability difference between substrates belongs inside an
  implementation, never in the trait.** Fly can only be polled; E2B pushes
  lifecycle webhooks. Modelling that as an optional `events()` stream — or as a
  `poll` method — bakes today's two substrates into the contract and breaks on
  the first one that streams over SSE or gRPC. A vendor reports progress on the
  sink however it learned of it, and the trait never learns which.
- **Adding a default-implemented method later is non-breaking; changing a
  signature is.** So ship the minimum that serves the three known vendors and
  let the fourth be additive.

Applying this deleted three members earlier drafts had: `events()`, `poll`,
and `RuntimeHandle::health_check` — the last derivable from `closed()`, since
today's implementation is literally "is the transport still registered".

## The two traits

```rust
/// Where a runtime is, as its vendor currently understands it.
pub enum RuntimeProgress {
    Requested,
    Starting { detail: String },
    Provisioning { detail: String },
    Ready(Arc<dyn RuntimeHandle>),
    Stopping,
    Stopped,
    Gone { reason: String },
}

/// One report, stamped with the runtime it concerns, so an account needs one
/// sink rather than a channel per call.
pub struct RuntimeEvent { pub runtime_id: String, pub progress: RuntimeProgress }

/// A plain channel, not another trait. `try_send`, and dropping on a full
/// channel is correct: a lagging consumer is not a failed runtime.
pub type RuntimeProgressSink = tokio::sync::mpsc::Sender<RuntimeEvent>;

#[async_trait]
pub trait RuntimeVendor: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> RuntimeVendorCapabilities;

    async fn create(&self, id: &str, spec: &RuntimeSpec, progress: RuntimeProgressSink)
        -> Result<RuntimeProgress, RuntimeVendorError>;
    async fn get(&self, id: &str, spec: &RuntimeSpec, progress: RuntimeProgressSink)
        -> Result<RuntimeProgress, RuntimeVendorError>;
    async fn hibernate(&self, id: &str, progress: RuntimeProgressSink)
        -> Result<RuntimeProgress, RuntimeVendorError>;
    async fn delete(&self, id: &str, progress: RuntimeProgressSink)
        -> Result<RuntimeProgress, RuntimeVendorError>;
}

/// A live runtime. Every member is the runtime protocol, which is why a handle
/// looks the same whatever substrate is underneath.
#[async_trait]
pub trait RuntimeHandle: Send + Sync + Debug {
    fn id(&self) -> &str;
    async fn relay(&self, msg: RuntimeInboundMessage)
        -> Result<RuntimeOutboundMessage, TransportError>;
    async fn relay_oneway(&self, msg: RuntimeInboundMessage) -> Result<(), TransportError>;
    async fn closed(&self);
}
```

Both traits live in `crates/runtime-vendor`, which the server already depends
on, so one contract serves both sides of the wire.

Implementations are named for what they talk to, so a fourth slots in without
a naming argument: `WebsocketRuntimeVendor` (a `horsie connect` process),
`FlyRuntimeVendor`, later `E2bRuntimeVendor`.

**There is one handle implementation**, `RuntimeHandleImpl`, holding a
`runtime_id`, an `Arc<dyn RuntimeTransport>` and a closed-signal. The
polymorphism lives in the transport, which already exists: the websocket vendor
supplies `RuntimeVendorTransport` (relays through the vendor link, resolving it
per call — the #187 fix), and every in-server vendor supplies the
`SocketRuntimeTransport` registered when its runtime dialled
`/api/runtime/connect`. Two handle types would have re-derived a split
`RuntimeTransport` already makes.

**Deleted by this design:** `RuntimeProvider` and `RuntimeHandle::stop` (merged
into `RuntimeVendor`; stopping is a vendor operation keyed by id, and `stop` was
ambiguous against `hibernate`/`delete`), the `VendorCore`/`InProcessRuntimeVendor<P>` layer, and
the server's duplicate `VendorCapabilities`. `RuntimeVendorTransport` survives —
it stops being a server-wide seam and becomes one vendor's transport adapter,
which is what it always was.

## Every operation returns its first observation

A single awaitable `create() -> Handle` hides four phases, only the last two of
which are ours: the substrate accepts the request, the substrate's object
reaches a running state, `horsie-runtime` boots and dials back, and it finishes
provision steps.

The substrates disagree about phase 2 in a way the trait must not encode.
**Fly**: `POST /machines` returns while the machine is still `created`/
`starting`; `GET /v1/apps/{app}/machines/{id}/wait?state=started` is a long poll
that can itself time out; there are **no webhooks** outside a partner
programme. **E2B**: has real lifecycle webhooks
(`sandbox.lifecycle.created/updated/paused/resumed/killed`).

So progress reporting is the vendor's own business. An earlier draft put a
`poll` method on the trait, which forced *polling* into the contract — the exact
mistake the design intent exists to prevent. Instead every operation returns the
**first observation** and anything later arrives on the sink. A vendor that
already knows the answer — a `horsie connect` process only replies once its
runtime is up — returns `Ready` and never touches the sink. A vendor whose
substrate needs minutes returns `Starting` and finishes in the background.
Neither is forced to hold a long await, so an interrupted operation leaves no
orphaned future.

**The ordering rule that makes this safe: an implementation must not emit on the
sink for an operation before that operation has returned.** Build the return
value, then start the background work. Without it a caller could observe `Ready`
before the `Starting` it was handed, and would need reconciliation logic; with
it the return value is simply the first event, one reducer handles both, and
"latest event wins per runtime" is well defined.

## RuntimeManager stays a plain struct — no new actor

`RuntimeManager` is already per-account (`users.rs:163`). It absorbs
`ConnectedRuntimeRegistry` and owns:

- the vendor map,
- `HashMap<runtime_id, Arc<dyn RuntimeHandle>>`, fed by one reducer draining the
  sink,
- the dial-back landing point for `/api/runtime/connect`.

An earlier draft made it an actor. That actor existed to own per-vendor poll
loops; the sink removed the loops, so it is a plain struct with a lock and a
broadcast sender. `SessionActor` already serialises per runtime, and progress
events reach the UI on the account's existing broadcast channel while
`SessionActor` still learns its terminal outcome through `FinishProvisioning`.

**It journals nothing.** The durable record of intent is already the session
journal: `SessionSpec` persists `vendor`, `workspaces` and `provision`
(`spec.rs:109`), `SessionStatus` carries `Provisioning`/`ProvisioningFailed`/
`Unrecoverable`, and `SessionActor` re-sends `Provision` on load to recover a
create the process died inside (`session_actor/types.rs:57-63`). Because
`create` is idempotent against the deterministic name `horsie-{runtime_id}`, the
manager can hold zero durable state and still recover by re-entering
`create`/`get`. Two journals for one fact is the thing to avoid.

This also retires `lifecycle_locks` (`vendor.rs:234`): `runtime_id` **is** the
session id, so `SessionActor` already serialises per runtime, and the actor
serialises the rest.

## Lifecycle events, which nothing handles today

`RuntimeStateChanged` currently arrives on the vendor link, gets a
`tracing::debug!`, and is dropped — it matches no waiter, so it falls off the
end of the read loop (`link.rs:170`). This design closes that hole rather than
inheriting it.

Three sources converge on one signal. A remote vendor's `RuntimeStateChanged`
closes its handle; an in-process runtime's WebSocket closing closes its handle;
a substrate that reports a dead machine closes it on its next report. All three
resolve `RuntimeHandle::closed()`, the manager drops the map entry, and
discovery of *what to do next* stays lazy — the next acquisition re-enters
`get`. What `closed()` buys is that a dead handle is never handed to a turn.

## Acquiring a runtime after a failure

After a horsie restart every runtime is dead but its machine still exists,
because the runtime has no reconnect loop and its socket died with the server.
So acquisition always ends in the same place — waiting for a dial-back — and
differs only in what the substrate had to do first.

| situation | what happens | outcome |
| --- | --- | --- |
| handle live in the map | return it | — |
| server restarted, machine exists | start it, await dial-back | — |
| hibernated (stopped or suspended) | start/resume, await dial-back | — |
| substrate API unreachable | — | `Unavailable`, retryable |
| machine destroyed | — | `Gone` → session `Unrecoverable` |

`create` stays distinct from `get` because "no runtime exists and I must not
build one" is a real safety property: an acquisition that silently provisions
rebuilds a workspace the user believes still holds their work.

## Who holds a runtime's connection

An in-server vendor has no listener of its own, so `/api/runtime/connect` comes
back as a server route: one port, TLS and reverse proxies for free.

1. `create` registers the manager's expectation for `runtime_id` *before* asking
   the substrate. The race this closes is the one velos's provider documents.
2. The machine boots and dials `GET /api/runtime/connect`.
3. The route authenticates the bearer, which is **self-describing**:
   `{user_id, runtime_id}` plus an HMAC tag over both, parsed from the right so
   a dotted account or vendor name is not a malformed token. A sandbox learning
   its own account id is not a disclosure — it is that account's own sandbox.
4. The account is a *claim* until the tag checks out, so the only thing done
   with it first is a bare settings read for that account's dial secret —
   never `UserRegistry::get`, which builds the account when it is absent and
   would let a stranger's token spawn a supervisor per request. Once the tag
   verifies, the route resolves the account, upgrades the socket, and hands it
   to that account's registry.
5. The route sits **outside** `require_auth`'s credential check. A runtime holds
   no session credential; the dial token is the whole authentication, and the
   middleware would 401 it on any deployment with authentication enabled.

Per-account rather than server-wide keeps this inside the scoping discipline of
#217/#233, and costs nothing because the token already carries the account.

**A handle is never cached anywhere but the manager's map.** A reconnect
replaces it; anything holding the old `Arc` would write into a dead socket for
the rest of the turn — #187, one layer down.

## Authenticating the dial-back

`crates/runtime-vendor/src/listener.rs` has no auth: a connection announces a
`runtime_id` and is registered as that runtime's transport, with a duplicate
check as the only guard. That is sound on a private container network and
nowhere else, and #191 named it the blocker for every provider but Fly.

The runtime gains `--connect-token`. The token is **derived, not stored**:

```
payload = user_id || runtime_id
token   = payload || HMAC-SHA256(dial_secret, payload)
```

`dial_secret` is generated once and kept in settings. Deriving rather than
storing means no per-runtime row to migrate, nothing to expire, a server restart
changes nothing, and rotating one secret invalidates every outstanding token.
The same check goes on the standalone listener, so `horsie connect` stops being
open too.

A token authorises exactly one `runtime_id` and does not expire. That is
acceptable because holding it is already equivalent to being that runtime; the
property being bought is that a *stranger* cannot become one.

## Naming

Nothing here is called bare "Vendor". Every type carries `Runtime` or
`RuntimeVendor`.

| Now | After |
| --- | --- |
| `RuntimeVendorLink` (server) | `WebsocketRuntimeVendor` |
| `RuntimeVendor` (runtime-vendor crate — the process that dials a server) | `RuntimeVendorClient` |
| `SharedVendors` | `RuntimeVendorMap` |
| `VendorError` | `RuntimeVendorError` |
| `VendorCapabilities` (server) | deleted — use the wire `RuntimeVendorCapabilities` |
| `RuntimeProvider` | deleted — merged into `RuntimeVendor` |
| `vendor_agents` field (#234) | `runtime_vendors` |
| — | `FlyRuntimeVendor`, `VelosRuntimeVendor`, `LocalProcessRuntimeVendor` |

"Shared" is dropped from the map alias deliberately: since #233 there is one per
account, so a name saying "Shared" reads as deployment-wide — the opposite of
what it is, on the type where being wrong means running tool calls on someone
else's machine.

## Runtime vendors are built just-in-time from configuration

A `runtime_vendors` table, per account, in the shape `providers` already uses:
name, kind, credential, kind-specific settings. When an account's services open,
each row becomes a vendor published into the map; saving settings rebuilds the
affected entry. Remote vendors keep publishing themselves on connect, and both
populate the same map.

**Reconfiguration replaces the object rather than mutating it** — the same
semantics a `horsie connect` reconnect already has, where the returning process
arrives as a brand-new link. Consequences: live handles survive (they are the
manager's, not the vendor's); the vendor's own caches are lost, which is
identical to a restart; and edits that change *substrate identity* (kind, or a
credential pointing at a different Fly org) make `horsie-{runtime_id}`
unfindable, so those runtimes answer `Gone`. Identity-class edits are allowed
but warn at save time, naming how many sessions reference that vendor —
blocking would only push an operator rotating a leaked token into
delete-and-re-add, which orphans the same runtimes with less warning.

One setting has a failure mode worth surfacing early: the vendor must be told
the server's **externally reachable** URL, because that string ends up in the
machine's argv. A deployment reachable only on localhost cannot use a cloud
substrate at all, and should be told so when the setting is saved rather than at
first session.

## The Fly vendor

`FlyRuntimeVendor` is a `reqwest` client and a `RuntimeVendor` impl.

- **create.** `POST /v1/apps/{app}/volumes`, then `POST /v1/apps/{app}/machines`
  with `name = horsie-{runtime_id}`, `config.init.exec` set to the
  `/bin/sh -c "mkdir -p … && exec horsie-runtime …"` line
  `build_container_command` already emits, `config.env` for the provision
  environment, and `config.mounts` binding the volume at the workspace root.
  Returns `Pending`.
- **progress.** A spawned task long-polls
  `GET /v1/apps/{app}/machines/{id}/wait?state=started`, then waits for the
  dial-back, emitting `Starting` → `Provisioning` → `Ready` on the sink. All of
  it inside the vendor; none of it in the trait.
- **hibernate.** `stop`, or `suspend` once the runtime can reconnect.
- **delete.** Destroy the machine, then the volume.

`ManagedWorkspaces` is reused verbatim. Auth is one bearer header. Volumes must
be attached at machine-create — Fly rejects adding one afterwards — and are
pinned to a host, so create is two calls in a fixed order and delete cleans up
both.

## Hibernate, and why the runtime needs a reconnect loop

`crates/runtime/src/main.rs:232` connects once with a startup retry budget, then
enters `run_loop`; when the socket dies the process exits. There is no
reconnect.

That rules out `suspend` as-is: a resumed machine wakes holding a dead TCP
connection, notices, and dies. It leaves `stop`/`start` working — the machine
reboots and a fresh `horsie-runtime` dials in — but only if the workspace
survives, which is what the volume is for. `persist_rootfs` is not a substitute:
Fly documents it as restart/update persistence and explicitly not a place for
data that matters.

So the work is both: volume plus `stop`/`start`, which needs no runtime change
and makes `supports_provisioning`-driven revival honest; then a reconnect loop
in the runtime, which makes `suspend`/`resume` available as the faster path. The
reconnect loop must handle a resume landing mid-turn — the runtime re-dials
under the same `runtime_id`, the manager replaces the handle rather than
refusing a duplicate, and a call in flight when the machine froze fails cleanly
instead of hanging the turn.

**A vendor that cannot suspend declines.** velos has no stop: its API is create
and delete, and deleting a container is not suspending it — it destroys the
workspace and everything in flight to free a slot on a worker. So velos leaves
the container running and pays the compute, which is the bargain the contract
already prefers. The consequence is that a velos container only ever disappears
because it died, so an acquisition that finds none answers `Gone` rather than
scheduling a replacement with an empty workspace.

## Orphans (#243)

A machine whose session no longer exists is found by listing `horsie-*` on the
substrate and comparing against the sessions table — one sweep per configured
vendor, in a single list call. Deleting a session already
tells its vendor; the sweep covers the case where the vendor was unreachable at
the time.

Volumes are swept the same way and separately, because Fly does not cascade: a
volume outlives the machine that mounted it, and one whose create half-failed
outlives a machine that never existed. A volume name is a group label rather
than an identifier, so the sweep compares against the names live runtimes *would*
use — a collision costs a leaked volume rather than a destroyed workspace.

One failure does not end a sweep. Everything it touches has been billing longer
than it should, and aborting on the first stuck object — discarding what had
already been reclaimed — let one undeletable machine keep every other orphan
alive indefinitely.

## Phasing

1. **The two traits**, in `crates/runtime-vendor`. `WebsocketRuntimeVendor`
   implements them over today's link, via one `RuntimeHandleImpl` over the
   existing `RuntimeVendorTransport`. `RuntimeProvider` is deleted. Naming pass
   (#234) rides along.
2. **`RuntimeManager` absorbs `ConnectedRuntimeRegistry`**, owns the live
   handles, and drains the progress sink into session status and the account's
   broadcast. `lifecycle_locks` deleted.
3. **Authenticated dial-back.** `--connect-token`, HMAC verify on both the
   standalone listener and the new `/api/runtime/connect` route.
4. **Acquisition carries its spec**; `spec.json` deleted. Wire change to
   `GetRuntimeRequest`.
5. **`runtime_vendors` table, JIT publication, settings UI.**
6. **`FlyRuntimeVendor`**, with volumes and `stop`/`start` hibernate.
7. **Runtime reconnect loop**, enabling `suspend`/`resume`.
8. **`VelosRuntimeVendor` in-server**; `crates/velos-runtime` deleted.
9. **Orphan sweep**; closes #243.

## Testing

`crates/runtime-vendor/tests/vendor_conformance.rs` is the real contract and
currently runs against a vendor *process*. Re-pointing it at the `RuntimeVendor`
trait so every implementation runs the identical suite is the main test-side
work, and it is what makes step 8 safe: velos moving in-process becomes
pass/fail against a suite it already passes.

Beyond that:

- `FlyRuntimeVendor` against a faked HTTP layer, the way velos's client is
  already structured.
- The dial-back check: a correct token registers; a token for another
  `runtime_id` or another `user_id` is refused; a missing token is refused.
- Progress: a vendor stuck in `Pending` past the provision window fails the
  acquisition rather than hanging it.
- Reconnect: a re-dial replaces the handle; an in-flight call at freeze time
  fails rather than hangs.
- Scope: two accounts with same-named vendors never see each other's runtimes
  (the isolation harness, per #223).

## Risks and open questions

- **Volume naming.** Fly volume names are more constrained than machine names.
  If a UUID-derived name does not fit, the volume is still reachable through the
  machine's `mounts`, so this costs a lookup rather than a stored mapping — but
  it needs confirming against the API before step 6.
- **Poll cadence.** Fly rate-limits per-machine polling. That bites when
  monitoring *every* runtime continuously, not on the handful of creates in
  flight at once — so it constrains the orphan sweep's list call, not `create`.
- **Host affinity.** A volume pins its machine to a host. A regional capacity
  failure must surface as `RuntimeVendorError::Provision`, not a hang.
- **Cost.** Stopped machines are billed for rootfs, volumes at $0.15/GB-mo. A
  stopped or suspended runtime is cheaper than a running one, not free.
- **Step 4 changes the wire.** `GetRuntimeRequest` gains the spec, so a
  `horsie connect` older than the server cannot revive a runtime. Acceptable:
  version skew between the two is already unsupported.
