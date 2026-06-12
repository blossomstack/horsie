#!/usr/bin/env python3
"""Mock GitHub upstream for the hackamore demo (stdlib only, no deps).

Listens on 127.0.0.1:9999 and logs every request — method, path, and the
Authorization header — to stdout. Its whole purpose is to PROVE credential
injection: when hackamore allows a request, the Authorization line printed here
must show the REAL upstream credential ("Bearer ghp_DEMO_REAL_TOKEN_do_not_use",
injected by hackamore from its vault), and NEVER the opaque per-job hackamore token
the sandboxed agent actually holds. If you ever see a hackamore token in this log,
the demo is broken.

Responses:
  POST .../pulls  -> 201 {"number": 1, "html_url": "http://mock/pr/1"}
  anything else   -> 200 {}

Usage: python3 examples/hackamore-demo/mock-github.py
"""

import json
from http.server import BaseHTTPRequestHandler, HTTPServer

HOST = "127.0.0.1"
PORT = 9999


class MockGitHub(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _handle(self) -> None:
        # Consume the body (if any) so keep-alive connections stay in sync.
        length = int(self.headers.get("Content-Length") or 0)
        if length:
            self.rfile.read(length)

        auth = self.headers.get("Authorization", "<no Authorization header>")
        print(f"{self.command} {self.path}  Authorization: {auth}", flush=True)

        if self.command == "POST" and self.path.split("?")[0].rstrip("/").endswith("/pulls"):
            status = 201
            body = json.dumps({"number": 1, "html_url": "http://mock/pr/1"}).encode()
        else:
            status = 200
            body = b"{}"

        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    do_GET = _handle
    do_POST = _handle
    do_PUT = _handle
    do_PATCH = _handle
    do_DELETE = _handle
    do_HEAD = _handle

    def log_message(self, fmt, *args):  # noqa: D102 - silence the default stderr log
        pass


def main() -> None:
    server = HTTPServer((HOST, PORT), MockGitHub)
    print(f"mock-github listening on http://{HOST}:{PORT}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
