#!/usr/bin/env python3
"""The software-decode retry that sits ahead of the client's fallback ladder.

A hardware H.264 decoder can accept its config and then fail at decode() —
isConfigSupported does not predict it — so the client retries the same encoder
with hardwareAcceleration 'prefer-software' before the ladder closes the socket,
degrades the encoder and reloads. These blocks inject exactly that failure into
a real browser and check each arm of the decision.

The ``nowebcodecs`` block is the rung below the ladder: an engine with no WebCodecs
at all is not refused but pinned to the striped-JPEG encoder at pre-flight, and
the dashboard offers it nothing else.

Usage: python3 tests/e2e/test_software_decode.py [retry|persisted|ladder|healthy|striped|nowebcodecs|all]
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C

from typing import Callable, Optional

# localStorage namespace the client derives from origin + pathname.
STORAGE_KEY_JS = (
    "((location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_')"
    " + '_prefer_software_decode')"
)


def shim_js(fail_mode: str) -> str:
    """Make every VideoDecoder in the page report an asynchronous decode error,
    either only on the default (hardware) path or on every path, and record the
    acceleration each decoder was configured with."""
    return """
    (() => {
      const Real = window.VideoDecoder;
      if (!Real) return;
      const MODE = '%s';
      const errorFor = new WeakMap(), softFor = new WeakMap();
      const record = (accel) => {
        const seen = JSON.parse(sessionStorage.getItem('__cfgs') || '[]');
        seen.push(accel);
        sessionStorage.setItem('__cfgs', JSON.stringify(seen));
      };
      const proto = Real.prototype;
      const origConfigure = proto.configure, origDecode = proto.decode;
      proto.configure = function (cfg) {
        const accel = (cfg && cfg.hardwareAcceleration) || 'default';
        softFor.set(this, accel === 'prefer-software');
        record(accel);
        return origConfigure.call(this, cfg);
      };
      proto.decode = function (chunk) {
        const soft = softFor.get(this) === true;
        if (MODE === 'all' || (MODE === 'hardware' && !soft)) {
          const cb = errorFor.get(this);
          if (cb) setTimeout(() => cb(new DOMException('injected decode failure',
                                                       'EncodingError')), 0);
          return;
        }
        return origDecode.call(this, chunk);
      };
      window.VideoDecoder = new Proxy(Real, {
        construct(target, args) {
          const init = Object.assign({}, (args && args[0]) || {});
          const out = init.output;
          init.output = (frame) => {
            window.__decoded = (window.__decoded || 0) + 1;
            if (out) out(frame);
          };
          const inst = Reflect.construct(target, [init]);
          errorFor.set(inst, init.error);
          return inst;
        },
      });
    })();
    """ % fail_mode


# Every WebCodecs global removed, the way an engine without the API presents.
NO_WEBCODECS_JS = """
  (() => {
    for (const name of ['VideoDecoder', 'VideoEncoder', 'VideoFrame', 'EncodedVideoChunk', 'ImageDecoder']) {
      try { Object.defineProperty(window, name, { value: undefined, configurable: true, writable: true }); } catch (e) {}
    }
  })();
"""

NAV_JS = """
  (() => {
    const n = Number(sessionStorage.getItem('__navs') || 0) + 1;
    sessionStorage.setItem('__navs', String(n));
  })();
"""

SEED_JS = "try { localStorage.setItem(%s, navigator.userAgent); } catch (e) {}" % STORAGE_KEY_JS

ENCODER_JS = """
  try {
    const app = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
    localStorage.setItem(app + '_encoder', '%s');
  } catch (e) {}
