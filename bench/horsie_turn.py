"""Driving one horsie turn to completion, and reading its result.

Split out from any particular benchmark because the two hard parts are not
benchmark-specific: knowing when a turn is *actually* over, and getting the
assistant's reply out of the transcript without accidentally reading its
reasoning instead. Both were wrong in an earlier version of this code and were
corrected against a live server; the docstrings say what the evidence was.
"""

from __future__ import annotations

import time
from typing import Any

from horsie_client import Horsie, HorsieError

# Statuses a session can sit in with no turn in flight. `Finished` is a
# workflow-run state a plain session never reaches, but treating it as terminal
# costs nothing and avoids a hang if that ever changes.
TERMINAL = {"Idle", "AwaitingInput", "Failed", "Unrecoverable", "Finished"}
FAILED = {"Failed", "Unrecoverable"}


def turn_ended(entries: list[dict[str, Any]]) -> bool:
    """Whether the transcript contains a completed turn.

    `TurnEnded` is a real entry on `GET /sessions/{id}/messages` -- a
    `Lifecycle` body whose `value.kind` is `TurnEnded`. That makes it the true
    turn boundary, which session *status* is not: a session is created
    `Provisioning`, and `Idle` means both "has not started" and "has finished",
    so waiting on status alone returns before the agent has read a single file.
    """
    for entry in entries:
        body = entry.get("body") or {}
        if body.get("type") != "Lifecycle":
            continue
        if (body.get("value") or {}).get("kind") == "TurnEnded":
            return True
    return False


def wait_for_turn(
    h: Horsie,
    session_id: str,
    *,
    timeout_s: float,
    poll_s: float = 5.0,
    on_poll=None,
) -> tuple[str, dict[str, Any]]:
    """Block until the session's turn is over. Returns `(status, detail)`.

    Status is polled because it is the cheap call, but it never *decides*
    anything alone: a terminal-looking status is believed only once the
    transcript actually contains a `TurnEnded`. A failed session is terminal
    either way -- there may be no turn to end.

    `status` is `"Timeout"` if the deadline passed with no completed turn.
    """
    deadline = time.monotonic() + timeout_s
    detail: dict[str, Any] = {}

    while time.monotonic() < deadline:
        detail = h.get_session(session_id)
        status = detail.get("status", "")
        if on_poll is not None:
            on_poll(status, detail)

        if status in FAILED:
            return status, detail

        if status in TERMINAL:
            try:
                page = h.read_messages(session_id, max_entries=1000)
                if turn_ended((page or {}).get("entries") or []):
                    return status, detail
            except HorsieError:
                pass  # transient read failure: keep waiting rather than lying

        time.sleep(poll_s)

    return "Timeout", detail


def last_assistant_text(h: Horsie, session_id: str) -> str:
    """The text of the newest assistant message, or "" if there is none.

    Walks the structure rather than pattern-matching the JSON. An assistant
    entry is `body.type == "Llm"` with `body.value.role == "Assistant"` --
    capitalised, which an earlier lowercase check missed silently. Its `parts`
    are tagged, and only `Text` parts are the reply: `Thinking` and `ToolCall`
    parts also carry a `text` field, so anything that greps for `"text"` returns
    the model's reasoning instead of its answer.
    """
    try:
        page = h.read_messages(session_id, max_entries=1000)
    except HorsieError:
        return ""
    for entry in reversed((page or {}).get("entries") or []):
        body = entry.get("body") or {}
        if body.get("type") != "Llm":
            continue
        msg = body.get("value") or {}
        if msg.get("role") != "Assistant":
            continue
        text = "".join(
            (part.get("value") or {}).get("text") or ""
            for part in msg.get("parts") or []
            if part.get("type") == "Text"
        )
        if text:
            return text
    return ""


def usage_of(detail: dict[str, Any]) -> dict[str, int]:
    """Token usage as plain ints, with absent fields as 0.

    Cache fields are genuinely absent on some providers rather than zero -- the
    ChatGPT/codex backend reports input and output only. Callers that care about
    the difference should check the raw `usageTotal`; callers that just want
    numbers get zeros.
    """
    u = detail.get("usageTotal") or {}
    return {
        "input_tokens": u.get("inputTokens") or 0,
        "output_tokens": u.get("outputTokens") or 0,
        "cache_read_tokens": u.get("cacheReadTokens") or 0,
        "cache_creation_tokens": u.get("cacheCreationTokens") or 0,
    }


def reports_cache(detail: dict[str, Any]) -> bool:
    """Whether the provider reported cache accounting at all.

    Absent cache numbers and a genuine 0% hit rate are different findings and
    only one is a problem, so they must not collapse into the same value.
    """
    u = detail.get("usageTotal") or {}
    return "cacheReadTokens" in u or "cacheCreationTokens" in u
