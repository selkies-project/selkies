#!/usr/bin/env python3
"""How far the remote pointer travels for a given movement of a real mouse.

Locked motion reaches the client as movementX/Y, which browsers report in whole
CSS pixels while carrying their own sub-pixel remainder. On a client whose
scale is not a whole number that arrives as a stream of deltas that only add up
over several events, so what the client does with each one decides whether a
flick lands where the hand aimed. Motion here is injected with XTEST into an
installed browser holding a real pointer lock, and the deltas it reports are
fed through the client's own relative-motion path: what is measured is the
engine, not a model of it.

The engines are also asked for raw (unadjusted) movement, which is what removes
the local acceleration curve from locked motion where an engine offers it.
"""
import http.server
import json
import os
import shutil
import socketserver
import subprocess
import sys
import threading
import time
from typing import Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
from core_lib import CHROME_PATH, FIREFOX_PATH

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
PAGE = "/tests/tools/pointer_motion_page.html"
PORT = int(os.environ.get("E2E_MOTION_PORT", "18099"))
LATEST: dict = {}
WARMED: set = set()

# (label, motions, pixels each, seconds between): a slow careful aim, a normal
# drag, and a flick, all at one device pixel of travel per motion or more.
PATTERNS = (("slow", 40, 1, 0.025),
            ("steady", 120, 2, 0.006),
            ("flick", 60, 8, 0.002))


class Handler(http.server.SimpleHTTPRequestHandler):
    """Serves the repo so the page can import the client's own modules."""

    def __init__(self, *a, **kw):
        super().__init__(*a, directory=REPO, **kw)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        # Swapped in whole: emptying it around the read would show the checks a
        # report that has no numbers in it yet.
        try:
            report = json.loads(body)
        except ValueError:
            report = None
        if report is not None:
            seq = LATEST.get("_seq", 0) + 1
            LATEST.clear()
            LATEST.update(report)
            LATEST["_seq"] = seq
        self.send_response(204)
        self.end_headers()

    def log_message(self, *a):
        pass


