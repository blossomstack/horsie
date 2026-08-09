# Terraform runtime vendors — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a `terraform-provider-horsie` configuration declare the cloud runtime vendors (Fly, velos) a horsie server builds, the vendor new sessions default to, and read the live vendor roster.

**Architecture:** Two pull requests in two repositories, in order. Phase A changes horsie: it renames the default-vendor API to `default-runtime-vendor` everywhere, and stops `save` silently rewriting `callback_url` so a written value round-trips. Phase B regenerates the provider's vendored wire types against that schema and adds one resource with a nested block per vendor kind, a singleton default-vendor resource, and the provider's first data source.

**Tech Stack:** Rust (axum, sqlx), fluorite codegen, React + bun (WebUI), Go + terraform-plugin-framework, OpenTofu.

**Spec:** `docs/superpowers/specs/2026-08-08-terraform-runtime-vendors-design.md`

## Global Constraints

- Phase A must be merged before Phase B's `make generate` runs. The provider cannot be written against a schema that does not exist.
- Every `.fl` edit regenerates **both** generated type trees — `clients/ts` (`make ts-types`) and `clients/web/src/generated` (`cd clients/web && bun install && bun run generate-types`). CI guards only the first; a missed second tree reddens `main` later.
- `clients/web` installs with `bun install`, never `npm ci`.
- The `settings` table row key `'default_vendor'` is **not** renamed. It is storage, not API.
- Never pass `-c user.name` / `-c user.email` to git. The configured identity is correct.
- No `gh pr merge --auto`. A green PR is the finish line.
- Rust iteration uses `cargo test -p horsie-server --lib`; the full workspace suite runs once before pushing.
- Provider `docs/` is tfplugindocs output — never hand-write a file there, and never put a spec or plan in it. `make docs-check` fails on anything it did not generate.

---

# Phase A — horsie

Worktree: `/Users/xiaoguang/works/repos/bloomstack/october/horsie/.claude/worktrees/tf-vendors`, branch `feat/terraform-runtime-vendors` (already created, holds the spec commit).

### Task A1: `callback_url` is validated, never rewritten

The server currently completes a bare origin into `…/api/runtime/connect`, so a Terraform Required attribute cannot round-trip. Validation stays; the rewrite goes.

**Files:**
- Modify: `crates/server/src/runtime_vendor/config.rs` (`normalise_callback` at :290, `validate` at :245 and :262/:283, `save` at :596, tests at :732 and :766–816)
- Modify: `crates/server/src/http/mod.rs` (vendor CRUD tests around :2540)

**Interfaces:**
- Produces: `pub fn validate_callback(url: &str) -> Result<(), String>` replacing `pub fn normalise_callback(url: &str) -> Result<String, String>`. `validate` keeps its signature `pub fn validate(settings: &StoredVendorSettings, credential: &str) -> Result<(), String>` — note the changed success type.

- [ ] **Step 1: Rewrite the callback tests to the new contract**

In the `mod tests` block of `config.rs`, replace the three normalisation tests with:

```rust
    #[test]
    fn a_bare_origin_is_refused() {
        // The server used to complete this silently. It cannot: a client that
        // declares configuration (Terraform) requires the value it wrote to be
        // the value stored, and a silent rewrite fails its apply with an error
        // that says nothing about the URL. Completing it is a typing affordance
        // and belongs in the form.
        let err = validate_callback("wss://horsie.example.com").unwrap_err();
        assert!(err.contains("/api/runtime/connect"), "{err} must name the path");
    }

    #[test]
    fn a_trailing_slash_is_refused() {
        assert!(validate_callback("wss://horsie.example.com/").is_err());
    }

    #[test]
    fn an_explicit_path_is_accepted() {
        assert!(validate_callback("wss://horsie.example.com/relay/rt").is_ok());
        assert!(validate_callback("wss://horsie.example.com/api/runtime/connect").is_ok());
    }

    #[test]
    fn surrounding_whitespace_is_refused() {
        // Not trimmed, for the same reason a bare origin is not completed:
        // whatever is stored must be what was written.
        assert!(validate_callback(" wss://horsie.example.com/api/runtime/connect").is_err());
    }
```

Update the two remaining callback tests (`a_loopback_callback_is_refused`, `a_non_websocket_scheme_is_refused`) to call `validate_callback` and give every URL in them a path — `ws://localhost:8080/api/runtime/connect`, `wss://[::1]:8080/api/runtime/connect`, `ws://0.0.0.0:8080/api/runtime/connect`, `ws://app.localhost/api/runtime/connect`, `https://horsie.example.com/api/runtime/connect` — so each still fails for the reason it is testing rather than for a missing path.

Update the `settings()` fixture at :732 to `callback_url: "wss://horsie.example.com/api/runtime/connect".to_string()`.

- [ ] **Step 2: Run the tests and watch them fail**

```bash
cargo test -p horsie-server --lib runtime_vendor::config
```

Expected: FAIL — `cannot find function validate_callback`.

- [ ] **Step 3: Replace the normalisation with validation**

In `config.rs`, replace `normalise_callback` with:

```rust
/// Check a vendor's callback URL is one a sandbox could actually dial.
///
/// Validates and never rewrites. An earlier version completed a bare origin
/// with [`CONNECT_PATH`], which is a helpful thing for a form to do and a
/// harmful thing for an API: a client that declares configuration reads back
/// something it did not write, and cannot tell that from drift.
pub fn validate_callback(url: &str) -> Result<(), String> {
    let Some(rest) = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
    else {
        return Err("the callback url must start with ws:// or wss://".to_string());
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(h, _)| h)
        .trim_matches(['[', ']']);
    if host.is_empty() {
        return Err("the callback url has no host".to_string());
    }
    // A sandbox on someone else's infrastructure resolves these to itself, so a
    // vendor configured this way can never work — and would fail as a silent
    // timeout rather than as an error anyone can act on.
    if matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1") || host.ends_with(".localhost")
    {
        return Err(format!(
            "a machine cannot reach '{host}' — the callback url must be an address reachable from outside this server"
        ));
    }
    if path.is_empty() {
        return Err(format!(
            "the callback url must include the connect path, e.g. wss://horsie.example.com{CONNECT_PATH}"
        ));
    }
    Ok(())
}
```

