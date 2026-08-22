---
title: Configuration reference
description: Every config.json field, command-line flag and environment variable, and what lives in the settings database instead.
kind: reference
sidebar:
  order: 6
---

Configuration lives in two places that never overlap.

**`config.json`** holds deployment and bootstrap settings. You edit it by hand
and the server reads it at startup. It is small and stable.

**The settings database** holds everything you tune day to day, and is edited
from the **Settings** pages. Secrets stored there are never returned by the
API — the UI shows only whether a key is set.

## `config.json`

Location: `$XDG_CONFIG_HOME/horsie/config.json`, else
`~/.config/horsie/config.json`. Pass `--config <path>` to use another.

Every field has a default, so an empty file — or no file — is valid.

```jsonc
{
  "storage": {
    // Ephemeral runtime state. Default: $XDG_STATE_HOME/horsie,
    // else ~/.local/state/horsie
    "state_dir": "/var/lib/horsie/state",
    // Durable data: plugin artifacts, and with the default SQLite database
    // the settings database and journal too.
    // Default: $XDG_DATA_HOME/horsie, else ~/.local/share/horsie
    "data_dir": "/var/lib/horsie/data"
  },
  "database": {
    // Default: a SQLite file at <data_dir>/server/config.db.
    // sqlite:// and postgres:// are both supported.
    "url": "postgres://user:password@host/horsie",
    // Pool size, shared by settings reads and journal writes. Default: 10.
    "max_connections": 10
  },
  "auth": {
    // "password" (the default), "delegated", or "off".
    "mode": "password"
  },
  "bus": {
    // Only for a deployment running more than one node. Absent means nodes
    // do not talk to each other, which is correct for a single server.
    "url": "redis://redis.internal:6379"
  },
  "cluster": {
    // Absent on a single server, which is the default and costs nothing.
    // See Running horsie clustered.
    "node_id": 1,
    "bind": "0.0.0.0:7100",
    "peers": { "2": "node2.internal:7100", "3": "node3.internal:7100" },
    "secret": "a-long-random-string",
    "raft_dir": "/raft"
  }
}
```

That is the whole server-side file.

### Fields

| Field | Default | Meaning |
| --- | --- | --- |
| `storage.state_dir` | `$XDG_STATE_HOME/horsie` | Ephemeral state. Safe to lose across a restart. |
| `storage.data_dir` | `$XDG_DATA_HOME/horsie` | Durable data. Back this up; mount a volume here in a container. |
| `database.url` | SQLite at `<data_dir>/server/config.db` | Settings store and session journal. |
| `database.max_connections` | `10` | Connection pool size. |
| `auth.mode` | `password` | `password`, `delegated`, or `off`. See [Authentication](/operating/authentication/). |
| `bus.url` | *(none)* | Where nodes publish to each other. Leave unset for a single server. Setting it on a deployment of one is harmless; leaving it unset on a deployment of several is not — see below. |
| `cluster.node_id` | *(required in the section)* | This node's identity, as an integer. Stable across restarts, unique in the cluster. |
| `cluster.bind` | *(required in the section)* | Where this node listens for its peers. Separate from `--addr`. |
| `cluster.peers` | `{}` | Where each other node listens, keyed by `node_id`. Read only while the Raft store is empty. |
| `cluster.secret` | *(required in the section)* | Shared secret every node presents to every other. Identical everywhere; authenticates but does not encrypt. |
| `cluster.raft_dir` | `<state_dir>/cluster` | Where this node keeps its Raft vote. Per-node; never shared. |
| `cluster.liveness_window_secs` | `3` | How long a peer may go unacknowledged before the leader stops counting it live. |

The whole `cluster` section is absent on a single server. Present, it requires a
`postgres://` `database.url` and a `bus.url`, and the boot refuses the
configuration without both — see
[Running horsie clustered](/operating/clustering/).

An unknown key is ignored rather than rejected, so an old file keeps parsing.

### Keys the CLI owns

The same file also carries settings the **CLI** reads and the server ignores.
Writing either side never destroys the other's keys.

| Field | Meaning |
| --- | --- |
| `default_server` | The server `horsie` commands target when `--server` is omitted. Managed with `horsie config set default-server`. |
| `storage.state_dir` | Where `horsie connect` keeps per-runtime scratch directories and materialized bundles. |
| `runtime.bin` | Path to the `horsie-runtime` binary `horsie connect` spawns. Absent → the sibling next to the running CLI. |
| `runtime.hook_path` | Directories prepended to `PATH` when running plugin hooks, and granted read access in the sandbox. Absent → `node` is auto-discovered. |

## Command-line flags

`horsie-server` accepts:

