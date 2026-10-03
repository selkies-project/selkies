"""1. Check all encoders, CRF/CBR, paintover, video streaming mode, 4:4:4 colors, and CPU encoding working properly.

Driven from the dashboard's own controls (the classic dashboard's video
section, which the images serve): every encoder its menu offers under CBR and
then CRF, Turbo (the video streaming mode) with paint-over on and off, 4:4:4
where its switch is offered, and CPU encoding. Each is verified on the decoded
picture -- the session's own browser shows a known pattern, and its four
quadrants must come back in the page -- and by the stream description the
server publishes (encoder, hardware, codec, chroma), which the stats read.
"""
import time
from typing import Any, Optional

from image_lib import pattern_matches, chroma_kept, TD

ITEM = 1
TITLE = "encoders, CRF/CBR, paint-over, Turbo, 4:4:4, CPU"
CODEC = {"h264enc": "h264", "h264enc-striped": "h264", "h265enc": "h265", "vp8enc": "vp8",
         "vp9enc": "vp9", "av1enc": "av1", "jpeg": "jpeg"}

# Playwright's WebKit, a GStreamer port, hands WebRTC VP8 to libwebrtc's decoder
# where GStreamer's best is vp8dec, and stamps those frames with their render
# time: zero under the stream's zero playout delay, and on no clock the codecs
# GStreamer decodes share. Its player drops each one as late and holds the frame
# before, so only the stream is checked there. Safari stamps them with the
# capture time.
PICTURE_UNCHECKED = {("webkit", "webrtc", "vp8enc"):
                     "WebKit's GStreamer player drops the VP8 frames libwebrtc decodes as late"}

# The longest a cleanup runs ("half a minute at most"), with a margin, and
# the low rate it is judged at (the sliders' floor, where a still screen needs it most).
SETTLE = 40
LOW_RATE = "(0.1 Mbps)"
# Frames that reached the page: the WebSockets client counts the chunks it
# received; a <video> counts the frames it presented (requestVideoFrameCallback,
# which every engine has, unlike a playback-quality count of a live stream).
FRAMES_JS = """() => {
  const v = document.querySelector('video');
  if (v && v.videoWidth > 0 && v.style.display !== 'none') {
    if (v.__itFrames === undefined && v.requestVideoFrameCallback) {
      v.__itFrames = 0;
      const cb = (now, meta) => { v.__itFrames = meta.presentedFrames; v.requestVideoFrameCallback(cb); };
      v.requestVideoFrameCallback(cb);
    }
    return v.__itFrames || 0;
  }
  return window.videoChunksReceived || 0;
}"""


def info(page: Any) -> dict:
    return page.evaluate("window.stream_info || {}") or {}


def described(page: Any, want: dict, timeout: float = 25) -> dict:
    """The stream description once every field in `want` reads as asked, else the last one."""
    deadline, got = time.time() + timeout, {}
    while time.time() < deadline:
        got = info(page)
        if all(got.get(k) == v for k, v in want.items()):
            return got
        time.sleep(0.5)
    return got


def frames_while(cell: Any, page: Any, secs: float) -> int:
    page.evaluate(FRAMES_JS)
    time.sleep(0.1)
    before = page.evaluate(FRAMES_JS)
    time.sleep(secs)
    return page.evaluate(FRAMES_JS) - before


SLIDER_JS = """([id, idx]) => {
  const el = document.getElementById(id);
  if (!el) return null;
  if (idx !== null) {
    Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value').set.call(el, String(idx));
    el.dispatchEvent(new Event('input', {bubbles: true}));
    el.dispatchEvent(new Event('change', {bubbles: true}));
  }
  const label = document.querySelector(`label[for="${id}"]`);
  return [Number(el.value), Number(el.max), label ? label.textContent.trim() : ''];
}"""


def slider(page: Any, sid: str, idx: Optional[int] = None) -> Optional[list]:
    """A dashboard slider's `[index, max, label]`, after moving it to `idx` when given."""
    got = page.evaluate(SLIDER_JS, [sid, idx])
    if idx is not None:
        time.sleep(2.0)
        got = page.evaluate(SLIDER_JS, [sid, None])
    return got


