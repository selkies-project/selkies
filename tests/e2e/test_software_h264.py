#!/usr/bin/env python3
"""Software H.264 streams with whichever encoder the server's pixelflux was built
with: libx264 (the default build) or Cisco OpenH264 (a PIXELFLUX_ENABLE_GPL=0
build). Selkies never chooses between them — it reads pixelflux.SOFTWARE_ENCODERS["h264"]
— so the same session has to come up, name that encoder in the server log, and
decode in a real browser on either build: `h264enc` forced onto the CPU, on X11
and on Wayland.

Run with SELKIES_TEST_SERVER pointing at the server whose pixelflux build is under
test, and the suites' interpreter carrying the matching pixelflux wheel, whose
SOFTWARE_ENCODERS names the encoder to expect.

Usage: python3 tests/e2e/test_software_h264.py [x11|wl|all]
"""
import os
import subprocess
import sys
import time
from typing import Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C

# Counts decoded VideoFrames and records every decoder configuration on the
# page, which is where a full-frame session decodes, beside what the client
# publishes -- the wire counter and which sink element is on screen.
INSTRUMENT_JS = """
  window.__decoded = 0;
  window.__cfgs = [];
  window.__decodeErrors = [];
  (() => {
    const Real = window.VideoDecoder;
    if (!Real) return;
    const proto = Real.prototype;
    const origConfigure = proto.configure;
    proto.configure = function (cfg) {
      window.__cfgs.push({w: cfg && cfg.codedWidth, h: cfg && cfg.codedHeight});
      return origConfigure.call(this, cfg);
    };
    window.VideoDecoder = new Proxy(Real, {
      construct(target, args) {
        const init = Object.assign({}, (args && args[0]) || {});
        const out = init.output;
        const err = init.error;
        init.output = (frame) => { window.__decoded++; if (out) out(frame); };
        init.error = (e) => { window.__decodeErrors.push(String(e && e.message || e)); if (err) err(e); };
        return Reflect.construct(target, [init]);
      },
    });
  })();
"""


def server_software_encoder() -> Optional[str]:
    """The software H.264 encoder of the pixelflux build the server interpreter
    imports ("x264" | "openh264"), or None when it does not say."""
    out = subprocess.run(
        [H.PYTHON, "-c", "import pixelflux; print(pixelflux.SOFTWARE_ENCODERS['h264'])"],
        capture_output=True, text=True, timeout=60)
    name = out.stdout.strip()
    return name or None


SINK_IDS = ("videoCanvas", "videoWorkerCanvas", "videoStream")


def read_state(page) -> dict:
    """Page decode counters plus what the client publishes: the wire chunk count
    and the sinks on screen."""
    return page.evaluate("""(() => ({
      decoded: window.__decoded || 0,
      cfgs: window.__cfgs || [],
      errors: window.__decodeErrors || [],
      chunks: window.videoChunksReceived || 0,
      sinks: ['videoCanvas', 'videoWorkerCanvas', 'videoStream'].filter((id) => {
        const el = document.getElementById(id);
        return el && getComputedStyle(el).display !== 'none';
      }),
    }))()""")


def wait_sinks(page, want: Optional[list] = None, timeout: float = 25) -> list:
    """The sinks on screen once one is left, and once it is `want` if given; a
    second shows during warm-up, while the canvas still covers a sink that has
    yet to render, and the outgoing sink stays up until the new one does."""
    deadline = time.time() + timeout
    sinks = read_state(page)["sinks"]
    while time.time() < deadline and not (len(sinks) == 1 and (want is None or sinks == want)):
        time.sleep(0.5)
        sinks = read_state(page)["sinks"]
    return sinks


def wait_state(page, predicate, timeout: float = 30) -> dict:
    """Poll the page state until `predicate` holds or `timeout` passes; the last state."""
    deadline = time.time() + timeout
    state = read_state(page)
    while time.time() < deadline and not predicate(state):
        time.sleep(0.5)
        state = read_state(page)
    return state


def run_block(r: "H.Results", wayland: bool) -> None:
    """Stream software H.264 on one backend and check that the build's encoder is
    named in the logs and decodes in the browser. The server runs Turbo, so the still test display
    keeps sending the frames counted."""
    encoder = server_software_encoder()
    r.check("server pixelflux names its software H.264 encoder",
            encoder in ("x264", "openh264"), encoder)
    from playwright.sync_api import sync_playwright
    H.server_start(mode="websockets", wayland=wayland,
                   extra_env={"SELKIES_USE_CPU": "true", "SELKIES_VIDEO_STREAMING_MODE": "true"})
    try:
        with sync_playwright() as pw:
            browser = C.chromium_launch(pw)
            ctx = browser.new_context(viewport={"width": 1280, "height": 720},
                                      device_scale_factor=1)
            ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
            ctx.add_init_script(INSTRUMENT_JS)
            page = ctx.new_page()
            page.goto(H.BASE_URL + "/", wait_until="load")
            try:
                info = C.wait_ws_video(page, timeout=30)
                r.check("h264enc: canvas painted", info is not None, info)
                start = read_state(page)
                state = wait_state(page, lambda s: s["chunks"] >= start["chunks"] + 24
                                   and s["decoded"] >= 12)
                r.check("h264enc: frames flow", state["chunks"] >= start["chunks"] + 24,
                        (start["chunks"], state["chunks"]))
                r.check("h264enc: frames decode", state["decoded"] >= 12, state["decoded"])
                r.check("h264enc: no decoder errors", not state["errors"], state["errors"][:3])
                heights = sorted({c["h"] for c in state["cfgs"] if c.get("h")})
                r.check("h264enc: one full-frame decoder geometry", len(heights) == 1, heights)
                full_sinks = wait_sinks(page)
                r.check("h264enc: one video sink on screen", len(full_sinks) == 1, full_sinks)
                log = H.server_log()
                r.check("server names the software encoder for the session",
                        f"encodes H.264 in software ({encoder})" in log, encoder)
                stream_line = f"Mode: H264 ({encoder})" if wayland else f"Encoder: CPU ({encoder})"
                r.check("pixelflux's stream-settings line names the software encoder",
                        stream_line in log, stream_line)
            finally:
                browser.close()
    finally:
        H.server_stop()


BLOCKS = {"x11": lambda r: run_block(r, False), "wl": lambda r: run_block(r, True)}

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    names = list(BLOCKS) if which == "all" else [which]
    ok = True
    for n in names:
        print(f"=== {n} ===", flush=True)
        res = H.Results(f"software-h264-{n}")
        try:
            BLOCKS[n](res)
        except Exception as e:
            res.check("block completed", False, e)
        ok = res.summary() and ok
    sys.exit(0 if ok else 1)
