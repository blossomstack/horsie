# In-server runtime vendors, and a Fly Machines provider

Closes the implementation half of #191, and folds in #234 (naming).

Diagrams:
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

`crates/server/src/runtime_vendor/mod.rs` records that in-process vendors were
deleted once as "pure indirection". They were, when every vendor was a socket.
A second implementation that is not a socket is what makes the seam earn its
keep.

## Decisions

**D1. Both cloud vendors and velos move in-server.** The `velos-runtime` binary
is deleted at the end of this work. One provisioning path, not two.

**D2. Fly Machines is the first cloud provider.** Plain REST, full OCI, named
machines, no session cap, and volumes — the last of which is the only reason
`HibernateRuntime` can stop being a no-op.

**D3. Everything ships in this repo.** The trait, the authenticated listener and
the Fly provider are all a capability an operator can use: point horsie at a Fly
account and it provisions sandboxes.

**D4. No event sourcing for vendor state.** See below.

**D5. Hibernate is real, and suspend is supported.** Volumes plus a reconnect
loop in the runtime binary, so both `stop`/`start` and `suspend`/`resume` work.

## Architecture

### One trait where there is now one concrete type

```rust
pub trait Vendor: Send + Sync {
    fn capabilities(&self) -> VendorCapabilities;
    fn is_connected(&self) -> bool;
    async fn create(&self, runtime_id: &str, spec: &RuntimeSpec) -> Result<(), VendorError>;
    async fn get(&self, runtime_id: &str) -> Result<(), VendorError>;
    async fn hibernate(&self, runtime_id: &str);
    async fn delete(&self, runtime_id: &str);
    async fn relay(&self, runtime_id: &str, msg: RuntimeInboundMessage)
        -> Result<RuntimeOutboundMessage, TransportError>;
    async fn relay_oneway(&self, runtime_id: &str, msg: RuntimeInboundMessage)
        -> Result<(), TransportError>;
}
```

