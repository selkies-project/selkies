#!/usr/bin/env python3
"""Browser-engine matrix: Chromium (Chrome binary), Firefox, WebKit over the
websockets transport (flow, audio, input, clipboard, resize, console health),
plus a reduced WebRTC flow on Firefox (parity with the Chrome reference) and
WebKit. Over WebSockets, video and audio reach the page and playback on every
engine, through libopus in WASM where the engine has no WebCodecs audio, and
play on the output device the page picks. Over WebRTC, a manual resolution
shown 1:1 is drawn nearest-sampled in Chromium only, since Firefox's compositor
draws that slower than a smoothed video."""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright
from typing import Optional


# A field of the class the dashboards put on their root, whose keys the input
# core leaves to the page, and a point on it.
NATIVE_FIELD_JS = """(() => {
  const f = document.createElement('input');
  f.id = 'native-field';
  f.className = 'allow-native-input';
  f.style.cssText = 'position:fixed;left:20px;top:20px;width:200px;height:30px;z-index:9999';
  document.body.appendChild(f);
})()"""
NATIVE_FIELD_AT = (120, 35)

DECODER_ERROR_PATTERNS = (
    "Failed to load resource", "Unexpected server response:", "ResizeObserver",
    "Error getting media devices", "AudioContext was not allowed",
    "FATAL DECODER ERROR", "vp8_codec_error_msg", "av_send_packet_error",
)


FF_E2E_PROFILE = C.FF_E2E_PROFILE
openh264_version = C.openh264_version


def engine_launch(p, engine: str, prefs: Optional[dict] = None):
    """Return (browser_or_none, ctx). Firefox needs a persistent profile that
    carries the OpenH264 GMP plugin (bundled profile ships without it, so the
    WebRTC answer rejects the video m-line) plus autoplay/clipboard prefs, and
    `prefs` on top."""
    if engine == "chromium":
        b = C.chromium_launch(p)
        return b, b.new_context(viewport={"width": 1280, "height": 720, "deviceScaleFactor": 1})
    if engine == "firefox":
        ctx = C.firefox_persistent_context(
            p, viewport={"width": 1280, "height": 720, "deviceScaleFactor": 1}, prefs=prefs)
        return None, ctx
    b = getattr(p, engine).launch(headless=True)
    return b, b.new_context(viewport={"width": 1280, "height": 720, "deviceScaleFactor": 1})


def hold(page, key: str):
    """Press `key` and leave it down; whether the X keymap then shows it held.

    Headless WebKit drops synthetic keydowns under load, so the whole press is
    retried rather than waited on.
    """
    pressed = False
    for _ in range(4):
        page.keyboard.down(key)
        time.sleep(0.8)
        pressed = C.x11_keymap_pressed(key)
        if pressed is True:
            break
        page.keyboard.up(key)
        time.sleep(0.6)
    return pressed


