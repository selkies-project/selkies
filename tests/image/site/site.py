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
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = sys.argv[1]
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8765
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
            try:
                body = open(os.path.join(ROOT, "next-" + os.path.basename(path)), "rb").read().strip()
            except OSError:
                body = b""
            return self.reply(200, body)
        name = os.path.normpath(path.lstrip("/") or "pattern.html")
        full = os.path.join(ROOT, "site", name)
        if name.startswith("..") or not os.path.isfile(full):
            return self.reply(404)
        with open(full, "rb") as f:
            self.reply(200, f.read(), TYPES.get(os.path.splitext(name)[1], "application/octet-stream"))

    def do_POST(self):
        if not self.path.startswith("/report/"):
            return self.reply(404)
        name = os.path.basename(self.path[len("/report/"):]) or "report"
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
