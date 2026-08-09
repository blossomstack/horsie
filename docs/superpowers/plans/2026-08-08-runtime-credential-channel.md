# Runtime credential channel — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** make the dial token the only credential in a runtime's environment, and mint the GitHub token and bundle access on demand against it.

**Architecture:** the server becomes the sole minter of dial tokens and ships them in `RuntimeSpec.env`, which already carries server-minted secrets. That makes the token verifiable by the server for *every* vendor, including `horsie connect`, which today signs with a per-process random secret. Two expiring secrets then stop riding the environment: bundle artifacts authenticate with the dial token directly, and GitHub moves behind a git credential helper that mints per operation.

**Tech Stack:** Rust (axum, tokio, sqlx `Any`, reqwest, clap), fluorite schemas for wire types.

**Spec:** `docs/superpowers/specs/2026-08-08-runtime-credential-channel-design.md`

## Global Constraints

- Workspace lints forbid `unwrap`/`expect`/`panic` outside `#[cfg(test)]`; test modules carry the existing `#[allow(...)]` header.
- Every write transaction uses `Db::begin_write()`, never `pool().begin()`.
- Wire types live in `crates/models/fluorite/*.fl`. **Editing any `.fl` requires regenerating both TS trees** — `cd clients/web && bun run generate-types` and `cd clients/ts && npm run generate-types`. CI only guards `clients/ts`.
- No backward-compatibility shims. Delete the old shape; do not deprecate it.
- Pre-push verification is `cargo test --workspace`, never `-p horsie-server`.

---

### Task 1: Server mints every dial token

**Files:**
- Modify: `crates/server/src/runtime_manager.rs` — `RuntimeDeps`, `runtime_spec`
- Modify: `crates/server/src/users.rs` — pass `dial_secret` into `RuntimeDeps`
- Modify: `crates/server/src/runtime_vendor/fly.rs` — drop `dial_secret`/`account` fields and `dial_token()`
- Modify: `crates/server/src/runtime_vendor/velos.rs` — same
- Modify: `crates/server/src/runtime_vendor/config.rs` — stop threading `dial_secret` into the two vendors
- Test: the existing `#[cfg(test)]` modules in each of the above

**Interfaces:**
- Consumes: `horsie_support::dial_token::{mint, DialClaims}`, `UserServices::dial_secret: Arc<Vec<u8>>`
- Produces: `RuntimeDeps { vendors, github_tokens, plugins, dial_secret: Arc<Vec<u8>>, account: String }`; `runtime_spec` pushes `EnvVar { name: ENV_CONNECT_TOKEN, value: <minted> }`

- [ ] **Step 1: Add the failing test in `runtime_manager.rs`**

```rust
#[tokio::test]
async fn the_spec_carries_a_dial_token_the_account_secret_verifies() {
    let deps = deps_with_dial_secret(b"s3cret".to_vec(), "acct-1");
    let manager = RuntimeManager::new(deps);
    let spec = manager
        .runtime_spec("sess-1", &SessionSpec::for_vendor("v"))
        .await
        .unwrap();
    let token = spec
        .env
        .iter()
        .find(|e| e.name == horsie_models::ENV_CONNECT_TOKEN)
        .expect("the spec must carry a dial token");
    let claims = horsie_support::dial_token::verify(b"s3cret", &token.value).unwrap();
    assert_eq!(claims.runtime_id, "sess-1");
    assert_eq!(claims.user_id, "acct-1");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p horsie-server --lib runtime_manager::tests::the_spec_carries_a_dial_token`
Expected: FAIL — `RuntimeDeps` has no `dial_secret` field.

- [ ] **Step 3: Add the fields and the mint**

In `RuntimeDeps` add `pub dial_secret: Arc<Vec<u8>>` and `pub account: String`. At the top of `runtime_spec`'s env assembly, after the environment's own vars:

```rust
rt_spec.env.push(horsie_models::executor::EnvVar {
    name: horsie_models::ENV_CONNECT_TOKEN.to_string(),
    value: horsie_support::dial_token::mint(
        &self.deps.dial_secret,
        &horsie_support::dial_token::DialClaims {
            user_id: self.deps.account.clone(),
            runtime_id: session.to_string(),
        },
    ),
});
```

