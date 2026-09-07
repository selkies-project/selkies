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

A ``-vp9`` suffix drives the same question for VP9, whose 4:4:4 is profile 1:
``ws-chromium-vp9`` asks the WebCodecs decoder, ``wr-chromium-vp9`` the RTP
receiver's capabilities, which is what the WebRTC client consults.

``ws-default`` and ``wr-default`` hold full colour on through the server's own
unlocked default against an engine that refuses it: the client turns it off
and streams 4:2:0 on the same codec.

Usage: python3 tests/e2e/test_full_color.py ws-webkit
"""
import os
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
import test_software_h264 as TS
from playwright.sync_api import sync_playwright

ENGINES = ("chromium", "firefox", "webkit")
# The profile the full-colour encoders emit, at the lowest level, so what the
# probe answers is about the profile and not the size of any one stream.
FULLCOLOR_CODEC = "avc1.F4001E"
VP9_FULLCOLOR_CODEC = "vp09.01.10.08.03"

RTP_VP9_PROBE_JS = """() => {
  try {
    return RTCRtpReceiver.getCapabilities('video').codecs.some((c) =>
      /^video\\/vp9$/i.test(c.mimeType) && /(^|;)profile-id=1(;|$)/.test(c.sdpFmtpLine || ''));
  } catch (e) { return false; }
}"""

# The answer is parked on the window and collected with a deadline: an engine's
# `isConfigSupported` has none of its own, and an evaluate awaiting a promise that
# never settles would hold the suite to the runner's kill.
PROBE_JS = """(codec) => {
  window.__fullcolorProbe = undefined;
  (async () => {
    if (typeof VideoDecoder === 'undefined') return false;
    try {
      const s = await VideoDecoder.isConfigSupported(
        {codec, codedWidth: 1280, codedHeight: 720});
      return !!(s && s.supported);
    } catch (e) { return false; }
  })().then((v) => { window.__fullcolorProbe = v; }, () => { window.__fullcolorProbe = false; });
}"""

STORED_JS = """() => {
  const k = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
  return localStorage.getItem(k + '_video_fullcolor');
}"""


def probe_decoder(page: Any, codec: str, timeout: float = 20.0) -> Any:
    """Whether the engine's decoder takes `codec`, or None when it never answers."""
    page.evaluate(PROBE_JS, codec)
    try:
        page.wait_for_function("() => window.__fullcolorProbe !== undefined",
                               timeout=timeout * 1000)
    except Exception:
        return None
    return page.evaluate("() => window.__fullcolorProbe")


def init_script(mode: str, encoder: Optional[str] = None) -> str:
    """Stores the encoder and full colour before the client's first line runs."""
    return """
window.__SELKIES_STREAMING_MODE__ = '%s';
(() => {
  const k = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
  localStorage.setItem(k + '_encoder', '%s');
  localStorage.setItem(k + '_video_fullcolor', 'true');
})();
""" % (mode, encoder or ("h264enc-striped" if mode == "websockets" else "h264enc"))


# A decoder without the 4:4:4 profile, whatever this engine's really has: the
# locked scenarios are about what the client does with a refusal, and which
# engines refuse changes with their releases and the codecs their host carries.
REFUSE_FULLCOLOR_JS = """
(() => {
  if (typeof VideoDecoder === 'undefined') return;
  const refused = (cfg) => /^avc1\\.f4/i.test((cfg && cfg.codec) || '');
  const supported = VideoDecoder.isConfigSupported.bind(VideoDecoder);
  VideoDecoder.isConfigSupported = (cfg) =>
    refused(cfg) ? Promise.resolve({supported: false, config: cfg}) : supported(cfg);
  const configure = VideoDecoder.prototype.configure;
  VideoDecoder.prototype.configure = function (cfg) {
    if (refused(cfg)) throw new DOMException('Unsupported configuration', 'NotSupportedError');
    return configure.call(this, cfg);
  };
})();
"""
# The stripe decoders on the page rather than in the video worker, where a
# script injected into the page cannot reach them.
PAGE_DECODE_URL = H.BASE_URL + "/?offscreen_worker=false"

# A decoder that takes the probe and never answers it, which is what WebKit's
# does on a loaded machine.
STALL_PROBE_JS = """
(() => {
  if (typeof VideoDecoder === 'undefined') return;
  VideoDecoder.isConfigSupported = () => new Promise(() => {});
})();
"""


def settings_sent(page: Any) -> int:
    """How many settings payloads the page has put on the wire."""
    return page.evaluate("(window.__wireSent || [])"
                         ".filter(d => typeof d === 'string' && d.startsWith('SETTINGS,')).length")


