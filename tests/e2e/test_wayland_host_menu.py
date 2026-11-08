#!/usr/bin/env python3
"""Host-capture virtual-pointer events drive labwc's release-gated root menu.

The suite sends the same browser input a remote user sends, through Selkies'
Wayland-host capture path into a private headless labwc. It checks the actual
virtual-pointer wire timestamps as well as labwc's ShowMenu actions and the
Terminal command. Labwc ignores a menu-item release whose timestamp follows the
press within its ignore period; an all-zero virtual-pointer clock therefore
opens the root menu but cannot activate its item.

Usage: python3 tests/e2e/test_wayland_host_menu.py
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright


def main() -> bool:
    """Run the browser-to-host menu lifecycle against real compositor actions."""
    res = H.Results("wl-host-menu")
    if shutil.which("labwc") is None:
        H.skip_suite("labwc is not installed")
    root = Path(H.WORKDIR) / "wl-host-menu-rig"
    config = root / "config" / "labwc"
    config.mkdir(parents=True, exist_ok=True)
    marker = root / "actions.log"
    labwc_log = root / "labwc.log"
    config_xml = """<?xml version=\"1.0\"?>
<labwc_config>
  <core><decoration>server</decoration></core>
  <keyboard>
    <keybind key=\"C-A-t\"><action name=\"Execute\"><command>sh -c 'echo key >> MARKER'</command></action></keybind>
  </keyboard>
  <mouse>
    <default />
    <context name=\"Root\">
      <mousebind button=\"Left\" action=\"Press\" />
      <mousebind button=\"Middle\" action=\"Press\"><action name=\"ShowMenu\" menu=\"client-list-combined-menu\" /></mousebind>
    </context>
  </mouse>
</labwc_config>
"""
    menu_xml = """<?xml version=\"1.0\"?>
<openbox_menu>
  <menu id=\"root-menu\" label=\"Root\">
    <item label=\"Terminal\"><action name=\"Execute\"><command>sh -c 'echo menu >> MARKER'</command></action></item>
  </menu>
