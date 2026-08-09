---
title: Runtimes & vendors
description: Why sandboxes dial out, why the vendor contract is four methods, and who owns a sandbox's spec.
kind: explanation
sidebar:
  order: 3
---

A **runtime** is horsie's sandbox: the isolated place where an agent's tool
calls actually execute. A **vendor** is a source of them.

The sandbox is the one part of the system that is allowed to fail. Everything
important — the session, the transcript, the record of what was done — lives
outside it, which is what makes rebuilding one a normal event rather than an
incident.

Everything below follows from one constraint: horsie should be able to run tools
on substrates it does not know about, without the substrate knowing about
horsie.

## Everything dials out

Neither shape of sandbox is dialled *into*.

A `horsie connect` process opens an outbound connection to the server and holds
it. A cloud sandbox — a container or machine the server created — dials
`/api/runtime/connect` on the server itself, authenticated with a token derived
from a per-account secret.

The alternative would be for the server to connect *to* each sandbox. That needs
every sandbox to be reachable: a listener, a routable address, and a credential
guarding it. Dialling out means a laptop behind NAT and a container on somebody
else's infrastructure both work with no inbound anything — and a substrate with
no listener of its own inherits TLS and reverse proxies for free, because it is
talking to the same HTTP server the browser is.

It also settles who owns configuration. A `horsie connect` vendor is configured
where it runs, because that is where the directories are. A cloud vendor is
configured in the server's settings, because there is nowhere else — its stored
configuration is the only evidence it exists.

## The contract is deliberately small

A vendor implements two traits and four lifecycle methods: create, get,
hibernate, delete. Each returns the caller's first observation and finishes on a
progress sink, so a caller learns immediately that something is happening
without waiting for it to finish.

The traits are sized for a substrate nobody has written yet. Capability
differences live **inside** an implementation and never reach the trait: one
substrate keeps a volume across a hibernate, another rebuilds the workspace, and
neither fact is visible to the code that calls them. A vendor that cannot
suspend declines to.

The temptation is to let the trait describe what each substrate can do, so
callers can branch. That earns the trait a new method every time a substrate is
added, and earns callers branches for cases they cannot test. Adding a kind
should be a match arm, not an API change.

## Acquisition carries the spec

When the harness asks a vendor for a sandbox, the request carries that sandbox's
spec — the workspace layout, the repositories, the variables.

The alternative is for each vendor to write the spec down at creation. Then two
things own one fact and they drift the moment a session's environment changes.
Worse, a vendor's copy ages: a stored credential replayed on a later revive is a
stale token by definition.

Carrying the spec is **not** permission to provision from nothing. A vendor with
no sandbox under the given id still fails the request, and the harness turns
that into a terminally unrecoverable session rather than silently rebuilding a
workspace the user believes still exists. The spec is how to rebuild a sandbox
the vendor knows it owns — never how to invent one.

## Losing the link means different things

A sandbox that dials a WebSocket endpoint re-dials for as long as it lives. Its
link is a network path, and network paths come back.

One spoken to over a Unix socket exits on the first dropped frame, because there
the link *is* its parent process. Without that distinction, a sandbox whose
local vendor died would linger for the whole connect budget, holding a workspace
nobody can reach.

Sandboxes already running survive a vendor's reconnection and survive a server
restart. What does not survive is a tool call that was in flight: it has no
result, and the turn is sent again.

## Isolation is per-session — with one exception

Each session gets its own sandbox, so stopping or deleting one session cannot
disturb another.

The local runtime is the exception you have to reason about. Every session on
that vendor works in the same directories, because those directories are yours
and existed before horsie did. The isolation is per-process, not
per-filesystem — two sessions running at once can edit the same files.

That is the actual reason to reach for a cloud vendor: a filesystem per session,
so ten sessions on ten branches do not fight.

## Hibernation is not deletion

A session that goes cold releases what it can.

Where the substrate can stop a machine and keep its volume, it does, and the
session comes back to the workspace it left. Where the substrate has no stop —
only delete, which would throw the workspace away — an idle session keeps its
container and its compute until the session is deleted.

The transcript is unaffected either way, because it was never in the sandbox.
→ [Sessions & durability](/internals/sessions-and-durability/)
