---
title: Running horsie clustered
description: Run several horsie server nodes as one deployment, sharing a journal and a bus, so any node can serve any request.
kind: how-to
sidebar:
  order: 7
---

A single server holds every session in one process. A cluster spreads them
across several: each node runs the API and the web UI, sessions and their
supervisors are placed across the nodes, and a request that arrives at one node
is answered wherever the work actually lives. A node that loses touch with the
majority stops answering rather than answering from a stale copy.

This is the most involved way to run horsie server, and the least necessary.
Start with [Deploying the server](/operating/deploying/); come here when one
process is no longer enough.

## What a cluster needs

Three things, all of them shared, and the boot refuses the configuration if any
is missing:

- **PostgreSQL.** Nodes agree on one journal. SQLite gives each node its own
  file, so the cluster would form, agree on placement, and then keep three
  divergent histories.
- **Redis.** Nodes reach each other's live streams through it. See `bus.url` in
  [Configuration](/operating/configuration/).
- **A private network** between the nodes. The cluster port authenticates its
  peers but does not encrypt them.

Plus **three nodes**, not two. Consensus needs a majority, and the majority of
two is two — so a two-node cluster stops serving the moment either node goes
away, which is worse than one node on its own.

## Configure a node

Every node gets the same `config.json` apart from `node_id`, `bind` and its own
entry being absent from `peers`:

```jsonc
{
  "storage": { "state_dir": "/data/state", "data_dir": "/data/data" },
  "database": { "url": "postgres://user:password@db.internal/horsie" },
  "bus": { "url": "redis://redis.internal:6379" },
  "cluster": {
    // This node, stable across restarts. A node that comes back under a
    // different id is a different node as far as consensus is concerned.
    "node_id": 1,
    // Where this node listens for its peers. Not the API port.
    "bind": "0.0.0.0:7100",
    // Where the others listen. Bootstrap only — see below.
    "peers": { "2": "node2.internal:7100", "3": "node3.internal:7100" },
    // Identical on every node.
    "secret": "a-long-random-string",
    // This node's own Raft vote. Never shared.
    "raft_dir": "/raft"
  }
}
```

Omit the `cluster` section entirely and the node takes the single-node boot
path: no transport is bound, no Raft store is opened, and it costs nothing.

### The fields

| Field | Default | Meaning |
| --- | --- | --- |
| `cluster.node_id` | *(required)* | This node's identity, as an integer. Must be stable across restarts and unique in the cluster. |
| `cluster.bind` | *(required)* | Where this node listens for peers. Separate from `--addr`, which is the API. |
| `cluster.peers` | `{}` | Where each other node listens, keyed by its `node_id`. Read only while the Raft store is empty. |
| `cluster.secret` | *(required)* | The shared secret every node presents to every other. Identical everywhere. |
| `cluster.raft_dir` | `<state_dir>/cluster` | Where this node keeps its Raft vote. |
| `cluster.liveness_window_secs` | `3` | How long a peer may go unacknowledged before the leader stops counting it live. |

There are no environment-variable overrides for these. `database.url` and
`bus.url` have them — `HORSIE_DATABASE_URL` and `HORSIE_BUS_URL` — and the
cluster section does not.

## The four things that catch people

**`peers` is a bootstrap list, not live membership.** It seeds consensus once,
while the Raft store is empty. After that, membership lives in the Raft log.
Editing `peers` on a running cluster changes nothing, and a new node cannot join
one by being added to somebody's config.

**`secret` must be identical on every node, and it does not encrypt.** It is
required rather than defaulted because an absent one would mean an
unauthenticated cluster port, and anyone able to reach that port could inject
messages straight into the actor system — a bypass of horsie's authentication
rather than merely weaker hardening. Keep `bind` on a private network or behind
a TLS tunnel.

**`raft_dir` is per-node and must never be shared.** It holds this node's vote.
Two nodes pointed at one directory are two nodes voting as one. It must also
survive a restart of its own node, which is why it is configurable separately
from `state_dir`.

**A corrupt Raft store is reported, not replaced.** The node refuses to start
rather than beginning fresh, because starting fresh discards a vote, and that is
how a node votes twice in one term.

## Start the nodes

Start them in any order. A node whose peers are not up yet starts anyway,
reports itself not ready, and begins serving once a majority appears —
unreachable peers at boot are not an error, or every rolling restart would
deadlock on the first node up.

`GET /api/health` is the readiness signal:

```bash
curl -i http://node1.internal:3789/api/health
```

`200` with `{"ok": true}` means this node is serving. `503` with
`{"ok": false}` means it is not — either it has not joined yet, or it has lost
touch with the majority. Point your load balancer's health check at it.

## What a node that has stood down does

It refuses rather than answering wrongly, and that is deliberate: a node in a
minority cannot know whether the sessions it remembers have since been placed
elsewhere, so serving from what it holds would hand out stale state.

Over HTTP the refusal is `503`. An agent whose tool call reaches a stood-down
node sees that tool call fail with the same reason, rather than a status code —
it is not making an HTTP request.

`503` means *this node cannot serve right now, retry* — so a client that retries
is doing what the status code asks. Behind a load balancer with a health check
on `/api/health`, a stood-down node is taken out of rotation and the retry lands
somewhere that can answer.

## With Docker Compose

`docker/docker-compose.yml` ships the clustered configuration commented out.
Each node needs the `cluster` block, its own `/raft` volume, and the shared
Postgres and Redis:

```yaml
services:
  horsie:
    image: ghcr.io/blossomstack/horsie:latest
    ports:
      - "3789:3789" # API and web UI
    volumes:
      - horsie-data:/data
      - horsie-raft:/raft # this node's own volume, never shared

volumes:
  horsie-data:
  horsie-raft:
```

The image creates `/raft` for you; it stays empty and unused on a single node.

`cluster.bind` is not published here. The other nodes have to reach it and
nothing else should, so how you expose it depends on how the nodes reach each
other — a shared Docker network for containers on one host, a private subnet or
an overlay for containers on several. Publishing it to the host's public
interface puts an authenticated-but-unencrypted port on the internet.

## Next

- [Configuration](/operating/configuration/) — the rest of `config.json`.
- [Deploying the server](/operating/deploying/) — PostgreSQL, image tags, and
  what to back up.
