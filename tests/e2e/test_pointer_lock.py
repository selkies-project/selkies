#!/usr/bin/env python3
"""Pointer lock against the server: the remote pointer travels by the deltas.

A locked page gets no positions from the browser, only movementX/Y, and the
client turns those into `m2,dx,dy` messages the server injects as relative
motion (XTEST on X11, the compositor's relative pointer on Wayland). Here a
real pointer lock is taken in the browser (the client's Ctrl+Shift+click on
the stream) and every move the test makes while it holds must move the server
pointer by exactly that much — a single move, a run of small ones, and a
negative one — and releasing the lock must put the client back on absolute
positions. The wire is tapped at WebSocket/RTCDataChannel send so the
messages that carried the motion are checked as well as the pointer; the
pointer itself is read from the X server or, on Wayland, from the observer
surface the compositor delivers pointer events to. Locked deltas are scaled
by the client from CSS pixels to stream pixels, so the expectations follow
the stream size the server realized; that the stream matches the window at
connect is its own check.

Where the pointer ends up cannot tell a delta from a warp to the same spot,
and a game can: it reads relative motion (XInput2 raw motion, the Wayland
relative pointer) and never the position. So the last block puts an SDL2
window in relative mouse mode over the desktop (tests/tools/sdl_relative_probe.py),
locks again, and requires every move to reach it as exactly the delta the
wire carried, one event per message, and a key pressed under the lock to
arrive as that key. Without a libSDL2 to load the block is skipped.

A desktop selector runs that block under the session manager the images run
the game under: openbox or kwin_x11 managing the X test display, or labwc or
kwin_wayland nested on the capture compositor with the game as a client of
the nested one, which then relays the motion it is given as a Wayland client
itself. Without that manager installed the block is skipped. Under a nested
kwin_wayland the delta check is recorded as skipped: KWin relays its host's
pointer as absolute motion only, an open gap rather than a fault in the path.

    python3 tests/e2e/test_pointer_lock.py ws-x11|wr-x11|ws-wl|wr-wl
    python3 tests/e2e/test_pointer_lock.py ws-x11-openbox|ws-x11-kwin|ws-wl-labwc|ws-wl-kwin

Headless Chromium's full build is driven (not the headless shell, whose
locked movement deltas do not add up), on both transports and backends.

"""
import json
import os
import shutil
import subprocess
import sys
import threading
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

START = (640, 360)
# The stream the server realizes for the browser window, and so the desktop the
# nested compositors are sized to.
WINDOW = (1280, 720)
# (dx, dy, repeat): one plain move, a run of small ones, and a move back.
MOVES = ((60, 40, 1), (5, -3, 10), (-200, 100, 1))
# What the game window is shown: a flick each way, a one-axis nudge, a diagonal.
GAME_MOVES = ((12, 7), (-5, 9), (80, -30), (3, 0), (0, -4))

# Motion messages off the shared wire tap, whichever thread owns the socket.
MOVES_JS = ("window.__wireSent.filter(d => typeof d === 'string' && "
            "(d.startsWith('m,') || d.startsWith('m2,')))")


def launch(p: Any, mode: str) -> tuple:
    """The full Chromium build on the stream page, with the wire tap installed."""
    kw = {"headless": True, "args": C.BROWSER_ARGS}
    if C.CHROME_PATH:
        kw["executable_path"] = C.CHROME_PATH
    else:
        kw["channel"] = "chromium"
    browser = p.chromium.launch(**kw)
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(C.WIRE_TAP_JS)
    page = ctx.new_page()
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(H.BASE_URL + "/", wait_until="load")
    return browser, page, errors


class Pointer:
    """Where the server's pointer is, read from the X server or the Wayland
    observer, polled until it reaches an expected spot or time runs out."""

    def __init__(self, obs: Optional["H.WlObs"]) -> None:
        self.obs = obs

    def read(self) -> Optional[tuple]:
        if self.obs is None:
            return C.x11_mouse_pos()
        seen = [(line["x"], line["y"]) for line in self.obs.lines
                if line.get("kind") in ("ptr_enter", "ptr_motion")]
        return seen[-1] if seen else None

    def wait(self, expected: tuple, timeout: float = 6) -> Optional[tuple]:
        deadline = time.time() + timeout
        pos = self.read()
        while time.time() < deadline:
            pos = self.read()
            if pos is not None and tuple(int(round(v)) for v in pos) == expected:
                return pos
            time.sleep(0.1)
        return pos


