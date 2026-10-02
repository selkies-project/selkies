#!/usr/bin/env python3
"""The tester site the image tier serves inside the session, on loopback.

Pages in this directory are opened by a browser running on the session's own
desktop; each POSTs what it saw to /report/<name>, which lands in
<root>/out/<name>.json for the harness to read through the target's shell.
Every page polls /next/<channel> and follows it, so the harness moves a
session browser from page to page by writing <root>/next-<channel>.

A request only ever picks an entry out of a listing of what is there (the
site's files, the channel files) or out of the fixed set of report names; no
path is built from it.

    python3 site.py ROOT [PORT]
"""
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.realpath(sys.argv[1])
SITE = os.path.join(ROOT, "site")
OUT = os.path.join(ROOT, "out")
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8765
TYPES = {".html": "text/html", ".js": "text/javascript", ".png": "image/png",
         ".webm": "video/webm", ".ogg": "audio/ogg", ".json": "application/json"}
# What the pages report, each to a file of its own.
REPORTS = {name: os.path.join(OUT, f"{name}.json") for name in
           ("clip", "gamepad", "ime", "media", "pattern", "play", "pointer", "texture", "tone")}


def listing(directory: str, prefix: str = "") -> dict:
    """The regular files in `directory` named `prefix`..., by the rest of their name."""
    try:
        return {e.name[len(prefix):]: e.path for e in os.scandir(directory)
                if e.is_file() and e.name.startswith(prefix)}
    except OSError:
        return {}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, code, body=b"", kind="text/plain"):
        self.send_response(code)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path.startswith("/next/"):
            target = listing(ROOT, "next-").get(path[len("/next/"):])
            body = b""
            if target:
                with open(target, "rb") as f:
                    body = f.read().strip()
            return self.reply(200, body)
        full = listing(SITE).get(path.lstrip("/") or "pattern.html")
        if not full:
            return self.reply(404)
        with open(full, "rb") as f:
            self.reply(200, f.read(), TYPES.get(os.path.splitext(full)[1], "application/octet-stream"))

    def do_POST(self):
        dest = REPORTS.get(self.path[len("/report/"):]) if self.path.startswith("/report/") else None
        if not dest:
            return self.reply(404)
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        try:
            json.loads(body)
        except ValueError:
            return self.reply(400)
        os.makedirs(OUT, exist_ok=True)
        tmp = f"{dest}.{threading.get_ident()}.tmp"
        with open(tmp, "wb") as f:
            f.write(body)
        os.replace(tmp, dest)
        self.reply(204)


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
