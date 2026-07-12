# DB-managed skill bundles: install once on the server, select per session, works on any vendor

- **Date:** 2026-07-12
- **Status:** design, pending implementation
- **Scope:** give the **session server** (`server` crate) a database-backed, web-managed
  library of **plugin bundles** (skills + `SessionStart` hooks + sibling resources), let
  users **select which bundles a session gets** at creation, and **materialize the selected
  bundles onto the runtime's filesystem uniformly across vendors** (local host process and
  velos remote container) so the *existing* runtime plugin machinery discovers them. Builds
  directly on [2026-06-01-shared-plugins-design.md](2026-06-01-shared-plugins-design.md)
  (the plugin/skill primitive), [2026-07-09-server-sessions-design.md](2026-07-09-server-sessions-design.md)
  (sessions as actors + `RuntimeVendor`), [2026-07-10-settings-ux-design.md](2026-07-10-settings-ux-design.md)
  (the SQLite `ConfigStore` + Settings web UX this mirrors), and
  [2026-07-10-velos-remote-runtime-vendor-design.md](2026-07-10-velos-remote-runtime-vendor-design.md)
  (the outbound-only velos vendor).

## Goal

From the web UI a user can:

1. **Install** a publicly shared bundle by git URL (e.g. `obra/superpowers`); the server
   clones it, packs it to a zip artifact, and records it in the DB.
2. **Update** it (re-clone from the remembered source) and **delete** it.
3. **Select** any subset of installed bundles when launching a new session.

Every selected bundle's skills then appear in that session's agent prompt, its
`SessionStart` hooks fire, and its sibling `references/`/`scripts/` are readable — **on both
the local vendor and the velos vendor**, with one provisioning code path.

## Motivation

The plugin/skill primitive already exists (see the 2026-06-01 design): the runtime globs
`.claude/skills/*/SKILL.md` and a plugin library under `plugins_dir`, parses
`name`/`description`, lists skills in the prompt, runs `SessionStart` hooks in-sandbox, and
exposes the library as the reserved `horsie_shared` workspace. What is missing for the
**session server** is everything *around* that primitive:

1. **Server + DB ownership, not a CLI host lockfile.** Today install is
   `horsie plugin install <url>` → `git clone` into a host `plugins_dir` + a `plugins.json`
   lockfile, owned by the **CLI**, on the **host**. The session server has no API to manage
   plugins; the web UI can't install/list/update/remove them.
2. **Per-session selection, not all-or-none.** The current opt-in is a boolean
   (`use_plugins`) that surfaces *all installed* plugins or none. Users want to choose *which*
   bundles a given session gets.
3. **Cross-vendor provisioning.** The current mechanism is a **host directory** passed to the
   runtime as `--plugins-dir`. The velos vendor has no volumes and no host filesystem access,
   so it sets `plugins_dir = None` — **velos sessions get zero plugins today.** The server
   cannot write into a velos container; the only pipe in is the runtime's own outbound
   networking.

The **runtime-side** plugin machinery needs no change. It already accepts a populated
`plugins_dir` and does the right thing. This design supplies a new, vendor-uniform way to
*populate that dir per session* and a server/DB/web layer to *manage what goes in it*.

## The core generalization

> **The runtime's `plugins_dir` contract is unchanged. Only the populator changes: instead
> of an operator-global host directory (local-only), each session's runtime fetches its
> selected bundles by URL and unpacks them into its own plugins dir before it announces
> ready.** Because the runtime does the fetching over its *own outbound* connection, local
> and velos become the same path — the server never touches the runtime's filesystem.

Concretely, at session provisioning the server hands the runtime a **plugin manifest** (the
selected bundles as `{name, hash, url}`) plus a short-lived **capability token** and a
**plugins dir path**, all via the container/process environment. The runtime, during its
startup provisioning phase (before `RuntimeReady`):

1. reads the manifest, HTTP-`GET`s each bundle zip (verifying `hash`), unpacks into the
   plugins dir;
2. announces ready → the server's *existing* `ScanWorkspace(include_shared=true)` and
   `RunSessionStart` pick the bundles up and fire their hooks, exactly as today.

