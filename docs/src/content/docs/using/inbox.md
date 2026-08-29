---
title: Inbox
description: One list of everything the agents in a project have said to you — notices they left while working, and questions they stopped for.
kind: how-to
sidebar:
  order: 2
---

The **inbox** collects everything the agents in a project have addressed to the
person steering them. Open it with **Inbox** at the top of the left rail. The
number on it is how many messages you have not read yet.

It holds two kinds of message, and what separates them is whether anything is
waiting on you.

**A notice** is something an agent said while it carried on working. It calls
`notify_user` and keeps going — work finished, something surprising found, a
decision it took on its own. Nothing is parked.

Every kind of agent has that tool, unattended ones included. A
[routine](/using/routines/) that runs overnight can leave you a message without
stalling to do it, which is the case that most justifies there being an inbox.

**A question** is an agent calling `ask_user`. That tool stops the agent, and it
stays stopped until the question is resolved. Before the inbox, the only way to
notice one was to open the session it was asked in. That is the problem this
solves.

## Deal with a message

- **Read it.** Unread messages are what the count on the rail counts.
- **Reply.** On a question your reply is the answer, and the agent picks up
  again immediately. On a notice it is an ordinary message to that agent, the
  same as typing it into the session.
- **Open the session it came from.** Every message names the session and the
  agent it came from, and links back to them.
- **Delete it**, one at a time or several together.

A message stays in the list after it has been dealt with. It is history until
you delete it.

## Decline a question

Deleting a question that is still open does more than remove the row. The agent
is told that nobody is going to answer, and it carries on using its own
judgement and says what it assumed. You are asked to confirm before that happens.

This is deliberate. Quietly dropping the row would leave the agent stopped for
good, with nothing left on screen to start it again.

## What the states mean

| State | Means |
| --- | --- |
| **Open** | Nothing has been done with it. |
| **Answered** | You answered the question, or replied to the notice. |
| **Declined** | You declined the question, and the agent went on without an answer. |
| **Closed** | Never answered. Typing a new message in the session instead of answering abandons the question, and it closes. |

The two places you can answer from are one path, not two. Answering on the
session page — see [Answer a question](/using/sessions/#answer-a-question) —
shows as **answered** in the inbox as well.

## Turn it off

`notify_user` sits in the same tool group as `ask_user`, under **session** in a
session's [Tools](/using/sessions/#create-one) control. They are the two halves
of talking to you and differ only in whether the agent stops to hear back, so
clearing that group leaves an agent that can do neither. Open the group to turn
off one and keep the other.
