#!/usr/bin/env python3
"""A page on a slow link beside the display's owner: one encoder, and each page what its link carries.

Every page of a display is served from one encoder (an SFU). A page whose link cannot carry
the stream falls behind and has frames dropped for it; where the encoder names each frame's
reference, the page is resynced on the first frame predicting past the drop, so the shared
stream carries no key frame for it. The owner, on a link with room, decodes on undisturbed,
and the slow page keeps receiving frames, lagging by about a second rather than by the
seconds of stream a byte budget holds.

The slow page is a second Chromium page beside the owner: over WebSockets behind a metered TCP
relay (tests/tools/bwrelay.py), over WebRTC behind narrow UDP relays (the pacer suite's), which
the page reaches the server through alone: its init script points the server's candidates at
them and withholds its own, so the server meets it only through them. The owner's key frames
are counted on its own page. The scene is tests/tools/motion_scene.py, whose every frame spells
its own index, so the pictures the slow page decodes are checked against the frames they claim
to be, and its lag behind the owner read off the two pages' pictures.

Usage: python3 tests/e2e/test_slow_page.py [websockets|webrtc|all]
"""
import asyncio
import os
import sys
import threading
import time
from typing import List

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "perf"))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
import test_video_drop_recovery as D  # noqa: E402

WIDTH, HEIGHT = 1280, 720
PAINTER = os.path.join(H.TOOLS, "motion_scene.py")
BWRELAY = os.path.join(H.TOOLS, "bwrelay.py")
RELAY_PORT = int(os.environ.get("E2E_SLOW_PAGE_PORT", "18214"))
# A third of the 8 Mbit/s the moving scene streams at.
LINK_KBIT = int(os.environ.get("E2E_SLOW_PAGE_KBIT", "2500"))
WINDOW_S = 30
SETTLE_S = 10
# Congestion control off: it would fit the stream to the owner's link, which has room, and
# the narrow link is the second page's alone.
ENV = {"SELKIES_CONGESTION_CONTROL": "false", "SELKIES_ENCODER": "h264enc", "SELKIES_VIDEO_BITRATE": "8000"}

# Each video frame a page receives over its socket: whether it is a key frame or an anchor
# (FRAME_ANCHOR), and when it came.
FRAME_TAP_JS = """
  window.__keys = 0;
  window.__anchors = 0;
  window.__frames = [];
  (() => {
    let last = -1;
    const tap = (e) => {
      if (!(e.data instanceof ArrayBuffer) || e.data.byteLength < 12) return;
      const h = new Uint8Array(e.data, 0, 12);
      if (h[0] !== 0x04) return;
      const id = (h[2] << 8) | h[3];
      if (id === last) return;
      last = id;
      window.__lastId = id;
      if ((h[1] & 0x0F) === 0x01) window.__keys++;
      if (h[1] & 0x08) window.__anchors++;
      window.__frames.push(performance.now());
      if (window.__frames.length > 40000) window.__frames.splice(0, 20000);
    };
    const WS = window.WebSocket;
    window.WebSocket = function(...a) {
      const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
      s.addEventListener('message', tap);
      return s;
    };
    window.WebSocket.prototype = WS.prototype;
    Object.setPrototypeOf(window.WebSocket, WS);
    // The websockets transport runs its socket in a worker; its receive side is the page handle.
    let transport = null;
    Object.defineProperty(window, 'selkiesTransport', {
      configurable: true,
      get: () => transport,
      set: (v) => { transport = v; if (v && v.addEventListener) v.addEventListener('message', tap); },
    });
  })();
"""


# The decoded picture, from whichever sink shows it (the <video> a full-frame stream renders
# through, else the canvas), checked against the frame it spells (test_video_drop_recovery).
PICTURE_JS = D.SAMPLE_JS.replace(
    """  const v = document.querySelector('video');
  if (!v || v.videoWidth === 0) return null;
  const W = v.videoWidth, H = v.videoHeight;""",
    """  let v = document.querySelector('video'), W = 0, H = 0;
  if (v && v.videoWidth > 0 && v.readyState >= 2 && v.style.display !== 'none') {
    W = v.videoWidth; H = v.videoHeight;
  } else {
    v = [...document.querySelectorAll('canvas')].find((c) => c.width >= 640 && c.style.display !== 'none');
    if (!v) return null;
    W = v.width; H = v.height;
  }""")
