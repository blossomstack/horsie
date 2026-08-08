# Environments

An **environment** answers one question about a piece of work: *where does this
run, and what does it run against?* Every way a session comes into being — the
new-session page, an agent-preset invoke, a routine trigger, a workflow run —
asks it, and none of them assumes an answer.

There are two ways to answer.

**Pick a runtime.** Choose one of the runtimes currently connected. If it
provisions its own workspace (velos), you also pick the repositories to check
out into it. This is the ad-hoc answer, and it is the right one most of the
time.

**Pick a saved environment.** A runtime, its repositories, its environment
variables and its setup steps, saved under a name. Choose it once and every
session created from it is configured the same way.

## When to save one

Save an environment when you find yourself ticking the same three repos and the
same runtime every time, or when a routine has to run somewhere specific every
night and you would rather fix it in one place than in every routine. Everything
else — a one-off session against one repo — is faster as an ad-hoc pick.

## Creating one

**Environments → New environment.**

- **Name** — a slug (`staging`). It is the id: environments are addressed by
  name in the API, and the name cannot be changed later.
- **Runtime vendor** *(required)* — which runtime builds the workspace. Only
  vendors that provision their own workspace are listed: an environment exists
  to describe a machine that gets built, and the `local` runtime runs in a fixed
  directory you already own. To run there, pick the runtime directly instead.
- **Repositories** — 0..N repos from your GitHub App installation, each with an
  optional ref. See [GitHub](github.md).
- **Environment variables** — plain, non-sensitive values injected into the
  runtime. Names in the server's reserved `HORSIE_*` namespace, and
  `GITHUB_TOKEN`, are refused: the server sets those itself. Secrets are a
  separate concept and not this one — treat everything here as readable.
- **Setup steps** — commands the runtime runs after the checkouts and before the
  agent's first turn (`make setup`, `npm install`). Checkouts always run first,
  so a step can assume its repository is on disk.

## What happens when you use one

The environment is **resolved and copied** into the session when the session is
created. Editing the environment afterwards changes what the *next* session
gets; it never re-points one that already exists, and it never moves a running
session to a different machine or checkout.

That also means deleting an environment is unconditional. Sessions already
created are unaffected. A **routine** is the one thing that holds a lasting
reference: its next run fails with `unknown environment '<name>'`, recorded on
the routine's page — the same way a routine reports an agent preset that was
deleted under it.

A session's own page shows what it resolved to: the environment's name, the
runtime it ran on, and the repositories it got.

## From the CLI

Commands that start work take the two answers as two flag shapes — one is
required:

```console
horsie agent invoke <name> -m <message> (--environment <env> | --vendor <v> [--repo <url>])
horsie workflow run <name> --input <text> (--environment <env> | --vendor <v> [--repo <url>])
```

`--repo` goes with `--vendor`. A saved environment carries its own repositories,
so passing both is an error rather than a merge.
