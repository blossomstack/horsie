# Server authentication: web UI, CLI, and vendor agents

Status: approved design, 2026-08-02. Ships as three sub-projects (A, B, C) off
one shared core, each with its own implementation plan and PR.

## Context

`horsie-server` has no authentication. `README.md` and `docs/guide/` say so
outright and tell operators to bind a trusted network or front the server with
their own proxy. That is tenable for a laptop deployment and untenable for the
two deployments we want next: a hosted SaaS, and standalone instances the user
pushes to a cloud provider such as fly.io.

Three surfaces are open today.

**Web UI ↔ server.** The SPA is served same-origin and calls `/api/*` REST plus
two SSE streams. Anything reachable on the port can read every session
transcript, mint GitHub tokens, and read or rewrite provider settings.

**CLI ↔ server.** `horsie session tail` streams `/api/sessions/:id/events`
over plain HTTP with no credential.

**Vendor agents ↔ server.** `GET /api/vendor/connect` performs a raw WebSocket
upgrade with no credential check at all. Whoever dials announces a vendor name
and is published into the map sessions select from. This is worse than merely
unauthenticated: `RuntimeVendorRegistry::register` does a bare `insert` keyed by
name, so a second connection announcing `local` silently *replaces* the live
one. An attacker who can reach the port takes over the vendor, receives tool
calls meant for someone's laptop, and is handed whatever credentials sessions
inject.

Existing material we build on: `jsonwebtoken` is already a dependency (the
plugin-artifact capability token), and `server/src/mcp/oauth.rs` implements
OAuth *client* mechanics for remote MCP servers — PKCE, dynamic registration,
refresh. The authorization-server routes in that file are test mocks. Issuing
tokens is new work.

## Goals

- Authenticate all three surfaces against one identity concept.
- Keep `docker compose up` a viable standalone install.
- Do not foreclose multi-tenancy, OIDC, or per-user data isolation later.
- Give the vendor surface a credential that is revocable per machine.

## Non-goals

