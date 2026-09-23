#!/usr/bin/env python3
"""The desktop through Jupyter's server proxy, as `selkies[jupyter]` offers it.

The package registers a server process with jupyter-server-proxy, so a
Jupyter server lists a Selkies item and serves the desktop at /selkies/: it
starts the launcher on a Unix socket of the proxy's own and carries the page,
the API and the data WebSocket behind Jupyter's token. Proved in a browser,
where the client loads under the /selkies/ prefix and video flows through
the proxy. The desktop is a stand-in the server's data directories hold, so the
host's own never starts beside the one it runs. A Jupyter server killed
outright leaves no session behind, the stand-in's daemonized helper included:
the launcher follows its parent down.

Needs jupyter-server-proxy beside the interpreter that runs the server under
test, with selkies installed there rather than only importable, since the
proxy finds the desktop through the package's entry point.
"""
import json
import os
import secrets
import subprocess
import sys
import tempfile
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

LOG = os.path.join(H.WORKDIR, "jupyter.log")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Report a redirect instead of following it: Jupyter refuses by sending the browser to its login page."""

    def redirect_request(self, *args, **kwargs):
        return None


OPENER = urllib.request.build_opener(NoRedirect)


def get(url: str, token: str) -> tuple:
    request = urllib.request.Request(url, headers={"Authorization": f"token {token}"} if token else {})
    try:
        with OPENER.open(request, timeout=5) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as err:
        return err.code, b""
    except Exception:
        return -1, b""


def registered() -> bool:
    """Whether the server under test's interpreter carries the proxy and the entry point."""
    probe = ("import jupyter_server_proxy\nfrom importlib.metadata import entry_points\n"
             "eps = entry_points()\ngroup = eps.select(group='jupyter_serverproxy_servers') "
             "if hasattr(eps, 'select') else eps.get('jupyter_serverproxy_servers', [])\n"
             "raise SystemExit(0 if any(e.name == 'selkies' for e in group) else 3)")
    return subprocess.run([H.PYTHON, "-c", probe], capture_output=True).returncode == 0


def start_jupyter(token: str, runtime: str, wayland: bool) -> tuple:
    port = H._free_port()
    share = os.path.join(runtime, "share")
    for kind in ("xsessions", "wayland-sessions"):
        os.makedirs(os.path.join(share, kind))
        with open(os.path.join(share, kind, "standin.desktop"), "w") as fh:
            fh.write("[Desktop Entry]\nName=Stand-in\nExec=sh -c 'setsid sleep 3600 & exec sleep 3600'\n")
    env = {"PATH": os.environ.get("PATH", ""), "HOME": os.path.expanduser("~"), "XDG_RUNTIME_DIR": runtime,
           "XDG_DATA_DIRS": share, "XDG_DATA_HOME": share,
           "JUPYTER_CONFIG_DIR": os.path.join(runtime, "config"), "JUPYTER_DATA_DIR": os.path.join(runtime, "data"),
           "JUPYTER_RUNTIME_DIR": os.path.join(runtime, "jupyter"), "SELKIES_WEB_ROOT": H.CORE_DIST,
           "SELKIES_WAYLAND": "true" if wayland else "false"}
    proc = H.spawn([H.PYTHON, "-m", "jupyter_server", "--ServerApp.ip=127.0.0.1", f"--ServerApp.port={port}",
                    "--ServerApp.port_retries=0", f"--ServerApp.token={token}", f"--ServerApp.root_dir={runtime}",
                    "--ServerApp.open_browser=False"],
                   env=env, cwd=H.WORKDIR, stdout=open(LOG, "w"), stderr=subprocess.STDOUT, start_new_session=True)
    deadline = time.time() + 90
    while time.time() < deadline:
        if get(f"http://127.0.0.1:{port}/api/status", token)[0] == 200:
            return proc, port
        if proc.poll() is not None:
            raise RuntimeError(f"jupyter exited {proc.returncode}; see {LOG}")
        time.sleep(0.5)
    raise RuntimeError(f"jupyter did not answer within 90 s; see {LOG}")


def launcher_of(jupyter: subprocess.Popen):
    """The launcher the proxy started, as the child of the Jupyter server running selkies.session."""
    for pid in open(f"/proc/{jupyter.pid}/task/{jupyter.pid}/children").read().split():
        if "selkies.session" in H._cmdline(int(pid)):
            return int(pid)
    return None


