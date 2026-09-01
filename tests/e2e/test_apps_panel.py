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

Driven against a stand-in `selkies-proot` -- the runner whose presence publishes
the panel -- and a stand-in catalogue, so nothing here reaches the network.

Usage: python3 tests/e2e/test_apps_panel.py
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

APP = "e2eapp"
# The catalogue shape the panel reads: one installable entry, its icon left to
# fail (the panel hides a broken one rather than waiting for it).
CATALOGUE = f"""include:
  - name: {APP}
    full_name: E2E App
    description: A stand-in catalogue entry
    icon: {APP}.png
"""
# Long enough that the panel is observed mid-command, short enough to wait out.
INSTALL_SECONDS = 6
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
                   lambda route: route.fulfill(status=200, content_type="text/yaml", body=CATALOGUE))
        try:
            page.goto(H.BASE_URL, wait_until="load")
            time.sleep(8.0)
            res.check(f"{mode}: the panel is published where its runner works", open_apps_modal(page))

            card = page.locator('.apps-modal-content:has-text("E2E App")').first
            res.check(f"{mode}: the catalogue is listed", card.count() > 0)
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


if __name__ == "__main__":
    results = H.Results("apps-panel")
    for transport in ("websockets", "webrtc"):
        run(transport, results)
    sys.exit(0 if results.summary() else 1)
