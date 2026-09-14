#!/usr/bin/env python3
"""Host capture of a compositor without the wlroots protocols goes through xdg-desktop-portal.

A KDE session offers neither screencopy nor the virtual keyboard and pointer protocols to
ordinary clients, so `SELKIES_WAYLAND_HOST_DISPLAY` aimed at its socket has pixelflux open a
RemoteDesktop portal session instead: the monitor's PipeWire stream carries the frames and the
seat's input goes through the portal by keysym and stream coordinates. This suite runs a real
KDE stack on a private bus — `kwin_wayland --virtual`, PipeWire, xdg-desktop-portal and its KDE
backend — starts the server against it over either transport, and checks the rung was taken,
the stream negotiated, and that a browser's pointer moves, clicks, scrolls and keys land on a
client window inside KWin. KWin's virtual backend records no screencast frames (only its DRM
and nested Wayland backends emit the output damage the stream is fed from), so the frame checks
report skipped there and pass where a backend that does emit them is captured.

Usage: python3 tests/e2e/test_wayland_host_portal.py [websockets|webrtc]
"""
import os
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

SIZE = (1280, 720)


def service_exec(name: str) -> str:
    """The executable behind a D-Bus service name, from its activation file, the way the bus
    itself finds it; empty when no data directory carries the file."""
    dirs = os.environ.get("XDG_DATA_DIRS", "/usr/local/share:/usr/share").split(":")
    for base in dirs:
        path = os.path.join(base, "dbus-1", "services", f"{name}.service")
        try:
            with open(path) as f:
                for line in f:
                    if line.startswith("Exec="):
                        return line[5:].strip().split()[0]
        except OSError:
            continue
    return ""


