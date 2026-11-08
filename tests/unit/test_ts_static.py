#!/usr/bin/env python3
"""The browser suites get the web client's TypeScript modules as JavaScript.

`tests/ts_static.py` serves a `.ts` file with its types stripped, because the
pages under `tests/tools` import `lib/input.ts` straight from the repository
and a browser runs only JavaScript. What is held here: a module's types are
gone and its code is untouched, it arrives as a JavaScript module type, a
declaration file and a plain file are served as they are, a module Node cannot
strip is an error and not a half-served page, and every module the client's
input handler imports, to the end of the chain, comes back as JavaScript.

Usage: python3 tests/unit/test_ts_static.py
"""
import functools
import http.server
import os
import re
import sys
import tempfile
import threading
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "tests"))
from ts_static import TypeScriptHandler, strip_types  # noqa: E402

CORE = os.path.join(ROOT, "web", "selkies-web-core")
TIMEOUT = 30

results = []


def check(label: str, ok: object, detail: object = "") -> None:
    """Record and print one check."""
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [ts-static] {label}  {detail}", flush=True)


class Quiet(TypeScriptHandler):
    """The handler under test, without a log line per request."""

    def log_message(self, *args) -> None:
        pass


def serve(directory: str) -> http.server.ThreadingHTTPServer:
    """Start a server on `directory` on a daemon thread."""
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Quiet, directory=directory))
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def get(server: http.server.ThreadingHTTPServer, path: str) -> tuple:
    """(status, content type, body) of `path`; an error status is returned, not raised."""
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{server.server_port}{path}", timeout=TIMEOUT) as r:
            return r.status, r.headers.get("Content-Type", ""), r.read().decode("utf-8")
    except urllib.error.HTTPError as err:
        return err.code, err.headers.get("Content-Type", ""), err.read().decode("utf-8", "replace")


def imports_of(path: str, body: str) -> list:
    """The relative modules `body` imports, as server paths."""
    base = os.path.dirname(path)
    found = re.findall(r"""(?:from|import)\s*\(?\s*['"](\.{1,2}/[^'"]+)['"]""", body)
    return [os.path.normpath(os.path.join(base, rel)).replace(os.sep, "/") for rel in found]


def main() -> bool:
    source = "export const n: number = 1;\nexport function f(a: string, b?: number): string { return a + (b ?? 0); }\n"
    stripped = strip_types(source)
    check("types are gone", ": number" not in stripped and ": string" not in stripped and "b?:" not in stripped, repr(stripped))
    check("the code is as written", "export const n" in stripped and "return a + (b ?? 0);" in stripped, repr(stripped))
    check("lines stay where they were", stripped.count("\n") == source.count("\n"))

    with tempfile.TemporaryDirectory() as tmp:
        for name, text in (("mod.ts", source), ("decl.d.ts", "export declare const x: number;\n"),
                           ("plain.txt", "hello\n"), ("bad.ts", "enum E { A }\nexport const e = E.A;\n")):
            with open(os.path.join(tmp, name), "w", encoding="utf-8", newline="") as fh:
                fh.write(text)
        server = serve(tmp)
        try:
            status, kind, body = get(server, "/mod.ts")
            check("a module is served stripped", status == 200 and ": number" not in body, status)
            check("as a JavaScript module type", kind.startswith("text/javascript"), kind)
            status, kind, body = get(server, "/decl.d.ts")
            check("a declaration file is served as it is", status == 200 and ": number" in body, status)
            status, kind, body = get(server, "/plain.txt")
            check("a plain file is served as it is", status == 200 and body == "hello\n" and kind.startswith("text/plain"), kind)
            status, _, body = get(server, "/bad.ts")
            check("a module needing a real transform is an error", status == 500, status)
            status, _, _ = get(server, "/missing.ts")
            check("a missing module is a 404", status == 404, status)
        finally:
            server.shutdown()
            server.server_close()

    server = serve(CORE)
    try:
        seen, todo, wrong = set(), ["/lib/input.ts"], []
        while todo:
            path = todo.pop()
            if path in seen:
                continue
            seen.add(path)
            status, kind, body = get(server, path)
            if status != 200 or not kind.startswith("text/javascript"):
                wrong.append((path, status, kind))
                continue
            todo.extend(imports_of(path, body))
        check("the input handler's whole import chain is served", not wrong and len(seen) > 1,
              f"{len(seen)} modules, wrong: {wrong}")
    finally:
        server.shutdown()
        server.server_close()

    failed = [label for label, ok in results if not ok]
    print(f"[ts-static] {len(results) - len(failed)}/{len(results)} passed")
    return not failed


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
