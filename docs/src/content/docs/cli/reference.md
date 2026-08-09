---
title: Command reference
description: Every horsie CLI command, its arguments and its flags.
kind: reference
sidebar:
  order: 3
---

Every command that talks to a server accepts `--server <url>`. Omit it and the
command uses your default server, falling back to `https://auth.horsie.dev`
when none is configured. See [Install & sign in](/cli/install-and-sign-in/).

## `horsie auth`

Log in to a server so other commands can reach it.

| Command | Arguments | Flags |
| --- | --- | --- |
| `auth login` | — | `--server <url>`, `--token <token>` (store this instead of running the browser flow), `--default` (make this the default server) |
| `auth logout` | — | `--server <url>` — omit to log out of every server |
| `auth status` | — | — |

## `horsie connect`

Dial a server as this machine's runtime source. See
[Running horsie connect](/cli/connect/).

| Flag | Default | Meaning |
| --- | --- | --- |
| `--server <url>` | default server | Server to dial. |
| `--workspace [name=]path` | *(required)* | Directory to serve. Repeatable. |
| `--name <label>` | `local` | Vendor name. Alias: `--runtime-id`. |
| `--no-sandbox` | off | Do not sandbox the runtimes. |
| `--config <path>` | user config | Config file to read. |

## `horsie session`

| Command | Arguments | Flags |
| --- | --- | --- |
| `session list` | — | `--server` |
| `session status` | `<session-id>` | `--server` |
| `session tail` | `<session-id>` | `--output <file-or-dir>` *(required)*, `--events <mode>` (default `messages`), `--agent <agent-id>`, `--server` |

`session tail` streams a session's messages to a local JSONL file until Ctrl-C,
and resumes after the last recorded event when the output file already exists.
`--output` accepts a directory, in which case it writes `<session-id>.jsonl`.

`--agent` picks whose transcript to follow; omitted, it follows the session's
main agent. **A workflow run has no main agent** — pass a step's agent id from
`horsie workflow status`.

## `horsie agent`

Agent presets, and starting sessions from them.

| Command | Arguments | Flags |
| --- | --- | --- |
| `agent list` | — | `--server` |
| `agent get` | `<name>` | `--server` |
| `agent invoke` | `<name>` | `-m/--message <text>` *(required)*, `--environment <name>` or `--vendor <name>` *(one required)*, `--repo <url>` (repeatable, goes with `--vendor`), `--session-name <title>`, `--server` |

`agent invoke` creates a session and prints its id and web link immediately.

## `horsie workflow`

| Command | Arguments | Flags |
| --- | --- | --- |
| `workflow list` | — | `--server` |
| `workflow get` | `<name>` | `--json`, `--server` |
| `workflow apply` | — | `-f/--file <path>` *(required)*, `--server` |
| `workflow delete` | `<name>` | `--server` |
| `workflow run` | `<name>` | `-i/--input <text>` *(required)*, `--environment <name>` or `--vendor <name>` *(one required)*, `--repo <url>` (repeatable), `--session-name <title>`, `--server` |
| `workflow status` | `<session-id>` | `--server` |
| `workflow retry` | `<session-id> <step-index>` | `--server` |

`workflow get --json` prints the definition that `workflow apply` takes back,
so a workflow round-trips through a file. `apply` creates or fully replaces;
the name comes from the file.

`workflow run` prints the run's session id. A run **is** a session, so
`session status` and `session tail` work on it. `workflow retry` appends an
attempt — the workspace is not rolled back.

Deleting a workflow leaves its runs alone; each holds its own snapshot of the
graph.

## `horsie routines`

| Command | Arguments | Flags |
| --- | --- | --- |
| `routines list` | — | `--server` |
| `routines get` | `<name>` | `--server` |
| `routines invoke` | `<name>` | `--server` |

`routines invoke` triggers a run now, creating an unattended session. It does
not disturb the schedule.

## `horsie config`

Read and write CLI settings in the user config file. Supported key:
`default-server`.

| Command | Arguments | Flags |
| --- | --- | --- |
| `config set` | `<key> <value>` | `--config <path>` |
| `config get` | `<key>` | `--config <path>` |
| `config unset` | `<key>` | `--config <path>` |

## Environment

| Variable | Effect |
| --- | --- |
| `HORSIE_TOKEN` | Bearer token to send instead of reading stored credentials. For scripts and CI. |

Config-file keys the CLI reads are listed in the
[Configuration reference](/operating/configuration/#keys-the-cli-owns).
