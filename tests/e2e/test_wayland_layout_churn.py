#!/usr/bin/env python3
"""Layout passes that restart both Wayland displays back to back keep both streams alive.

A user fullscreening either display over and over, or dragging a browser edge, has the
server restart every live capture on each layout pass: the resized display's encoder
session is reconfigured in place and the unchanged sibling's is left as it is. Two hardware
sessions torn down and rebuilt back to back on one GPU is what has wedged an NVENC driver
before (a GPU page fault in the kernel log, then a capture thread blocked in the driver for
good), so this suite churns those passes on both transports: two Chromium pages toggle
their viewports between a windowed and a fullscreen-like size, one at a time and both at
once, with a nested labwc session composited too when one is installed, and both streams
have to decode new frames after every pass. A stream that stays frozen is reported with
py-spy and gdb stack traces of the server, when those tools are present, so a lockup that
only shows on some hardware leaves its evidence behind.

Usage: python3 tests/e2e/test_wayland_layout_churn.py [websockets|webrtc]
"""
import importlib.util
import os
import random
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import core_lib as C
import helpers as H

PASSES = 24
STALL_SECONDS = 15.0
MAX_GAP_SECONDS = 0.6
PRIMARY_SIZES = [(1600, 838), (1920, 1080)]
SECONDARY_SIZES = [(1280, 720), (1920, 1080)]


def log_trouble(line: str) -> bool:
    """A compositor refusal, a dropped frame or a crash; the audio capture's own retries are
    not this suite's to judge."""
    if line.startswith("[pcmflux]"):
        return False
    if "[Wayland]" in line and any(t in line for t in ("rejected", "failed", "not signaled")):
        return True
    return any(t in line for t in ("HW Encode Error", "Traceback", "panicked"))


def spawn_labwc(socket: str):
    """labwc nested on the capture socket, as the images run it, or None without a labwc."""
    if not shutil.which("labwc"):
        return None
    env = dict(os.environ, WAYLAND_DISPLAY=socket, WLR_BACKENDS="wayland",
               XDG_RUNTIME_DIR=os.environ.get("XDG_RUNTIME_DIR", H.WORKDIR), WLR_WL_OUTPUTS="1")
    env.pop("DISPLAY", None)
    log = open(os.path.join(H.WORKDIR, "labwc.log"), "w")
    proc = H.spawn(["labwc"], env=env, stdout=log, stderr=subprocess.STDOUT)
    time.sleep(2.0)
    if proc.poll() is not None:
        return None
    return proc


def frames_taken(page, mode: str):
    """Frames the page has taken in: decoded WebRTC video frames, or WebSocket video chunks."""
    try:
        if mode == "websockets":
            return page.evaluate("window.videoChunksReceived || 0")
        return page.evaluate("""(() => {
          const v = document.querySelector('video');
          if (!v) return -1;
          const q = v.getVideoPlaybackQuality ? v.getVideoPlaybackQuality() : null;
          return q ? q.totalVideoFrames : (v.webkitDecodedFrameCount || 0);
        })()""")
    except Exception as e:
        return f"evaluate failed: {e}"


def wait_video(page, mode: str, timeout: float):
    if mode == "websockets":
        return C.wait_ws_video(page, timeout=timeout)
    return C.wait_wr_video(page, timeout=timeout)


def streams_advance(pages: dict, mode: str, stall: float) -> list:
    """The pages that took in no new frame within `stall` seconds."""
    before = {name: frames_taken(page, mode) for name, page in pages.items()}
    pending = set(pages)
    deadline = time.time() + stall
    while pending and time.time() < deadline:
        time.sleep(0.25)
        for name in list(pending):
            now = frames_taken(pages[name], mode)
            if isinstance(now, int) and isinstance(before[name], int) and now > before[name]:
                pending.discard(name)
    return sorted(pending)


def server_stacks() -> str:
    """py-spy and gdb stack traces of every server process, with whichever tool is installed."""
    out = []
    for pid in sorted(H.server_pids()):
        for tool, cmd in (("py-spy", ["py-spy", "dump", "--pid", str(pid), "--native"]),
                          ("gdb", ["gdb", "-p", str(pid), "-batch", "-ex", "set pagination off",
                                   "-ex", "thread apply all bt"])):
            if not shutil.which(tool):
                out.append(f"({tool} not installed)")
                continue
            try:
                run = subprocess.run(cmd, capture_output=True, text=True, timeout=240)
                out.append(f"===== {tool} pid {pid} =====\n{run.stdout}{run.stderr}")
            except Exception as e:
                out.append(f"===== {tool} pid {pid} failed: {e}")
    return "\n".join(out)


def drive(res: "H.Results", mode: str) -> None:
    from playwright.sync_api import sync_playwright
    socket = H.capture_socket()
    res.check("the compositor announced its socket", bool(socket), socket)
    labwc = spawn_labwc(socket) if socket else None
    if labwc is None:
        res.skip("a nested labwc session is composited", "labwc not installed or not started")
    try:
        with sync_playwright() as p:
            browser, page, _errors, _not_found = C.launch_chrome(p, mode=mode)
            try:
                info = wait_video(page, mode, 60)
                res.check("primary video flows", bool(info), info)
                dpage = C.new_page(browser.contexts[0], mode=mode, url_hash="#display2")
                info2 = wait_video(dpage, mode, 90)
                res.check("secondary video flows beside the primary", bool(info2), info2)
                if not (info and info2):
                    return
                pages = {"primary": page, "secondary": dpage}
                rng = random.Random(11)
                mark = len(H.server_log())
                frozen = None
                pi = si = 1
                for i in range(PASSES):
                    kind = ("primary", "secondary", "both")[i % 3]
                    if kind in ("primary", "both"):
                        pi ^= 1
                        page.set_viewport_size({"width": PRIMARY_SIZES[pi][0], "height": PRIMARY_SIZES[pi][1]})
                    if kind in ("secondary", "both"):
                        si ^= 1
                        dpage.set_viewport_size({"width": SECONDARY_SIZES[si][0], "height": SECONDARY_SIZES[si][1]})
                    stalled = streams_advance(pages, mode, STALL_SECONDS)
                    if stalled:
                        frozen = (f"pass {i} ({kind}: primary {PRIMARY_SIZES[pi]}, secondary "
                                  f"{SECONDARY_SIZES[si]}) left {stalled} frozen for {STALL_SECONDS}s")
                        print(server_stacks(), flush=True)
                        print("===== server log tail =====\n" + H.server_log(tail=80), flush=True)
                        break
                    time.sleep(rng.random() * MAX_GAP_SECONDS)
                res.check(f"every one of {PASSES} layout passes kept both streams decoding",
                          frozen is None, frozen or "")
                trouble = [line for line in H.server_log()[mark:].splitlines() if log_trouble(line)]
                res.check("the server refused or failed nothing during the passes",
                          not trouble, trouble[:3])
                if frozen is None:
                    dpage.close()
            finally:
                C.close_browser(browser)
    finally:
        if labwc is not None:
            labwc.terminate()


def main() -> "H.Results":
    mode = sys.argv[1] if len(sys.argv) > 1 else "websockets"
    res = H.Results(f"wl-layout-churn-{mode}")
    if importlib.util.find_spec("pixelflux") is None:
        H.skip_suite("pixelflux is not installed")
    H.server_start(mode=mode, wayland=True)
    try:
        drive(res, mode)
    finally:
        H.server_stop()
    res.summary()
    return res


if __name__ == "__main__":
    main()