- **Local vendor:** base URL = `http://127.0.0.1:<http_port>`. The plugins dir is a
  per-session dir; a content-hash **cache** (`<data_dir>/plugins-cache/<hash>/`) that the
  runtime unpacks into once and links from preserves byte-level "shared on host" without
  cross-session write races.
- **Velos vendor:** base URL = `http://<advertise_host>:<http_port>` — reachable via the
  container's outbound NAT, the *same egress it already uses to dial back*. The plugins dir
  is an in-container dir. No volumes, no inbound networking, no velos change — consistent
  with the velos vendor's outbound-only ethos.
- **Future:** because the manifest carries a URL and the token is a bearer credential, the
  artifact can move behind a CDN or object store with a signed URL and **no runtime change**.

## Decisions

Settled during brainstorming; not open:

| # | Decision | Choice |
|---|----------|--------|
| 1 | Managed/selectable unit | **A plugin bundle** (may contain many skills + hooks + scripts + `.claude-plugin/plugin.json`). Install/update/delete and per-session select operate on whole bundles — reuses the existing plugin-library discovery + hooks unchanged. Not single-skill granularity. |
| 2 | Ingestion source (v1) | **Git repo / marketplace ref only.** Server `git clone`s, locates the plugin root, packs a zip, records the source so *update* re-clones. Zip-URL and upload are out of scope. |
| 3 | Manual authoring | **Out of scope.** No in-browser skill/bundle editor in v1 ("maybe we never need them"). The data model leaves room for a `source_kind` other than `git` later. |
| 4 | Provisioning (the generalization) | **Runtime pulls each bundle zip by authed URL and unpacks into its plugins dir.** One path for both vendors. The server serves the artifact; a CDN can later. Not server-side host writes; not a divergent per-vendor path. |
| 5 | Hook trust | **Installed bundles are trusted.** Installation is an authenticated admin action (the operator chose the git repo), the same trust level as today's operator-installed plugins, which already run hooks in-sandbox. Hooks run on **both** vendors. No per-bundle trust flag. |
| 6 | Versioning | **Latest at session start.** The persisted session spec stores bundle **names**; the concrete version/hash is resolved live from the DB at each provisioning. Re-attach (a fresh velos container) picks up the current version. No pinning; old artifacts are GC-able once unreferenced. |
| 7 | Auth for the artifact fetch | **Stateless HMAC-signed capability token** encoding `{session_id, allowed_hashes, exp}`, signed with a server secret. No server-side token table; re-minted on re-attach. CDN-migratable to a signed URL. |
| 8 | Relationship to the operator host `plugins_dir` | For the **session server**, DB bundles are the source of truth: the runtime's per-session plugins dir is populated by fetch, and `horsie_shared` resolves to it. An operator "base set" is expressed as bundles with `enabled_default = true`. The CLI/daemon job path keeps its existing host `plugins_dir` + lockfile, unchanged. |

## Data model

### New `plugins` table (`server/migrations/0002_plugins.sql`)

```sql
CREATE TABLE plugins (
    name            TEXT PRIMARY KEY,   -- canonical name (from plugin.json, else repo basename)
    source_kind     TEXT NOT NULL,      -- 'git' (only kind in v1)
    source_url      TEXT NOT NULL,      -- git remote
    source_ref      TEXT,               -- branch/tag; NULL = default branch
    version         TEXT,               -- resolved commit sha (or manifest version)
    description     TEXT,               -- plugin.json description, for the UI
    skill_count     INTEGER NOT NULL DEFAULT 0,  -- #SKILL.md, for the UI
    has_hooks       INTEGER NOT NULL DEFAULT 0,  -- bool, for the UI badge
    artifact_hash   TEXT NOT NULL,      -- sha256 of the zip = artifact filename
    artifact_size   INTEGER NOT NULL,
    enabled_default INTEGER NOT NULL DEFAULT 0,  -- pre-checked in the new-session modal
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
```

Follows the existing `0001_init.sql` conventions (TEXT PK, JSON/flat columns, a `settings`
kv table already present). The migration is additive — `sqlx migrate!` runs it on `open`.

