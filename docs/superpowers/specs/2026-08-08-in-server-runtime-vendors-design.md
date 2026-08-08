# In-server runtime vendors, and a Fly Machines vendor

Closes the implementation half of #191. Folds in #234 (naming) and #243
(orphaned runtime state).

Diagrams — **both predate the naming and ownership decisions below and are kept
only for the shape of the flow**:
[components](https://excalidraw.com/#json=7KcJeXYoO05ZGIh-_veHq,8b__XasgnlnEJeMTKtWNfA),
[sequence](https://excalidraw.com/#json=sm6DoGp01tCrbIX7W0OXD,V-t0R_UqVEWhqT_JHeAxSw).

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

## Naming

Nothing in this area is called just "vendor". Every type carries `Runtime` or
`RuntimeVendor`, and the existing `Runtime`-prefixed names are already correct
and stay.

| Now | After |
| --- | --- |
| `RuntimeVendorLink` | `RemoteRuntimeVendor` |
| `SharedVendors` | `RuntimeVendorMap` |
| `VendorError` | `RuntimeVendorError` |
| `VendorCapabilities` (server) | deleted — use the wire `RuntimeVendorCapabilities` |
| `vendor_agents` field (#234) | `runtime_vendors` |
| — | `InProcessRuntimeVendor<P: RuntimeProvider>` |
| — | `FlyRuntimeProvider`, `VelosRuntimeProvider` |

`RuntimeProvider` keeps its name: it provisions *runtimes*, not vendors, and is
the thing a vendor drives. `RuntimeManager` keeps its name and its job.

"Shared" is dropped from the map alias deliberately — since #233 there is one
per account, so a name saying "Shared" reads as "the deployment-wide one",
which is the opposite of what it is, on the type where being wrong means running
tool calls on someone else's machine.

## One trait, several runtime vendors

```rust
#[async_trait]
pub trait RuntimeVendor: Send + Sync {
    fn capabilities(&self) -> RuntimeVendorCapabilities;
    fn is_connected(&self) -> bool;
    async fn create(&self, runtime_id: &str, spec: &RuntimeSpec) -> Result<(), RuntimeVendorError>;
    async fn get(&self, runtime_id: &str, spec: &RuntimeSpec) -> Result<(), RuntimeVendorError>;
    async fn hibernate(&self, runtime_id: &str);
    async fn delete(&self, runtime_id: &str);
    async fn relay(&self, runtime_id: &str, msg: RuntimeInboundMessage)
        -> Result<RuntimeOutboundMessage, TransportError>;
    async fn relay_oneway(&self, runtime_id: &str, msg: RuntimeInboundMessage)
        -> Result<(), TransportError>;
}
```

Implementations:

- **`RemoteRuntimeVendor`** — the WebSocket link to a `horsie connect` process.
  Today's `RuntimeVendorLink`, renamed and given the trait.
- **`InProcessRuntimeVendor<P: RuntimeProvider>`** — one generic type, not a
  layer. `FlyRuntimeVendor` and `VelosRuntimeVendor` are aliases over
  `FlyRuntimeProvider` and `VelosRuntimeProvider`.

`RuntimeVendorMap` becomes `Arc<RwLock<HashMap<String, Arc<dyn RuntimeVendor>>>>`.
That is exactly the surface `RuntimeManager` and `RuntimeVendorTransport`
already use, so neither changes: the transport keeps resolving through the map
on every call, which is what makes a reconnect invisible to a turn already in
flight (#187).

`crates/server/src/runtime_vendor/mod.rs` records that in-process vendors were
deleted once as "pure indirection". They were, when every vendor was a socket.
A second implementation that is not a socket is what makes the seam earn its
keep.

## What this deletes, and why

The vendor process today keeps two pieces of bookkeeping that exist **only
because it runs in a different process from the server**. In-server, both are
duplicates of state the server already has.

**`lifecycle_locks` (`vendor.rs:234`) is deleted.** It exists so a `GetRuntime`
arriving mid-`CreateRuntime` waits instead of answering "gone". But
`runtime_id` **is** the session id, and since #232/#235 `SessionActor` owns
provisioning and journals it — `session_actor/types.rs:61` notes that
provisioning stays exactly-once "without any bookkeeping beyond the status the
journal already carries". A per-runtime actor and the session actor are the same
actor. The vendor's locks re-derive an ordering the session actor already
guarantees.

**`<state_dir>/<runtime_id>/spec.json` is deleted.** It is the only durable
thing a vendor keeps, written before spawning so a runtime that dies during
provisioning is still rebuildable. But `SessionSpec` already persists `vendor`,
`workspaces` and `provision` (`spec.rs:109`), and `SessionStatus` carries
`Provisioning` / `ProvisioningFailed` / `Unrecoverable` in the session journal.
The file duplicates data the server already holds durably; it exists only
because a separate process cannot read the server's database.

So **`get` takes the spec** rather than the vendor recalling it. `respawnable`
stops being a reason to write a file and becomes purely a capability: can this
vendor rebuild a runtime that is not live. This also removes the local-disk
record whose leak is #243.

**No new actor, and no second journal.** The desired state is already
event-sourced, in the session journal, keyed by the same id. A runtime actor
would be a second writer for one fact, and drift between two journals is worse
than either alone. What such an actor would genuinely have bought —
reconciling machines whose session no longer exists — is a periodic sweep, not
durable state; see Orphans below.

## Who holds a runtime's connection

An in-server vendor has no listener of its own, so `/api/runtime/connect` comes
back as a server route: one port, TLS and reverse proxies for free.

`ConnectedRuntimeRegistry` keeps its name and its behaviour, and moves from the
vendor process into `UserServices` beside the vendor map — **one per account**,
not one per vendor and not one per server.

1. `create` registers a waiter for `runtime_id` *before* launching. The race
   this closes is the one velos's provider already documents.
2. The machine boots and dials `GET /api/runtime/connect`.
3. The route authenticates the bearer, which is **self-describing**:
   `{user_id, runtime_id}` plus an HMAC tag over both. No database read is
   needed to know where the socket belongs. A sandbox learning its own user id
   is not a disclosure — it is that user's own sandbox.
4. The route resolves `UserServices` through `UserRegistry::get(user_id)`,
   upgrades the socket, and registers the transport in that account's registry.
5. The waiting vendor wakes and `create` returns.

Per-account rather than server-wide keeps this inside the scoping discipline of
#217/#233, and costs nothing because the token already carries the user.

**`LiveRuntime` stops caching the transport.** It holds `handle + transport`
today. Once the runtime can reconnect (step 6), a re-dial replaces the transport
in the registry and a vendor still holding the old `Arc` would send into a dead
socket for the rest of the turn — #187 exactly, one layer down. `LiveRuntime`
keeps only the `handle`; the transport is resolved from the registry per call,
mirroring what `RuntimeVendorTransport` already does with the vendor link.

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
The same check is added to the standalone listener, so external vendors stop
being open too.

A token authorizes exactly one `runtime_id` and does not expire. That is
acceptable because holding it is already equivalent to being that runtime; the
property being bought is that a *stranger* cannot become one.

## Runtime vendors are built just-in-time from configuration

A `runtime_vendors` table, per user, in the shape `providers` already uses:
name, kind, credential, and kind-specific settings. When an account's services
open, each row becomes an `InProcessRuntimeVendor` published into the vendor
map; saving settings rebuilds the affected entry. Remote vendors keep
publishing themselves on connect, and both populate the same map.

One setting has a failure mode worth surfacing early: the vendor must be told
the server's **externally reachable** URL, because that string ends up in the
machine's argv. A deployment reachable only on localhost cannot use a cloud
provider at all, and should be told so when the setting is saved rather than at
first session.

## The Fly runtime vendor

`FlyRuntimeProvider` implements the existing `RuntimeProvider` trait: a
`reqwest` client and nothing more.

- **Create.** `POST /v1/apps/{app}/volumes`, then
  `POST /v1/apps/{app}/machines` with `name = horsie-{runtime_id}`,
  `config.init.exec` set to the `/bin/sh -c "mkdir -p … && exec horsie-runtime …"`
  line `build_container_command` already emits, `config.env` for the provision
  environment, and `config.mounts` binding the volume at the workspace root.
- **Get.** Look the machine up by name; `start` it if stopped, `resume` if
  suspended.
- **Hibernate.** `stop`, or `suspend` once the runtime can reconnect.
- **Delete.** Destroy the machine, then the volume.
- **Health.** Is the transport still registered.

`ManagedWorkspaces` is reused verbatim. Auth is one bearer header.

Volumes must be attached at machine-create — Fly rejects adding one afterwards —
and are pinned to a host, so create is two calls in a fixed order and delete has
to clean up both.

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

So the work is both:

1. **Volume + `stop`/`start`**, needing no runtime change, which makes
   `with_respawnable_runtimes(true)` honest for the first time.
2. **A reconnect loop in the runtime**, which then makes `suspend`/`resume`
   available as the faster path.

The reconnect loop must handle a resume landing mid-turn: the runtime re-dials
under the same `runtime_id`, the registry replaces the transport rather than
refusing a duplicate, and a tool call in flight when the machine froze fails
cleanly instead of hanging the turn.

## Orphans (#243)

A machine whose session no longer exists is found by listing `horsie-*` on the
provider and comparing against the sessions table — a periodic sweep per
configured vendor. Deleting a session already tells its vendor; the sweep exists
for the case where the vendor was unreachable at the time.

## Phasing

Each step is a PR, in order.

1. **`RuntimeVendor` trait**, `RuntimeVendorMap`, `RemoteRuntimeVendor`, and the
   rename pass. Pure refactor; closes #234.
2. **Authenticated dial-back.** `--connect-token` on the runtime, HMAC verify on
   the standalone listener. Closes #191's blocker on its own.
3. **`get` carries the spec**; delete `spec.json` and `lifecycle_locks`. Wire
   change to `GetRuntimeRequest`.
4. **`/api/runtime/connect`, `ConnectedRuntimeRegistry` into `UserServices`,
   `InProcessRuntimeVendor`, `runtime_vendors` table, settings UI.**
5. **`FlyRuntimeProvider`**, with volumes and `stop`/`start` hibernate.
6. **Runtime reconnect loop**, enabling `suspend`/`resume`.
7. **`VelosRuntimeProvider` in-server**; `crates/velos-runtime` deleted.
8. **Orphan sweep**; closes #243.

The implementation plan covers steps 1–4. Steps 5–8 get their own plan once the
seam has actually run.

## Testing

`crates/runtime-vendor/tests/vendor_conformance.rs` is the real contract and
currently runs against a vendor *process*. Re-pointing it at the `RuntimeVendor`
trait so every implementation runs the identical suite is the main test-side
work, and it is what makes step 7 safe: velos moving in-process becomes
pass/fail against a suite it already passes.

Beyond that:

- `FlyRuntimeProvider` against a faked `ContainerApi`-shaped trait, the way
  velos's client is already structured.
- The dial-back check: a correct token registers; a token for another
  `runtime_id` or another `user_id` is refused; a missing token is refused.
- Reconnect: a re-dial replaces the transport; an in-flight call at freeze time
  fails rather than hangs.
- Settings: saving a vendor publishes it into the map with no restart; deleting
  one removes it.
- Scope: two accounts with same-named vendors do not see each other's runtimes
  (the isolation harness, per #223).

## Risks and open questions

- **Volume naming.** Fly volume names are more constrained than machine names.
  If a UUID-derived name does not fit, the volume is still reachable through the
  machine's `mounts`, so this costs a lookup rather than a stored mapping — but
  it needs confirming against the API before step 5.
- **Host affinity.** A volume pins its machine to a host. A regional capacity
  failure must surface as `RuntimeVendorError::Provision`, not a hang.
- **Cost.** Stopped machines are billed for rootfs, volumes at $0.15/GB-mo. A
  stopped or suspended runtime is cheaper than a running one, not free.
- **Step 3 changes the wire.** `GetRuntimeRequest` gains the spec, so a
  `horsie connect` older than the server cannot respawn. Acceptable: version
  skew between the two is already not supported.
- **Step 4 is large.** If it needs splitting, the natural cut is the route and
  `InProcessRuntimeVendor` first against a test provider, then configuration and
  UI.
