# Crate consolidation, and a publish surface that matches the install path

Two merges, one publish-flag audit. Takes the workspace from 12 crates to 10,
and makes the set of crates published to crates.io exactly the `cargo install
horsie` closure — nothing more.

## The problem

### The registry no longer describes the repo

Thirteen `horsie-*` crates sit on crates.io at 0.1.6, all published on
2026-07-21 by tag `v0.1.6`. Since that tag the workspace has been reorganised
five times (#274, #279, #280, #288, #293), and the registry did not follow:

| crates.io | in the workspace today |
| --- | --- |
| `horsie`, `horsie-actor`, `horsie-agentcore`, `horsie-models`, `horsie-runtime`, `horsie-runtime-client`, `horsie-workflow` | present, publishable |
| `horsie-anthropic`, `horsie-openai` | gone — merged into `horsie-llm-providers` |
| `horsie-executor`, `horsie-executor-client` | gone — renamed to `runtime`, `runtime-client` |
| `horsie-mcp-client` | gone — merged into `horsie-support` |
| `horsie-supervisor` | gone — renamed to `horsie-server`, now `publish = false` |
| — | `horsie-support` and `horsie-runtime-vendor` are publishable but were never published |

So the next version tag would create two brand-new registry names, and per the
bootstrap caveat in `publish.yml` each needs its own trusted-publishing config
on crates.io before it can ship.

Nothing forced this drift, because nothing decides what should be published.
`publish = false` appears on three crates with a one-line comment each; the
other nine are publishable by default rather than by intent.

### One crate does two unrelated jobs, and one is misnamed

`horsie-runtime-client` and `horsie-runtime-vendor` are the two halves of the
same wire. The vendor half already depends on the client half for
`RuntimeTransport`. No consumer takes one without the other: the CLI names only
`runtime-vendor` but reaches `agentcore` through `runtime-client` transitively,
and `horsie-server` depends on both directly.

`horsie-workflow` is not what its package description claims ("Agent workflow
graphs for horsie"). Its `lib.rs` is explicit:

> The agent loop on top of the event-sourced `actor` runtime.

It is `AgentActor`, toolbox composition, workspace context, task list, timers
and hook translation. The workflow-graph feature is elsewhere, in
`crates/server/src/workflows/`. The crate's only consumers are `horsie-server`
and `integration-tests`.

## Design intent

**The published set is the `cargo install horsie` closure, and nothing else.**
This is the governing rule. It is self-checking: a crate that the CLI binary
cannot reach has no business on crates.io, and a crate the CLI needs cannot be
forgotten. It replaces the current situation, where publishability is a default
rather than a decision.

Two consequences the rest of this document follows:

- **A crate reachable only from `horsie-server` is `publish = false`.** The
  server is not distributed through crates.io; it ships as a release binary and
  a container image. Anything it alone consumes is an internal module that
  happens to live in its own directory.
- **A crate whose only consumer is another crate in the same closure is a
  merge candidate,** because the boundary is buying encapsulation from nobody.

## Merge one: `runtime-client` + `runtime-vendor` → `horsie-runtime-host`

The host side of the runtime wire becomes one crate: dialling into a runtime
(`RuntimeClient`, `RuntimeTransport`) and supplying one (`listener`,
`provider`, credentials, reconnect, baseline capabilities). It reads as the
counterpart to `horsie-runtime`, which is the sandboxed child process itself.

This is close to a straight file move. Verified before committing to it:

- **No module-name collision.** `client`, `testkit`, `tools`, `transport` from
  the client half; `baseline`, `connected_registry`, `env_scrub`, `error`,
  `issued_tokens`, `listener`, `process_provider`, `provider`, `reconnect`,
  `runtime_listener`, `runtime_vendor`, `socket_transport`, `vendor` from the
  vendor half.
- **No exported-symbol collision.** The two `pub use` lists are disjoint.
- **No new dependency edge for anyone.** The merged crate depends on `models`,
  `support`, `agentcore` — the union of what the two already had. Every
  consumer already had all three on its graph.
- **No cycle.** `runtime-vendor`'s dev-dependency on `horsie-server` is
  path-only with no version, so cargo strips it on publish and tolerates the
  dev-cycle in-workspace. That is unchanged.

The `test-util` feature carries over as-is, still forwarding to
`horsie-agentcore/test-util`.

`pub mod runtime_vendor` keeps its name and its doc comment explaining why it
is a module rather than a root re-export. `horsie_runtime_host::runtime_vendor`
is a slightly long path, but renaming it is a separate concern from this merge
and the comment documents a live migration.

## Merge two: `horsie-workflow` → `crates/server/src/agent_loop/`

The crate's 9,582 lines move into `horsie-server` as a module. The destination
is **`agent_loop`, not `workflow`** — `server/src/workflows/` (the graph
feature) and `server/src/sessions/workflow/` already exist, and a third
`workflow` beside them naming a fourth unrelated thing would be actively
misleading. `agent_loop` is what the code's own module doc calls it.

Every reference, enumerated:

- **Twenty files under `crates/server/src/`** name `horsie_workflow::` and get
  rewritten to `crate::agent_loop::`. The heaviest are
  `sessions/session_actor/types.rs` and `.../context.rs` at seven each.
- **`crates/workflow/tests/workspace_context.rs`** moves to
  `crates/server/tests/`, joining the existing `sql_journal.rs`.
- **`crates/tests/tests/agent_recovery_e2e.rs`** has thirteen `horsie_workflow`
  sites and rewrites to `horsie_server::agent_loop`. `crates/tests/Cargo.toml`
  drops the `horsie-workflow` dependency; it already depends on
  `horsie-server`.
- **`crates/support/src/plugin/skills.rs:47`** is a doc comment pointing at
  "the runtime-side reader in `horsie_workflow`" and needs the new name. It is
  prose only — `support` is a dependency *of* the agent loop, not a consumer.

`agent_loop` is declared `pub mod` in `crates/server/src/lib.rs`, matching the
other feature modules, because `integration-tests` reaches its types through
`horsie-server`.

This grows `horsie-server` from 51,132 to roughly 60,700 lines across 131
files, which is the cost. It is accepted here: the crate was already the
largest in the workspace by a factor of five, and splitting it is its own
project with its own seams to pick. This merge does not make that project
harder — `agent_loop` remains a self-contained directory, and would be the
first thing to lift back out.

### The actor leak closes for free

`horsie-workflow` uses exactly one symbol from `horsie-actor` —
`JournalError`, in two places — while `horsie-server` uses eight distinct
types from it. That single reference was the only thing keeping `horsie-actor`
from being server-private.

Once `workflow` is part of `server`, those two references are server
references. No code changes; `horsie-actor`'s only non-test consumer becomes
`horsie-server`, and it becomes `publish = false` by the governing rule.

`horsie-actor` stays a separate crate. Folding it in too would grow the server
another 1,410 lines to erase a boundary that is doing real work: actor
primitives and durable journaling are generic, and the crate wall is what keeps
server logic from leaking into them.

## The publish surface

| crate | loc | publish | change |
| --- | --- | --- | --- |
| `horsie` (cli) | 4,435 | yes | — |
| `horsie-models` | 1,126 | yes | — |
| `horsie-support` | 6,047 | yes | **register on crates.io** |
| `horsie-agentcore` | 3,518 | yes | — |
| `horsie-runtime` | 7,504 | yes | — |
| `horsie-runtime-host` | 6,473 | yes | **register on crates.io** |
| `horsie-server` | ~60,700 | no | absorbs `workflow` |
| `horsie-actor` | 1,410 | no | **was publishable** |
| `horsie-llm-providers` | 2,348 | no | — |
| `integration-tests` | — | no | — |

Every `publish = false` gets a comment saying *why* by the governing rule, not
just *that*.

Three names freeze at 0.1.6 on crates.io and are deliberately left alone,
neither yanked nor updated: `horsie-actor`, `horsie-workflow`, and the six
already-dead names from the pre-`v0.1.6` layout. crates.io never frees a
published name, so all of them remain reserved. Yanking would buy only
cosmetics and would break anyone who pinned 0.1.6.

### Pre-flight before the next tag

`horsie-support` and `horsie-runtime-host` do not exist on crates.io, and
trusted publishing can only be configured for a crate that already exists. The
first publish of each therefore needs `CARGO_REGISTRY_TOKEN`, which
`publish.yml`'s own comment says should have been deleted after OIDC was set
up. Before tagging `v0.1.7`:

1. Confirm whether the `CARGO_REGISTRY_TOKEN` repo secret still exists. If not,
   mint a scoped publish token and add it back temporarily.
2. Tag and let the publish run create both crates.
3. Configure trusted publishing for each new crate (repository
   `blossomstack/horsie`, workflow `publish.yml`).
4. Delete the secret again.

The auth step in `publish.yml` already has `continue-on-error: true` and falls
through to the secret, so no workflow change is needed for this.

## What does not change

- **`publish.yml`'s version guard.** It compares the tag against every
  workspace package including `publish = false` ones, so all ten crates still
  move in lockstep. That is more churn than the publish surface strictly needs,
  but a uniform workspace version is a real simplification and a separate
  decision.
- **The `release-binaries` job**, the install script, and the container image.
  These are the actual distribution channels and are untouched.
- **`horsie-llm-providers`, `horsie-models`, `horsie-support`,
  `horsie-agentcore`, `horsie-runtime`, `horsie` (cli)** keep their current
  boundaries.

## Verification

Boundary changes of this shape fail in places a single-crate check does not
reach, so:

- `cargo build --workspace --locked` and `cargo clippy --workspace
  --all-targets` under the workspace lints.
- `cargo test --workspace` — `integration-tests` exercises the routes that both
  merges sit underneath, and a `-p horsie-server` run alone would be a false
  green.
- `cargo publish --dry-run -p <crate>` for each of the six publishable crates.
  This is the only check that catches a path dependency missing its `version =`
  key, which is the classic way a workspace refactor breaks publishing without
  breaking the build.
- The web e2e suite, since `agent_loop` sits under the session routes it drives.

## Out of scope

- Splitting `horsie-server`. Real, and larger than this.
- Renaming `runtime_vendor` the module, or `RuntimeHandle`/`RuntimeVendor`.
- Yanking anything on crates.io.
- Decoupling crate versions from the workspace version.
