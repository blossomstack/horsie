# Refuse a vendor name that is already in use

**Status:** designed, 2026-08-03

## The problem

`RuntimeVendorRegistry::register` refuses a name only when the incumbent link
belongs to a *different* principal. The same principal claiming a live name
silently replaces it, and nothing tells either side.

Two `horsie connect` processes on one machine, both defaulting to the same
`--runtime-id`, therefore fight over it. The newcomer displaces the incumbent;
dropping the incumbent's link closes its socket; its agent reads that as a
transport fault and re-dials a second later, displacing the newcomer in turn.
Neither process reports anything, and sessions bound to whichever agent lost the
last round address runtimes that agent no longer holds.

Three properties are missing, and each is independently a defect:

- **A registration has no acknowledgement.** `connect` = dial, send `Ready`,
  serve. A refusal is a dropped socket, indistinguishable from a network fault,
  so `RuntimeVendor::run` retries it forever (`runtime-vendor/src/vendor.rs`).
- **A name is not owned while it is in use.** Only cross-principal takeover is
  refused.
- **A disconnected vendor is never removed from the map.** Nothing removes an
  entry; `RuntimeVendorLink::is_connected()` flips, the entry stays. There is no
  heartbeat, so a half-open socket reads connected indefinitely.

## Decisions

1. **First live claim wins.** Any second *process* claiming a name in use is
   refused, whatever principal it presents.
2. **A process may reclaim its own name.** Each agent process generates one
   `instance_id` and presents it on every dial. Same instance = my own stale
   link, replace it. This is what keeps a reconnect after a network blip
   instant instead of costing an eviction window.
3. **`instance_id` is a registration detail and nothing else.** Sessions,
   settings, and every API address a vendor by *name*. The id never appears in a
   reference, a URL, or a stored field.
4. **The agent heartbeats.** The client pings; the server evicts on silence.
5. **No backward compatibility.** `instance_id` is a required field. An agent
   built before this change fails the handshake.

## Design

### Wire protocol (`models/fluorite/runtime_vendor.fl`)

`RuntimeVendorReady` gains a required `instance_id: String` — a UUID generated
once per agent process, stable across that process's reconnects.

Two server→vendor commands answer the handshake. Both echo the `request_id` the
agent put on its `Ready`, so they are ordinary correlated replies rather than a
special case in the envelope:

```
struct VendorRegistered {}
struct VendorRejected { reason: String }

union RuntimeVendorCommand {
    ...
    VendorRegistered(VendorRegistered),
    VendorRejected(VendorRejected),
}
```

The ack is worth having on its own: today an agent cannot distinguish "published
and idle" from "refused and dead".

### Registration gates (`server/src/runtime_vendor/registry.rs`)

`register` resolves in this order, under the map write lock:

| # | Condition | Outcome |
|---|---|---|
| 1 | no entry under this name | insert |
| 2 | entry, **different principal** | refuse |
| 3 | entry, same principal, **same instance id** | replace |
| 4 | entry, same principal, different instance, `!is_connected()` | replace |
| 5 | otherwise | refuse |

Gate 2 stays first and outranks the instance id. An instance id is announced by
the client and is not a secret; matching one must never buy a stranger a name
that belongs to someone else.

Gate 4 closes the race between a read loop ending and the eviction task removing
the entry — a corpse must not hold a name.

`RegisterError::NameTaken` keeps the incumbent's owner for the server log. The
reason string sent to the dialer does **not** include it: a refused stranger
learns only that the name is in use.

### Heartbeat and eviction

The agent sends a WebSocket **Ping every 15s**. The server's read loop already
tolerates ping and pong frames; it now reads under a **45s timeout** and treats
expiry as death. Any inbound frame counts as liveness, so a busy link never
depends on the ping arriving on time.

The agent does not wait for a pong. The ping exists to be observed, not
answered, which keeps the agent's own liveness detection where it already is:
socket errors.

When the read loop ends — hangup, error, or idle timeout — the entry is removed
from the vendor map, **compare-and-remove by instance id** so a link that was
already replaced cannot evict its successor. The registry does this in a task
awaiting a `closed()` signal on the link, so the link holds no reference to the
registry.

Consequence to expect: a disconnected vendor now disappears from `GET
/api/config` instead of lingering as `active: false`. That matches what the ops
RUNBOOK already documents ("reports whoever is connected right now"), and
changes Settings → Runtimes from listing a dead vendor to omitting it.

### Agent and CLI

`VendorRejected` is terminal in `RuntimeVendor::run`, in the same shape as
`CredentialError::Dead`: no backoff, no retry, `Err` out of the loop. `horsie
connect` prints the reason and exits non-zero, naming `--runtime-id` as the way
to pick a different name.

A rejection can only arrive at registration, so an agent that has been serving
never sees one mid-life.

## Testing

- **Registry unit tests** — one per gate, plus: eviction frees the name; a
  replaced link cannot evict its successor.
- **Link unit test** — the read loop ends when no frame arrives within the idle
  timeout (timeout injected, not slept through).
- **Agent unit test** — `run()` returns `Err` on `VendorRejected` without
  consulting the backoff.
- **E2e** (`tests/tests/vendor_reconnect_e2e.rs`) — a second agent claiming a
  live name is rejected and exits while the first keeps serving; the same
  instance re-dialling is accepted immediately; a new instance may claim the
  name once the first is evicted.

## Out of scope

- Reconciling runtimes across a reconnect (`QueryRuntimes`, issue #92 item 4).
- Any UI for choosing or renaming a vendor.
- Server-initiated heartbeats toward the agent.
