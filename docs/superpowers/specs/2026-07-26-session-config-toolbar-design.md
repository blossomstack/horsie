# Session Creation via an Inline Config Toolbar

**Date:** 2026-07-26
**Status:** Approved design, pre-implementation

## Problem

Creating a new session today is a Radix **Dialog popup** (`clients/web/src/components/NewSessionModal.tsx`) launched from the sidebar "New" button. It collects a name, model, runtime vendor, and — for a provisioning vendor — repos, skill bundles, MCP servers, and a `usePlugins` toggle, then `POST /api/sessions` and navigates to `/sessions/:id`.

We want a more direct, chat-first experience:

- No popup. Opening "new chat" shows the chat UI directly.
- No session-name field (sessions already auto-title from the first message, PR #19).
- A **toolbar above the input box** where the user picks the **runtime**. For a local runtime that's all. For a remote/provisioning runtime (velos-style), the toolbar also exposes **repos + skills + MCP** selectors. Each is a button-with-dropdown.
- The user can only send the **first message once configuration is complete**; send is blocked while required config is missing.
- When loading an **existing** session, the same toolbar area shows its configuration **read-only** (not changeable).
- Resource allocation should be **deferred to the first message** — session actors and their resources are only initialized when the user sends.

## Approach

**Client-only draft.** Opening "new chat" creates nothing on the server. The runtime/model/repos/skills/MCP configuration lives purely in React state. The first message does `POST /api/sessions` with the full config, then sends the message, then navigates to `/sessions/:id`. A session appears in the sidebar only after the first message (ChatGPT-style).

This satisfies "defer resource allocation to first message" with **no backend lifecycle change**: because no server session exists until the first send, actor spawn, workspace scan, and repo/skill provisioning already happen only at first message.

**Shared toolbar component, two modes.** A single `SessionConfigBar` renders the toolbar for both the editable draft (`mode: 'draft'`) and the read-only existing session (`mode: 'locked'`). Draft mode's controls are interactive and write to draft state; locked mode's are disabled and read from the loaded `SessionDetail`.

The index route (`/`) becomes the new-chat view instead of the static `Welcome` landing. This reuses the existing `Composer` and transcript shell (an empty draft is just an empty transcript) and keeps `SessionView` from growing draft-vs-live conditionals.

### Alternatives considered

- **Server session created eagerly, actors/resources lazy** — a persisted "draft" session row with deferred actor init. Rejected: more backend work (new draft state, config mutability before start) for durability we don't need pre-send.
- **Dedicated `/new` route** — functionally identical to using `/`; one extra route, no gain.
- **Unify draft + live into `SessionView` with a nullable id** — maximal shell reuse but overloads `SessionView` with `if (draft)` branches.

## Frontend design

### Routing & components

- **`/` (index)** renders a new **`NewSessionView`**: the chat shell in draft state — an empty transcript area, `SessionConfigBar` in `draft` mode, and the existing `Composer`. Replaces the static `Welcome` at this route.
- **Sidebar "New" button** navigates to `/` and resets draft state (was: open modal). `NewSessionModal.tsx` is deleted.
- **`/sessions/:id`** (`SessionView`) renders the same `SessionConfigBar` in `locked` mode, populated from `SessionDetail`, above `Composer`. This subsumes the current ad-hoc header chips for model/vendor/repos and additionally surfaces skills/MCP.
- **`SessionConfigBar`** (`src/components/SessionConfigBar.tsx`) — new shared component, prop `mode: 'draft' | 'locked'`.

### The toolbar (`SessionConfigBar`)

A horizontal bar directly above the input, inside the same `max-w-3xl` container as `Composer` (mirroring the existing `pendingQuestion` banner precedent at `Composer.tsx`).

Left group — button-with-dropdown selectors:

- **Runtime** — always shown. Lists active vendors (`useSettings().vendors`, filtered `active`). Defaults to `settings.defaultVendor`. Branch on capability, not name: a vendor with `capabilities.supportsProvisioning === false` ("local") hides the remote-only controls; `true` ("velos"/remote) reveals them.
- **Repos** — remote-only. Multi-select GitHub repo picker (reuses `useGithubRepos` / `useGithubStatus`). If GitHub isn't connected, the button shows a "Connect GitHub" state linking to Settings — this is the gating condition for remote.
- **Skills** — remote-only. Multi-select of skill bundles (`usePlugins()` → the request's `plugins: string[]`). Default-enabled bundles (`enabledDefault`) pre-selected. No separate on/off toggle.
- **MCP** — remote-only. Multi-select of MCP servers (reuses `useMcp`).

Right group:

- **Model** — always shown, standalone control on the right. Editable in draft mode; disabled/read-only in locked mode. Kept structurally independent so a future change can make it editable on existing sessions without layout rework.

In **locked mode** every control renders as a static, disabled chip/button showing the chosen value(s), non-interactive.

### Draft state & send-gating

Draft config lives in a small hook `useSessionDraft` in `NewSessionView`, initialized to: `vendor = defaultVendor`, `model = first configured model`, `repos = []`, `skills = default-enabled bundles`, `mcp = []`.

**Send enabled only when config is complete.** `Composer`'s send button and Enter-to-send are gated by `canSend`:

- Model selected — always required.
- Runtime selected — required (defaulted; blocks only if no vendor configured).
- If selected runtime is provisioning/remote → **GitHub must be connected** (`useGithubStatus`). Repos, skills, MCP all optional.
- Message text non-empty.

When blocked, the send button is disabled with a short reason (e.g. "Connect GitHub to use this runtime", "Select a model"). Locked/existing sessions gate `canSend` only on message text + session status, as today.

### First-message create-and-send flow

On first send from a draft:

1. Assemble `CreateSessionRequest` from draft: `agent: { model, use_plugins: true, mcp_servers }`, `vendor`, `repos: RepoConfig[]` (remote only), `plugins: string[]` (skills; remote only).
2. `POST /api/sessions` → `{ session.id }`.
3. `POST /api/sessions/:id/messages` with the text.
4. Navigate to `/sessions/:id`.

The existing provisioning-progress SSE (`Provisioning` status + `Progressed` stages: provisioning_runtime / scanning_workspace / connecting_tools / ready) drives the "starting up" feedback once on `/sessions/:id`.

Error handling: create succeeds but send fails → still navigate (empty resumable session). Create fails → surface the error inline, stay on the draft.

**`use_plugins` semantics note:** `CreateSessionRequest.plugins` is documented as "absent → server default-enabled bundles; non-empty implies plugins on." The design sends `use_plugins: true` (machinery on) plus `plugins` = selected bundle names. Confirm exact interplay in code during implementation so we don't regress the "no skills selected still gets operator defaults" behavior.

## Backend design (only backend work)

To render an existing session's config read-only, `SessionDetail` must echo the full config. Today (`models/fluorite/session.fl:40-52`) it carries `model`, `vendor`, `repos: Vec<String>` only. Add:

- `plugins: Vec<String>` — selected skill-bundle names.
- `mcp_servers: Vec<String>` — enabled MCP server names.
- `use_plugins: bool`.

Populate from the stored `SessionSpec` in the session-detail handler. Regenerate fluorite types into `clients/web/src/generated` and `clients/ts` (ts-drift CI). No new endpoints, no actor/lifecycle changes, no DB migration.

## Testing

- **Web e2e (Playwright, existing harness):** draft renders at `/`; send disabled until model set / GitHub-connected for remote; switching runtime local↔remote shows/hides repo/skill/MCP controls; first send creates a session and lands on `/sessions/:id`; existing session shows locked, disabled config controls reflecting `SessionDetail`.
- **Server:** extend a `session_server_e2e` assertion so `GET /api/sessions/:id` returns the new config fields.
- `make check` + web `typecheck` / `build` green.

## Out of scope

- Making model (or any control) editable on an existing session — deliberately structured to allow it later, but not implemented now.
- Any change to the backend session lifecycle / lazy-init (client-only draft makes it unnecessary).
- Rename UI, LLM-based titles (unchanged from PR #19).
