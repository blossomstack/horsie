---
title: Sessions & durability
description: Why a session survives a closed tab, a lost connection and a server restart, and what that costs.
kind: explanation
sidebar:
  order: 1
---

A session is an **actor** with a **journal**. Almost everything else about how
sessions behave follows from those two words.

## The journal is the session

The server does not keep a session's state and write a copy somewhere for
safety. It appends every fact — a message sent, a turn started, a tool
dispatched, a tool result, a status change — to an ordered journal, and the
in-memory state is what you get by replaying it.

That inverts the usual failure question. Instead of "did we manage to save
this?" the question is "was this appended?", and everything downstream —
recovery, reconnection, the transcript you read — is the same replay.

Two consequences worth naming.

**Restart is not special.** A server that comes back up replays what it has.
There is no separate recovery path that only runs after a crash, and therefore
no recovery path that is only exercised after a crash.

**The transcript is the audit log.** What you scroll through in the browser is
not a rendering of a summary; it is the journal, including the raw input and
output of every tool call. Nothing is dropped for being verbose.

Replaying from the beginning every time would get slower forever, so the
journal is periodically snapshotted and compacted: a snapshot is a serialized
state, and replay starts from the newest one. This is why the state a session
serializes is a contract rather than an implementation detail — a change to it
has to be able to read what older snapshots wrote.

## The browser is a view, not a holder

The browser subscribes to a single server-sent-events stream and receives
journal entries as they are appended, with a cursor. Reconnecting means
resubscribing from the cursor, not refetching.

That is why closing a tab mid-run costs nothing, and why two tabs open on the
same session agree: neither of them holds anything.

History is paginated the same way. Opening a session loads recent messages;
scrolling up asks for older ones. A transcript that has been running for hours
is never fetched whole.

## Idle sessions are unloaded

An actor that has done nothing for long enough is offloaded from memory. Its
journal stays; only the running state goes.

When you next open it, or when a routine fires, it is brought back by replay.
This is invisible except in one place: `SessionStart` hooks fire again with
`source: "resume"`, because from a plugin's point of view the agent really is
starting again. That is why hooks that must run exactly once match on
`startup`.

Reading an idle session does not wake its runtime. Looking is free.

## Where a turn can be interrupted

**Stop** ends the current turn. What has already happened stays journaled, so
stopping is not undoing — the session simply stops adding to itself, and your
next message continues from there.

A dropped connection during a turn is different from stopping. The run
continues on the server; the browser missed the middle and catches up on
reconnect. But a *runtime* that loses its link mid-call is a real
interruption: a tool call in flight has no result, and the turn has to be sent
again.

Some failures cannot be continued from — a journal that cannot be appended to,
a state that cannot be replayed. Those surface as **unrecoverable** rather than
as a session that looks fine and quietly is not.

## What durability does not promise

The journal records what the agent did. It does not record what the agent's
tools did to the world. A retried step re-runs against whatever the previous
attempt left on disk, and files it wrote are still written. This is why
[workflow retries](/using/workflows/) are explicit about not being rollbacks.

The workspace belongs to the runtime, and the runtime's lifetime is not the
session's — see [Runtimes & vendors](/internals/runtimes-and-vendors/).
