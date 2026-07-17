# Live vendor activation + inline-only secrets — implementation plan

- **Spec:** `docs/superpowers/specs/2026-07-17-live-vendor-activation-inline-secrets-design.md`
- **Branch:** `live-vendor-activation`

## Goal

Drop `api_key_env`/`token_env` env-var indirection (inline secrets only), and
make vendor add/edit apply live in the common case instead of always
requiring a restart.

## Architecture

Two independent changes to the settings/vendor system in `server/src/config`,
`server/src/sessions`, and `server/src/vendor`. Part 1 shrinks the provider/
vendor wire+DB shape (drop two fields, one migration). Part 2 turns
`ServerDeps.vendors`/`OpenedConfig.vendors` from a frozen `HashMap` into a
`SharedVendors = Arc<RwLock<HashMap<...>>>` (mirroring the existing
`SharedProviderRegistry` hot-swap pattern) and teaches `ConfigStore::update()`
to reconcile that live map against each vendor row after persisting, instead
of only building vendors once at boot.

## Tech stack

Rust workspace (sqlx/SQLite, tokio, axum), fluorite-generated wire types
(`models/fluorite/settings.fl` → Rust via `build.rs` + TS via `make
ts-types`), React/Vite web client.

## File structure

| File | Responsibility |
|---|---|
| `server/migrations/0006_drop_api_key_env.sql` | New migration: drop `providers.api_key_env`. |
| `server/src/config/store.rs` | `ProviderRow`/`VelosConfig`/`build_anthropic`/`resolve_velos_token` collapse to inline-only; `OpenedConfig`/`DbConfigStore` hold live `SharedVendors` + per-vendor error/restart state; `update()` reconciles vendors live; `build_one_vendor`/`BuiltVendor` factor per-row vendor building out of `build_vendors`. |
| `models/fluorite/settings.fl` | Drop `api_key_env`/`token_env` fields; add `VendorView.error`; reword `restart_required` doc. |
| `server/src/sessions/spec.rs` | New `SharedVendors` type alias; `ServerDeps.vendors` becomes that type. |
| `server/src/sessions/session_actor.rs` | `fn vendor()` reads through the `RwLock`; test harness wraps its `HashMap`. |
| `server/src/sessions/supervisor.rs` | `fn test_deps` wraps its `HashMap`. |
| `server/src/http/mod.rs` | `fn test_state` wraps its `HashMap` (a third test-helper site found while reading the code, not listed in the brief). |
| `cli/src/serve.rs` | No-op once both sides are `SharedVendors` — verified, not edited. |
| `server/src/vendor/velos.rs` | `VelosMutableSettings` + `RwLock` inside `VelosVendor`; `reconfigure()`; listener/registry/endpoint stay immutable. |
| `server/src/vendor/mod.rs` | Export `VelosMutableSettings`. |
| `clients/ts/src/generated/**`, `clients/web/src/api/types.ts` | Regenerated, not hand-edited. |
| `clients/web/src/pages/SettingsPage.tsx` | Drop env-var inputs/state; show per-vendor `error`; reword restart banner + Velos section copy. |

## Task 1 — migration + inline-only provider secrets

**Files:** `server/migrations/0006_drop_api_key_env.sql` (new);
`server/src/config/store.rs` lines 328-334 (`ProviderRow`), 400-430
(`build_registry`), 432-461 (`build_anthropic`), 200-213 (`update()` provider
INSERT), 634-641 (`provider_view`), 686-706 (`read_providers`), 757+ (test
mod, add migration test).

**Interfaces produced:** `build_anthropic(base_url: Option<&str>, api_key:
Option<&str>, model_id: &str, max_tokens: Option<u32>) -> Result<Arc<dyn
LlmProvider>, String>` (drops the `api_key_env` parameter — task 3's
`build_registry` call site already matches this new arity).

- [ ] Write a failing test in `server/src/config/store.rs`'s `mod tests`:
  ```rust
  #[tokio::test]
  async fn migration_0006_drops_api_key_env_and_preserves_rows() {
      let dir = tempfile::tempdir().unwrap();
      let url = format!("sqlite://{}/old.db", dir.path().display());
      let opts = SqliteConnectOptions::from_str(&url)
          .unwrap()
          .create_if_missing(true);
      let pool = SqlitePool::connect_with(opts).await.unwrap();

      // Mirror the pre-0006 `providers` shape (0001_init.sql).
      sqlx::query(
          "CREATE TABLE providers (
              name TEXT PRIMARY KEY, kind TEXT NOT NULL, base_url TEXT,
              api_key_env TEXT, api_key TEXT)",
      )
      .execute(&pool)
      .await
      .unwrap();
      sqlx::query(
          "INSERT INTO providers (name, kind, base_url, api_key_env, api_key) \
           VALUES ('p', 'anthropic', NULL, 'OLD_ENV_VAR', 'sk-inline')",
      )
      .execute(&pool)
      .await
      .unwrap();

      sqlx::query(include_str!("../../migrations/0006_drop_api_key_env.sql"))
          .execute(&pool)
          .await
          .expect("DROP COLUMN should succeed on the bundled sqlite");

      let cols: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info('providers')")
          .fetch_all(&pool)
          .await
          .unwrap()
          .iter()
          .map(|r| r.try_get::<String, _>("name").unwrap())
          .collect();
      assert!(!cols.iter().any(|c| c == "api_key_env"));

      let row = sqlx::query("SELECT name, api_key FROM providers WHERE name = 'p'")
          .fetch_one(&pool)
          .await
          .unwrap();
      assert_eq!(row.try_get::<String, _>("name").unwrap(), "p");
      assert_eq!(row.try_get::<Option<String>, _>("api_key").unwrap().as_deref(), Some("sk-inline"));
  }
  ```
  This won't compile yet (the migration file doesn't exist) — that's the
  failing step.
- [ ] Run `cargo test -p server migration_0006 2>&1 | tail -30`, confirm it
  fails to compile (missing file for `include_str!`).
- [ ] Create `server/migrations/0006_drop_api_key_env.sql`:
  ```sql
  -- Inline secrets only now — the env-var indirection was a second,
  -- silently-broken-until-restart failure mode with no remaining benefit once
  -- the DB path is the supported one (see the live-vendor-activation spec).
  ALTER TABLE providers DROP COLUMN api_key_env;
  ```
- [ ] Run `cargo test -p server migration_0006`, confirm it passes.
- [ ] Commit:
  ```
  git add server/migrations/0006_drop_api_key_env.sql server/src/config/store.rs
  git commit -m "settings: migration dropping providers.api_key_env"
  ```
- [ ] Now collapse the Rust side. Edit `ProviderRow` (drop the field):
  ```rust
  struct ProviderRow {
      name: String,
      kind: String,
      base_url: Option<String>,
      api_key: Option<String>,
  }
  ```
- [ ] `read_providers`: drop `api_key_env` from the `SELECT` and the struct
  build:
  ```rust
  let rows = sqlx::query("SELECT name, kind, base_url, api_key FROM providers ORDER BY name")
      .fetch_all(ex)
      .await?;
  let mut out = Vec::with_capacity(rows.len());
  for r in &rows {
      out.push(ProviderRow {
          name: r.try_get("name")?,
          kind: r.try_get("kind")?,
          base_url: r.try_get("base_url")?,
          api_key: r.try_get("api_key")?,
      });
  }
  ```
- [ ] `update()`'s provider INSERT (was line 200-212): drop the column +
  bind:
  ```rust
  let api_key = resolve_secret(&p.api_key, keep.get(name).copied());
  sqlx::query("INSERT INTO providers (name, kind, base_url, api_key) VALUES (?, ?, ?, ?)")
      .bind(name)
      .bind(&p.kind)
      .bind(trimmed(&p.base_url))
      .bind(api_key)
      .execute(&mut *tx)
      .await
      .map_err(|e| e.to_string())?;
  ```
- [ ] `build_registry`'s call site drops the `api_key_env` arg:
  ```rust
  build_anthropic(p.base_url.as_deref(), p.api_key.as_deref(), &m.model_id, max_tokens)?,
  ```
- [ ] `build_anthropic` collapses (drop the `api_key_env` param + branch):
  ```rust
  fn build_anthropic(
      base_url: Option<&str>,
      api_key: Option<&str>,
      model_id: &str,
      max_tokens: Option<u32>,
  ) -> Result<Arc<dyn LlmProvider>, String> {
      let key: Option<Secret> = match api_key {
          Some(k) if !k.is_empty() => Some(Secret::from(k)),
          Some(_) => return Err("inline api_key is empty".into()),
          None => None,
      };
      let mut p = match key {
          Some(k) => AnthropicProvider::with_api_key(k).map_err(|e| e.to_string())?,
          None => AnthropicProvider::new().map_err(|e| e.to_string())?,
      };
      p = p.with_model(model_id).with_max_tokens(max_tokens);
      if let Some(u) = base_url {
          p = p.with_base_url(u);
      }
      Ok(Arc::new(p))
  }
  ```
- [ ] `provider_view`: `ProviderRow` no longer carries `api_key_env`, but the
  wire `ProviderView` still requires the field until task 3 — stopgap it to
  `None` (task 3 deletes this line):
  ```rust
  fn provider_view(r: &ProviderRow) -> ProviderView {
      ProviderView {
          name: r.name.clone(),
          kind: r.kind.clone(),
          base_url: r.base_url.clone(),
          api_key_env: None, // dropped in task 3
          has_inline_key: r.api_key.as_deref().is_some_and(|s| !s.is_empty()),
      }
  }
  ```
- [ ] Run `cargo test -p server config::store`, confirm the whole module's
  tests (including the pre-existing ones) still pass unchanged — none of
  them assert on `api_key_env`.
- [ ] Commit:
  ```
  git add server/src/config/store.rs
  git commit -m "settings: drop api_key_env from ProviderRow/build_anthropic"
  ```

## Task 2 — inline-only velos token

**Files:** `server/src/config/store.rs` lines 352-375 (`VelosConfig`),
528-546 (`resolve_velos_token`), 653-668 (`velos_view`).

**Interfaces produced:** `resolve_velos_token(vc: &VelosConfig) ->
Result<Option<Secret>, String>` — same signature, inline-only body.