Change `validate`'s return type to `Result<(), String>` and its two tail expressions from `normalise_callback(&fly.callback_url)` / `normalise_callback(&velos.callback_url)` to `validate_callback(…)`.

- [ ] **Step 4: Drop the rewrite from `save`**

In `save` (:596), replace the `let callback = validate(…)?;` line and the whole `let settings = match row.settings { … };` / `let row = RuntimeVendorRow { settings, ..row };` block with:

```rust
        validate(&row.settings, &row.credential)?;
```

so the row is stored exactly as it arrived.

- [ ] **Step 5: Fix the callers the change breaks**

```bash
cargo test -p horsie-server --lib 2>&1 | tail -40
grep -rn "normalise_callback" crates/ | grep -v target
grep -rn "wss://\|ws://" crates/server/src/http/mod.rs | head -20
```

Give every vendor-CRUD test body in `http/mod.rs` a full callback URL (`wss://horsie.example.com/api/runtime/connect`), and update any assertion that expected the completed value.

- [ ] **Step 6: Run the tests and verify they pass**

```bash
cargo test -p horsie-server --lib
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/server/src
git commit -m "fix(vendors): validate the callback url instead of rewriting it"
```

---

### Task A2: the WebUI completes a bare origin

The affordance the server just gave up moves to the form, which is where it belongs.

**Files:**
- Modify: `clients/web/src/pages/settings/CloudVendors.tsx` (:63 `callbackOf`, :101 `submit`, :256 the Callback URL field)
- Test: `clients/web/src/pages/settings/CloudVendors.test.tsx`

**Interfaces:**
- Consumes: nothing from A1 (the server change is independent).
- Produces: `export function withConnectPath(url: string): string` — exported so the test can drive it directly.

- [ ] **Step 1: Write the failing test**

Add to `CloudVendors.test.tsx`:

```tsx
import { withConnectPath } from "./CloudVendors";

describe("withConnectPath", () => {
  it("completes a bare origin", () => {
    expect(withConnectPath("wss://horsie.example.com")).toBe(
      "wss://horsie.example.com/api/runtime/connect",
    );
  });

  it("does not double up a trailing slash", () => {
    expect(withConnectPath("wss://horsie.example.com/")).toBe(
      "wss://horsie.example.com/api/runtime/connect",
    );
  });

  it("leaves an explicit path alone", () => {
    expect(withConnectPath("wss://horsie.example.com/relay/rt")).toBe(
      "wss://horsie.example.com/relay/rt",
    );
  });

  it("trims what the user pasted", () => {
    expect(withConnectPath("  wss://horsie.example.com  ")).toBe(
      "wss://horsie.example.com/api/runtime/connect",
    );
  });

  it("leaves something it cannot parse for the server to refuse", () => {
    expect(withConnectPath("horsie.example.com")).toBe("horsie.example.com");
  });
});
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cd clients/web && bun install && bun run test -- CloudVendors
```

Expected: FAIL — `withConnectPath` is not exported.

- [ ] **Step 3: Implement it**

In `CloudVendors.tsx`, above `callbackOf`:

```tsx
const CONNECT_PATH = "/api/runtime/connect";

/**
 * Complete a bare origin with the connect path.
 *
 * The server validates this field and refuses a URL with no path, because a
 * stored value that differs from the written one is indistinguishable from
 * drift to anything that declares configuration. Completing it is a typing
 * convenience, so it lives here — a URL this cannot parse is passed through
 * untouched and refused server-side, where the message is written for a human.
 */
export function withConnectPath(url: string): string {
  const trimmed = url.trim();
  const rest =
    trimmed.startsWith("wss://") ? trimmed.slice(6)
    : trimmed.startsWith("ws://") ? trimmed.slice(5)
    : null;
  if (rest === null) return trimmed;
  const [, path = ""] = [rest, rest.split("/").slice(1).join("/")];
  return path === "" ? `${trimmed.replace(/\/+$/, "")}${CONNECT_PATH}` : trimmed;
}
```

In `submit`, apply it before sending:

```tsx
      const settings = {
        kind: draft.settings.kind,
        value: {
          ...draft.settings.value,
          callbackUrl: withConnectPath(callbackOf(draft.settings)),
        },
      } as RuntimeVendorSettings;
      await save.mutateAsync({
        name: draft.name,
        body: {
          name: draft.name,
          settings,
          credential: draft.credential || undefined,
        },
      });
```

Update the two placeholders on the Callback URL field to `wss://horsie.example.com/api/runtime/connect` and `ws://horsie.internal:3789/api/runtime/connect`.

- [ ] **Step 4: Run the tests and verify they pass**

```bash
cd clients/web && bun run test -- CloudVendors && bun run build
```

