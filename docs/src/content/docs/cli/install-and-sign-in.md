---
title: Install & sign in
description: Install the horsie binary, authorize it against a server, and set a default server.
kind: how-to
sidebar:
  order: 1
---

`horsie` is a client for a horsie server. It runs the machine it is on as a
runtime source, and inspects sessions, agents, routines and workflows from a
terminal.

It is not the server, and it is not required to use one — a browser is enough
for everything except `horsie connect`.

## Install

```bash
curl -fsSL https://get.horsie.dev | sh
```

That installs a single binary for your OS and architecture, plus its sandboxed
`horsie-runtime` child.

From source instead:

```bash
make build-cli
make install-cli
```

## Sign in

```bash
horsie auth login --server https://horsie.example.com
```

```text
To authorize this machine, open:

    https://horsie.example.com/auth/device?code=PXL8-7TL7

and confirm the code:  PXL8-7TL7
```

Open the link, check the code matches what your terminal printed, and approve.
Credentials land in `~/.config/horsie/credentials.json`, readable only by you,
and refresh themselves as they age.

`horsie auth status` lists the servers this machine has credentials for.
`horsie auth logout --server <url>` forgets one; omitting `--server` forgets
all of them, revoking server-side where reachable.

For a script or a CI job, set `HORSIE_TOKEN` to a machine token instead of
logging in — see [Authentication](/operating/authentication/). To store a token
without the browser flow, `horsie auth login --token <token>`.

## The default server

The first server you log in to becomes your **default**. From then on every
command that talks to a server — `horsie session`, `horsie agent`,
`horsie workflow`, `horsie routines`, `horsie connect`, `horsie auth login` —
works without `--server`.

A later login never moves the default on its own. Pass `--default` to move it,
or manage it directly:

```bash
horsie config set default-server https://horsie.example.com
horsie config get default-server
horsie config unset default-server
```

With no default configured, commands fall back to `https://auth.horsie.dev`.

## Next

- [Running horsie connect](/cli/connect/) — make this machine a runtime.
- [Command reference](/cli/reference/) — every command and flag.
