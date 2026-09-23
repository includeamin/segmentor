#!/usr/bin/env python3
"""A minimal mapper for local development and demos.

Implements just enough of the wire protocol in docs/mapper-api.md for segmentor to resolve
assets against it: GET /v1/health and GET /v1/assets/{id}. It answers from catalog.json, read
fresh on every request, so editing that file (or the mount in docker-compose.dev.yml) and waiting
out the short TTL below is enough to see a change without restarting anything.

This is not a mapper you should run in production: no TLS, no authentication, no caching, and a
short reuse window that would be wasteful at real traffic. It exists to show the shape of a real
one and to give docker-compose.dev.yml something to resolve assets against.
"""

import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

PORT = int(os.environ.get("PORT", "9911"))
CATALOG_PATH = Path(os.environ.get("CATALOG_PATH", Path(__file__).parent / "catalog.json"))
TTL_SECONDS = 5  # short, so an edited catalog is visible almost immediately


def load_catalog() -> dict:
    with CATALOG_PATH.open("rb") as handle:
        return json.load(handle)


class Mapper(BaseHTTPRequestHandler):
    def log_message(self, format: str, *args) -> None:  # noqa: A002 - matches the base signature
        sys.stderr.write(f"mapper: {self.address_string()} {format % args}\n")

    def do_GET(self) -> None:  # noqa: N802 - required name for BaseHTTPRequestHandler
        if self.path == "/v1/health":
            self.send_response(200)
            self.end_headers()
            return

        prefix = "/v1/assets/"
        if not self.path.startswith(prefix):
            self.send_response(404)
            self.end_headers()
            return
        asset_id = self.path[len(prefix):]

        try:
            catalog = load_catalog()
        except (OSError, json.JSONDecodeError) as error:
            self.send_response(500)
            self.end_headers()
            self.wfile.write(f"catalog.json: {error}\n".encode())
            return

        entry = catalog.get(asset_id)
        if entry is None:
            self.send_response(404)
            self.end_headers()
            return

        answer = {
            "asset_id": asset_id,
            "version": entry["version"],
            "ttl_seconds": TTL_SECONDS,
            "location": entry["location"],
        }
        if "subtitles" in entry:
            answer["subtitles"] = entry["subtitles"]
        body = json.dumps(answer).encode()

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    load_catalog()  # fail fast on a broken catalog rather than on the first request
    print(f"mapper: serving {CATALOG_PATH} on 0.0.0.0:{PORT}", flush=True)
    HTTPServer(("0.0.0.0", PORT), Mapper).serve_forever()