def at(pos: Optional[tuple], expected: tuple) -> bool:
    return pos is not None and tuple(int(round(v)) for v in pos) == expected


def runtime_dir() -> str:
    return os.environ.get("XDG_RUNTIME_DIR", H.WORKDIR)


class Desktop:
    """The session manager a desktop selector names, started the way the images
    run it: openbox or kwin_x11 on the X test display; labwc or kwin_wayland
    nested on the capture compositor, sized to the stream, publishing the
    socket its own clients connect to."""

    def __init__(self, name: str, wayland: bool) -> None:
        self.name = name
        self.wayland = wayland
        self.proc: Optional[subprocess.Popen] = None
        self.socket: Optional[str] = None
        self.why = ""

    def start(self, capture: str) -> bool:
        binary = {"openbox": "openbox", "kwin": "kwin_wayland" if self.wayland else "kwin_x11",
                  "labwc": "labwc"}[self.name]
        if not shutil.which(binary):
            self.why = f"{binary} not installed"
            return False
        if self.name == "kwin" and not shutil.which("dbus-run-session"):
            self.why = "dbus-run-session not installed"
            return False
        log = open(os.path.join(H.WORKDIR, f"{binary}.log"), "w")
        if not self.wayland:
            display = H.require_display()
            env = {**os.environ, "DISPLAY": display}
            cmd = ["openbox", "--replace"] if self.name == "openbox" else \
                ["dbus-run-session", "--", "kwin_x11", "--replace"]
            self.proc = H.spawn(cmd, env=env, stdout=log, stderr=subprocess.STDOUT)
            return self._wait(lambda: self._x11_managed(display), f"{binary} never took the display")
        if not capture:
            self.why = "the capture compositor announced no socket"
            return False
        env = {"PATH": os.environ.get("PATH", ""), "HOME": os.path.expanduser("~"),
               "XDG_RUNTIME_DIR": runtime_dir(), "WAYLAND_DISPLAY": capture}
        if self.name == "labwc":
            # labwc picks its socket name; its startup command runs with that name
            # exported, so the command is what publishes it.
            marker = os.path.join(H.WORKDIR, "labwc-session-socket")
            if os.path.exists(marker):
                os.unlink(marker)
            env.update({"WLR_BACKENDS": "wayland", "WLR_WL_OUTPUTS": "1"})
            cmd = ["labwc", "-s", f"sh -c 'echo \"$WAYLAND_DISPLAY\" > {marker}'"]
            self.proc = H.spawn(cmd, env=env, stdout=log, stderr=subprocess.STDOUT)
            if not self._wait(lambda: os.path.exists(marker) and os.path.getsize(marker) > 0,
                              "labwc never published its socket"):
                return False
            self.socket = open(marker).read().strip()
        else:
            self.socket = f"wayland-kwin-{os.getpid()}"
            # KWin hides its restricted globals (fake input, screencast) from clients
            # it did not start unless told not to; pixelflux reaches a nested KWin
            # only under this, as the KDE images run it.
            env["KWIN_WAYLAND_NO_PERMISSION_CHECKS"] = "1"
            cmd = ["dbus-run-session", "--", "kwin_wayland", "--wayland-display", capture,
                   "--socket", self.socket, "--width", str(WINDOW[0]), "--height", str(WINDOW[1]),
                   "--no-lockscreen", "--no-global-shortcuts", "--no-kactivities"]
            self.proc = H.spawn(cmd, env=env, stdout=log, stderr=subprocess.STDOUT)
            path = os.path.join(runtime_dir(), self.socket)
            if not self._wait(lambda: os.path.exists(path), "kwin_wayland never opened its socket"):
                return False
        # The nested compositor's own window has to map on the capture compositor
        # before the pointer can reach anything inside it.
        time.sleep(2.0)
        return True

    def _wait(self, ready: Any, why: str, timeout: float = 20) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if ready():
                return True
            if self.proc is not None and self.proc.poll() is not None:
                self.why = f"{self.name} exited with {self.proc.returncode}: {H.tail(self._log_path(), 3)}"
                return False
            time.sleep(0.25)
        self.why = why
        return False

    def _log_path(self) -> str:
        binary = {"openbox": "openbox", "kwin": "kwin_wayland" if self.wayland else "kwin_x11",
                  "labwc": "labwc"}[self.name]
        return os.path.join(H.WORKDIR, f"{binary}.log")

    @staticmethod
    def _x11_managed(display: str) -> bool:
        """Whether a window manager holds the display, by the check window it publishes."""
        try:
            out = subprocess.run(["xprop", "-root", "_NET_SUPPORTING_WM_CHECK"], capture_output=True,
                                 text=True, timeout=5, env={**os.environ, "DISPLAY": display}).stdout
        except (OSError, subprocess.TimeoutExpired):
            return False
        return "window id" in out

    def stop(self) -> None:
        if self.proc is None:
            return
        try:
            self.proc.terminate()
            self.proc.wait(5)
        except Exception:
            try:
                self.proc.kill()
            except Exception:
                pass


