# QA sweep 2026-08-09: the five blockers and the five cross-cutting causes

Ten fixes from a full-surface browser test of `sha-e1b5d71`. Ten independent
PRs off `main`; nothing here stacks.

That test's own summary is worth repeating, because it sets the shape of the
work: *"horsie's engine is in good shape and its edges are not. Almost every
defect is in the last few centimetres: a server that knows exactly what went
wrong and a client that drops the message, or a validation nobody wrote."*

Two of the ten are ordinary bugs with a known one-line cause (H1, X5). Six are
small, well-understood, and touch two or three files each. One is a security
boundary that was never drawn (H5). One — H3 — has **no root cause yet**, and
this spec says so rather than pretending otherwise.

## Decisions taken before writing this

Three of the ten carried a genuine product choice. They were settled first,
because each one changes what the code has to do.

### X2 does not filter addresses

The sweep demonstrated horsie fetching a LAN service the browser could not
reach and reflecting its body — `INTERNAL-ONLY-SECRET: db_password=…` — back
into the API, the UI and the database. `169.254.169.254` was accepted and
dialled.

The obvious fix is to deny loopback, link-local and RFC1918. **We are not doing
that.** Those addresses are how self-hosted horsie reaches a local model server:
`http://localhost:11434/v1` is Ollama's documented base URL, and a LAN MCP
server is an ordinary setup. Blocking them is a real regression for the primary
deployment shape in exchange for closing a hole that today requires an
authenticated admin who already owns the box.

What we do instead is remove the two things that make it *useful* to an
attacker and the one thing that makes it dangerous beyond SSRF:

- never reflect a fetched response body back to the caller;
- allowlist schemes, so `file://`, `ext::` and a leading `-` cannot reach git.

The address question is worth revisiting if these ever become fields that a
less-privileged party can set, because the blast radius changes completely at
that point. It is not the right answer today, when setting one already requires
an administrator.

### H5 constrains only the machine token

There are four token kinds. `Web` is the browser cookie, `Access` and `Refresh`
are the CLI login, and `Agent` is the "machine token" the UI describes as *"for
runtime vendor processes that run unattended"*.

Only `Agent` changes. The login credentials stay full-privilege — that is what
a login is.

`vendor_connect.rs:1` states the constraint outright: `/api/vendor/connect` is
*"the one endpoint runtime vendor processes dial."* So the token's real job is
one route, and today it reaches all of them.

No expiry field. Once a token reaches a single route and cannot mint
successors, revocation is a sufficient control, and an expiry the operator must
remember to renew is one more way to take a runtime fleet down at 3am.

### X3 refuses nothing

Four edges break when the thing they point at is renamed or deleted. The
tempting fix is a `409` on every one, matching the `agent_in_use` check that
already exists for routines.

We are not adding refusals. Sessions are numerous and never cleaned up; a rule
that says "you cannot rename this model alias because forty archived sessions
mention it" trades a rare silent failure for a constant loud one.

Instead every dangling reference becomes **legible and repairable**. Note that
rewriting a reference is not refusing — where a rename can simply fix its own
pointers, it should.

The existing routine→agent `409` stays. Removing a shipped safety check is its
own regression and is out of scope here.

## The ten

### H1 · Fly: every first turn of every new session fails

`412 failed_precondition: unable to start machine from current state:
'created'`, deterministic 3/3.

A Fly machine sits in `created` for about six seconds before `started`.
`fly_api.rs::parse_state` has four arms — `started`, `stopped`, `suspended`,
and `_ => Other`. `fly.rs::get` then groups `Other` with `Stopped` and
`Suspended` and calls `start` on it. Fly rejects starting a machine that is
already booting.

`MachineState`'s own doc comment says `Other` is *"treated as not usable yet,
never as gone"*, and `get` does not honour it.

**Fix.** *(As shipped: no new variant.)* `get` is the only place that matches
on `MachineState`, and `parse_state` already maps `created`/`starting`/
`replacing` to `Other` — so the whole fix is one match arm. `Stopped` and
`Suspended` start, because they are the only two states Fly starts from and
exactly the two a hibernate leaves behind; `Other` waits, honouring what its
doc comment always claimed. The waiter is already bounded, so waiting is the
safe default and starting is not. A `Transitional` variant was the first plan
and was dropped: both arms would have behaved identically, so it was a
distinction with no consequence.

