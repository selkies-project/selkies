#!/usr/bin/env python3
"""Browser-engine matrix: Chromium (Chrome binary), Firefox, WebKit over the
websockets transport (flow, input, clipboard, resize, console health), plus a
reduced WebRTC flow on Firefox (parity with the Chrome reference) and WebKit."""
import os
import platform
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

from typing import Optional

DECODER_ERROR_PATTERNS = (
    "Failed to load resource", "Unexpected server response:", "ResizeObserver",
    "Error getting media devices", "AudioContext was not allowed",
    "FATAL DECODER ERROR", "vp8_codec_error_msg", "av_send_packet_error",
)


# Persistent profile: Firefox only grants clipboard access to a profile that
# carries the permission, which a fresh temporary profile never does.
FF_E2E_PROFILE: str = os.environ.get(
    "E2E_FIREFOX_PROFILE", os.path.join(H.WORKDIR, "firefox-profile"))


def openh264_version() -> Optional[str]:
    """Version of the OpenH264 GMP side-loaded by tests/tools/fetch-openh264.sh,
    or None. Without the plugin Firefox answers the offer with the video m-line
    rejected, so every video, input and clipboard check in the WebRTC block
    reports the same one absence."""
    root = os.path.join(FF_E2E_PROFILE, "gmp-gmpopenh264")
    if not os.path.isdir(root):
        return None
    for version in sorted(os.listdir(root), reverse=True):
        if os.path.isfile(os.path.join(root, version, "libgmpopenh264.so")):
            return version
    return None


def openh264_prefs() -> dict:
    """Prefs that point Firefox at the side-loaded plugin. Firefox only scans
    `gmp-gmpopenh264/<version>` once a pref names that exact version, and the GMP
    updater is turned off so it does not replace it mid-run."""
    version = openh264_version()
    if not version:
        return {}
    abi = "aarch64-gcc3" if platform.machine() in ("aarch64", "arm64") else "x86_64-gcc3"
    return {
        "media.gmp-gmpopenh264.version": version,
        "media.gmp-gmpopenh264.abi": abi,
        "media.gmp-gmpopenh264.lastUpdate": 1,
        "media.gmp-manager.updateEnabled": False,
    }


def engine_launch(p, engine: str):
    """Return (browser_or_none, ctx). Firefox needs a persistent profile that
    carries the OpenH264 GMP plugin (bundled profile ships without it, so the
    WebRTC answer rejects the video m-line) plus autoplay/clipboard prefs."""
    if engine == "chromium":
        b = C.chromium_launch(p)
        return b, b.new_context(viewport={"width": 1280, "height": 720, "deviceScaleFactor": 1})
    if engine == "firefox":
        ctx = p.firefox.launch_persistent_context(
            user_data_dir=FF_E2E_PROFILE, headless=True,
            viewport={"width": 1280, "height": 720, "deviceScaleFactor": 1},
            firefox_user_prefs={
                "media.gmp-gmpopenh264.enabled": True,
                "media.autoplay.default": 0,
                "media.autoplay.blocking_policy": 0,
                "media.autoplay.block-webaudio": False,
                "dom.events.testing.asyncClipboard": True,
                **openh264_prefs(),
            })
        return None, ctx
    b = getattr(p, engine).launch(headless=True)
    return b, b.new_context(viewport={"width": 1280, "height": 720, "deviceScaleFactor": 1})


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
            pressed = False
            for _ in range(4):
                # Headless WebKit drops synthetic keydowns under load, so the whole
                # press is retried rather than waited on.
                page.keyboard.down("x")
                time.sleep(0.8)
                pressed = C.x11_keymap_pressed("x")
                if pressed is True:
                    break
                page.keyboard.up("x")
                time.sleep(0.6)
            res.check("input: key held in X keymap", pressed is True, pressed)
            page.keyboard.up("x")
            time.sleep(0.3)
            res.check("input: key released in X keymap",
                      C.x11_keymap_pressed("x") is False, "")

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
    """The striped encoder on one engine: the video worker decodes, composites
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
        # decode failure flips between them while the stream is coming up.
        deadline = time.time() + 30
        while time.time() < deadline:
            time.sleep(1.0)
            if page.evaluate("() => (window.videoChunksReceived || 0) > 0"):
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
