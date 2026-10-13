#!/usr/bin/env python3
"""A page riding over a cut of its session socket, end to end.

The page reaches the server through a relay that can cut every connection at
once, with no close frame either way: what an ingress does when it retires the
process holding them (NRP's HAProxy cuts each socket at its `hard-stop-after`,
ten minutes after a reload). The page reconnects in place at once, and a
quick reconnect is one nobody notices: the picture stalls for no longer than
the new socket takes, no notice covers it, playback keeps its context and
runs on, the server restarts nothing of its audio capture, and keys typed in
the gap reach the desktop in order. Keys and a button held across the cut stay
held, the microphone stays on, and video and audio the user switched off stay
off. A cut the server does not see (the page's side alone, a half-open
connection) is ridden over the same way. A reconnect that cannot be quick (the
relay refuses connections for a while) shows the notice once it has waited,
and the stream comes back after.

Usage: python3 tests/e2e/test_ws_cut.py [chromium|firefox|all]
(`firefox` runs the typing cut alone: Gecko routes the worker socket through
the page's thread.)
"""
import asyncio
import os
import subprocess
import sys
import threading
import time
from typing import Any, List

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402

WIDTH, HEIGHT = 1280, 720
PAINTER = os.path.join(H.TOOLS, "motion_scene.py")
RELAY_PORT = int(os.environ.get("E2E_CUT_PORT", "18214"))
# How long after a cut the page is watched.
WINDOW_MS = 3000.0
# A drop counts as a reconnect in place only for a socket that held this long
# (the client's RECONNECT_STABLE_MS), so cuts are spaced past it.
STABLE_S = 11.0
# The longest the picture may stall and playback stay silent through a quick
# reconnect, by engine: Gecko holds a socket to a host whose last one failed
# for 200 to 400 ms first (network.websocket.delay-failed-reconnects).
QUICK_MS = {"chromium": (250, 120), "firefox": (550, 400)}


class Pair:
    """One relayed connection: the page's side and the server's."""

    def __init__(self, client: asyncio.StreamWriter, upstream: asyncio.StreamWriter) -> None:
        self.client, self.upstream = client, upstream
        self.half_open = False


class CutRelay:
    """TCP from RELAY_PORT to the server whose open connections `cut()`
    aborts at once, so each side sees its stream end with no close frame."""

    def __init__(self, listen: int, target: int) -> None:
        self.listen, self.target = listen, target
        self.loop = asyncio.new_event_loop()
        self.pairs: set = set()
        self.refuse_until = 0.0
        self.ready = threading.Event()
        threading.Thread(target=self._run, daemon=True).start()
        self.ready.wait(10)

    def _run(self) -> None:
        asyncio.set_event_loop(self.loop)
        self.loop.run_until_complete(asyncio.start_server(self._handle, "127.0.0.1", self.listen))
        self.ready.set()
        self.loop.run_forever()

    async def _handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        if time.monotonic() < self.refuse_until:
            writer.transport.abort()
            return
        try:
            up_reader, up_writer = await asyncio.open_connection("127.0.0.1", self.target)
        except OSError:
            writer.transport.abort()
            return
        pair = Pair(writer, up_writer)
        self.pairs.add(pair)

        async def pump(src: asyncio.StreamReader, dst: asyncio.StreamWriter) -> None:
            try:
                while True:
                    data = await src.read(65536)
                    if not data:
                        break
                    dst.write(data)
                    await dst.drain()
            except (OSError, asyncio.CancelledError):
                pass
            finally:
                # A half-open cut leaves the server's side open and unread.
                if not (pair.half_open and dst is up_writer):
                    dst.transport.abort()

        await asyncio.gather(pump(reader, up_writer), pump(up_reader, writer))
        self.pairs.discard(pair)

    def cut(self, server_side: bool = True, refuse_s: float = 0.0) -> int:
        """Abort every open connection, the server's side too unless
        `server_side` is false, and refuse new ones for `refuse_s`; returns
        how many connections there were."""
        done = threading.Event()
        count = [0]

        def _cut() -> None:
            self.refuse_until = time.monotonic() + refuse_s
            count[0] = len(self.pairs)
            for pair in list(self.pairs):
                pair.half_open = not server_side
                pair.client.transport.abort()
                if server_side:
                    pair.upstream.transport.abort()
            done.set()

        self.loop.call_soon_threadsafe(_cut)
        done.wait(5)
        return count[0]


