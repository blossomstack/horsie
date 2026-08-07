# Delegated authentication — design

Written 2026-08-07. Replaces work item 9 of
`2026-08-04-per-user-scoping-design.md` ("Extension points") and closes #231,
whose three proposed additions — `routes()`, `UserResolver`, `UsagePolicy` — are
rejected below.

## Why

horsie authenticates callers itself. It owns a password, a browser cookie, a
device-code flow for the CLI, and machine tokens. That is the right shape when
horsie is the only thing serving the port.

It is the wrong shape for a deployment that already has an identity system in
front of it — an SSO gateway, an identity-aware proxy, an operator's own
service. There the account exists, and has been authenticated, before horsie is
asked anything. Such a deployment has two options today and both are bad:

- **Authentication on.** horsie insists on issuing a credential of its own, so
  everyone signs in twice, and the identity the front layer already established
  is discarded.
- **Authentication off.** Every request becomes `Principal::Anonymous` and
  resolves to one account (`server/src/http/mod.rs:103`). Ten authenticated
  people share one workspace.

The second is the notable one, because it throws away work the server has
already done. Since per-user scoping landed, every durable row and every
in-memory service belongs to exactly one account. A deployment that knows who
its callers are still cannot use any of it.

**What is missing is the seam, not the scoping.** `UserRegistry::get()`
(`server/src/users.rs:255`) builds an account's entire world — supervisor,
stores, journal, event channel, vendor map — from a `UserId` and nothing else.
It never consults `auth_users`; nothing in the scoped tier does. The auth tables
are touched only by the auth store itself. So horsie can already serve any
account id it is handed. It just has no way to be handed one.

## The rule

> **When authentication is delegated, horsie takes the caller's account from the
> identity the front layer supplies, issues no credentials of its own, and
> refuses any request that arrives without one.**

Four consequences, and the third is the whole reason this works.

### 1. The identity arrives on the request, as an extension

`require_auth` (`server/src/http/auth.rs:47`) gains a third branch beside
"verify a horsie credential" and "authentication is off". It reads what the
surrounding middleware put in the request extensions and inserts the
corresponding `Principal`.

An extension, not a header. A trusted header like `X-Forwarded-User` is
attacker-supplied input unless every deployment remembers to strip it at the
edge, and forgetting once lets any caller name themselves anyone. An extension
can only have been set by code in the same process — the deployment's own
middleware, which is what we are asking it to write anyway.

The identity carries **only who**. Not a role, not a credential kind, not an
entitlement. horsie has no role model to enforce, and inventing one here would
commit every deployment to ours.

### 2. A missing identity is `401`, never anonymous

If a request reaches the router in this mode with no identity attached, the
answer is `401` — never a fall back to the anonymous account.

This is the one mistake in this design that would not announce itself. Falling
back would mean a single mis-ordered layer quietly serves every caller the same
workspace, while every request succeeds and every page renders. It has to be a
test, not a comment.

### 3. In this mode horsie mounts no credential routes at all

The whole `/api/auth/*` family — login, logout, password, refresh, the
device-code flow, machine tokens — belongs to the front layer.

Not mounted, rather than mounted-and-404. axum panics when two merged routers
claim the same path, so leaving these unmounted is precisely what lets the front
layer serve those paths itself, under its own identity model. That is what keeps
the rest of the product working unchanged:

| Path | Default | Delegated |
| --- | --- | --- |
| `/api/auth/login`, `/logout`, `/password` | horsie | front layer |
| `/api/auth/device/code`, `/token`, `/approve`, `/deny` | horsie | front layer |
| `/api/auth/refresh`, `/api/auth/tokens` | horsie | front layer |
| `/api/auth/status` | horsie | front layer |
| everything else | horsie | horsie |

`horsie auth login` posts to `/api/auth/device/code` and `/api/auth/device/token`
on whatever server URL it was given (`cli/src/auth.rs:301`), and the approval
page is the web UI's own `/auth/device` route. Neither cares who implements
those paths. So a front layer that serves them gets an unmodified CLI and an
unmodified browser flow.

A front layer that does so must match one contract exactly: the poll error codes
`authorization_pending`, `slow_down`, `access_denied` and `expired_token`
(`cli/src/auth.rs:220`). Anything else is read as "keep polling", so a
misspelled denial becomes an infinite loop rather than an error.

### 4. `/api/vendor/connect` follows the same identity

The vendor dial authenticates separately from the middleware, verifying its own
bearer (`server/src/http/vendor_connect.rs:42`). In delegated mode it reads the
same injected identity instead.

Its current rule — only `access` and `agent` kinds may drive a runtime link,
never a browser cookie — does not survive into this mode, and should not. Only
whoever issues credentials knows what kinds exist. A front layer that
distinguishes a browser session from a machine credential enforces that itself,
on the path, where it has the information.

## The web UI

The SPA offers a login page and a change-password form. Both are meaningless
when identity lives elsewhere, and the password form is worse than meaningless:
it points at an account horsie is no longer the authority for.

