# Per-user scoping, the runtime tier — design

Written 2026-08-05. The second half of
[`2026-08-04-per-user-scoping-design.md`](2026-08-04-per-user-scoping-design.md):
its items 4–7, plus the routine scheduler. Issue #225.

## Why

#217 scoped every durable row to a `user_id` and deliberately stopped at the
database. The process-global state above it is still shared, and three parts of
it decide what a second account would actually see.

None of it is reachable today — no route in this repo creates a second account,
so there is exactly one scope and no way to cross it. This is what is owed
before that changes, not a live fault.

**The vendor map.** `SharedVendors` is a flat `HashMap<String,
Arc<RuntimeVendorLink>>` and a session picks its runtime by name
(`SessionSpec.vendor`). With two accounts, one creating a session with `vendor:
"main"` would attach to whichever runtime claimed that name — so tool calls
would execute on someone else's machine. There is no query to fix; the map is in
memory.

**The session supervisor.** `PersistenceId::new("session-supervisor", "main")` —
one actor whose event-sourced state *is* the session list. Not filterable.

**The global event stream.** `global_events` is a single broadcast channel and
`sse.rs` forwards everything to every subscriber, so every session title and
status transition reaches everyone.

Underneath all three, `main.rs` builds exactly one of everything.

## The shape

`main.rs` lines 148–282 become `build_user(user, &Shared) -> Arc<UserServices>`
in `server/src/users.rs`, held in a registry and built lazily on first touch.

**Per account.** The `SessionSupervisor` — an `ActorRef`, and transitively every
session and agent actor it spawns, since they are its children. The `SqlJournal`
bound to that user, which is what those actors persist through. The
`global_events` sender. The vendor map, the `RuntimeVendorRegistry` that
publishes into it, and the `RuntimeManager` that reads it. The `DbConfigStore`
and its live provider registry — so each account holds its own LLM clients,
built from its own BYOK credentials. Then every scoped service: github, mcp,
plugins, memory, agents, routines and the routine runner, environments,
workflows, model cards, chatgpt login.

**Per deployment.** The `Db` pool. `AuthService`, which defines the scope rather
than living in one. The artifact store and its HS256 secret. The
`RoutineScheduler` — one timer. And the values boot resolves once: `ServerInfo`,
the model-card seed set, and the account `Principal::Anonymous` maps to.

Two seams cross that line, both deliberately. `PluginService` is per-account —
its `PluginStore` and `MarketplaceStore` are scoped — but holds an `Arc` to the
*shared* artifact store: a per-account library over shared, content-addressed
bytes, which is also why `ArtifactStore::gc` is on the scope audit's allowlist.
`GithubService` is per-account for credentials while the `github_app` table it
reads is deployment config: the operator registers one App, each account
installs it.

**Built lazily, never unloaded.** A bundle is a handful of `Arc`s, one actor and
one channel. The supervisor already unloads its own idle sessions, so a dormant
account costs about what a dormant deployment costs today. Idle-unloading the
bundle is a follow-up to do when it is measured — unloading a bundle whose
session is mid-turn is a bug worth not inventing early.

The registry is `RwLock<HashMap<UserId, Arc<OnceCell<Arc<UserServices>>>>>`, and
the `OnceCell` is load-bearing rather than tidy: two concurrent first requests
that each built a bundle would spawn two `SessionSupervisor` actors on one
persistence id — two event-sourced actors writing one journal. Taking the write
lock only to insert the empty cell, then initialising it outside the lock, keeps
the build off a synchronous lock while still admitting exactly one builder.

## How a request finds its account

`require_auth` already resolves the credential and puts a `Principal` in the
request extensions. A `Scope` extractor implementing `FromRequestParts` reads it
there, maps `Principal::User(id)` to that id and `Principal::Anonymous` to the
account resolved at boot, and asks the registry for the bundle. Handlers take
`Scope` where they take `State(AppState)` today.

`AppState` keeps what is genuinely deployment-wide: `auth`, `web_dir`, the
registry, and the shared bundle. That split is what keeps the routes running
*ahead* of the auth layer working — `/api/health`, `/api/auth/*`, and
`/api/plugin-artifacts/*`. The artifact route in particular moves onto the
shared artifact store, which it can do precisely because artifacts are
content-addressed: a capability token names a hash, and knowing a hash means
already holding the bytes it addresses.

Resolution happens per request rather than once per connection because a token
is what carries the scope, and the same process may hold several.

## Nothing per-account lands on disk

Two things had to go for that to be true, and both were worth removing anyway.

