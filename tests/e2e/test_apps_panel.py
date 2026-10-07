#!/usr/bin/env python3
"""The apps panel while one of its commands is running, and what it knows after.

Driven over both transports: the installed set rides the settings payload and an
`apps_installed` system action, which each transport carries its own way.

An install is a shell command on the session that takes a minute or more, and a
panel that goes back to looking idle the moment it is clicked reads as one that
dropped the click. So a posted command is tracked until the server settles it,
and the button that started it says so meanwhile and holds the row. What ends
up installed is the session's fact rather than the browser's, so a page that
kept no record of the install is still told about it.

A session on a proot-apps local repository installs only what that folder holds,
so the panel lists the folder's own catalog, served by the server, and never
reaches for the remote one.

Driven against a stand-in `selkies-proot` -- the runner whose presence publishes
the panel -- and a stand-in catalog, so nothing here reaches the network.

Usage: python3 tests/e2e/test_apps_panel.py
"""
import base64
import os
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

APP = "e2eapp"
# The catalog shape the panel reads: one installable entry, its icon left to
# fail (the panel hides a broken one rather than waiting for it).
CATALOG = f"""include:
  - name: {APP}
    full_name: E2E App
    description: A stand-in catalog entry
    icon: {APP}.png
"""
# Long enough that the panel is observed mid-command, short enough to wait out.
INSTALL_SECONDS = 6
LOCAL_APP = "localapp"
LOCAL_CATALOG = f"""include:
  - name: {LOCAL_APP}
    full_name: Local App
    description: From the local repository
    icon: {LOCAL_APP}.png
"""
# A 1x1 PNG, so a rendered icon has a size to measure.
LOCAL_ICON = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
# The stand-in answers `list` from a file it writes on install, the way the real
# wrapper answers it from the runner's install directory: what is installed is
# the session's fact, and the panel has to be able to ask for it.
RUNNER = f"""#!/bin/bash
state="{{STATE}}"
case "$1" in
  check) exit 0 ;;
  list) [ -f "$state" ] && cat "$state"; exit 0 ;;
  install) sleep {INSTALL_SECONDS}; echo "$2" >> "$state"; exit 0 ;;
  remove) if [ -f "$state" ]; then grep -vx "$2" "$state" > "$state.new" || true; mv "$state.new" "$state"; fi; exit 0 ;;
  *) exit 0 ;;
esac
"""


def runner_stub(directory: str) -> str:
    """Write the `selkies-proot` stand-in and return the directory to prepend to PATH."""
    os.makedirs(directory, exist_ok=True)
    state = os.path.join(directory, "installed")
    if os.path.exists(state):
        os.unlink(state)
    path = os.path.join(directory, "selkies-proot")
    with open(path, "w") as fh:
        fh.write(RUNNER.replace("{STATE}", state))
    os.chmod(path, 0o755)
    return directory


def local_repository(folder: str) -> str:
    """Lay out a proot-apps local repository holding one application and its catalog."""
    shutil.rmtree(folder, ignore_errors=True)
    os.makedirs(os.path.join(folder, f"ghcr.io_linuxserver_proot-apps_{LOCAL_APP}"))
    os.makedirs(os.path.join(folder, "metadata", "img"))
    with open(os.path.join(folder, "metadata", "metadata.yml"), "w") as fh:
        fh.write(LOCAL_CATALOG)
    with open(os.path.join(folder, "metadata", "img", f"{LOCAL_APP}.png"), "wb") as fh:
        fh.write(LOCAL_ICON)
    return folder


def open_apps_modal(page) -> bool:
    """Open the sidebar's apps panel; False when it never appears."""
    page.locator(".toggle-handle").first.click()
    time.sleep(0.6)
    header = page.locator('.sidebar-section-header:has-text("Apps")').first
    if header.count() == 0:
        return False
    header.click()
    time.sleep(0.6)
    page.locator('#apps-content button').first.click()
    time.sleep(1.0)
    return page.locator(".apps-modal").count() > 0