Multiple users and roles; OIDC/SSO; per-user data isolation; TLS termination
(the proxy's job); replacing the plugin-artifact JWT; changing the MCP or
GitHub OAuth client flows.

## Locked decisions

1. **Single-tenant now, multi-tenant later.** No ownership columns on sessions,
   memories, settings, or model cards. Identity is a first-class principal with
   a stable id so isolation can be layered on without redesigning auth.
2. **Built-in accounts; horsie is its own token issuer.** No external IdP. The
   CLI and vendor agents get tokens from horsie's own endpoints, so adding OIDC
   later changes only how a human proves who they are — the CLI and vendor
   flows do not move.
3. **Exactly one admin account.** `auth_users` holds one row; the service
   refuses a second. Multi-user is additive later, not a retrofit.
4. **Auth is on by default**, disabled by explicit config.
5. **First boot generates the admin password** and prints it to the log.
6. **Opaque, server-stored tokens — not JWTs** — for principals. Revocation
   matters more here than statelessness, and every request already touches
   SQLite.
7. **The CLI uses the device authorization grant**, with a pasted token as the
   scripting escape hatch.
8. **One bearer check on `/api/vendor/connect`, two accepted token kinds** —
   a user access token or a minted agent token.

## Shared core

### Storage

Migration `0014_auth.sql`. No foreign keys, matching the rest of this schema.

```sql
CREATE TABLE auth_users (
    id            INTEGER PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,        -- argon2id PHC string
    -- 1 while the first-boot generated password is still in use
    password_is_generated INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

CREATE TABLE auth_tokens (
    id           TEXT PRIMARY KEY,      -- public uuid; safe to list and log
    kind         TEXT NOT NULL,         -- web | access | refresh | agent
    principal    TEXT NOT NULL,         -- user:<id> | agent:<token id>
    token_hash   BLOB NOT NULL UNIQUE,  -- SHA-256 of the presented secret
    label        TEXT,                  -- agent tokens: operator-chosen name
    chain_id     TEXT,                  -- access/refresh: rotation chain
    expires_at   INTEGER,               -- NULL = never (agent tokens)
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at   INTEGER
);
CREATE INDEX auth_tokens_hash_idx ON auth_tokens(token_hash);
CREATE INDEX auth_tokens_chain_idx ON auth_tokens(chain_id);

CREATE TABLE auth_device_codes (
    device_code_hash BLOB PRIMARY KEY,
    user_code        TEXT NOT NULL UNIQUE,
    principal        TEXT,              -- set on approval
    created_at       INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL,
    approved_at      INTEGER,
    denied_at        INTEGER,
    consumed_at      INTEGER,
    last_polled_at   INTEGER            -- drives slow_down
);
```

### Token format and verification

A secret is `hsk_<tag>_<43 url-safe base64 chars>` over 32 random bytes, where
`tag` is `web`, `usr` (access), `ref` (refresh), or `agt`. The prefix makes
tokens greppable and recognisable to secret scanners; the tag lets the server
reject a wrong-kind credential before touching the database.

Only `SHA-256(secret)` is stored. A plain hash — not argon2 — is correct here:
the input carries 256 bits of entropy, so there is nothing to brute-force, and
verification is on the hot path of every request. Passwords, which do not have
that property, use argon2id.

Verification is one path for every surface: read the credential, check the tag,
hash, look up by hash, reject if `revoked_at` is set or `expires_at` has
passed, and return a `Principal`. `last_used_at` is written back at most once a
minute per token so a busy SSE stream does not turn into a write per request.

### Request middleware

An axum layer wraps the `/api` router.

- Auth disabled → insert `Principal::Anonymous` and pass everything through.
- Auth enabled → take the credential from the `horsie_session` cookie or an
  `Authorization: Bearer` header, verify, and insert the resulting `Principal`
  into request extensions. Failure is `401` with a JSON body.
- Unauthenticated allowlist: `/api/health`, `/api/auth/status`,
  `/api/auth/login`, `/api/auth/device/code`, `/api/auth/device/token`,
  `/api/auth/refresh`, and `/api/plugin-artifacts/*` (guarded by its own
  capability JWT).
- Non-`/api` paths — the SPA shell and its assets — are never guarded. The app
  must load in order to render a login page, and the bundle holds no secrets.

The browser authenticates by cookie because it has no choice: both streams use
the native `EventSource`, which cannot set headers. The CLI uses
`reqwest_eventsource` and sends a bearer header.

### Configuration

```json
{ "auth": { "enabled": true } }
```

in `BootConfig`, defaulting to `true`, with `HORSIE_AUTH_ENABLED=false`
overriding the file. Disabled means the middleware passes everything,
`/api/auth/status` reports `enabled: false`, the UI hides all login surface,
the CLI sends no token, and `/api/vendor/connect` skips its check — that is,
exactly today's behaviour.

### First boot

On startup with auth enabled and `auth_users` empty, the server generates a
24-character password, stores its argon2id hash as user `admin`, and:

- logs a boxed warning containing the password, and
- writes it to `<state_dir>/initial-admin-password` with mode 0600.

The file is deleted when the password is next changed. Without it, an operator
who has rotated their container logs is locked out of their own deployment with
no recovery path short of editing SQLite.

## Sub-project A — web UI ↔ server

Endpoints, all under `/api/auth`:

| Route | Auth | Behaviour |
| --- | --- | --- |
| `GET /status` | none | `{ enabled, authenticated, mustChangePassword }`. `mustChangePassword` tracks `auth_users.password_is_generated` and is only ever true for an authenticated caller — telling an anonymous one that a deployment still has its first-boot password just tells an attacker where to aim |
| `POST /login` | none | `{ password }` → sets cookie, returns status |
| `POST /logout` | cookie | revokes the `web` token, clears the cookie |
| `POST /password` | cookie | `{ currentPassword, newPassword }`, revokes all `web` tokens except the caller's |

The cookie is `horsie_session`: httpOnly, `SameSite=Lax`, `Path=/`, 30-day
max-age, and `Secure` when the request arrived over TLS (direct scheme or
`X-Forwarded-Proto`). `SameSite=Lax` is the CSRF defence — it blocks
cross-site POSTs outright, and cross-site GETs cannot mutate. Bearer-carrying
requests are immune by construction, since a cross-origin page cannot set the
header.

Failed logins get per-IP backoff held in memory: after 3 failures, the next
attempt is refused with `429` and a `Retry-After`, delay doubling from one
second to a thirty-second ceiling. The generated password is strong; the one
the operator replaces it with will not be.

Web UI: a login page shown whenever `/api/auth/status` reports enabled and
unauthenticated, a logout control in the existing settings navigation, and a
password-change form. A `401` from any API call drops the app back to the login
page.

## Sub-project B — CLI ↔ server

### Device flow

| Route | Auth | Behaviour |
| --- | --- | --- |
| `POST /api/auth/device/code` | none | → `{ deviceCode, userCode, verificationUri, verificationUriComplete, expiresIn: 600, interval: 5 }` |
| `POST /api/auth/device/token` | none | `{ deviceCode }` → token pair, or a pending/`slow_down`/expired/denied error |
| `POST /api/auth/device/approve` | cookie | `{ userCode }` |
| `POST /api/auth/device/deny` | cookie | `{ userCode }` |
| `POST /api/auth/refresh` | none | `{ refreshToken }` → new pair |

`userCode` is `XXXX-XXXX` over an alphabet with `0`, `O`, `1`, and `I`
removed — roughly 36 bits, which a ten-minute expiry and a five-second poll
floor make untargetable. Polling faster than `interval` returns `slow_down`
and does not reset the timer.

This is the device grant's *shape*, not RFC 8628 on the wire: request and
response bodies are fluorite-generated JSON, not form-encoded OAuth, and there
is no discovery document or client registration. Only our own CLI talks to
these endpoints, and pretending to be a general authorization server would buy
interoperability nobody is asking for. Error codes reuse the RFC's names
(`authorization_pending`, `slow_down`, `expired_token`, `access_denied`)
because they are already the right words.

Access tokens live one hour; refresh tokens ninety days and rotate on every
use. Presenting an already-rotated refresh token revokes its whole `chain_id` —
the standard reuse-detection response, and the only signal available that a
credential file was copied.

The SPA gains an `/auth/device` route where a logged-in admin approves or
denies a code, reachable pre-filled through `verificationUriComplete`.

### CLI surface

`horsie auth login --server <url>` prints the code and URL, polls, and stores
the pair. `--token <t>` skips the flow and validates a pasted token instead.
`horsie auth logout [--server <url>]` revokes server-side and forgets locally.
`horsie auth status` lists configured servers and whether each credential is
live.

Credentials live in `~/.config/horsie/credentials.json`, mode 0600, keyed by
normalised server URL, holding the access token, refresh token, and access
expiry. `HORSIE_TOKEN` overrides the file for the targeted server.

`horsie session tail` and `horsie connect` attach the bearer, refresh once on a
`401`, and on failure tell the user to run `horsie auth login --server …`.

## Sub-project C — vendor agents ↔ server

`/api/vendor/connect` requires `Authorization: Bearer` before the upgrade and
responds `401` — not `101` — when it is missing or bad. Accepted kinds are
`access` and `agent` only; a `web` or `refresh` token is rejected even though
it verifies, because neither belongs on a machine link.

`RuntimeVendorLink` records the owning principal, and
`RuntimeVendorRegistry::register` stops overwriting blindly:

- the same principal reconnecting under an existing name replaces the link, so
  a dropped network connection still recovers;
- a *different* principal claiming a live name is rejected and logged.

A known limitation, stated rather than hidden: two machines that both run
`horsie connect` under the same human's login share principal `user:<id>`, so
the second still displaces the first. The fix is to give each machine its own
agent token, which this sub-project makes possible; enforcing it is not
worthwhile while there is exactly one account.

Agent tokens get a Settings page: create with a label (secret shown once and
never again), list id/label/created/last-used, revoke. `horsie connect` uses
stored login credentials by default. `velos-runtime` takes `--token`, and
prefers `HORSIE_TOKEN` from the environment — matching the guidance already
given for `HORSIE_VELOS_TOKEN`, so the secret stays out of process listings.

## Wire types

A new `models/fluorite/auth.fl` (package `auth`) carries `AuthStatus`,
`LoginRequest`, `PasswordChangeRequest`, `DeviceCodeResponse`,
`DeviceTokenRequest`, `DeviceTokenResponse`, `DeviceApprovalRequest`,
`RefreshRequest`, `TokenView`, `TokenCreateInput`, and `TokenCreated`.
TypeScript generation into `clients/web/src/generated` and `clients/ts` follows
the existing convention, and the CI drift job covers it without changes.

## Deferred

**D — session-scoped short-lived tokens.** Fold the plugin-artifact capability
JWT together with fetch-on-demand runtime credentials (#96) and the artifact
base-URL bug (#99) into one per-session capability minted at provisioning. That
token authorizes a *session*, not a principal, so it is genuinely a different
mechanism and does not belong in this design.

**E — multi-tenancy.** Ownership columns, per-user isolation, roles, and OIDC.
This design's contribution is only that principals have stable ids and every
handler can reach one.

## Migration and operational impact

Turning auth on by default is a breaking change for existing deployments,
including the homelab GitOps deploy: after upgrade it needs the generated
password from the container log or state directory, and the `ops` repo will
want that password stored where the deploy can reach it. That is a follow-up in
`ops`, not something this change can do for itself.

Documentation changes land with the sub-project that makes them true: the "no
built-in authentication" warnings in `README.md`, `docs/guide/README.md`, and
`docs/guide/self-hosting.md` are replaced in A; `docs/guide/getting-started.md`
gains the login step in B; `docs/guide/runtime-vendors.md` gains agent tokens
in C.

## Testing

Every existing HTTP and e2e test constructs unauthenticated requests. They keep
running with auth disabled — which is a real configuration, not a test-only
escape — and authenticated coverage is added alongside rather than by rewriting
them. This is a deliberate choice, recorded so the coverage shape is not
mistaken for something quietly dropped.

- **Unit**: token generation, tag parsing, hashing and verification, expiry and
  revocation; argon2 round trip; `userCode` alphabet and shape; backoff
  arithmetic; credential-file round trip and permissions.
- **Server HTTP** (the `server/src/http/mod.rs` harness, with an
  auth-enabled variant of `test_state`): unauthenticated `/api/sessions` is
  `401` while `/api/health` is `200`; login sets a cookie and the cookie then
  works; logout revokes it; a bearer access token works; expired and revoked
  tokens are rejected; login backoff triggers; device flow happy path, pending,
  `slow_down`, expiry, and denial; refresh rotation, and reuse revoking the
  chain.
- **Vendor**: connect without a token fails before the upgrade; a `web` token
  is rejected; an agent token connects and is published; a second principal
  claiming a live name is rejected while the same principal reconnecting
  replaces it.
- **CLI**: a `401` triggers exactly one refresh and retry; a failed refresh
  produces the "run horsie auth login" error.
- **Web e2e** (the existing Playwright harness): a login spec against an
  auth-enabled server; the rest of the suite runs auth-disabled.
