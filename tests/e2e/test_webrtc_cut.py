#!/usr/bin/env python3
"""A WebRTC page riding over a cut of its signaling socket, end to end.

The page reaches the server through a relay that can cut every connection at
once, with no close frame either way, as an ingress retiring its connections
does; the media path (ICE, straight to the server) never passes it. The
session carries on: the picture and sound run on through the cut, the page
keeps its peer connection and the server its media session (no session ends,
none starts), no notice shows, and keys typed in the gap reach the desktop in
order. A cut the server does not see (the page's side alone, a half-open
connection) resumes the same way. A relay refusing the page past the server's
hold ends the session, and the page starts a new one.

Usage: python3 tests/e2e/test_webrtc_cut.py [chromium|firefox|all]
"""
import os
import sys
import time
from typing import Any, List

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
import test_ws_cut as ws  # noqa: E402
from selkies.webrtc_signaling_server import SESSION_HOLD_S  # noqa: E402

WIDTH, HEIGHT = ws.WIDTH, ws.HEIGHT
RELAY_PORT = int(os.environ.get("E2E_CUT_PORT", "18215"))
# Cuts are spaced so each meets a settled session.
SETTLE_S = 5.0
# The longest the picture may stand still through a resumed cut: the scene paints at 60 fps.
STALL_MS = 200

# The sound the page's live peer connections received.
AUDIO_JS = """async () => {
  const out = {packets: 0, samples: 0, concealed: 0};
  for (const pc of window.__pcs || []) {
    if (pc.connectionState === 'closed') continue;
    (await pc.getStats()).forEach((r) => {
      if (r.type !== 'inbound-rtp' || r.kind !== 'audio') return;
      out.packets += r.packetsReceived || 0;
      out.samples += r.totalSamplesReceived || 0;
      out.concealed += r.concealedSamples || 0;
    });
  }
  return out;
}"""
PCS_JS = "(window.__pcs || []).map((pc) => pc.connectionState)"


def session_lines(log: str) -> List[str]:
    return [ln for ln in log.splitlines()
            if "resumed its session" in ln or "Session started for peer" in ln or "session ended" in ln
            or "is held" in ln]