def run(mode: str, res: "H.Results") -> None:
    """Run one install through the panel over `mode` and watch what it reports.

    Args:
        mode: Transport mode, ``websockets`` or ``webrtc``.
        res: Results accumulator shared across both transports.
    """
    stub = runner_stub(os.path.join(H.WORKDIR, f"apps-stub-bin-{mode}"))
    H.server_start(mode=mode, wayland=False, web_root=H.CLASSIC_DIST,
                   extra_env={"PATH": stub + os.pathsep + os.environ.get("PATH", ""),
                              "SELKIES_COMMAND_ENABLED": "true"})
    with sync_playwright() as pw:
        browser = C.chromium_launch(pw)
        ctx = browser.new_context(viewport={"width": 1440, "height": 900}, device_scale_factor=1)
        ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
        page = ctx.new_page()
        page.route("**/proot-apps/**/metadata.yml",
                   lambda route: route.fulfill(status=200, content_type="text/yaml", body=CATALOG))
        try:
            page.goto(H.BASE_URL, wait_until="load")
            time.sleep(8.0)
            res.check(f"{mode}: the panel is published where its runner works", open_apps_modal(page))

            card = page.locator('.apps-modal-content:has-text("E2E App")').first
            res.check(f"{mode}: the catalog is listed", card.count() > 0)
            page.locator('text=E2E App').first.click()
            time.sleep(0.5)
            install = page.locator('.app-action-button.install').first
            res.check(f"{mode}: an uninstalled app offers Install", install.count() > 0)

            install.click()
            running = held = False
            deadline = time.time() + INSTALL_SECONDS - 1
            while time.time() < deadline:
                if page.locator(".app-action-button.running").count() > 0:
                    running = True
                    held = install.is_disabled()
                    break
                time.sleep(0.2)
            res.check(f"{mode}: the button that started the command reports it running", running)
            res.check(f"{mode}: and holds the row while it runs", held)

            settled = False
            deadline = time.time() + INSTALL_SECONDS + 20
            while time.time() < deadline:
                if page.locator(".app-action-button.running").count() == 0:
                    settled = True
                    break
                time.sleep(0.3)
            res.check(f"{mode}: the server settling the command clears it", settled)
            res.check(f"{mode}: the panel then offers the app as installed",
                      page.locator('.app-action-button.remove').count() > 0)

            # What is installed belongs to the session, not to one browser: a
            # page with no stored record of the install must still be told.
            page.evaluate("localStorage.removeItem('prootInstalledApps')")
            page.reload(wait_until="load")
            time.sleep(8.0)
            reopened = open_apps_modal(page)
            page.locator('text=E2E App').first.click()
            time.sleep(0.5)
            res.check(f"{mode}: a page with no stored record is told what is installed",
                      reopened and page.locator('.app-action-button.remove').count() > 0)
        finally:
            browser.close()
    H.server_stop()


def run_local_repository(mode: str, res: "H.Results") -> None:
    """Open the panel over `mode` on a session installing from a local repository.

    Args:
        mode: Transport mode, ``websockets`` or ``webrtc``.
        res: Results accumulator shared across both transports.
    """
    stub = runner_stub(os.path.join(H.WORKDIR, f"apps-stub-bin-{mode}"))
    folder = local_repository(os.path.join(H.WORKDIR, f"apps-local-repo-{mode}"))
    H.server_start(mode=mode, wayland=False, web_root=H.CLASSIC_DIST,
                   extra_env={"PATH": stub + os.pathsep + os.environ.get("PATH", ""),
                              "SELKIES_COMMAND_ENABLED": "true", "PA_REPO_FOLDER": folder})
    with sync_playwright() as pw:
        browser = C.chromium_launch(pw)
        ctx = browser.new_context(viewport={"width": 1440, "height": 900}, device_scale_factor=1)
        ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
        page = ctx.new_page()
        remote_requests = []
        page.route("**/raw.githubusercontent.com/**",
                   lambda route: (remote_requests.append(route.request.url), route.abort()))
        try:
            page.goto(H.BASE_URL, wait_until="load")
            time.sleep(8.0)
            res.check(f"{mode}: the panel is published on a local repository", open_apps_modal(page))
            card = page.locator('.apps-modal-content:has-text("Local App")').first
            res.check(f"{mode}: the repository's own catalog is listed", card.count() > 0)
            icon = page.locator('.app-card-icon[alt="Local App"]').first
            width = 0
            deadline = time.time() + 10
            while time.time() < deadline:
                width = icon.evaluate("img => img.naturalWidth") if icon.count() else 0
                if width:
                    break
                time.sleep(0.2)
            res.check(f"{mode}: its icon is served by the server", width == 1, width)
            res.check(f"{mode}: the remote catalog is never asked for", not remote_requests, remote_requests[:2])
        finally:
            browser.close()
    H.server_stop()


if __name__ == "__main__":
    results = H.Results("apps-panel")
    for transport in ("websockets", "webrtc"):
        run(transport, results)
    for transport in ("websockets", "webrtc"):
        run_local_repository(transport, results)
    sys.exit(0 if results.summary() else 1)
