# Per-user resource scoping — design

Written 2026-08-04. Turns horsie server from a single-account application into
one where every durable resource belongs to a user, so that several people can
share one server without sharing anything else.

## Why

horsie has exactly one account. `server/migrations/sqlite/0014_auth.sql` says so
in its first line — "one admin account plus the opaque tokens every
authenticated surface presents" — and every table underneath it is written for a
deployment with a single occupant. `memory_spaces.name` is a primary key.
`providers.name` is a primary key. `SessionSupervisor` is one actor whose
event-sourced state is *the* session list.

That is fine for the product as described in PRODUCT.md. It stops being fine the
moment two people share a server.

Two things follow, and only one of them is about the database.

**The stated one.** A team that self-hosts horsie today shares one login. There
is no way for two developers on one server to have their own sessions, their own
provider keys, or their own connected runtime.

**The one that matters more.** `SharedVendors` is a flat, deployment-wide
`HashMap<String, Arc<RuntimeVendorLink>>` (`server/src/sessions/spec.rs:22`) and
a session selects its runtime by name (`SessionSpec.vendor: String`). Two people
sharing a server today are one `vendor: "main"` away from **executing tool calls
on each other's laptops**. There is no query to add a `WHERE` clause to; the map
is in memory. Any design that scopes the database and stops there has fixed the
smaller half of this.

## The scope

The scope key is `user_id: i64`. Nothing else.

Three reasons it is the user and not a group:

- `horsie connect` runs on somebody's laptop. There is no coherent reading in
  which a teammate's session executes tools on my machine, so the runtime is
  per-user by physics, not by policy.
- Provider credentials are BYOK. A shared key makes one person's spend invisible
  to them.
- `Principal::User(id)` is **already** in request extensions on every `/api`
  call (`server/src/http/auth.rs:60`). The scope key is already on the request,
  so no new token field, no scope-switching endpoint, no route changes.

Naming: `user`, not `workspace` (taken — `WorkspaceSpec`, runtime workspaces)
and not `space` (taken — memory spaces).

`Principal::Anonymous`, which is what every request carries when
`auth.enabled = false`, resolves to user 1. Existing single-user deployments
keep working unchanged, which PRODUCT.md's "auth is on by default, but" story
depends on.

Grouping users — teams, shared ownership, delegated administration — is out of
scope here and needs nothing from this schema. A deployment that wants it can
build above: reading across a set of users is `WHERE user_id IN (…)`, which the
shape below already permits.

## Data model

Sixteen tables gain the scope. Thirteen of them have a natural `TEXT PRIMARY
KEY` today, which must become composite:

| Change | Tables |
| --- | --- |
| Composite PK `(user_id, name)` — 13 | `providers`, `models`, `settings`, `mcp_servers`, `plugins`, `memory_spaces`, `agents`, `routines`, `environments`, `workflows`, `provider_oauth`, `marketplaces`, `model_cards` |
| Plain `user_id` column — 3 | `memories`, `github_credentials`, `journal_logs` |
| Already scoped | `auth_tokens`, `auth_device_codes` — both carry `principal` |
| Scoped through a parent | `journal_events`, `journal_snapshots` — via `journal_logs.log_id` |
| Identity root | `auth_users` |
| Deployment config | `github_app` |
| Dropped | `vendors` |

**SQLite cannot alter a primary key.** Those thirteen are create-new / copy /
drop / rename rebuilds on the SQLite side and plain `ALTER` on PostgreSQL. The
two migration directories must declare identical versions and descriptions or
`migrations_are_in_parity` fails CI (`server/src/db/mod.rs`). Backfill is
`DEFAULT 1` throughout: every row that exists belongs to the only user there has
ever been.

Four of these entries are decisions rather than mechanics.

**`vendors` is dropped, not scoped.** It has zero query sites in `server/src`,
and `server/src/config/store.rs` states the reason: "The server builds no
vendors: every vendor is an agent that dials in and publishes itself into this
map. It starts empty at boot and is never repopulated from the database." The
table is vestigial.

**`model_cards` is per-user.** It is reference data — context windows, pricing,
thinking efforts — which argues for one shared catalogue. It loses that argument
to a concrete case: a member who cannot add a card for a model the operator has
not blessed cannot use their own self-hosted or newly-released model, which
contradicts per-user BYOK outright.

That leaves the upgrade path to solve, because `seed_if_missing` at boot
(`server/src/bin/horsie-server/main.rs:138`) is how a new horsie version
delivers newly-released cards to an existing deployment, and looping it over
every user at every boot is O(users × cards) writes at startup. So: **seed
lazily, per user, inside `build_user`, guarded by a seed-version marker on the
user row.** O(1) per user per upgrade, paid on first touch after the upgrade,
never at boot, and no fallback path on the read side. A user who deletes a
bundled card gets it back at the next upgrade; that is an acceptable price for
not having a shared table.

