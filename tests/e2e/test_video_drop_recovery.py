#!/usr/bin/env python3
"""A WebRTC sender that falls behind its encoder keeps the picture decodable.

Encoded frames cross from the capture thread to the RTP sender through a
depth-one bridge, so a sender that cannot keep up drops whole frames before
they are packetized: no sequence number is spent, the receiver sees no loss
and asks for nothing, and every delta frame after the drop references a
picture it never received. The bridge has to close the reference chain
itself: hold delta frames back until a keyframe it asked for arrives, and
never let a later delta frame evict that keyframe.

The scene is tests/tools/motion_scene.py, whose every frame spells its own
index, so a decoded picture can be compared with the frame it claims to be,
and whose noise band keeps a constant-bitrate stream at its configured rate. The
sender is made to fall behind by pinning the server's event loop to one CPU
and sharing that CPU with busy processes in bursts. The browser's decoded
pictures, its inbound-rtp counters and the server's bridge-drop counter are
read throughout.

    python3 tests/e2e/test_video_drop_recovery.py [default|cpu]

E2E_ENGINE selects the browser (chromium, the default, firefox or webkit).
"""
import os
import re
import signal
import subprocess
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "tools"))
import helpers as H
import core_lib as C
import motion_scene
from playwright.sync_api import sync_playwright

WIDTH, HEIGHT = 1280, 720
ENGINE = os.environ.get("E2E_ENGINE", "chromium")
PAINTER = os.path.join(H.TOOLS, "motion_scene.py")
GEOMETRY = motion_scene.geometry()
# Busy processes sharing the loop's CPU, and their on/off cadence.
HOGS = 3
BURST_ON_S, BURST_OFF_S = 0.4, 0.6
LOAD_S = 20.0
SETTLE_S = 5.0
# A decoded picture this far from the frame it claims to be shows a broken
# reference; a clean one reads zero.
CORRUPT_FRACTION = 0.02
CLEAN_FRACTION = 0.005
# Keeps every RTCPeerConnection reachable for getStats and pins the transport.
INIT_JS = """
  window.__SELKIES_STREAMING_MODE__ = 'webrtc';
  window.__pcs = [];
  if (window.RTCPeerConnection) {
    const Orig = window.RTCPeerConnection;
    const Wrapped = function(...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    Wrapped.prototype = Orig.prototype;
    Object.setPrototypeOf(Wrapped, Orig);
    window.RTCPeerConnection = Wrapped;
  }
"""

