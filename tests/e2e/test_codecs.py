#!/usr/bin/env python3
"""Every video codec, decoded by each browser engine, over both transports.

For each codec the server is started with ``SELKIES_ENCODER`` set to
its encoder and the page opens in Chromium, Firefox, or WebKit. Over WebSockets
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

The picture is a known color the test paints on the server, sampled from the
decoded frame in the page, exactly as ``test_encoders.py`` does; over WebRTC
the <video> must then keep presenting frames while the screen changes.

    python3 tests/e2e/test_codecs.py ws-x11|ws-wl|wr-x11 [chromium|firefox|webkit|all]

A selector may carry the engine as a third part (``ws-x11-firefox``); without
one every engine runs.
"""
import os
import re
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
ENGINES = TENC.ENGINES

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
    """The server's stream line naming `mode_name` once one is printed, else the
    last one seen. A pipeline prints its line from its capture thread when the
    encoder comes up, so a stopped pipeline's can land after its replacement's:
    the newest line alone does not say what streams now."""
    deadline = time.time() + timeout
    while True:
        lines = [l for l in H.server_log().splitlines() if "Stream settings active" in l]
        named = [l for l in lines if f"Mode: {mode_name}" in l]
        if named:
            return named[-1]
        if time.time() >= deadline:
            return lines[-1] if lines else ""
        time.sleep(0.5)


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
    # Every stream declares the matrix it converts with -- BT.709, or BT.601 for
    # VP8, whose keyframe header carries a single bit that can name no other.
    # Two engines paint some streams with a matrix of their own, twenty levels
    # off on the saturated block through no fault of the stream: WebKit's
    # GStreamer ports take a WebCodecs VP8 stream as BT.709 above 576 lines
    # (over WebRTC the color-space header extension reaches them), and Firefox
    # before 157 decodes WebRTC AV1 through libwebrtc's dav1d, whose frames it
    # paints as BT.601 whatever they declare.
    matrix = not (engine == "webkit" and encoder == "vp8enc" and mode == "websockets")
    # WebKit's GStreamer ports hand WebRTC VP8 to libwebrtc's decoder where
    # GStreamer's best is vp8dec and stamp its frames with their render time,
    # zero under the stream's zero playout delay: the player presents the first
    # few and drops the rest as late. Safari stamps them with the capture time.
    held = ("WebKit's GStreamer player drops the VP8 frames libwebrtc decodes as late"
            if engine == "webkit" and encoder == "vp8enc" else "")
    # The codec under test is the default; the ladder's rungs stay allowed.
    H.server_start(mode=mode, wayland=wayland,
                   extra_env={"SELKIES_ENCODER": f"{encoder},h264enc,jpeg"})
    picture = TENC.Picture(wayland, live=mode == "webrtc")
    try:
        picture.paint()
        with sync_playwright() as p:
            owner, page = TENC.open_page(p, mode, engine)
            said: list = []
            page.on("console", lambda m: said.append(m.text))
            try:
                if mode == "webrtc":
                    taken = page.evaluate(RTP_PROBE_JS, rtp_mime)
                    video = C.wait_wr_video(page)
                    res.check(f"{tag}: stream up", bool(video), video)
                    if taken and engine == "firefox" and encoder == "av1enc":
                        version = re.search(r"Firefox/(\d+)", page.evaluate("navigator.userAgent"))
                        matrix = bool(version) and int(version.group(1)) >= 157
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
                    picture.keeps_presenting(res, tag, page, waived=held if taken else "")
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
                    # The page settles before the capture restarts behind its request;
                    # on Wayland the compositor's restart is the slower of the two.
                    line = wait_stream_mode("H264" if settled == "h264enc" else "JPEG")
                    res.check(f"{tag}: the server followed the fallback",
                              "Mode: H264" in line or "Mode: JPEG" in line, TENC.encoder_field(line))
                fps = 0
                for _ in range(20):
                    if "Mode: JPEG" in line:
                        # JPEG sends only what damage covers, so a still picture presents
                        # no frames: repaint it so the rate measures the stream.
                        picture.clear()
                        picture.paint()
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
    res = H.Results(f"codecs {cell}")
    for engine in engines:
        for encoder, mode_name, probe, rtp_mime in CODECS:
            block_codec(mode, wayland, engine, encoder, mode_name, probe, rtp_mime, res)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