# Timestamps (performance.now) of every change of the stream's picture with
# its mean luma, of the status notice showing or hiding, the playback contexts
# made, the playback level every 50 ms of each playback worklet the page
# builds, and the microphone button's states.
HOOKS = """
window.__cut = {frames: [], overlay: [], acs: 0, levels: [], mic: []};
// Absent from a realm that is not a page (Gecko runs the script in more of them).
if (window.AudioContext && window.AudioWorkletNode) {
  const AC = window.AudioContext;
  window.AudioContext = class extends AC {
    constructor(...a) { super(...a); window.__cut.acs++; }
  };
  const AWN = window.AudioWorkletNode;
  window.AudioWorkletNode = class extends AWN {
    constructor(...a) {
      super(...a);
      if (a[1] !== 'audio-frame-processor') return;
      const port = this.port;
      port.addEventListener('message', (e) => {
        if (e.data && e.data.type === 'audioBufferSize') window.__cut.levels.push([performance.now(), e.data.level]);
      });
      setInterval(() => { try { port.postMessage({type: 'getBufferSize'}); } catch (e) {} }, 50);
    }
  };
}
window.addEventListener('message', (e) => {
  const d = e.data;
  if (d && d.type === 'sidebarButtonStatusUpdate' && d.microphone !== undefined) window.__cut.mic.push(d.microphone);
});
(() => {
  const probe = document.createElement('canvas');
  probe.width = 32; probe.height = 18;
  let pctx = null, last = null, shown = null;
  const tick = () => {
    // The stream shows on a <video> (a track generator) or on a canvas, whichever is displayed.
    const c = [...document.querySelectorAll('video, canvas')].find((x) =>
      (x.videoWidth || x.width) >= 640 && getComputedStyle(x).display !== 'none');
    if (c) {
      try {
        pctx = pctx || probe.getContext('2d', {willReadFrequently: true});
        pctx.drawImage(c, 0, 0, 32, 18);
        const d = pctx.getImageData(0, 0, 32, 18).data;
        let h = 0, sum = 0;
        for (let i = 0; i < d.length; i += 4) { h = (h * 31 + d[i] + d[i + 1] * 7 + d[i + 2] * 13) | 0; sum += d[i + 1]; }
        if (h !== last) { last = h; window.__cut.frames.push([performance.now(), Math.round(sum / (d.length / 4))]); }
      } catch (e) { /* not drawable yet */ }
    }
    const s = document.getElementById('status-display');
    const now = !!s && !s.classList.contains('hidden') && getComputedStyle(s).display !== 'none'
      && getComputedStyle(s).visibility !== 'hidden' && !!s.textContent;
    if (now !== shown) { shown = now; window.__cut.overlay.push([performance.now(), now, s ? s.textContent : '']); }
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
})();
"""


def longest_stall(frames: List[list], after: float) -> float:
    """The longest the picture stood still from just before `after` to
    WINDOW_MS past it, in ms."""
    times = [f[0] for f in frames if after - 200 <= f[0] <= after + WINDOW_MS]
    return max((b - a for a, b in zip(times, times[1:])), default=WINDOW_MS)


def longest_silence(levels: List[list], after: float, floor: float = 0.01) -> float:
    """The longest playback stayed silent within WINDOW_MS of `after`, in ms."""
    longest = 0.0
    quiet_since = None
    for t, level in levels:
        if not after <= t <= after + WINDOW_MS:
            continue
        if level < floor:
            quiet_since = t if quiet_since is None else quiet_since
            longest = max(longest, t - quiet_since)
        else:
            quiet_since = None
    return longest


def log_offset() -> int:
    with open(H.LOG, errors="replace") as f:
        f.seek(0, 2)
        return f.tell()


def log_since(offset: int) -> str:
    with open(H.LOG, errors="replace") as f:
        f.seek(offset)
        return f.read()


def keycodes(chars: str) -> dict:
    from selkies.Xlib import XK
    d = H.x_display()
    try:
        return {d.keysym_to_keycode(XK.string_to_keysym(ch)): ch for ch in chars}
    finally:
        d.close()


def mic_playbacks() -> List[str]:
    """pcmflux's playback streams on the sound server (the microphone's), by index."""
    listing = subprocess.run(["pactl", "list", "sink-inputs"], capture_output=True, text=True,
                             timeout=10).stdout
    return [block.split("\n", 1)[0].strip() for block in listing.split("Sink Input #")[1:]
            if 'application.name = "pcmflux"' in block]


def page_now(page: Any) -> float:
    return page.evaluate("performance.now()")


def wait_until(page: Any, at: float) -> None:
    """Sleep until the page's clock reads `at`."""
    time.sleep(max(0.0, (at - page_now(page)) / 1000))


def post(page: Any, message: str) -> None:
    page.evaluate(f"window.postMessage({message}, window.location.origin)")