class GameProbe:
    """The SDL2 relative-mode window (tests/tools/sdl_relative_probe.py) on the
    server's display, its JSON lines collected from a reader thread. On Wayland
    it connects to `socket`: the capture compositor's, or a nested session's."""

    def __init__(self, wayland: bool, socket: Optional[str] = None) -> None:
        env = {"PATH": os.environ.get("PATH", ""), "HOME": os.path.expanduser("~"),
               "XDG_RUNTIME_DIR": runtime_dir()}
        if os.environ.get("SDL2_LIB"):
            env["SDL2_LIB"] = os.environ["SDL2_LIB"]
        if wayland:
            env["SDL_VIDEODRIVER"] = "wayland"
            env["WAYLAND_DISPLAY"] = socket or ""
        else:
            env["SDL_VIDEODRIVER"] = "x11"
            env["DISPLAY"] = H.require_display()
        self.proc = H.spawn([H.PYTHON, os.path.join(H.TOOLS, "sdl_relative_probe.py"), "120"],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env)
        self.lines: list = []
        self.noise: list = []
        # Kept as a log the runner collects, so a run that saw nothing says what it did see.
        log = open(os.path.join(H.WORKDIR, "game-probe.log"), "w")

        def read() -> None:
            for line in self.proc.stdout:
                log.write(line)
                log.flush()
                line = line.strip()
                if line.startswith("{"):
                    try:
                        self.lines.append(json.loads(line))
                        continue
                    except ValueError:
                        pass
                if line:
                    self.noise.append(line)
            log.close()
        threading.Thread(target=read, daemon=True).start()

    def wait(self, kind: str, timeout: float, **match: Any) -> Optional[dict]:
        """First line of `kind` whose fields equal `match`, or None on timeout."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            for line in self.lines:
                if line.get("kind") == kind and all(line.get(k) == v for k, v in match.items()):
                    return line
            if self.proc.poll() is not None:
                break
            time.sleep(0.05)
        return None

    def unavailable(self) -> str:
        """Why the window could not run, when it said so."""
        for line in self.lines:
            if line.get("kind") == "unavailable":
                return line.get("reason", "unavailable")
        return " ".join(self.noise)[:160]

    def events(self, kind: str, start: int, count: int, timeout: float) -> list:
        """The `kind` lines after index `start`, waited for until `count` are in."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            got = [line for line in self.lines[start:] if line.get("kind") == kind]
            if len(got) >= count:
                break
            time.sleep(0.05)
        return [line for line in self.lines[start:] if line.get("kind") == kind]

    def stop(self) -> None:
        try:
            self.proc.terminate()
            self.proc.wait(5)
        except Exception:
            pass


def take_lock(page: Any, where: tuple) -> bool:
    """The client's own gesture, Ctrl+Shift+click on the stream, and whether it locked."""
    page.keyboard.down("Control")
    page.keyboard.down("Shift")
    page.mouse.click(*where)
    page.keyboard.up("Shift")
    page.keyboard.up("Control")
    deadline = time.time() + 5
    while time.time() < deadline:
        if page.evaluate("document.pointerLockElement !== null"):
            return True
        time.sleep(0.1)
    return False