### Artifact store (content-addressed, on the data dir)

The zip lives at `<data_dir>/plugins/<artifact_hash>.zip`. The DB holds only metadata + the
hash. Rationale over a SQLite blob: (a) the fetch endpoint streams a file, CDN-friendly;
(b) keeps the config DB small and its transactions cheap; (c) the data dir is already the
durable volume (`/data` in the deploy). GC: after `update`/`delete`, delete any
`<hash>.zip` no longer referenced by a `plugins` row.

## Ingestion (git → zip)

Server-side, in a new `server/src/plugins/` module. Install is an authenticated admin
action (same trust boundary the 2026-06-01 design established for `plugin install`), so the
server runs `git` directly (not sandboxed):

1. `git clone --depth 1 [--branch <ref>] <url>` into a temp dir.
2. **Locate the plugin root.** If `.claude-plugin/plugin.json` at repo root → that's the
   plugin. (Marketplace/`marketplace.json` sub-selection is a documented follow-up, matching
   the 2026-06-01 "out of scope" note.) Read `name`, `version`, `description`; fall back to
   repo basename + git sha when absent.
3. **Validate:** exposes ≥1 skill (reuse the discovery logic from `runtime/src/plugins.rs`,
   factored into a shared, non-runtime helper — see "Shared plugin-inspection" below);
   detect `has_hooks` by presence of a `SessionStart` entry in `hooks/hooks.json` (or a
   manifest `hooks` override).
4. **Pack + hash:** zip the plugin root deterministically (sorted entries, fixed mtimes so
   re-clones of identical trees hash identically), sha256 the bytes → `artifact_hash`, write
   `<data_dir>/plugins/<hash>.zip`.
5. **Upsert** the `plugins` row (records `source_url`/`source_ref`/resolved `version`).
6. GC the previous artifact if the hash changed and it's now unreferenced.

`update` re-runs steps 1–6 from the remembered `source_url`/`source_ref`. `delete` removes
the row and GCs the artifact.

**Shared plugin-inspection.** Skill discovery + hook detection currently live in
`runtime/src/plugins.rs` (compiled into the runtime). Factor the pure directory-inspection
parts (enumerate plugin dirs, resolve the manifest `skills` override, glob `*/SKILL.md`,
parse `name`/`description`, detect `SessionStart`) into a small crate/module usable by both
the runtime and the server, so install-time validation and runtime scanning agree. No
behavior change to the runtime path.

## HTTP surface (`server/src/http/plugins.rs`)

Management endpoints (mirror the `config.rs` handler style; return fluorite wire types):

- `GET  /api/plugins` → `Vec<PluginView>` — the library (metadata only, no bytes).
- `POST /api/plugins` `{ source_url, source_ref? }` → `PluginView` — install (git ingest).
- `POST /api/plugins/{name}/update` → `PluginView` — re-clone + re-pack.
- `PUT  /api/plugins/{name}` `{ enabled_default }` → `PluginView` — toggle the default flag.
- `DELETE /api/plugins/{name}` → 204.

Artifact endpoint (consumed by the **runtime**, token-guarded):

- `GET /api/plugins/artifacts/{hash}.zip` with `Authorization: Bearer <jwt>` → streams the
  zip. The handler verifies the HMAC token (§Decision 7), checks `hash ∈ allowed_hashes` and
  `exp` not passed, then streams `<data_dir>/plugins/<hash>.zip`. The token is a header (not a
  URL query) so it never lands in process args or access logs. Content-addressed path so it
  is a drop-in for a CDN origin later (which would swap the bearer for a signed URL).

Install/update/delete are long-ish (network + git); run the git work on a blocking task and
keep the DB write transactional. Errors map to `Api::unprocessable` (bad URL, no skills,
clone failure) vs `Api::internal`.

## Per-session selection & wiring

### Wire types — `models/fluorite/plugins.fl` (new package) + `session_api.fl`