Knock-on: PRODUCT.md describes Admin as "the model card catalog". It becomes a
per-user settings page, and `/api/admin/model-cards` leaves the admin prefix.

**The `auth_*` tables define the scope rather than living inside it.**
`auth_users` cannot carry a `user_id` because it *is* the user — the column
would point at its own primary key. `auth_tokens` is already scoped: every row
has `principal = user:<id>`, which is the scope column under another name.
`auth_device_codes` backs the CLI device-approval flow, where a code is
necessarily minted *before* anyone has authenticated; it already gains
`principal` on approval.

**`github_app` is deployment configuration, not a shared resource.** A GitHub
App is registered against a deployment — one callback URL, one webhook URL, one
client ID and private key, all bound to the server's public address. Users then
*install* that App on their own repos, which is what produces the per-user rows
in `github_credentials`. One App serving every user on a deployment is how every
GitHub integration works; the operator registers it once.

**`journal_events` and `journal_snapshots` are scoped through their parent.**
They join `journal_logs` on `log_id` with `ON DELETE CASCADE`. `journal_logs`
gains `user_id`, and the `(kind, id)` → `log_id` lookup binds it, so no user can
obtain another's `log_id` — and without one the events are unreachable.
Enforcement is real; it happens one table up.

A redundant `user_id` on `journal_events` is **rejected**. That table is
`WITHOUT ROWID` with `PRIMARY KEY (log_id, seq)` precisely so the hot query —
`WHERE log_id = ? AND seq > ? ORDER BY seq` — is a contiguous range scan with no
index indirection. Widening that key damages the one thing it is tuned for, in
order to duplicate a fact the parent already enforces.

## Runtime topology

`server/src/bin/horsie-server/main.rs` builds exactly one of everything in 334
clean lines. That becomes `build_user(user_id, shared) -> UserServices`, held in
a registry and built lazily on first request.

Per user: the `SessionSupervisor` (its `PersistenceId` keyed by user id rather
than today's hardcoded `PersistenceId::new("session-supervisor", "main")` at
`supervisor.rs:486`), the `global_events` broadcast channel, the vendor map, the
provider registry, every scoped store and service, and a `RuntimeManager` rooted
at `<state_dir>/server/users/<id>/`.

Shared across users: the `Db` pool, the `Journal`, the artifact store, the
GitHub App config, and the routine scheduler's timer.

**Built lazily, never unloaded.** A `UserServices` is a handful of `Arc`s, one
actor and two channels; the supervisor already unloads its own idle sessions, so
a dormant user costs about what a dormant deployment costs today. Idle-unloading
the bundle is a follow-up to do when it is measured, not guessed — unloading a
bundle whose session is mid-turn is a bug worth not inventing early.

Three parts of this are load-bearing beyond the database.

**The vendor map is keyed by `(user_id, name)`.** `vendor_connect.rs` already
authenticates the dialling agent and holds its `Principal` before the upgrade
completes, so the owner is known at exactly the right moment. This also fixes a
denial of service that exists today in miniature: `RegisterError::NameTaken`
means the first person to run `horsie connect --runtime-id main` denies that
name to everyone else forever. Per-user maps make `main` available to each
person. Registry gate 2 — "a different principal holds it, refuse" — becomes
redundant under per-user maps and is **kept anyway**, as defence in depth.

**`global_events` becomes one channel per user**, held in the bundle. Not one
channel with a `user_id` on the frame and a filter in the SSE handler: a filter
is one forgotten line away from leaking every session title and status
transition on the server, whereas separate channels are structurally incapable
of it. `sse.rs:240` subscribes and forwards unconditionally today.

**Two queries must stay deliberately unscoped.** `ArtifactStore::gc(keep)` needs
the union of referenced hashes across *all* users — artifacts are
content-addressed, so scoping that query deletes bundle bytes other users are
still using. `RoutineScheduler::tick` needs due routines across all users, then
runs each in its owner's scope. Both go on the static-check allowlist with the
reason written at the call site. The rule for this work is not "add `user_id`
everywhere"; it is "decide per query, and be right every time".

## Isolation enforcement

Three layers. The third carries the weight and the other two are cheap, so all
three ship.

**Construction-time binding.** Stores take the scope in their constructor —
`MemoryStore::for_user(db, user)` — never as a per-call argument. Every store
already takes a `Db` by value at construction (`Db` is `Clone` over an internal
`Arc`), so this is the existing shape with one more field. There is then no call
site that *can* pass the wrong scope.

