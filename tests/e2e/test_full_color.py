#!/usr/bin/env python3
"""Full colour is asked for only where the engine can decode it.

`video_fullcolor` makes the H.264 encoders emit 4:4:4, which is High 4:4:4
Predictive on the wire, and engines differ on whether their decoder has that
profile. One that does not shows no picture at all rather than a worse one,
with every stripe decoder built and refused for as long as the session runs, so
the client settles the question against its own decoder before it asks the
server for anything. This suite asks each engine the same way rather than
naming which ones can: what an engine decodes changes with its releases.

Driven with the setting stored on before the page loads, the way a user who
turned it on in Chromium and then opened the same URL in Safari arrives. Both
transports, because the server encodes the same 4:4:4 for either.

Firefox is covered on websockets alone: its WebRTC answer needs the OpenH264
plugin the browser matrix side-loads into a profile of its own, and the answer
about its decoder is the same on either transport.

Usage: python3 tests/e2e/test_full_color.py ws-webkit
"""
import os
import sys
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

ENGINES = ("chromium", "firefox", "webkit")
# The profile the full-colour encoders emit, at the lowest level, so what the
# probe answers is about the profile and not the size of any one stream.
FULLCOLOR_CODEC = "avc1.F4001E"

PROBE_JS = """async (codec) => {
  if (typeof VideoDecoder === 'undefined') return false;
  try {
    const s = await VideoDecoder.isConfigSupported(
      {codec, codedWidth: 1280, codedHeight: 720});
    return !!(s && s.supported);
  } catch (e) { return false; }
}"""

STORED_JS = """() => {
  const k = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
  return localStorage.getItem(k + '_video_fullcolor');
}"""


def init_script(mode: str) -> str:
    """Stores the encoder and full colour before the client's first line runs."""
    return """
window.__SELKIES_STREAMING_MODE__ = '%s';
(() => {
  const k = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
  localStorage.setItem(k + '_encoder', '%s');
  localStorage.setItem(k + '_video_fullcolor', 'true');
})();
""" % (mode, "h264enc-striped" if mode == "websockets" else "h264enc")


def drive_locked(res: "H.Results", p: Any, pinned: bool = False) -> None:
    """The server holding full colour on, for an engine that cannot decode it.

    A client cannot turn a locked setting off, so it takes the ladder's own
    last rung instead: the JPEG encoder, whose stripes need no `VideoDecoder`
    at all. What must not happen is the stripe decoders being built and refused
    for as long as the session runs, with nothing on the page to say why.

    Args:
        pinned: The encoder is held to H.264 as well, so the rung is refused
            too and there is nothing left for the client to do but say so.
    """
    if pinned:
        drive_pinned(res, p)
        return
    browser = C.launch_browser(p, "webkit")
    try:
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        ctx.add_init_script(init_script("websockets"))
        page = ctx.new_page()
        said = []
        page.on("console", lambda m: said.append(m.text))
        page.goto(H.BASE_URL, wait_until="load")
        played = bool(C.wait_ws_video(page, timeout=45))
        encoder = page.evaluate(STORED_JS.replace("_video_fullcolor", "_encoder"))
        switched = [t for t in said if "has no decoder for avc1.F4" in t]
        res.check("[locked] the 4:4:4 the server insists on is named once",
                  len(switched) == 1, switched or said[-2:])
        res.check("[locked] the client takes the rung that needs no decoder",
                  encoder == "jpeg", encoder)
        res.check("[locked] and the stream plays there", played, played)
        spam = [t for t in said if "Error configuring VNC stripe decoder" in t]
        res.check("[locked] no stripe is left reporting the refusal per frame",
                  not spam, spam[:2])
    finally:
        browser.close()


def drive_pinned(res: "H.Results", p: Any) -> None:
    """Full colour and the encoder both held by the server: nothing the client
    may change reaches a stream it can decode, so it says so on the page rather
    than staying black and quiet."""
    browser = C.launch_browser(p, "webkit")
    try:
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        ctx.add_init_script(init_script("websockets"))
        page = ctx.new_page()
        said = []
        page.on("console", lambda m: said.append(m.text))
        page.goto(H.BASE_URL, wait_until="load")
        page.wait_for_timeout(20000)
        told = [t for t in said if "which this browser cannot decode" in t]
        res.check("[pinned] the stream it cannot decode is reported once",
                  len(told) == 1, told or said[-2:])
        shown = page.evaluate(
            "() => { const e = document.getElementById('status-display');"
            " return e ? [e.className, e.textContent.slice(0, 60)] : null; }")
        res.check("[pinned] and said on the page, not only in the console",
                  shown and "hidden" not in shown[0], shown)
        spam = [t for t in said if "Error configuring VNC stripe decoder" in t]
        res.check("[pinned] no stripe is left reporting the refusal per frame",
                  not spam, spam[:2])
    finally:
        browser.close()


def drive(res: "H.Results", engine: str, mode: str, p: Any) -> None:
    """One engine: what its decoder answers, and what the client then does."""
    tag = f"{engine}-{mode}"
    browser = C.launch_browser(p, engine)
    try:
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        ctx.add_init_script(init_script(mode))
        page = ctx.new_page()
        warnings = []
        page.on("console", lambda m: warnings.append(m.text))
        page.goto(H.BASE_URL, wait_until="load")
        decodable = page.evaluate(PROBE_JS, FULLCOLOR_CODEC)

        video = (C.wait_wr_video(page, timeout=45) if mode == "webrtc"
                 else C.wait_ws_video(page, timeout=45))
        res.check(f"[{tag}] the stream plays with full colour asked for",
                  bool(video), video)
        res.check(f"[{tag}] full colour survives exactly where it decodes",
                  (page.evaluate(STORED_JS) == "true") == decodable,
                  f"stored={page.evaluate(STORED_JS)} decodable={decodable}")
        said = [w for w in warnings if "full colour (4:4:4) is off" in w]
        res.check(f"[{tag}] turning it off is said once, and only then",
                  (len(said) > 0) == (not decodable), said[:1] or decodable)
        refused = [w for w in warnings if "config not supported" in w]
        res.check(f"[{tag}] no stripe decoder is left asking for a profile it lacks",
                  not refused, refused[:1])
    finally:
        browser.close()


def main() -> "H.Results":
    """One engine on one transport per run, the way the browser matrix is
    driven: a WebRTC session left behind by one engine is still winding down
    when the next connects, and the second sees no video for that alone."""
    selector = sys.argv[1] if len(sys.argv) > 1 else "ws-chromium"
    short, engine = selector.split("-", 1)
    mode = "webrtc" if short == "wr" else "websockets"
    res = H.Results(f"full-color-{selector}")
    locked = engine in ("locked", "pinned")
    env = {"SELKIES_VIDEO_FULLCOLOR": "true|locked"} if locked else None
    if engine == "pinned":
        env["SELKIES_ENCODER"] = "h264enc-striped"
    H.server_start(mode=mode, wayland=False, extra_env=env)
    try:
        with sync_playwright() as p:
            if locked:
                drive_locked(res, p, pinned=engine == "pinned")
            else:
                drive(res, engine, mode, p)
    finally:
        H.server_stop()
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
