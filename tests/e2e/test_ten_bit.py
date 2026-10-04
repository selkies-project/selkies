#!/usr/bin/env python3
"""10-bit is streamed only where the server encodes it and the engine shows it.

`video_10bit` makes an encoder that carries it emit its codec's 10-bit profile
(H.265 Main 10, VP9 profile 2, AV1 at 10 bits), and an engine that cannot decode
that shows no picture rather than a worse one. Engines accept 10-bit decoder
configurations they then fail to decode, so the client settles the question by
decoding a 10-bit key frame before it asks the server for anything, and this
suite holds it to the outcome rather than to its own answer: with the setting
stored on before the page loads, the picture painted on the server has to decode
in the page, at 10 bits where the client kept the setting and the host encodes
them, at 8 bits on the same codec everywhere else. Chromium's WebRTC AV1 is one
such: libwebrtc's dav1d wrapper drops every picture deeper than 8 bits, where
WebCodecs decodes them, and a stream that opened at 10 bits never showed one.

The host's encoders decide how much a run covers: a codec the encode node
carries no 10 bits for runs on the build's software encoder where that codes
them, and at 8 bits where neither does, and the run holds whichever applies. A
`-cpu` suffix forces the software encoder, and `striped` is the striped H.264
path. `ws-refused` and `wr-refused` drive an engine whose decoder gives
no 10-bit picture back, whatever this one's really does, and `ws-default` the
server's own unlocked default against it.

Usage: python3 tests/e2e/test_ten_bit.py ws-chromium-av1
"""
import os
import sys
import time
from typing import Any, List, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
import test_encoders as TENC
from playwright.sync_api import sync_playwright

CODECS = {"av1": ("av1enc", "AV1"), "vp9": ("vp9enc", "VP9"), "h265": ("h265enc", "H265"),
          "h264": ("h264enc", "H264"), "striped": ("h264enc-striped", "H264")}

STORED_JS = """() => {
  const k = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
  return localStorage.getItem(k + '_video_10bit');
}"""

# A decoder that takes every configuration and gives no frame back for a 10-bit
# one: what Firefox does with VP9 profile 2, on any engine.
REFUSE_TEN_BIT_JS = """
(() => {
  if (typeof VideoDecoder === 'undefined') return;
  const tenBit = (cfg) => /^(hev1\\.2\\.|vp09\\.0[23]\\.|av01\\..*\\.10$)/i.test((cfg && cfg.codec) || '');
  const configure = VideoDecoder.prototype.configure;
  VideoDecoder.prototype.configure = function (cfg) {
    this.__tenBit = tenBit(cfg);
    return configure.call(this, cfg);
  };
  const decode = VideoDecoder.prototype.decode;
  VideoDecoder.prototype.decode = function (chunk) {
    if (this.__tenBit) throw new DOMException('Decoding error', 'EncodingError');
    return decode.call(this, chunk);
  };
  const caps = RTCRtpReceiver.getCapabilities.bind(RTCRtpReceiver);
  RTCRtpReceiver.getCapabilities = (kind) => {
    const answer = caps(kind);
    if (!answer || kind !== 'video') return answer;
    return Object.assign({}, answer, {codecs: answer.codecs.filter((c) =>
      !/profile-id=[23]/.test(c.sdpFmtpLine || ''))});
  };
})();
"""


def init_script(mode: str, encoder: str, stored: bool = True) -> str:
    """Stores the encoder, and 10-bit where asked, before the client's first line runs."""
    return """
window.__SELKIES_STREAMING_MODE__ = '%s';
(() => {
  const k = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
  localStorage.setItem(k + '_encoder', '%s');
  %s
})();
""" % (mode, encoder, "localStorage.setItem(k + '_video_10bit', 'true');" if stored else "")


def host_carries(codec: str, software: bool = False) -> bool:
    """Whether this host encodes `codec` at 10 bits, 4:2:0: on its encode node, or failing
    that in the build's software encoder, which is where such a session then runs."""
    import pixelflux
    name = "h264" if codec == "striped" else codec
    return ((not software and "420-10" in dict(pixelflux.hardware_formats()).get(name, []))
            or "420-10" in dict(pixelflux.SOFTWARE_FORMATS).get(name, []))


def encoder_lines() -> List[str]:
    """The encoder lines pixelflux printed, oldest first."""
    return [line for line in H.server_log().splitlines() if "] Encoder: " in line]