def drive_stalled(res: "H.Results", p: Any, mode: str) -> None:
    """A decoder that never answers the probe, on the transport under test.

    The client asks its own decoder before it asks the server for anything, so
    an unanswered probe stands between the session and its first message: what
    must not happen is the session waiting on it. Full colour is dropped as it
    is for a decoder that refuses outright, since an engine that will not say
    cannot be shown a stream only its 4:4:4 profile could decode.
    """
    browser = C.chromium_launch(p)
    try:
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        ctx.add_init_script(init_script(mode) + STALL_PROBE_JS + C.WIRE_TAP_JS)
        page = ctx.new_page()
        started = time.time()
        page.goto(H.BASE_URL, wait_until="load")
        deadline = started + 45
        while time.time() < deadline and not settings_sent(page):
            time.sleep(0.5)
        res.check("[stalled] the session's settings reach the server", settings_sent(page) > 0,
                  f"{settings_sent(page)} sent in {time.time() - started:.0f}s")
        video = (C.wait_wr_video(page, timeout=60) if mode == "webrtc"
                 else C.wait_ws_video(page, timeout=60))
        res.check("[stalled] and the stream plays", bool(video), video)
        res.check("[stalled] full colour is dropped, as for a decoder that refuses",
                  page.evaluate(STORED_JS) == "false", page.evaluate(STORED_JS))
    finally:
        C.close_browser(browser)


def drive_openh264(res: "H.Results", p: Any, tag: str) -> None:
    """The striped encoder of an OpenH264 build cannot emit 4:4:4, so a locked
    full colour reaches the client as 4:2:0, which every engine decodes:
    nothing is refused, nothing is said, and the stream plays where it is."""
    browser = C.launch_browser(p, "webkit")
    try:
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        ctx.add_init_script(init_script("websockets") + REFUSE_FULLCOLOR_JS)
        page = ctx.new_page()
        said = []
        page.on("console", lambda m: said.append(m.text))
        page.goto(PAGE_DECODE_URL, wait_until="load")
        played = bool(C.wait_ws_video(page, timeout=45))
        encoder = page.evaluate(STORED_JS.replace("_video_fullcolor", "_encoder"))
        res.check(f"[{tag}] OpenH264 streams 4:2:0 under a locked full colour",
                  "Colorspace: I420" in H.server_log(), H.server_log()[-200:])
        res.check(f"[{tag}] and the stream plays where it is", played and encoder == "h264enc-striped",
                  (played, encoder))
        refused = [t for t in said if "has no decoder for" in t or "cannot decode" in t]
        res.check(f"[{tag}] nothing is refused", not refused, refused[:2])
    finally:
        browser.close()


def drive_locked(res: "H.Results", p: Any, pinned: bool = False) -> None:
    """The server holding full colour on, for an engine that cannot decode it.

    A client cannot turn a locked setting off, so it walks the refusal ladder
    instead: the next allowed video encoder whose stream it decodes at the
    locked full colour, or whose codec carries none, and JPEG only when no
    video codec is left. What must not happen is the stripe decoders being
    built and refused for as long as the session runs, with nothing on the
    page to say why.

    Args:
        pinned: The encoder is held to H.264 as well, so the rung is refused
            too and there is nothing left for the client to do but say so.
    """
    if TS.server_software_encoder() == "openh264":
        drive_openh264(res, p, "pinned" if pinned else "locked")
        return
    if pinned:
        drive_pinned(res, p)
        return
    browser = C.launch_browser(p, "webkit")
    try:
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        ctx.add_init_script(init_script("websockets") + REFUSE_FULLCOLOR_JS)
        page = ctx.new_page()
        said = []
        page.on("console", lambda m: said.append(m.text))
        page.goto(PAGE_DECODE_URL, wait_until="load")
        played = bool(C.wait_ws_video(page, timeout=45))
        # The ladder may take more than one step; read the encoder once it holds still.
        encoder, since = None, time.time()
        while time.time() - since < 4:
            now = page.evaluate(STORED_JS.replace("_video_fullcolor", "_encoder"))
            if now != encoder:
                encoder, since = now, time.time()
            time.sleep(0.5)
        switched = [t for t in said if "has no decoder for avc1.F4" in t]
        res.check("[locked] the 4:4:4 the server insists on is named once",
                  len(switched) == 1, switched or said[-2:])
        res.check("[locked] the client steps to a video codec it decodes before JPEG",
                  encoder in ("vp8enc", "vp9enc", "av1enc"), encoder)
        res.check("[locked] and the stream plays there", played, played)
        spam = [t for t in said if "Error configuring VNC stripe decoder" in t]
        res.check("[locked] no stripe is left reporting the refusal per frame",
                  not spam, spam[:2])
    finally:
        C.close_browser(browser)


def drive_pinned(res: "H.Results", p: Any) -> None:
    """Full colour and the encoder both held by the server: nothing the client
    may change reaches a stream it can decode, so it says so on the page rather
    than staying black and quiet."""
    browser = C.launch_browser(p, "webkit")
    try:
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        ctx.add_init_script(init_script("websockets") + REFUSE_FULLCOLOR_JS)
        page = ctx.new_page()
        said = []
        page.on("console", lambda m: said.append(m.text))
        page.goto(PAGE_DECODE_URL, wait_until="load")
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
        C.close_browser(browser)


