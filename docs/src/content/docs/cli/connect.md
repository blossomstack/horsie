---
title: Running horsie connect
description: Serve directories on this machine as a runtime, and keep the process healthy.
kind: how-to
sidebar:
  order: 2
---

`horsie connect` holds one outbound connection to a server and spawns a
`horsie-runtime` child per session. It is how a session's tools come to run on
this machine, against directories you name.

What it does and how to operate it is covered in
[The local runtime](/operating/local-runtime/) — read that first. This page is
the terminal-side detail.

## The shape of a run

```bash
horsie connect \
  --server https://horsie.example.com \
  --workspace main=/path/to/project \
  --workspace docs=/path/to/docs \
  --name my-laptop
```

```text
connected to https://horsie.example.com as vendor "my-laptop" · workspace "main" -> /path/to/project
note: every session on this vendor works in /path/to/project; concurrent sessions will edit the same files
open https://horsie.example.com in your browser to start a session
```

The process stays in the foreground. Sessions can reach this machine only while
it is up.

## Flags

| Flag | Default | Meaning |
| --- | --- | --- |
| `--server <url>` | your default server | The server to dial. |
| `--workspace [name=]path` | *(required)* | A directory to serve. Repeatable. A bare path becomes `main=<path>`. |
| `--name <label>` | `local` | The vendor name the server publishes this machine under. `--runtime-id` is an accepted alias. |
| `--no-sandbox` | off | Run the spawned runtimes unconfined, inheriting the ambient environment. |
| `--config <path>` | the user config path | Which config file to read. |

## Keeping it running

There is deliberately no `--background`. It is a supervisor with child
processes, so its lifetime and its logs belong to a process manager.

A minimal systemd user unit:

```ini
[Unit]
Description=horsie runtime vendor
After=network-online.target

[Service]
ExecStart=%h/.local/bin/horsie connect --server https://horsie.example.com --workspace %h/code
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now horsie-connect
```

For an unattended host, put a machine token in the unit's environment as
`HORSIE_TOKEN` rather than running an interactive login.

## Reading its output

Every reconnection attempt is printed, with a backoff from one second to
thirty. Runtimes already running survive the gap; only stopping the process
stops them.

If it exits immediately, the two usual causes are a name collision — another
vendor already holds `--name` — and a host that cannot sandbox a child process.
Both print the reason.
