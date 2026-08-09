---
title: Cloud runtime vendors
description: Configure a Fly Machines or velos vendor so the server builds a fresh runtime for each session.
kind: how-to
sidebar:
  order: 3
---

A **cloud vendor** is a row in your settings. The server talks to the
substrate's API directly, so there is no process of yours to deploy and nothing
to restart — fill the row in and the next session can use it.

Each session gets its own sandbox — container or machine — with its own
filesystem and its own checkouts, torn down when the session is deleted. That
per-session isolation is the difference from
[the local runtime](/operating/local-runtime/), where every session shares the
directories you already own. It is the reason to reach for one: ten sessions on
ten branches that do not fight.

Two kinds ship today. Both check out GitHub repositories and load skill
bundles.

## Add one

**Settings → Runtimes → Cloud vendors → Add.** Pick a kind and give the vendor
a name — that name is what sessions pick it by, and it cannot collide with a
`horsie connect` vendor's name.

### Fly Machines

Starts one Fly Machine per session.

| Field | Notes |
| --- | --- |
| **App** | The Fly app machines are created in. It must already exist — horsie creates machines, not apps. |
| **API token** | Required. Stored write-only; the settings page can show only that a credential is set. |
| **Image** | An OCI image with `horsie-runtime` in it. |
| **Region** | Where machines are created. |
| **Workspace root** | Where inside the machine workspaces are allocated. |
| **Callback URL** | See below. |
| **Volumes** | Give each runtime a volume, so a stopped machine keeps its workspace. |
| **CPU kind / CPUs / Memory** | Machine size. At least one CPU and some memory. |
| **Volume size** | Required if volumes are on. |

### velos

Schedules one container per session on a
[velos](https://github.com/blossomstack/velos) backend.

| Field | Notes |
| --- | --- |
| **Server URL** | The velos root, e.g. `http://velos:8080`. Must start with `http://` or `https://`. |
| **API token** | Optional — a velos deployment may run without auth, so horsie does not demand one. |
| **Image** | An OCI image bundling `horsie-runtime`, built without the sandbox feature: the container is already the isolation boundary. |
| **Runtime binary** | Path to `horsie-runtime` inside that image. |
| **Workspace root** | Where inside the container workspaces are allocated. |
| **Callback URL** | See below. |
| **CPU / Memory** | Container size. |

## The callback URL

This is the one field with no sensible default, and the one worth getting right
first. It is the `ws://` or `wss://` address a sandbox reaches **your server**
on, from wherever that sandbox runs — not necessarily the address your browser
uses.

Two things happen when you save it:

- A bare origin gains the connect path, so `wss://horsie.example.com` is stored
  as `wss://horsie.example.com/api/runtime/connect`. A URL you wrote with a
  trailing slash is handled too.
- An address that only resolves on the server itself — `localhost`,
  `127.0.0.1`, `0.0.0.0`, `::1`, or anything under `.localhost` — is **refused**
  with an error naming the host. Inside a container those names mean the
  container. Without the check a vendor configured this way fails as a session
  that waits forever rather than as something you can act on.

## Build the runtime image

From the repository:

```bash
docker build -f docker/horsie.Dockerfile --target runtime -t your-registry/horsie-runtime:latest .
docker push your-registry/horsie-runtime:latest
```

Push it somewhere the substrate can pull from, and put that reference in the
vendor's **Image** field.

## What an idle session costs

This is the one place the two kinds are not interchangeable.

A **Fly** machine is stopped when its session goes cold, and started again on
the next message. It keeps its volume, so the session finds its workspace as it
left it.

**velos** has no way to stop a container — only to delete one, which would
throw the workspace away. So an idle velos session keeps its container, and its
compute, until the session is deleted.

Your transcript is safe either way: it lives on the server, not in the sandbox.

## Choosing a vendor per session

**Settings → Runtimes → Default vendor** names which vendor new sessions use.
It may name a `horsie connect` vendor that has not connected yet — the
preference takes effect once it dials in.

Per session, the environment control offers whatever is available: connected
vendors, configured cloud vendors, and your saved
[environments](/using/environments/).

## Adding another kind

Implement `RuntimeVendor` and `RuntimeHandle` in the `horsie-runtime-vendor`
crate against your substrate's API, and add a variant to the settings union so
it can be configured. Four lifecycle methods: create, get, hibernate, delete.

The Fly and velos vendors are deliberately structural twins and are the worked
examples. What each substrate can do — Fly keeps a workspace across a
hibernate, velos rebuilds it — stays inside its implementation and never
reaches the trait. See [Runtimes & vendors](/internals/runtimes-and-vendors/)
for why the contract is shaped that way.

If the runtimes must live somewhere the server cannot reach, you want
[`horsie connect`](/operating/local-runtime/) instead, not a new vendor kind.