assert PICTURE_JS != D.SAMPLE_JS


def pictures(owner, slow, seconds: float) -> list:
    """The two pages' pictures sampled together about once a second, as (owner, slow page)."""
    out = []
    end = time.time() + seconds
    while time.time() < end:
        a = owner.evaluate(PICTURE_JS, D.GEOMETRY)
        b = slow.evaluate(PICTURE_JS, D.GEOMETRY)
        if a and b and not a["ambiguous"] and not b["ambiguous"]:
            out.append((a, b))
        owner.wait_for_timeout(1000)
    return out


def lag_frames(samples: list) -> list:
    """How many frames of the scene the slow page's picture trails the owner's, per sample;
    the page read second may show a newer one."""
    return sorted(((a["index"] - b["index"] + 0x8000) & 0xFFFF) - 0x8000 for a, b in samples)


def keep_log(name: str, text: str) -> None:
    """The server log of a block's window, beside the run's other logs."""
    with open(os.path.join(H.WORKDIR, f"slow-page-{name}.log"), "w") as f:
        f.write(text)


# Over WebRTC, the slow page's path to the server is the relays the test runs: every UDP host
# candidate of the server's becomes the relay the page's `__shapedRelay` returns for it, every
# other is dropped, and the page's own candidates are withheld, so the server meets the page as
# a peer-reflexive candidate behind a relay. Its decode counters are read from `__pcs`.
SHAPED_JS = """
  (() => {
    const Orig = window.RTCPeerConnection;
    window.__pcs = [];
    const Wrapped = function(...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    Wrapped.prototype = Orig.prototype;
    Object.setPrototypeOf(Wrapped, Orig);
    window.RTCPeerConnection = Wrapped;
    const CAND = /^(a=)?candidate:(\\S+) (\\d+) (udp) (\\d+) (\\S+) (\\d+) typ host(.*)$/i;
    const shaped = async (line) => {
      const m = line.match(CAND);
      if (!m) return null;
      const [host, port] = await window.__shapedRelay(m[6], Number(m[7]));
      return `${m[1] || ''}candidate:${m[2]} ${m[3]} ${m[4]} ${m[5]} ${host} ${port} typ host${m[8]}`;
    };
    const setRemote = Orig.prototype.setRemoteDescription;
    Orig.prototype.setRemoteDescription = async function(desc) {
      if (desc && desc.sdp) {
        const lines = [];
        for (const l of desc.sdp.split('\\r\\n')) {
          if (!l.startsWith('a=candidate:')) { lines.push(l); continue; }
          const r = await shaped(l);
          if (r) lines.push(r);
        }
        desc = {type: desc.type, sdp: lines.join('\\r\\n')};
      }
      return setRemote.call(this, desc);
    };
    const addCandidate = Orig.prototype.addIceCandidate;
    Orig.prototype.addIceCandidate = async function(c) {
      if (!c || !c.candidate) return addCandidate.call(this, c);
      const r = await shaped(c.candidate);
      if (!r) return;
      return addCandidate.call(this, {candidate: r, sdpMid: c.sdpMid, sdpMLineIndex: c.sdpMLineIndex});
    };
    const handler = Object.getOwnPropertyDescriptor(Orig.prototype, 'onicecandidate');
    Object.defineProperty(Orig.prototype, 'onicecandidate', {
      configurable: true,
      get() { return handler.get.call(this); },
      set(fn) { handler.set.call(this, (e) => (e.candidate ? undefined : fn(e))); },
    });
  })();
"""

# The inbound video counters of a page's peer connections.
INBOUND_JS = """async () => {
  const out = {keyframes: 0, decoded: 0, plis: 0, freezes: 0, frozen: 0};
  for (const pc of window.__pcs || []) {
    (await pc.getStats()).forEach((r) => {
      if (r.type !== 'inbound-rtp' || r.kind !== 'video') return;
      out.keyframes += r.keyFramesDecoded || 0;
      out.decoded += r.framesDecoded || 0;
      out.plis += r.pliCount || 0;
      out.freezes += r.freezeCount || 0;
      out.frozen += r.totalFreezesDuration || 0;
    });
  }
  return out;
}"""


