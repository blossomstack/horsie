---
title: Models & providers
description: Add model providers and models, pick the right provider kind, and know how reasoning differs between them.
kind: how-to
sidebar:
  order: 4
---

A **provider** is where requests go and what credential they carry. A **model**
is one thing a session can be pointed at. You need at least one of each before
any session can run a turn.

Both live under **Settings → Models**, which batches its edits behind **Save
changes** — leaving the page with unsaved edits asks first.

## Add a provider

Give it a name, pick a **kind**, optionally set a base URL, and paste an API
key.

| Kind | Speaks | Use it for |
| --- | --- | --- |
| **Anthropic** | the Anthropic Messages API | Claude models, or any endpoint speaking that wire — set a base URL. |
| **OpenAI-compatible** | `/v1/chat/completions` | OpenAI, plus anything exposing the same API: Ollama, vLLM, llama.cpp, OpenRouter, DeepSeek. |
| **OpenAI Responses** | `/responses` | OpenAI's Responses API with a platform key. Unlike chat completions it carries reasoning across turns, so a reasoning model keeps its train of thought through a tool loop. |
| **ChatGPT plan** | `/responses` on OpenAI's subscription backend | Spending a ChatGPT subscription rather than API credit. Signs in instead of storing a key. |

Every provider must carry its own credential. horsie does **not** fall back to
`ANTHROPIC_API_KEY` or `OPENAI_API_KEY` in the server's environment, and does
not let `ANTHROPIC_BASE_URL` or `OPENAI_BASE_URL` redirect a provider whose
base URL is unset here. A provider is exactly what this page says it is, and
nothing in the server's environment can quietly lend it a key or move where
that key is sent. A provider with no credential is rejected when you save a
model against it.

Secrets are never returned by the API. The page shows only whether a key is
set.

## Add a model

A model needs an alias you will recognise in a dropdown, the provider, and the
provider's own model id. Optionally a max-tokens cap.

The **Admin → Model cards** catalogue autocompletes the context window,
generation cap and thinking configuration for well-known model ids. It is a
convenience for filling the form, not a source of truth — a model already saved
is unaffected by what the catalogue later says.

## Worked examples

**A local Ollama server.** Kind **OpenAI-compatible**, base URL
`http://127.0.0.1:11434`, any placeholder for the key (Ollama ignores it), and
a model id you have pulled, such as `qwen2.5`.

**DeepSeek.** Kind **OpenAI-compatible**, base URL `https://api.deepseek.com`,
models `deepseek-v4-flash` and `deepseek-v4-pro`. Both are in the bundled card
catalogue. Thinking is on by default and accepts the full effort ladder —
`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` — despite DeepSeek's
own documentation listing three.

One DeepSeek constraint is worth knowing before using it for subagents: it
rejects a pinned tool choice while thinking is enabled, answering `400 Thinking
mode does not support this tool_choice`. The model's **Pinned tool choice
disables thinking** setting handles that by turning thinking off for exactly
those requests, and the bundled cards enable it. A forced-handoff agent pins a
tool on *every* turn, so such an agent runs with thinking off throughout.
Ordinary sessions are unaffected.

## ChatGPT plan providers

A **ChatGPT plan** provider spends a ChatGPT subscription's own allowance
rather than API credit. It has no API key field: you sign in instead.

Add the provider (kind **ChatGPT plan**, no base URL) and save it. Its row
appears with a **Connect** button; press **Sign in with ChatGPT**. horsie shows
an eight-character code and a link to `auth.openai.com/codex/device`. Open that
link on any device — laptop, phone — sign in there, and enter the code. horsie
polls until you approve, then stores the credential and refreshes it without
asking again.

The sign-in is code-based rather than a browser redirect on purpose: the OAuth
client belongs to OpenAI and only redirects to `localhost`, so a horsie
reachable at a public domain could never receive the callback. With the device
flow every call runs outbound from the server — **no callback URL, no inbound
route, no reverse-proxy change** — and it works the same on a laptop as on a
server.

Three things to know:

- **Usage counts against that plan's own limits**, not an API bill. When the
  window is exhausted, turns fail with a rate-limit error until it resets.
- **Model ids are the ones the plan offers**, which are not the platform API's. A model the subscription cannot reach fails with the backend's own
  error rather than being silently substituted.
- **A model's max-tokens is ignored.** That backend rejects the parameter,
  so horsie does not send it. The field still applies to every other kind.

horsie identifies itself honestly to OpenAI, under its own name rather than
impersonating another client, and sends no client-attestation header, since only
OpenAI's own clients can produce a valid one. This is a personal-use path: sign in with your own plan.

## How reasoning differs by kind

Reasoning models surface their thinking differently by backend, and horsie
shows it the way it shows Claude's.

DeepSeek, vLLM started with a reasoning parser, and OpenRouter stream a
reasoning trace over `/v1/chat/completions`, which horsie displays as a
thinking block. On that wire the reasoning is shown but never sent back on the
next turn, because some backends reject it.

**Genuine OpenAI** models — the o-series, GPT-5 — keep their reasoning hidden
on chat completions, so on that kind you see the answer but not the thinking.
Use **OpenAI Responses** or **ChatGPT plan** for summaries: those replay the
model's own reasoning in encrypted form, which is what lets a reasoning model
keep its thread across a tool loop.

**Streaming is required.** A backend that cannot stream
`/v1/chat/completions` is not supported.

## When a turn fails immediately

After adding an OpenAI-compatible provider, the usual causes are a base URL
that does not end at the server root — horsie appends
`/v1/chat/completions` itself — or a model id the backend has not loaded.

Provider and model changes apply to the next turn. Nothing needs restarting.