"""


def open_client(pw, fail_mode: str = "none", seed_preference: bool = False,
                encoder: Optional[str] = None, query: str = ""):
    """Launch Chromium with the decode-failure shim installed.

    Args:
        pw: Active Playwright instance.
        fail_mode: ``none``, ``hardware``, or ``all`` decode failure injection.
        seed_preference: Pre-store the software-decode preference key.
        encoder: Optional encoder to pin in localStorage before load.
        query: Optional query string appended to the stream URL.

    Returns:
        Tuple of (browser, page) with the stream page loaded.
    """
    browser = C.chromium_launch(pw)
    ctx = browser.new_context(viewport={"width": 1280, "height": 720},
                              device_scale_factor=1)
    ctx.add_init_script(NAV_JS)
    if seed_preference:
        ctx.add_init_script(SEED_JS)
    if encoder:
        ctx.add_init_script(ENCODER_JS % encoder)
    ctx.add_init_script(shim_js(fail_mode))
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/" + (f"?{query}" if query else ""), wait_until="load")
    return browser, page


def read_state(page, retries: int = 6) -> dict:
    """Page state, retried across the reloads the ladder triggers."""
    js = """(() => ({
      navs: Number(sessionStorage.getItem('__navs') || 0),
      cfgs: JSON.parse(sessionStorage.getItem('__cfgs') || '[]'),
      decoded: window.__decoded || 0,
      stored: (() => { try { return localStorage.getItem(%s); } catch (e) { return null; } })(),
      ua: navigator.userAgent,
    }))()""" % STORAGE_KEY_JS
    for attempt in range(retries):
        try:
            return page.evaluate(js)
        except Exception:
            if attempt == retries - 1:
                raise
            time.sleep(1)


def wait_for(page, predicate: Callable[[dict], bool], timeout: float = 25) -> Optional[dict]:
    """Poll the page state until the predicate holds; return the last state."""
    deadline = time.time() + timeout
    state = None
    while time.time() < deadline:
        state = read_state(page)
        if predicate(state):
            return state
        time.sleep(1)
    return state


def block_retry(r: "H.Results") -> None:
    """Hardware decode fails: the client switches to software in place, keeps
    decoding, and does not reload."""
    from playwright.sync_api import sync_playwright
    H.server_start(mode="websockets")
    try:
        with sync_playwright() as pw:
            browser, page = open_client(pw, fail_mode="hardware")
            try:
                state = wait_for(page, lambda s: s["decoded"] > 0
                                 and "prefer-software" in s["cfgs"])
                r.check("switched to software decode",
                        "prefer-software" in state["cfgs"], state["cfgs"][:6])
                r.check("frames decode after the switch", state["decoded"] > 0,
                        state["decoded"])
                r.check("preference persisted for this browser build",
                        state["stored"] == state["ua"], (state["stored"] or "")[:40])
                # The ladder reloads 3s after a fatal error; outlast it.
                time.sleep(6)
                after = read_state(page)
                r.check("page never reloaded", after["navs"] == 1, after["navs"])
                r.check("still decoding", after["decoded"] > state["decoded"],
                        (state["decoded"], after["decoded"]))
            finally:
                browser.close()
    finally:
        H.server_stop()


def block_persisted(r: "H.Results") -> None:
    """A remembered preference is applied to the first decoder, so a client with
    a broken hardware path pays no failed decode at all."""
    from playwright.sync_api import sync_playwright
    H.server_start(mode="websockets")
    try:
        with sync_playwright() as pw:
            browser, page = open_client(pw, fail_mode="hardware", seed_preference=True)
            try:
                state = wait_for(page, lambda s: s["decoded"] > 0)
                first = state["cfgs"][0] if state["cfgs"] else None
                r.check("first decoder is already software",
                        first == "prefer-software", state["cfgs"][:6])
                r.check("no hardware decoder was ever configured",
                        "default" not in state["cfgs"], state["cfgs"][:6])
                r.check("frames decode from the start", state["decoded"] > 0,
                        state["decoded"])
                time.sleep(6)
                after = read_state(page)
                r.check("page never reloaded", after["navs"] == 1, after["navs"])
            finally:
                browser.close()
    finally:
        H.server_stop()


def block_ladder(r: "H.Results") -> None:
    """Software decode fails too: the ladder still runs, and the preference is
    dropped so the next load re-probes hardware."""
    from playwright.sync_api import sync_playwright
    H.server_start(mode="websockets")
    try:
        with sync_playwright() as pw:
            browser, page = open_client(pw, fail_mode="all")
            try:
                # The reload is only half the event: wait for the decoder the
                # reloaded page configures, or a busy machine is read between the
                # two and the ladder looks like it stopped at software.
                state = wait_for(
                    page,
                    lambda s: s["navs"] > 1 and "prefer-software" in s["cfgs"]
                    and s["cfgs"][s["cfgs"].index("prefer-software") + 1:],
                    timeout=60)
                r.check("software was tried first",
                        "prefer-software" in state["cfgs"], state["cfgs"][:6])
                r.check("fallback ladder still reached", state["navs"] > 1,
                        state["navs"])
                # The preference is dropped on the way into the ladder, so the reloaded
                # page re-probes hardware rather than being pinned by a failure software
                # did not cause; it then arms its own retry, which sets the key again.
                after_software = state["cfgs"][state["cfgs"].index("prefer-software") + 1:]
                r.check("next load re-probes hardware",
                        bool(after_software) and after_software[0] == "default",
                        state["cfgs"][:6])
            finally:
                browser.close()
    finally:
        H.server_stop()


def block_healthy(r: "H.Results") -> None:
    """Nothing is injected: the retry must be invisible — no software decoder and
    no stored preference on a client whose hardware decoder works."""
    from playwright.sync_api import sync_playwright
    H.server_start(mode="websockets")
    try:
        with sync_playwright() as pw:
            browser, page = open_client(pw, fail_mode="none")
            try:
                state = wait_for(page, lambda s: s["decoded"] > 0)
                r.check("frames decode", state["decoded"] > 0, state["decoded"])
                r.check("no software decoder configured",
                        "prefer-software" not in state["cfgs"], state["cfgs"][:6])
                r.check("nothing persisted", state["stored"] is None, state["stored"])
                time.sleep(6)
                after = read_state(page)
                r.check("page never reloaded", after["navs"] == 1, after["navs"])
            finally:
                browser.close()
    finally:
        H.server_stop()


def block_striped(r: "H.Results") -> None:
    """The striped encoder runs a decoder per stripe and they fail together, so
    the errors still in flight when the switch happens must not reach the
    ladder.

    The failure shim only reaches page-scope decoders, so the video worker is
    disabled with its own escape hatch and the page striped ladder — still the
    fallback whenever the worker cannot divert — is what is exercised."""
    from playwright.sync_api import sync_playwright
    H.server_start(mode="websockets")
    try:
        with sync_playwright() as pw:
            browser, page = open_client(pw, fail_mode="hardware",
                                        encoder="h264enc-striped",
                                        query="offscreen_worker=false")
            try:
                state = wait_for(page, lambda s: s["decoded"] > 0
                                 and "prefer-software" in s["cfgs"], timeout=40)
                r.check("more than one decoder was in play",
                        len(state["cfgs"]) > 2, len(state["cfgs"]))
                r.check("switched to software decode",
                        "prefer-software" in state["cfgs"], state["cfgs"][:4])
                r.check("stripes decode after the switch", state["decoded"] > 0,
                        state["decoded"])
                time.sleep(6)
                after = read_state(page)
                r.check("page never reloaded", after["navs"] == 1, after["navs"])
                r.check("still decoding", after["decoded"] > state["decoded"],
                        (state["decoded"], after["decoded"]))
            finally:
                browser.close()
    finally:
        H.server_stop()


def block_nowebcodecs(r: "H.Results") -> None:
    """No WebCodecs at all: the stream comes up as striped JPEG without a reload,
    the encoder is pinned to jpeg for the session, and the classic dashboard's
    encoder menu offers only what this engine can play."""
    from playwright.sync_api import sync_playwright
    import test_dashboards as TD
    H.server_start(mode="websockets", web_root=H.CLASSIC_DIST)
    try:
        with sync_playwright() as pw:
            browser = C.chromium_launch(pw)
            ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
            ctx.add_init_script(NAV_JS)
            ctx.add_init_script(NO_WEBCODECS_JS)
            page = ctx.new_page()
            errors = []
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.goto(H.BASE_URL + "/", wait_until="load")
            try:
                video = C.wait_ws_video(page, timeout=30)
                r.check("stream comes up without WebCodecs", bool(video), str(video))
                r.check("VideoDecoder really is absent", page.evaluate("typeof window.VideoDecoder") == "undefined")
                stored = page.evaluate("localStorage.getItem((location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_') + '_encoder')")
                r.check("encoder pinned to jpeg", stored == "jpeg", stored)
                fps = C.page_fps(page, timeout=15)
                r.check("striped JPEG frames are painted", fps > 0, fps)
                status = page.evaluate("(() => { const el = document.querySelector('#status-display, .status-display, #status'); return el ? (el.classList.contains('hidden') ? '' : el.textContent) : ''; })()")
                r.check("no fatal WebCodecs message", "WebCodecs" not in (status or ""), status)
                navs = page.evaluate("Number(sessionStorage.getItem('__navs') || 0)")
                r.check("page never reloaded", navs == 1, navs)
                opened = TD.classic_open_video(page)
                options = page.evaluate("Array.from(document.querySelectorAll('#encoderSelect option')).map(o => o.value)") if opened else []
                r.check("dashboard offers only the jpeg encoder", opened and options == ["jpeg"], str(options))
                r.check("no page errors", not errors, "; ".join(errors)[:200])
            finally:
                browser.close()
    finally:
        H.server_stop()


BLOCKS = {"retry": block_retry, "persisted": block_persisted,
          "ladder": block_ladder, "healthy": block_healthy,
          "striped": block_striped, "nowebcodecs": block_nowebcodecs}

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    names = list(BLOCKS) if which == "all" else [which]
    ok = True
    for n in names:
        print(f"=== {n} ===", flush=True)
        res = H.Results(n)
        try:
            BLOCKS[n](res)
        except Exception as e:
            res.check("block completed", False, e)
        ok = res.summary() and ok
    sys.exit(0 if ok else 1)