def engine_block(engine: str, mode: str = "websockets") -> "H.Results":
    """Run the flow/input/clipboard/resize checklist on one browser engine.

    Args:
        engine: Playwright engine name: ``chromium``, ``firefox``, or ``webkit``.
        mode: Transport mode, ``websockets`` or ``webrtc``.

    Returns:
        The Results accumulator for this engine's checks.
    """
    tag = f"{engine}-{mode}"
    res = H.Results(tag)
    H.server_start(mode=mode, wayland=False)
    with sync_playwright() as p:
        browser, ctx = engine_launch(p, engine)
        ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
        if engine == "chromium":
            # WebKit doesn't implement the clipboard permissions schema at all.
            try:
                ctx.grant_permissions(["clipboard-read", "clipboard-write"], origin=H.BASE_URL)
            except Exception:
                pass
        page = ctx.pages[0] if (engine == "firefox" and ctx.pages) else ctx.new_page()
        console_errors = []
        page.on("console", lambda m: console_errors.append(m.text) if m.type == "error" else None)
        page.on("pageerror", lambda e: console_errors.append(str(e)))
        not_found = []
        page.on("response", lambda r: not_found.append(r.url) if r.status == 404 else None)
        page.add_init_script("""
          window.__clipMsgs = [];
          window.__wsFrames = 0;
          window.addEventListener('message', (e) => {
            if (e.data && e.data.type === 'clipboardContentUpdate') window.__clipMsgs.push(e.data);
          });
          (() => {
            const WS = window.WebSocket;
            window.WebSocket = function(...a) {
              const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
              s.addEventListener('message', (e) => { if (e.data instanceof ArrayBuffer) window.__wsFrames++; });
              return s;
            };
            window.WebSocket.prototype = WS.prototype;
            Object.setPrototypeOf(window.WebSocket, WS);
          })();
        """)
        page.goto(H.BASE_URL, wait_until="load")
        # Headless WebKit drops synthetic key events after a long idle render
        # round; wait_ws_video polls for the painted canvas anyway.
        time.sleep(2.0 if engine == "webkit" else 6.0)

        try:
            if mode == "websockets":
                info = C.wait_ws_video(page, timeout=25)
                res.check("video: canvas painted", info is not None,
                          info or "no canvas>=640")
                deadline = time.time() + 12
                frames = 0
                while time.time() < deadline:
                    frames = page.evaluate("window.videoChunksReceived || window.__wsFrames") or 0
                    if frames >= 24:
                        break
                    time.sleep(0.5)
                res.check("video: WS frames flowing", frames >= 24, frames)
            else:
                info = C.wait_wr_video(page, timeout=45)
                res.check("video: <video> receiving", info is not None, info)

            page.mouse.click(640, 360)
            time.sleep(0.5)
            if mode == "websockets":
                # The capture sends nothing while the desktop is silent.
                tone = H.pulse_sine()
                try:
                    deadline = time.time() + 12
                    depth = 0
                    while time.time() < deadline:
                        depth = page.evaluate("window.currentAudioBufferSize || 0") or 0
                        if depth > 0:
                            break
                        time.sleep(0.5)
                    res.check("audio: packets reach playback", depth > 0, depth)
                finally:
                    H.pulse_unload(tone)
            pressed = hold(page, "x")
            res.check("input: key held in X keymap", pressed is True, pressed)
            page.keyboard.up("x")
            time.sleep(0.3)
            res.check("input: key released in X keymap",
                      C.x11_keymap_pressed("x") is False, "")
            page.evaluate(NATIVE_FIELD_JS)
            pressed = hold(page, "x")
            page.mouse.click(*NATIVE_FIELD_AT)
            page.keyboard.up("x")
            time.sleep(0.5)
            res.check("input: a key let go in a dashboard field is released",
                      pressed is True and C.x11_keymap_pressed("x") is False, pressed)
            page.evaluate("document.getElementById('native-field').remove()")
            page.mouse.click(640, 360)
            time.sleep(0.3)

            push = f"e2e-{tag}-s2c"
            ext, stop = H.x_own_clipboard(push.encode())
            got = []
            deadline = time.time() + (10 if engine == "webkit" else 6)
            while time.time() < deadline:
                got = page.evaluate("window.__clipMsgs.map(m => m.text)")
                if push in got:
                    break
                page.wait_for_timeout(500)
            stop["flag"] = True
            res.check("clipboard: server push reached page", push in got, repr(got)[-120:])

            probe = f"e2e-{tag}-c2s"
            C.send_clipboard_from_client(page, probe, engine)
            if engine == "webkit":
                # Headless WebKit strips clipboardData from synthetic paste events
                # and has no system clipboard; a skip claims no coverage.
                res.skip("clipboard: client text reached server",
                         "headless WebKit has no system clipboard")
            else:
                deadline = time.time() + 8
                got_text = None
                while time.time() < deadline:
                    got_text = H.x_read_clipboard(timeout=3)
                    if got_text == probe:
                        break
                    time.sleep(0.5)
                res.check("clipboard: client text reached server", got_text == probe, repr(got_text))

            # Headless WebKit's viewport quirks make the realized root size
            # unreliable.
            if engine != "webkit":
                req_w, req_h = 1242, 694
                page.set_viewport_size({"width": req_w, "height": req_h})
                deadline = time.time() + 12
                realized = None
                while time.time() < deadline:
                    realized = H.x_root_size()
                    if realized and abs(realized[0] - req_w) <= 16 and abs(realized[1] - req_h) <= 64:
                        break
                    time.sleep(0.5)
                res.check("resize: X root follows browser", realized and abs(realized[0] - req_w) <= 16,
                          f"req={req_w}x{req_h} actual={realized}")

            if mode == "webrtc":
                # Inside the window: the page caps the video at its container.
                for message in ({"type": "setScaleLocally", "value": False},
                                {"type": "setManualResolution", "width": 1024, "height": 576}):
                    page.evaluate("(m) => window.postMessage(m, window.location.origin)", message)
                deadline = time.time() + 15
                size = None
                while time.time() < deadline:
                    size = page.evaluate("(() => { const v = document.getElementById('stream'); "
                                         "return v ? [v.videoWidth, v.videoHeight] : null; })()")
                    if size == [1024, 576]:
                        break
                    time.sleep(0.5)
                time.sleep(1.0)
                got = page.evaluate("document.getElementById('stream').style.imageRendering")
                want = "pixelated" if engine == "chromium" else "auto"
                res.check("render: a 1:1 manual resolution is nearest-sampled in Chromium only",
                          size == [1024, 576] and got == want, f"stream={size} rendering={got} want={want}")

            real_errors = [e for e in console_errors
                           if not any(p_ in e for p_ in DECODER_ERROR_PATTERNS)]
            benign = [u for u in not_found if u.endswith("/manifest.json") or "favicon" in u]
            bad404 = [u for u in not_found if u not in benign]
            res.check("no console errors (filtered)", len(real_errors) == 0,
                      "; ".join(real_errors)[:160])
            res.check("no unexpected 404s", not bad404, bad404[:2])
        finally:
            if browser:
                browser.close()
            else:
                ctx.close()
    res.summary()
    return res


