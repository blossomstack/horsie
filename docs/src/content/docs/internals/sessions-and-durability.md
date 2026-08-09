---
title: Sessions & durability
description: The session as an append-only log, why checkpoints exist, and what durability does and does not promise.
kind: explanation
sidebar:
  order: 2
---

A **session** is one running agent and the append-only record of everything it
did. Not a chat window, and not a process — a log.

Almost everything else about how sessions behave follows from that.

## The log is the session

The server does not keep a session's state and write a copy somewhere for
safety. It appends every fact — a message sent, a turn started, a tool
dispatched, a tool result, a status change, a hook's verdict, an answer you gave
— and the in-memory state is what you get by replaying it.

That inverts the usual failure question. Instead of "did we manage to save
this?", the question is "was this appended?" — and recovery, reconnection and
the transcript you read are all the same replay.

**Restart is not special.** A server that comes back replays what it has. There
is no separate recovery path, and therefore no recovery path that is only
exercised after a crash.

**The transcript is the trace.** What you scroll through is not a rendering of a
summary; it is the log, including the raw input and output of every tool call.
Nothing is dropped for being verbose, because the run you most need to read is
the one that went wrong.

## Checkpoints

Replaying from the first message forever gets slower forever. So the journal is
periodically snapshotted and compacted: a snapshot is a serialized state, and
replay starts from the newest one.

That makes the state a session serializes a **contract**, not an implementation
detail. A change to it has to be able to read what older snapshots wrote —
renaming a persisted variant once took a live deployment down, because the
running supervisor could no longer read its own history.

The sandbox has its own version of the same idea. A session that goes cold can
have its machine stopped and brought back, with the workspace intact, rather
than rebuilt from nothing. → [Runtimes & vendors](/internals/runtimes-and-vendors/)

## The browser is a view, not a holder

The browser subscribes to one server-sent-events stream and receives entries as
they are appended, with a cursor. Reconnecting means resubscribing from the
cursor, not refetching.

That is why closing a tab mid-run costs nothing, and why two tabs open on the
same session agree — neither of them holds anything.

History paginates the same way: opening a session loads recent messages and
fetches older ones as you scroll. A transcript that has been running for hours
is never fetched whole.

## Idle sessions are unloaded

A session that has done nothing for long enough is offloaded from memory. The
log stays; only the running state goes. Opening it, or a routine firing, brings
it back by replay.

This is invisible except in one place: `SessionStart` hooks fire again with
`source: "resume"`, because from a bundle's point of view the agent really is
starting again. That is why a hook that must run exactly once matches on
`startup`. → [Plugins & hooks](/internals/plugins-and-hooks/)

Reading an idle session does not wake its sandbox. Looking is free.

## Where a turn can be interrupted

**Stop** ends the current turn. What already happened stays journaled, so
stopping is not undoing — the session stops adding to itself, and your next
message continues from there.

**A dropped browser connection** is not an interruption at all. The run
continues on the server; the browser missed the middle and catches up.

**A sandbox that loses its link mid-call** is a real one. A tool call in flight
has no result, and the turn has to be sent again.

Some failures cannot be continued from — a log that cannot be appended to, a
state that cannot be replayed. Those surface as **unrecoverable**, rather than
as a session that looks fine and quietly is not.

## What durability does not promise

The log records what the agent did. It does not record what the agent's tools
did to the world.

A retried step re-runs against whatever the previous attempt left on disk. Files
it wrote are still written; commits it made are still made. This is why
[workflow retries](/using/workflows/) are explicit about not being rollbacks,
and why a step that is not safe to run twice needs to say so in its own prompt.

The workspace belongs to the sandbox, and the sandbox's lifetime is not the
session's.
