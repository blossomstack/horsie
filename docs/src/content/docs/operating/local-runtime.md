---
title: The local runtime
description: Run horsie connect so sessions execute their tools on your own machine, against directories you name.
kind: how-to
sidebar:
  order: 2
---

`horsie connect` turns the machine you run it on into a source of sandboxes.
It dials *out* to the server and holds the connection open; the server never
connects to you, so there is no inbound port and no firewall change.

Use it when the agent should work on files that already exist somewhere — a
checkout you have open in an editor, a machine holding credentials the server
should not have, anything behind a network the server cannot reach.

## Sign in first

A vendor has to prove who it is before the server will publish it. A name like
`local` belongs to whoever claimed it, so nobody else can take it over and
start receiving your tool calls.

On a machine you sit at, a normal login is enough:

```bash
horsie auth login --server https://horsie.example.com
```

For something unattended — a container, a CI runner, anything with nobody to
approve a code — mint a **machine token** in **Settings → Account → Machine
tokens** and pass it as `HORSIE_TOKEN`. The secret is shown once and only its
hash is stored, so there is nothing to recover if you lose it. Revoke it from
the same page.

Against a server with authentication off, neither is needed.

## Connect

```bash
horsie connect \
  --server https://horsie.example.com \
  --workspace main=/path/to/your/project \
  --name my-laptop
```

- `--server` — the server's HTTP(S) URL. Omit it to use your default server.
- `--workspace name=path` — a directory to serve, repeatable. A bare path
  becomes `main=<path>`. At least one is required.
- `--name` — how this machine appears when picking a runtime. Defaults to
  `local`, matching the server's own default runtime vendor.
- `--no-sandbox` — run the runtimes unconfined.

It appears under **Settings → Runtimes** as soon as it dials in. Keep it
running: sessions can reach the machine only while it is up.

## What to expect

**One runtime per session.** A separate `horsie-runtime` child is spawned per
session, so stopping or deleting one session does not disturb another. When
`horsie connect` exits it kills the runtimes it started.

**Every session shares your directories.** They all work in the paths you
passed, so two sessions running at once can edit the same files. The process
prints this warning on startup. If you want a filesystem per session, use a
[cloud vendor](/operating/cloud-vendors/).

**Sandboxing is on by default.** Each runtime is confined with the vendor's
baseline capability spec — workspaces read-write, system toolchain read-only,
network allowed. Sandbox support is probed at startup, and the process refuses
to start on a host that cannot confine a child unless you pass `--no-sandbox`.

**There is no `--background`.** It is a long-lived supervisor with child
processes, so run it under a process manager — systemd, launchd, tmux — where
its lifetime and its logs are managed explicitly.

**It reconnects on its own.** A server restart or a network blip is retried
with a backoff from one second to thirty, indefinitely, and every attempt is
printed. Runtimes already running are kept alive across the gap; only stopping
the process stops them. A turn that was in flight when the link dropped has to
be sent again.

## One vendor per name

A name belongs to the process holding it, for as long as it is connected.
Starting a second one on a name already in use stops immediately:

```text
runtime vendor name "my-laptop" is already in use by another vendor process
```

A refused vendor learns that the name is taken and nothing about who has it.
Stop the process already serving it, or pass `--name <label>` to serve under a
different one. A name that a **cloud vendor** is configured under is reserved
the same way, and reported as `is configured on the server`.

Your own process reconnecting is not a collision — it reclaims its name
straight away after a blip or a server restart. A name is released as soon as
its vendor disconnects, and within 45 seconds if the machine vanishes without
hanging up, such as a closed laptop or a dropped VPN. The vendor heartbeats
every 15 seconds so the server can tell the two apart.

## What it cannot do

It cannot check out GitHub repositories. It runs over a directory you own
rather than one it built, so the repository picker stays hidden for it. Use a
cloud vendor for checkouts.

It *can* load skill bundles: the runtime fetches the ones its session selected
over its own outbound connection, which needs no workspace to have been built.
See [Skills & plugins](/using/skills-and-plugins/).

## Troubleshooting

**It does not appear in Settings.** Confirm the process is still running.
Registrations are held in memory, so a server restart drops them; the vendor
reconnects on its own within about half a minute and prints each attempt.

**A session will not run a turn.** Either there is no runtime — check this
process is connected — or there is no model. Both are required.

**It refuses to start, citing the sandbox.** The host cannot confine a child
process. Fix the host, or accept the risk with `--no-sandbox`.