def wire_deltas(messages: list) -> list:
    """The non-zero relative deltas the wire carried, in order."""
    out = []
    for m in messages:
        if m.startswith("m2,"):
            parts = m.split(",")
            d = (int(parts[1]), int(parts[2]))
            if d != (0, 0):
                out.append(d)
    return out


def game_view(page: Any, wayland: bool, res: "H.Results", socket: Optional[str] = None,
              desktop: Optional[str] = None) -> None:
    """What a game over the desktop sees of a locked pointer and the keyboard."""
    probe = GameProbe(wayland, socket)
    try:
        mode = probe.wait("relative_mode", 20)
        if mode is None:
            res.skip("a game window over the desktop takes relative mouse mode", probe.unavailable())
            return
        res.check("a game window over the desktop takes relative mouse mode", mode.get("on"), mode)
        # The window has to hold the pointer before it may lock it: an absolute
        # move puts the pointer over it and a click gives it the keyboard.
        time.sleep(0.5)
        page.mouse.move(START[0] + 1, START[1] + 1)
        page.mouse.click(START[0] + 1, START[1] + 1)
        entered = probe.wait("window", 8, event="enter")
        res.check("the pointer reaches the game window", entered is not None, probe.lines[-3:])
        res.check("the game window locks the pointer again", take_lock(page, START))
        time.sleep(0.5)
        probe_mark = len(probe.lines)
        wire_mark = len(page.evaluate(MOVES_JS))
        cursor = START
        for dx, dy in GAME_MOVES:
            cursor = (cursor[0] + dx, cursor[1] + dy)
            page.mouse.move(*cursor)
            time.sleep(0.08)
        wire = wire_deltas(moves_since(page, wire_mark))
        seen = [(m["dx"], m["dy"]) for m in probe.events("motion", probe_mark, len(wire), 6)]
        res.check("the wire carried every game move as relative motion", len(wire) == len(GAME_MOVES), wire)
        if wayland and desktop == "kwin":
            # KWin's nested backend relays its host's pointer as absolute motion
            # only, so a game under it reads no deltas; recorded as the open gap it
            # is rather than as a failure of the path under test.
            res.skip("every locked move reaches the game as the delta the wire carried, one event each",
                     f"a nested kwin_wayland relays absolute motion only; game saw {seen}")
        else:
            res.check("every locked move reaches the game as the delta the wire carried, one event each",
                      seen == wire, f"game {seen} wire {wire}")
        key_mark = len(probe.lines)
        page.keyboard.press("w")
        keys = [(k["down"], k["sym"]) for k in probe.events("key", key_mark, 2, 6)]
        res.check("a key pressed under the lock reaches the game", keys == [(True, ord("w")), (False, ord("w"))], keys)
        page.evaluate("document.exitPointerLock()")
        time.sleep(0.3)
    finally:
        probe.stop()


def wait_stream_size(page: Any, mode: str, size: tuple, timeout: float = 8) -> Optional[dict]:
    """The stream's dimensions, polled until they match `size` or time runs out."""
    deadline = time.time() + timeout
    info = None
    while time.time() < deadline:
        info = C.wait_ws_video(page, timeout=2) if mode == "websockets" else C.wait_wr_video(page, timeout=2)
        if info and (info["w"], info["h"]) == size:
            return info
        time.sleep(0.5)
    return info


def moves_since(page: Any, start: int) -> list:
    return page.evaluate(MOVES_JS)[start:]


def relative_sum(messages: list) -> tuple:
    dx = dy = 0
    for m in messages:
        if m.startswith("m2,"):
            parts = m.split(",")
            dx += int(parts[1])
            dy += int(parts[2])
    return dx, dy