</openbox_menu>
"""
    (config / "rc.xml").write_text(config_xml.replace("MARKER", str(marker)))
    (config / "menu.xml").write_text(menu_xml.replace("MARKER", str(marker)))
    marker.unlink(missing_ok=True)
    env = {
        "PATH": os.environ.get("PATH", ""),
        "HOME": os.path.expanduser("~"),
        "XDG_RUNTIME_DIR": H.RUNTIME_DIR,
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_CONFIG_DIRS": "/etc/xdg",
        "WLR_BACKENDS": "headless",
        "WLR_RENDERER": "pixman",
        "WLR_HEADLESS_OUTPUTS": "1",
        "WLR_LIBINPUT_NO_DEVICES": "1",
        "LIBGL_ALWAYS_SOFTWARE": "1",
    }
    host = "wayland-0"
    host_socket = Path(H.RUNTIME_DIR) / host
    labwc_output = open(labwc_log, "w")
    labwc = H.spawn(
        ["labwc", "--debug", "--config-dir", str(config)],
        env=env,
        stdout=labwc_output,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    browser = None
    try:
        deadline = time.monotonic() + 15
        while not host_socket.is_socket() and time.monotonic() < deadline:
            if labwc.poll() is not None:
                break
            time.sleep(0.05)
        res.check("private headless labwc publishes a socket", host_socket.is_socket(),
                  H.tail(str(labwc_log)))
        if not host_socket.is_socket():
            return res.summary()

        H.server_start(mode="websockets", wayland=True, extra_env={
            "SELKIES_WAYLAND_HOST_DISPLAY": str(host_socket),
            "SELKIES_WIDTH": "1280",
            "SELKIES_HEIGHT": "720",
            "WAYLAND_DEBUG": "1",
            "LIBGL_ALWAYS_SOFTWARE": "1",
        })
        with sync_playwright() as p:
            browser, page, console_errors, not_found = C.launch_chrome(p, mode="websockets")
            video = C.wait_ws_video(page, timeout=60)
            res.check("the remote page displays the host desktop", video is not None,
                      H.server_log(tail=15))
            if video is None:
                return res.summary()
            res.check("host capture binds labwc's virtual-input protocols",
                      C.wait_log("[HostCapture] capturing host compositor", 5),
                      H.server_log(tail=10))
            time.sleep(1.0)

            def click(x: float, y: float, button: str) -> None:
                """Send one 40 ms browser click and let the compositor settle."""
                page.mouse.click(x, y, button=button, delay=40)
                time.sleep(0.6)

            def labwc_since(offset: int) -> str:
                """Return newly emitted compositor actions after `offset`."""
                return labwc_log.read_text(errors="replace")[offset:]

            def actions() -> list:
                """Return real commands labwc has executed so far."""
                return marker.read_text().splitlines() if marker.exists() else []

            left_offset = labwc_log.stat().st_size
            click(640, 300, "left")
            click(680, 315, "left")
            res.check("left root clicks open no menu and execute nothing",
                      "ShowMenu" not in labwc_since(left_offset) and not actions(),
                      labwc_since(left_offset)[-500:])

            wire_offset = os.path.getsize(H.LOG)
            right_offset = labwc_log.stat().st_size
            click(640, 300, "right")
            res.check("right root click opens labwc's root menu",
                      "ShowMenu" in labwc_since(right_offset),
                      labwc_since(right_offset)[-500:])
            page.screenshot(path=str(root / "root-menu.png"))
            click(680, 315, "left")
            time.sleep(0.8)
            wire_text = H.server_log()[wire_offset:]
            wire = [
                {"time": int(t), "button": int(b), "state": int(s)}
                for t, b, s in re.findall(
                    r"zwlr_virtual_pointer_v1@\d+\.button\((\d+), (\d+), ([01])\)",
                    wire_text,
                )
            ]
            durations = []
            for button in (273, 272):
                states = [event for event in wire if event["button"] == button]
                if len(states) == 2 and [e["state"] for e in states] == [1, 0]:
                    durations.append((states[1]["time"] - states[0]["time"]) & 0xFFFFFFFF)
            res.check("right open and item selection each carry one press/release pair",
                      len(wire) == 4, json.dumps(wire))
            res.check("button times are live, ordered, and leave the item-release gate",
                      len(wire) == 4 and all(e["time"] != 0 for e in wire)
                      and len(durations) == 2 and all(0 < n < 250 for n in durations),
                      json.dumps({"events": wire, "durations_ms": durations}))
            res.check("selecting Terminal executes its real labwc action", "menu" in actions(),
                      actions())

            click(100, 400, "left")
            middle_offset = labwc_log.stat().st_size
            click(100, 300, "middle")
            middle_log = labwc_since(middle_offset)
            res.check("middle root click opens the configured client list",
                      "ShowMenu" in middle_log, middle_log[-500:])
            page.screenshot(path=str(root / "client-list.png"))
            click(100, 400, "left")

            page.keyboard.down("Control")
            page.keyboard.down("Alt")
            page.keyboard.press("t")
            page.keyboard.up("Alt")
            page.keyboard.up("Control")
            time.sleep(0.8)
            res.check("adjacent virtual-keyboard input still executes Ctrl+Alt+T",
                      "key" in actions(), actions())
            real_errors, bad_404 = C.benign_console(console_errors, not_found)
            res.check("the browser reports no errors", not real_errors and not bad_404,
                      (real_errors + bad_404)[:3])
            C.close_browser(browser)
            browser = None
    finally:
        if browser is not None:
            C.close_browser(browser)
        H.server_stop()
        labwc.terminate()
        try:
            labwc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            labwc.kill()
            labwc.wait()
        labwc_output.close()
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
