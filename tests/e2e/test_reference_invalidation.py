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
was told to predict past, the keyframes decoded, and the picture-loss
indications the receiver sent. Measured against the same load without the
repair, which spends a keyframe for about every eight frames it drops: 456 and
621 drops and 69 and 72 keyframes there, 173 to 264 drops and none here.

What the repair itself may spend depends on the encoder that coded the load, read
from the server log since a host whose NVENC sessions run out falls back to x264:
a loss covering the frame where H.264's frame_num wraps is answered with a
keyframe, every 256 frames on NVENC and every 4096 on x264, whose own sixteen
values pixelflux carries a byte wider, and so is a burst that let go about as many
frames as the encoder keeps references, since the frame it would predict past them
from has left its window, so the ceiling is worked out from what each burst let
go (`keyframe_ceiling`). On NVENC a repaired load cost 0 to 3 keyframes, and one
that asked a keyframe for every loss 46 to 64. On x264 it cost 0; x264's own
sixteen values cost 0 and 3, and a keyframe for every loss 44 to 62. A GitHub
runner slow enough that nine of 21 bursts ran past the window cost 12.

    python3 tests/e2e/test_reference_invalidation.py

E2E_ENGINE selects the browser, Chromium by default.
"""
import math
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
# Short bursts, inside the eight frames of references the encoder keeps (133 ms
# at 60 fps) and well inside the ~400 ms a receiver waits before asking for a
# keyframe itself: each drops a handful of frames and nothing else.
BURST_ON_S, BURST_OFF_S = 0.08, 0.42
LOAD_S = 24.0
TOLD = "the encoder predicts past it"
# How many values frame_num takes before it wraps in each H.264 encoder's
# stream, by the name the capture's settings line gives the encoder, and the
# frames of references each keeps at this size.
FRAME_NUM_RANGE = {"NVENC": 256, "CPU (x264)": 4096}
REFERENCES = 8
SETTINGS = re.compile(r"Stream settings active -> .*?\| Encoder: ([^|]+?) \|")


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


def load_encoders(mark: int) -> list:
    """The encoders that coded the load, from the server log: the one the capture
    ran on when the load began at offset `mark`, then each it restarted on. The
    capture of a fresh display may start on another encoder before the page sizes
    it, and one that finds no hardware session free falls back to x264."""
    found = [(m.start(), m.group(1)) for m in SETTINGS.finditer(H.server_log())]
    return [e for at, e in found if at < mark][-1:] + [e for at, e in found if at >= mark]


def sample_for(page: Any, seconds: float) -> list:
    """Every readable sample over `seconds`."""
    out = []
    end = time.time() + seconds
    while time.time() < end:
        s = D.sample(page)
        if s:
            out.append(s)
    return out


def keyframe_ceiling(bursts: list, frame_num_range: int) -> int:
    """The keyframes a working repair can spend on a load that let frames go in
    `bursts` (the count let go in each burst that lost any, named to the encoder
    or held back behind one that was), allowing three standard deviations.

    A loss is answered with a keyframe when a frame from the lost one to the newest
    encoded carries frame_num 0, which the browsers' FFmpeg decoder cannot be
    predicted past, or when the frame before the loss, which the encoder would
    predict the next frame from, has left its window of REFERENCES frames; later
    reports of frames before that keyframe are ignored, so a burst costs at most
    one. Every frame a burst let go was encoded after that frame, as were the one
    that ended the burst and the one the encoder has in hand when the word reaches
    it, so a burst that let go REFERENCES - 2 or more counts as one. Frames reach
    the server's loop in order, so a shorter burst that let go n frames had the
    encoder about n frames past the first of them: its odds of covering a wrap are
    taken as (n + REFERENCES) / frame_num_range, a span longer than any loss the
    references still hold.
    """
    odds = [1.0 if n >= REFERENCES - 2 else (n + REFERENCES) / frame_num_range for n in bursts]
    return min(len(bursts), math.ceil(sum(odds) + 3 * math.sqrt(sum(p * (1 - p) for p in odds))))


def report(res: H.Results, tag: str, samples: list, dropped: int, named: int, bursts: list,
           encoders: list, keyframes: int, plis: Optional[int], frames: int) -> None:
    """The checks both transports share: the drops happened, the encoder was told which
    frames to predict past, the recovery cost no keyframe beyond the frame_num wraps the
    losses covered, and every decoded picture was the frame it claimed to be. `bursts`
    holds each burst's frames let go and frames named."""
    bad = [s for s in samples if D.corrupt(s)]
    detail = (f"dropped {dropped}, named {named}, keyframes +{keyframes}, "
              f"{'plis +%d, ' % plis if plis is not None else ''}frames +{frames}, "
              f"corrupt {len(bad)}/{len(samples)}")
    print(f"      [{tag}] {detail}, on {' then '.join(encoders) or 'no encoder logged'}, "
          f"let go and named per burst {bursts}", flush=True)
    res.check(f"{tag}: the load really dropped frames", dropped > 0, detail)
    res.check(f"{tag}: the encoder was told which frames to predict past", named > 0, detail)
    # Beyond what the wraps and the deep bursts cost (keyframe_ceiling), one is the
    # measured allowance -- a burst that lands badly still trips the receiver's own
    # keyframe timer now and then -- and every capture restart opens with one. A
    # repair that spends a keyframe on every loss costs about one per burst, against
    # a ceiling of a handful.
    ranges = [FRAME_NUM_RANGE.get(e) for e in encoders]
    known = bool(ranges) and None not in ranges
    res.check(f"{tag}: the load ran on an encoder whose frame_num range is known", known, encoders)
    if known:
        ceiling = 1 + (len(encoders) - 1) + keyframe_ceiling([n for n, _ in bursts], min(ranges))
        res.check(f"{tag}: the drops cost at most one keyframe beyond the frame_num wraps",
                  keyframes <= ceiling,
                  f"keyframes +{keyframes}, ceiling {ceiling} on {encoders[-1]} from {len(bursts)} bursts")
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
        if ENGINE == "firefox":
            # The persistent profile carries the OpenH264 plugin Firefox decodes H.264 with.
            browser = None
            ctx = C.firefox_persistent_context(pw, viewport={"width": WIDTH, "height": HEIGHT})
        else:
            browser = C.launch_browser(pw, ENGINE)
            ctx = browser.new_context(viewport={"width": WIDTH, "height": HEIGHT},
                                      device_scale_factor=1)
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
            # Firefox's receiver implements no dependency descriptor, so it answers without one; a
            # frame dropped before it was sent leaves no gap for one to bridge.
            keeps = sdp["answer"] or ENGINE == "firefox"
            res.check(f"{tag}: the server offers the dependency descriptor and the browser keeps it",
                      sdp["offer"] and keeps, sdp)

            before = page.evaluate(D.STATS_JS)
            drops0, named0 = bridge_counter("dropped"), bridge_counter("invalidated")
            mark = len(H.server_log())
            # Slowed, not stopped: a stall longer than the encoder's references
            # costs a keyframe however it is repaired.
            load = D.Load(pid, stall=False)
            load.start()
            samples = []
            dropped, named, bursts = drops0, named0, []
            end = time.time() + LOAD_S
            while time.time() < end:
                load.resume()
                # Timed on its own: a sample still running when the burst should end
                # would stretch it past those references, and a broken reference shows
                # in every picture after it, which the samples between bursts see.
                time.sleep(BURST_ON_S)
                load.pause()
                samples += sample_for(page, BURST_OFF_S)
                dropped_now, named_now = bridge_counter("dropped"), bridge_counter("invalidated")
                if named_now > named:
                    bursts.append((dropped_now - dropped, named_now - named))
                elif dropped_now > dropped and bursts:
                    # Held behind a loss named earlier: the burst before goes on.
                    bursts[-1] = (bursts[-1][0] + dropped_now - dropped, bursts[-1][1])
                dropped, named = dropped_now, named_now
            load.stop()
            load = None
            samples += sample_for(page, D.SETTLE_S)
            after = page.evaluate(D.STATS_JS)
            report(res, tag, samples, bridge_counter("dropped") - drops0,
                   bridge_counter("invalidated") - named0, bursts, load_encoders(mark),
                   after["keyframes"] - before["keyframes"], after["plis"] - before["plis"],
                   after["frames"] - before["frames"])
        finally:
            if load is not None:
                load.stop()
            C.close_browser(browser if browser is not None else ctx)
            painter.kill()
            H.server_stop()


def main() -> int:
    res = H.Results("reference-invalidation")
    webrtc(res)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