def session_processes(runtime: str) -> list:
    """The processes whose runtime directory is a session's the launcher made under `runtime`."""
    marker = f"XDG_RUNTIME_DIR={runtime}/selkies-session-".encode()
    found = []
    for entry in os.listdir("/proc"):
        try:
            with open(f"/proc/{entry}/environ", "rb") as fh:
                if any(v.startswith(marker) for v in fh.read().split(b"\0")):
                    found.append(int(entry))
        except (OSError, ValueError):
            continue
    return found


def block(res: H.Results, wayland: bool) -> None:
    tag = "[wayland]" if wayland else "[x11]"
    token = secrets.token_hex(16)
    runtime = tempfile.mkdtemp(prefix="jupyter-", dir=H.WORKDIR)
    try:
        jupyter, port = start_jupyter(token, runtime, wayland)
    except RuntimeError as err:
        res.check(f"{tag} the Jupyter server starts", False, err)
        return
    res.check(f"{tag} the Jupyter server starts", True, port)
    base = f"http://127.0.0.1:{port}"
    launcher = None
    try:
        code, body = get(f"{base}/server-proxy/servers-info", token)
        servers = {s["name"]: s for s in json.loads(body or b"{}").get("server_processes", [])} if code == 200 else {}
        entry = servers.get("selkies", {})
        res.check(f"{tag} the proxy lists the Selkies desktop", entry.get("launcher_entry", {}).get("title") == "Selkies",
                  entry)
        res.check(f"{tag} the desktop opens in a tab of its own", entry.get("new_browser_tab") is True)
        with sync_playwright() as pw:
            browser = C.launch_browser(pw, "chromium")
            context = browser.new_context(viewport={"width": 1280, "height": 720})
            context.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
            page = context.new_page()
            errors = []
            page.on("pageerror", lambda e: errors.append(str(e)))
            try:
                page.goto(f"{base}/selkies/?token={token}", wait_until="load")
                res.check(f"{tag} the client loads under the proxy's prefix", page.url.startswith(f"{base}/selkies/"),
                          page.url)
                info = C.wait_ws_video(page, timeout=120)
                res.check(f"{tag} video flows through the proxy", info is not None, info)
                res.check(f"{tag} the page raised no error", not errors, errors[:2])
            finally:
                browser.close()
        code, body = get(f"{base}/selkies/api/status", token)
        mode = json.loads(body or b"{}").get("current_mode") if code == 200 else None
        res.check(f"{tag} the API answers behind the proxy, on the WebSocket transport",
                  code == 200 and mode == "websockets", (code, mode))
        refused = get(f"{base}/selkies/api/status", "")[0]
        res.check(f"{tag} without Jupyter's token the desktop is not reachable", refused in (302, 401, 403), refused)
        launcher = launcher_of(jupyter)
        res.check(f"{tag} the proxy runs the launcher as its child", launcher is not None, launcher)
        desktop = [line for line in open(LOG, errors="replace") if "[selkies-session] desktop " in line]
        res.check(f"{tag} the session the server's data directories hold runs",
                  desktop and "desktop standin on " in desktop[0], desktop[:1])
        started = [line for line in open(LOG, errors="replace") if " starting: " in line]
        res.check(f"{tag} Selkies streams the backend SELKIES_WAYLAND names",
                  started and (" transport, Wayland capture" in started[0]) == wayland, started[:1])
    finally:
        jupyter.kill()
        jupyter.wait(timeout=10)
    deadline = time.time() + 30
    while time.time() < deadline and (session_processes(runtime) or (launcher and H._cmdline(launcher))):
        time.sleep(0.5)
    left = session_processes(runtime)
    res.check(f"{tag} a Jupyter server killed outright takes the session with it",
              launcher is not None and not H._cmdline(launcher) and not left, left[:5])


def main() -> bool:
    res = H.Results("jupyter-proxy")
    if not registered():
        H.skip_suite("jupyter-server-proxy with the selkies entry point is not installed beside "
                     f"{H.PYTHON}; pip install 'selkies[jupyter]' there")
    block(res, wayland=False)
    block(res, wayland=True)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
