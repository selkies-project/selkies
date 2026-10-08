#!/usr/bin/env python3
"""A session on a proot-apps local repository lists that repository.

With `PA_REPO_FOLDER` set, proot-apps installs only what that folder holds, so
the server serves the folder's catalog under `/api/apps/` and tells the page
to read it there (`apps_local_repo` in the settings payload) instead of the
remote one. The folder reaches the server through its own setting as well as
through the runner's variable, and the server hands it to the runner either
way, so the panel and the runner never read two repositories. Without a
repository nothing is served and the page reads the remote catalog.

Driven against a stand-in `selkies-proot` that records the environment its
`check` ran in, and a repository laid out in the work directory.
"""
import asyncio
import json
import os
import shutil
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets

METADATA = ("include:\n  - name: localapp\n    full_name: Local App\n"
            "    description: From the local repository\n    icon: localapp.svg\n")
ICON = "<svg xmlns='http://www.w3.org/2000/svg'/>"


def runner_stub(directory: str, env_file: str) -> str:
    """Write a `selkies-proot` stand-in whose `check` records its environment."""
    os.makedirs(directory, exist_ok=True)
    path = os.path.join(directory, "selkies-proot")
    with open(path, "w") as fh:
        fh.write('#!/bin/sh\n'
                 f'if [ "$1" = check ]; then env > "{env_file}"; fi\n'
                 'exit 0\n')
    os.chmod(path, 0o755)
    return directory


def repository(folder: str) -> str:
    """Lay out a repository with one application and its catalog."""
    shutil.rmtree(folder, ignore_errors=True)
    os.makedirs(os.path.join(folder, "ghcr.io_linuxserver_proot-apps_localapp"))
    os.makedirs(os.path.join(folder, "metadata", "img"))
    with open(os.path.join(folder, "metadata", "metadata.yml"), "w") as fh:
        fh.write(METADATA)
    with open(os.path.join(folder, "metadata", "img", "localapp.svg"), "w") as fh:
        fh.write(ICON)
    return folder


def get(path: str) -> "tuple":
    """Status and body of a GET on the server, 0 and the reason when refused."""
    try:
        with urllib.request.urlopen(f"{H.BASE_URL}{path}", timeout=10) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, ""


async def published_setting(name: str) -> object:
    """One value of the connect-time settings payload over WebSockets."""
    async with websockets.connect(f"ws://localhost:{H.PORT}/api/websockets", max_size=None) as ws:
        for _ in range(20):
            msg = await asyncio.wait_for(ws.recv(), timeout=10)
            if isinstance(msg, str) and msg.startswith("{"):
                obj = json.loads(msg)
                if obj.get("type") == "server_settings":
                    return obj["settings"].get(name, {}).get("value")
    return None


def run() -> "H.Results":
    res = H.Results("apps-local-repo")
    env_file = os.path.join(H.WORKDIR, "apps-local-repo-env")
    stub = runner_stub(os.path.join(H.WORKDIR, "apps-local-repo-bin"), env_file)
    folder = repository(os.path.join(H.WORKDIR, "apps-local-repo"))
    path = stub + os.pathsep + os.environ.get("PATH", "")
    if os.path.exists(env_file):
        os.unlink(env_file)

    # The Selkies setting alone, so the server has to hand the folder to the runner.
    H.server_start(mode="websockets", wayland=False,
                   extra_env={"PATH": path, "SELKIES_COMMAND_ENABLED": "true",
                              "SELKIES_APPS_REPO_FOLDER": folder})
    try:
        # The runner is probed at startup, which the status endpoint does not wait for.
        deadline = time.time() + 15
        while not os.path.exists(env_file) and time.time() < deadline:
            time.sleep(0.2)
        recorded = open(env_file).read() if os.path.exists(env_file) else ""
        res.check("the runner is handed the repository as PA_REPO_FOLDER",
                  f"PA_REPO_FOLDER={folder}\n" in recorded, recorded[:200])
        res.check("the page is told the catalog is local",
                  asyncio.run(published_setting("apps_local_repo")) is True)
        status, body = get("/api/apps/metadata.yml")
        res.check("the repository's catalog is served", status == 200 and body == METADATA, (status, body[:80]))
        status, body = get("/api/apps/img/localapp.svg")
        res.check("its icon is served", status == 200 and body == ICON, (status, body[:80]))
        status, _ = get("/api/apps/img/..%2Fmetadata.yml")
        res.check("a path in place of an icon name is refused", status == 404, status)
        status, _ = get("/api/apps/img/localapp.svg%00.png")
        res.check("as is a name with a NUL byte in it", status == 404, status)
        status, _ = get("/api/apps/img/absent.svg")
        res.check("an icon the repository lacks is not found", status == 404, status)
        res.check("the folder is logged as the catalog's source",
                  f"catalog from the local repository {folder} (1 application)." in H.server_log())
    finally:
        H.server_stop()

    # proot-apps' own variable is read as well.
    if os.path.exists(env_file):
        os.unlink(env_file)
    H.server_start(mode="websockets", wayland=False,
                   extra_env={"PATH": path, "SELKIES_COMMAND_ENABLED": "true",
                              "PA_REPO_FOLDER": folder})
    try:
        res.check("PA_REPO_FOLDER alone names the repository",
                  asyncio.run(published_setting("apps_local_repo")) is True
                  and get("/api/apps/metadata.yml")[0] == 200)
    finally:
        H.server_stop()

    H.server_start(mode="websockets", wayland=False,
                   extra_env={"PATH": path, "SELKIES_COMMAND_ENABLED": "true"})
    try:
        res.check("without a repository the page reads the remote catalog",
                  asyncio.run(published_setting("apps_local_repo")) is False)
        res.check("and nothing is served here", get("/api/apps/metadata.yml")[0] == 404)
    finally:
        H.server_stop()
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(1 if run().failed() else 0)
