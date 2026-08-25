#!/usr/bin/env python3
"""Where client-requested commands launch: the session the applications run in.

app_session() maps the backend and the compositor topology onto the display(s)
a launched application must use; app_launch_env() turns that into DISPLAY /
WAYLAND_DISPLAY / XDG_SESSION_TYPE and adopts the desktop's session bus from
its own processes; app_terminal() picks the terminal for that windowing system.
"""
import os
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "src"))
from selkies import input_handler as ih  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402

results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(("PASS  " if ok else "FAIL  ") + f"[app-session] {name}" + (f"  {detail}" if detail else ""))


def make_handler(is_wayland, app_display="wayland-1", separate=False):
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = is_wayland
    h._session_env_cache = {}
    h._session_env_empty_at = {}
    h._session_env_negcache_ttl = 5.0
    h._app_wayland_display = lambda: app_display
    h._has_separate_app_compositor = lambda: separate
    return h


def main():
    saved_live = ih.x_display_live
    saved_xwl = ih.x_display_is_xwayland
    saved_list = ih.live_x_displays
    saved_which = ih.shutil.which
    saved_display = os.environ.get("DISPLAY")
    disp = f":{1000 + os.getpid() % 50000}"
    try:
        os.environ["DISPLAY"] = disp
        # On the Wayland backend a live X server on $DISPLAY is only the session's
        # rootful Xwayland when it really is an Xwayland process serving it.
        ih.x_display_is_xwayland = lambda name: name == disp
        s = make_handler(False).app_session()
        check("x11 backend: apps on the server's DISPLAY", s == {"x11_display": disp, "wayland_display": None, "type": "x11"}, str(s))

        ih.x_display_live = lambda name: name == disp
        s = make_handler(True).app_session()
        check("rootful Xwayland desktop: X session, no WAYLAND_DISPLAY offered", s == {"x11_display": disp, "wayland_display": None, "type": "x11"}, str(s))

        # A live X server that is NOT an Xwayland (a leftover Xvfb/Xorg holding
        # the display number) is not the session: apps stay Wayland clients.
        ih.x_display_is_xwayland = lambda name: False
        s = make_handler(True).app_session()
        check("non-Xwayland X server on $DISPLAY is not adopted",
              s == {"x11_display": None, "wayland_display": "wayland-1", "type": "wayland"}, str(s))
        ih.x_display_is_xwayland = lambda name: name == disp

        ih.x_display_live = lambda name: False
        ih.live_x_displays = lambda: [":0"]
        s = make_handler(True, "wayland-0", separate=True).app_session()
        check("nested compositor: its socket and its Xwayland", s == {"x11_display": ":0", "wayland_display": "wayland-0", "type": "wayland"}, str(s))

        ih.live_x_displays = lambda: []
        s = make_handler(True).app_session()
        check("plain Wayland session: capture compositor socket", s == {"x11_display": None, "wayland_display": "wayland-1", "type": "wayland"}, str(s))

        ih.shutil.which = lambda n: "/usr/bin/" + n if n in ("foot", "st") else None
        ih.x_display_live = lambda name: name == disp
        check("X11 session launches in st", make_handler(True).app_terminal() == "st")
        ih.x_display_live = lambda name: False
        check("Wayland session launches in foot", make_handler(True).app_terminal() == "foot")
        ih.shutil.which = lambda n: "/usr/bin/st" if n == "st" else None
        check("Wayland session falls back to st when foot is missing", make_handler(True).app_terminal() == "st")
        ih.shutil.which = lambda n: None
        check("no terminal installed: nothing published", make_handler(True).app_terminal() is None)

        # The session bus is adopted from a real dbus-daemon, never from our own launched
        # children; the real which() must be back for the dbus-run-session probe below.
        ih.shutil.which = saved_which
        ih.x_display_live = lambda name: name == disp
        if not shutil.which("dbus-run-session"):
            print("SKIP  [app-session] dbus-run-session not installed: session-bus adoption checks skipped")
            print(f"{sum(results)}/{len(results)} passed")
            return all(results)
        with tempfile.TemporaryDirectory() as runtime:
            os.environ["XDG_RUNTIME_DIR"] = runtime
            env = {**os.environ, "DISPLAY": disp, "XDG_CURRENT_DESKTOP": "MATE"}
            env.pop("WAYLAND_DISPLAY", None); env.pop("DBUS_SESSION_BUS_ADDRESS", None)
            session = subprocess.Popen(["dbus-run-session", "--", "sleep", "30"], env=env,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                       start_new_session=True)
            try:
                deadline = time.time() + 5
                adopted = {}
                while time.time() < deadline and not adopted:
                    adopted = ih.session_environment(disp, None)
                    time.sleep(0.1)
                check("session bus found from the desktop's process", adopted.get("DBUS_SESSION_BUS_ADDRESS", "").startswith("unix:"), str(adopted))
                check("desktop identity adopted with it", adopted.get("XDG_CURRENT_DESKTOP") == "MATE", str(adopted))
                os.environ.pop("DBUS_SESSION_BUS_ADDRESS", None)
                os.environ.pop("WAYLAND_DISPLAY", None)
                h = make_handler(True)
                launch = h.app_launch_env()
                check("launch env DISPLAY/XDG_SESSION_TYPE for the X session",
                      launch.get("DISPLAY") == disp and "WAYLAND_DISPLAY" not in launch and launch.get("XDG_SESSION_TYPE") == "x11",
                      str({k: launch.get(k) for k in ("DISPLAY", "WAYLAND_DISPLAY", "XDG_SESSION_TYPE")}))
                check("launch env carries the adopted bus", launch.get("DBUS_SESSION_BUS_ADDRESS") == adopted["DBUS_SESSION_BUS_ADDRESS"])
                check("adoption is cached while the bus answers", h.app_launch_env().get("DBUS_SESSION_BUS_ADDRESS") == adopted["DBUS_SESSION_BUS_ADDRESS"] and len(h._session_env_cache) == 1)
                os.killpg(session.pid, 9); session.wait(timeout=5)
                time.sleep(0.3)
                check("a dead bus is not kept", not ih.dbus_address_live(adopted["DBUS_SESSION_BUS_ADDRESS"]))
                h2 = make_handler(True)
                check("no session process: nothing adopted, server env stays", "DBUS_SESSION_BUS_ADDRESS" not in h2.app_launch_env())
            finally:
                if session.poll() is None:
                    os.killpg(session.pid, 9)
    finally:
        ih.x_display_live = saved_live
        ih.x_display_is_xwayland = saved_xwl
        ih.live_x_displays = saved_list
        ih.shutil.which = saved_which
        if saved_display is None:
            os.environ.pop("DISPLAY", None)
        else:
            os.environ["DISPLAY"] = saved_display
    print(f"{sum(results)}/{len(results)} passed")
    return all(results)


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