`SharedVendors` becomes `Arc<RwLock<HashMap<String, Arc<dyn Vendor>>>>`. This is
exactly the surface `RuntimeManager` and `RuntimeVendorTransport` already use,
so neither changes: the transport keeps resolving through the map on every call,
which is what makes a reconnect invisible to a turn in flight (#187).

`RuntimeVendorLink` implements the trait unchanged.

### `VendorCore`, split out of `vendor.rs`

`crates/runtime-vendor/src/vendor.rs` is 1514 lines doing two jobs: a WebSocket
loop to the server, and a command handler that drives a `RuntimeProvider` and a
`ConnectedRuntimeRegistry`. Only the second is reusable. After the split:

- `horsie connect` keeps the loop, which calls `VendorCore`.
- `InProcessVendor` is `VendorCore` behind the `Vendor` trait.

Both drive the same `RuntimeProvider` and `WorkspaceResolver` traits, so porting
velos is moving a file, not rewriting one.

### The runtime dials the server

An in-server vendor has no listener of its own, so `/api/runtime/connect` comes
back as a server route. One port, TLS and reverse proxies for free.

### Authenticating the dial-back

`crates/runtime-vendor/src/listener.rs` has no auth: a connection announces a
`runtime_id` and is registered as that runtime's transport, with a duplicate
check as the only guard. That is sound on a private container network and
nowhere else, and #191 called it the blocker for every provider but Fly.

The runtime gains `--connect-token`, presented as a bearer on the dial. The
token is **derived, not stored**:

```
token = HMAC-SHA256(dial_secret, runtime_id)
```

`dial_secret` is generated once and kept in settings. Deriving rather than
storing means there is no per-runtime row to migrate, nothing to expire, a
server restart changes nothing, and rotating one secret invalidates every
outstanding token at once. The same check is added to the standalone listener,
so external vendors stop being open too.

A token authorizes exactly one `runtime_id` and does not expire. That is
acceptable because holding it is already equivalent to being that runtime; the
property being bought is that a *stranger* cannot become one.

### Vendors are built just-in-time from configuration

A `vendors` table, per user, in the shape `providers` already uses: name, kind,
credential, and kind-specific settings. When a user's services open, each row
becomes an `InProcessVendor` published into `SharedVendors`; saving settings
rebuilds the affected entry. External vendors keep publishing themselves on
connect, and the two populate the same map.

One setting has a failure mode worth surfacing early: the vendor must be told
the server's **externally reachable** URL, because that string ends up in the
machine's argv. A deployment reachable only on localhost cannot use a cloud
provider at all, and should be told so when the setting is saved rather than at
first session.

## Why there is no journal

A vendor holds three things, and none of them wants event sourcing:

- **`runtime_id` → machine.** Not state. The machine is *named*
  `horsie-{runtime_id}`, so this is a query. A journal here can disagree with
  the provider; a query cannot.
- **Live runtime sockets.** Ephemeral by construction — a journal of them is
  wrong the moment the process restarts.
- **Configuration.** Durable, but it is a settings row.

The one thing that genuinely had to survive a restart was the dial-back
credential, and deriving it removes even that. `VendorCore` already serializes
concurrent lifecycle calls through its handle map, which is the only thing an
actor would have added.

## The Fly provider

`FlyProvider` implements the existing `RuntimeProvider` trait: a `reqwest`
client and nothing more.

- **Create.** `POST /v1/apps/{app}/volumes`, then
  `POST /v1/apps/{app}/machines` with `name = horsie-{runtime_id}`,
  `config.init.exec` set to the `/bin/sh -c "mkdir -p … && exec horsie-runtime …"`
  line `build_container_command` already emits, `config.env` for the provision
  environment, and `config.mounts` binding the volume at the workspace root.
  The readiness waiter is registered *before* launching — the race documented in
  velos's provider is identical here.
- **Get.** Look the machine up by name; `start` it if stopped, `resume` if
  suspended.
- **Hibernate.** `stop` or `suspend`.
- **Delete.** Destroy the machine, then the volume.
- **Health.** Is the transport still registered.

`ManagedWorkspaces` is reused verbatim. Auth is one bearer header.

Volumes must be attached at machine-create — Fly rejects adding one afterwards —
and are pinned to a host, so create is two calls in a fixed order and delete has
to clean up both. The orphan case is already filed as #243 and this makes it
concrete rather than theoretical.

## Hibernate, and why the runtime needs a reconnect loop

`crates/runtime/src/main.rs` connects once with a startup retry budget, then
enters `run_loop`; when the socket dies the process exits. There is no
reconnect.

That rules out `suspend` as-is: a resumed machine wakes holding a dead TCP
connection, notices, and dies. It leaves `stop`/`start` working — the machine
reboots and a fresh `horsie-runtime` dials in — but only if the workspace
survives, which is what the volume is for. `persist_rootfs` is not a substitute:
Fly documents it as restart/update persistence and explicitly not a place for
data that matters.

So the work is both:

1. **Volume + `stop`/`start`**, which needs no runtime change and makes
   `with_respawnable_runtimes(true)` honest.
2. **A reconnect loop in the runtime**, which then makes `suspend`/`resume`
   available as the faster path.

The reconnect loop has to handle a resume that lands mid-turn. The runtime
re-dials under the same `runtime_id`; the server must replace the registered
transport rather than refuse it as a duplicate, and a tool call that was in
flight when the machine froze has to fail cleanly rather than hang the turn.
This is the same class of bug #187 fixed on the vendor link, one layer down.

## Phasing

Each step is a PR, in order.

1. **`Vendor` trait**, `SharedVendors` retyped, `RuntimeVendorLink` implements
   it. Pure refactor. Folds in #234's rename, since it touches those names
   anyway.
2. **Authenticated dial-back.** `--connect-token` on the runtime, HMAC verify on
   the standalone listener. Closes #191's blocker on its own, before any
   in-server vendor exists.
3. **`VendorCore` split, `/api/runtime/connect`, `InProcessVendor`,
   `vendors` table, settings UI.** The largest step.
4. **`FlyProvider`**, with volumes and `stop`/`start` hibernate.
5. **Runtime reconnect loop**, enabling `suspend`/`resume`.
6. **velos ported in-server**; `crates/velos-runtime` deleted.

## Testing

`crates/runtime-vendor/tests/vendor_conformance.rs` is the real contract and
currently runs against a vendor *process*. Re-pointing it at the `Vendor` trait
so both implementations run the identical suite is the main test-side work, and
it is what makes step 6 safe: velos moving in-process is a pass/fail against a
suite it already passes.

Beyond that:

- `FlyProvider` against a faked `ContainerApi`-shaped trait, the way velos's
  client is already structured.
- The dial-back check: a correct token registers, a token for another
  `runtime_id` is refused, a missing token is refused.
- Reconnect: a runtime that re-dials replaces its transport; an in-flight call
  at freeze time fails rather than hangs.
- Settings: saving a vendor publishes it into the map without a restart;
  deleting one removes it.

## Risks and open questions

- **Volume naming.** Fly volume names are more constrained than machine names.
  If a UUID-derived name does not fit, the volume is still discoverable through
  the machine's `mounts`, so this costs a lookup rather than a stored mapping —
  but it needs confirming against the API before step 4.
- **Host affinity.** A volume pins its machine to a host. Capacity failures in a
  region become a create-time error that must surface as
  `VendorError::Provision`, not a hang.
- **Cost.** Stopped machines are billed for rootfs, and volumes at $0.15/GB-mo.
  A suspended or stopped runtime is cheaper than a running one, not free.
- **Step 3 is large.** If it needs splitting, the natural cut is the route and
  `InProcessVendor` first with a hard-coded test provider, then configuration
  and UI.