def slider_to(page: Any, sid: str, label: str) -> Optional[list]:
    """Move a slider to the first stop whose label reads `label`."""
    got = page.evaluate(SLIDER_JS, [sid, None])
    if not got:
        return None
    for idx in range(got[1] + 1):
        got = page.evaluate(SLIDER_JS, [sid, idx])
        if label in got[2]:
            break
    time.sleep(2.0)
    return page.evaluate(SLIDER_JS, [sid, None])


def toggle(page: Any, sel: str, on: bool) -> Optional[bool]:
    """Set a classic dashboard toggle; its state after, or None where it is not offered."""
    el = page.locator(sel)
    if not el.count():
        return None
    if TD.switch_on(el.first) != on:
        el.first.click(timeout=5000)
        time.sleep(1.5)
    return TD.switch_on(el.first)


def run(cell: Any) -> None:
    R = cell.res
    cell.go("/pattern.html")
    page = cell.open()
    first = cell.wait_pattern(page, 30)
    R.check("the stream shows the session's picture", pattern_matches(first), first)
    TD.classic_open_video(page)
    if not page.locator("#encoderSelect").count():
        R.check("the dashboard offers the encoder menu", False, "no #encoderSelect")
        return
    menu = page.eval_on_selector_all("#encoderSelect option",
                                     "els => els.map(e => [e.value, e.disabled, e.textContent.trim()])")
    offered = [v for v, off, _ in menu if not off]
    allowed = (cell.settings(page).get("encoder") or {}).get("allowed") or []
    R.check("the encoder menu offers what the server allows", set(offered) <= set(allowed) and offered,
            f"menu {offered}, server {allowed}")
    for value, off, text in menu:
        if off:
            R.skip(f"{value}", f"the menu greys it out here: {text}")
    # A run that stopped midway leaves its pick on the server; start from the defaults.
    page.select_option("#encoderSelect", "h264enc", timeout=5000)
    TD.pick_rate_control(page, "classic", "cbr")
    for enc in offered:
        page.select_option("#encoderSelect", enc, timeout=5000)
        # JPEG has a quality, not a rate control.
        for rc in ("cbr", "crf") if enc != "jpeg" else ("quality",):
            picked = rc == "quality" or TD.pick_rate_control(page, "classic", rc)
            d = described(page, {"codec": CODEC.get(enc, enc)})
            name = f"{enc} {rc.upper() if rc != 'quality' else ''}"
            used = f"({d.get('encoder')}{', hardware' if d.get('hardware') else ''})"
            why = PICTURE_UNCHECKED.get((cell.engine, cell.transport, enc))
            if why:
                R.check(f"{name}: streams {CODEC[enc]} {used}", picked and d.get("codec") == CODEC[enc],
                        f"codec {d.get('codec')} ({d.get('encoder_reason') or 'no reason'})")
                R.skip(f"{name}: decodes the picture", why)
                continue
            s = cell.wait_pattern(page, 25)
            R.check(f"{name}: decodes the picture {used}",
                    picked and pattern_matches(s) and d.get("codec") == CODEC.get(enc, enc),
                    f"sample {s and s['points']} codec {d.get('codec')} ({d.get('encoder_reason') or 'no reason'})")
    page.select_option("#encoderSelect", "h264enc", timeout=5000)
    TD.pick_rate_control(page, "classic", "cbr")
    described(page, {"codec": "h264"})

    # Turbo and paint-over at a low rate on a detailed still picture: with Turbo
    # off the screen gets frames after it stops only while paint-over cleans it
    # up, and ends cleaner than without; with Turbo on every frame goes out.
    rate = slider(page, "videoBitrateSlider")
    toggle(page, "#videoStreamingModeToggle", False)
    low = slider_to(page, "videoBitrateSlider", LOW_RATE)
    # Each arm starts from a settled rate and a texture of its own, not from a
    # picture the other arm refined.
    cell.go("/texture.html?motion=1&seed=3")
    time.sleep(3)
    errors: dict = {}
    for paint, seed in ((True, 1), (False, 2)):
        t = TD.switch_on(page.locator("#videoStreamingModeToggle").first)
        p = toggle(page, "#usePaintOverQualityToggle", paint)
        cell.go(f"/texture.html?motion=1&seed={seed}")
        time.sleep(3)
        cell.go(f"/texture.html?seed={seed}")
        stopped = time.time()
        after, quiet = 0, 0
        while time.time() - stopped < SETTLE and quiet < 2:
            n = frames_while(cell, page, 1.0)
            after += n
            quiet = quiet + 1 if n <= 1 else 0
        settled = round(time.time() - stopped - quiet, 1) if quiet >= 2 else None
        errors[paint] = cell.texture_error(page, seed)
        cell.shot(page, f"paint-over-{'on' if paint else 'off'}")
        how = f"{after} frames after the stop, quiet after {settled} s; error {errors[paint]} levels"
        if paint:
            R.check(f"Turbo off, paint-over on at {low and low[2]}: a still screen is cleaned up, then "
                    "sends nothing", t is False and p and after >= 1 and settled is not None, how)
        else:
            R.check(f"Turbo off, paint-over off at {low and low[2]}: a still screen sends nothing",
                    t is False and p is False and settled is not None and settled <= 3, how)
    R.check("paint-over leaves the still screen no worse than without it",
            None not in errors.values() and errors[True] <= errors[False] + 1,
            f"error {errors[True]} levels with it, {errors[False]} without")
    slider(page, "videoBitrateSlider", rate[0] if rate else None)
    t = toggle(page, "#videoStreamingModeToggle", True)
    still = frames_while(cell, page, 3.0)
    R.check("Turbo on: every frame keeps going out on a still screen", t and still >= 10, f"{still} frames in 3 s")
    cell.go("/pattern.html")

    # 4:4:4 keeps the one-pixel chroma columns apart that 4:2:0 averages.
    on = toggle(page, "#videoFullColorToggle", True)
    if on is None:
        R.skip("4:4:4", "the dashboard offers no full-color switch here (server or browser cannot)")
    else:
        d = described(page, {"fullcolor": True})
        s = cell.wait_pattern(page, 20)
        R.check("4:4:4 on: the stream is full color and keeps one-pixel chroma",
                on and d.get("fullcolor") and chroma_kept(s), {"info": d.get("fullcolor"), "band": s and s["band"][:4]})
        toggle(page, "#videoFullColorToggle", False)
        d = described(page, {"fullcolor": False})
        s = cell.wait_pattern(page, 20)
        R.check("4:4:4 off: back to 4:2:0", d.get("fullcolor") is False and chroma_kept(s) is False,
                {"info": d.get("fullcolor"), "band": s and s["band"][:4]})

    # CPU encoding: the software encoder takes over wherever a GPU encoded, and
    # the GPU takes the stream back once it is off again.
    before = info(page)
    cpu = toggle(page, "#useCpuToggle", True)
    if cpu is None:
        R.skip("CPU encoding", f"no CPU switch where nothing encodes on a GPU ({info(page).get('encoder')} already)")
    else:
        d = described(page, {"hardware": False})
        s = cell.wait_pattern(page, 25)
        R.check(f"CPU encoding on: a software encoder ({d.get('encoder')}) streams the picture",
                cpu and d.get("hardware") is False and pattern_matches(s), d.get("encoder"))
        toggle(page, "#useCpuToggle", False)
        back = bool(before.get("hardware"))
        d = described(page, {"hardware": back})
        s = cell.wait_pattern(page, 25)
        R.check(f"CPU encoding off: {before.get('encoder')} streams the picture again",
                pattern_matches(s) and bool(d.get("hardware")) == back,
                f"{before.get('encoder')} before, {d.get('encoder')} after")
