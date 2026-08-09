---
title: Plugins & hooks
description: Where bundle content is stored, where it executes, and why a hook that could not run denies the call it guarded.
kind: explanation
sidebar:
  order: 4
---

A bundle is content the server owns and the runtime executes. Keeping those two
halves straight explains most of the behaviour in
[Skills & plugins](/using/skills-and-plugins/).

## The server owns the content

Installing a bundle clones it once and stores it as a content-addressed zip.
The catalogue — what a bundle contains, which skills and commands and agent
types it declares — is read at install time and kept in the database.

Nothing scans a runtime to find out what a plugin offers. The catalogue is a
database read, so the UI can list a bundle's contents with no runtime running
at all, and two sessions selecting the same bundle get provably the same bytes.

## The runtime executes it

When a runtime starts, the server hands it the `{name, hash}` refs its session
selected plus a short-lived token. The runtime fetches each zip over its own
outbound connection, checks the hash, and unpacks it into a directory of its
own.

That directory belongs to one runtime and is deleted with the session. A
session sees exactly the bundles it selected and never another session's.

Hooks, plugin MCP servers, and anything a skill runs execute **there** — next
to the workspace, under the same sandbox as the rest of the runtime. Running a
plugin's local MCP server in the server process would be both wrong and a way
for a plugin to execute commands on the server host.

That is the difference from the MCP servers you add in Settings, which do run
in the server process and can therefore do interactive OAuth. A plugin server
authenticates with whatever its declaration carries, sent as written.

## Hooks are participants, not observers

A hook can change what happens. `PreToolUse` can deny a call or rewrite its
input; `PostToolUse` can rewrite the output before the agent reads it;
`UserPromptSubmit` can refuse a prompt; `Stop` can refuse to let a turn end and
send the agent back for another round. Any hook can halt the agent outright
with `continue: false`.

Because they participate, they are journaled. Hook records live in the agent's
transcript as their own kind of entry rather than being folded into the tool
call they wrapped, so reading a transcript tells you a hook intervened, and
which one.

## A guard that could not run has not permitted anything

An HTTP hook that does not answer — a non-2xx, an unreachable endpoint, a
timeout — is recorded as having failed. For `PreToolUse`, that **denies** the
call it was guarding.

This is a deliberate divergence: upstream Claude Code lets an HTTP hook failure
through. The argument for failing closed is that a `PreToolUse` hook exists to
withhold permission, and a hook that never ran has withheld it. The argument
against is that a flaky webhook now breaks your session.

Both are real, which is why the guidance is narrow rather than general: point a
`PreToolUse` webhook only at an endpoint you are willing to have gate your tool
calls. Command hooks have an exit code and do not have this ambiguity.

## Why only eight events

horsie runs eight of the events the plugin spec documents. Eight more are
understood and not yet fired. The remaining fifteen have no horsie equivalent —
permission prompts, context compaction, worktrees, file watching, agent teams,
MCP elicitation, the display layer. Each would need a subsystem, not a hook.

A bundle declaring an event horsie cannot fire still installs, and the events
it cannot run are named rather than silently ignored. Silently ignoring them
would mean a bundle whose guard never fires looks exactly like one whose guard
passes.

## Trust is not partitioned

Installing a bundle is an account-level act, and the software it brings runs
with the runtime's privileges. There is no per-session sandbox *within* the
runtime that would make an untrusted bundle safe to select for one session and
not another.

"Who installed this plugin" was never a privilege question, and treating it as
one would suggest a boundary that does not exist. Install bundles you trust.