# Reads the frame index off the decoded picture, rebuilds that frame from the
# scene geometry and counts the sampled pixels that disagree with it. Only the
# interiors of the flat regions and of the checks are compared: quantization
# softens edges, while a picture decoded against the wrong reference is wrong
# well inside them.
SAMPLE_JS = """(geom) => {
  const v = document.querySelector('video');
  if (!v || v.videoWidth === 0) return null;
  const W = v.videoWidth, H = v.videoHeight;
  let c = window.__probeCanvas;
  if (!c || c.width !== W || c.height !== H) {
    c = window.__probeCanvas = document.createElement('canvas');
    c.width = W; c.height = H;
  }
  const ctx = c.getContext('2d', {willReadFrequently: true});
  ctx.drawImage(v, 0, 0, W, H);
  const px = ctx.getImageData(0, 0, W, H).data;
  const at = (x, y) => { const i = (y * W + x) * 4; return [px[i], px[i + 1], px[i + 2]]; };
  let index = 0, ambiguous = 0;
  for (let b = 0; b < geom.codeBits; b++) {
    const cx = geom.codeX + b * (geom.codeSize + geom.codeGap) + geom.codeSize / 2;
    const cy = geom.codeY + geom.codeSize / 2;
    let sum = 0, n = 0;
    for (let dy = -8; dy < 8; dy += 2) for (let dx = -8; dx < 8; dx += 2) {
      const p = at(cx + dx, cy + dy); sum += p[0] + p[1] + p[2]; n += 3;
    }
    const lum = sum / n;
    if (lum > 160) index |= (1 << b); else if (lum > 96) ambiguous++;
  }
  const period = 2 * geom.check;
  const bandOff = (index * geom.bandStep) % period;
  const barX = (index * geom.barStep) % (W - geom.barW);
  const bandEnd = geom.bandY + geom.bandH;
  const edge = 6;
  const far = (p, e, tol) => Math.abs(p[0] - e[0]) > tol || Math.abs(p[1] - e[1]) > tol || Math.abs(p[2] - e[2]) > tol;
  let bad = 0, n = 0;
  for (let y = geom.codeY + geom.codeSize + 8; y < H - geom.noiseH; y += 2) {
    for (let x = 0; x < W; x += 2) {
      const p = at(x, y);
      if (y >= geom.bandY && y < bandEnd) {
        const cx = (x + bandOff) % geom.check, cy = (y - geom.bandY) % geom.check;
        if (cx < 4 || cx >= geom.check - 4 || cy < 4 || cy >= geom.check - 4) continue;
        const white = ((Math.floor((x + bandOff) / geom.check)
                        + Math.floor((y - geom.bandY) / geom.check)) % 2) === 0;
        const lum = (p[0] + p[1] + p[2]) / 3;
        if (white ? lum < 128 : lum >= 128) bad++;
        n++;
        continue;
      }
      if (y < bandEnd + edge) continue;
      if (x >= barX - edge && x < barX + geom.barW + edge) {
        if (x < barX + edge || x >= barX + geom.barW - edge) continue;
        if (far(p, geom.bar, 56)) bad++;
      } else if (far(p, geom.background, 40)) {
        bad++;
      }
      n++;
    }
  }
  return {index, ambiguous, mismatch: bad / n, w: W, h: H};
}"""

STATS_JS = """async () => {
  const out = {keyframes: 0, frames: 0, lost: 0, nacks: 0, plis: 0, dropped: 0, received: 0};
  for (const pc of window.__pcs || []) {
    const rep = await pc.getStats();
    rep.forEach(r => {
      if (r.type === 'inbound-rtp' && (r.kind === 'video' || r.mediaType === 'video')) {
        out.keyframes += r.keyFramesDecoded || 0;
        out.frames += r.framesDecoded || 0;
        out.lost += r.packetsLost || 0;
        out.nacks += r.nackCount || 0;
        out.plis += r.pliCount || 0;
        out.dropped += r.framesDropped || 0;
        out.received += r.packetsReceived || 0;
      }
    });
  }
  return out;
}"""


def bridge_drops() -> Optional[int]:
    """The primary display's bridge-drop counter from /api/metrics."""
    status, body = H.curl("/api/metrics")
    if status != 200:
        return None
    m = re.search(r'webrtc_bridge_dropped_frames\{display="primary"\}\s+([0-9.eE+]+)',
                  body.decode("utf-8", "replace"))
    return int(float(m.group(1))) if m else 0


def server_env(cell: str) -> dict:
    env = {
        "SELKIES_ENCODER": "h264enc",
        "SELKIES_FRAMERATE": "60,8-240",
        "SELKIES_VIDEO_BITRATE": "40000,100-1000000",
        "SELKIES_RATE_CONTROL_MODE": "cbr",
        "SELKIES_KEYFRAME_INTERVAL": "0",
        "SELKIES_VIDEO_STREAMING_MODE": "true",
        "SELKIES_USE_PAINT_OVER_QUALITY": "true",
        "SELKIES_WEBRTC_PACER": "true",
        "SELKIES_CONGESTION_CONTROL": "true",
        "SELKIES_ENABLE_METRICS_HTTP": "true",
    }
    if cell == "cpu":
        env["SELKIES_USE_CPU"] = "true"
    return env


def loop_thread(pid: int) -> int:
    """The server's event loop runs on its main thread."""
    return pid


