#!/usr/bin/env python3
"""The file-manager listing carries a parent link below the root, never at it.

The dashboard's Download Files iframe navigates by the listing's own
anchors. Below the root a listing without a parent row strands the user in
the subdirectory; at the root a parent row is an escape hatch out of the
file tree entirely. On the nginx `/files/` mount its `../` resolves to the
desktop page, whose load registers a fresh primary client and kills the
running session; on `/api/files/` it merely 404s at `/api/`, wrong either
way. So the server renders the row itself: exactly one below the root, none
at it, and the traversal gates in front stay as they are.

The nginx mount renders the row unconditionally and leaves it to the shared
footer script, so the script's behaviour is pinned as well, on both of its
copies through `tests/tools/file_index_footer_audit.mjs`: the root decision
on every mount shape, a sorted listing keeping its directories navigable,
and the session token joining a query already there.
"""
import asyncio
import os
import re
import shutil
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
AUDIT = os.path.join(TESTS, "tools", "file_index_footer_audit.mjs")
sys.path.insert(0, os.path.join(REPO, "src"))

for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]
_SCRATCH = os.path.realpath(tempfile.mkdtemp(prefix="selkies-file-index-"))
os.environ["SELKIES_FILE_MANAGER_PATH"] = _SCRATCH

import pathlib  # noqa: E402

from aiohttp import web  # noqa: E402
from aiohttp.test_utils import TestClient, TestServer  # noqa: E402

from selkies.stream_server import FILE_INDEX_FOOTER, CentralizedStreamServer  # noqa: E402

passed = failed = 0


def check(name, ok, detail=""):
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(("PASS" if ok else "FAIL") + "  [file-index-parent] {}  {}".format(name, detail),
          flush=True)


class _Settings:
    file_transfers = ["download"]
    file_transfer_limit_mbps = 0.0


def _app():
    server = CentralizedStreamServer.__new__(CentralizedStreamServer)
    server.settings = _Settings()
    server.upload_dir = pathlib.Path(_SCRATCH)
    app = web.Application()
    app.router.add_get("/api/files/{path:.*}", server.fancy_index_handler)
    return TestClient(TestServer(app))


PARENT_ROW = re.compile(r'<a href="\.\./"')


async def main():
    os.makedirs(os.path.join(_SCRATCH, "sub", "inner"), exist_ok=True)
    with open(os.path.join(_SCRATCH, "sub", "a.txt"), "w", encoding="utf-8") as fh:
        fh.write("x")

    client = _app()
    await client.start_server()
    try:
        resp = await client.get("/api/files/")
        body = await resp.text()
        check("root listing answers 200", resp.status == 200, str(resp.status))
        check("root listing has no parent row", not PARENT_ROW.search(body))
        check("root listing shows the subdirectory", '<a href="sub/">' in body)

        resp = await client.get("/api/files/sub/")
        body = await resp.text()
        check("subdirectory listing answers 200", resp.status == 200, str(resp.status))
        check("subdirectory listing has exactly one parent row",
              len(PARENT_ROW.findall(body)) == 1, len(PARENT_ROW.findall(body)))
        check("parent row precedes the entries",
              body.find('href="../"') < body.find("a.txt"))

        resp = await client.get("/api/files/sub/inner/")
        body = await resp.text()
        check("nested listing has exactly one parent row",
              len(PARENT_ROW.findall(body)) == 1, len(PARENT_ROW.findall(body)))

        resp = await client.get("/api/files/sub", allow_redirects=False)
        check("slashless directory redirects to its slash form",
              resp.status in (301, 302, 307, 308)
              and resp.headers.get("Location", "").endswith("/sub/"),
              "{} {}".format(resp.status, resp.headers.get("Location")))

        resp = await client.get("/api/files/..%2f", allow_redirects=False)
        check("traversal stays refused", resp.status in (400, 403), resp.status)
    finally:
        await client.close()

    # The client-side half of the contract: the two copies of the footer
    # script carry the same listing function, and it behaves as the audit
    # says. The Python copy is read as the string the server serves, since
    # its source escapes what the file does not.
    python_footer = os.path.join(_SCRATCH, "python-footer.html")
    with open(python_footer, "w", encoding="utf-8") as fh:
        fh.write(FILE_INDEX_FOOTER)
    footer_copies = (
        ("nginx footer", os.path.join(REPO, "addons", "selkies-web-core",
                                      "nginx", "footer.html")),
        ("python footer", python_footer),
    )
    bodies = []
    for _label, path in footer_copies:
        with open(path, encoding="utf-8") as fh:
            text = fh.read()
        start = text.find("function processDirectoryListing()")
        end = text.find("let attempts = 0;", start)
        bodies.append(text[start:end] if start != -1 and end != -1 else None)
    check("both footer copies carry the same listing function",
          bodies[0] is not None and bodies[0] == bodies[1])
    node = shutil.which("node")
    if not node:
        print("SKIP node not found, so the footer script is not exercised", flush=True)
    for label, path in footer_copies if node else ():
        r = subprocess.run([node, AUDIT, path], capture_output=True, text=True, timeout=120)
        for line in r.stdout.splitlines():
            if line.startswith(("PASS", "FAIL")):
                check(line[6:].replace("[file-index-footer]", label + ":", 1).strip(),
                      line.startswith("PASS"))
        check("{} audit exits clean".format(label), r.returncode == 0, (r.stderr or "")[-300:])

    print(f"[file-index-parent] {passed}/{passed + failed} passed", flush=True)
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if asyncio.run(main()) else 1)