**A CI static check.** Every SQL statement in this repo is a literal written in
this repo, and `Db::q` is the single chokepoint — both are already stated
invariants in `server/src/db/mod.rs`. A test walks the source tree from
`CARGO_MANIFEST_DIR`, extracts string literals naming a scoped table, and fails
any statement that does not also mention `user_id`. The allowlist has exactly
two entries, named above, each with its reason.

**An isolation test harness.** For every store: write as user 1 and user 2, then
call every read, update, and delete as user 2 and assert user 1's rows are
invisible and unchanged. This is the test that fails when somebody adds a method
and forgets.

It creates its users through `AuthStore::create_user` rather than over HTTP,
because no route in this repo creates a second user — see the extension points
below. That makes this harness the *only* thing exercising the scoping code in
CI. It is load-bearing, not belt-and-braces: if it rots, the isolation
guarantees rot silently with it.

PostgreSQL row-level security is **not** part of this design. SQLite has no
equivalent and it is the backend every self-hoster runs, so RLS can only ever be
defence in depth on a PostgreSQL deployment — never the mechanism.

Be clear-eyed that this is a permanent maintenance obligation. It is the price
of scoping a shared database rather than separating databases, and it is paid
forever, on every new feature.

## Extension points

horsie enforces the scope. It deliberately does **not** decide where accounts
come from, because that varies more between deployments than anything else in
the server: one operator wants OIDC, another LDAP, another a file they edit by
hand, another their own provisioning service. Picking one would be wrong for
everybody else, so the server exposes the seam instead and ships no
account-management surface of its own.

`AuthStore::create_user` already exists and is public
(`server/src/auth/store.rs:100`, used by `bootstrap`), so a deployment that
provisions its own accounts inserts through the method that is already there.
`role` and `disabled_at` are **not** added to `auth_users`; a deployment that
needs a role model owns it, in its own tables, with `UserResolver` as the
enforcement point on the auth path. `auth_users` is unchanged by this design.

Three additions make that seam usable from outside the crate:

- **`routes(state) -> Router`** alongside today's `app(state)`, so another
  binary can `.merge()` its own routes and wrap its own middleware.
- **`trait UserResolver`** on `AppState`. The bundled implementation reads the
  principal off the request, which is what already happens. Another can add
  external identity and suspension checks.
- **`trait UsagePolicy`** — hooks at session create and turn start, for
  deployments that need to bound what a user may consume.

A deployment's own tables run under a second `sqlx::Migrator` with
`dangerous_set_table_name` (verified present in sqlx 0.9), because two migrators
on one database otherwise collide on `_sqlx_migrations` — the collision
`0017_journal.sql` already warns about.

**Deleting a user is out of scope for this repo**, and deliberately so — not an
omission to be filled in later. Erasing an account belongs with whoever
provisions accounts, so this repo offers no `purge_user`, no `ON DELETE CASCADE`
from `auth_users`, and no enumeration of scoped tables for a deletion to walk.

Worth stating because a half-built one is worse than none: `PRAGMA foreign_keys`
is never enabled on the pool (`0009_memory.sql`, `0014_auth.sql`,
`server/src/db/journal.rs:328`), so a declared cascade between `auth_users` and
a scoped table is *silently ignored* on SQLite. Anyone reaching for one later
would get a constraint that looks right, compiles, runs, and does nothing.

## What does not change

`clients/web` needs no changes for scoping. The API is implicitly scoped by the
caller's own token, so every existing call returns that user's data and nothing
else. This should be confirmed as the work lands rather than assumed.

The wire protocol is unchanged: no fluorite schema gains a `user_id`, because
the client already knows whose token it is holding.

## Risks

- **The thirteen table rebuilds** are the most error-prone piece of the work.
  Each needs its data preservation asserted by a test, in both dialects.
- **Per-user provider registries** mean each active user holds their own
  provider HTTP clients, where today the deployment holds one set. Fine at the
  intended scale; worth watching.
- **Lazy per-user model-card seeding** puts a write on the path of a user's
  first request after an upgrade. It should be a single guarded batch, not a
  per-card round trip.
- **`UserServices` is never unloaded** in this design. A deployment with very
  many users that all touch it once will accumulate bundles for the process
  lifetime.

## Work breakdown

Eight changes, in dependency order. The first and third are the large ones.

1. Schema: 13 composite-key rebuilds, 3 column adds, drop `vendors`, backfill to
   user 1 — both dialects, in parity.
2. Scope the stores: constructor-bound `user_id` across 143 `sqlx::query` sites.
3. `UserServices` + the lazy registry; refactor the composition root.
4. Per-user `SessionSupervisor` and journal scoping.
5. Per-user vendor map and event channel.
6. Lazy per-user model-card seeding.
7. The isolation harness and the CI static check.
8. Extension-point traits, `routes()`, and documentation.