Wire both fields from `UserServices` in `users.rs`. Every `RuntimeDeps` literal in test modules needs the two new fields.

- [ ] **Step 4: Delete vendor-side minting**

Remove `dial_secret` and `account` from `FlyRuntimeVendor` and `VelosRuntimeVendor`, delete both `dial_token()` methods, and delete the `env.push(ENV_CONNECT_TOKEN …)` in `spec_for`/`launch_spec`. Update `config.rs`'s constructor calls. Keep `fly.rs`'s `the_dial_token_rides_the_environment_and_never_argv` test but retarget it: the token now arrives via `spec.env`, so the test seeds it there and asserts it reaches the machine env and never argv.

- [ ] **Step 5: Run the crate tests**

Run: `cargo test -p horsie-server --lib`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git commit -am "feat(runtime): the server mints every dial token"
```

---

### Task 2: The `horsie connect` listener authenticates by issued-token lookup

**Files:**
- Create: `crates/runtime-vendor/src/issued_tokens.rs`
- Modify: `crates/runtime-vendor/src/listener.rs` — `serve_runtime_connections`, `upgrade_and_serve`
- Modify: `crates/runtime-vendor/src/vendor.rs` — drop `dial_secret`, record issued tokens
- Modify: `crates/runtime-vendor/src/runtime_vendor.rs` — delete `new_dial_secret`
- Modify: `crates/runtime-vendor/src/lib.rs` — exports
- Modify: `crates/cli/src/connect.rs` — drop the secret, share an `IssuedTokens`
- Test: `crates/runtime-vendor/src/issued_tokens.rs` unit tests, `crates/runtime/tests/provision_steps.rs` fixture update

**Interfaces:**
- Produces: `IssuedTokens::new() -> Arc<IssuedTokens>`, `issue(&self, token: &str, runtime_id: &str)`, `resolve(&self, token: &str) -> Option<String>`, `revoke_runtime(&self, runtime_id: &str)`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn only_a_token_this_vendor_issued_resolves() {
    let issued = IssuedTokens::new();
    issued.issue("tok-a", "rt-1");
    assert_eq!(issued.resolve("tok-a").as_deref(), Some("rt-1"));
    assert_eq!(issued.resolve("tok-b"), None);
}

#[test]
fn reissuing_for_one_runtime_retires_its_previous_token() {
    // A revive mints a fresh token for the same runtime. The old one must stop
    // working, or a leaked token outlives every rotation forever.
    let issued = IssuedTokens::new();
    issued.issue("old", "rt-1");
    issued.issue("new", "rt-1");
    assert_eq!(issued.resolve("old"), None);
    assert_eq!(issued.resolve("new").as_deref(), Some("rt-1"));
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p horsie-runtime-vendor --lib issued_tokens`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement `IssuedTokens`**

A `Mutex<HashMap<String, String>>` token→runtime plus a `HashMap<String, String>` runtime→token so `issue` can retire the previous one. Constant-time comparison is not needed: this is an exact-match map lookup, not a secret comparison.

- [ ] **Step 4: Rewire the listener and vendor**

`serve_runtime_connections(listener, connected, issued, cancel)` replaces the `dial_secret` parameter. `upgrade_and_serve` resolves the bearer through `issued.resolve(token)` instead of `dial_token::verify`, refusing when it is `None`. In `vendor.rs::provision`, read the token out of `request.env` and call `issued.issue(token, runtime_id)` before spawning; delete the `dial_secret` field, `with_dial_secret`, and `new_dial_secret`.

- [ ] **Step 5: Run the crate tests**