def striped_block(engine: str) -> "H.Results":
    """The striped encoder on one engine: the video worker decodes, composites,
    and presents it off the page where the engine allows.

    chromium and firefox take the divert (fps counts the worker's composites,
    the worker canvas is the visible sink). Playwright's WebKit claims support
    for striped decoder configs but every decode fails (`EncodingError`), in
    the worker and on the page ladder alike, leaving its software-decode retry
    cycling; so there the checks stop at the wire flowing without page errors,
    and real Safari cannot be exercised in this harness.
    """
    tag = f"{engine}-striped"
    res = H.Results(tag)
    H.server_start(mode="websockets", wayland=False,
                   extra_env={"SELKIES_ENCODER": "h264enc-striped"})
    with sync_playwright() as p:
        browser, ctx = engine_launch(p, engine)
        ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
        page = ctx.pages[0] if (engine == "firefox" and ctx.pages) else ctx.new_page()
        page_errors = []
        page.on("pageerror", lambda e: page_errors.append(str(e)))
        page.goto(H.BASE_URL, wait_until="load")
        # Polled, not sampled once: which counter is live depends on the divert,
        # and an engine that takes it optimistically and falls back on the first
        # decode failure flips between them while the stream is coming up. The
        # worker's own rows and composites are what settle it, and they trail
        # the first chunks by as long as its decoders take to come up.
        settled = ("() => (window.videoChunksReceived || 0) > 0" if engine == "webkit" else """() => {
          const c = document.getElementById('videoWorkerCanvas');
          return !!window.videoDivertOn && Object.keys(window.videoStripeRows || {}).length > 1
            && (window.fps || 0) > 0 && !!c && c.width >= 640;
        }""")
        deadline = time.time() + 30
        while time.time() < deadline:
            time.sleep(1.0)
            if page.evaluate(settled):
                break
        try:
            state = page.evaluate("""({
              divert: !!window.videoDivertOn,
              rows: Object.keys(window.videoStripeRows || {}).length,
              chunks: window.videoChunksReceived || 0,
              fps: window.fps || 0,
              enc: window.encoder,
              workerCanvas: (() => {
                const c = document.getElementById('videoWorkerCanvas');
                return c ? {w: c.width, shown: c.style.display !== 'none'} : null;
              })(),
              mainShown: (() => {
                const c = document.getElementById('videoCanvas');
                return c ? c.style.display !== 'none' : null;
              })(),
            })""")
            res.check("striped stream reaches the client",
                      state["enc"] == "h264enc-striped" and state["chunks"] > 0, state)
            if engine == "webkit":
                res.skip("worker decode assertions",
                         "Playwright WebKit fails decoding stripe streams outright")
            else:
                res.check("the video worker takes the striped divert",
                          state["divert"] and state["rows"] > 1, state)
                res.check("the worker presents (fps counts its composites)",
                          state["fps"] > 0, state)
                res.check("the worker canvas is the visible sink",
                          bool(state["workerCanvas"]) and state["workerCanvas"]["shown"]
                          and state["workerCanvas"]["w"] >= 640 and not state["mainShown"], state)
            res.check("no page errors", not page_errors, "; ".join(page_errors)[:160])
        finally:
            if browser:
                browser.close()
            else:
                ctx.close()
    res.summary()
    return res