def typing_cut(res: "H.Results", page: Any, relay: ws.CutRelay, label: str, server_side: bool = True) -> None:
    """Type through a cut: two keys before it, four in its first 0.7 s, two
    after; every one arrives, in order, and the stream runs on."""
    d, events, stop = H.x_key_watcher()
    page.mouse.click(WIDTH // 2, HEIGHT // 2)
    time.sleep(1)
    codes = ws.keycodes("abcdefgh")
    page.keyboard.press("a")
    page.keyboard.press("b")
    time.sleep(0.5)
    offset = ws.log_offset()
    pcs = page.evaluate(PCS_JS)
    audio = page.evaluate(AUDIO_JS)
    cut_at = ws.page_now(page)
    res.check(f"{label}: the relay cut the page's signaling socket", relay.cut(server_side=server_side) > 0)
    for delay, ch in ((50, "c"), (200, "d"), (450, "e"), (700, "f")):
        ws.wait_until(page, cut_at + delay)
        page.keyboard.press(ch)
    time.sleep(3.5)
    heard = {k: v - audio[k] for k, v in page.evaluate(AUDIO_JS).items()}
    page.keyboard.press("g")
    page.keyboard.press("h")
    time.sleep(1)
    stop["flag"] = True
    state = page.evaluate("window.__cut")
    log = ws.log_since(offset)
    arrived = "".join(codes[code] for name, code in events if name == "KeyPress" and code in codes)
    res.check(f"{label}: every key typed before, through, and after the cut reaches the desktop in order",
              arrived == "abcdefgh", arrived)
    stall = ws.longest_stall(state["frames"], cut_at)
    res.check(f"{label}: the picture runs on through the cut", stall < STALL_MS, f"{stall:.0f} ms")
    shown = [o for o in state["overlay"] if o[0] > cut_at and o[1]]
    res.check(f"{label}: no notice shows", not shown, shown[:2])
    res.check(f"{label}: sound plays on through the cut",
              heard["packets"] > 100 and heard["concealed"] < 0.05 * max(heard["samples"], 1), heard)
    res.check(f"{label}: the page keeps its peer connection", page.evaluate(PCS_JS) == pcs == pcs[:-1] + ["connected"],
              (pcs, page.evaluate(PCS_JS)))
    lines = session_lines(log)
    res.check(f"{label}: the server resumes the session: none ends, none starts",
              any("resumed its session" in ln for ln in lines)
              and not any("Session started" in ln or "session ended" in ln for ln in lines), lines[:4])
    if server_side:
        res.check(f"{label}: the server saw the cut and held the session for it",
                  any("is held" in ln for ln in lines), lines[:4])


def slow_cut(res: "H.Results", page: Any, relay: ws.CutRelay) -> None:
    """A cut the page cannot come back from within the hold ends the session,
    and the page starts a new one."""
    pcs = len(page.evaluate(PCS_JS))
    offset = ws.log_offset()
    cut_at = ws.page_now(page)
    relay.cut(refuse_s=SESSION_HOLD_S + 2)
    time.sleep(SESSION_HOLD_S + 10)
    state = page.evaluate("window.__cut")
    lines = session_lines(ws.log_since(offset))
    res.check("slow: a session nobody resumes within the hold ends",
              any("session ended" in ln for ln in lines) and not any("resumed" in ln for ln in lines), lines[:4])
    res.check("slow: the page starts a new one",
              any("Session started" in ln for ln in lines) and len(page.evaluate(PCS_JS)) == pcs + 1,
              (lines[-2:], page.evaluate(PCS_JS)))
    moving = [f for f in state["frames"] if f[0] > cut_at + (SESSION_HOLD_S + 6) * 1000]
    res.check("slow: the picture moves again", len(moving) > 30, len(moving))


def session(engine: str) -> "H.Results":
    from playwright.sync_api import sync_playwright

    res = H.Results(f"webrtc-cut-{engine}")
    if engine == "firefox" and not C.openh264_version():
        res.skip(f"no OpenH264 GMP plugin in {C.FF_E2E_PROFILE}",
                 "run tests/tools/fetch-openh264.sh to cover H.264 in Firefox")
        res.summary()
        return res
    painter = tone = None
    relay = ws.CutRelay(RELAY_PORT, H.PORT)
    H.server_start("webrtc", wayland=False, web_root=H.DASHBOARD_DIST,
                   extra_env={"SELKIES_STUN_HOST": "", "SELKIES_TURN_REST_URI": ""})
    try:
        painter = H.spawn([sys.executable, ws.PAINTER, H.TEST_DISPLAY, str(WIDTH), str(HEIGHT), "60"])
        tone = H.pulse_sine()
        with sync_playwright() as p:
            viewport = {"width": WIDTH, "height": HEIGHT}
            if engine == "firefox":
                browser = ctx = C.firefox_persistent_context(p, viewport=viewport, device_scale_factor=1)
            else:
                browser = C.launch_browser(p, "chromium")
                ctx = browser.new_context(viewport=viewport, device_scale_factor=1)
            ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'webrtc';")
            ctx.add_init_script(C.PC_TAP_JS)
            ctx.add_init_script(ws.HOOKS)
            page = ctx.new_page()
            errors: List[str] = []
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.goto(f"http://127.0.0.1:{RELAY_PORT}", wait_until="load", timeout=90000)
            res.check("the page streams with its signaling through the relay", bool(C.wait_wr_video(page, 60)))
            page.mouse.click(WIDTH // 2, HEIGHT // 2)
            time.sleep(SETTLE_S)
            typing_cut(res, page, relay, "cut")
            time.sleep(SETTLE_S)
            typing_cut(res, page, relay, "again")
            if engine == "chromium":
                time.sleep(SETTLE_S)
                typing_cut(res, page, relay, "half-open", server_side=False)
                time.sleep(SETTLE_S)
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
    print(f"\n=== WEBRTC CUT: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
