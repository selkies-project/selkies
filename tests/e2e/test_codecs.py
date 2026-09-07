#!/usr/bin/env python3
"""Every video codec, decoded by each browser engine, over both transports.

For each codec the server is started with ``SELKIES_ENCODER`` set to
its encoder and the page opens in Chromium, Firefox or WebKit. Over WebSockets
the stream must come up and the painted picture decode whatever the engine can
do: an engine whose WebCodecs decoder takes the codec keeps it (the page's
encoder stays the requested one and the server's stream line names the codec),
one that refuses it steps down the client's ladder to ``h264enc`` and the
picture still decodes there. The engine's own answer to
``VideoDecoder.isConfigSupported`` decides which of the two is required, except
that an engine which takes the configuration and refuses the stream at decode
(the page says so) is held to the refusal outcome. Over
WebRTC the engine's own RTP receiver decides (``RTCRtpReceiver.getCapabilities``):
a codec it takes is negotiated and streamed, one it declines is answered with
H.264 and the display moves to ``h264enc``, logged by the server.

The picture is a known colour the test paints on the server, sampled from the
decoded frame in the page, exactly as ``test_encoders.py`` does.

    python3 tests/e2e/test_codecs.py ws-x11|ws-wl|wr-x11 [chromium|firefox|webkit|all]

A selector may carry the engine as a third part (``ws-x11-firefox``); without
one every engine runs.
"""
import os
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_encoders as TENC
from playwright.sync_api import sync_playwright

CODECS = [("h264enc", "H264", "avc1.64001F", "video/H264"),
          ("h265enc", "H265", "hev1.1.6.L93.B0", "video/H265"),
          ("vp8enc", "VP8", "vp8", "video/VP8"),
          ("vp9enc", "VP9", "vp09.00.31.08", "video/VP9"),
          ("av1enc", "AV1", "av01.0.05M.08", "video/AV1")]
ENGINES = ("chromium", "firefox", "webkit")

PROBE_JS = """async (codec) => {
  if (typeof VideoDecoder === 'undefined') return false;
  try {
    const s = await VideoDecoder.isConfigSupported({codec, codedWidth: 1280, codedHeight: 720});
    return !!(s && s.supported);
  } catch (e) { return false; }
}"""

RTP_PROBE_JS = """(mime) => {
  try {
    const caps = RTCRtpReceiver.getCapabilities('video');
    return !!caps && caps.codecs.some((c) => c.mimeType.toLowerCase() === mime.toLowerCase());
  } catch (e) { return false; }
}"""


def wait_stream_mode(mode_name: str, timeout: float = 15) -> str:
    """The server's latest stream line once it names `mode_name`, else the last one seen."""
    deadline = time.time() + timeout
    line = TENC.last_stream_line()
    while time.time() < deadline and f"Mode: {mode_name}" not in line:
        time.sleep(0.5)
        line = TENC.last_stream_line()
    return line


def open_engine_page(p: Any, engine: str, mode: str) -> tuple:
    """A page of `engine` on the core client in `mode`, with its browser or context to close."""
    if engine == "firefox":
        ctx = C.firefox_persistent_context(p, viewport={"width": 1280, "height": 720})
        owner = ctx
    else:
        browser = C.launch_browser(p, engine)
        ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
        owner = browser
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    # Firefox runs on one persistent profile: the encoder a previous block's
    # ladder stored must not become this block's pick.
    ctx.add_init_script("try { localStorage.clear(); } catch (e) {}")
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    return owner, page


def wait_settled_encoder(page: Any, timeout: float = 20) -> Optional[str]:
    """The page's encoder once it has stopped moving for a few seconds: the
    ladder answers a refusal within a second of the first frame."""
    deadline = time.time() + timeout
    last, since = None, time.time()
    while time.time() < deadline:
        enc = page.evaluate("window.encoder")
        if enc != last:
            last, since = enc, time.time()
        elif time.time() - since > 4.0:
            return enc
        time.sleep(0.5)
    return last