def typing_cut(res: "H.Results", page: Any, relay: CutRelay, label: str, engine: str,
               server_side: bool = True) -> None:
    """Type through a cut: two keys before it, four in its first 0.7 s, two
    after the stream is back; every one arrives, in order."""
    d, events, stop = H.x_key_watcher()
    page.mouse.move(WIDTH // 2, HEIGHT // 2)
    page.mouse.click(WIDTH // 2, HEIGHT // 2)
    time.sleep(1)
    codes = keycodes("abcdefgh")
    page.keyboard.press("a")
    page.keyboard.press("b")
    time.sleep(0.5)
    offset = log_offset()
    contexts = page.evaluate("window.__cut.acs")
    cut_at = page_now(page)
    res.check(f"{label}: the relay cut the page's connections", relay.cut(server_side=server_side) > 0)
    for delay, ch in ((50, "c"), (200, "d"), (450, "e"), (700, "f")):
        wait_until(page, cut_at + delay)
        page.keyboard.press(ch)
    time.sleep(4)
    page.keyboard.press("g")
    page.keyboard.press("h")
    time.sleep(1)
    stop["flag"] = True
    state = page.evaluate("window.__cut")
    log = log_since(offset)
    arrived = "".join(codes[code] for name, code in events if name == "KeyPress" and code in codes)
    res.check(f"{label}: every key typed before, through, and after the cut reaches the desktop in order",
              arrived == "abcdefgh", arrived)
    stall_ms, silence_ms = QUICK_MS[engine]
    stall = longest_stall(state["frames"], cut_at)
    res.check(f"{label}: the picture stalls no longer than the new socket takes", stall < stall_ms,
              f"{stall:.0f} ms")
    shown = [o for o in state["overlay"] if o[0] > cut_at and o[1]]
    res.check(f"{label}: no notice covers a quick reconnect", not shown, shown[:2])
    res.check(f"{label}: playback keeps its context", state["acs"] == contexts, (contexts, state["acs"]))
    silence = longest_silence(state["levels"], cut_at)
    res.check(f"{label}: playback runs on through the cut", silence < silence_ms, f"{silence:.0f} ms silent")
    res.check(f"{label}: the server restarts nothing of its audio capture",
              "RED gate changed" not in log and "Stopping pcmflux" not in log,
              [ln for ln in log.splitlines() if "RED gate" in ln or "Stopping pcmflux" in ln][:2])
    if server_side:
        res.check(f"{label}: the server sees a cut, and the page reclaims its display in the grace",
                  "code 1006" in log and "claimed by a new connection" in log, "")


def held_cut(res: "H.Results", page: Any, relay: CutRelay) -> None:
    """A key and a mouse button held across the cut stay held, and let go
    when the user lets go."""
    page.mouse.move(WIDTH // 2, HEIGHT // 2)
    page.keyboard.down("Shift")
    page.mouse.down()
    time.sleep(0.6)
    before = (C.x11_keymap_pressed("Shift_L"), C.x11_buttons_held())
    relay.cut()
    time.sleep(1.5)
    during = (C.x11_keymap_pressed("Shift_L"), C.x11_buttons_held())
    page.mouse.up()
    page.keyboard.up("Shift")
    time.sleep(0.6)
    after = (C.x11_keymap_pressed("Shift_L"), C.x11_buttons_held())
    res.check("held: a key and a button held before the cut", before == (True, (1,)), before)
    res.check("held: both are still held once the page is back", during == (True, (1,)), during)
    res.check("held: both let go when the user lets go", after == (False, ()), after)


def toggles_cut(res: "H.Results", page: Any, relay: CutRelay) -> None:
    """The microphone the user switched on stays on, and video and audio the
    user switched off stay off."""
    post(page, "{type: 'pipelineControl', pipeline: 'microphone', enabled: true}")
    deadline = time.time() + 15
    while time.time() < deadline and not mic_playbacks():
        time.sleep(0.25)
    first = mic_playbacks()
    res.check("toggles: the microphone reaches the server", bool(first), first)
    post(page, "{type: 'pipelineControl', pipeline: 'video', enabled: false}")
    post(page, "{type: 'pipelineControl', pipeline: 'audio', enabled: false}")
    time.sleep(STABLE_S)
    cut_at = page_now(page)
    relay.cut()
    deadline = time.time() + 15
    while time.time() < deadline and not (set(mic_playbacks()) - set(first)):
        time.sleep(0.25)
    res.check("toggles: the microphone is on again on the new socket",
              bool(set(mic_playbacks()) - set(first)), (first, mic_playbacks()))
    mic = page.evaluate("window.__cut.mic")
    res.check("toggles: and the page shows it on", mic[-1:] == [True], mic[-3:])
    time.sleep(3)
    state = page.evaluate("window.__cut")
    moved = [f for f in state["frames"] if f[0] > cut_at + 1000]
    res.check("toggles: video the user stopped stays stopped", not moved, f"{len(moved)} picture changes")
    loud = [lv for t, lv in state["levels"] if t > cut_at + 1000 and lv >= 0.01]
    res.check("toggles: audio the user stopped stays stopped", not loud, f"{len(loud)} audible samples")
    post(page, "{type: 'pipelineControl', pipeline: 'microphone', enabled: false}")
    post(page, "{type: 'pipelineControl', pipeline: 'video', enabled: true}")
    post(page, "{type: 'pipelineControl', pipeline: 'audio', enabled: true}")
    time.sleep(3)
    now = page_now(page)
    moving = [f for f in page.evaluate("window.__cut.frames") if f[0] > now - 2000]
    res.check("toggles: video comes back when the user starts it", len(moving) > 30, len(moving))


def slow_cut(res: "H.Results", page: Any, relay: CutRelay) -> None:
    """A reconnect that cannot be quick shows the notice once it has waited,
    and the stream comes back."""
    cut_at = page_now(page)
    relay.cut(refuse_s=3.0)
    time.sleep(8)
    state = page.evaluate("window.__cut")
    shown = [o[0] - cut_at for o in state["overlay"] if o[0] > cut_at and o[1]]
    res.check("slow: the notice shows once the reconnect has waited", bool(shown) and shown[0] >= 1400,
              [round(s) for s in shown[:2]])
    hidden = [o[0] - cut_at for o in state["overlay"] if o[0] > cut_at and not o[1]]
    res.check("slow: and goes once the stream is back", bool(shown) and bool(hidden) and hidden[-1] > shown[0],
              [round(h) for h in hidden[-2:]])
    moving = [f for f in state["frames"] if f[0] > cut_at + 4000]
    res.check("slow: the picture moves again", len(moving) > 30, len(moving))
    loud = [lv for t, lv in state["levels"] if t > cut_at + 6000 and lv >= 0.01]
    res.check("slow: and sound plays again", len(loud) > 10, len(loud))


def launch(p: Any, engine: str) -> tuple:
    if engine == "chromium":
        # The fake device plays a tone into getUserMedia; the headless shell has no media capture.
        kw = {"headless": True, "args": C.BROWSER_ARGS + ["--use-fake-device-for-media-stream"]}
        if C.CHROME_PATH:
            kw["executable_path"] = C.CHROME_PATH
        else:
            kw["channel"] = "chromium"
        browser = p.chromium.launch(**kw)
        ctx = browser.new_context(viewport={"width": WIDTH, "height": HEIGHT}, permissions=["microphone"])
    else:
        browser = C.launch_browser(p, engine)
        ctx = browser.new_context(viewport={"width": WIDTH, "height": HEIGHT})
    ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
    ctx.add_init_script(HOOKS)
    return browser, ctx


def session(engine: str) -> "H.Results":
    from playwright.sync_api import sync_playwright

    res = H.Results(f"ws-cut-{engine}")
    painter = tone = None
    relay = CutRelay(RELAY_PORT, H.PORT)
    H.server_start(mode="websockets", wayland=False, web_root=H.CLASSIC_DIST)
    try:
        painter = H.spawn([sys.executable, PAINTER, H.TEST_DISPLAY, str(WIDTH), str(HEIGHT), "60"])
        tone = H.pulse_sine()
        with sync_playwright() as p:
            browser, ctx = launch(p, engine)
            page = ctx.new_page()
            errors: List[str] = []
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.goto(f"http://127.0.0.1:{RELAY_PORT}", wait_until="load", timeout=90000)
            res.check("the page streams through the relay", C.wait_ws_video(page, timeout=60) is not None)
            page.mouse.click(WIDTH // 2, HEIGHT // 2)
            time.sleep(STABLE_S)
            typing_cut(res, page, relay, "cut", engine)
            if engine == "chromium":
                time.sleep(STABLE_S)
                typing_cut(res, page, relay, "half-open", engine, server_side=False)
                time.sleep(STABLE_S)
                held_cut(res, page, relay)
                time.sleep(STABLE_S)
                toggles_cut(res, page, relay)
                time.sleep(STABLE_S)
                slow_cut(res, page, relay)
            res.check("no page errors", not errors, "; ".join(errors)[:300])
            C.close_browser(browser)
    finally:
        if painter is not None:
            painter.kill()
        H.pulse_unload(tone)
        H.server_stop()
    res.summary()
    return res


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = []
    for engine in ("chromium", "firefox"):
        if which in ("all", engine):
            blocks.append(session(engine))
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== WS CUT: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