**Tests.** Unit tests on `parse_state` for the new strings, and a `FakeFly`
test asserting `get` on a `created` machine issues no `start` call.

### H2 · GitHub OAuth cannot complete on any HTTPS deployment

`request_base` (`http/mod.rs:39-45`) builds `format!("http://{host}")` from the
`Host` header. Behind any TLS terminator the `redirect_uri` therefore goes out
as `http://` and GitHub rejects the mismatch. MCP OAuth builds its redirect the
same way, so both are one fix.

The escape hatch, `callback_base`, has no field in the UI and is wiped by every
Admin save, because `GithubAppPage.tsx` submits only
`clientId`/`clientSecret`/`appId`/`privateKey` against a full-replacement PUT.
Setting it by API made the flow work; one Admin save re-broke it. That second
half is X4's `callback_base` bullet, folded in here because it is the same file
and the same bug.

**Fix.** Honour `X-Forwarded-Proto` when present, falling back to the
connection scheme. Give `callback_base` a real field in `GithubAppPage.tsx` and
include it in the submitted body.

**Tests.** Unit tests on `request_base` for present/absent/garbage
`X-Forwarded-Proto`. A Vitest assertion that the GitHub App form round-trips
`callback_base` rather than dropping it.

**Verification caveat.** The forwarded-proto path cannot be fully proven by
unit test; confirming it end to end needs a real HTTPS deployment.

### H3 · Skills never reach any runtime — investigation, not a known fix

`HORSIE_PLUGINS_DIR` is created and left empty. The `skill` tool reports
`unknown skill '' … available:` with nothing listed. Found independently by two
testers, and environment-wide: a control session with the bundle picked
directly behaves identically.

`plugins_fetch::materialize` fails at `req.send()` with a **transport** error
rather than a status — a 403 would have printed `HTTP 403`. From inside the
same sandbox, `curl` on the identical URL reports `http=403 dns=0.0037
connect=0.0050`: DNS, TCP and TLS all fine.

