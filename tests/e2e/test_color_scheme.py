#!/usr/bin/env python3
"""The session's desktop follows the page's light or dark color scheme.

A private session bus at the suite's `XDG_RUNTIME_DIR/bus`, the address GLib
connects a process naming no bus to, with dconf and the settings portal and its
GTK backend activated behind it (configured by the base image's own
`portals.conf`), stands in for what the images run; on X11 an
xsettingsd serves the test display, as the base image's does. A GTK 3 window is
up before the first change and stays up, as an application would, and reports
the theme it draws with each time it changes.

The page's browser prefers dark, then light: the desktop's GNOME color-scheme
key, what the portal serves as `org.freedesktop.appearance color-scheme`, and
the running window's theme follow each, live. The dashboard's theme toggle then
picks a theme, which holds over the browser's preference. A `color_scheme` the
operator locks then gives way to no page.

    python3 tests/e2e/test_color_scheme.py ws-x11|wr-x11|ws-wl|wr-wl
"""
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402

PROBE = os.path.join(H.REPO, "tests", "image", "site", "scheme_probe.py")
# The portal configuration the base image installs, which this session reads as its own.
PORTALS_CONF = os.path.join(H.REPO, "addons", "base", "xdg-desktop-portal.conf")
BUS_SERVICES = "/usr/share/dbus-1/services"
NEEDED_SERVICES = {
    "dconf": "ca.desrt.dconf.service",
    "xdg-desktop-portal": "org.freedesktop.portal.Desktop.service",
    "xdg-desktop-portal-gtk": "org.freedesktop.impl.portal.desktop.gtk.service",
}
DARK_THEME = "/usr/share/themes/Adwaita-dark/gtk-3.0"
SCHEMA = "org.gnome.desktop.interface"
PORTAL_VALUE = {True: "uint32 1", False: "uint32 2"}
SET_LINE = "Desktop color scheme set to"


def gtk_python() -> Optional[str]:
    """The first of the system's Python interpreters PyGObject's GTK 3 imports in: the
    suites' own interpreter is seldom the one the distribution built it for."""
    candidates = ["/usr/bin/python3"] + sorted(p for p in glob.glob("/usr/bin/python3.*")
                                                if re.fullmatch(r".*/python3\.\d+", p))
    for exe in candidates:
        if os.path.exists(exe) and subprocess.run(
                [exe, "-c", "import gi; gi.require_version('Gtk', '3.0'); from gi.repository import Gtk"],
                capture_output=True).returncode == 0:
            return exe
    return None


SYSTEM_PYTHON = gtk_python()


def missing_tools(x11: bool) -> list:
    """What this host lacks of the stack the suite stands up."""
    lacking = [tool for tool in ("dbus-daemon", "gsettings", "gdbus") if not shutil.which(tool)]
    if x11 and not shutil.which("xsettingsd"):
        lacking.append("xsettingsd")
    lacking += [name for name, unit in NEEDED_SERVICES.items()
                if not os.path.exists(os.path.join(BUS_SERVICES, unit))]
    if not os.path.isdir(DARK_THEME):
        lacking.append("the Adwaita-dark GTK 3 theme (gnome-themes-extra-data)")
    if SYSTEM_PYTHON is None:
        lacking.append("PyGObject with GTK 3 (python3-gi, gir1.2-gtk-3.0)")
    return lacking


