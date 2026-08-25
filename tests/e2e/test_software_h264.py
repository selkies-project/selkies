#!/usr/bin/env python3
"""Software H.264 streams with whichever encoder the server's pixelflux was built
with: libx264 (the default build) or Cisco OpenH264 (a PIXELFLUX_ENABLE_GPL=0
build). Selkies never chooses between them — it reads pixelflux.SOFTWARE_H264_ENCODER
— so the same session has to come up, name that encoder in the server log, and
decode in a real browser on either build: full-frame `h264enc` forced onto the
CPU, then the per-stripe `h264enc-striped` streams, on X11 and on Wayland.

Run with SELKIES_TEST_PYTHON pointing at the interpreter whose pixelflux build is
under test (tests/helpers.py starts the server with it).

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

# Counts decoded VideoFrames and records every decoder configuration, so the
# checks can tell a full-frame session (one decoder) from a striped one (one
# decoder per stripe) and prove the bytes actually decode rather than merely
# arrive. Headless rAF throttling zeroes the client's own fps counter, so the
# WebSocket binary frames are counted as well.
INSTRUMENT_JS = """
  window.__wsFrames = 0;
  window.__decoded = 0;
  window.__cfgs = [];
  window.__decodeErrors = [];
  (() => {
    const WS = window.WebSocket;
    window.WebSocket = function(...a) {
      const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
      s.addEventListener('message', (e) => {
        if (e.data instanceof ArrayBuffer) window.__wsFrames++;
      });
      return s;
    };
    window.WebSocket.prototype = WS.prototype;
    Object.setPrototypeOf(window.WebSocket, WS);
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
        [H.PYTHON, "-c", "import pixelflux; print(pixelflux.SOFTWARE_H264_ENCODER)"],
        capture_output=True, text=True, timeout=60)
    name = out.stdout.strip()
    return name or None


def read_state(page) -> dict:
    """The instrumentation counters: WS frames, decoded frames, decoder configs, errors."""
    return page.evaluate("""(() => ({
      frames: window.__wsFrames || 0,
      decoded: window.__decoded || 0,
      cfgs: window.__cfgs || [],
      errors: window.__decodeErrors || [],
    }))()""")


def wait_state(page, predicate, timeout: float = 30) -> dict:
    """Poll the page state until `predicate` holds or `timeout` passes; the last state."""
    deadline = time.time() + timeout
    state = read_state(page)
    while time.time() < deadline and not predicate(state):
        time.sleep(0.5)
        state = read_state(page)
    return state


def wait_log_after(mark: int, substr: str, timeout: float = 20) -> bool:
    """Whether `substr` appears in the server log past byte offset `mark` within `timeout`."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if substr in H.server_log()[mark:]:
            return True
        time.sleep(0.5)
    return substr in H.server_log()[mark:]


def run_block(r: "H.Results", wayland: bool) -> None:
    """Stream full-frame then striped software H.264 on one backend and check
    that the build's encoder is named in the logs and decodes in the browser."""
    encoder = server_software_encoder()
    r.check("server pixelflux names its software H.264 encoder",
            encoder in ("x264", "openh264"), encoder)
    from playwright.sync_api import sync_playwright
    # Only software encoding is pinned: an operator encoder pin would narrow the
    # published menu to that one encoder, and the switch below must stay allowed.
    H.server_start(mode="websockets", wayland=wayland,
                   extra_env={"SELKIES_USE_CPU": "true"})
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
                state = wait_state(page, lambda s: s["frames"] >= 24 and s["decoded"] >= 12)
                r.check("h264enc: frames flow", state["frames"] >= 24, state["frames"])
                r.check("h264enc: frames decode", state["decoded"] >= 12, state["decoded"])
                r.check("h264enc: no decoder errors", not state["errors"], state["errors"][:3])
                heights = sorted({c["h"] for c in state["cfgs"] if c.get("h")})
                r.check("h264enc: one full-frame decoder geometry", len(heights) == 1, heights)
                full_h = heights[0] if heights else 0
                log = H.server_log()
                r.check("server names the software encoder for the session",
                        f"encodes H.264 in software ({encoder})" in log, encoder)
                stream_line = f"Mode: H264 ({encoder})" if wayland else f"Encoder: CPU ({encoder})"
                r.check("pixelflux's stream-settings line names the software encoder",
                        stream_line in log, stream_line)

                mark = len(H.server_log())
                before = read_state(page)
                C.settings_change(page, {"encoder": "h264enc-striped"})
                restarted = wait_log_after(mark, "Capture started", timeout=20)
                r.check("h264enc-striped: capture restarted", restarted, "")

                def stripe_decoders(s: dict) -> list:
                    return [c for c in s["cfgs"][len(before["cfgs"]):]
                            if c.get("h") and c["h"] < full_h]
                state = wait_state(
                    page,
                    lambda s: s["decoded"] >= before["decoded"] + 24 and len(stripe_decoders(s)) >= 2,
                    timeout=40)
                r.check("h264enc-striped: frames keep decoding",
                        state["decoded"] >= before["decoded"] + 24,
                        (before["decoded"], state["decoded"]))
                r.check("h264enc-striped: a decoder per stripe",
                        len(stripe_decoders(state)) >= 2,
                        sorted({c["h"] for c in stripe_decoders(state)}))
                r.check("h264enc-striped: no decoder errors", not state["errors"], state["errors"][:3])
                tail = H.server_log()[mark:]
                r.check("h264enc-striped: the session is the build's software encoder",
                        f"encodes H.264 in software ({encoder})" in tail, encoder)
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