```
// A library entry as shown in the web UI (metadata only).
struct PluginView {
    name: String,
    description: Option<String>,
    version: Option<String>,
    source_url: String,
    source_ref: Option<String>,
    skill_count: u32,
    has_hooks: bool,
    enabled_default: bool,
    artifact_size: u64,
}
struct PluginInstallInput { source_url: String, source_ref: Option<String> }
struct PluginDefaultInput { enabled_default: bool }
```

`CreateSessionRequest` (in `session_api.fl`) gains:

```
plugins: Option<Vec<String>>,   // selected bundle names; None ⇒ the enabled_default set
```

Regenerate `models` (Rust, via `build.rs`) and TS for `clients/ts` **and**
`clients/web/src/generated` (both `generate-types` scripts add `plugins.fl`; CI ts-drift job
must stay green).

### Server plumbing — `server/src/sessions/`

- `SessionSpec` gains `plugins: Vec<String>` (persisted in the session journal, so re-attach
  re-provisions the same *names*; versions re-resolve live per Decision 6).
- On `POST /api/sessions`: resolve `req.plugins` (or default to the `enabled_default` set),
  validate each name exists, store in `SessionSpec`.
- In `ensure_runtime()` → `write_runtime_spec()`: resolve the selected names against the DB
  to `{name, artifact_hash}` (current versions), mint the HMAC token over those hashes +
  session id, and put on the `RuntimeSpec`:
  - `plugin_manifest: Vec<PluginArtifactRef>` where `PluginArtifactRef { name, hash, url }`
    (`url` = the vendor's artifact base + `/api/plugins/artifacts/<hash>.zip`, no secret in it),
  - `plugins_token: String` — the bearer the runtime sends as `Authorization` on each fetch,
  - the target `plugins_dir` path (per-session).

### Vendor pass-through — `server/src/vendor/`

The base URL the runtime should dial for artifacts differs per vendor, so the vendor
supplies it via a new `RuntimeVendor::artifact_base_url(&self) -> String`:

- **local** → `http://127.0.0.1:<http_port>` (from `server.public_http_base`).
- **velos** → `http://<advertise_host>:<http_port>`; add `public_http_base`/`http_port` to
  `VelosVendorConfig`/`VelosView`/`VelosInput`.
- Both vendors inject three env vars into the runtime (velos via `spec.env` on the container
  command; local via the child process env): `HORSIE_PLUGIN_MANIFEST` (JSON of
  `[{name,hash,url}]`), `HORSIE_PLUGINS_TOKEN`, `HORSIE_PLUGINS_DIR`. This reuses the
  existing `spec.env`/provision plumbing — no new RPC.

The server must know its own externally-reachable HTTP base. Add `server.public_http_base`
(or derive from the bind addr for local + `advertise_host:http_port` for velos). This is the
one genuinely new config surface.

### Runtime fetch — `runtime/src/main.rs` + `runtime/src/plugins_fetch.rs` (new)

Before announcing `RuntimeReady` (in the same provisioning window the repo-clone provision
steps already use, emitting `RuntimeProvisioning`):

1. If `HORSIE_PLUGIN_MANIFEST` is set, for each entry: `GET` the `url`, stream to a temp
   file, verify sha256 == `hash`, unpack the zip into `HORSIE_PLUGINS_DIR/<name>/`.
2. **Local cache:** when `HORSIE_PLUGINS_CACHE` is set (local vendor points it at
   `<data_dir>/plugins-cache`), unpack once into `<cache>/<hash>/` and hardlink/symlink to
   `HORSIE_PLUGINS_DIR/<name>/`; on a cache hit skip the fetch entirely.
3. Set the runtime's `plugins_dir` (the existing `--plugins-dir` path) to
   `HORSIE_PLUGINS_DIR`. From here the *existing* scan + `RunSessionStart` machinery is
   untouched.

New runtime deps: a small HTTP client (`ureq` or `reqwest` with rustls), `zip`, `sha2`.
Compatible with the runtime's `--no-default-features` velos build (container is the sandbox).
LAN deploys use plain HTTP; HTTPS servers need the rustls path. A fetch/verify/unpack failure
emits a warning and omits that bundle (non-fatal, mirroring the "hook fails ⇒ skills still
listed" degradation) — a session never fails to start because a bundle is unavailable.

## Web UI (`clients/web`)

- New **Skills** page (mirrors `SettingsPage.tsx`): a table of installed bundles
  (name, version, description, `skill_count`, a "hooks" badge, a default toggle) with an
  **Install** form (git URL + optional ref) and per-row **Update** / **Delete**. Uses a
  `usePlugins` hook + `api/client.ts` methods (`plugins.list/install/update/delete/setDefault`).
- `NewSessionModal.tsx`: add a **bundle multi-select** (checkbox list from `GET /api/plugins`,
  the `enabled_default` ones pre-checked), alongside the existing model + vendor controls.
  The selected names go into `CreateSessionRequest.plugins`.
- Sidebar nav entry + `/skills` route.

## Deployment impact

- **Server image** (`docker/server.Dockerfile`) must include `git` (for ingestion) and allow
  outbound egress to git hosts. Note in `october/ops/horsie/RUNBOOK.md`.
- **Runtime image** (`docker/runtime.Dockerfile`) gains the HTTP/zip fetch capability
  (compiled into the binary — no new OS packages).
- **Config:** set `server.public_http_base` (local) and the velos vendor's
  `public_http_base`/`http_port` so containers can reach the artifact endpoint at
  `advertise_host`. In the homelab that is `http://192.168.68.60:3789` (already published).
- **HMAC secret:** a `server.artifact_token_secret` (env-overridable, e.g.
  `HORSIE_ARTIFACT_SECRET`); if unset, generate one at startup (fine for single-instance; a
  fixed secret is only needed if artifact URLs must survive a restart mid-provision).

## Failure handling & resume

- **No bundles selected / library empty:** manifest is empty, the runtime fetches nothing,
  `plugins_dir` is empty — behavior == a session with no plugins today (fully inert).
- **Bundle fetch/verify/unpack failure:** warned + skipped; session still starts.
- **Hook non-zero exit / timeout / missing interpreter:** unchanged from the 2026-06-01
  design — logged, that bundle's bootstrap omitted, never fatal.
- **Git ingest failure** (bad URL, no skills, clone error): `POST /api/plugins` returns 422
  with the reason; nothing is written.
- **Resume / re-attach:** `SessionSpec.plugins` (names) is journaled; each provisioning
  re-resolves current hashes, re-mints the token, re-fetches. A velos re-attach (fresh
  container) transparently re-provisions. Nothing about artifacts is journaled.
- **Artifact GC race:** never GC a hash still referenced by a `plugins` row; a session that
  resolved a now-updated bundle simply fetches the new hash on its next provisioning.

## Security notes

- **Trust:** installed bundles execute hook code on the host (local vendor) and in the
  container (velos). Per Decision 5 this is accepted — install is an authenticated admin
  action, the same posture as the existing `plugin install`. The web install endpoint must
  therefore be an authenticated/admin surface (same auth as the rest of `/api`).
- **Token scope:** the capability token authorizes only the specific hashes of one session
  for a short TTL, so a leaked runtime env cannot enumerate the whole library.
- **SSRF at install:** `git clone <user-url>` reaches operator-supplied hosts. Acceptable
  for an admin action; note it (no allowlist in v1).

## Testing

- **Migration:** `0002_plugins.sql` applies on a fresh temp DB; columns/defaults correct.
- **Ingestion** (`server/src/plugins/`): install from a `file://` git fixture (a plugin with
  one skill + one `SessionStart` hook + a `references/` sibling) → row written, artifact zip
  on disk, hash stable across two identical clones, `skill_count`/`has_hooks` correct;
  missing-manifest fallback to repo basename; a repo with no skills → 422; `update` re-packs;
  `delete` GCs the artifact; unreferenced-only GC (two rows sharing a hash keep it).
- **Artifact endpoint:** valid token + allowed hash streams bytes; wrong hash / expired /
  bad signature → 403; unknown hash → 404.
- **Token:** round-trip sign/verify; `allowed_hashes` enforcement; expiry.
- **Server session wiring:** `CreateSessionRequest.plugins` resolves (explicit set; `None` →
  `enabled_default` set); unknown name → 422; `SessionSpec.plugins` persists;
  `write_runtime_spec` builds the manifest with current hashes + a valid token + per-vendor
  URLs.
- **Runtime fetch** (`runtime/src/plugins_fetch.rs`): manifest → fetch (against a stub HTTP
  server) → hash mismatch rejected → good zip unpacked into `plugins_dir`; cache hit skips
  fetch; a fetch failure is non-fatal and the runtime still reaches ready.
- **e2e** (`server` `tests/`, mock/local vendor): install a fixture bundle, create a session
  selecting it, assert the agent's prompt contains the bundle's skill + `SessionStart`
  sentinel and the session reaches `Idle`; create a second session selecting nothing and
  assert neither appears. (Velos path validated by the existing session e2e harness with a
  mock artifact server if feasible; otherwise smoke-tested on the homelab per the RUNBOOK.)
- **ts-drift:** `clients/ts` regenerated with `plugins.fl`, `git diff --exit-code` clean.

## Out of scope (YAGNI / follow-ups)

- **Manual/in-browser authoring** of skills or bundles (Decision 3).
- **Non-git sources:** zip-URL ingest, zip upload, `marketplace.json` sub-selection. The
  `source_kind` column leaves room.
- **Version pinning** per session (Decision 6 is latest-at-start).
- **Per-bundle hook trust gating** (Decision 5 trusts installed bundles).
- **Private-repo auth for ingest** — could later reuse the GitHub connection from
  [2026-07-11-github-repos-design.md](2026-07-11-github-repos-design.md) (#103).
- **CDN/object-store artifact origin** — the content-addressed URL + bearer token are
  designed for it, but v1 serves from the server.
- **Convergence with the CLI/daemon `plugin install` lockfile path** — the daemon job path
  keeps its host `plugins_dir`; unifying it onto the DB store is a separate effort.
- **Sub-bundle skill selection** (enable individual skills within a bundle) — v1 selects
  whole bundles.

## Touched files (summary)

- `server/migrations/0002_plugins.sql` — new `plugins` table.
- `models/fluorite/plugins.fl` (new) — `PluginView`/`PluginInstallInput`/`PluginDefaultInput`;
  `session_api.fl` — `CreateSessionRequest.plugins`.
- `server/src/plugins/` (new) — git ingest, pack/hash, artifact store, GC; shared
  plugin-inspection helper (factored from `runtime/src/plugins.rs`).
- `server/src/http/plugins.rs` (new) — the 5 management routes + the artifact route; wired in
  the router.
- `server/src/config/` — read/write `plugins` rows (or a dedicated `PluginStore` alongside
  `ConfigStore`); `server.public_http_base` + `artifact_token_secret` config.
- `server/src/sessions/spec.rs`, `session_actor.rs`, `http/handlers.rs` — `SessionSpec.plugins`;
  resolve/validate selection; build manifest + token in `write_runtime_spec`.
- `server/src/vendor/{mod.rs,local.rs,velos.rs}` — per-vendor artifact base URL; inject
  `HORSIE_PLUGIN_MANIFEST`/`HORSIE_PLUGINS_TOKEN`/`HORSIE_PLUGINS_DIR` (+ `HORSIE_PLUGINS_CACHE`
  for local); `VelosVendorConfig`/`VelosView`/`VelosInput` gain `public_http_base`/`http_port`.
- `runtime/src/plugins_fetch.rs` (new), `runtime/src/main.rs` — manifest fetch/verify/unpack
  + local cache, before ready; new deps (`ureq`/`reqwest`, `zip`, `sha2`).
- `clients/web` — Skills page, `NewSessionModal` bundle multi-select, `api/client.ts`,
  `usePlugins` hook, route + nav; `src/generated` regenerated.
- `clients/ts` — `generate-types` includes `plugins.fl`.
- `docker/server.Dockerfile` (add `git`), `october/ops/horsie/RUNBOOK.md`/`docker-compose.yml`
  (config for `public_http_base`, artifact secret).
```