def stalls(times: List[float]) -> float:
    """The longest gap between frames, in the times' unit; the whole window for under two."""
    return max((b - a for a, b in zip(times, times[1:])), default=float("inf"))


def ws_block() -> "H.Results":
    from playwright.sync_api import sync_playwright

    res = H.Results("slow-page-websockets")
    painter = relay = None
    H.server_start(mode="websockets", wayland=False, extra_env=ENV)
    try:
        painter = H.spawn([sys.executable, PAINTER, H.TEST_DISPLAY, str(WIDTH), str(HEIGHT), "60"])
        relay = H.spawn([sys.executable, BWRELAY, str(RELAY_PORT), str(H.PORT), str(LINK_KBIT)])
        with sync_playwright() as p:
            pages = []
            for url in (H.BASE_URL + "/", f"http://127.0.0.1:{RELAY_PORT}/"):
                browser = C.launch_browser(p, "chromium")
                ctx = browser.new_context(viewport={"width": WIDTH, "height": HEIGHT}, device_scale_factor=1)
                ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
                ctx.add_init_script(FRAME_TAP_JS)
                page = ctx.new_page()
                page.goto(url, wait_until="load", timeout=90000)
                pages.append((browser, page))
                if len(pages) == 1:
                    res.check("the owner streams the moving scene", C.wait_ws_video(page, timeout=45) is not None)
            owner, slow = pages[0][1], pages[1][1]
            res.check("a page beside it streams over a link a third of the stream",
                      C.wait_ws_video(slow, timeout=90) is not None)
            owner.wait_for_timeout(SETTLE_S * 1000)
            keys0, since = owner.evaluate("window.__keys"), slow.evaluate("performance.now()")
            anchors0 = slow.evaluate("window.__anchors")
            mark = len(H.server_log())
            samples = pictures(owner, slow, WINDOW_S)
            keys = owner.evaluate("window.__keys") - keys0
            anchors = slow.evaluate("window.__anchors") - anchors0
            frames = slow.evaluate("(t0) => window.__frames.filter((t) => t >= t0)", since)
            log = H.server_log()[mark:]
            keep_log("websockets", log)
            lags = lag_frames(samples)
            worst = max((b["mismatch"] for _, b in samples), default=1.0)
            res.check("the owner's stream carries no key frame for the slow page", keys <= 1, f"{keys} key frames")
            res.check("the slow page keeps receiving frames", len(frames) >= WINDOW_S * 5, f"{len(frames)} frames")
            res.check("without a stall past two seconds", stalls(frames) < 2000, f"{stalls(frames):.0f} ms")
            res.check("every picture it decodes is the frame it claims to be",
                      len(samples) >= WINDOW_S // 2 and worst < D.CORRUPT_FRACTION,
                      f"{len(samples)} samples, worst {worst:.3f}")
            res.check("it trails the owner by under a second", bool(lags) and lags[len(lags) // 2] < 60,
                      lags)
            res.check("the drops were answered by the encoder predicting past them",
                      "lost by a client; the encoder predicts past it" in log)
            print(f"      owner: {keys} key frames in {WINDOW_S} s; slow page: {len(frames)} frames "
                  f"({anchors} anchors), longest stall {stalls(frames):.0f} ms, lag "
                  f"{lags[len(lags) // 2] if lags else '?'} frames (median), worst picture {worst:.3f}", flush=True)
            for browser, _ in pages:
                C.close_browser(browser)
    finally:
        for proc in (painter, relay):
            if proc is not None:
                proc.kill()
        H.server_stop()
    res.summary()
    return res


def wr_block() -> "H.Results":
    from playwright.sync_api import sync_playwright
    import test_pacer as rig

    res = H.Results("slow-page-webrtc")
    painter = None
    loop = asyncio.new_event_loop()
    threading.Thread(target=loop.run_forever, daemon=True).start()
    up, down = rig.Shaper(), rig.Shaper(rate_bps=LINK_KBIT * 1000, delay_s=0.03)

    def shaped_relay(host: str, port: int) -> list:
        """A relay toward one of the server's candidates, shaped on the way to the page."""
        _, addr = asyncio.run_coroutine_threadsafe(
            rig.make_relay((host, int(port)), up, down, "slow-page"), loop).result(timeout=10)
        return [addr[0], addr[1]]

    H.server_start("webrtc", wayland=False, extra_env={
        **ENV, "SELKIES_WEBRTC_PACER": "true", "SELKIES_STUN_HOST": "", "SELKIES_TURN_REST_URI": ""})
    try:
        painter = H.spawn([sys.executable, PAINTER, H.TEST_DISPLAY, str(WIDTH), str(HEIGHT), "60"])
        with sync_playwright() as p:
            pages = []
            for script in (C.PC_TAP_JS, SHAPED_JS):
                browser = C.launch_browser(p, "chromium")
                ctx = browser.new_context(viewport={"width": WIDTH, "height": HEIGHT}, device_scale_factor=1)
                ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'webrtc';")
                if script is SHAPED_JS:
                    ctx.expose_function("__shapedRelay", shaped_relay)
                ctx.add_init_script(script)
                page = ctx.new_page()
                page.goto(H.BASE_URL + "/", wait_until="load", timeout=90000)
                pages.append((browser, page))
                if len(pages) == 1:
                    res.check("the owner streams the moving scene", bool(C.wait_wr_video(page)))
            owner, slow = pages[0][1], pages[1][1]
            res.check("a page beside it streams over a link a third of the stream",
                      bool(C.wait_wr_video(slow, timeout=90)))
            owner.wait_for_timeout(SETTLE_S * 1000)
            before = (owner.evaluate(INBOUND_JS), slow.evaluate(INBOUND_JS))
            mark = len(H.server_log())
            samples = pictures(owner, slow, WINDOW_S)
            after = (owner.evaluate(INBOUND_JS), slow.evaluate(INBOUND_JS))
            log = H.server_log()[mark:]
            keep_log("webrtc", log)
            own, sl = ({k: a[k] - b[k] for k in a} for a, b in zip(after, before))
            lags = lag_frames(samples)
            worst = max((b["mismatch"] for _, b in samples), default=1.0)
            res.check("the owner decodes on beside it", own["decoded"] >= max(WINDOW_S * 10, sl["decoded"]), own)
            res.check("the owner's stream carries no key frame for the slow page", own["keyframes"] <= 1,
                      f"{own['keyframes']} key frames")
            res.check("the slow page keeps decoding frames", sl["decoded"] >= WINDOW_S * 5, sl)
            res.check("without asking for a key frame", sl["plis"] == 0, sl)
            res.check("every picture it decodes is the frame it claims to be",
                      len(samples) >= WINDOW_S // 2 and worst < D.CORRUPT_FRACTION,
                      f"{len(samples)} samples, worst {worst:.3f}")
            res.check("it trails the owner by under a second", bool(lags) and lags[len(lags) // 2] < 60, lags)
            res.check("its drops were answered by the encoder predicting past them",
                      "lost by a peer; the encoder predicts past it" in log and "GOP reset" not in log,
                      (log.count("the sender resyncs its peer"), log.count("GOP reset")))
            print(f"      owner: {own['keyframes']} key frames, {own['decoded']} decoded; slow page: "
                  f"{sl['decoded']} decoded, {sl['keyframes']} key frames, {sl['plis']} PLIs, "
                  f"{sl['freezes']} freezes ({sl['frozen']:.1f} s), lag {lags[len(lags) // 2] if lags else '?'} "
                  f"frames (median), worst picture {worst:.3f}; pacer cuts "
                  f"{log.count('the sender resyncs its peer')}", flush=True)
            for browser, _ in pages:
                C.close_browser(browser)
    finally:
        if painter is not None:
            painter.kill()
        H.server_stop()
        loop.call_soon_threadsafe(loop.stop)
    res.summary()
    return res


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = []
    if which in ("all", "websockets"):
        blocks.append(ws_block())
    if which in ("all", "webrtc"):
        blocks.append(wr_block())
    sys.exit(0 if all(not b.failed() for b in blocks) else 1)


if __name__ == "__main__":
    main()