Reading the source adds one fact the sweep did not have. `materialize` reports
`e.to_string()`, and `reqwest::Error`'s `Display` prints `error sending request
for url (…)` while **dropping its source chain** — which is where "invalid peer
certificate" or "dns error" would have been. Nobody knows the cause because the
cause was never printed.

**Prior.** `provision_into` builds a bare `reqwest::Client`, and the workspace
pins reqwest's `default-tls`, i.e. **native-tls** (`Cargo.toml:20`). native-tls
reads the OS trust store; `curl` carries its own CA bundle and would not notice
losing access to it. A confined process that cannot reach the trust store fails
exactly this way — transport, not status.

**Approach, in order.**

1. Print the full source chain in `materialize`. This lands regardless of what
   the cause turns out to be: an unprintable error is its own defect.
2. Confirm `ENV_CONNECT_TOKEN` is actually populated. If it is absent the fetch
   would 403 — a *different* bug currently masked by this one.
3. Run the fetch with the sandbox off and compare.
4. If the prior holds, build this one client with `.use_rustls_tls()` and
   bundled roots.

**Timebox.** If the cause is not the trust store, land step 1 and report back
rather than sinking the session into it.

### H4 · A large streamed reply hard-locks the browser tab, permanently

100% CPU, never recovers, survives the turn being stopped server-side, needs
`kill -9` on the renderer. `sample(1)` showed 2219/2219 samples on the main
thread in one unbroken JIT stack.

`Markdown.tsx:15` passes `rehypeHighlight` `{ detect: true }` — highlight.js
auto-detection, which runs every registered grammar over every code block. The
component is memoised on `text`, so every streamed token re-runs the whole
pipeline over the whole message. Measured against the repo's own highlight.js:
a 3000-char unbroken line takes 448 ms, 5000 chars takes 1230 ms
(super-linear), and 60 growing prefixes cost 9.3 s of pure CPU. Real streaming
issues hundreds of updates, not 60.

**Fix.** Three independent mitigations, all of them cheap:

- drop `detect`, so only fenced blocks with an explicit language highlight;
- skip highlighting entirely while the segment is streaming, and highlight once
  when it completes;
- bail above a size threshold.

**Tests.** *(As shipped: unit tests only.)* Unit tests pin all three
conditions, with the positive case probing the same selector as the negative
ones so none can pass vacuously.

The Playwright case this called for was written and then **deleted**: checked
against a deliberately restored `detect: true` build, it passed in 1.0s. The
mock streams fast enough that React batches the chunks into a handful of
commits, and a single highlight pass over even a 15k-char block is sub-second —
the pathological cost is in the *repeated* re-render, which needs real network
pacing. A test named "does not lock the tab" that passes while the tab-locking
code is present is worse than no test.

### H5 · Machine tokens are unrestricted, never-expiring admin credentials

One machine token authenticates against `/api/sessions` including full
transcripts, `/api/agents`, `/api/environments`, `/api/routines`,
`/api/workflows`, `/api/config` and `/api/runtime-vendors` — all 200, writes
reaching handlers. It can call `POST /api/auth/password`. And it can mint
further machine tokens, so revoking a leaked one does not lock the holder out.

`require_auth` (`http/auth.rs:100-104`) inserts the principal for *any* verified
token kind, with no `TokenKind` check anywhere after it.

**Fix.** A kind check in the authenticated path: a `TokenKind::Agent`
credential authorizes `/api/vendor/connect` and returns `403` everywhere else,
minting included. `Web`, `Access` and `Refresh` are untouched.

**Tests.** Rust tests asserting an Agent token connects a vendor, is refused on
a representative read (`/api/sessions`), a representative write, and on
`POST /api/auth/agent-tokens`. The existing
`an_agent_token_connects_and_becomes_a_selectable_vendor` and
`a web token must not open a machine link` tests already pin the other
direction and must keep passing.

### X1 · The client drops server errors at about eight sites

Two mechanisms.

Axum body-rejections return `text/plain`. `client.ts:124` sets `message` to
`` `${status} ${statusText}` ``, then calls `res.json()`, which throws on
non-JSON and leaves the status line in place — and `statusText` is empty over
HTTP/2. So the user sees a bare `422 ` while the real message was
`provision[0]: missing field 'name' at line 1 column 63`.

Separately, several mutations call `.mutate()` with no `onError`, so even a
well-formed `{code,message}` vanishes. `409 agent_in_use` *names the blocking
routine* and the user sees only a row that does not disappear.

Sites: group create/rename, session delete, cloud-vendor save, environment
provision JSON, routine schedule, agent delete, MCP delete, machine-token
create/revoke. On the cloud-vendor page the banner additionally renders
off-screen — banner top at −389 px with SAVE at +542 px — so SAVE appears to do
nothing at all.

**Fix.** Read the body as text once, attempt `JSON.parse`, fall back to the
text itself when it is non-empty. Add `onError` at the listed sites. Scroll the
banner into view, or move it adjacent to the action.

**Tests.** Vitest over the error path: `{code,message}` JSON, a `text/plain`
rejection, an empty body, and an empty `statusText`.

### X2 · Every user-supplied URL the server dereferences is unvalidated

Three sites: provider base URL, git clone URL, MCP server URL. No `Url::parse`,
no scheme allowlist. `mcp/store.rs:120-128` is literally two `is_empty()`
checks. Body reflection is `support/src/mcp/transport.rs:168`.

Per the decision above, no address filtering.

**Fix.**

- `transport.rs` stops putting a fetched body into the error it returns. The
  body is logged; the caller gets the status and a generic transport failure.
- `Url::parse` plus an `http`/`https` allowlist at all three sites.
- The git path additionally rejects a URL beginning with `-` (git reads it as
  an option) and the `ext::` scheme, which the allowlist covers.

*(As shipped: `file://` is allowed for git.)* Rejecting it was wrong. Cloning a
repo on the server's own disk is a legitimate self-host setup and is how this
repo's entire plugin suite builds its fixtures — the strict version broke 20
tests. `ssh` and `git` stay rejected: horsie manages no SSH identity, so an
`ssh://` remote would borrow whatever key the server process happens to hold.