| Flag | Default | Purpose |
| --- | --- | --- |
| `--addr <host:port>` | `127.0.0.1:3789` | Bind address. Use `0.0.0.0:3789` to accept connections from other hosts. |
| `--config <path>` | the user config path | Config file to load. A path given here must exist and parse. |
| `--web <dir>` | *(off)* | Also serve built web-UI assets from `<dir>` on the same port, same-origin — no separate dev server and no CORS setup. |
| `--model-cards-seed <path>` | *(none)* | JSON file of extra model cards to seed at startup, inserted if missing. Bundled defaults are always seeded. |

## Environment variables

| Variable | Effect |
| --- | --- |
| `HORSIE_DATABASE_URL` | Overrides `database.url`. Takes precedence over the config file. Accepts `sqlite://` or `postgres://`. |
| `HORSIE_AUTH_MODE` | Overrides `auth.mode`: `password`, `delegated`, or `off`. An unrecognised value falls through to the config file rather than silently changing who may reach the server. |
| `HORSIE_BUS_URL` | Overrides `bus.url`. Takes precedence over the config file. |
| `HORSIE_MODEL_CARDS_SEED` | Same as `--model-cards-seed`. |
| `HORSIE_TOKEN` | **CLI.** Bearer token to send instead of reading stored credentials. For scripts and CI. |
| `RUST_LOG` | Which log events the server prints. Unset, empty, or unparseable → `info`. |

### Logs

The server writes its log to stdout, at `info` and above by default. That
default is what tells you a node refused to boot, or that an actor could not
replay its journal and stopped — the class of fault whose only other symptom
is an endpoint answering `500`.

`RUST_LOG` narrows or widens it, either by level (`debug`) or per module
(`info,horsie_server::sessions=debug`). Setting it to the empty string means
*unset*, not *silent*: a container that passes `RUST_LOG` through without
defining it still logs at `info`, because a stack that had quietly stopped
logging looked exactly like a stack with nothing to say. To actually silence
the server, ask for it — `RUST_LOG=off`.

### Running more than one node

One server needs no `bus.url`: everything that has to reach anything else is in
the same process.

More than one server sharing a database does. Nodes reach each other by
publishing to the bus, and without it each one publishes into its own process
and hears nothing from the others. Nothing errors — a live stream simply never
delivers, and a runtime that dials one node cannot be reached from another. A
bad URL fails the boot on purpose, so a node that cannot reach the bus refuses
to start rather than running in that state.

## What is not here

Providers, models, runtime vendors, the default runtime vendor, GitHub, MCP servers,
skill bundles, agent presets, environments, routines, workflows and memory are
**not** in `config.json`. They live in the settings database and are managed
from the UI.

## The settings pages

| Page | Sections | What you configure |
| --- | --- | --- |
| **Models** | Providers | Name, kind, optional base URL, inline API key. See [Models & providers](/operating/models-and-providers/). |
| | Models | Alias, provider, model id, optional max tokens. |
| **Runtimes** | Vendors | One list. A `horsie connect` process appears here while it is attached and is configured where it runs, so its row only sets the default. A cloud vendor is configured here — see [Cloud runtime vendors](/operating/cloud-vendors/). |
| **Skills** | — | Skill and plugin bundles, and marketplaces. See [Skills & plugins](/using/skills-and-plugins/). |
| **Memory** | — | Memory spaces and the notes the agent has saved in them. |
| **Integrations** | GitHub | App configuration, the connection, and the GitHub tools toggle. See [GitHub repositories](/using/github-repositories/). |
| | MCP servers | Remote MCP servers. See [MCP servers](/using/mcp-servers/). |
| | Server *(read-only)* | Config file path, database, state dir, data dir, version. |
| **Appearance** | — | Theme, light/dark/system, text size, transcript switches. Stored in the browser, not the database, so each browser can differ. |
| **Account** | — | Password, machine tokens, sign out. |

Every settings page saves as you go: a provider, a model and a cloud vendor
each save on their own, from the row you opened. Anything destructive asks
first.

Operator settings live under **Admin**, whose only page today is **Model
cards**: the catalogue the Models page autocompletes from.

### When changes take effect

| Change | Effect |
| --- | --- |
| Providers and models | The next turn. No restart. |
| Cloud vendors | The next session. Nothing to deploy or restart. |
| Default vendor | The next session created. It may name a vendor that has not connected yet. |
| Connected agents | Not editable here. Each is configured where it runs, and appears or disappears as it connects. |
| GitHub, MCP servers, skill bundles | As you save them. |

## On-disk layout

**`data_dir`** — plugin artifacts, plus with the default SQLite database the
settings database and the journal under `<data_dir>/server/`. A PostgreSQL
deployment keeps only plugin artifacts here.

**`state_dir`** — ephemeral runtime state, including
`server/initial-admin-password` on a fresh install.