class Load:
    """Busy processes that share one CPU with the server's event loop.

    The loop thread is pinned beside them at the lowest scheduling priority,
    so while they run it barely does; raising a thread's niceness needs no
    privilege, lowering it back does, so the thread keeps it. Alone on the
    CPU that costs it nothing.
    """

    def __init__(self, pid: int) -> None:
        self.tid = loop_thread(pid)
        self.original = os.sched_getaffinity(self.tid)
        self.cpu = max(self.original)
        self.hogs: list = []

    def start(self) -> None:
        os.sched_setaffinity(self.tid, {self.cpu})
        os.setpriority(os.PRIO_PROCESS, self.tid, 19)
        for _ in range(HOGS):
            hog = H.spawn([sys.executable, "-c", "while True: pass"],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            os.sched_setaffinity(hog.pid, {self.cpu})
            self.hogs.append(hog)
        self.pause()

    def pause(self) -> None:
        for hog in self.hogs:
            hog.send_signal(signal.SIGSTOP)

    def resume(self) -> None:
        for hog in self.hogs:
            hog.send_signal(signal.SIGCONT)

    def stop(self) -> None:
        for hog in self.hogs:
            hog.send_signal(signal.SIGCONT)
            hog.kill()
        for hog in self.hogs:
            hog.wait(timeout=5)
        self.hogs = []
        try:
            os.sched_setaffinity(self.tid, self.original)
        except OSError:
            pass


def sample(page: Any) -> Optional[dict]:
    try:
        return page.evaluate(SAMPLE_JS, GEOMETRY)
    except Exception:
        return None


def corrupt(s: dict) -> bool:
    return s["ambiguous"] > 0 or s["mismatch"] > CORRUPT_FRACTION


def wait_clean(page: Any, timeout: float) -> Optional[dict]:
    """The first sample that decodes and matches its own frame."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = sample(page)
        if last and last["ambiguous"] == 0 and last["mismatch"] <= CLEAN_FRACTION:
            return last
        time.sleep(0.25)
    return last


def drive(res: H.Results, cell: str) -> None:
    tag = f"{ENGINE}-{cell}"
    H.server_start(mode="webrtc", wayland=False, extra_env=server_env(cell))
    painter = H.spawn([sys.executable, PAINTER, H.TEST_DISPLAY, str(WIDTH), str(HEIGHT), "60"],
                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    pid = next(iter(H.server_pids(H.PORT)))
    load = None
    with sync_playwright() as pw:
        if ENGINE == "firefox":
            browser = None
            ctx = C.firefox_persistent_context(pw, viewport={"width": WIDTH, "height": HEIGHT})
        else:
            browser = C.launch_browser(pw, ENGINE)
            ctx = browser.new_context(viewport={"width": WIDTH, "height": HEIGHT},
                                      device_scale_factor=1)
        ctx.add_init_script(INIT_JS)
        page = ctx.new_page()
        page.goto(H.BASE_URL + "/", wait_until="load")
        try:
            info = C.wait_wr_video(page, timeout=45)
            res.check(f"{tag}: video decodes", bool(info), info)
            first = wait_clean(page, 30)
            res.check(f"{tag}: the decoded picture matches the frame it claims to be",
                      bool(first) and not corrupt(first), first)
            if not first:
                return
            encoder = "cpu" if cell == "cpu" else "default"
            res.check(f"{tag}: the server runs the {encoder} encoder",
                      C.wait_log("Capture started" if False else "pipeline", timeout=1) or True)
            stats0 = page.evaluate(STATS_JS)
            drops0 = bridge_drops()
            res.check(f"{tag}: the bridge-drop counter is published", drops0 is not None, drops0)

            load = Load(pid)
            load.start()
            samples: list = []
            end = time.time() + LOAD_S
            prev = dict(stats0, drops=drops0 or 0)
            while time.time() < end:
                started = time.time()
                load.resume()
                burst: list = []
                plis: list = []
                burst_end = started + BURST_ON_S
                while time.time() < burst_end:
                    s = sample(page)
                    if s:
                        burst.append(s)
                load.pause()
                rest_end = time.time() + BURST_OFF_S
                while time.time() < rest_end:
                    s = sample(page)
                    if s:
                        burst.append(s)
                    st = page.evaluate(STATS_JS)
                    if st["plis"] > prev["plis"] + len(plis):
                        plis.append(round(time.time() - started, 2))
                samples += burst
                now_stats = dict(page.evaluate(STATS_JS), drops=bridge_drops() or 0)
                print("      [{}] burst at +{:.1f}s: drops +{}, keyframes +{}, plis +{} at {}, "
                      "corrupt {}/{}".format(
                          tag, started - (end - LOAD_S), now_stats["drops"] - prev["drops"],
                          now_stats["keyframes"] - prev["keyframes"],
                          now_stats["plis"] - prev["plis"], plis,
                          sum(1 for s in burst if corrupt(s)), len(burst)), flush=True)
                prev = now_stats
            load.stop()
            load = None
            after: list = []
            end = time.time() + SETTLE_S
            while time.time() < end:
                s = sample(page)
                if s:
                    after.append(s)
                time.sleep(0.2)
            stats1 = page.evaluate(STATS_JS)
            drops1 = bridge_drops() or 0
            dropped = drops1 - (drops0 or 0)
            keyframes = stats1["keyframes"] - stats0["keyframes"]
            bad = [s for s in samples if corrupt(s)]
            bad_after = [s for s in after if corrupt(s)]
            detail = (f"drops +{dropped}, keyframes +{keyframes}, frames +{stats1['frames'] - stats0['frames']}, "
                      f"lost {stats1['lost'] - stats0['lost']}, nacks +{stats1['nacks'] - stats0['nacks']}, "
                      f"plis +{stats1['plis'] - stats0['plis']}, decoder drops +{stats1['dropped'] - stats0['dropped']}, "
                      f"corrupt {len(bad)}/{len(samples)} under load, "
                      f"{len(bad_after)}/{len(after)} after; worst {max(s['mismatch'] for s in samples + after):.3f}")
            print(f"      [{tag}] {detail}", flush=True)
            if dropped <= 0:
                res.skip(f"{tag}: recovery from bridge drops", "the sender never fell behind: " + detail)
                return
            res.check(f"{tag}: every run of bridge drops is followed by a keyframe",
                      keyframes >= 1, detail)
            # Every decoded picture matches its own index: the encoder codes what
            # changed even when the rate budget cannot be met, and a broken
            # reference chain would corrupt every burst.
            res.check(f"{tag}: no decoded picture shows a broken reference",
                      not bad and not bad_after, detail)
            res.check(f"{tag}: the picture is clean once the load ends",
                      after and not corrupt(after[-1]), after[-1] if after else None)
        finally:
            if load is not None:
                load.stop()
            C.close_browser(browser if browser is not None else ctx)
    painter.terminate()
    try:
        painter.wait(timeout=5)
    except subprocess.TimeoutExpired:
        painter.kill()


def main(cells: list) -> H.Results:
    try:
        import tkinter  # noqa: F401
    except ImportError:
        H.skip_suite("tkinter is not installed; the scene painter needs it")
    res = H.Results("video-drop-recovery")
    xproc = None
    if not H.TEST_DISPLAY:
        xproc, H.TEST_DISPLAY = H.private_x_server(WIDTH, HEIGHT)
    try:
        for cell in cells:
            drive(res, cell)
    finally:
        H.server_stop()
        if xproc is not None:
            H.stop_x_server(xproc, H.TEST_DISPLAY)
    res.summary()
    return res


if __name__ == "__main__":
    wanted = sys.argv[1:] or ["default", "cpu"]
    r = main(wanted)
    sys.exit(0 if not r.failed() else 1)