class Session:
    """The bus, the XSETTINGS manager, and the running GTK 3 window of one run."""

    def __init__(self, home: str, display_env: dict) -> None:
        self.home, self.bus = home, os.path.join(H.RUNTIME_DIR, "bus")
        self.env = {"PATH": os.environ.get("PATH", ""), "HOME": home, "XDG_RUNTIME_DIR": H.RUNTIME_DIR,
                    "XDG_CONFIG_HOME": os.path.join(home, ".config"), **display_env}
        self.procs: list = []
        self.probe_out = os.path.join(H.WORKDIR, "color-scheme-gtk3.json")

    def start(self, x11: bool) -> None:
        if os.path.exists(self.bus):
            os.unlink(self.bus)
        portals = os.path.join(self.env["XDG_CONFIG_HOME"], "xdg-desktop-portal")
        os.makedirs(portals, exist_ok=True)
        shutil.copy(PORTALS_CONF, os.path.join(portals, "portals.conf"))
        self.procs.append(H.spawn(["dbus-daemon", "--session", "--nofork", "--nopidfile",
                                   f"--address=unix:path={self.bus}"], env=self.env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                  start_new_session=True))
        deadline = time.time() + 10
        while not os.path.exists(self.bus) and time.time() < deadline:
            time.sleep(0.1)
        if x11:
            with open(os.path.join(self.home, ".xsettingsd"), "w") as f:
                f.write("Xft/DPI 98304\n")
            self.procs.append(H.spawn(["xsettingsd"], env=self.env, stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL, start_new_session=True))
        self.start_window()

    def start_window(self, display_env: Optional[dict] = None) -> None:
        """The GTK 3 window, on `display_env` where the display it ran on went away."""
        self.env.update(display_env or {})
        if os.path.exists(self.probe_out):
            os.unlink(self.probe_out)
        self.procs.append(H.spawn([SYSTEM_PYTHON, PROBE, "gtk3", self.probe_out], env=self.env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                  start_new_session=True))

    def stop(self) -> None:
        for proc in reversed(self.procs):
            try:
                os.killpg(proc.pid, 15)
            except OSError:
                pass
            try:
                proc.wait(5)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, 9)
        if os.path.exists(self.bus):
            os.unlink(self.bus)

    def tool(self, *args: str) -> str:
        env = dict(self.env, DBUS_SESSION_BUS_ADDRESS=f"unix:path={self.bus}")
        try:
            p = subprocess.run(list(args), env=env, capture_output=True, timeout=30)
        except subprocess.TimeoutExpired:
            return "timed out"
        return (p.stdout + p.stderr).decode(errors="replace").strip()

    def gnome_key(self, key: str) -> str:
        return self.tool("gsettings", "get", SCHEMA, key).strip("'")

    def portal(self) -> str:
        return self.tool("gdbus", "call", "--session", "-d", "org.freedesktop.portal.Desktop",
                         "-o", "/org/freedesktop/portal/desktop", "-m",
                         "org.freedesktop.portal.Settings.ReadOne", "org.freedesktop.appearance", "color-scheme")

    def window_theme(self) -> Optional[str]:
        try:
            with open(self.probe_out) as f:
                return json.load(f).get("theme")
        except (OSError, ValueError):
            return None


def until(pred: Any, timeout: float = 20) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if pred():
            return True
        time.sleep(0.3)
    return pred()


def expect(res: "H.Results", session: Session, dark: bool, why: str) -> None:
    scheme = "dark" if dark else "light"
    key = "prefer-dark" if dark else "prefer-light"
    theme = "Adwaita-dark" if dark else "Adwaita"
    res.check(f"{why}: the GNOME color-scheme key is {key}",
              until(lambda: session.gnome_key("color-scheme") == key), session.gnome_key("color-scheme"))
    res.check(f"{why}: the settings portal serves {scheme}",
              until(lambda: PORTAL_VALUE[dark] in session.portal()), session.portal()[-160:])
    res.check(f"{why}: the running GTK 3 window draws with {theme}",
              until(lambda: session.window_theme() == theme), session.window_theme())


def open_page(browser: Any, mode: str, scheme: str, display_id: Optional[str] = None) -> Any:
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, color_scheme=scheme)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/" + (f"#{display_id}" if display_id else ""), wait_until="load")
    return page


def wait_video(page: Any, mode: str) -> Optional[dict]:
    return C.wait_ws_video(page, timeout=30) if mode == "websockets" else C.wait_wr_video(page)