**The file journal is dropped.** `journal.backend` had two values; `database`
has been the default since it landed, the CLI turns out to use no journal at all
despite a comment claiming otherwise, and `FileJournal`'s own conformance suite
carries five tests ignored as red — no snapshots, no compaction, a full replay
forever (#61 item 9). So `JournalConfig` and `JournalBackend` go, `SqlJournal`
becomes unconditional and is built inside `build_user`, and `FileJournal` leaves
`horsie-actor` with its tests and testkit helpers. `ServerInfo.journal_backend`
would report a constant, so it leaves the wire schema and the Settings info row
with it.

A deployment currently running `backend = "file"` loses its session history.
`BootConfig` does not deny unknown fields, so the stale setting is ignored rather
than rejected. That is the same one-way door `main.rs` already warned about,
walked through once and for all.

**The per-session state dir was already dead.** `RuntimeManager::runtime_spec`
did `create_dir_all(<state_dir>/sessions/<id>)` and never touched the path
again — a leftover from when the server authored the sandbox capability spec,
the `capabilities` field that `SessionSpec`'s test still proves it can ignore on
old journal rows. `RuntimeDeps::state_dir` and `ServerDeps::state_dir` go with
the call.

What remains on disk is shared by construction: content-addressed plugin
artifacts under `<data_dir>/server/plugins`, and
`<state_dir>/server/initial-admin-password`, which belongs to the deployment.
So the question of how to root a per-account directory does not arise.

## The three named parts

**The vendor map is one map per account**, inside the bundle, rather than one
shared map keyed by `(user, name)`. `DbConfigStore::open_on` already builds an
empty map per call — the server publishes no vendors, every one is an agent that
dials in — so building the store per account produces this for free. Separate
maps are structurally incapable of resolving another account's name, where a
composite key is one lookup site away from it.

`vendor_connect` authenticates the dialling agent and holds its `Principal`
before the upgrade completes, so it resolves the owner's bundle and publishes
there. This also fixes something live today in miniature:
`RegisterError::NameTaken` means the first person to run `horsie connect
--runtime-id main` denies that name to everyone else forever. Per-account maps
make `main` available to each person. Registry gate 2 — "a different principal
holds it, refuse" — becomes redundant under per-account maps and is **kept
anyway**, as defence in depth.

**The supervisor's persistence id carries the user**:
`PersistenceId::new("session-supervisor", user.as_str())`. Recovery rebuilds the
registry and stops there — no session actor spawned, no vendor called — so
building a bundle lazily costs one journal replay, paid on that account's first
request rather than at boot.

**The idle policy stays deployment-wide.** `SupervisorConfig` — the clock, the
idle timeout, whether a background ticker runs — is cloned into every account's
supervisor from `Shared`. How long a session may sit idle is an operator's
decision, not an account's preference, and putting it on the deployment tier is
also what lets a test drive time for every account at once.

**`global_events` is one channel per bundle.** Not one channel with a `user_id`
on the frame and a filter in the SSE handler: a filter is one forgotten line
away from leaking every session title and status transition on the server,
whereas separate channels cannot.

## The scheduler and the seed

**One timer, every account.** `RoutineScheduler` moves to the shared tier and
drives `RoutineStore::due_across_all_users` — which already exists, is tested,
and is on the scope audit's allowlist with its reason — then resolves each due
routine's owner bundle to arm and run it in that account's scope. Scoping this
read instead would mean one timer per account, and a timer per dormant account
is exactly what lazy bundles are for avoiding.

Its scoped twin goes with it. `RoutineStore::due` had exactly one caller, and
leaving a second answer to "what is due" in the tree is leaving something that
silently under-reports the moment a deployment has two accounts.

**Model-card seeding leaves boot.** Looping the bundled catalogue over every
account at every startup is O(accounts × cards) of writes before the port opens.
Instead `build_user` seeds once, guarded by a row in the account's own `settings`
table keyed on a hash of the resolved seed set — bundled defaults plus any
`--model-cards-seed` file, both read once at boot. A hash rather than the server
version because the operator's seed file can change without a version bump, and
a marker that misses that change is a marker that lies. O(1) per account per
change, paid on first touch, never at boot, and no fallback on the read side. An
account that deletes a bundled card gets it back the next time the seed set
changes; that is the price of not having a shared catalogue, and the design
already paid it.

## Testing

Two tests carry this, and both are load-bearing rather than belt-and-braces.

**An HTTP isolation test**, the runtime-tier twin of `tests/user_isolation.rs`.
Two accounts are two tokens from `AuthStore::insert_token` rather than two
`auth_users` rows: `create_user` enforces the single-account rule this repo
ships with, and a token *is* the scope — `auth_tokens.principal` is what every
request resolves through. It
asserts that B cannot see A's sessions, that B's `/api/events` receives none of
A's frames, and that two agents both announcing `main` each resolve to their own
owner's link. This is the test that fails when a handler keeps reaching for
something process-global.

**A boot test** (#226). #217 shipped two bugs on the boot path with the Rust
suite green, because nothing in the tree boots `run()` — the first surfaced as a
Playwright timeout five minutes into CI, the second not at all. This change
rewrites that exact path, so `run()` splits to let a test bring the real
composition root up against a temp directory and answer `/api/health`.

## What is not in this change

Work-breakdown item 9 — `routes()`, `trait UserResolver`, `trait UsagePolicy` —
is the seam a downstream deployment plugs into, and needs nothing from this
change to land afterwards. It gets its own issue.

Idle-unloading a `UserServices`, per above. And account provisioning, which the
parent design leaves to the deployment on purpose.

## Risks

- **The `Scope` extractor is the isolation boundary for ~100 handlers.** A
  handler that keeps reaching for something process-global is the failure mode,
  and the HTTP isolation test is the only thing that catches it.
- **Lazy building puts work on a first request**: one journal replay for the
  supervisor, and a guarded seed batch when the seed set has changed.
- **Each active account holds its own provider HTTP clients and vendor map**,
  where today the deployment holds one set. Fine at the intended scale.
- **Bundles are never unloaded**, so a deployment with very many accounts that
  each touch it once accumulates bundles for the process lifetime.
- **Dropping the file journal is a one-way door** for any deployment that
  selected it.