Run: `cargo test -p horsie-runtime-vendor`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git commit -am "feat(vendor): authenticate dial-backs by issued token"
```

---

### Task 3: `HORSIE_SERVER_URL` for every vendor

**Files:**
- Modify: `crates/models/src/lib.rs` — rename `ENV_PLUGINS_BASE` → `ENV_SERVER_URL`, value `HORSIE_SERVER_URL`
- Create: `crates/server/src/runtime_vendor/server_url.rs` — `http_base_of(callback_url: &str) -> String`
- Modify: `crates/server/src/runtime_vendor/fly.rs`, `velos.rs` — push `ENV_SERVER_URL` and `ENV_PLUGINS_DIR`
- Modify: `crates/runtime-vendor/src/vendor.rs` — `bundle_env` emits the renamed var
- Modify: `crates/runtime/src/plugins_fetch.rs` — read the renamed var

**Interfaces:**
- Produces: `pub fn http_base_of(callback_url: &str) -> String`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_callback_url_becomes_the_http_base_its_runtimes_fetch_from() {
    assert_eq!(
        http_base_of("wss://horsie.example.com/api/runtime/connect"),
        "https://horsie.example.com"
    );
    assert_eq!(
        http_base_of("ws://horsie:8080/api/runtime/connect"),
        "http://horsie:8080"
    );
    // A bare origin is already the base.
    assert_eq!(http_base_of("ws://horsie:8080"), "http://horsie:8080");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p horsie-server --lib server_url`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement and wire**

`http_base_of` swaps the scheme and trims a trailing `/api/runtime/connect`. Both cloud vendors push `ENV_SERVER_URL` (from `http_base_of(&self.settings.callback_url)`) and `ENV_PLUGINS_DIR` (a path under the workspace root) into the machine env, which is what makes bundles reach a cloud runtime for the first time.

- [ ] **Step 4: Run the crate tests**

Run: `cargo test -p horsie-server --lib && cargo test -p horsie-runtime-vendor`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(runtime): one server URL every vendor supplies"
```

---

### Task 4: Artifacts authenticate with the dial token

**Files:**
- Modify: `crates/server/src/http/plugins.rs` — `get_artifact`
- Delete: `crates/server/src/plugins/token.rs`
- Modify: `crates/server/src/plugins/{mod.rs,service.rs,artifact.rs}` — drop `mint_token`, `sign_token`, `verify_token`, `TOKEN_TTL_SECS`, `token_secret`
- Modify: `crates/server/src/users.rs`, `boot.rs`, `bin/horsie-server/main.rs` — delete `artifact_secret` and `HORSIE_ARTIFACT_SECRET`
- Modify: `crates/server/src/runtime_manager.rs` — stop pushing `ENV_PLUGINS_TOKEN`
- Modify: `crates/models/src/lib.rs` — delete `ENV_PLUGINS_TOKEN`
- Modify: `crates/runtime/src/plugins_fetch.rs` — send the dial token as bearer

**Interfaces:**
- Consumes: `dial_token::{claimed_account, verify}`, `config::dial_secret_of`, `PluginStore::list()`
- Produces: `get_artifact` authorising on `(account, hash)`

- [ ] **Step 1: Write the failing tests in `http/mod.rs`**

```rust
#[tokio::test]
async fn an_artifact_needs_a_dial_token_for_an_account_that_installed_it() {
    // installed by acct-1; acct-2 must not be able to fetch it
}

#[tokio::test]
async fn an_artifact_the_account_never_installed_is_refused() {}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p horsie-server --lib plugin_artifact`
Expected: FAIL — the route still verifies an HS256 capability token.

- [ ] **Step 3: Rewrite `get_artifact`**

Parse the bearer, `claimed_account` → `dial_secret_of` → `verify`. On success resolve the account's services, read `plugins.store().list()`, and refuse unless some row's `artifact_hash` equals the requested hash. A missing account, a bad signature and an unreferenced hash all answer the same way, so the route never confirms an account or an artifact exists.

- [ ] **Step 4: Delete the old machinery**

Remove `plugins/token.rs` and every reference; `provision_plugins` sends `ENV_CONNECT_TOKEN` as the bearer.

- [ ] **Step 5: Run the crate tests**

Run: `cargo test -p horsie-server && cargo test -p horsie-runtime`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git commit -am "feat(plugins): artifacts authenticate with the dial token"
```

---

### Task 5: GitHub credential helper

