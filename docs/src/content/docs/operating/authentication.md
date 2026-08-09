---
title: Authentication & accounts
description: The generated first password, machine tokens, and the three authentication modes.
kind: how-to
sidebar:
  order: 5
---

Authentication is **on by default**. A deployment reachable from anywhere but
localhost should not be open by accident.

## First boot

The server creates an `admin` account and generates a password for it. The
password is printed once, and written to `initial-admin-password` in the
server's state directory — so a rotated log is not a lockout.

```bash
docker compose -f docker/docker-compose.yml logs horsie | grep -A4 'admin account'
```

Change it under **Settings → Account**, which deletes that file. The account
page shows a notice for as long as the deployment is still on its generated
password.

## Machine tokens

**Settings → Account → Machine tokens** mints a bearer token for something with
nobody to approve a login: a CI job, a webhook receiver, a `horsie connect`
running unattended in a container.

The secret is shown once, and only its hash is stored — there is nothing to
recover if you lose it. Revoke it from the same page.

Pass it as `HORSIE_TOKEN` to the CLI, or as an `Authorization: Bearer` header
to the API.

## Signing in from the CLI

`horsie auth login` runs a device-approval flow: it prints a URL and a code,
you open the URL and confirm the code matches. Credentials are stored in
`~/.config/horsie/credentials.json`, readable only by you, and refresh
themselves as they age. See [Install & sign in](/cli/install-and-sign-in/).

## The three modes

Set with `HORSIE_AUTH_MODE`, or `auth.mode` in `config.json`. The environment
variable wins, and an unrecognised value falls through to the file rather than
picking a default — the two wrong guesses here are "open to everyone" and
"trusts a header nobody is setting".

### `password` — the default

horsie owns the accounts. Sessions in a browser, device approval for the CLI,
machine tokens for everything else.

### `off`

Anything that can reach the port has full access, and every caller shares one
account. For a trusted network only.

### `delegated`

For a deployment that already has SSO, an identity-aware proxy, or its own
service in front. In this mode:

- horsie serves **no credential routes at all**. Nothing under `/api/auth/` or
  `/api/device/` exists, so your layer is free to serve those paths itself. A
  browser and the CLI both keep working against whatever it puts there.
- Every request must arrive with an identity attached, as a
  `horsie_server::http::auth::DelegatedIdentity` extension set by your own axum
  middleware wrapping `app(state)`. A request without one is answered `401` —
  never the shared account, which would silently serve every caller the same
  data.
- The account id is yours to choose, and horsie stores it as given. It must be
  stable for the life of the account and **never reused**: recycling one hands
  a person somebody else's workspace, and nothing here can detect that.
- No account rows are created. `auth_users` stays empty; horsie resolves an
  account from the id alone.

This mode is for embedding the server as a library — `horsie_server::boot::boot`
builds everything and `http::app` gives you the router to wrap. It is not
something the stock binary can be pointed at a proxy and told to trust.

## What an account scopes

Sessions, agents, environments, routines, workflows, runtime vendors, skill
bundles, MCP servers and memory are all scoped to the account that created
them. One account cannot read or reach another's, and the server enforces it
rather than the UI hiding it.

Where accounts come from is deliberately not decided here, because it varies
more between deployments than anything else in the server: one operator wants
OIDC, another LDAP, another a file they edit by hand.
