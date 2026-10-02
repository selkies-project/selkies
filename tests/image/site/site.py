#!/usr/bin/env python3
"""The tester site the image tier serves inside the session, on loopback.

Pages in this directory are opened by a browser running on the session's own
desktop; each POSTs what it saw to /report/<name>, which lands in
<root>/out/<name>.json for the harness to read through the target's shell.
Every page polls /next/<channel> and follows it, so the harness moves a
session browser from page to page by writing <root>/next-<channel>.

    python3 site.py ROOT [PORT]
"""
import json
import os
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.realpath(sys.argv[1])
SITE = os.path.join(ROOT, "site")
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8765
# Channel and report names: plain words only, never a path.
NAME = re.compile(r"[A-Za-z0-9_-]{1,64}")
TYPES = {".html": "text/html", ".js": "text/javascript", ".png": "image/png",
         ".webm": "video/webm", ".ogg": "audio/ogg", ".json": "application/json"}


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
            ch = path[len("/next/"):]
            if not NAME.fullmatch(ch):
                return self.reply(404)
            try:
                with open(os.path.join(ROOT, f"next-{ch}"), "rb") as f:
                    body = f.read().strip()
            except OSError:
                body = b""
            return self.reply(200, body)
        full = os.path.realpath(os.path.join(SITE, path.lstrip("/") or "pattern.html"))
        if not full.startswith(SITE + os.sep) or not os.path.isfile(full):
            return self.reply(404)
        with open(full, "rb") as f:
            self.reply(200, f.read(), TYPES.get(os.path.splitext(full)[1], "application/octet-stream"))

    def do_POST(self):
        name = self.path[len("/report/"):] if self.path.startswith("/report/") else ""
        if not NAME.fullmatch(name):
            return self.reply(404)
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        try:
            json.loads(body)
        except ValueError:
            return self.reply(400)
        os.makedirs(os.path.join(ROOT, "out"), exist_ok=True)
        tmp = os.path.join(ROOT, "out", f"{name}.{threading.get_ident()}.tmp")
        with open(tmp, "wb") as f:
            f.write(body)
        os.replace(tmp, os.path.join(ROOT, "out", name + ".json"))
        self.reply(204)


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