- [ ] `VelosConfig`: drop the `token_env` field:
  ```rust
  #[derive(Deserialize)]
  struct VelosConfig {
      server_url: String,
      image: String,
      advertise_host: String,
      #[serde(default)]
      token: Option<Secret>,
      #[serde(default = "default_runtime_bin")]
      runtime_bin: String,
      #[serde(default = "default_workspace_root")]
      workspace_root: String,
      #[serde(default = "default_listen")]
      listen: String,
      #[serde(default = "default_cpu")]
      cpu: u32,
      #[serde(default = "default_memory_mib")]
      memory_mib: u64,
      #[serde(default = "default_connect_timeout_secs")]
      connect_timeout_secs: u64,
      #[serde(default)]
      http_port: Option<u32>,
  }
  ```
  (Old stored JSON blobs may still have a `"token_env"` key — `serde`
  silently ignores unknown fields here since `VelosConfig` has no
  `#[serde(deny_unknown_fields)]`, so this is a safe, migration-free
  collapse.)
- [ ] `resolve_velos_token` collapses:
  ```rust
  fn resolve_velos_token(vc: &VelosConfig) -> Result<Option<Secret>, String> {
      match &vc.token {
          Some(t) if t.is_empty() => Err("velos inline token is empty".into()),
          Some(t) => Ok(Some(t.clone())),
          None => Ok(None),
      }
  }
  ```
- [ ] `velos_view`: `vc.token_env` no longer exists — stopgap to `None`
  (task 3 deletes this line):
  ```rust
  fn velos_view(vc: &VelosConfig) -> VelosView {
      VelosView {
          server_url: vc.server_url.clone(),
          image: vc.image.clone(),
          advertise_host: vc.advertise_host.clone(),
          token_env: None, // dropped in task 3
          has_inline_token: vc.token.as_ref().is_some_and(|t| !t.is_empty()),
          runtime_bin: vc.runtime_bin.clone(),
          workspace_root: vc.workspace_root.clone(),
          listen: vc.listen.clone(),
          cpu: vc.cpu,
          memory_mib: vc.memory_mib,
          connect_timeout_secs: vc.connect_timeout_secs,
          http_port: vc.http_port,
      }
  }
  ```
- [ ] Run `cargo test -p server config::store`, confirm green (existing
  `velos_vendor_persists_redacted_and_flags_restart` test still passes — it
  never asserted on `token_env`).
- [ ] Commit:
  ```
  git add server/src/config/store.rs
  git commit -m "settings: drop token_env from VelosConfig/resolve_velos_token"
  ```

## Task 3 — drop the fields from the wire schema

**Files:** `models/fluorite/settings.fl` lines 14-15, 56, 100, 132;
`server/src/config/store.rs` (remove the two stopgap lines from task 1/2,
plus `insert_trimmed(&mut m, "token_env", &v.token_env)` at line 584, plus
test fixtures at ~793-801 and ~890-933).

**Interfaces produced:** `ProviderView`/`ProviderInput`/`VelosView`/
`VelosInput` (regenerated) no longer have `api_key_env`/`token_env`.

- [ ] Edit `models/fluorite/settings.fl`: remove `api_key_env` from
  `ProviderView` (was line 14-15) and `ProviderInput` (was line 100), and
  `token_env` from `VelosView` (was line 56) and `VelosInput` (was line 132).
  Resulting `ProviderView`/`ProviderInput`/`VelosView`/`VelosInput`:
  ```
  struct ProviderView {
      name: String,
      kind: String,
      base_url: Option<String>,
      has_inline_key: bool,
  }
  ```
  ```
  struct VelosView {
      server_url: String,
      image: String,
      advertise_host: String,
      has_inline_token: bool,
      runtime_bin: String,
      workspace_root: String,
      listen: String,
      cpu: u32,
      memory_mib: u64,
      connect_timeout_secs: u64,
      http_port: Option<u32>,
  }
  ```
  ```
  struct ProviderInput {
      name: String,
      kind: String,
      base_url: Option<String>,
      /// New inline key. Omit to keep the existing stored key; "" to clear.
      api_key: Option<String>,
  }
  ```
  ```
  struct VelosInput {
      server_url: String,
      image: String,
      advertise_host: String,
      token: Option<String>,
      runtime_bin: Option<String>,
      workspace_root: Option<String>,
      listen: Option<String>,
      cpu: Option<u32>,
      memory_mib: Option<u64>,
      connect_timeout_secs: Option<u64>,
      http_port: Option<u32>,
  }
  ```
  Also update the module doc comment at the top (line 3-4) which says
  "provider/vendor views carry only the env-var name and a boolean flag" —
  reword to "a boolean flag for a stored inline secret".
- [ ] Run `cargo build -p server 2>&1 | tail -60` — expect compile errors at
  the two stopgap lines (`api_key_env: None,` / `token_env: None,` — now
  unknown fields) and at `insert_trimmed(&mut m, "token_env", &v.token_env)`
  (`v.token_env` no longer exists) and the two test fixtures. This is the
  "failing test" step for this task (a compile failure is the sharpest
  possible failing test for a type-level change).
- [ ] Remove the `api_key_env: None,` line from `provider_view` (task 1's
  stopgap) and the `token_env: None,` line from `velos_view` (task 2's
  stopgap).
- [ ] Remove `insert_trimmed(&mut m, "token_env", &v.token_env);` from
  `build_vendor_config` (was line 584).
- [ ] Fix the test fixture `provider()` helper (was lines 793-801): drop
  `api_key_env: None,`:
  ```rust
  fn provider(name: &str, key: Option<&str>) -> ProviderInput {
      ProviderInput {
          name: name.into(),
          kind: "anthropic".into(),
          base_url: Some("http://localhost:1".into()),
          api_key: key.map(str::to_string),
      }
  }
  ```
- [ ] Fix the `velos_vendor_persists_redacted_and_flags_restart` test's
  `VelosInput` literal (was ~line 900-913): drop `token_env: None,`.
- [ ] Run `cargo build -p server`, confirm clean. Run `cargo test -p server
  config::store`, confirm green.
- [ ] Commit:
  ```
  git add models/fluorite/settings.fl server/src/config/store.rs
  git commit -m "settings: drop api_key_env/token_env from the wire schema"
  ```

## Task 4 — regenerate TS clients

**Files:** `clients/ts/src/generated/**` (regenerated), `clients/web/src/api/types.ts`
(regenerated via its own codegen step).

- [ ] Run `make ts-types` from the repo root. Confirm it regenerates
  `clients/ts/src/generated/settings/{providerView,providerInput,velosView,velosInput}.ts`
  with `apiKeyEnv`/`tokenEnv` gone, and `npm run typecheck` (invoked by the
  target) passes.
- [ ] Run `git diff --stat clients/ts/src/generated` to eyeball the diff is
  exactly the expected field removals.
- [ ] Run `cd clients/web && bun install && bun run generate-types` —
  confirm it regenerates `clients/web/src/api/types.ts` (or wherever its
  codegen writes) with the same fields gone.
- [ ] Run `cd clients/web && bun run build` — expect it to **fail** at this
  point (`SettingsPage.tsx` still references `apiKeyEnv`/`tokenEnv`). That
  failure is the expected "failing test" for this task — task 5 fixes it.