def sink_block(engine: str) -> "H.Results":
    """Where frames are presented is said, including when the sink is not the
    one asked for.

    The video worker is the first choice on every engine, so a session that
    ends up on the page canvas because the worker could not start is the one
    whose sink most needs naming -- and the one that used to say nothing. The
    worker is blocked the way a content policy that forbids blob workers
    blocks it: the constructor throws.

    The generator sink itself is out of reach here: it needs
    `VideoTrackGenerator` in a worker, which WebKit gates on a preference that
    defaults on under `PLATFORM(COCOA)` alone, so the build this matrix launches
    reports a canvas sink however the shipping browser behaves. What a browser
    that has it settles on is read from the line this check makes it print.
    """
    res = H.Results(f"sink-{engine}")
    H.server_start(mode="websockets", wayland=False)
    try:
        with sync_playwright() as p:
            browser = C.launch_browser(p, engine)
            try:
                ctx = browser.new_context(viewport={"width": 1280, "height": 720})
                ctx.add_init_script("window.Worker = function () { throw new Error('blocked'); };")
                page = ctx.new_page()
                said = []
                page.on("console", lambda m: said.append(m.text))
                page.goto(H.BASE_URL, wait_until="load")
                res.check(f"[{engine}] the stream plays without the worker",
                          bool(C.wait_ws_video(page, timeout=45)), "")
                sinks = [t for t in said if "video sink:" in t]
                res.check(f"[{engine}] the sink it fell back to is named",
                          any("canvas on the page" in t or "MediaStreamTrackGenerator" in t
                              for t in sinks), sinks[:2])
                res.check(f"[{engine}] and named once, not per handshake",
                          len(sinks) == len(set(sinks)), sinks)
            finally:
                browser.close()
    finally:
        H.server_stop()
    res.summary()
    return res


def wasm_block(engine: str, channels: int = 2) -> "H.Results":
    """An engine without WebCodecs audio plays the stream's sound through
    libopus in WASM: the decode worker says it fell back, and the playback
    worklet is fed PCM that carries the desktop's tone. Surround comes as
    pcmflux's multistream packets, split and decoded a stream at a time."""
    res = H.Results(f"wasm-{engine}-{channels}ch")
    H.server_start(mode="websockets", wayland=False,
                   extra_env={"SELKIES_AUDIO_CHANNELS": str(channels)} if channels != 2 else None)
    try:
        with sync_playwright() as p:
            browser, ctx = engine_launch(p, engine)
            try:
                ctx.add_init_script(C.NO_WEBCODECS_AUDIO_JS)
                page = ctx.pages[0] if (engine == "firefox" and ctx.pages) else ctx.new_page()
                said, errors = [], []
                page.on("console", lambda m: said.append(m.text))
                page.on("pageerror", lambda e: errors.append(str(e)))
                page.goto(H.BASE_URL, wait_until="load")
                res.check(f"[{engine}] video up", bool(C.wait_ws_video(page, timeout=45)), "")
                page.mouse.click(640, 360)
                tone = H.pulse_sine()
                depth = level = 0
                try:
                    deadline = time.time() + 15
                    while time.time() < deadline:
                        depth = page.evaluate("window.currentAudioBufferSize || 0") or 0
                        level = page.evaluate("window.currentAudioLevel || 0") or 0
                        if depth > 0 and level > 0:
                            break
                        time.sleep(0.5)
                finally:
                    H.pulse_unload(tone)
                res.check(f"[{engine}] the decode worker fell back on libopus in WASM",
                          any("decodes on libopus in WASM" in t for t in said),
                          [t for t in said if "Audio Decoder" in t][:3])
                res.check(f"[{engine}] {channels}-channel packets reach playback", depth > 0, depth)
                res.check(f"[{engine}] and play as sound, not silence", level > 0, level)
                res.check(f"[{engine}] no page errors", not errors, "; ".join(errors)[:200])
            finally:
                if browser:
                    browser.close()
                else:
                    ctx.close()
    finally:
        H.server_stop()
    res.summary()
    return res


