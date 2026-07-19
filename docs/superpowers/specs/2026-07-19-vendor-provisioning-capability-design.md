# Vendor-announced provisioning capability

## Problem

A session's new-session form has to decide what workspace inputs to prompt for
(git repos, skill bundles, a directory). Today the web UI has no way to know
what the chosen runtime vendor can actually honor, so it either guesses by
vendor name (hardcode) or shows inputs the vendor will reject at `create()`:

- `velos` provisions a fresh managed workspace and can clone repos / install
  skill bundles into it.
- `local` (the shared local-runtime daemon vendor) runs in the connected
  daemon's own fixed directory and provisions nothing — it rejects repos,
  host-dir workspaces, and (by the same reasoning) skill provisioning.

The current UI still offers a "Local directory" workspace input and a
Scratch/repos toggle that no vendor honors, plus a `workdirs` field that is
dead end-to-end (no vendor accepts a caller-supplied host directory).

There will be more vendor kinds in future, so the answer must not be hardcoded
per vendor. **The vendor should announce its capabilities; the server and UI
translate that announcement into what the user may specify.**

## Goals

- Each vendor announces, in one place, whether it supports **provisioning**
  (general — repos today, skills and others later; all unsupported by `local`).
- The server surfaces that capability on the settings wire so the UI can adapt.
- The new-session UI shows provisioning inputs (repo picker, skill bundles,
  enable-plugins) only for a vendor that supports provisioning; for a
  non-provisioning vendor (`local`) it prompts for nothing beyond name/model.
- Remove the dead user-specified `workdirs` / host-directory path end to end.

## Non-goals / explicitly out of scope

- **The runtime-reported working directory** (`RuntimeReady.workdir` →
  `LocalDaemonVendor::workdir()`, in the daemon dial-back protocol) is left
  untouched. That is the real, runtime-sourced working dir that will feed the
  agent system prompt next; it is a distinct concept from the removed
  user-specified new-session workdir. Tracked in issue #13.
- No change to the runtime dial-back / daemon protocol (`daemon.fl`).
- No change to the handler's repo→provision translation
  (`provision_from_repos`).

## Design

### 1. The announce point — `RuntimeVendor::capabilities()`

A single trait method returns a small domain struct. Each vendor implements it
directly; nothing matches on vendor name or kind anywhere.

```rust
// server/src/vendor/mod.rs

/// What a vendor can do with a session's workspace, announced by the vendor
/// itself so the server and UI never branch on vendor name/kind. Extensible:
/// add a field here and each vendor declares its own value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorCapabilities {
    /// The vendor provisions a fresh workspace it owns — cloning repos,
    /// installing skill bundles, running provision steps. A vendor that runs
    /// in a fixed, user-owned directory (e.g. the shared local daemon)
    /// provisions nothing and announces `false`.
    pub supports_provisioning: bool,
}

pub trait RuntimeVendor: Send + Sync + 'static {
    fn capabilities(&self) -> VendorCapabilities;
    // …existing create/attach/delete…
}
```

Values:

| Vendor | `supports_provisioning` |
| --- | --- |
| `velos` (`VelosVendor`) | `true` |
| `local` (`LocalDaemonVendor`) | `false` |
| `mock` (test-only) | `true` (so provisioning-path tests keep exercising it) |

`LocalDaemonVendor::reject_unsupported_inputs` becomes the *enforcement* of the
`false` it announces — the announcement and the runtime rejection stay in the
same file, consistent with each other.

### 2. Server → wire — `VendorView.capabilities`

`models/fluorite/settings.fl`:

```
/// What a vendor can do with a session workspace. Announced by the vendor;
/// the UI reads it to decide what to prompt for at session creation.
struct VendorCapabilities {
    /// The vendor provisions a workspace it owns (repos, skills, …). A
    /// fixed-directory vendor provisions nothing.
    supports_provisioning: bool,
}

struct VendorView {
    name: String,
    active: bool,
    is_default: bool,
    config: Option<VendorConfigView>,
    error: Option<String>,
    /// Announced capabilities of the live vendor instance. `None` when the
    /// vendor is configured but not currently loaded (no instance to ask);
    /// the new-session UI only offers active vendors, so it always has a value
    /// where it matters.
    capabilities: Option<VendorCapabilities>,
}
```