- [ ] Commit the regenerated files (the web build stays red until task 5,
  committed as prep so task 5's diff is UI-only):
  ```
  git add clients/ts/src/generated clients/web/src/api
  git commit -m "settings: regenerate clients for api_key_env/token_env removal"
  ```

## Task 5 — web UI: drop env-var inputs (Part 1)

**Files:** `clients/web/src/pages/SettingsPage.tsx` lines 46-121 (draft
types + `to*Drafts`), 184-217 (`save`'s input building), 309 (Providers
section desc), 377 (Velos section desc — found while reading, not in the
brief's line list but directly stale after this change), 699-735
(`ProviderRow`), 789-882 (`VelosRow`).

- [ ] Remove `apiKeyEnv`/`tokenEnv` from the draft types:
  ```ts
  type ProviderDraft = {
    name: string;
    baseUrl: string;
    apiKeyInput: string; // "" = leave the stored key unchanged
    hasInlineKey: boolean;
  };
  ```
  ```ts
  type VelosDraft = {
    name: string;
    serverUrl: string;
    image: string;
    advertiseHost: string;
    tokenInput: string; // "" = keep stored token
    hasInlineToken: boolean;
    runtimeBin: string;
    workspaceRoot: string;
    listen: string;
    cpu: string;
    memoryMib: string;
    connectTimeoutSecs: string;
    active: boolean;
  };
  ```
- [ ] Update `toProviderDrafts`/`toVelosDrafts` to drop the two fields:
  ```ts
  const toProviderDrafts = (v: SettingsView): ProviderDraft[] =>
    v.providers.map((p) => ({
      name: p.name,
      baseUrl: p.baseUrl ?? "",
      apiKeyInput: "",
      hasInlineKey: p.hasInlineKey,
    }));
  ```
  ```ts
  const toVelosDrafts = (v: SettingsView): VelosDraft[] =>
    v.vendors.flatMap((vd) =>
      vd.config?.kind === "Velos"
        ? [
            {
              name: vd.name,
              serverUrl: vd.config.value.serverUrl,
              image: vd.config.value.image,
              advertiseHost: vd.config.value.advertiseHost,
              tokenInput: "",
              hasInlineToken: vd.config.value.hasInlineToken,
              runtimeBin: vd.config.value.runtimeBin,
              workspaceRoot: vd.config.value.workspaceRoot,
              listen: vd.config.value.listen,
              cpu: num(vd.config.value.cpu),
              memoryMib: num(vd.config.value.memoryMib),
              connectTimeoutSecs: num(vd.config.value.connectTimeoutSecs),
              active: vd.active,
            },
          ]
        : [],
    );
  ```
- [ ] Update `save()`'s input building: drop `apiKeyEnv`/`tokenEnv`:
  ```ts
  const providerInputs: ProviderInput[] = providers.map((p) => ({
    name: p.name.trim(),
    kind: "anthropic",
    baseUrl: p.baseUrl.trim() || undefined,
    apiKey: p.apiKeyInput === "" ? undefined : p.apiKeyInput,
  }));
  ```
  and in the velos vendor input's `value`:
  ```ts
  value: {
    serverUrl: v.serverUrl.trim(),
    image: v.image.trim(),
    advertiseHost: v.advertiseHost.trim(),
    token: v.tokenInput === "" ? undefined : v.tokenInput,
    runtimeBin: v.runtimeBin.trim() || undefined,
    workspaceRoot: v.workspaceRoot.trim() || undefined,
    listen: v.listen.trim() || undefined,
    cpu: v.cpu.trim() ? Number(v.cpu.trim()) : undefined,
    memoryMib: v.memoryMib.trim() ? Number(v.memoryMib.trim()) : undefined,
    connectTimeoutSecs: v.connectTimeoutSecs.trim()
      ? Number(v.connectTimeoutSecs.trim())
      : undefined,
  },
  ```
- [ ] Update the "Add provider"/"Add velos vendor" seed objects (drop the
  two fields from each literal).
- [ ] `ProviderRow`: remove the "API key env var" `TextField` (was lines
  719-724); keep the inline-key field. Update the grid to `grid-cols-2` with
  the remaining 3 fields (Name, Base URL, Inline key) wrapping naturally.
- [ ] `VelosRow`: remove the "Token env var" `TextField` (was lines 823-828);
  keep the inline-token field.
- [ ] Providers `Section`'s `desc` (was line 309): drop the now-false "prefer
  an env var" line:
  ```tsx
  desc="Anthropic-compatible API endpoints."
  ```
- [ ] Velos `Section`'s `desc` (was line 377) currently says "changes apply
  on the next server restart" — no longer true after Part 2, but Part 2
  hasn't landed yet in this task's scope. Leave a note for task 13 to fix it
  (don't touch it here — changing it now would be inaccurate until Part 2
  ships); confirmed by grep it's the only other stale-after-Part-2 string
  besides the restart banner (already tracked for task 13).
- [ ] Run `cd clients/web && bun run build`, confirm it passes (no more
  `apiKeyEnv`/`tokenEnv` references).
- [ ] Commit:
  ```
  git add clients/web/src/pages/SettingsPage.tsx
  git commit -m "web: drop env-var inputs for provider/velos secrets"
  ```

**Part 1 complete here** — full gate should be green:
```
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny check advisories bans licenses sources --all-features
make ts-types && git diff --exit-code clients/ts/src/generated
cd clients/web && bun run build
```
Run this now (not just at the very end) before starting Part 2, so any
regression is caught before it's compounded by task 6+.

## Task 6 — `SharedVendors` type alias

**Files:** `server/src/sessions/spec.rs` line 18 (add alias), line 133
(`ServerDeps.vendors`); `server/src/sessions/supervisor.rs` lines 413-424
(`test_deps`); `server/src/sessions/session_actor.rs` lines 761-782
(`harness_custom`); `server/src/http/mod.rs` lines 174-212 (`test_state` —
found while reading the code, a third test-helper site the brief didn't
list).

**Interfaces produced:**
```rust
pub type SharedVendors = Arc<RwLock<HashMap<String, Arc<dyn RuntimeVendor>>>>;
```
`ServerDeps.vendors: SharedVendors` (was `HashMap<String, Arc<dyn
RuntimeVendor>>`).

- [ ] In `server/src/sessions/spec.rs`, add the alias right after
  `SharedProviderRegistry` (line 18):
  ```rust
  /// LLM providers keyed by model alias, behind a shared lock so the settings API
  /// can swap the whole set live. Read once per turn in
  /// [`crate::sessions::session_actor::SessionActor::ensure_agent`]; the guard is
  /// never held across an `.await`.
  pub type SharedProviderRegistry = Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>>;

  /// Runtime vendors keyed by name, behind a shared lock so a settings-API vendor
  /// edit can activate/reconfigure/retire a vendor without a restart. Read once
  /// per provision call in [`crate::sessions::session_actor::SessionActor::vendor`].
  pub type SharedVendors = Arc<RwLock<HashMap<String, Arc<dyn RuntimeVendor>>>>;
  ```
- [ ] Change `ServerDeps.vendors` (was line 133):
  ```rust
  /// Runtime vendors keyed by the session spec's `vendor` name.
  pub vendors: SharedVendors,
  ```
- [ ] This will fail to compile everywhere a plain `HashMap` is still handed
  to `ServerDeps`/`OpenedConfig` — that's the failing-test signal for this
  task. Run `cargo build --workspace 2>&1 | tail -80` to enumerate them
  (expect `server/src/config/store.rs`, `server/src/sessions/supervisor.rs`,
  `server/src/sessions/session_actor.rs`, `server/src/http/mod.rs`,
  `cli/src/serve.rs` to fail).
- [ ] Fix `supervisor.rs`'s `test_deps` (was lines 413-424):
  ```rust
  fn test_deps(tmp: &tempfile::TempDir) -> ServerDeps {
      let mut vendors: HashMap<String, Arc<dyn RuntimeVendor>> = HashMap::new();
      vendors.insert("mock".into(), Arc::new(MockVendor::new()));
      ServerDeps {
          provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
          vendors: Arc::new(std::sync::RwLock::new(vendors)),
          state_dir: tmp.path().to_path_buf(),
          github_tokens: None,
          mcp: None,
          plugins: None,
      }
  }
  ```
- [ ] Fix `session_actor.rs`'s `harness_custom` (was lines 761-782), same
  pattern:
  ```rust
  let mut vendors: HashMap<String, Arc<dyn crate::vendor::RuntimeVendor>> = HashMap::new();
  vendors.insert("mock".into(), vendor.clone());
  let deps = ServerDeps {
      provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
      vendors: Arc::new(std::sync::RwLock::new(vendors)),
      state_dir: tmp.path().to_path_buf(),
      github_tokens,
      mcp: None,
      plugins: None,
  };
  ```
- [ ] Fix `http/mod.rs`'s `test_state` (was lines 174-212), same pattern:
  ```rust
  let mut vendors: HashMap<String, Arc<dyn RuntimeVendor>> = HashMap::new();
  vendors.insert("mock".into(), Arc::new(MockVendor::new()));
  // ... (unchanged DbConfigStore::open / github / plugins / mcp setup) ...
  let deps = ServerDeps {
      provider_registry: opened.registry,
      vendors: Arc::new(std::sync::RwLock::new(vendors)),
      state_dir: tmp.path().to_path_buf(),
      github_tokens: None,
      mcp: Some(mcp.clone()),
      plugins: None,
  };
  ```
- [ ] `server/src/config/store.rs` and `cli/src/serve.rs` still won't
  compile (`OpenedConfig.vendors` is still a plain `HashMap`) — that's
  expected; task 7 fixes `store.rs`, and task 9 verifies `serve.rs`. Confirm
  the *only* remaining compile errors are in `store.rs`, e.g. run:
  ```
  cargo build -p server --lib 2>&1 | grep "^error" 
  ```
  and eyeball that every error points at `config/store.rs`.
- [ ] Commit:
  ```
  git add server/src/sessions/spec.rs server/src/sessions/supervisor.rs server/src/sessions/session_actor.rs server/src/http/mod.rs
  git commit -m "sessions: add SharedVendors, wrap ServerDeps.vendors"
  ```
  (This commit alone doesn't build the workspace — `store.rs` isn't fixed
  yet. That's fine for an interior commit in this branch's history; task 7's
  commit restores a green build. If a stricter per-commit-green policy is
  preferred, squash tasks 6+7 into one commit instead — noted here as an
  explicit, deliberate call given the two changes are inseparable at the
  type level.)

## Task 7 — live `vendors` map in `DbConfigStore`

**Files:** `server/src/config/store.rs` lines 10-32 (imports), 52-71
(`OpenedConfig`/`DbConfigStore`), 76-120 (`open`), 139-163 (`vendors_view`),
279-294 (`update()`'s `default_vendor` validation).

**Interfaces produced:** `OpenedConfig.vendors: SharedVendors`;
`DbConfigStore` gains a `vendors: SharedVendors` field (no `active_vendors`
field anymore).

- [ ] Add `SharedVendors` to the import (line 11):
  ```rust
  use crate::sessions::spec::{SharedProviderRegistry, SharedVendors};
  ```
- [ ] `OpenedConfig.vendors` (was line 55):
  ```rust
  pub vendors: SharedVendors,
  ```
- [ ] `DbConfigStore` struct (was lines 61-71) — replace `active_vendors`
  with a live `vendors` field:
  ```rust
  pub struct DbConfigStore {
      pool: SqlitePool,
      registry: SharedProviderRegistry,
      default_vendor: RwLock<String>,
      /// Live runtime vendors, kept in sync with the DB by `update()`'s
      /// reconciliation so most vendor edits apply without a restart.
      vendors: SharedVendors,
      /// Set once a persisted change (a vendor edit) needs a restart, so the
      /// view reports `restart_required` until then.
      vendors_dirty: AtomicBool,
      info: ServerInfo,
  }
  ```
  (`vendor_errors`/`velos_instances` land in task 11/12 — keep this task's
  diff minimal: just the `HashMap` → `SharedVendors` plumbing.)
- [ ] `open()` (was lines 76-120) — wrap the map once, drop the
  `active_vendors` snapshot:
  ```rust
  pub async fn open(db_url: &str, deps: StoreDeps) -> Result<OpenedConfig, String> {
      let pool = open_pool(db_url).await?;

      let provs = read_providers(&pool).await.map_err(|e| e.to_string())?;
      let mods = read_models(&pool).await.map_err(|e| e.to_string())?;
      let registry: SharedProviderRegistry =
          Arc::new(RwLock::new(build_registry(&provs, &mods)?));

      let vendor_rows = read_vendors(&pool).await.map_err(|e| e.to_string())?;
      let vendors = build_vendors(
          &vendor_rows,
          deps.runtime_bin,
          deps.workspace_root,
          deps.public_http_base,
      )
      .await;

      let default_vendor = read_setting(&pool, "default_vendor")
          .await
          .map_err(|e| e.to_string())?
          .unwrap_or_else(|| "local".into());
      let default_vendor = if vendors.contains_key(&default_vendor) {
          default_vendor
      } else {
          eprintln!("warning: default vendor '{default_vendor}' is not loaded; using 'local'");
          "local".into()
      };

      let vendors: SharedVendors = Arc::new(RwLock::new(vendors));
      let store = Arc::new(Self {
          pool: pool.clone(),
          registry: registry.clone(),
          default_vendor: RwLock::new(default_vendor),
          vendors: vendors.clone(),
          vendors_dirty: AtomicBool::new(false),
          info: deps.info,
      });
      Ok(OpenedConfig {
          store,
          registry,
          vendors,
          pool,
      })
  }
  ```
- [ ] `vendors_view` (was lines 139-163) reads the live map instead of the
  frozen snapshot:
  ```rust
  fn vendors_view(&self, default_vendor: &str, rows: &[VendorRow]) -> Vec<VendorView> {
      let live = self.vendors.read().unwrap_or_else(|e| e.into_inner());
      let active = |name: &str| live.contains_key(name);
      let mut out = vec![VendorView {
          name: "local".into(),
          active: active("local"),
          is_default: default_vendor == "local",
          config: None,
      }];
      for r in rows {
          let config = match r.kind.as_str() {
              "velos" => serde_json::from_str::<VelosConfig>(&r.config)
                  .ok()
                  .map(|vc| VendorConfigView::Velos(velos_view(&vc))),
              _ => None,
          };
          out.push(VendorView {
              name: r.name.clone(),
              active: active(&r.name),
              is_default: default_vendor == r.name,
              config,
          });
      }
      out.sort_by(|a, b| a.name.cmp(&b.name));
      out
  }
  ```
  (`VendorView.error` isn't added until task 12's `.fl` change — this task
  only fixes the `active` lookup.)
- [ ] `update()`'s `default_vendor` validation (was lines 279-285) reads the
  live map:
  ```rust
  if let Some(dv) = &update.default_vendor {
      let loaded = self.vendors.read().unwrap_or_else(|e| e.into_inner());
      if !loaded.contains_key(dv) {
          let mut names: Vec<&str> = loaded.keys().map(String::as_str).collect();
          names.sort();
          return Err(format!("vendor '{dv}' is not loaded (available: {})", names.join(", ")));
      }
      drop(loaded);
      sqlx::query(
          "INSERT INTO settings (key, value) VALUES ('default_vendor', ?) \
           ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      )
      .bind(dv)
      .execute(&mut *tx)
      .await
      .map_err(|e| e.to_string())?;
  }
  ```
- [ ] Run `cargo build --workspace 2>&1 | tail -80` — should now be clean
  except `cli/src/serve.rs`, which task 9 verifies. Run `cargo test -p
  server`, confirm the whole `server` crate's tests pass (this also
  exercises `session_actor.rs`/`supervisor.rs`/`http/mod.rs` test helpers
  from task 6).
- [ ] Commit:
  ```
  git add server/src/config/store.rs
  git commit -m "settings: DbConfigStore holds a live SharedVendors map"
  ```

## Task 8 — `session_actor.rs` reads the live map

**Files:** `server/src/sessions/session_actor.rs` lines 147-153 (`fn
vendor`).

- [ ] Write a quick regression test first, added to `session_actor.rs`'s
  `mod tests`: a session whose vendor is removed from the live map after
  creation reports the "unknown runtime vendor" error on its next lookup.
  This exercises the same code path as production reconciliation without
  needing the full `ConfigStore` — directly mutate the shared map:
  ```rust
  #[tokio::test]
  async fn vendor_lookup_reflects_live_removal() {
      let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
      let harness = harness_on(journal, MockVendor::new());
      // Remove "mock" from the live map behind the actor's back.
      harness.actor... // see note below
  }
  ```
  In practice `SessionActor` doesn't expose `deps` to tests directly, and
  `fn vendor` is private — so instead of a new actor-level test, verify this
  at the *type* level: the existing `harness_custom` already builds
  `ServerDeps.vendors` as a `SharedVendors`; grab that same `Arc` before
  constructing the harness, remove the entry, and assert a subsequent
  `UserMessage` command surfaces the "unknown runtime vendor" error via its
  `reply` channel (the existing harness already round-trips `UserMessage`
  errors — reuse that path rather than inventing a new one). Concretely,
  change `harness_custom` to also return the `SharedVendors` handle:
  ```rust
  struct Harness {
      actor: ActorRef<SessionCommand>,
      vendor: Arc<MockVendor>,
      vendors: SharedVendors,
      statuses: tokio::sync::mpsc::UnboundedReceiver<...>,
      id: Uuid,
      _tmp: tempfile::TempDir,
  }
  ```
  and thread `vendors.clone()` through into the returned `Harness`. Then:
  ```rust
  #[tokio::test]
  async fn vendor_removed_from_live_map_fails_the_next_provision() {
      let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
      let h = harness_on(journal, MockVendor::new());
      h.vendors.write().unwrap().remove("mock");
      let (tx, rx) = tokio::sync::oneshot::channel();
      h.actor
          .tell(SessionCommand::UserMessage { text: "hi".into(), reply: tx })
          .await
          .unwrap();
      let err = rx.await.unwrap().unwrap_err();
      assert!(format!("{err:?}").contains("unknown runtime vendor"), "{err:?}");
  }
  ```
  (Exact assertion shape depends on `UserMessageError`'s `Debug`/`Display` —
  inspect it while writing this step; if it doesn't stringify the inner
  `vendor()` error, assert on whatever variant carries it instead — the goal
  is just "an unknown-vendor lookup surfaces as a session-level error, not a
  panic".)
- [ ] Run the new test, confirm it fails to compile (`fn vendor` still does
  a bare `HashMap::get`, so removing from `SharedVendors` — once
  `harness_custom` is updated to build one — won't be visible; more
  precisely, this step's *purpose* is exercised once the implementation
  below lands, so treat "does `harness_custom` compile with the new
  `Harness.vendors` field" as the immediate failing/passing signal).
- [ ] Implement: `fn vendor` (was lines 147-153) reads through the lock:
  ```rust
  fn vendor(&self) -> Result<Arc<dyn RuntimeVendor>, String> {
      let vendors = self
          .deps
          .vendors
          .read()
          .map_err(|_| "vendor registry lock poisoned".to_string())?;
      vendors
          .get(&self.spec.vendor)
          .cloned()
          .ok_or_else(|| format!("unknown runtime vendor '{}'", self.spec.vendor))
  }
  ```
  (Matches the existing `provider_registry` read pattern a few lines away in
  `ensure_agent`, which uses `.map_err(|_| "provider registry lock
  poisoned".to_string())?` rather than `.unwrap()` — `unwrap_used`/
  `expect_used` are workspace-`deny`d in non-test code, so this is required,
  not stylistic.)
- [ ] Run `cargo test -p server sessions::session_actor`, confirm green.
- [ ] Commit:
  ```
  git add server/src/sessions/session_actor.rs
  git commit -m "sessions: SessionActor::vendor reads the live SharedVendors map"
  ```

## Task 9 — verify `cli/src/serve.rs`

**Files:** `server/src/cli/serve.rs` (actually `cli/src/serve.rs`) lines
136-138.

- [ ] No source change expected — `ServerDeps { vendors: opened.vendors, ...
  }` (line 138) already moves an `OpenedConfig.vendors: SharedVendors` into
  a `ServerDeps.vendors: SharedVendors`, same type both sides.
- [ ] Run `cargo build --workspace`, confirm the whole workspace (including
  `cli`) now compiles clean.
- [ ] Run `cargo test --workspace`, confirm green.
- [ ] Nothing to commit for this task (verification only) — if `cargo build`
  surprisingly does require an edit here, make the minimal fix and commit
  `cli: verify ServerDeps.vendors under SharedVendors`.

## Task 10 — `VelosVendor` gains `reconfigure()`

**Files:** `server/src/vendor/velos.rs` lines 1-34 (imports), 309-360
(`VelosVendor` struct + `bind`), 362-455 (`provision`/trait impls),
502-832 (test mod: `FakeVelosApi` at 564-590, `bind_vendor` at 694-698, add a
new test).

**Interfaces produced:**
```rust
#[derive(Clone)]
pub struct VelosMutableSettings {
    pub api: Arc<dyn ContainerApi>,
    pub image: String,
    pub runtime_bin: String,
    pub workspace_root: String,
    pub cpu: u32,
    pub memory_bytes: u64,
    pub connect_timeout: Duration,
    pub public_http_base: Option<String>,
}

impl VelosVendor {
    pub fn settings(&self) -> VelosMutableSettings;
    pub fn reconfigure(&self, settings: VelosMutableSettings);
}
```
(`VelosVendorSettings`, the *bind-time* struct passed to `VelosVendor::bind`,
is unchanged — `VelosMutableSettings` is new and separate.)

- [ ] Add `RwLock` to the `std::sync` import (line ~32):
  ```rust
  use std::sync::{Arc, RwLock};
  ```
- [ ] Add `images: Mutex<Vec<String>>` to `FakeVelosApi` (was lines 564-570)
  so tests can assert what image a reconfigured vendor's next `provision()`
  used:
  ```rust
  struct FakeVelosApi {
      creates: Mutex<Vec<String>>,
      deletes: Mutex<Vec<String>>,
      incarnations: Mutex<Vec<String>>,
      images: Mutex<Vec<String>>,
      tasks: Mutex<HashMap<String, JoinHandle<()>>>,
  }

  impl FakeVelosApi {
      fn new() -> Arc<Self> {
          Arc::new(Self {
              creates: Mutex::new(Vec::new()),
              deletes: Mutex::new(Vec::new()),
              incarnations: Mutex::new(Vec::new()),
              images: Mutex::new(Vec::new()),
              tasks: Mutex::new(HashMap::new()),
          })
      }
      fn creates(&self) -> Vec<String> { self.creates.lock().unwrap().clone() }
      fn deletes(&self) -> Vec<String> { self.deletes.lock().unwrap().clone() }
      fn incarnations(&self) -> Vec<String> { self.incarnations.lock().unwrap().clone() }
      fn images(&self) -> Vec<String> { self.images.lock().unwrap().clone() }
  }
  ```
  and in `create_container` (was ~line 633-647), push `spec.image.clone()`:
  ```rust
  async fn create_container(&self, name: &str, spec: &ContainerLaunchSpec) -> Result<(), VelosError> {
      self.creates.lock().unwrap().push(name.to_string());
      self.images.lock().unwrap().push(spec.image.clone());
      // ... unchanged dial-back spawn ...
  }
  ```
- [ ] Write the failing test (append to the test mod):
  ```rust
  #[tokio::test]
  async fn reconfigure_swaps_settings_without_rebinding_listener() {
      let api = FakeVelosApi::new();
      let vendor = bind_vendor(api.clone()).await;
      let bound_before = vendor.endpoint_ws.clone();

      vendor.create("rt-1", &test_spec()).await.expect("create");
      assert_eq!(api.images(), vec!["test/image".to_string()]);

      let mut new_settings = vendor.settings();
      new_settings.image = "test/image-v2".into();
      vendor.reconfigure(new_settings);

      vendor.create("rt-2", &test_spec()).await.expect("create after reconfigure");
      assert_eq!(
          api.images(),
          vec!["test/image".to_string(), "test/image-v2".to_string()]
      );
      assert_eq!(
          vendor.endpoint_ws, bound_before,
          "reconfigure must not rebind the listener"
      );
  }
  ```
- [ ] Run `cargo test -p server vendor::velos`, confirm it fails to compile
  (`VelosVendor::settings`/`reconfigure` don't exist yet; `FakeVelosApi`
  changes should already compile on their own — verify that sub-step first
  if useful).
- [ ] Implement. `VelosMutableSettings` (new, place above `VelosVendor`):
  ```rust
  /// Settings that can change under a vendor's feet without rebinding its
  /// listener — everything `provision()` reads except the immutable
  /// listener/registry/endpoint bound once in `bind()`.
  #[derive(Clone)]
  pub struct VelosMutableSettings {
      pub api: Arc<dyn ContainerApi>,
      pub image: String,
      pub runtime_bin: String,
      pub workspace_root: String,
      pub cpu: u32,
      pub memory_bytes: u64,
      pub connect_timeout: Duration,
      /// `http://<advertise_host>:<http_port>` when configured — recomputed by
      /// the caller on every reconfigure since `advertise_host` itself is
      /// listener-affecting (frozen) but `http_port` is not.
      pub public_http_base: Option<String>,
  }
  ```
- [ ] `VelosVendor` struct (was lines 309-324) collapses the now-duplicated
  plain fields into the lock:
  ```rust
  pub struct VelosVendor {
      connected: Arc<ConnectedRuntimeRegistry>,
      /// `ws://<advertise_host>:<bound_port>` — where scheduled runtimes dial
      /// back. Immutable: baked in at `bind()` time from `advertise_host` +
      /// the actually-bound port, and never revisited by `reconfigure()`.
      endpoint_ws: String,
      settings: RwLock<VelosMutableSettings>,
      _serve_guard: DropGuard,
  }
  ```
- [ ] `bind()` (was lines 329-360) builds the initial `VelosMutableSettings`:
  ```rust
  pub async fn bind(
      api: Arc<dyn ContainerApi>,
      settings: VelosVendorSettings,
  ) -> Result<Self, VendorError> {
      let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Tcp(settings.listen))
          .await
          .map_err(|e| VendorError::Provision(format!("velos vendor listener: {e}")))?;
      let port = listener
          .tcp_addr()
          .ok_or_else(|| VendorError::Provision("velos vendor requires a TCP listener".into()))?
          .port();
      let endpoint_ws = format!("ws://{}:{port}", settings.advertise_host);
      let public_http_base = settings
          .http_port
          .map(|p| format!("http://{}:{p}", settings.advertise_host));
      let connected = Arc::new(ConnectedRuntimeRegistry::new());
      let cancel = CancellationToken::new();
      serve_runtime_connections(listener, connected.clone(), cancel.clone());
      Ok(Self {
          connected,
          endpoint_ws,
          settings: RwLock::new(VelosMutableSettings {
              api,
              image: settings.image,
              runtime_bin: settings.runtime_bin,
              workspace_root: settings.workspace_root,
              cpu: settings.cpu,
              memory_bytes: settings.memory_bytes,
              connect_timeout: settings.connect_timeout,
              public_http_base,
          }),
          _serve_guard: cancel.drop_guard(),
      })
  }

  /// Current mutable settings (a cheap clone under a read lock) — for
  /// inspection (tests, a future debug endpoint) and as the base a caller
  /// mutates before calling `reconfigure`.
  pub fn settings(&self) -> VelosMutableSettings {
      self.settings.read().unwrap_or_else(|e| e.into_inner()).clone()
  }

  /// Swap in new mutable settings (e.g. after a live config edit). Never
  /// touches the listener — only the next `provision()` call sees the new
  /// values.
  pub fn reconfigure(&self, settings: VelosMutableSettings) {
      *self.settings.write().unwrap_or_else(|e| e.into_inner()) = settings;
  }
  ```
- [ ] `provision()` (was lines 362-413) reads the lock once at the top:
  ```rust
  async fn provision(
      &self,
      runtime_id: &str,
      spec: &RuntimeSpec,
      attach: bool,
  ) -> Result<VendorRuntime, VendorError> {
      let wrap = |e: String| {
          if attach { VendorError::Attach(e) } else { VendorError::Provision(e) }
      };
      let container = container_name(runtime_id);
      let incarnation = format!("{runtime_id}-{}", uuid::Uuid::new_v4().simple());
      let (provider, workspace_root) = {
          let settings = self.settings.read().unwrap_or_else(|e| e.into_inner());
          let provider = Arc::new(VelosRuntimeProvider {
              api: settings.api.clone(),
              connected: self.connected.clone(),
              container_name: container,
              endpoint_ws: self.endpoint_ws.clone(),
              image: settings.image.clone(),
              runtime_bin: settings.runtime_bin.clone(),
              cpu: settings.cpu,
              memory_bytes: settings.memory_bytes,
              connect_timeout: settings.connect_timeout,
          });
          (provider, settings.workspace_root.clone())
      };
      let client = ExecutorClient::new(InMemExecutorTransport::new(provider, self.connected.clone()));
      let config = runtime_config_from(spec, &workspace_root).map_err(wrap)?;
      let result = if attach {
          client.attach_runtime(&incarnation, config).await
      } else {
          client.create_runtime(&incarnation, config).await
      };
      result.map_err(|e| wrap(e.to_string()))?;
      let transport = client
          .runtime_transport(&incarnation)
          .await
          .map_err(|e| wrap(e.to_string()))?;
      Ok(VendorRuntime {
          runtime_client: RuntimeClient::from_arc(transport),
          handle: Arc::new(VelosHandle { client, runtime_id: incarnation }),
      })
  }
  ```
  (the read guard is scoped to the block so it's dropped before any `.await`
  below, matching the documented pattern for `SharedProviderRegistry`).
- [ ] `RuntimeVendor::artifact_base_url` (was line 422-424):
  ```rust
  fn artifact_base_url(&self) -> Option<String> {
      self.settings.read().unwrap_or_else(|e| e.into_inner()).public_http_base.clone()
  }
  ```
- [ ] `RuntimeVendor::delete` (was lines 448-454):
  ```rust
  async fn delete(&self, runtime_id: &str) {
      let api = self.settings.read().unwrap_or_else(|e| e.into_inner()).api.clone();
      let _ = api.delete_container(&container_name(runtime_id)).await;
  }
  ```
- [ ] `server/src/vendor/mod.rs`: export `VelosMutableSettings` alongside the
  existing exports (line 18):
  ```rust
  pub use velos::{VelosMutableSettings, VelosVendor, VelosVendorSettings};
  ```
- [ ] Run `cargo test -p server vendor::velos`, confirm the new test passes
  and all pre-existing velos tests (reverse-dial round trip, host-dir
  rejection, attach/reclaim, distinct incarnations, dead-container fast
  fail) still pass unchanged.
- [ ] Run `cargo clippy -p server --all-targets --all-features -- -D
  warnings`, fix anything (in particular: no `.unwrap()`/`.expect()` in the
  non-test code added above).
- [ ] Commit:
  ```
  git add server/src/vendor/velos.rs server/src/vendor/mod.rs
  git commit -m "vendor/velos: RwLock<VelosMutableSettings> + reconfigure()"
  ```

## Task 11 — factor per-row vendor building out of `build_vendors`

**Files:** `server/src/config/store.rs` lines 463-526 (`build_vendors`,
`build_velos_vendor`), 61-71/106-119 (`DbConfigStore` struct + `open`, add
`velos_instances`).

**Interfaces produced:**
```rust
enum BuiltVendor { Velos(Arc<VelosVendor>) }
impl BuiltVendor { fn as_dyn(&self) -> Arc<dyn RuntimeVendor>; }
async fn build_one_vendor(row: &VendorRow) -> Result<BuiltVendor, String>;
async fn build_vendors(
    rows: &[VendorRow], runtime_bin: PathBuf, workspace_root: PathBuf, public_http_base: Option<String>,
) -> (HashMap<String, Arc<dyn RuntimeVendor>>, HashMap<String, Arc<VelosVendor>>);
```
(`build_vendors`'s return type changes from a single `HashMap` to a tuple —
task 7's `open()` call site is updated here to match, since task 7 didn't
yet need the second map.)

- [ ] Write a failing test first: `build_one_vendor` on an unknown kind
  returns a descriptive error, and on a `"velos"` row with a deliberately
  unbindable `listen` (a port already held by another listener) returns
  `Err` containing the underlying bind failure, not a panic:
  ```rust
  #[tokio::test]
  async fn build_one_vendor_reports_unknown_kind() {
      let row = VendorRow { name: "x".into(), kind: "bogus".into(), config: "{}".into() };
      let err = build_one_vendor(&row).await.unwrap_err();
      assert!(err.contains("bogus"), "{err}");
  }

  #[tokio::test]
  async fn build_one_vendor_velos_returns_arc_dyn_runtime_vendor() {
      let row = VendorRow {
          name: "cluster-a".into(),
          kind: "velos".into(),
          config: serde_json::json!({
              "server_url": "http://velos:8080",
              "image": "img",
              "advertise_host": "10.0.0.5",
              "listen": "127.0.0.1:0",
          })
          .to_string(),
      };
      let built = build_one_vendor(&row).await.expect("velos row builds");
      assert_eq!(built.as_dyn().name(), "velos");
  }
  ```
- [ ] Run `cargo test -p server config::store`, confirm compile failure
  (`build_one_vendor`/`BuiltVendor` don't exist yet).
- [ ] Implement `BuiltVendor` + `build_one_vendor`, replacing the inline
  match inside the old `build_vendors` loop (was lines 480-499):
  ```rust
  /// A freshly built vendor, tagged so the caller can register it under both
  /// the generic `vendors` map and (for kinds that support live reconfigure)
  /// a concrete-typed side table — without ever downcasting a
  /// `dyn RuntimeVendor`.
  enum BuiltVendor {
      Velos(Arc<VelosVendor>),
  }

  impl BuiltVendor {
      fn as_dyn(&self) -> Arc<dyn RuntimeVendor> {
          match self {
              BuiltVendor::Velos(v) => v.clone(),
          }
      }
  }

  /// Build one row's vendor instance, kind-dispatched. Used both at boot
  /// (`build_vendors`'s loop) and per-row during a live config update.
  async fn build_one_vendor(row: &VendorRow) -> Result<BuiltVendor, String> {
      match row.kind.as_str() {
          "velos" => {
              let vc = serde_json::from_str::<VelosConfig>(&row.config)
                  .map_err(|e| format!("invalid config: {e}"))?;
              let vendor = build_velos_vendor(&vc).await?;
              Ok(BuiltVendor::Velos(Arc::new(vendor)))
          }
          other => Err(format!("unknown kind '{other}'")),
      }
  }
  ```
- [ ] Rewrite `build_vendors` as a thin loop over `build_one_vendor`, now
  also collecting the concrete `velos_instances` side table:
  ```rust
  /// Build the vendor set: `local` always, plus one per configured row. A
  /// vendor that fails to build is logged and left out (reported inactive),
  /// never fatal — matches `reconcile_vendors`'s per-update behavior below.
  async fn build_vendors(
      rows: &[VendorRow],
      runtime_bin: PathBuf,
      workspace_root: PathBuf,
      public_http_base: Option<String>,
  ) -> (
      HashMap<String, Arc<dyn RuntimeVendor>>,
      HashMap<String, Arc<VelosVendor>>,
  ) {
      let mut vendors: HashMap<String, Arc<dyn RuntimeVendor>> = HashMap::new();
      let mut velos_instances: HashMap<String, Arc<VelosVendor>> = HashMap::new();
      vendors.insert(
          "local".into(),
          Arc::new(LocalProcessVendor::new(runtime_bin, workspace_root, public_http_base)),
      );
      for r in rows {
          match build_one_vendor(r).await {
              Ok(built) => {
                  println!("vendor '{}' ({}) enabled", r.name, r.kind);
                  if let BuiltVendor::Velos(v) = &built {
                      velos_instances.insert(r.name.clone(), v.clone());
                  }
                  vendors.insert(r.name.clone(), built.as_dyn());
              }
              Err(e) => eprintln!("warning: vendor '{}' failed to start ({e})", r.name),
          }
      }
      (vendors, velos_instances)
  }
  ```
- [ ] `build_velos_vendor` (was lines 504-526) is unchanged in body (still
  takes `vc: &VelosConfig`, returns `Result<VelosVendor, String>`) — no edit
  needed here beyond what task 2 already did.
- [ ] Add a `velos_mutable_settings` helper (used later by task 12's
  reconfigure path, added now since it lives right beside
  `build_velos_vendor`):
  ```rust
  /// Build a fresh `VelosMutableSettings` from a row's config — used by
  /// `reconcile_vendors` to `reconfigure()` an already-bound vendor whose
  /// listener-affecting fields didn't change.
  fn velos_mutable_settings(vc: &VelosConfig) -> Result<VelosMutableSettings, String> {
      let token = resolve_velos_token(vc)?;
      let client =
          VelosClient::new(&vc.server_url, token).map_err(|e| format!("velos client: {e}"))?;
      Ok(VelosMutableSettings {
          api: Arc::new(client),
          image: vc.image.clone(),
          runtime_bin: vc.runtime_bin.clone(),
          workspace_root: vc.workspace_root.clone(),
          cpu: vc.cpu,
          memory_bytes: vc.memory_mib.saturating_mul(1024 * 1024),
          connect_timeout: Duration::from_secs(vc.connect_timeout_secs),
          public_http_base: vc.http_port.map(|p| format!("http://{}:{p}", vc.advertise_host)),
      })
  }
  ```
- [ ] Import `VelosMutableSettings` in `store.rs`'s `use crate::vendor::{...}`
  line (was line 13):
  ```rust
  use crate::vendor::{
      LocalProcessVendor, RuntimeVendor, VelosMutableSettings, VelosVendor, VelosVendorSettings,
  };
  ```
- [ ] Update `DbConfigStore` struct to add `velos_instances`:
  ```rust
  pub struct DbConfigStore {
      pool: SqlitePool,
      registry: SharedProviderRegistry,
      default_vendor: RwLock<String>,
      vendors: SharedVendors,
      /// Concrete handles for vendor kinds that support live reconfigure
      /// (currently only `velos`), keyed by name — lets `update()` call
      /// `.reconfigure()` on the right concrete type without downcasting the
      /// generic `vendors` map.
      velos_instances: RwLock<HashMap<String, Arc<VelosVendor>>>,
      vendors_dirty: AtomicBool,
      info: ServerInfo,
  }
  ```
- [ ] Update `open()`'s call site and `Self { ... }` construction:
  ```rust
  let vendor_rows = read_vendors(&pool).await.map_err(|e| e.to_string())?;
  let (vendors, velos_instances) = build_vendors(
      &vendor_rows,
      deps.runtime_bin,
      deps.workspace_root,
      deps.public_http_base,
  )
  .await;

  let default_vendor = read_setting(&pool, "default_vendor")
      .await
      .map_err(|e| e.to_string())?
      .unwrap_or_else(|| "local".into());
  let default_vendor = if vendors.contains_key(&default_vendor) {
      default_vendor
  } else {
      eprintln!("warning: default vendor '{default_vendor}' is not loaded; using 'local'");
      "local".into()
  };

  let vendors: SharedVendors = Arc::new(RwLock::new(vendors));
  let store = Arc::new(Self {
      pool: pool.clone(),
      registry: registry.clone(),
      default_vendor: RwLock::new(default_vendor),
      vendors: vendors.clone(),
      velos_instances: RwLock::new(velos_instances),
      vendors_dirty: AtomicBool::new(false),
      info: deps.info,
  });
  Ok(OpenedConfig { store, registry, vendors, pool })
  ```
- [ ] Run `cargo test -p server config::store`, confirm the two new tests
  and everything else still pass.
- [ ] Run `cargo clippy -p server --all-targets --all-features -- -D
  warnings`, fix anything.
- [ ] Commit:
  ```
  git add server/src/config/store.rs
  git commit -m "settings: factor build_one_vendor out of build_vendors"
  ```

## Task 12 — `VendorView.error` + live reconciliation in `update()`

**Files:** `models/fluorite/settings.fl` lines 33-42 (`VendorView`), 88-90
(`SettingsView.restart_required` doc); `server/src/config/store.rs` lines
139-163 (`vendors_view`, revisit for `error`), 172-316 (`update()`), struct
+ `open()` (add `vendor_errors`, rename `vendors_dirty` →
`restart_required`), test mod (replace the old
`velos_vendor_persists_redacted_and_flags_restart` test, add 5 new tests).

**Interfaces produced:**
```rust
// settings.fl
struct VendorView {
    name: String,
    active: bool,
    is_default: bool,
    config: Option<VendorConfigView>,
    error: Option<String>,
}
```
```rust
impl DbConfigStore {
    async fn reconcile_vendors(&self, before: &[VendorRow], after: &[VendorRow]);
    async fn apply_active_vendor_edit(&self, row: &VendorRow, prior: &VendorRow);
    async fn activate_vendor(&self, row: &VendorRow);
}
```

- [ ] Edit `models/fluorite/settings.fl`'s `VendorView` (was lines 33-42):
  ```
  /// A runtime vendor sessions can target. `local` is built-in and carries no
  /// config; every other vendor carries a kind-tagged config block.
  struct VendorView {
      name: String,
      /// Loaded and usable in the running server right now.
      active: bool,
      /// Whether new sessions default to this vendor.
      is_default: bool,
      /// Kind-specific config, redacted. Absent for the built-in `local` vendor.
      config: Option<VendorConfigView>,
      /// The last build/reconfigure failure for this vendor, if any. `None`
      /// when active or never attempted.
      error: Option<String>,
  }
  ```
  and reword `SettingsView.restart_required`'s doc (was lines 88-90):
  ```
      /// True only when an already-active vendor's listener-affecting fields
      /// (`listen`/`advertise_host`/`server_url`) changed and are pending a
      /// restart. Every other provider/model/vendor edit applies live.
      restart_required: bool,
  ```
- [ ] Run `cargo build -p server 2>&1 | tail -40` — expect a compile error:
  `vendors_view` constructs `VendorView { ... }` without the new required
  `error` field. This is the failing step.
- [ ] Rename `vendors_dirty` → `restart_required` throughout `store.rs` (the
  struct field, its `open()` initializer, and `build_view`'s read) for
  clarity now that the field's meaning narrows:
  ```rust
  restart_required: AtomicBool,
  ```
  ```rust
  restart_required: AtomicBool::new(false),
  ```
  ```rust
  restart_required: self.restart_required.load(Ordering::Relaxed),
  ```
- [ ] Add `vendor_errors: RwLock<HashMap<String, String>>` to the struct and
  `open()`'s constructor:
  ```rust
  /// Last build/reconfigure failure per vendor name, surfaced on
  /// `VendorView.error`. Cleared when that vendor next builds or
  /// reconfigures successfully.
  vendor_errors: RwLock<HashMap<String, String>>,
  ```
  ```rust
  vendor_errors: RwLock::new(HashMap::new()),
  ```
- [ ] `vendors_view` (was lines 139-163) reads `vendor_errors` too:
  ```rust
  fn vendors_view(&self, default_vendor: &str, rows: &[VendorRow]) -> Vec<VendorView> {
      let live = self.vendors.read().unwrap_or_else(|e| e.into_inner());
      let errors = self.vendor_errors.read().unwrap_or_else(|e| e.into_inner());
      let active = |name: &str| live.contains_key(name);
      let mut out = vec![VendorView {
          name: "local".into(),
          active: active("local"),
          is_default: default_vendor == "local",
          config: None,
          error: None,
      }];
      for r in rows {
          let config = match r.kind.as_str() {
              "velos" => serde_json::from_str::<VelosConfig>(&r.config)
                  .ok()
                  .map(|vc| VendorConfigView::Velos(velos_view(&vc))),
              _ => None,
          };
          out.push(VendorView {
              name: r.name.clone(),
              active: active(&r.name),
              is_default: default_vendor == r.name,
              config,
              error: errors.get(&r.name).cloned(),
          });
      }
      out.sort_by(|a, b| a.name.cmp(&b.name));
      out
  }
  ```
- [ ] Run `cargo build -p server`, confirm clean now. Run `cargo test -p
  server config::store` — expect the *existing*
  `velos_vendor_persists_redacted_and_flags_restart` test to still pass
  unchanged at this point (reconciliation isn't wired into `update()` yet,
  so behavior is unchanged: brand-new vendor stays inactive until restart).
  This confirms the `.fl`/struct plumbing alone is safe before behavior
  changes.
- [ ] Commit this plumbing-only step:
  ```
  git add models/fluorite/settings.fl server/src/config/store.rs
  git commit -m "settings: add VendorView.error, narrow restart_required"
  ```
- [ ] Now write the 5 failing behavior tests (replacing the old
  `velos_vendor_persists_redacted_and_flags_restart`, which asserted exactly
  the old "always needs a restart" behavior this task replaces):
  ```rust
  fn velos_input(image: &str, listen: &str, http_port: Option<u32>, token: Option<&str>) -> VendorInput {
      VendorInput {
          name: "cluster-a".into(),
          config: VendorConfigInput::Velos(VelosInput {
              server_url: "http://velos:8080".into(),
              image: image.into(),
              advertise_host: "10.0.0.5".into(),
              token: token.map(str::to_string),
              runtime_bin: None,
              workspace_root: None,
              listen: Some(listen.into()),
              cpu: None,
              memory_mib: None,
              connect_timeout_secs: None,
              http_port,
          }),
      }
  }

  #[tokio::test]
  async fn new_vendor_activates_live_without_restart() {
      let dir = tempfile::tempdir().unwrap();
      let o = open(dir.path()).await;
      let view = o
          .store
          .update(SettingsUpdate {
              providers: None,
              models: None,
              vendors: Some(vec![velos_input("img", "127.0.0.1:0", None, Some("secret"))]),
              default_vendor: None,
          })
          .await
          .expect("velos update ok");
      assert!(!view.restart_required);
      let v = view.vendors.iter().find(|v| v.name == "cluster-a").expect("present");
      assert!(v.active, "a valid new vendor activates immediately");
      assert!(v.error.is_none());
      assert!(o.vendors.read().unwrap().contains_key("cluster-a"));
  }

  #[tokio::test]
  async fn not_yet_active_vendor_build_failure_reports_error_then_recovers() {
      let dir = tempfile::tempdir().unwrap();
      let o = open(dir.path()).await;

      // Occupy a port so the vendor's listener bind fails deterministically
      // (a real "bad token" only surfaces on the vendor's first actual velos
      // API call, not at build time — an unbindable listen is the reliable,
      // portable way to force a build-time failure here).
      let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
      let busy = blocker.local_addr().unwrap().to_string();

      let view = o
          .store
          .update(SettingsUpdate {
              providers: None,
              models: None,
              vendors: Some(vec![velos_input("img", &busy, None, Some("secret"))]),
              default_vendor: None,
          })
          .await
          .expect("persists even though the vendor fails to build");
      let v = view.vendors.iter().find(|v| v.name == "cluster-a").expect("present");
      assert!(!v.active);
      assert!(v.error.is_some());
      assert!(!view.restart_required);

      drop(blocker);
      let view2 = o
          .store
          .update(SettingsUpdate {
              providers: None,
              models: None,
              vendors: Some(vec![velos_input("img", "127.0.0.1:0", None, None)]),
              default_vendor: None,
          })
          .await
          .expect("second update ok");
      let v2 = view2.vendors.iter().find(|v| v.name == "cluster-a").expect("present");
      assert!(v2.active);
      assert!(v2.error.is_none());
  }

  #[tokio::test]
  async fn active_vendor_non_listener_edit_applies_live() {
      let dir = tempfile::tempdir().unwrap();
      let o = open(dir.path()).await;
      o.store
          .update(SettingsUpdate {
              providers: None,
              models: None,
              vendors: Some(vec![velos_input("img-v1", "127.0.0.1:0", None, Some("secret"))]),
              default_vendor: None,
          })
          .await
          .unwrap();
      assert!(o.vendors.read().unwrap().contains_key("cluster-a"));

      let view = o
          .store
          .update(SettingsUpdate {
              providers: None,
              models: None,
              vendors: Some(vec![velos_input("img-v2", "127.0.0.1:0", Some(9000), None)]),
              default_vendor: None,
          })
          .await
          .unwrap();

      assert!(!view.restart_required);
      let handle = o
          .store
          .velos_instances
          .read()
          .unwrap()
          .get("cluster-a")
          .cloned()
          .expect("still the live instance");
      let settings = handle.settings();
      assert_eq!(settings.image, "img-v2");
      assert_eq!(settings.public_http_base.as_deref(), Some("http://10.0.0.5:9000"));
  }

  #[tokio::test]
  async fn active_vendor_listener_edit_requires_restart_and_keeps_old_instance() {
      let dir = tempfile::tempdir().unwrap();
      let o = open(dir.path()).await;
      o.store
          .update(SettingsUpdate {
              providers: None,
              models: None,
              vendors: Some(vec![velos_input("img", "127.0.0.1:0", None, Some("secret"))]),
              default_vendor: None,
          })
          .await
          .unwrap();
      let before = o.store.velos_instances.read().unwrap().get("cluster-a").cloned().unwrap();

      let view = o
          .store
          .update(SettingsUpdate {
              providers: None,
              models: None,
              vendors: Some(vec![velos_input("img", "127.0.0.1:4551", None, None)]),
              default_vendor: None,
          })
          .await
          .unwrap();

      assert!(view.restart_required);
      let v = view.vendors.iter().find(|v| v.name == "cluster-a").unwrap();
      assert!(v.active, "the old instance keeps serving");
      assert!(v.error.as_deref().unwrap_or_default().contains("restart"));
      let after = o.store.velos_instances.read().unwrap().get("cluster-a").cloned().unwrap();
      assert!(Arc::ptr_eq(&before, &after), "no rebuild happened");
  }

  #[tokio::test]
  async fn removed_vendor_row_drops_from_live_map() {
      let dir = tempfile::tempdir().unwrap();
      let o = open(dir.path()).await;
      o.store
          .update(SettingsUpdate {
              providers: None,
              models: None,
              vendors: Some(vec![velos_input("img", "127.0.0.1:0", None, Some("secret"))]),
              default_vendor: None,
          })
          .await
          .unwrap();
      assert!(o.vendors.read().unwrap().contains_key("cluster-a"));

      let view = o
          .store
          .update(SettingsUpdate {
              providers: None,
              models: None,
              vendors: Some(vec![]),
              default_vendor: None,
          })
          .await
          .unwrap();

      assert!(!o.vendors.read().unwrap().contains_key("cluster-a"));
      assert!(view.vendors.iter().all(|v| v.name != "cluster-a"));
  }
  ```
  Also delete the old `velos_vendor_persists_redacted_and_flags_restart`
  test (its assertions — `!v.active`, `view.restart_required` — describe
  exactly the behavior this task replaces).
- [ ] Run `cargo test -p server config::store`, confirm the new tests fail
  to compile (`reconcile_vendors` doesn't exist; `update()` never calls it,
  so behaviorally several would also just fail their assertions once
  compiling — expect compile failure first).
- [ ] Implement. First, hoist the pre-update vendor rows out of the `if let
  Some(vendors) = &update.vendors` block in `update()` (was lines 246-277)
  so they're available after commit:
  ```rust
  let mut vendor_rows_before: Option<Vec<VendorRow>> = None;
  if let Some(vendors) = &update.vendors {
      let existing = read_vendors(&mut *tx).await.map_err(|e| e.to_string())?;
      let keep: HashMap<&str, &str> = existing
          .iter()
          .map(|r| (r.name.as_str(), r.config.as_str()))
          .collect();
      let mut seen = HashSet::new();
      sqlx::query("DELETE FROM vendors")
          .execute(&mut *tx)
          .await
          .map_err(|e| e.to_string())?;
      for v in vendors {
          let name = v.name.trim();
          if name.is_empty() {
              return Err("vendor name cannot be empty".into());
          }
          if name == "local" {
              return Err("'local' is reserved and cannot be a configured vendor".into());
          }
          if !seen.insert(name.to_string()) {
              return Err(format!("duplicate vendor '{name}'"));
          }
          let (kind, config) = build_vendor_config(name, &v.config, keep.get(name).copied())?;
          sqlx::query("INSERT INTO vendors (name, kind, config) VALUES (?, ?, ?)")
              .bind(name)
              .bind(kind)
              .bind(config)
              .execute(&mut *tx)
              .await
              .map_err(|e| e.to_string())?;
      }
      vendor_rows_before = Some(existing);
  }
  ```
  (identical logic to today, just captures `existing` into
  `vendor_rows_before` at the end instead of letting it drop).
- [ ] After `tx.commit()` and the existing registry/default-vendor swap
  (was lines 302-313), add the reconciliation call and drop the old
  `vendors_dirty` set-on-any-edit line (that blanket behavior is exactly
  what this task replaces):
  ```rust
  tx.commit().await.map_err(|e| e.to_string())?;

  *self.registry.write().unwrap_or_else(|e| e.into_inner()) = new_registry;
  if let Some(dv) = &update.default_vendor {
      *self
          .default_vendor
          .write()
          .unwrap_or_else(|e| e.into_inner()) = dv.clone();
  }
  if let Some(before) = vendor_rows_before {
      let after = read_vendors(&self.pool).await.map_err(|e| e.to_string())?;
      self.reconcile_vendors(&before, &after).await;
  }

  self.build_view().await
  ```
- [ ] Add the three reconciliation methods to `impl DbConfigStore` (the
  inherent block, alongside `build_view`/`vendors_view`):
  ```rust
  /// After vendor rows are persisted, bring the live vendor map in line with
  /// the new DB state: build newly-added or previously-inactive rows,
  /// live-reconfigure an active `velos` vendor whose listener-affecting
  /// fields are unchanged, leave an active vendor's old instance running
  /// (flagged) when those fields did change, and drop rows that were
  /// removed. Never fails the caller — outcomes land in `vendor_errors` /
  /// `restart_required` for the view to report.
  async fn reconcile_vendors(&self, before: &[VendorRow], after: &[VendorRow]) {
      let before_by_name: HashMap<&str, &VendorRow> =
          before.iter().map(|r| (r.name.as_str(), r)).collect();
      let after_names: HashSet<&str> = after.iter().map(|r| r.name.as_str()).collect();

      for name in before_by_name.keys().filter(|n| !after_names.contains(*n)) {
          self.vendors.write().unwrap_or_else(|e| e.into_inner()).remove(*name);
          self.velos_instances.write().unwrap_or_else(|e| e.into_inner()).remove(*name);
          self.vendor_errors.write().unwrap_or_else(|e| e.into_inner()).remove(*name);
      }

      for row in after {
          let was_active = self
              .vendors
              .read()
              .unwrap_or_else(|e| e.into_inner())
              .contains_key(&row.name);
          if was_active
              && let Some(prior) = before_by_name.get(row.name.as_str()).copied()
              && prior.kind == row.kind
          {
              self.apply_active_vendor_edit(row, prior).await;
          } else {
              self.activate_vendor(row).await;
          }
      }
  }

  /// A previously-active vendor of the same kind was edited: reconfigure it
  /// in place if the listener-affecting fields (`listen`/`advertise_host`/
  /// `server_url`) didn't change, else leave the running instance untouched
  /// and flag that vendor as needing a restart.
  async fn apply_active_vendor_edit(&self, row: &VendorRow, prior: &VendorRow) {
      if row.kind != "velos" {
          return;
      }
      let parsed = (
          serde_json::from_str::<VelosConfig>(&prior.config),
          serde_json::from_str::<VelosConfig>(&row.config),
      );
      let (Ok(old_vc), Ok(new_vc)) = parsed else {
          self.vendor_errors
              .write()
              .unwrap_or_else(|e| e.into_inner())
              .insert(row.name.clone(), "stored config no longer parses".to_string());
          return;
      };
      let listener_unchanged = old_vc.listen == new_vc.listen
          && old_vc.advertise_host == new_vc.advertise_host
          && old_vc.server_url == new_vc.server_url;
      if listener_unchanged {
          match velos_mutable_settings(&new_vc) {
              Ok(settings) => {
                  let handle = self
                      .velos_instances
                      .read()
                      .unwrap_or_else(|e| e.into_inner())
                      .get(&row.name)
                      .cloned();
                  if let Some(handle) = handle {
                      handle.reconfigure(settings);
                      self.vendor_errors
                          .write()
                          .unwrap_or_else(|e| e.into_inner())
                          .remove(&row.name);
                      return;
                  }
              }
              Err(e) => {
                  self.vendor_errors
                      .write()
                      .unwrap_or_else(|e| e.into_inner())
                      .insert(row.name.clone(), e);
                  return;
              }
          }
      }
      self.restart_required.store(true, Ordering::Relaxed);
      self.vendor_errors.write().unwrap_or_else(|e| e.into_inner()).insert(
          row.name.clone(),
          "listen/advertise_host/server_url changed — restart the server to apply".to_string(),
      );
  }

  /// Bring a row online: a brand-new vendor, a previously-inactive one, or a
  /// kind change (which can't reuse an old listener, so it's rebuilt fresh).
  async fn activate_vendor(&self, row: &VendorRow) {
      match build_one_vendor(row).await {
          Ok(built) => {
              if let BuiltVendor::Velos(v) = &built {
                  self.velos_instances
                      .write()
                      .unwrap_or_else(|e| e.into_inner())
                      .insert(row.name.clone(), v.clone());
              }
              self.vendors
                  .write()
                  .unwrap_or_else(|e| e.into_inner())
                  .insert(row.name.clone(), built.as_dyn());
              self.vendor_errors
                  .write()
                  .unwrap_or_else(|e| e.into_inner())
                  .remove(&row.name);
          }
          Err(e) => {
              self.vendor_errors
                  .write()
                  .unwrap_or_else(|e| e.into_inner())
                  .insert(row.name.clone(), e);
          }
      }
  }
  ```
- [ ] Run `cargo test -p server config::store`, confirm all 5 new tests
  (plus everything else in the file) pass.
- [ ] Run `cargo clippy -p server --all-targets --all-features -- -D
  warnings`, fix anything (watch for the `let`-chain syntax in
  `reconcile_vendors` — this codebase already uses `if let ... && ...`
  chains elsewhere in `store.rs`, e.g. `build_vendor_config`, so it's not a
  new pattern here).
- [ ] Run `cargo test --workspace --all-features` (full workspace — this
  task touches the most central piece of the feature).
- [ ] Commit:
  ```
  git add server/src/config/store.rs
  git commit -m "settings: reconcile live vendors on update()"
  ```

## Task 13 — regenerate clients + web UI (Part 2)

**Files:** `clients/ts/src/generated/**`, `clients/web/src/api/types.ts`
(regenerated); `clients/web/src/pages/SettingsPage.tsx` lines 297-303
(restart banner), 377 (Velos section desc — deferred from task 5), 61-76
(`VelosDraft`, add `error`), 99-121 (`toVelosDrafts`), 789-882 (`VelosRow`).

- [ ] Run `make ts-types` from the repo root — confirm
  `clients/ts/src/generated/settings/vendorView.ts` gains `error?: string`.
  Run `git diff --stat clients/ts/src/generated` to eyeball it.
- [ ] Run `cd clients/web && bun run generate-types` — confirm
  `clients/web/src/api/types.ts` (or wherever it's generated) reflects the
  same.
- [ ] Run `cd clients/web && bun run build` — expect it to still pass (a new
  optional field doesn't break existing callers) but the UI is now stale
  (old restart banner copy, no per-vendor error display) — that's this
  task's remaining work, not a build failure. Treat "the banner still says
  the old, now-inaccurate copy" as the failing condition to fix (no
  automated test enforces UI copy here; this is a manual read-through
  step against the spec's Design section).
- [ ] `VelosDraft` (was lines 61-76): add `error`:
  ```ts
  type VelosDraft = {
    name: string;
    serverUrl: string;
    image: string;
    advertiseHost: string;
    tokenInput: string;
    hasInlineToken: boolean;
    runtimeBin: string;
    workspaceRoot: string;
    listen: string;
    cpu: string;
    memoryMib: string;
    connectTimeoutSecs: string;
    active: boolean;
    error: string | null;
  };
  ```
- [ ] `toVelosDrafts` (was lines 99-121): thread `vd.error` through:
  ```ts
  const toVelosDrafts = (v: SettingsView): VelosDraft[] =>
    v.vendors.flatMap((vd) =>
      vd.config?.kind === "Velos"
        ? [
            {
              name: vd.name,
              serverUrl: vd.config.value.serverUrl,
              image: vd.config.value.image,
              advertiseHost: vd.config.value.advertiseHost,
              tokenInput: "",
              hasInlineToken: vd.config.value.hasInlineToken,
              runtimeBin: vd.config.value.runtimeBin,
              workspaceRoot: vd.config.value.workspaceRoot,
              listen: vd.config.value.listen,
              cpu: num(vd.config.value.cpu),
              memoryMib: num(vd.config.value.memoryMib),
              connectTimeoutSecs: num(vd.config.value.connectTimeoutSecs),
              active: vd.active,
              error: vd.error ?? null,
            },
          ]
        : [],
    );
  ```
  Also add `error: null` to the "Add velos vendor" seed object (in
  `SettingsPage`'s `onAdd`).
- [ ] `VelosRow`: replace the "Not loaded — restart to activate." line (was
  lines 876-878) with the per-vendor error, falling back to a neutral
  not-loaded message when there's no error yet (e.g. a fresh unsaved row):
  ```tsx
  {draft.error && (
    <p className="text-[11px] text-error">{draft.error}</p>
  )}
  {!draft.active && !draft.error && draft.name.trim() && (
    <p className="text-[11px] text-faint">Not loaded yet.</p>
  )}
  ```
- [ ] Reword the global restart banner (was lines 297-303) to match its
  narrowed meaning:
  ```tsx
  {settings?.restartRequired && (
    <div className="flex items-start gap-2 rounded-[var(--radius)] border border-warning/40 bg-warning-soft px-3 py-2 text-sm text-warning">
      <AlertTriangle size={15} className="mt-0.5 shrink-0" />
      A vendor's server URL, listen address, or advertise host changed and
      needs a server restart to take effect. Other vendor edits apply
      immediately.
    </div>
  )}
  ```
- [ ] Fix the Velos section's stale description (deferred from task 5, was
  line 377):
  ```tsx
  desc="Remote container runtimes (velos clusters). Add as many as you need — most changes apply immediately; changing a vendor's listen address or server URL needs a restart."
  ```
- [ ] Run `cd clients/web && bun run build`, confirm clean.
- [ ] Commit:
  ```
  git add clients/ts/src/generated clients/web/src/api clients/web/src/pages/SettingsPage.tsx
  git commit -m "web: show per-vendor error, narrow restart banner copy"
  ```

## Final gate (run in order, fix forward on any red)

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny check advisories bans licenses sources --all-features
make ts-types
git diff --exit-code clients/ts/src/generated
cd clients/web && bun install && bun run generate-types && bun run build
```

Then push and open the PR against `blossomstack/horsie` `main`.

## Self-review notes (spec requirement coverage)

- No `api_key_env`/`token_env` anywhere in DB/wire/web → tasks 1-5.
- Vendor add/edit applies live (new, previously-inactive, active
  non-listener edit) → task 12's `activate_vendor`/`apply_active_vendor_edit`
  + tests.
- Settings view reports *why* a vendor is inactive → task 12's
  `VendorView.error` + `vendor_errors`.
- `restart_required` narrows to the one listener-change case → task 12
  (rename + only set in `apply_active_vendor_edit`'s "changed" branch).
- Non-goals respected: no live rebind of `listen`/`advertise_host`/
  `server_url` (task 12 explicitly routes that to the flag-and-keep-running
  branch); `cli/src/config.rs` untouched (not in any task's file list);
  removed vendor rows don't force-terminate sessions (task 12's removal path
  only drops the map entry — in-flight sessions hold their own `Arc` clone
  from before removal, untouched).
- Type/signature consistency check: `SharedVendors` (task 6) is used
  identically in `spec.rs`, `store.rs` (tasks 7/11/12), `session_actor.rs`
  (task 8), `supervisor.rs`/`http/mod.rs` (task 6). `VelosMutableSettings`
  (task 10) is constructed in exactly two places — `VelosVendor::bind`
  (task 10) and `velos_mutable_settings` (task 11) — with the same field set
  both times. `build_one_vendor`/`BuiltVendor` (task 11) is consumed by both
  `build_vendors` (task 11, boot) and `activate_vendor` (task 12, live
  update) with the same signature.

## Known deviations from the mission brief (tracked here, not asked about mid-flight)

- `build_one_vendor`'s signature drops the brief's suggested `runtime_bin: &Path,
  workspace_root: &Path, public_http_base: Option<&str>` parameters — reading
  the current code, those three are used *only* for the built-in `local`
  vendor (inserted separately in `build_vendors`, never part of `rows`), so a
  per-row builder never needs them today; keeping unused parameters would
  need `_`-prefixing that misleads readers about what a future vendor kind
  might need. `build_one_vendor(row: &VendorRow)` is enough for the one real
  kind (`velos`).
- `reconcile_vendors` compares each row's *previous persisted config* against
  its *new persisted config* (not against "what the live instance actually
  has bound") to decide if listener fields changed. This matches every
  scenario in the spec's Testing summary; it does not special-case a
  double-edit-revert (edit listener, notice the restart-needed flag, edit it
  back to the original value) — that would still (harmlessly) re-flag
  restart-needed once more on the revert, since it only compares
  before/after, not "vs. what's live". Not attempted: no scenario in the
  spec calls for it, and building it would need tracking a third state
  (what's actually bound) beyond DB-before/DB-after.
- The "not-yet-active vendor with a bad token" scenario from the Testing
  summary is implemented with a deliberately-unbindable `listen` address
  instead of a literal bad token — `resolve_velos_token` only errors on an
  explicitly-empty inline token, and the persistence layer's `resolve_secret`
  already converts an explicit empty-string input into "clear the stored
  secret" (`None`) before it ever reaches `resolve_velos_token`, so a "bad
  token" can't deterministically fail at *build* time (only at the vendor's
  first real API call, later). An unbindable listener is the reliable,
  portable way to exercise the same "build fails, error surfaces, view
  reports inactive" code path.