def output_block(engine: str) -> "H.Results":
    """The output device a page picks carries its sound. Two null sinks stand in
    for speakers; the page is told to play to one and then the other
    (`audioDeviceSelected`), and the desktop's tone is heard on that sink's
    monitor and not on the other's. Chromium picks through
    AudioContext.setSinkId, Firefox, which has none, through a media element's."""
    import test_microphone_audio as M
    res = H.Results(f"output-{engine}")
    names = ("selkies_pick_a", "selkies_pick_b")
    sinks = [H.pulse_null_sink(n, sink_properties=f"device.description={n}") for n in names]
    tone = None
    H.server_start(mode="websockets", wayland=False)
    try:
        with sync_playwright() as p:
            if engine == "chromium":
                # Heard, not muted: the shared flags mute Chromium's output, and headless
                # Chromium plays nothing to the sound server, so it runs on the test display.
                kw = {"headless": False, "args": [a for a in C.BROWSER_ARGS if a != "--mute-audio"],
                      "env": {**os.environ, "DISPLAY": H.TEST_DISPLAY}}
                if C.CHROME_PATH:
                    kw["executable_path"] = C.CHROME_PATH
                browser = p.chromium.launch(**kw)
                ctx = browser.new_context(viewport={"width": 1280, "height": 720})
                ctx.grant_permissions(["microphone"], origin=H.BASE_URL)
            else:
                # The microphone grant that names the outputs, without a prompt no one answers.
                browser, ctx = engine_launch(p, engine, prefs={"media.navigator.permission.disabled": True})
            try:
                page = ctx.pages[0] if (engine == "firefox" and ctx.pages) else ctx.new_page()
                errors, said = [], []
                page.on("pageerror", lambda e: errors.append(str(e)))
                page.on("console", lambda m: said.append(m.text) if "output" in m.text.lower() else None)
                page.goto(H.BASE_URL, wait_until="load")
                res.check(f"[{engine}] video up", bool(C.wait_ws_video(page, timeout=45)), "")
                page.mouse.click(640, 360)
                tone = H.pulse_sine()
                # Output devices are named to a page that holds a microphone grant.
                outputs = page.evaluate("""async () => {
                  try { const s = await navigator.mediaDevices.getUserMedia({ audio: true });
                        s.getTracks().forEach((t) => t.stop()); } catch (e) { /* named or not, list them */ }
                  return (await navigator.mediaDevices.enumerateDevices())
                    .filter((d) => d.kind === 'audiooutput').map((d) => [d.deviceId, d.label]);
                }""")
                for target in names:
                    other = names[1 - names.index(target)]
                    device = next((d for d in outputs if target in d[1]), None)
                    res.check(f"[{engine}] {target} is offered as an output", device is not None, outputs)
                    if device is None:
                        continue
                    page.evaluate("(id) => window.postMessage({ type: 'audioDeviceSelected', context: 'output', "
                                  "deviceId: id }, location.origin)", device[0])
                    time.sleep(2.5)
                    heard = {n: M.analyze(M.record(f"{n}.monitor", 2.0), 440) for n in names}
                    res.check(f"[{engine}] picked, {target} plays the desktop's tone",
                              heard[target]["rms"] > 150 and heard[target]["ratio"] > 0.5, (heard[target], said[-2:]))
                    res.check(f"[{engine}] and {other} goes quiet", heard[other]["rms"] < 50, heard[other])
                res.check(f"[{engine}] no page errors", not errors, "; ".join(errors)[:200])
            finally:
                if browser:
                    browser.close()
                else:
                    ctx.close()
    finally:
        H.pulse_unload(tone)
        for module in sinks:
            H.pulse_unload(module)
        H.server_stop()
    res.summary()
    return res


def main() -> None:
    """Run the engine blocks named on argv (default: all available)."""
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = []
    try:
        if which in ("all", "chromium-ws"):
            blocks.append(engine_block("chromium", "websockets"))
        if which in ("all", "firefox-ws"):
            blocks.append(engine_block("firefox", "websockets"))
        if which in ("all", "webkit-ws"):
            blocks.append(engine_block("webkit", "websockets"))
        if which in ("all", "sink"):
            blocks.append(sink_block("webkit"))
            blocks.append(sink_block("chromium"))
        if which in ("all", "output"):
            blocks.append(output_block("chromium"))
            blocks.append(output_block("firefox"))
        if which in ("all", "wasm"):
            blocks.append(wasm_block("chromium"))
            blocks.append(wasm_block("firefox"))
            blocks.append(wasm_block("webkit"))
            blocks.append(wasm_block("chromium", channels=6))
        if which in ("all", "striped"):
            blocks.append(striped_block("chromium"))
            blocks.append(striped_block("firefox"))
            blocks.append(striped_block("webkit"))
        if which in ("all", "chromium-wr"):
            blocks.append(engine_block("chromium", "webrtc"))
        if which in ("all", "firefox-wr"):
            reason = (f"firefox webrtc: no OpenH264 GMP plugin in {FF_E2E_PROFILE}; "
                      "run tests/tools/fetch-openh264.sh to cover H.264 in Firefox")
            if openh264_version():
                blocks.append(engine_block("firefox", "webrtc"))
            elif which == "firefox-wr":
                H.skip_suite(reason)
            else:
                print(f"SKIP {reason}", flush=True)
    except Exception as e:
        print("FATAL block error:", e, flush=True)
        raise
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== BROWSERS: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