**Tests.** Rust unit tests per site: accepted `https`, accepted
`http://localhost:11434/v1` — explicitly, so nobody "hardens" it later without
reading this spec — rejected `file://`, rejected `ext::`, rejected `-upload-pack=…`.
A test asserting a failing MCP fetch does not carry the target's body.

### X3 · Renaming or deleting X silently breaks everything pointing at X

| edge | today | fix |
|---|---|---|
| workflow step → its own transitions | rename does not rewrite them; save fails naming a step absent from the form | rewrite the transitions — one object, one save |
| model alias → sessions | next turn fails `no provider registered for model '…'`, and the picker is `mode="locked"`, so the session is permanently unusable through the UI | *(As shipped: legible, not repairable.)* the key reads `… — missing` and says the next turn will fail. Repointing was dropped: no API exists to change a live session's model, and adding one is a feature — route, actor command, journal event — not part of making a failure visible. Worth a follow-up. |
| memory space → sessions | `memory_list` returns `{"memories":[]}`, indistinguishable from "no memories" | say the space is gone |
| agent preset → workflows | `delete_agent` consults only `routines.using_agent`; DELETE returns 204 and it fails at run time | name the missing agent in the workflow editor and at run time |

**Tests.** A Rust test that renaming a workflow step carries its transitions. A
Rust test that `memory_list` against a deleted space is an error, not an empty
list. A Vitest assertion that a session whose model is absent renders an
editable picker.

### X4 · Full-replacement PUTs plus partial forms silently delete data

Editing a model card wipes `thinkingEfforts`, `defaultThinkingEffort` and
`thinkingDialect` — three of eight fields, and precisely the ones that make a
card worth more than a token-count lookup. The editor cannot display them, so
the loss is unrecoverable in-product, and `seed_if_missing` never repairs an
existing row. One operator bumping Opus 4.8's max tokens permanently strips its
thinking config.

This is the same defect as B32 ("the editor cannot set thinking efforts"); they
are one fix, not two.

The `callback_base` half of X4 is handled in H2.

**Fix.** Add the three fields to the model-card editor, backed by the canonical
effort and dialect sets already encoded in the repo's own tests.

**Tests.** A Vitest assertion that loading a card with thinking config, editing
an unrelated field and saving round-trips all eight fields.

### X5 · Unmatched routes render a blank white page

Confirmed still absent on `main`: no `path="*"` in `App.tsx`. Zero DOM, not
even the sidebar; the only escape is the URL bar. Reached at `/admin/github` (a
plausible typo for `/admin/github-app`), `/nonsense`, `/device`,
`/agents/<name>`, `/settings/memory/<id>`, `/admin/model-cards/<id>`.

The contrast is instructive: `/agents/x/edit` gives a proper `No such agent: x.`

Related on the API side, unknown `/api/*` paths return `200 text/html` — the
SPA shell — so a consumer checking status codes parses an HTML document as
success. That is B43, and it is the same missing-fallback shape, so it lands
here.

**Fix.** A `path="*"` catch-all rendering a real not-found inside the normal
chrome. A `/api/*` fallback returning `404` JSON ahead of the SPA `ServeDir`.

**Tests.** A Playwright case for an unmatched route rendering navigation rather
than nothing. A Rust test that `GET /api/nope` is `404` with a JSON body.

## Out of scope

The 44 `B*` findings, except the three that are indivisible from work above
(B32 with X4, B43 with X5, the `callback_base` bullet with H2). Cleaning up
after the test run — leftover machines, tokens and deliberately-broken
fixtures — is deployment work rather than code, and is tracked separately.

## Verification

`-p horsie-server` is a false green for anything touching routes: the integration tests and the web e2e suite call them
too. Each PR runs its own crate's tests while iterating, and the full workspace
suite once before pushing.