def drive_default(res: "H.Results", engine: str, mode: str, p: Any) -> None:
    """The server's own default holding full colour on, unlocked, for an engine
    whose decoder has no 4:4:4 H.264: the client turns the setting off for
    itself and the stream comes back 4:2:0 on the same codec, not on JPEG and
    not as a stream this browser paints nothing of."""
    tag = f"{engine}-{mode}-default"
    browser = C.launch_browser(p, engine)
    try:
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        ctx.add_init_script(init_script(mode).replace("localStorage.setItem(k + '_video_fullcolor', 'true');", "")
                            + REFUSE_FULLCOLOR_JS)
        page = ctx.new_page()
        said = []
        page.on("console", lambda m: said.append(m.text))
        page.goto(PAGE_DECODE_URL if mode == "websockets" else H.BASE_URL, wait_until="load")
        video = (C.wait_wr_video(page, timeout=45) if mode == "webrtc"
                 else C.wait_ws_video(page, timeout=45))
        res.check(f"[{tag}] the stream plays", bool(video), video)
        if mode == "webrtc":
            # The hello named no 4:4:4, so the server settled it before the offer.
            res.check(f"[{tag}] the server turned full colour off before the offer",
                      C.wait_log("streams 4:2:0", timeout=20), H.server_log()[-300:])
            res.check(f"[{tag}] and the page's toggle shows it off",
                      page.evaluate("() => window.video_fullcolor") is False,
                      page.evaluate("() => window.video_fullcolor"))
        else:
            res.check(f"[{tag}] the client turned the server's full colour off",
                      page.evaluate(STORED_JS) == "false", page.evaluate(STORED_JS))
            res.check(f"[{tag}] and said so once",
                      len([t for t in said if "full colour (4:4:4) is off" in t]) == 1, said[-3:])
        res.check(f"[{tag}] the server streams 4:2:0 on the same codec",
                  C.wait_log("Colorspace: I420", timeout=20) and "Mode: H264" in H.server_log()[-4000:],
                  H.server_log()[-300:])
        encoder = page.evaluate(STORED_JS.replace("_video_fullcolor", "_encoder"))
        res.check(f"[{tag}] no ladder step to JPEG", encoder != "jpeg", encoder)
    finally:
        browser.close()


def drive(res: "H.Results", engine: str, mode: str, p: Any, vp9: bool = False) -> None:
    """One engine: what its decoder answers, and what the client then does."""
    tag = f"{engine}-{mode}{'-vp9' if vp9 else ''}"
    browser = C.launch_browser(p, engine)
    try:
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        ctx.add_init_script(init_script(mode, "vp9enc" if vp9 else None))
        page = ctx.new_page()
        warnings = []
        page.on("console", lambda m: warnings.append(m.text))
        page.goto(H.BASE_URL, wait_until="load")
        if vp9 and mode == "webrtc":
            decodable = page.evaluate(RTP_VP9_PROBE_JS)
        else:
            decodable = probe_decoder(page, VP9_FULLCOLOR_CODEC if vp9 else FULLCOLOR_CODEC)
        res.check(f"[{tag}] the decoder answers the profile probe", decodable is not None, decodable)

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
        C.close_browser(browser)


def main() -> "H.Results":
    """One engine on one transport per run, the way the browser matrix is
    driven: a WebRTC session left behind by one engine is still winding down
    when the next connects, and the second sees no video for that alone."""
    selector = sys.argv[1] if len(sys.argv) > 1 else "ws-chromium"
    short, engine = selector.split("-", 1)
    vp9 = engine.endswith("-vp9")
    engine = engine[:-4] if vp9 else engine
    mode = "webrtc" if short == "wr" else "websockets"
    res = H.Results(f"full-color-{selector}")
    locked = engine in ("locked", "pinned")
    default = engine == "default"
    env = {"SELKIES_VIDEO_FULLCOLOR": "true|locked"} if locked else None
    if engine == "pinned":
        env["SELKIES_ENCODER"] = "h264enc-striped"
    if default:
        env = {"SELKIES_VIDEO_FULLCOLOR": "true"}
    if vp9:
        env = {"SELKIES_ENCODER": "vp9enc,h264enc,jpeg"}
    H.server_start(mode=mode, wayland=False, extra_env=env)
    try:
        with sync_playwright() as p:
            if locked:
                drive_locked(res, p, pinned=engine == "pinned")
            elif engine == "stalled":
                drive_stalled(res, p, mode)
            elif default:
                drive_default(res, "webkit" if mode == "websockets" else "chromium", mode, p)
            else:
                drive(res, engine, mode, p, vp9)
    finally:
        H.server_stop()
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
