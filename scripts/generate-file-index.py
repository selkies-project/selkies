#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Write the nginx fancyindex fragments from the file index Selkies serves.

Selkies serves the directory listing itself, but a deployment that fronts it
with nginx (the LinuxServer.io images do) renders the same listing through
fancyindex, which takes a header and a footer fragment. Those are this page's
two halves verbatim, so they are generated rather than kept by hand: the copies
drifted apart once already, leaving the proxied listing on a palette the
dashboards had stopped using.

Usage:
    python3 scripts/generate-file-index.py          # rewrite the fragments
    python3 scripts/generate-file-index.py --check  # exit 1 when they are stale
"""
import argparse
import ast
import os
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
NGINX = os.path.join(REPO, "addons", "selkies-web-core", "nginx")
SERVER = os.path.join(REPO, "src", "selkies", "stream_server.py")


def constants() -> dict:
    """The two page halves, read from the source rather than imported.

    Importing the module would pull in aiohttp, which the lint gate and the
    pre-commit hook that run this have no reason to install.
    """
    found = {}
    for node in ast.parse(open(SERVER, encoding="utf-8").read()).body:
        target = None
        if isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
            target = node.target.id
        elif isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            target = node.targets[0].id
        if target in ("FILE_INDEX_HEADER", "FILE_INDEX_FOOTER") and isinstance(node.value, ast.Constant):
            found[target] = node.value.value
    missing = {"FILE_INDEX_HEADER", "FILE_INDEX_FOOTER"} - set(found)
    if missing:
        raise SystemExit(f"{os.path.relpath(SERVER, REPO)} no longer defines {', '.join(sorted(missing))}")
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true",
                        help="exit 1 when a fragment is out of date instead of rewriting it")
    args = parser.parse_args()

    found = constants()
    stale = []
    for name, key in (("header.html", "FILE_INDEX_HEADER"), ("footer.html", "FILE_INDEX_FOOTER")):
        body = found[key]
        path = os.path.join(NGINX, name)
        current = open(path, encoding="utf-8").read() if os.path.exists(path) else None
        if current == body:
            continue
        if args.check:
            stale.append(os.path.relpath(path, REPO))
            continue
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(body)
        print(f"wrote {os.path.relpath(path, REPO)}")
    if stale:
        print("stale, rerun scripts/generate-file-index.py: " + ", ".join(stale), file=sys.stderr)
        return 1
    if args.check:
        print("the nginx fancyindex fragments are up to date")
    return 0


if __name__ == "__main__":
    sys.exit(main())