class KdeRig:
    """kwin_wayland (virtual backend), PipeWire and the KDE portal on a private session bus and
    runtime dir, the way a KDE session exposes them; `socket_path` is the compositor's socket."""

    def __init__(self) -> None:
        # A short path: the compositor's, PipeWire's and the bus's sockets all live under it.
        self.root = tempfile.mkdtemp(prefix="pf-portal-")
        self.runtime = os.path.join(self.root, "rt")
        os.makedirs(self.runtime, mode=0o700)
        self.socket_name = "wayland-portal"
        self.socket_path = os.path.join(self.runtime, self.socket_name)
        self.procs: list = []
        self.bus = ""
        self.portal = service_exec("org.freedesktop.portal.Desktop")
        self.portal_kde = service_exec("org.freedesktop.impl.portal.desktop.kde")
        self.env = {
            "PATH": os.environ.get("PATH", ""), "HOME": os.path.expanduser("~"),
            "XDG_RUNTIME_DIR": self.runtime, "WAYLAND_DISPLAY": self.socket_name,
            "XDG_CURRENT_DESKTOP": "KDE", "XDG_SESSION_TYPE": "wayland", "QT_QPA_PLATFORM": "wayland",
        }

    def _spawn(self, cmd: list, name: str, **extra: str) -> subprocess.Popen:
        log = open(os.path.join(self.root, f"{name}.log"), "w")
        proc = H.spawn(cmd, env={**self.env, **extra}, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        self.procs.append(proc)
        return proc

    def start(self) -> str:
        """Bring the stack up; returns why it could not, or an empty string."""
        for binary in ("kwin_wayland", "dbus-daemon", "pipewire", "wireplumber"):
            if not shutil.which(binary):
                return f"{binary} not installed"
        for name, path in (("xdg-desktop-portal", self.portal), ("xdg-desktop-portal-kde", self.portal_kde)):
            if not path or not os.path.exists(path):
                return f"{name} not installed"
        bus_proc = subprocess.Popen(["dbus-daemon", "--session", "--print-address=1", "--fork",
                                     "--address", f"unix:path={self.root}/bus"],
                                    stdout=subprocess.PIPE, text=True, env=self.env)
        self.bus = (bus_proc.stdout.read() or "").strip()
        if not self.bus:
            return "dbus-daemon printed no address"
        self.env["DBUS_SESSION_BUS_ADDRESS"] = self.bus
        self._spawn(["pipewire"], "pipewire")
        time.sleep(0.5)
        self._spawn(["wireplumber"], "wireplumber")
        self._spawn(["kwin_wayland", "--virtual", "--width", str(SIZE[0]), "--height", str(SIZE[1]),
                     "--no-lockscreen", "--no-global-shortcuts", "--no-kactivities",
                     "--socket", self.socket_name], "kwin")
        deadline = time.time() + 20
        while not os.path.exists(self.socket_path):
            if time.time() > deadline:
                return f"kwin_wayland never opened its socket: {H.tail(self.log('kwin'), 3)}"
            time.sleep(0.2)
        time.sleep(1.0)
        self._spawn([self.portal_kde], "portal-kde", KDE_FULL_SESSION="true")
        self._spawn([self.portal, "-r"], "portal")
        time.sleep(2.0)
        return ""

    def log(self, name: str) -> str:
        return os.path.join(self.root, f"{name}.log")

    def stop(self) -> None:
        for proc in reversed(self.procs):
            try:
                os.killpg(proc.pid, 15)
            except OSError:
                pass
        time.sleep(1.0)
        for proc in self.procs:
            try:
                os.killpg(proc.pid, 9)
            except OSError:
                pass
        subprocess.run(["pkill", "-f", f"dbus-daemon.*{self.root}/bus"], capture_output=True)
        # The stack's logs go where the suite runner keeps a suite's record.
        for name in ("kwin", "pipewire", "wireplumber", "portal-kde", "portal"):
            try:
                shutil.copy2(self.log(name), os.path.join(H.WORKDIR, f"portal-{name}.log"))
            except OSError:
                pass
        shutil.rmtree(self.root, ignore_errors=True)


def wait_video(page, mode: str, timeout: float):
    return C.wait_ws_video(page, timeout=timeout) if mode == "websockets" else C.wait_wr_video(page, timeout=timeout)


def run(mode: str) -> "H.Results":
    res = H.Results(f"wl-host-portal-{mode}")
    rig = KdeRig()
    why = rig.start()
    if why:
        rig.stop()
        H.skip_suite(f"KDE portal stack unavailable: {why}")
    try:
        # The observer is the KDE session's own client: fullscreen, so every injected pointer
        # event lands on it, and it reports the seat events it receives.
        obs = H.WlObs(rig.socket_name, XDG_RUNTIME_DIR=rig.runtime, WLOBS_FILL="ff2878dc")
        res.check("observer client maps inside KWin", obs.ready(15), H.tail(rig.log("kwin"), 3))
        H.server_start(mode=mode, wayland=True, extra_env={
            "SELKIES_WAYLAND_HOST_DISPLAY": rig.socket_path,
            "DBUS_SESSION_BUS_ADDRESS": rig.bus,
            "XDG_CURRENT_DESKTOP": "KDE",
        })
        with sync_playwright() as p:
            browser, page, console_errors, not_found = C.launch_chrome(p, mode=mode)
            try:
                res.check("the portal rung is taken for the frames",
                          C.wait_log("frames come through the xdg-desktop-portal ScreenCast", 60), H.tail(rig.log("portal"), 2))
                res.check("keyboard and pointer go through the portal",
                          C.wait_log("keyboard goes through the portal by keysym", 10)
                          and C.wait_log("pointer goes through the portal", 10), H.server_log(tail=5))
                res.check("the portal session opens with the monitor stream and both devices",
                          C.wait_log("portal session open: 1 stream(s), devices keyboard pointer", 60), H.server_log(tail=8))
                res.check("the stream negotiates the host's mode",
                          C.wait_log(f"portal stream: {SIZE[0]}x{SIZE[1]}", 30), H.server_log(tail=8))
                res.check("the stream reaches the streaming state", C.wait_log("portal stream streaming", 30), H.server_log(tail=8))
                res.check("no host connect failure", "connect failed" not in H.server_log(), "")

                if C.wait_log("first portal frame", 8):
                    res.check("video reaches the browser", bool(wait_video(page, mode, 30)), "")
                else:
                    res.skip("video reaches the browser", "the compositor emitted no screencast frame (KWin's virtual backend never does)")

                # Input from the browser: pointer motion, a click, a wheel notch and a key,
                # each of which the KDE client inside KWin must report.
                page.mouse.move(640, 360)
                page.mouse.click(640, 360)
                time.sleep(0.3)
                page.mouse.move(300, 200)
                time.sleep(0.3)
                page.mouse.wheel(0, 120)
                time.sleep(0.3)
                page.keyboard.press("a")
                time.sleep(0.3)
                page.keyboard.press("Shift+B")
                res.check("pointer motion reaches the KDE client", obs.wait_for("ptr_motion", timeout=8) is not None,
                          [l for l in obs.lines if l.get("kind") == "ptr_enter"][:2])
                res.check("a click reaches the KDE client", obs.wait_for("ptr_button", timeout=8, state=1) is not None
                          and obs.wait_for("ptr_button", timeout=8, state=0) is not None, "")
                res.check("a wheel notch reaches the KDE client", obs.wait_for("ptr_axis", timeout=8) is not None, "")
                keys = [l["key"] for l in obs.lines if l.get("kind") == "kbd_key" and l.get("state") == 1]
                res.check("keys reach the KDE client by keysym (a, Shift, b)",
                          obs.wait_for("kbd_key", timeout=8, key=30, state=1) is not None
                          and obs.wait_for("kbd_key", timeout=8, key=48, state=1) is not None
                          and obs.wait_for("kbd_key", timeout=8, key=42, state=1) is not None,
                          keys)
                res.check("the portal cursor sprite is delivered when the client draws the cursor",
                          C.wait_log("portal cursor sprites arrive", 10), H.server_log(tail=4))
                real_errors, bad404 = C.benign_console(console_errors, not_found)
                res.check("no browser console errors", not real_errors and not bad404, (real_errors + bad404)[:3])
            finally:
                page.context.close()
                C.close_browser(browser)
        obs.stop()
    finally:
        H.server_stop()
        rig.stop()
    res.summary()
    return res


def main() -> bool:
    modes = [a for a in sys.argv[1:] if a in ("websockets", "webrtc")] or ["websockets"]
    ok = True
    for mode in modes:
        ok = not run(mode).failed() and ok
    return ok


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