def wait_depth(bits: int, timeout: float = 20) -> str:
    """The newest encoder line once it names `bits`, else the newest one seen."""
    deadline = time.time() + timeout
    while True:
        lines = encoder_lines()
        if lines and f"{bits}-bit" in lines[-1]:
            return lines[-1]
        if time.time() >= deadline:
            return lines[-1] if lines else ""
        time.sleep(0.5)


def wait_video(page: Any, mode: str) -> Optional[dict]:
    return C.wait_wr_video(page, timeout=45) if mode == "webrtc" else C.wait_ws_video(page, timeout=45)


def drive(res: "H.Results", p: Any, engine: str, mode: str, codec: str, picture: "TENC.Picture",
          refused: bool = False, stored: bool = True, cpu: bool = False) -> None:
    """One engine on one codec: the painted picture decodes, at the depth the
    client settled on and the host encodes."""
    encoder, mode_name = CODECS[codec]
    tag = f"{engine}-{mode}-{codec}{'-cpu' if cpu else ''}{'-refused' if refused else ''}{'' if stored else '-default'}"
    carried = host_carries(codec, software=cpu)
    browser = C.launch_browser(p, engine)
    try:
        ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
        ctx.add_init_script(init_script(mode, encoder, stored) + (REFUSE_TEN_BIT_JS if refused else ""))
        page = ctx.new_page()
        said: List[str] = []
        page.on("console", lambda m: said.append(m.text))
        page.goto(H.BASE_URL + ("/?offscreen_worker=false" if refused and mode == "websockets" else "/"),
                  wait_until="load")
        res.check(f"[{tag}] the stream plays with 10-bit asked for", bool(wait_video(page, mode)), said[-3:])
        encoder_now, since = None, time.time()
        while time.time() - since < 4:
            now = page.evaluate("window.encoder")
            if now != encoder_now:
                encoder_now, since = now, time.time()
            time.sleep(0.5)
        if encoder_now != encoder:
            # The engine does not decode the codec at all, which is the codec ladder's to
            # answer and says nothing of 10 bits.
            res.check(f"[{tag}] the engine does not play {codec}; the ladder moved to a codec it does",
                      bool(picture.matches(picture.wait(page))), encoder_now)
            return
        kept = (page.evaluate(STORED_JS) != "false") if mode == "websockets" else bool(page.evaluate("window.video_10bit"))
        if refused:
            res.check(f"[{tag}] an engine that shows no 10-bit picture has it turned off", not kept, said[-3:])
        want = 10 if kept and carried else 8
        line = wait_depth(want)
        res.check(f"[{tag}] the server streams {want}-bit {mode_name} (host carries 10-bit: {carried}, kept: {kept})",
                  f"{want}-bit" in line and mode_name in line or (not carried and "10-bit" not in line), line[-120:])
        sample = picture.wait(page)
        res.check(f"[{tag}] the painted picture decodes", picture.matches(sample), sample)
        off = [t for t in said if "10-bit is off" in t]
        if mode == "websockets":
            res.check(f"[{tag}] turning it off is said once, and only then", len(off) == (0 if kept else 1), off or kept)
    finally:
        C.close_browser(browser)


def main() -> "H.Results":
    """One engine on one transport and codec per run, as the browser matrix is driven."""
    selector = sys.argv[1] if len(sys.argv) > 1 else "ws-chromium-av1"
    parts = selector.split("-")
    mode = "webrtc" if parts[0] == "wr" else "websockets"
    engine = parts[1]
    codec = parts[2] if len(parts) > 2 else "av1"
    cpu = parts[-1] == "cpu"
    res = H.Results(f"ten-bit-{selector}")
    refused = engine in ("refused", "default")
    env = {"SELKIES_ENCODER": f"{CODECS[codec][0]},h264enc,jpeg"}
    if cpu:
        env["SELKIES_USE_CPU"] = "true"
    if engine == "default":
        env["SELKIES_VIDEO_10BIT"] = "true"
    H.server_start(mode=mode, wayland=False, extra_env=env)
    picture = TENC.Picture(False)
    try:
        picture.paint()
        with sync_playwright() as p:
            drive(res, p, "chromium" if refused else engine, mode, codec, picture,
                  refused=refused, stored=engine != "default", cpu=cpu)
    finally:
        picture.clear()
        H.server_stop()
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
