---
title: Environments
description: Save a runtime, its repositories, variables and setup steps under a name you can pick from anywhere.
kind: how-to
sidebar:
  order: 3
---

An **environment** answers one question about a piece of work: *where does this
run, and what does it run against?* Every way a session comes into being — the
new-session page, an agent invoke, a routine trigger, a workflow run — asks it,
and none of them assumes an answer.

There are two ways to answer.

**Pick a runtime.** Choose one of the runtimes connected right now. If it
builds its own workspace, you also pick the repositories to check out into it.
This is the ad-hoc answer, and it is the right one most of the time.

**Pick a saved environment.** A runtime, its repositories, its variables and
its setup steps, under a name. Choose it once and every session created from it
is configured the same way.

## When to save one

Save an environment when you notice yourself ticking the same three
repositories and the same runtime every time, or when a routine has to run
somewhere specific every night and you would rather fix that in one place than
in every routine.

Everything else — a one-off session against one repository — is faster as an
ad-hoc pick.

## Create one

**Environments → New environment.**

**Name** — a slug, like `staging`. It is the id: environments are addressed by
name in the API, and the name cannot be changed later.

**Runtime vendor** *(required)* — which runtime builds the workspace. Only
vendors that build their own are listed. The local runtime works in a fixed
directory you already own, so there is nothing for an environment to describe;
to run there, pick the runtime directly.

**Repositories** — zero or more repositories from your GitHub App
installation, each with an optional ref. See
[GitHub repositories](/using/github-repositories/).

**Environment variables** — plain, non-sensitive values injected into the
runtime. Names in the server's reserved `HORSIE_*` and `GIT_CONFIG_*`
namespaces are refused: the server and the runtime set those themselves.
Secrets are a separate concept and this is not it — treat everything here as
readable.

**Setup steps** — commands the runtime runs after the checkouts and before the
agent's first turn, like `make setup` or `npm install`. Checkouts always run
first, so a step may assume its repository is on disk.

## What happens when you use one

The environment is **resolved and copied** into the session at creation.
Editing it afterwards changes what the *next* session gets; it never re-points
one that already exists, and never moves a running session to a different
machine or checkout.

That is also why deleting an environment is unconditional. Sessions already
created are unaffected.

A **routine** is the one thing holding a lasting reference. Its next run fails
with `unknown environment '<name>'`, recorded on the routine's page — the same
way it reports an agent preset deleted underneath it.

A session's own page shows what it resolved to: the environment's name, the
runtime it ran on, and the repositories it got.

## From the CLI

Commands that start work take the two answers as two flag shapes, and one of
them is required:

```bash
horsie agent invoke <name> -m <message> (--environment <env> | --vendor <v> [--repo <url>])
horsie workflow run <name> --input <text> (--environment <env> | --vendor <v> [--repo <url>])
```

`--repo` goes with `--vendor`. A saved environment carries its own
repositories, so passing both is an error rather than a merge.
