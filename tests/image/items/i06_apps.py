"""6. Check app install, but it would only work when `SELKIES_COMMAND_ENABLED` is enabled, so check if it's enabled.

The setting is read first, from what the server tells the page. Enabled, the
dashboard's Apps panel must be published, and an app from its catalog
(Geany, small) is installed through the panel's own Install button, found
installed on the session by the runner, and removed again. Disabled, the
panel must not be offered. An install is the session's own command, the same
from every browser, so it runs once per transport, in Chrome.
"""
import time
from typing import Any

from image_lib import open_sidebar
import test_apps_panel as AP

ITEM = 6
TITLE = "app install (SELKIES_COMMAND_ENABLED)"
ENGINES = ("chromium",)
ENGINE_NOTE = "an install is the session's command, run once per transport from Chrome"
APP, LABEL = "geany", "Geany"


def settle(page: Any, timeout: float) -> bool:
    deadline = time.time() + timeout
    time.sleep(2)
    while time.time() < deadline:
        if page.locator(".app-action-button.running").count() == 0:
            return True
        time.sleep(1)
    return False


def run(cell: Any) -> None:
    R = cell.res
    page = cell.open()
    time.sleep(3)
    enabled = bool((cell.settings(page).get("command_enabled") or {}).get("value"))
    log = cell.target.selkies_log(4000)
    hidden = [ln for ln in log.splitlines() if "Apps panel hidden" in ln][-1:]
    open_sidebar(page)
    shown = page.locator('.sidebar-section-header:has-text("Apps")').count() > 0
    if not enabled:
        R.check("SELKIES_COMMAND_ENABLED is off and the Apps panel is not offered", not shown, f"panel shown={shown}")
        R.skip("app install", "SELKIES_COMMAND_ENABLED is off in this image")
        return
    R.check("SELKIES_COMMAND_ENABLED is on and the Apps panel is published", shown, hidden or "no reason logged")
    if not shown:
        return
    page.locator(".toggle-handle").first.click()
    time.sleep(0.5)
    R.check("the Apps panel opens its catalog", AP.open_apps_modal(page))
    card = page.locator(f'.apps-modal-content:has-text("{LABEL}")').first
    if not card.count():
        R.check(f"the catalog lists {LABEL}", False, "catalog without it (offline?)")
        return
    page.locator(f"text={LABEL}").first.click()
    time.sleep(0.5)
    install = page.locator(".app-action-button.install").first
    if not install.count():
        R.check(f"{LABEL} offers Install", page.locator(".app-action-button.remove").count() > 0,
                "neither Install nor Remove")
    else:
        install.click()
        done = settle(page, 600)
        listed = cell.target.out("selkies-proot list 2>/dev/null; proot-apps list 2>/dev/null")
        R.check(f"Install puts {LABEL} on the session", done and page.locator(".app-action-button.remove").count() > 0
                and APP in listed, f"settled={done}; runner lists {listed[:120]!r}")
    remove = page.locator(".app-action-button.remove").first
    if remove.count():
        remove.click()
        done = settle(page, 300)
        listed = cell.target.out("selkies-proot list 2>/dev/null; proot-apps list 2>/dev/null")
        R.check(f"Remove takes {LABEL} off the session again", done and APP not in listed, listed[:120])