`AuthStatus` gains two fields — that identity is managed externally, and where to
send someone who is not signed in. In delegated mode the front layer serves
`/api/auth/status`, so it fills them. The SPA hides the password form and
redirects instead of rendering its own login page.

This is the only wire change in the design, and the only reason the web client
needs touching at all.

## Composition

`app(state)` stays the single entry point. A deployment wraps it:

```rust
app(state).layer(my_auth).merge(my_routes)
```

The layer encloses horsie's routes; the front layer's own routes are merged
outside it, which is what a login endpoint needs. No `routes()` function is
required for this and none is added.

What is missing is one level down. `boot()` lives in the binary
(`server/src/bin/horsie-server/main.rs:90`), so a second binary embedding the
server copies about 120 lines — directories, the pool, bootstrap, the model-card
seed, the artifact secret, the routine scheduler — and then silently drifts from
it at the first change. It moves into the library as a documented builder, and
the binary keeps argument parsing and `serve`.

## What does not change

- **The default deployment.** Password login, the device flow and machine tokens
  are still what horsie does out of the box, unchanged. Authentication off still
  means one anonymous account.
- **Account provisioning.** `AuthStore::create_user` stays what it is:
  bootstrap-only, and it refuses a second account
  (`server/src/auth/store.rs:128`). A delegated deployment writes no rows to
  `auth_users` at all — the id comes from the front layer and the scoped tier
  needs nothing else. horsie ships no account-management surface, and this
  design deliberately does not add the beginnings of one.
- **Deleting an account.** Still out of scope, for the reasons the scoping
  design gives: no `purge_user`, no cascades, and `PRAGMA foreign_keys` is never
  enabled, so a declared one would be silently ignored on SQLite.
- **The wire protocol**, apart from the two `AuthStatus` fields.

## Risks

- **Silent collapse to one account** if the front layer's middleware is missing
  or ordered wrongly. Answered by `401` and a test that asserts it, but it is
  the failure worth watching for in review.
- **The ids come from outside.** horsie stores whatever it is given; `UserId` is
  TEXT. A front layer that recycles an id hands one person another person's
  workspace, and horsie cannot tell. The id must be stable for the life of the
  account and never reused — documented as a requirement of the mode.
- **`auth_users` goes unused in this mode.** True today, and safe today, because
  only the auth store reads it. Anything that later joins a scoped table against
  it would break exactly here, where nobody is looking.
- **Two generated client trees.** An `AuthStatus` change has to be regenerated
  into both `clients/web` and `clients/ts`; CI drift-checks only the latter.

## Rejected alternatives

**`trait UserResolver` on `AppState`** (the #231 proposal). A trait whose one
method turns a request into a principal is middleware with extra steps — and
middleware is a thing axum already has, that a deployment already knows how to
write, and that composes with everything else it needs to do on the same path.
The trait would also have to carry a bundled implementation for the behaviour
horsie itself ships, which is the branch this design writes in ten lines.

**`trait UsagePolicy`** (also #231). Admission control at session create and
turn start, for deployments bounding what an account may consume. A layer in
front of `POST /api/sessions` does the same thing today with no seam at all. The
one limit that genuinely cannot be enforced from outside is a mid-run budget,
and if that is ever wanted the right shape is a plain per-account limit horsie
enforces for everyone — a feature, not an extension point. Nothing is added
here.

**`routes(state) -> Router` alongside `app(state)`** (also #231). Proposed so a
second binary could merge its own routes and wrap its own middleware. It can
already: `AppState` and every constructor are public, and `layer` plus `merge`
compose in the right order. The premise that it was needed was wrong.

**A trusted header instead of an extension.** Works with any proxy and needs no
Rust. Rejected as the primary mechanism because it is only safe when every
deployment strips the header at every edge, and the failure is total
impersonation. A deployment that wants it can write the four-line middleware
that reads its header and sets the extension, having made that decision
explicitly.

**Authentication off, plus a per-request account selector.** Simplest possible
change and an open door: the selector is the credential, and there is nothing
behind it.

## Work breakdown

1. Replace `auth.enabled` with `auth.mode` — `password`, `delegated`, `off`. The
   boolean cannot express three states, and no compatibility shim: the config
   shape changes.
2. The third branch in `require_auth`, plus the `401`-not-anonymous test.
3. Mount the `/api/auth/*` group only outside delegated mode.
4. `vendor_connect` reads the injected identity in delegated mode.
5. `AuthStatus` fields, regenerated into both client trees; SPA hides the
   password form and redirects when unauthenticated.
6. `boot()` into the library as a builder; the binary keeps CLI parsing and
   `serve`.
7. An operator guide: what the mode guarantees, what the front layer owes it
   (stable ids, never reused; the device-flow contract if it serves one).

Items 1–4 are the mode. 5 makes it usable from a browser. 6 is what makes it
usable from another binary at all, and is the only item that touches code no
part of the mode strictly needs.
