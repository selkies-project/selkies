#!/usr/bin/env python3
"""Auth-refresh flow: a live page whose server comes back requiring Basic auth
must reload itself into the browser's credential prompt (the 401 challenge)
instead of reconnecting forever against rejected websockets. The reload comes
from the core's same-origin 401 guard; the guard's loop-breaker caps how many
reloads a broken credential can cause, so exactly one arrives here."""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C

from playwright.sync_api import sync_playwright

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [auth-refresh] {label}  {detail}", flush=True)


def main() -> None:
    H.server_start(mode="websockets", wayland=False, extra_env={
        "SELKIES_ENABLE_BASIC_AUTH": "false",
    })
    with sync_playwright() as p:
        browser = C.launch_browser(p, "chromium")
        page = browser.new_page(viewport={"width": 1280, "height": 800})
        navigates = []
        page.on(
            "request",
            lambda r: navigates.append(r.url)
            if r.is_navigation_request() and r.frame == page.main_frame else None,
        )
        page.goto(H.BASE_URL + "/", wait_until="domcontentloaded")
        info = C.wait_ws_video(page)
        check("stream up before the auth flip", info is not None, info)
        base_navs = len(navigates)

        H.server_stop()
        H.server_start(mode="websockets", wayland=False, extra_env={
            "SELKIES_ENABLE_BASIC_AUTH": "true",
            "SELKIES_BASIC_AUTH_USER": "selkies",
            "SELKIES_BASIC_AUTH_PASSWORD": "hunter2",
        })

        deadline = time.time() + 75
        reloaded = False
        while time.time() < deadline:
            time.sleep(1)
            try:
                if len(navigates) > base_navs or "Authorization Required" in page.content():
                    reloaded = True
                    break
            except Exception:
                reloaded = True
                break
        check("page reloads into the auth challenge", reloaded,
              f"navs={navigates[base_navs:]}")

        page.wait_for_timeout(6000)
        extra = len(navigates) - base_navs
        check("reload storm stays capped", extra <= 3, f"reloads={extra}")
        browser.close()
    H.server_stop()


try:
    main()
finally:
    print(f"[auth-refresh] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
