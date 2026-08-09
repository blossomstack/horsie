---
title: What horsie is
description: A self-hosted web app for running LLM agents as durable sessions, with tools executing in a runtime you control.
kind: explanation
sidebar:
  order: 1
---

horsie server is a web app you run yourself. You open it in a browser, create a
**session**, and chat with an agent that reads files, runs commands, and edits
code — inside a **runtime** you chose.

Two things distinguish it, and both are consequences of the same decision: the
software runs on your infrastructure, not somebody else's.

## The runtime is yours

An agent is only useful if it can touch real work. horsie gives you two ways to
let it, and neither involves uploading your source tree.

The **local runtime** is a process you start on the machine your code already
lives on. It dials *out* to the server and holds that connection open. The
server never connects to you — there is no inbound port, no tunnel, and no
firewall change. The agent's tools then run against the directories you named,
on the machine you named them on.

**Cloud vendors** go the other way: the server creates a fresh container or
virtual machine for each session, checks out the repositories you picked, and
tears it down afterwards. You configure one in Settings and never run anything
yourself.

Which you want depends on the work. Editing a project you have open in an
editor wants the local runtime. Ten sessions working on ten branches at once
want a cloud vendor, because they each get their own filesystem.

## The session is durable

A session is not a chat window whose contents live in a browser tab. Every
message, tool call, tool result, and status change is written to a server-side
journal as it happens. The browser is a live view of that journal, not the
place it lives.

That makes several things ordinary that would otherwise be failure cases. You
can close the tab while a run is going and reopen it an hour later. The server
can restart mid-turn. A session that has been idle for a long time is unloaded
from memory and brought back when you next open it, with nothing lost. The
transcript is also the audit log: what the agent did, in order, with the raw
input and output of every tool call still readable.

## What it is not

It is not an IDE. There is deliberately no file browser and no diff view — you
already have an editor open, and horsie's job ends where that begins. File
edits appear in the transcript as tool calls you can expand.

It does not bring its own model. You add a provider and an API key, or sign in
with a ChatGPT plan, and horsie speaks to it on your behalf. Nothing in the
server's environment can lend a provider a credential it was not given
explicitly.

It does not decide who your users are. The server enforces that one account
cannot see another's sessions, and leaves where accounts come from to the
deployment — one operator wants a password, another an identity-aware proxy in
front.

## Where to go next

- [Quickstart](/start-here/quickstart/) — from nothing to a running session.
- [Sessions](/using/sessions/) — what the chat view offers once you are in one.
- [How it works](/internals/sessions-and-durability/) — the design behind the
  durability, for when you want to know why rather than how.
