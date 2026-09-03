#!/usr/bin/env python3
"""The file-manager listing carries a parent link below the root, never at it.

The dashboard's Download Files iframe navigates by the listing's own
anchors. Below the root a listing without a parent row strands the user in
the subdirectory; at the root a parent row is an escape hatch out of the
file tree entirely. On the nginx `/files/` mount its `../` resolves to the
desktop page, whose load registers a fresh primary client and kills the
running session (linuxserver/docker-baseimage-selkies#131 documents that);
on `/api/files/` it merely 404s at `/api/`, wrong either way. So the
server renders the row itself: exactly one below the root, none at it,
and the traversal gates in front stay as they are.
"""
import asyncio
import os
import re
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]
_SCRATCH = os.path.realpath(tempfile.mkdtemp(prefix="selkies-file-index-"))
os.environ["SELKIES_FILE_MANAGER_PATH"] = _SCRATCH

import pathlib  # noqa: E402

from aiohttp import web  # noqa: E402
from aiohttp.test_utils import TestClient, TestServer  # noqa: E402

from selkies.stream_server import CentralizedStreamServer  # noqa: E402

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
    file_transfer_cc = False


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

    # The client-side half of the contract, pinned structurally in BOTH copies
    # of the shared footer script: the mount lookup must run on the normalized
    # path (both needles end in a slash, so the raw path never matches a
    # slashless root), and the root test must be an exact-length match, not
    # endsWith, which also fired inside any subdirectory itself named to end
    # in the mount (/files/backup/files/).
    footer_copies = (
        ("nginx footer", os.path.join(REPO, "addons", "selkies-web-core",
                                      "nginx", "footer.html")),
        ("python footer", os.path.join(REPO, "src", "selkies",
                                       "stream_server.py")),
    )
    for label, path in footer_copies:
        with open(path, encoding="utf-8") as fh:
            text = fh.read()
        check("{} searches both mounts on the normalized path".format(label),
              "normPath.indexOf('/api/files/')" in text
              and "normPath.indexOf('/files/')" in text)
        check("{} lets the outermost mount win".format(label),
              "idxLegacy !== -1 && (idxApi === -1 || idxLegacy < idxApi)" in text)
        check("{} detects the root by exact length".format(label),
              "idx + webPathPrefix.length === normPath.length" in text)
        check("{} removes the root parent row".format(label),
              "parentRow.remove()" in text)

    print(f"[file-index-parent] {passed}/{passed + failed} passed", flush=True)
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if asyncio.run(main()) else 1)
