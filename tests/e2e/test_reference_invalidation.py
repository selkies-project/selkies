#!/usr/bin/env python3
"""A dropped frame costs the stream one prediction further back, not a keyframe.

A WebRTC sender that falls behind its encoder for a moment drops frames before
they are packetized. Told which frame went, the encoder predicts past it and the
receiver decodes on; told nothing, the stream can only resume at a keyframe,
which costs the link an intra picture and the viewer a frozen one. The load
here is that moment, repeated: the server's event loop shares one CPU with
busy processes in short bursts, each long enough to drop frames and too short
for the receiver to ask for a keyframe on its own. The scene is tests/tools/motion_scene.py, whose every frame spells its own
index, so a decoded picture can be compared with the frame it claims to be.
Counted throughout: the frames the server dropped, those of them the encoder
was told to predict past, the keyframes decoded and the picture-loss
indications the receiver sent. Measured against the same load without the
repair, which spends a keyframe for every six frames it drops: 286 drops and 49
keyframes there, 260 drops and none here.

    python3 tests/e2e/test_reference_invalidation.py

E2E_ENGINE selects the browser, Chromium by default.
"""
import os
import re
import subprocess
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_video_drop_recovery as D
from playwright.sync_api import sync_playwright

WIDTH, HEIGHT = 1280, 720
ENGINE = os.environ.get("E2E_ENGINE", "chromium")
# Short stalls, well inside the ~400 ms a receiver waits before asking for a
# keyframe itself: each drops a handful of frames and nothing else.
BURST_ON_S, BURST_OFF_S = 0.08, 0.42
LOAD_S = 24.0
TOLD = "the encoder predicts past it"


def bridge_counter(name: str) -> int:
    """The primary display's bridge counter of that name from /api/metrics:
    `dropped` for the frames it let go, `invalidated` for those of them the encoder
    was told to predict past. Zero where the server publishes no such counter."""
    status, body = H.curl("/api/metrics")
    if status != 200:
        return 0
    m = re.search(r'webrtc_bridge_%s_frames\{display="primary"\}\s+([0-9.eE+]+)' % name,
                  body.decode("utf-8", "replace"))
    return int(float(m.group(1))) if m else 0


def told(mark: int) -> int:
    """How many frames the server logged as lost to a consumer since `mark`, the
    throttled line's own count of the rest included."""
    text = H.server_log()[mark:]
    return text.count(TOLD) + sum(
        int(n) for n in re.findall(r"\(\+(\d+) more in the last", text))


def sample_for(page: Any, seconds: float) -> list:
    """Every readable sample over `seconds`."""
    out = []
    end = time.time() + seconds
    while time.time() < end:
        s = D.sample(page)
        if s:
            out.append(s)
    return out


def report(res: H.Results, tag: str, samples: list, dropped: int, named: int,
           keyframes: int, plis: Optional[int], frames: int) -> None:
    """The checks both transports share: the drops happened, the encoder was told which
    frames to predict past, the recovery cost no keyframe, and every decoded picture was
    the frame it claimed to be."""
    bad = [s for s in samples if D.corrupt(s)]
    detail = (f"dropped {dropped}, named {named}, keyframes +{keyframes}, "
              f"{'plis +%d, ' % plis if plis is not None else ''}frames +{frames}, "
              f"corrupt {len(bad)}/{len(samples)}")
    print(f"      [{tag}] {detail}", flush=True)
    res.check(f"{tag}: the load really dropped frames", dropped > 0, detail)
    res.check(f"{tag}: the encoder was told which frames to predict past", named > 0, detail)
    # One of each is the measured ceiling: a stall that lands badly still trips the
    # receiver's own keyframe timer now and then. Without the repair the same load
    # spends one keyframe per six drops. x264 sizes frame_num for sixteen frames, so a
    # loss covering its wrap is answered with the keyframe FFmpeg's decoder needs: with
    # the few frames a stall drops at a time, that is a share of the losses named.
    software = "Encoder: software H264" in H.server_log()
    ceiling = max(1, named // 2) if software else 1
    res.check(f"{tag}: the drops cost at most {'the wrap share of' if software else 'one'} keyframe",
              keyframes <= ceiling, detail)
    if plis is not None:
        res.check(f"{tag}: and the receiver asked for at most one", plis <= 1, detail)
    res.check(f"{tag}: the stream kept flowing", frames >= 20 * LOAD_S, detail)
    res.check(f"{tag}: every decoded picture was the frame it claimed to be", not bad, detail)


def webrtc(res: H.Results) -> None:
    tag = f"webrtc-{ENGINE}"
    H.server_start(mode="webrtc", wayland=False, extra_env=D.server_env("default"))
    painter = H.spawn([sys.executable, D.PAINTER, H.TEST_DISPLAY, str(WIDTH), str(HEIGHT), "60"],
                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    pid = next(iter(H.server_pids(H.PORT)))
    load = None
    with sync_playwright() as pw:
        browser = C.launch_browser(pw, ENGINE)
        ctx = browser.new_context(viewport={"width": WIDTH, "height": HEIGHT}, device_scale_factor=1)
        ctx.add_init_script(D.INIT_JS)
        page = ctx.new_page()
        page.goto(H.BASE_URL + "/", wait_until="load")
        try:
            res.check(f"{tag}: video decodes", bool(C.wait_wr_video(page, timeout=45)))
            first = D.wait_clean(page, 30)
            res.check(f"{tag}: the decoded picture matches the frame it claims to be",
                      bool(first) and not D.corrupt(first), first)
            if not first:
                return
            sdp = page.evaluate("""() => {
              const out = {offer: false, answer: false};
              const dd = (d) => !!(d && d.sdp.includes('dependency-descriptor-rtp-header-extension'));
              for (const pc of window.__pcs || []) {
                out.offer = out.offer || dd(pc.remoteDescription);
                out.answer = out.answer || dd(pc.localDescription);
              }
              return out;
            }""")
            res.check(f"{tag}: the server offers the dependency descriptor and the browser keeps it",
                      sdp["offer"] and sdp["answer"], sdp)

            before = page.evaluate(D.STATS_JS)
            drops0, named0 = bridge_counter("dropped"), bridge_counter("invalidated")
            load = D.Load(pid)
            load.start()
            samples = []
            end = time.time() + LOAD_S
            while time.time() < end:
                load.resume()
                samples += sample_for(page, BURST_ON_S)
                load.pause()
                samples += sample_for(page, BURST_OFF_S)
            load.stop()
            load = None
            samples += sample_for(page, D.SETTLE_S)
            after = page.evaluate(D.STATS_JS)
            report(res, tag, samples, bridge_counter("dropped") - drops0,
                   bridge_counter("invalidated") - named0,
                   after["keyframes"] - before["keyframes"], after["plis"] - before["plis"],
                   after["frames"] - before["frames"])
        finally:
            if load is not None:
                load.stop()
            C.close_browser(browser)
            painter.kill()
            H.server_stop()


def main() -> int:
    res = H.Results("reference-invalidation")
    webrtc(res)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