Expected: PASS, and the build typechecks.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/pages/settings
git commit -m "feat(web): complete a bare vendor callback url in the form"
```

---

### Task A3: rename the default-vendor API

**Files:**
- Modify: `crates/models/fluorite/settings.fl` (:97, :139)
- Regenerate: `clients/ts/src/generated/settings/**`, `clients/web/src/generated/settings/**`
- Modify: `crates/server/src/http/config.rs` (:10, :23–34, :137–145), `crates/server/src/http/mod.rs` (:188, and tests at :692, :729–745, :2165, :2913)
- Modify: `crates/server/src/config/mod.rs` (:26, :30, :59), `crates/server/src/config/store.rs` (:96, :151, :160, :186–194, :204–210, :258–299, :490)
- Modify: `clients/web/src/api/client.ts` (:441, :446–448), `clients/web/src/pages/settings/RuntimesSettings.tsx` (:60–62), `clients/web/src/hooks/useSessionDraft.ts` (:159), `clients/web/src/hooks/draftPersistence.ts` (:129–146)
- Modify tests: `clients/web/src/hooks/useSessionDraft.test.tsx`, `clients/web/src/hooks/useAgentDraft.test.tsx`, `clients/web/src/components/configPickers.test.tsx`, `clients/web/src/pages/settings/ModelsSettings.test.tsx`, `clients/web/src/pages/routines/RoutineEditPage.test.tsx`
- Modify: `clients/web/e2e/harness.ts` (:106), `clients/web/e2e/global-setup.ts` (:302), `clients/web/e2e/README.md` (:28)

**Interfaces:**
- Produces: route `/api/config/default-runtime-vendor`; `DefaultRuntimeVendorInput { vendor: String }`; `SettingsView.default_runtime_vendor` (TS: `defaultRuntimeVendor`); `ConfigStore::set_default_runtime_vendor` / `clear_default_runtime_vendor` / `default_runtime_vendor()`.

- [ ] **Step 1: Rename in the schema**

In `settings.fl`, `SettingsView.default_vendor` → `default_runtime_vendor`, and:

```
/// Set the runtime vendor new sessions default to when they name none.
///
/// Deliberately not validated against the live roster: the agent answering to
/// this name may connect long after the preference is set, and rejecting it
/// here would make the setting unusable before its agent is running.
struct DefaultRuntimeVendorInput {
    vendor: String,
}
```

- [ ] **Step 2: Regenerate both type trees**

```bash
make ts-types
cd clients/web && bun install && bun run generate-types && cd ../..
git status --porcelain clients/ts clients/web/src/generated
```

Expected: `defaultVendorInput.ts` deleted and `defaultRuntimeVendorInput.ts` added in **both** trees, `settingsView.ts` changed in both. Generation never deletes on its own, so remove any orphaned `defaultVendorInput.ts` by hand and confirm with `git status`, not `git diff`.

- [ ] **Step 3: Rename through the Rust server**

Mechanically, then read the result:

```bash
grep -rln "default_vendor\|DefaultVendorInput\|default-vendor" crates/server/src crates/models \
  | xargs sed -i '' \
    -e 's/DefaultVendorInput/DefaultRuntimeVendorInput/g' \
    -e 's/put_default_vendor/put_default_runtime_vendor/g' \
    -e 's/delete_default_vendor/delete_default_runtime_vendor/g' \
    -e 's/set_default_vendor/set_default_runtime_vendor/g' \
    -e 's/clear_default_vendor/clear_default_runtime_vendor/g' \
    -e 's/default_vendor/default_runtime_vendor/g' \
    -e 's#/api/config/default-vendor#/api/config/default-runtime-vendor#g'
```

Then **restore the two SQL literals** the blanket rename broke — the DB key stays `default_vendor`:

- `store.rs` `read_setting(&db, db.pool(), &user, "default_vendor")`
- `store.rs` `INSERT INTO settings (user_id, key, value) VALUES (?, 'default_vendor', ?)`
- `store.rs` `DELETE FROM settings WHERE user_id = ? AND key = 'default_vendor'`
- the comments in `crates/server/migrations/{sqlite,postgres}/0001_init.sql`, which describe that key

Add a line to the `store.rs` module docs recording why:

```rust
//! The default runtime vendor is stored under the row key `default_vendor`,
//! which the API rename deliberately left alone: it is storage no client can
//! observe, and renaming it would need a migration whose failure mode is
//! silently resetting every deployment's default.
```

- [ ] **Step 4: Build and fix what the rename missed**

```bash
cargo test -p horsie-server --lib 2>&1 | tail -40
```

Expected: PASS. The `crates/cli/src/main.rs:591` use of the string `"default-vendor"` is an *invalid config key* test and must not change.

- [ ] **Step 5: Rename through the WebUI**

```bash
grep -rln "defaultVendor\|default-vendor\|DefaultVendorInput" clients/web/src clients/web/e2e \
  | xargs sed -i '' \
    -e 's/DefaultVendorInput/DefaultRuntimeVendorInput/g' \
    -e 's/defaultVendor/defaultRuntimeVendor/g' \
    -e 's#/config/default-vendor#/config/default-runtime-vendor#g' \
    -e 's#/api/config/default-vendor#/api/config/default-runtime-vendor#g'
```

Read `clients/web/src/api/client.ts` afterwards and rename the two client methods to match if they are still called `setDefaultVendor` / `clearDefaultVendor`, along with their callers.

- [ ] **Step 6: Verify the WebUI**

```bash
cd clients/web && bun run test && bun run build && cd ../..
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor(config): rename the default-vendor api to default-runtime-vendor"
```

---

### Task A4: documentation

**Files:**
- Modify: `docs/src/content/docs/operating/cloud-vendors.md` (:60 "The callback URL")
- Check: `docs/src/content/docs/operating/configuration.md` (:99), `docs/src/content/docs/operating/local-runtime.md` (:50)

- [ ] **Step 1: Rewrite the callback-URL section**

Replace the "Two things happen when you save it" list with:

```markdown
Two things are checked when you save it:

- It must include a path. The settings form completes a bare origin for you, so
  typing `wss://horsie.example.com` still gives you
  `wss://horsie.example.com/api/runtime/connect` — but the value stored is the
  value sent, so anything configuring horsie over its API writes the full URL.
- An address that only resolves on the server itself — `localhost`,
  `127.0.0.1`, `0.0.0.0`, `::1`, or anything under `.localhost` — is **refused**
  with an error naming the host. Inside a container those names mean the
  container. Without the check a vendor configured this way fails as a session
  that waits forever rather than as something you can act on.
```

- [ ] **Step 2: Check the other two pages**

```bash
grep -rn "default vendor" docs/src/content/docs/operating/
```

Reword to "default runtime vendor" where it names the setting rather than the general idea.

- [ ] **Step 3: Verify and commit**

```bash
make docs-check
git add docs/src && git commit -m "docs: callback urls are validated, not completed"
```

---

### Task A5: full verification and PR

- [ ] **Step 1: Run the whole gate once**

```bash
make check
```

Expected: PASS. Run it once, not twice — it is the expensive one.

- [ ] **Step 2: Run the web e2e suite**

```bash
cd clients/web && TMPDIR=/tmp bun run test:e2e && cd ../..
```

`TMPDIR=/tmp` is required on macOS: Playwright's global setup dies under the default `$TMPDIR` because the socket path exceeds `sun_path`.

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin feat/terraform-runtime-vendors
gh pr create --title "Validate vendor callback urls, and rename the default-vendor api" --body "$(cat <<'EOF'
Two changes the Terraform provider needs before it can manage runtime vendors.

`save` used to complete a bare `callback_url` with `/api/runtime/connect`. A client that declares configuration reads back something it never wrote and cannot tell that from drift, so the server now validates the field and refuses a URL with no path. The completion moves into the settings form, which is where a typing affordance belongs.

`/api/config/default-vendor` becomes `/api/config/default-runtime-vendor`, with `DefaultVendorInput` and `SettingsView.default_vendor` renamed to match. The `settings` row key stays `default_vendor` — it is storage no client can observe, and renaming it would need a migration whose failure mode is silently resetting every deployment's default.

Spec: `docs/superpowers/specs/2026-08-08-terraform-runtime-vendors-design.md`
EOF
)"
```

- [ ] **Step 4: Wait for CI green**

```bash
gh pr checks --watch
```

Seven checks are required on this org; do not merge.

---

# Phase B — terraform-provider-horsie

Starts once Phase A is merged.

### Task B0: worktree and regenerated types

**Files:**
- Modify: `internal/horsieapi/**` (generated)

- [ ] **Step 1: Create the worktree**

```bash
cd /Users/xiaoguang/works/repos/bloomstack/october/terraform-provider-horsie
git fetch origin
git worktree add -b feat/runtime-vendors .claude/worktrees/runtime-vendors origin/main
```

- [ ] **Step 2: Regenerate against merged horsie**

```bash
cd /Users/xiaoguang/works/repos/bloomstack/october/horsie && git checkout main && git pull
cd /Users/xiaoguang/works/repos/bloomstack/october/terraform-provider-horsie/.claude/worktrees/runtime-vendors
make generate HORSIE_FLUORITE=/Users/xiaoguang/works/repos/bloomstack/october/horsie/crates/models/fluorite
go build ./...
```

Expected: new `runtime_vendor_config_input.go`, `runtime_vendor_config_view.go`, `runtime_vendor_settings.go`, `fly_vendor_settings.go`, `velos_vendor_settings.go`, `default_runtime_vendor_input.go`; `default_vendor_input.go` gone. Churn elsewhere is expected and is the point. A compile error here is a schema drift that was previously silent — fix the caller, never the generated file.

- [ ] **Step 3: Commit**

```bash
git add internal/horsieapi && git commit -m "chore: regenerate wire types against horsie main"
```

---

### Task B1: client methods

**Files:**
- Create: `internal/client/runtime_vendors.go`
- Create: `internal/client/runtime_vendors_test.go`
- Modify: `internal/client/config.go` (add `GetSettings`, `SetDefaultRuntimeVendor`, `ClearDefaultRuntimeVendor`)

**Interfaces:**
- Produces:
  - `func (c *Client) ListRuntimeVendors(ctx context.Context) ([]api.RuntimeVendorConfigView, error)`
  - `func (c *Client) GetRuntimeVendor(ctx context.Context, name string) (*api.RuntimeVendorConfigView, error)`
  - `func (c *Client) PutRuntimeVendor(ctx context.Context, name string, in api.RuntimeVendorConfigInput) (*api.RuntimeVendorConfigView, error)`
  - `func (c *Client) DeleteRuntimeVendor(ctx context.Context, name string) error`
  - `func (c *Client) GetSettings(ctx context.Context) (*api.SettingsView, error)`
  - `func (c *Client) SetDefaultRuntimeVendor(ctx context.Context, vendor string) (*api.SettingsView, error)`
  - `func (c *Client) ClearDefaultRuntimeVendor(ctx context.Context) (*api.SettingsView, error)`

- [ ] **Step 1: Write the failing test**

`internal/client/runtime_vendors_test.go`:

```go
package client

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	api "github.com/blossomstack/terraform-provider-horsie/internal/horsieapi"
)

// GetRuntimeVendor filters the list because horsie offers no per-name read.
// The 404 it synthesizes is what Read turns into "removed outside Terraform",
// so it has to be a *client.Error and not a bare error.
func TestGetRuntimeVendorSynthesizesNotFound(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/api/runtime-vendors" {
			t.Errorf("unexpected path %s", r.URL.Path)
		}
		_ = json.NewEncoder(w).Encode([]api.RuntimeVendorConfigView{{Name: "other"}})
	}))
	defer srv.Close()

	_, err := New(srv.URL, "t").GetRuntimeVendor(context.Background(), "missing")
	if !IsNotFound(err) {
		t.Fatalf("want a 404, got %v", err)
	}
}

func TestPutRuntimeVendorUsesTheNameInThePath(t *testing.T) {
	var gotPath, gotMethod string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath, gotMethod = r.URL.Path, r.Method
		_ = json.NewEncoder(w).Encode(api.RuntimeVendorConfigView{Name: "fly prod"})
	}))
	defer srv.Close()

	_, err := New(srv.URL, "t").PutRuntimeVendor(context.Background(), "fly prod",
		api.RuntimeVendorConfigInput{Name: "fly prod"})
	if err != nil {
		t.Fatal(err)
	}
	if gotMethod != http.MethodPut {
		t.Errorf("method = %s, want PUT", gotMethod)
	}
	if gotPath != "/api/runtime-vendors/fly prod" {
		t.Errorf("path = %q, want the name escaped into it", gotPath)
	}
}

func TestSetDefaultRuntimeVendorHitsTheRenamedRoute(t *testing.T) {
	var gotPath string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath = r.URL.Path
		_ = json.NewEncoder(w).Encode(api.SettingsView{DefaultRuntimeVendor: "fly-prod"})
	}))
	defer srv.Close()

	got, err := New(srv.URL, "t").SetDefaultRuntimeVendor(context.Background(), "fly-prod")
	if err != nil {
		t.Fatal(err)
	}
	if gotPath != "/api/config/default-runtime-vendor" {
		t.Errorf("path = %q", gotPath)
	}
	if got.DefaultRuntimeVendor != "fly-prod" {
		t.Errorf("default = %q", got.DefaultRuntimeVendor)
	}
}
```

- [ ] **Step 2: Run it and watch it fail**

```bash
go test ./internal/client/ -run RuntimeVendor -v
```

Expected: FAIL — undefined methods.

- [ ] **Step 3: Implement the client**

`internal/client/runtime_vendors.go` follows `config.go`'s existing shape exactly: `List` does a `GET`, `Get` filters the list and returns `&Error{Status: http.StatusNotFound, …}`, `Put` uses `url.PathEscape(name)`, `Delete` discards the body. `GetSettings` reads `/api/config`; `SetDefaultRuntimeVendor` `PUT`s `api.DefaultRuntimeVendorInput{Vendor: vendor}` to `/api/config/default-runtime-vendor` and decodes a `SettingsView`; `ClearDefaultRuntimeVendor` `DELETE`s the same path and decodes a `SettingsView`.

- [ ] **Step 4: Verify**

```bash
go test ./internal/client/ && go vet ./...
```

- [ ] **Step 5: Commit**

```bash
git add internal/client && git commit -m "feat(client): runtime-vendor and default-runtime-vendor calls"
```

---

### Task B2: the `horsie_runtime_vendor` resource

**Files:**
- Create: `internal/provider/runtime_vendor_resource.go`
- Create: `internal/provider/runtime_vendor_settings_test.go`
- Modify: `internal/provider/provider.go` (register in `Resources`)
- Create: `examples/resources/horsie_runtime_vendor/resource.tf`, `examples/resources/horsie_runtime_vendor/import.sh`

**Interfaces:**
- Consumes: `client.PutRuntimeVendor`, `client.GetRuntimeVendor`, `client.DeleteRuntimeVendor` from Task B1.
- Produces:
  - `func NewRuntimeVendorResource() resource.Resource`
  - `type runtimeVendorModel struct { Name, Credential types.String; HasCredential types.Bool; Fly *flySettingsModel; Velos *velosSettingsModel }`
  - `type flySettingsModel struct { App, Image, Region, WorkspaceRoot, CallbackURL types.String; Volumes types.Bool; CPUKind types.String; CPUs, MemoryMB, VolumeSizeGB types.Int64 }`
  - `type velosSettingsModel struct { ServerURL, Image, RuntimeBin, WorkspaceRoot, CallbackURL types.String; CPU, MemoryMB types.Int64 }`
  - `func (m runtimeVendorModel) settings() (api.RuntimeVendorSettings, error)` and `func (m *runtimeVendorModel) applyView(v api.RuntimeVendorConfigView)`

- [ ] **Step 1: Write the failing tests**

`internal/provider/runtime_vendor_settings_test.go`:

```go
package provider

import (
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/types"

	api "github.com/blossomstack/terraform-provider-horsie/internal/horsieapi"
)

// Every field is non-zero on purpose. A field added by a future `fluorite go`
// arrives as a zero value, which this catches — a green test against a fake
// built from the same generated types proves nothing else about the wire.
func flyModel() *flySettingsModel {
	return &flySettingsModel{
		App:           types.StringValue("horsie-runtimes"),
		Image:         types.StringValue("ghcr.io/x/runtime:1"),
		Region:        types.StringValue("iad"),
		WorkspaceRoot: types.StringValue("/workspaces"),
		CallbackURL:   types.StringValue("wss://horsie.example.com/api/runtime/connect"),
		Volumes:       types.BoolValue(true),
		CPUKind:       types.StringValue("performance"),
		CPUs:          types.Int64Value(2),
		MemoryMB:      types.Int64Value(2048),
		VolumeSizeGB:  types.Int64Value(20),
	}
}

func TestFlySettingsRoundTrip(t *testing.T) {
	m := runtimeVendorModel{Name: types.StringValue("fly-prod"), Fly: flyModel()}
	got, err := m.settings()
	if err != nil {
		t.Fatal(err)
	}
	v, ok := got.Variant.(api.RuntimeVendorSettingsFly)
	if !ok {
		t.Fatalf("variant = %T, want Fly", got.Variant)
	}
	want := api.FlyVendorSettings{
		App: "horsie-runtimes", Image: "ghcr.io/x/runtime:1", Region: "iad",
		WorkspaceRoot: "/workspaces",
		CallbackURL:   "wss://horsie.example.com/api/runtime/connect",
		Volumes:       true, CPUKind: "performance", CPUs: 2, MemoryMB: 2048, VolumeSizeGB: 20,
	}
	if v.Value != want {
		t.Errorf("fly settings = %#v, want %#v", v.Value, want)
	}

	back := runtimeVendorModel{}
	back.applyView(api.RuntimeVendorConfigView{Name: "fly-prod", Settings: got, HasCredential: true})
	if back.Fly == nil || back.Velos != nil {
		t.Fatalf("applyView chose the wrong block: fly=%v velos=%v", back.Fly, back.Velos)
	}
	if *back.Fly != *flyModel() {
		t.Errorf("round trip lost a field: %#v", back.Fly)
	}
	if !back.HasCredential.ValueBool() {
		t.Error("has_credential was dropped")
	}
}

func TestVelosSettingsRoundTrip(t *testing.T) {
	velos := &velosSettingsModel{
		ServerURL:     types.StringValue("http://velos:8080"),
		Image:         types.StringValue("ghcr.io/x/runtime:1"),
		RuntimeBin:    types.StringValue("/usr/bin/horsie-runtime"),
		WorkspaceRoot: types.StringValue("/workspaces"),
		CallbackURL:   types.StringValue("ws://horsie.internal:3789/api/runtime/connect"),
		CPU:           types.Int64Value(4),
		MemoryMB:      types.Int64Value(4096),
	}
	m := runtimeVendorModel{Name: types.StringValue("velos-lab"), Velos: velos}
	got, err := m.settings()
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := got.Variant.(api.RuntimeVendorSettingsVelos); !ok {
		t.Fatalf("variant = %T, want Velos", got.Variant)
	}

	back := runtimeVendorModel{}
	back.applyView(api.RuntimeVendorConfigView{Name: "velos-lab", Settings: got})
	if back.Velos == nil || back.Fly != nil {
		t.Fatalf("applyView chose the wrong block")
	}
	if *back.Velos != *velos {
		t.Errorf("round trip lost a field: %#v", back.Velos)
	}
}

// Without a block the generated marshaller fails with "has no variant set"
// halfway through an apply. It has to be a config error instead.
func TestSettingsRefusesZeroOrTwoBlocks(t *testing.T) {
	if _, err := (runtimeVendorModel{}).settings(); err == nil {
		t.Error("no block: want an error")
	}
	both := runtimeVendorModel{Fly: flyModel(), Velos: &velosSettingsModel{}}
	if _, err := both.settings(); err == nil {
		t.Error("two blocks: want an error")
	}
}
```

- [ ] **Step 2: Run and watch it fail**

```bash
go test ./internal/provider/ -run RuntimeVendor -v
```

Expected: FAIL — undefined types.

- [ ] **Step 3: Implement the resource**

`runtime_vendor_resource.go` mirrors `mcp_server_resource.go`'s structure. Schema:

- `name` — `Required`, `PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()}`, described as: horsie refuses a body whose name disagrees with the path rather than renaming a row, so a rename is a replace.
- `credential` — `Optional: true, Sensitive: true`. Documented as: omit to leave the stored token untouched. A velos deployment that runs without auth is configured with `""`.
- `has_credential` — `Computed: true`.
- Blocks `fly` and `velos`, each a `schema.SingleNestedBlock`, every attribute `Required`. Strings for `app`/`image`/`region`/`workspace_root`/`callback_url`/`cpu_kind`/`server_url`/`runtime_bin`, `schema.BoolAttribute` for `volumes`, `schema.Int64Attribute` for `cpus`/`cpu`/`memory_mb`/`volume_size_gb`. Document `callback_url` as needing the full URL including `/api/runtime/connect`, and `volume_size_gb` as ignored when `volumes = false`.

`settings()` returns an error naming both failure modes; `applyView` sets exactly one block pointer and nils the other.

Implement `resource.ResourceWithValidateConfig` so zero or two blocks is a plan-time diagnostic on the right attribute path, and `resource.ResourceWithImportState` as `resource.ImportStatePassthroughID(ctx, path.Root("name"), req, resp)`.

Create checks `Credential.IsNull()` and fails with: `a new runtime vendor needs a credential; use credential = "" for a velos deployment that runs without auth`. Update sends the credential only when non-null. Read calls `GetRuntimeVendor` and removes the resource from state on `client.IsNotFound`. Delete tolerates a 404.

Register `NewRuntimeVendorResource` in `provider.go`.

- [ ] **Step 4: Verify**

```bash
go test ./... && go vet ./... && gofmt -l .
```

- [ ] **Step 5: Write the examples**

`examples/resources/horsie_runtime_vendor/resource.tf`:

```hcl
# A Fly Machines vendor. The app must already exist -- horsie creates machines,
# not apps -- and the image must have horsie-runtime baked in.
resource "horsie_runtime_vendor" "fly" {
  name       = "fly-prod"
  credential = var.fly_api_token

  fly {
    app            = "horsie-runtimes"
    image          = "ghcr.io/blossomstack/horsie-runtime:latest"
    region         = "iad"
    workspace_root = "/workspaces"
    callback_url   = "wss://horsie.example.com/api/runtime/connect"
    volumes        = true
    cpu_kind       = "shared"
    cpus           = 1
    memory_mb      = 1024
    volume_size_gb = 10
  }
}

# A velos vendor. velos may run without auth, in which case the credential is
# empty -- it cannot be omitted, only emptied.
resource "horsie_runtime_vendor" "velos" {
  name       = "velos-lab"
  credential = ""

  velos {
    server_url     = "http://velos:8080"
    image          = "ghcr.io/blossomstack/horsie-runtime:latest"
    runtime_bin    = "horsie-runtime"
    workspace_root = "/workspaces"
    callback_url   = "ws://horsie.internal:3789/api/runtime/connect"
    cpu            = 1
    memory_mb      = 1024
  }
}
```

`examples/resources/horsie_runtime_vendor/import.sh`:

```shell
# Import by vendor name. The credential is not imported -- horsie never returns
# one -- so it stays unmanaged until the configuration sets it.
terraform import horsie_runtime_vendor.fly fly-prod
```

- [ ] **Step 6: Commit**

```bash
git add internal/provider examples/resources/horsie_runtime_vendor
git commit -m "feat: horsie_runtime_vendor resource"
```

---

### Task B3: the `horsie_default_runtime_vendor` resource

**Files:**
- Create: `internal/provider/default_runtime_vendor_resource.go`
- Modify: `internal/provider/provider.go`
- Create: `examples/resources/horsie_default_runtime_vendor/resource.tf`, `.../import.sh`

**Interfaces:**
- Consumes: `client.GetSettings`, `client.SetDefaultRuntimeVendor`, `client.ClearDefaultRuntimeVendor`.
- Produces: `func NewDefaultRuntimeVendorResource() resource.Resource`, `type defaultRuntimeVendorModel struct { Vendor types.String }`.

- [ ] **Step 1: Implement it**

One `vendor` attribute, `Required`, no `RequiresReplace` — a change is a `PUT`. Create and Update both call `SetDefaultRuntimeVendor`. Read calls `GetSettings` and writes `DefaultRuntimeVendor` into state. Delete calls `ClearDefaultRuntimeVendor`. Import passthrough onto `vendor`, so `terraform import horsie_default_runtime_vendor.this fly-prod` reads the current value back and overwrites it if it disagrees.

The schema description carries both limitations, because neither can be enforced:

```
Which runtime vendor new sessions target when they name none.

A server has exactly one. Declaring two of these against the same server is not
an error Terraform can catch, and they will overwrite each other on every apply.

Deleting this resource clears the preference, and horsie falls back to `local`.
Because horsie reports `local` both when no preference is set and when `local`
is set deliberately, those two states cannot be told apart on read.
```

- [ ] **Step 2: Write the example**

```hcl
resource "horsie_default_runtime_vendor" "this" {
  vendor = horsie_runtime_vendor.fly.name
}
```

```shell
# The id is the vendor name currently set on the server.
terraform import horsie_default_runtime_vendor.this fly-prod
```

- [ ] **Step 3: Verify and commit**

```bash
go build ./... && go vet ./... && go test ./...
git add internal/provider examples/resources/horsie_default_runtime_vendor
git commit -m "feat: horsie_default_runtime_vendor resource"
```

---

### Task B4: the `horsie_runtime_vendors` data source

**Files:**
- Create: `internal/provider/runtime_vendors_data_source.go`
- Create: `internal/provider/runtime_vendors_data_source_test.go`
- Modify: `internal/provider/provider.go` (`DataSources` currently returns nil)
- Create: `examples/data-sources/horsie_runtime_vendors/data-source.tf`

**Interfaces:**
- Consumes: `client.GetSettings`.
- Produces: `func NewRuntimeVendorsDataSource() datasource.DataSource`; `var runtimeVendorObjectType = types.ObjectType{AttrTypes: map[string]attr.Type{"name": types.StringType, "is_default": types.BoolType, "supports_provisioning": types.BoolType}}`.

- [ ] **Step 1: Write the failing test**

```go
package provider

import (
	"context"
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
)

// The element type is hand-written because the model holds a types.List --
// every data source attribute is Computed, and a Go slice cannot hold the
// unknown one carries before refresh. Hand-written and schema drift silently,
// so pin them together.
func TestRuntimeVendorObjectTypeMatchesTheSchema(t *testing.T) {
	var resp datasource.SchemaResponse
	NewRuntimeVendorsDataSource().(datasource.DataSource).Schema(
		context.Background(), datasource.SchemaRequest{}, &resp)

	attr, ok := resp.Schema.Attributes["vendors"]
	if !ok {
		t.Fatal("no vendors attribute")
	}
	nested, ok := attr.(interface{ GetNestedObject() any })
	_ = nested
	_ = ok
	if got := resp.Schema.Attributes["vendors"].GetType(); got.String() == "" {
		t.Fatal("vendors has no type")
	}
	want := resp.Schema.Attributes["vendors"].GetType()
	if got := (list_of(runtimeVendorObjectType)); !got.Equal(want) {
		t.Errorf("element type drifted:\n got %s\nwant %s", got, want)
	}
}
```

with a helper `func list_of(t attr.Type) attr.Type { return types.ListType{ElemType: t} }` in the same file. Adjust the assertion to whatever the framework version exposes — the requirement is that the test fails if a schema attribute is added to the nested object without being added to `runtimeVendorObjectType`.

- [ ] **Step 2: Run and watch it fail**

```bash
go test ./internal/provider/ -run RuntimeVendorObjectType -v
```

- [ ] **Step 3: Implement the data source**

Schema: `vendors`, a `schema.ListNestedAttribute`, `Computed`, whose nested object has `name` (String), `is_default` (Bool) and `supports_provisioning` (Bool), all Computed. Model field is `types.List`.

`Read` calls `GetSettings` and builds the list from `SettingsView.Vendors`, mapping `VendorView.Capabilities.SupportsProvisioning` onto `supports_provisioning`.

Description:

```
Every runtime vendor this server can start a session on.

The live roster rather than the stored configuration: `local`, vendors that
dialled in with `horsie connect`, and vendors configured here all appear. It is
the only way to reference a vendor Terraform did not create.
```

Register `NewRuntimeVendorsDataSource` in `DataSources`.

- [ ] **Step 4: Write the example**

`examples/data-sources/horsie_runtime_vendors/data-source.tf`:

```hcl
data "horsie_runtime_vendors" "all" {}

# Only vendors that can build a workspace can back an environment.
output "provisioning_vendors" {
  value = [
    for v in data.horsie_runtime_vendors.all.vendors : v.name
    if v.supports_provisioning
  ]
}
```

- [ ] **Step 5: Verify and commit**

```bash
go test ./... && go vet ./... && gofmt -l .
git add internal/provider examples/data-sources
git commit -m "feat: horsie_runtime_vendors data source"
```

---

### Task B5: verification against a real horsie

The unit tests above run against a fake built from the same vendored types, so they agree with any mistake those types contain. This task is where bugs are found.

- [ ] **Step 1: Start an isolated server**

```bash
mkdir -p /tmp/tfvendor
cat > /tmp/tfvendor/config.json <<'EOF'
{ "storage": { "state_dir": "/tmp/tfvendor/state", "data_dir": "/tmp/tfvendor/data" } }
EOF
cd /Users/xiaoguang/works/repos/bloomstack/october/horsie
cargo run -p horsie-server --bin horsie-server -- --config /tmp/tfvendor/config.json --addr 127.0.0.1:3899
```

The isolated config file is not optional: `horsie-server` ignores `HORSIE_STATE_DIR` and otherwise writes to the developer's real state directory.

- [ ] **Step 2: Mint a token**

```bash
PW=$(cat /tmp/tfvendor/state/server/initial-admin-password)
TOK=$(curl -s -X POST 127.0.0.1:3899/api/auth/login -H 'content-type: application/json' \
  -d "{\"username\":\"admin\",\"password\":\"$PW\"}" | jq -r .access_token)
export HORSIE_TOKEN=$(curl -s -X POST 127.0.0.1:3899/api/device/tokens \
  -H "authorization: Bearer $TOK" -H 'content-type: application/json' \
  -d '{"label":"tf"}' | jq -r .token)
export HORSIE_ENDPOINT=http://127.0.0.1:3899
```

- [ ] **Step 3: Point tofu at the local build**

```bash
cd .claude/worktrees/runtime-vendors && go build -o /tmp/tfvendor/terraform-provider-horsie .
cat > /tmp/tfvendor/tofurc <<'EOF'
provider_installation {
  dev_overrides { "blossomstack/horsie" = "/tmp/tfvendor" }
  direct {}
}
EOF
export TF_CLI_CONFIG_FILE=/tmp/tfvendor/tofurc
```

- [ ] **Step 4: Apply the examples**

Write `/tmp/tfvendor/main.tf` from `examples/resources/horsie_runtime_vendor/resource.tf` plus the default-vendor resource and the data source, using a dummy Fly token — no Fly account is needed, since saving a vendor validates its settings and constructs an API client without calling out.

```bash
cd /tmp/tfvendor && tofu apply -auto-approve
```

- [ ] **Step 5: Assert an empty second plan**

```bash
tofu plan -detailed-exitcode
```

Expected: exit code 0 and "No changes". **This is the assertion that matters.** A successful apply only shows the writes went through; an empty second plan is what proves every attribute round-trips. A non-empty plan here means an attribute the server rewrites or defaults, and the fix is in the schema, not in the test.

- [ ] **Step 6: Round-trip an import and a destroy**

```bash
tofu state rm horsie_runtime_vendor.fly
tofu import horsie_runtime_vendor.fly fly-prod
tofu plan -detailed-exitcode   # credential is null after import; expect only that diff
tofu destroy -auto-approve
```

- [ ] **Step 7: Record what was verified**

Append the commands and their outcomes to the PR body draft. No commit — this task produces evidence, not code.

---

### Task B6: docs and PR

- [ ] **Step 1: Regenerate the registry docs**

```bash
make docs
git status --porcelain docs/
```

Expected: new `docs/resources/runtime_vendor.md`, `docs/resources/default_runtime_vendor.md`, `docs/data-sources/runtime_vendors.md`.

- [ ] **Step 2: Run the whole gate**

```bash
make all
```

Expected: PASS, including `docs-check`.

- [ ] **Step 3: Commit and open the PR**

```bash
git add docs && git commit -m "docs: regenerate for the runtime-vendor resources"
git push -u origin feat/runtime-vendors
gh pr create --title "Manage remote runtime vendors" --body "$(cat <<'EOF'
`horsie_runtime_vendor` declares a Fly or velos vendor the server builds itself, with one nested block per kind — the block present is the kind, so there is no discriminator to keep in sync and a third substrate is a third block rather than a new resource type.

Every settings field is required. horsie's wire types carry no optionals here and the server applies no defaults on write, so any default the provider offered would be a third copy of the same constants and would keep writing stale values if horsie's ever changed.

`horsie_default_runtime_vendor` sets what new sessions target when they name none. `data.horsie_runtime_vendors` reads the live roster — `local`, dialled-in agents and configured vendors alike — which is the only way to reference a vendor Terraform did not create.

Verified against a real horsie: apply, an empty second plan, an import round trip and a destroy.
EOF
)"
gh pr checks --watch
```

---

## Self-review

- **Spec coverage.** Rename → A3. `callback_url` → A1 (server), A2 (form), A4 (docs). Wire types → B0. Client → B1. `horsie_runtime_vendor` with per-kind blocks, all-Required fields, credential semantics, no timestamps, import → B2. Default-vendor resource → B3. Data source → B4. Real-server verification → B5. Provider docs → B6.
- **Naming consistency.** `validate_callback` is used in A1 steps 1, 3 and 5. `withConnectPath` in A2 steps 1 and 3. `settings()` / `applyView` in B2 steps 1 and 3. `SetDefaultRuntimeVendor` / `ClearDefaultRuntimeVendor` / `GetSettings` in B1 and are consumed in B3 and B4.
- **Known soft spot.** Task B4 step 1's schema assertion is written against an API surface that varies by framework version; the step states the invariant to preserve so it can be adjusted rather than dropped.