def run(mode: str, backend: str, res: "H.Results") -> None:
    x11 = backend == "x11"
    home = os.path.join(H.WORKDIR, "color-scheme-home")
    shutil.rmtree(home, ignore_errors=True)
    os.makedirs(os.path.join(home, ".config"))
    xvfb = display = None
    if x11:
        xvfb, display = H.private_x_server(1280, 720)
        H.TEST_DISPLAY = display
    session: Optional[Session] = None
    try:
        H.server_start(mode=mode, wayland=not x11, web_root=H.DASHBOARD_DIST,
                       extra_env={"HOME": home, "XDG_CONFIG_HOME": os.path.join(home, ".config")})
        display_env = {"DISPLAY": display} if x11 else {"WAYLAND_DISPLAY": H.capture_socket(),
                                                         "GDK_BACKEND": "wayland"}
        session = Session(home, display_env)
        session.start(x11)
        res.check("the GTK 3 window is up", until(lambda: session.window_theme() is not None),
                  session.window_theme())
        with sync_playwright() as p:
            browser = C.chromium_launch(p)
            try:
                page = open_page(browser, mode, "dark")
                res.check("video flowing", bool(wait_video(page, mode)))
                res.check("the server says what the desktop's scheme became",
                          C.wait_log(f"{SET_LINE} dark", timeout=20), "")
                expect(res, session, True, "a browser preferring dark")
                page.emulate_media(color_scheme="light")
                expect(res, session, False, "the browser's preference flipping to light")

                page.emulate_media(color_scheme="dark")
                expect(res, session, True, "and back to dark")
                page.locator(".theme-toggle").first.dispatch_event("click")
                picked = page.evaluate("localStorage.getItem('theme')")
                res.check("the dashboard's toggle picks light over the dashboard's default dark",
                          picked == "light", picked)
                expect(res, session, False, "a dashboard theme picked light under a dark browser")
                page.emulate_media(color_scheme="light")
                page.emulate_media(color_scheme="dark")
                time.sleep(2)
                expect(res, session, False, "the pick holding as the browser's preference flips")
                page.locator(".theme-toggle").first.dispatch_event("click")
                expect(res, session, True, "the dashboard's theme picked dark again")
                page.context.close()

                H.server_start(mode=mode, wayland=not x11, web_root=H.DASHBOARD_DIST,
                               extra_env={"HOME": home, "XDG_CONFIG_HOME": os.path.join(home, ".config"),
                                          "SELKIES_COLOR_SCHEME": "light"})
                if not x11:
                    # The window was a client of the compositor the server it ended took along.
                    session.start_window({"WAYLAND_DISPLAY": H.capture_socket()})
                    res.check("locked light: the GTK 3 window is up again on the new compositor",
                              until(lambda: session.window_theme() is not None), session.window_theme())
                page = open_page(browser, mode, "dark")
                res.check("locked light: video flowing", bool(wait_video(page, mode)))
                expect(res, session, False, "a dark page under a scheme the operator locked light")
                res.check("locked light: the page's dark is never applied",
                          f"{SET_LINE} dark" not in H.server_log(), "")
                page.context.close()
            finally:
                browser.close()
    finally:
        if session:
            session.stop()
        H.server_stop()
        if xvfb is not None:
            H.stop_x_server(xvfb, display)


SELECTORS = ("ws-x11", "wr-x11", "ws-wl", "wr-wl")


def main() -> bool:
    which = sys.argv[1] if len(sys.argv) > 1 else "ws-x11"
    if which not in SELECTORS:
        raise SystemExit(f"unknown selector {which!r}; one of {SELECTORS}")
    transport, backend = which.split("-")
    lacking = missing_tools(backend == "x11")
    if lacking:
        H.skip_suite("missing " + ", ".join(lacking))
    if not os.path.exists(os.path.join(H.DASHBOARD_DIST, "index.html")):
        H.skip_suite("the dashboard is not built (scripts/ci/build-web.sh)")
    res = H.Results(f"color-scheme-{which}")
    run("websockets" if transport == "ws" else "webrtc", backend, res)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