`DbConfigStore::vendors_view()` fills `capabilities` from the **live instance**
in the `vendors` map (`live.get(name).map(|v| v.capabilities().into())`), for
both daemon-registered vendors and DB-row vendors. Inactive DB rows have no
live instance → `None`. No hardcoding by name/kind.

Regenerate both generated TS clients (`clients/web` and `clients/ts`) with
`fluorite ts` so the drift check passes.

### 3. UI — capability-driven new-session form (`NewSessionModal.tsx`)

- **Vendor selector** becomes a primary field (rendered above Workspace) when
  more than one vendor is active; with a single active vendor it is implicit.
  Its selection drives everything below.
- Resolve `const caps = activeVendors.find(v => v.name === effectiveVendor)?.capabilities;`
- **Provisioning inputs are gated on `caps?.supportsProvisioning`:**
  - `true` → show the GitHub repo picker (zero repos selected = empty managed
    workspace / scratch) **and** the Enable-plugins toggle + skill-bundles
    selector.
  - `false` → show none of them; the form is Name + Model (+ vendor if >1).
- Remove the Scratch/repos toggle, the `WorkspaceSource` union, the `workdir`
  input, and the `effectiveVendor !== "local"` warning added earlier.
- `submit`: send `repos` only when `caps?.supportsProvisioning` and repos are
  selected; never send `workdirs`. Skill-bundle `plugins` likewise only when
  provisioning is supported.

### 4. Remove the dead user-specified workdir path

The user-supplied host directory is already non-functional (every vendor
rejects it). Remove it end to end:

- `models/fluorite/session_api.fl`: drop `workdirs` from `CreateSessionRequest`.
- `server/src/http/handlers.rs`: drop the `workdirs` branch and the
  `workdirs`/`repos` mutual-exclusion check; new sessions always build a
  managed workspace (plus repo provision steps when repos are given).
- `models/fluorite/session.fl`: drop `workdirs` from `SessionSummary`/detail;
  drop its handler response mapping and its display in
  `clients/web/src/pages/SessionView.tsx`. (Always empty post-change, and the
  future working dir is runtime-sourced and single-valued, not a user list —
  see issue #13.)
- `server/src/vendor/mod.rs`: remove `WorkspaceSource::HostDir`; the session
  workspace source is always `Managed`.
- `server/src/sessions/`: remove the per-workspace `path` field and the
  `HostDir` construction in `session_actor.rs`.
- `server/src/vendor/velos.rs` and `local.rs`: remove the now-unreachable
  host-dir reject arms and their tests.
- **Do not touch** `models/fluorite/daemon.fl` or the runtime-reported
  `workdir` — see Non-goals and issue #13.

## Data flow (after)

```
NewSessionModal
  reads settings.vendors[].capabilities.supportsProvisioning
  → shows repo picker + skills only when true
  → POST /api/sessions { vendor, repos?, plugins? }   (no workdirs)
handlers.rs
  → repos → provision steps (provision_from_repos); workspace always Managed
session_actor
  → RuntimeSpec { workspaces: [Managed], provision, … }
  → vendor.create()   (velos provisions; local rejects any provision — but the
                        UI never sends provisioning to a non-provisioning vendor)
GET /api/config
  → vendors_view() reads live instance → VendorView.capabilities
```

## Testing

- **Rust unit:** `capabilities()` returns the expected value for velos / local
  / mock; `vendors_view()` reports `Some(caps)` for an active vendor and `None`
  for an inactive DB row. Update the existing `config_get_and_put_round_trip`
  and vendor tests for the removed `HostDir`/`workdirs`.
- **Handler:** creating a session with `repos` still provisions; the removed
  `workdirs` tests (`create_rejects_workdirs_and_repos_together`,
  `create_without_workdirs_gets_managed_workspace`, the `workdirs:["/tmp"]`
  case) are removed or rewritten to the managed-only shape.
- **Web build:** `bun run build` (tsc typecheck + vite) is clean; both TS
  clients regenerate with no drift.
- Full `cargo test`, `cargo clippy --workspace --all-targets`, `cargo fmt
  --check`, and `cargo-deny` green (CI parity).

## Rollout

This extends the existing PR #11 branch
(`fix/settings-hide-unregistered-local-vendor`), which already stopped
hardcoding the `local` vendor row and removed the misleading
`RuntimeVendor::name()`. The capability announce is the natural completion of
that work: the same "vendors describe themselves, the UI adapts" principle.
