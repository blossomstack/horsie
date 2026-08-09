---
title: Runtimes & vendors
description: Why runtimes dial out, why the vendor contract is only four methods, and who owns a runtime's spec.
kind: explanation
sidebar:
  order: 2
---

A **runtime** is where an agent's tools actually run. A **vendor** is a source
of runtimes. Everything here follows from one constraint: horsie should be able
to run tools on substrates it does not know about, without the substrate
knowing about horsie.

## Everything dials out

Neither shape of runtime is dialed into.

A `horsie connect` process opens an outbound WebSocket to the server and holds
it. A cloud runtime — a container or a machine the server created — dials
`/api/runtime/connect` on the server itself, authenticated with a token derived
from a per-account secret.

The alternative would be for the server to connect *to* each runtime. That
needs every runtime to be reachable: a listener, a public address, and a
credential to guard it. Dialing out means a laptop behind NAT and a container
on someone else's infrastructure both work with no inbound anything, and a
vendor with no listener of its own gets TLS and reverse proxies for free
because it is talking to the same HTTP server the browser is.

It also decides who owns configuration. A `horsie connect` vendor is configured
where it runs, because that is where the directories are. A cloud vendor is
configured in the server's settings, because there is nowhere else — its stored
configuration is the only evidence it exists.

## The contract is deliberately small

A vendor implements two traits and four lifecycle methods: create, get,
hibernate, delete. Each returns the caller's first observation and finishes on
a progress sink, so a caller learns immediately that something is happening
without waiting for it to finish.

The traits are sized for a substrate nobody has written yet. Capability
differences live **inside** an implementation and never reach the trait: Fly
keeps a volume across a hibernate, velos rebuilds the workspace, and neither
fact is visible to the code that calls them. A vendor that cannot suspend
declines to.

The temptation is to let the trait describe what each substrate can do, so
callers can branch. That gets the trait a new method every time a substrate is
added, and callers gain branches for cases they cannot test. Adding a kind
should be a match arm, not an API change.

## Acquisition carries the spec

When the server asks a vendor for a runtime, the request carries the runtime's
spec — the workspace layout, the repositories, the variables.

The alternative is for each vendor to write the spec down when the runtime is
created. Then two things own one fact, and they drift the moment a session's
environment changes. Worse, a vendor's copy ages: a stored credential replayed
on a later revive is a stale token by definition.

Carrying the spec is **not** permission to provision from nothing. A vendor
with no runtime under the given id still fails the request, and the server
turns that into a terminally unrecoverable session rather than silently
rebuilding a workspace the user believes still exists. The spec is how to
rebuild a runtime the vendor knows it owns — never how to invent one.

## Losing the link means different things

A runtime that dials a WebSocket endpoint re-dials for as long as it lives. Its
link is a network path, and network paths come back.

A runtime spoken to over a Unix socket exits on the first dropped frame,
because there the link *is* its parent process. Without that distinction, a
runtime whose local vendor died would linger for the whole connect budget,
holding a workspace nobody can reach.

Runtimes already running survive a vendor's reconnection, and survive a server
restart. What does not survive is a tool call that was in flight: it has no
result, and the turn is sent again.

## One runtime per session

Each session gets its own runtime, so stopping or deleting one session cannot
disturb another.

The local runtime is the exception you have to reason about: every session on
that vendor works in the same directories, because those directories are yours
and existed before horsie did. The isolation is per-process, not per-filesystem.
A cloud vendor gives each session its own filesystem, which is the actual
reason to reach for one.

## Hibernation is not deletion

A session that goes cold releases what it can. On Fly that means stopping the
machine, which keeps its volume, so the session comes back to the workspace it
left. On velos there is no stop — only delete, which would throw the workspace
away — so an idle velos session keeps its container and its compute.

The transcript is unaffected either way, because it was never in the sandbox.
It is on the server, in the journal. See
[Sessions & durability](/internals/sessions-and-durability/).