def run(mode: str, wayland: bool, desktop: Optional[str], res: "H.Results") -> None:
    H.server_start(mode=mode, wayland=wayland)
    obs = None
    capture = ""
    try:
        if wayland:
            capture = H.capture_socket()
            res.check("the capture compositor announced its socket", bool(capture), capture)
            obs = H.WlObs(capture)
            res.check("wl observer mapped", obs.ready(20))
        pointer = Pointer(obs)
        with sync_playwright() as p:
            browser, page, errors = launch(p, mode)
            try:
                video = C.wait_ws_video(page, timeout=30) if mode == "websockets" else C.wait_wr_video(page)
                res.check("video flowing", bool(video), video)
                video = wait_stream_size(page, mode, (1280, 720)) or video
                res.check("stream follows the 1280x720 window at connect",
                          video and (video["w"], video["h"]) == (1280, 720), video)
                # Locked deltas and positions are scaled by the client onto the
                # stream the server realized; a stream that did not follow the
                # window still has to move the pointer by exactly what it scaled to.
                scale = (video["w"] / 1280.0) if video else 1.0

                def server(css: tuple) -> tuple:
                    return (int(round(css[0] * scale)), int(round(css[1] * scale)))

                time.sleep(1.0)
                page.mouse.move(*START)
                res.check("absolute move lands the pointer at the start",
                          at(pointer.wait(server(START)), server(START)), pointer.read())

                res.check("Ctrl+Shift+click takes the pointer lock", take_lock(page, START))
                time.sleep(0.5)

                x, y = START
                cursor = (x, y)
                for dx, dy, repeat in MOVES:
                    mark = len(page.evaluate(MOVES_JS))
                    for _ in range(repeat):
                        cursor = (cursor[0] + dx, cursor[1] + dy)
                        page.mouse.move(*cursor)
                        time.sleep(0.05)
                    x, y = x + dx * repeat, y + dy * repeat
                    pos = pointer.wait(server((x, y)))
                    label = f"{repeat} x ({dx},{dy})" if repeat > 1 else f"({dx},{dy})"
                    res.check(f"locked move {label} carries the server pointer to {server((x, y))}",
                              at(pos, server((x, y))), pos)
                    sent = moves_since(page, mark)
                    res.check(f"locked move {label} went out as relative motion",
                              sent and all(m.startswith("m2,") for m in sent), sent[:4])
                    res.check(f"locked move {label} deltas add up on the wire",
                              relative_sum(sent) == server((dx * repeat, dy * repeat)), relative_sum(sent))

                mark = len(page.evaluate(MOVES_JS))
                page.evaluate("document.exitPointerLock()")
                deadline = time.time() + 5
                while time.time() < deadline and page.evaluate("document.pointerLockElement !== null"):
                    time.sleep(0.1)
                res.check("lock released", page.evaluate("document.pointerLockElement === null"))
                time.sleep(0.3)
                target = (300, 200)
                page.mouse.move(*target)
                pos = pointer.wait(server(target))
                res.check("after release an absolute move lands where it says", at(pos, server(target)), pos)
                sent = moves_since(page, mark)
                res.check("after release the motion goes out as positions",
                          sent and sent[-1].startswith("m,"), sent[-2:])
                session = Desktop(desktop, wayland) if desktop else None
                try:
                    if session is None:
                        game_view(page, wayland, res, capture or None)
                    elif session.start(capture):
                        res.check(f"{desktop} manages the session", True, session.socket or "")
                        game_view(page, wayland, res, session.socket, desktop)
                    else:
                        res.skip(f"{desktop} manages the session", session.why)
                finally:
                    if session is not None:
                        session.stop()
                res.check("no page errors", not errors, "; ".join(errors)[:200])
            finally:
                browser.close()
    finally:
        if obs is not None:
            obs.stop()
        H.server_stop()


SELECTORS = ("ws-x11", "wr-x11", "ws-wl", "wr-wl",
             "ws-x11-openbox", "ws-x11-kwin", "ws-wl-labwc", "ws-wl-kwin")


def main() -> bool:
    which = sys.argv[1] if len(sys.argv) > 1 else "ws-x11"
    if which not in SELECTORS:
        raise SystemExit(f"unknown selector {which!r}; one of {SELECTORS}")
    parts = which.split("-")
    transport, backend = parts[0], parts[1]
    desktop = parts[2] if len(parts) > 2 else None
    res = H.Results(f"pointer-lock-{which}")
    run("websockets" if transport == "ws" else "webrtc", backend == "wl", desktop, res)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