def serve() -> None:
    """Start the page server on a daemon thread.

    Threaded: the same server hands out the client's modules and takes the
    page's reports, and a browser fetching one must not hold up the other.
    """
    class Threaded(socketserver.ThreadingMixIn, http.server.HTTPServer):
        allow_reuse_address = True
        daemon_threads = True

    httpd = Threaded(("127.0.0.1", PORT), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()


def wait_for(pred, timeout: float) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return True
        time.sleep(0.2)
    return False


def settle(timeout: float = 8.0) -> None:
    """Wait until the page reports the same travel three times running.

    Warping the pointer under a lock reaches the engine as one large delta, and
    a baseline read before that delta is reported puts it in the measurement
    instead. The page posts every 200 ms, so quiet is three identical posts.
    """
    end = time.time() + timeout
    prev, stable, last_seq = None, 0, None
    while time.time() < end:
        seq = LATEST.get("_seq")
        if seq != last_seq:
            last_seq = seq
            now = (LATEST.get("moves"), LATEST.get("travelX"))
            stable = stable + 1 if now == prev else 0
            prev = now
            if stable >= 2:
                return
        time.sleep(0.05)


def binary(browser: str) -> Optional[str]:
    """The installed browser to drive, or None when it is not on this machine.

    The suite-wide overrides name a browser that is not on PATH, and the rest
    of the e2e tier resolves them the same way.
    """
    named = CHROME_PATH if browser == "chrome" else FIREFOX_PATH
    if named:
        return named
    return shutil.which("google-chrome" if browser == "chrome" else "firefox")


def launch(browser: str, profile: str, dpr: float) -> subprocess.Popen:
    """Start an installed browser on the test display at a device pixel ratio."""
    url = f"http://localhost:{PORT}{PAGE}"
    if browser == "firefox":
        with open(os.path.join(profile, "user.js"), "w") as fh:
            fh.write(f'user_pref("layout.css.devPixelsPerPx", "{dpr}");\n')
            fh.write('user_pref("browser.shell.checkDefaultBrowser", false);\n')
            # Nothing may open in front of the probe page: pointer lock needs a
            # click on it, and a welcome tab would take that click instead.
            fh.write('user_pref("browser.aboutwelcome.enabled", false);\n')
            fh.write('user_pref("browser.startup.firstrunSkipsHomepage", true);\n')
            fh.write('user_pref("browser.startup.homepage_override.mstone", "ignore");\n')
            fh.write('user_pref("datareporting.policy.firstRunURL", "");\n')
        cmd = [binary("firefox"), "--no-remote", "--profile", profile,
               "--width", "1600", "--height", "900", url]
    else:
        cmd = [binary("chrome"), "--no-first-run", "--no-default-browser-check",
               "--disable-gpu", "--user-data-dir=" + profile,
               f"--force-device-scale-factor={dpr}",
               "--window-position=0,0", "--window-size=1600,900", url]
    env = dict(os.environ, DISPLAY=H.require_display())
    return H.spawn(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def measure(res, browser: str, dpr: float) -> None:
    """Lock the pointer in `browser` and record the travel for each pattern."""
    from selkies.Xlib import X
    from selkies.Xlib.ext import xtest

    profile = os.path.join(H.WORKDIR, f"motion-profile-{browser}")
    if browser == "firefox" and browser not in WARMED:
        # A first run writes the profile and shows onboarding, neither of which may
        # happen in the window holding pointer lock; user.js is rewritten per launch.
        shutil.rmtree(profile, ignore_errors=True)
        os.makedirs(profile, exist_ok=True)
        subprocess.run([binary("firefox"), "--headless", "--profile", profile,
                        "--screenshot", os.devnull, "about:blank"],
                       capture_output=True, timeout=180)
        WARMED.add(browser)
    elif browser != "firefox":
        shutil.rmtree(profile, ignore_errors=True)
    os.makedirs(profile, exist_ok=True)
    LATEST.clear()
    label = f"{browser} at dpr {dpr}"
    d = H.x_display()
    proc = launch(browser, profile, dpr)
    try:
        if not wait_for(lambda: bool(LATEST), 90):
            res.check(f"{label}: the probe page loads", False, "no report")
            return
        root = d.screen().root
        # A browser still opening its window swallows the first click; a machine
        # under load is not a product regression, so the click is repeated.
        for _ in range(15):
            root.warp_pointer(700, 450)
            d.sync()
            time.sleep(0.5)
            xtest.fake_input(d, X.ButtonPress, 1)
            d.flush()
            time.sleep(0.05)
            xtest.fake_input(d, X.ButtonRelease, 1)
            d.flush()
            if wait_for(lambda: LATEST.get("locked"), 3):
                break
        if not LATEST.get("locked"):
            res.check(f"{label}: the pointer locks", False, LATEST.get("err", ""))
            return
        # Raw movement is refused by every engine on Linux, so the fallback to a
        # plain lock is the path this runs on; a lock is what matters either way.
        res.check(f"{label}: the pointer locks", True, LATEST.get("lockPath", ""))

        for name, count, delta, gap in PATTERNS:
            root.warp_pointer(700, 450)
            d.sync()
            settle()
            mark = dict(LATEST)
            for _ in range(count):
                xtest.fake_input(d, X.MotionNotify, detail=True, root=X.NONE,
                                 x=delta, y=0)
                d.flush()
                if gap:
                    time.sleep(gap)
            settle()
            injected = count * delta
            travelled = LATEST.get("travelX", 0) - mark.get("travelX", 0)
            # Tolerance: one event swallowed around the warp, or one percent of
            # sub-pixel carry caught mid-flight; a scale error is tens of percent.
            res.check(f"{label}: a {name} move of {injected} px travels {injected} px",
                      abs(travelled - injected) <= max(delta, injected // 100),
                      f"{travelled}")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        d.close()


def run() -> "H.Results":
    res = H.Results("pointer-motion")
    browsers = [b for b in ("chrome", "firefox") if binary(b)]
    if not browsers:
        H.skip_suite("neither Chrome nor Firefox is installed")
    serve()
    for browser in browsers:
        for dpr in (1, 1.25, 1.5):
            measure(res, browser, dpr)
    res.summary()
    return res


if __name__ == "__main__":
    r = run()
    sys.exit(0 if not r.failed() else 1)
