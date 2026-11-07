"""A static file handler for the browser suites that load the web client's own modules.

The web core is TypeScript and a browser runs only JavaScript, so a page that
imports `lib/input.ts` straight from the repository needs the types removed on
the way out. `TypeScriptHandler` serves every `.ts` file as the JavaScript its
types strip to, using Node's `stripTypeScriptTypes`, the same removal the Node
audits under `tests/tools` rely on when they import these modules. Types are
replaced by blanks rather than deleted, so line and column positions in a
stack trace stay those of the source.

Only stripping is done: a module that needs a real transform (an enum, a
namespace, a constructor parameter property) fails here and in the audits
alike, which keeps the client inside the subset both can run.
"""
import http.server
import io
import os
import shutil
import subprocess
import threading

# Reads one module on stdin and writes the stripped JavaScript to stdout.
_STRIP = (
    "const { stripTypeScriptTypes } = require('node:module');"
    "let s = '';"
    "process.stdin.setEncoding('utf8');"
    "process.stdin.on('data', (c) => { s += c; });"
    "process.stdin.on('end', () => { process.stdout.write(stripTypeScriptTypes(s)); });"
)


def strip_types(source: str) -> str:
    """The JavaScript a TypeScript module strips to.

    Args:
        source: The module's text.

    Returns:
        The same text with every type annotation blanked out.

    Raises:
        RuntimeError: Node is missing, or rejected the module.
    """
    node = shutil.which("node")
    if node is None:
        raise RuntimeError("node is not on PATH; it strips the types of the web client's modules")
    done = subprocess.run(
        [node, "--disable-warning=ExperimentalWarning", "-e", _STRIP],
        input=source, capture_output=True, text=True, encoding="utf-8", timeout=60)
    if done.returncode != 0:
        raise RuntimeError(done.stderr.strip() or f"node exited {done.returncode}")
    return done.stdout


class TypeScriptHandler(http.server.SimpleHTTPRequestHandler):
    """`SimpleHTTPRequestHandler` that serves a `.ts` file as JavaScript.

    A stripped module is kept by path and modification time, since a page
    imports dozens of them on every load. Declaration files (`.d.ts`) carry no
    code and are served as they are.
    """

    _cache: dict = {}
    _lock = threading.Lock()

    def send_head(self):
        """Answer a `.ts` request with its stripped source; anything else as the base class does."""
        path = self.translate_path(self.path)
        if not path.endswith(".ts") or path.endswith(".d.ts") or not os.path.isfile(path):
            return super().send_head()
        stamp = os.stat(path).st_mtime_ns
        with self._lock:
            hit = self._cache.get(path)
            if hit is None or hit[0] != stamp:
                try:
                    with open(path, encoding="utf-8") as fh:
                        hit = (stamp, strip_types(fh.read()).encode("utf-8"))
                except (OSError, RuntimeError, subprocess.SubprocessError) as err:
                    self.send_error(500, f"cannot strip types from {os.path.basename(path)}: {err}")
                    return None
                self._cache[path] = hit
        body = hit[1]
        self.send_response(200)
        self.send_header("Content-Type", "text/javascript; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        return io.BytesIO(body)