def block_codec(mode: str, wayland: bool, engine: str, encoder: str, mode_name: str,
                probe: str, rtp_mime: str, res: "H.Results") -> None:
    tag = f"{engine} {encoder}"
    # WebKit paints VP8, which carries no colour matrix, as BT.709 on both
    # transports, and VP9 as BT.709 over RTP, where its receiver ignores the
    # matrix the header declares; the stream is BT.601 for every other engine.
    matrix = not (engine == "webkit" and (encoder == "vp8enc" or (encoder == "vp9enc" and mode == "webrtc")))
    # The codec under test is the default; the ladder's rungs stay allowed.
    H.server_start(mode=mode, wayland=wayland,
                   extra_env={"SELKIES_ENCODER": f"{encoder},h264enc,jpeg"})
    picture = TENC.Picture(wayland)
    try:
        picture.paint()
        with sync_playwright() as p:
            owner, page = open_engine_page(p, engine, mode)
            said: list = []
            page.on("console", lambda m: said.append(m.text))
            try:
                if mode == "webrtc":
                    taken = page.evaluate(RTP_PROBE_JS, rtp_mime)
                    video = C.wait_wr_video(page)
                    res.check(f"{tag}: stream up", bool(video), video)
                    if taken:
                        res.check(f"{tag}: the browser took {mode_name} over RTP",
                                  C.wait_log(f"negotiated {rtp_mime}", timeout=10), "")
                        line = wait_stream_mode(mode_name)
                        res.check(f"{tag}: the server streams {mode_name}",
                                  f"Mode: {mode_name}" in line, TENC.encoder_field(line))
                    else:
                        res.check(f"{tag}: the browser declined it, so the display moved to h264enc",
                                  C.wait_log("is not decoded by a WebRTC peer", timeout=10)
                                  and C.wait_log("using 'h264enc'", timeout=10), "")
                        line = wait_stream_mode("H264")
                        res.check(f"{tag}: the server streams H.264 instead",
                                  "Mode: H264" in line, TENC.encoder_field(line))
                    sample = picture.wait(page, matrix=matrix)
                    res.check(f"{tag}: the painted picture decodes", picture.matches(sample, matrix), sample)
                    print(f"      {tag}: rtp={'yes' if taken else 'no'} {TENC.encoder_field(line)}")
                    return
                supported = page.evaluate(PROBE_JS, probe)
                video = C.wait_ws_video(page, timeout=30)
                res.check(f"{tag}: stream up", bool(video), video)
                settled = wait_settled_encoder(page)
                line = TENC.last_stream_line()
                refused_at_decode = any("refused at decode" in t for t in said)
                if refused_at_decode:
                    supported = False
                if supported:
                    res.check(f"{tag}: the engine decodes it, so the codec is kept",
                              settled == encoder, f"page encoder {settled}")
                    res.check(f"{tag}: the server streams {mode_name}",
                              f"Mode: {mode_name}" in line, TENC.encoder_field(line))
                else:
                    res.check(f"{tag}: the engine refuses it, so the ladder steps to h264enc",
                              settled in ("h264enc", "jpeg"), f"page encoder {settled}")
                    res.check(f"{tag}: the server followed the fallback",
                              "Mode: H264" in line or "Mode: JPEG" in line, TENC.encoder_field(line))
                fps = 0
                for _ in range(20):
                    fps = page.evaluate("window.fps || 0")
                    if fps > 0:
                        break
                    time.sleep(0.5)
                res.check(f"{tag}: frames present", fps > 0, f"fps {fps}")
                sample = picture.wait(page, matrix=matrix)
                res.check(f"{tag}: the painted picture decodes", picture.matches(sample, matrix), sample)
                print(f"      {tag}: probe={'refused at decode' if refused_at_decode else ('yes' if supported else 'no')}"
                      f" settled={settled} {TENC.encoder_field(line)}")
            finally:
                owner.close()
    finally:
        picture.clear()
        H.server_stop()


def main() -> bool:
    cell = sys.argv[1] if len(sys.argv) > 1 else "ws-x11"
    parts = cell.split("-")
    transport, backend = parts[0], parts[1]
    which = parts[2] if len(parts) > 2 else (sys.argv[2] if len(sys.argv) > 2 else "all")
    mode = "websockets" if transport == "ws" else "webrtc"
    wayland = backend == "wl"
    engines = ENGINES if which == "all" else (which,)
    if mode == "webrtc":
        engines = tuple(e for e in engines if e == "chromium")
    res = H.Results(f"codecs {cell}")
    for engine in engines:
        for encoder, mode_name, probe, rtp_mime in CODECS:
            block_codec(mode, wayland, engine, encoder, mode_name, probe, rtp_mime, res)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
