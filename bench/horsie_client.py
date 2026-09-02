"""A thin, dependency-free client for the horsie server API.

Only the calls the benchmark pilot needs. Everything goes through
``/api/p/{project}`` except the vendor routes, which are project-scoped too --
the whole management surface lives under that prefix.

Two wire facts this file exists to encode, because getting either wrong fails
silently rather than loudly:

* **Bodies are camelCase.** A snake_case key is not rejected, it is ignored, so
  a mis-spelled ``workspaceRoot`` reads as "field absent" and the machine comes
  up without the directory the agent was told to work in.
* **Unions are adjacently tagged** as ``{"kind"|"type": Variant, "value": {...}}``
  with a *capitalised* variant name. A flattened body deserialises to nothing.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any


class HorsieError(RuntimeError):
    def __init__(self, status: int, body: str, method: str, path: str) -> None:
        super().__init__(f"{method} {path} -> {status}: {body}")
        self.status = status
        self.body = body


@dataclass
class Horsie:
    """One project on one server."""

    base_url: str
    token: str
    project: str = "default"
    timeout: float = 60.0

    # ---------------------------------------------------------------- plumbing

    def _request(self, method: str, path: str, body: Any | None = None) -> Any:
        url = f"{self.base_url.rstrip('/')}/api/p/{self.project}{path}"
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(url, data=data, method=method)
        req.add_header("Authorization", f"Bearer {self.token}")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                raw = resp.read()
        except urllib.error.HTTPError as e:
            raise HorsieError(e.code, e.read().decode(errors="replace"), method, path) from None
        return json.loads(raw) if raw else None

    # ----------------------------------------------------------------- vendors

    def save_vendor_velos(
        self,
        name: str,
        *,
        server_url: str,
        image: str,
        callback_url: str,
        credential: str | None,
        cpu: int = 2,
        memory_mb: int = 4096,
        runtime_bin: str = "/usr/local/bin/horsie-runtime",
        workspace_root: str = "/workspaces",
    ) -> Any:
        """Create or fully replace a velos vendor. ``PUT`` is the only verb: a
        vendor row is a connection setting keyed by name, so re-saving is how a
        rotated token is applied.

        ``callback_url`` is the ``ws://`` URL a container reaches the horsie
        server on **from velos's container network** -- not necessarily the
        address a browser uses.

        ``credential`` may be ``None``: a velos deployment running without auth
        need not serve ``/auth/v1/me`` at all, and the vendor treats its 404 as
        "no auth here" rather than "your token is wrong".
        """
        return self._request(
            "PUT",
            f"/runtime-vendors/{name}",
            {
                "name": name,
                "credential": credential,
                "settings": {
                    "kind": "Velos",
                    "value": {
                        "serverUrl": server_url,
                        "image": image,
                        "runtimeBin": runtime_bin,
                        "workspaceRoot": workspace_root,
                        "callbackUrl": callback_url,
                        "cpu": cpu,
                        "memoryMb": memory_mb,
                    },
                },
            },
        )

    def test_vendor(self, name: str) -> Any:
        """Ask the substrate whether this vendor is usable, without creating
        anything. A substrate saying no is ``ok: false``, not an HTTP error."""
        return self._request("POST", f"/runtime-vendors/{name}/test", {})

    def delete_vendor(self, name: str) -> None:
        self._request("DELETE", f"/runtime-vendors/{name}")

    # ---------------------------------------------------------------- sessions

    def create_session(
        self,
        *,
        message: str,
        model: str,
        vendor: str,
        name: str | None = None,
        max_iterations: int | None = None,
        thinking_effort: str | None = None,
    ) -> str:
        """Create a session with its first message and return the session id.

        There is no create-then-message shape: a session with no message is a
        provisioned runtime nobody asked a question. The repo is *not* cloned
        via ``repos`` here -- the benchmark image already contains the checkout
        at the right commit, which is the whole point of a per-task image.
        """
        agent: dict[str, Any] = {"model": model}
        if max_iterations is not None:
            agent["maxIterations"] = max_iterations
        if thinking_effort is not None:
            agent["thinkingEffort"] = thinking_effort

        body: dict[str, Any] = {
            "message": message,
            "agent": agent,
            "environment": {"type": "Runtime", "value": {"vendor": vendor}},
        }
        if name is not None:
            body["name"] = name
        return self._request("POST", "/sessions", body)["session"]["id"]

    def get_session(self, session_id: str) -> dict[str, Any]:
        return self._request("GET", f"/sessions/{session_id}")["session"]

    def delete_session(self, session_id: str) -> None:
        self._request("DELETE", f"/sessions/{session_id}")

    def send_message(self, session_id: str, text: str) -> Any:
        return self._request("POST", f"/sessions/{session_id}/messages", {"message": text})

    def read_messages(self, session_id: str, *, max_entries: int = 200) -> Any:
        return self._request("GET", f"/sessions/{session_id}/messages?max={max_entries}")