**Files:**
- Create: `crates/server/src/http/runtime_credentials.rs` — `GET /api/runtime/github-credential`
- Modify: `crates/server/src/http/mod.rs` — route registration, outside `require_auth`
- Create: `crates/runtime/src/git_credential.rs` — the helper protocol
- Modify: `crates/runtime/src/main.rs` — `GitCredential` subcommand, `GIT_CONFIG_*` at startup
- Modify: `crates/runtime/src/steps.rs` — drop `github_token`, `is_github`, `github_auth_header`
- Modify: `crates/server/src/runtime_manager.rs` — stop minting/pushing `GITHUB_TOKEN`
- Modify: `crates/models/src/lib.rs` — delete `ENV_GITHUB_TOKEN`
- Modify: `crates/server/src/environments/service.rs` — the reserved-name list loses `GITHUB_TOKEN`

**Interfaces:**
- Produces: `git_credential::respond(input: &str, server: &str, token: &str) -> Option<String>` — parses git's key/value block, returns the `username=…\npassword=…` reply or `None`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_get_request_without_a_path_cannot_be_scoped_and_is_declined() {
    // Without credential.useHttpPath git omits `path=`, and a token scoped to
    // "every repo" is exactly what this design refuses to mint.
    assert_eq!(query_of("protocol=https\nhost=github.com\n"), None);
}

#[test]
fn a_host_other_than_github_is_declined() {
    assert_eq!(
        query_of("protocol=https\nhost=gitlab.com\npath=o/r.git\n"),
        None
    );
}

#[test]
fn the_repo_path_loses_its_git_suffix() {
    assert_eq!(
        query_of("protocol=https\nhost=github.com\npath=o/r.git\n").unwrap(),
        ("github.com".to_string(), "o/r".to_string())
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p horsie-runtime --lib git_credential`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement the helper**

`respond` parses stdin's `key=value` lines, requires `protocol=https` and `host=github.com` and a non-empty `path`, then GETs
`{server}/api/runtime/github-credential?host=…&path=…` with the dial token as bearer, and prints `username=x-access-token` plus `password=<token>`. Any failure prints nothing and exits 0 — git reads that as "no credentials", which is what keeps a public-repo clone working on a deployment with no GitHub connection.

- [ ] **Step 4: Implement the server endpoint**

Verify the dial token exactly as `runtime_connect.rs` does. Then `supervisor` `Get` on the runtime id (the runtime id *is* the session id), read `record.spec.provision`, collect the `git_checkout` urls, and refuse unless the requested `host`/`path` matches one. On success return `mint_token_for(&[matched_url])`.

- [ ] **Step 5: Set the git config at runtime startup**

In sync `main()`, before the tokio runtime is built — the only window where `set_var` is sound — set `GIT_CONFIG_COUNT=2` plus the `helper` and `useHttpPath` pairs, using `std::env::current_exe()`. Document why the `unsafe` block is sound.

- [ ] **Step 6: Strip the baked token from provisioning**

`run_steps` loses its `github_token` parameter; `git_checkout` clones with no `GIT_CONFIG_*` of its own and inherits the helper. Delete the now-unused `is_github` and `github_auth_header` and their tests, and the `GITHUB_TOKEN` mint in `runtime_spec`.

- [ ] **Step 7: Run the tests**

Run: `cargo test -p horsie-runtime && cargo test -p horsie-server`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git commit -am "feat(github): mint repo credentials per operation"
```

---

### Task 6: Whole-workspace verification and the PR

- [ ] **Step 1: Format and lint**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
```

- [ ] **Step 2: Regenerate both TS trees if any `.fl` changed**

```bash
cd clients/web && bun run generate-types
cd ../ts && npm run generate-types
cd ../.. && git status --porcelain
```

Expected: clean. `git status`, not `git diff` — generation leaves orphans.

- [ ] **Step 3: Full workspace suite**

```bash
cargo test --workspace
```

- [ ] **Step 4: Web gates**

```bash
cd clients/web && bun run typecheck && bun run test:unit
```

- [ ] **Step 5: Grep the e2e harness for anything that moved**

```bash
grep -rn "plugin-artifacts\|PLUGINS_TOKEN\|PLUGINS_BASE\|ARTIFACT_SECRET\|GITHUB_TOKEN" clients/web/e2e crates/tests docker/ render.yaml fly.toml
```

- [ ] **Step 6: Push and open the PR**

Do not enable auto-merge. Watch all eight required checks, including `license/cla`, which silently drops and is re-triggered by commenting `recheck`.
