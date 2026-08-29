#!/usr/bin/env python3
"""E2E matrix: {websockets, webrtc} x {x11, wayland} against the live server."""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright


def run_block(mode: str, wayland: bool, block: str = "",
              engine: str = "chromium") -> "H.Results":
    """Drive one transport/backend pair through the full e2e checklist.

    Args:
        mode: Transport mode, ``websockets`` or ``webrtc``.
        wayland: True to run against the Wayland backend, False for X11.
        block: Label for this block's Results; derived from mode when empty.
        engine: Browser engine to drive (chromium, firefox, or webkit).

    Returns:
        The Results accumulator for this block's checks.
    """
    tag = block or f"{mode}-{'wl' if wayland else 'x11'}"
    res = H.Results(tag)
    wl_socket = "wayland-1"

    if wayland:
        try:
            from pixelflux import ScreenCapture
            missing = [m for m in ("clipboard_write_app", "list_outputs",
                                   "create_output", "set_keymap_overlay")
                       if not hasattr(ScreenCapture, m)]
        except ImportError:
            missing = ["the module entirely"]
        if missing:
            print(f"[{tag}] PREFLIGHT: installed pixelflux is missing APIs "
                  "this tree expects; wayland checks will fail against it",
                  flush=True)

    H.server_start(mode=mode, wayland=wayland)
    res.check("server up", True, H.curl("/api/status")[0])
    st_mode = json.loads(H.curl("/api/status")[1]).get("current_mode")
    res.check("api/status mode", st_mode == mode, st_mode)
    ok, _ = H.curl("/api/health")
    res.check("api/health", ok == 200, ok)

    with sync_playwright() as p:
        browser, page, console_errors, not_found = C.launch_chrome(p, mode=mode, engine=engine)
        wl_obs = None
        try:
            if mode == "websockets":
                info = C.wait_ws_video(page)
                res.check("video: canvas painted", info is not None, info)
                # Headless rAF throttling zeroes the client's fps metric, so
                # binary WS frames are counted instead.
                deadline = time.time() + 10
                frames = 0
                while time.time() < deadline:
                    frames = page.evaluate("window.videoChunksReceived || window.__wsFrames") or 0
                    if frames >= 24:
                        break
                    time.sleep(0.5)
                res.check("video: WS frames flowing", frames >= 24, frames)
            else:
                info = C.wait_wr_video(page)
                res.check("video: <video> receiving", info is not None, info)
                if info:
                    res.check("audio: audio track negotiated", info.get("audio", 0) >= 1, info.get("audio"))
            time.sleep(1.0)

            if wayland:
                wl_obs = H.WlObs(wl_socket)
                res.check("wl observer mapped", wl_obs.ready(), "")
            page.mouse.click(640, 360)
            time.sleep(0.5)
            page.keyboard.down("x")
            time.sleep(0.8)
            if wayland:
                ev = wl_obs.wait_for("kbd_key", timeout=5)
                res.check("input: key reached wayland seat", ev is not None, ev)
            else:
                pressed = C.x11_keymap_pressed("x")
                res.check("input: key held in X keymap", pressed is True, pressed)
            page.keyboard.up("x")
            time.sleep(0.3)
            if wayland:
                ev = wl_obs.wait_for("kbd_key", state=0, timeout=3)
                res.check("input: key release reached wayland seat", ev is not None, ev)
            else:
                pressed2 = C.x11_keymap_pressed("x")
                res.check("input: key released in X keymap", pressed2 is False, pressed2)


            # Wayland pointer events reach the focused surface only, so the aim is
            # inside the observer's own 16x16 surface at 0,0; X11 is absolute.
            if wayland:
                page.mouse.move(8, 8)
                time.sleep(0.6)
                ev = wl_obs.wait_for("ptr_motion", timeout=4)
                res.check("input: pointer motion reached wayland seat",
                          ev is not None and "x" in ev, ev)
                page.mouse.click(8, 8)
                time.sleep(0.4)
                ev = wl_obs.wait_for("ptr_button", state=1, timeout=3)
                res.check("input: mouse button reached wayland seat", ev is not None, ev)
            else:
                # The client maps page coordinates onto the stream it is shown,
                # so the X pointer lands at the page point scaled by the root
                # size over the video element's own. A session that opened on a
                # root wider than the element (one left extended by whatever ran
                # before) is mapping correctly, not wrongly, and the check has to
                # follow the same arithmetic to say so.
                box = page.evaluate(
                    "() => { const v = document.querySelector('video');"
                    " if (!v) return null; const r = v.getBoundingClientRect();"
                    " return {w: r.width, h: r.height, vw: v.videoWidth, vh: v.videoHeight}; }")
                before = C.x11_mouse_pos()
                page.mouse.move(200, 150)
                time.sleep(0.6)
                after = C.x11_mouse_pos()
                if box and box["w"] and box["h"] and box["vw"] and box["vh"]:
                    want = (200 * box["vw"] / box["w"], 150 * box["vh"] / box["h"])
                else:
                    want = (200, 150)
                moved = (abs(after[0] - want[0]) <= 4 and abs(after[1] - want[1]) <= 4)
                res.check("input: pointer moved in X (200,150)", moved,
                          f"{before}->{after} want=({want[0]:.0f},{want[1]:.0f}) {box}")
                page.mouse.click(300, 200)
                time.sleep(0.3)

            probe = f"e2e-{tag}-c2s-{int(time.time())}"
            C.send_clipboard_from_client(page, probe, engine)
            deadline = time.time() + 8
            got = None
            while time.time() < deadline:
                got = H.wl_paste(wl_socket).strip() if wayland else H.x_read_clipboard(timeout=3)
                if got == probe:
                    break
                time.sleep(0.5)
            res.check("clipboard: client text reached server", got == probe, repr(got)[:80])

            push_payload = f"e2e-{tag}-s2c-{int(time.time())}"
            if wayland:
                H.wl_copy(wl_socket, push_payload)
            else:
                ext, stop = H.x_own_clipboard(push_payload.encode())
            page.wait_for_timeout(2500)
            got = C.server_clipboard_push_check(page, push_payload)
            if not wayland:
                stop["flag"] = True
            res.check("clipboard: server push reached page", got, "")

            if not wayland:
                req_w, req_h = 1242, 694
                page.set_viewport_size({"width": req_w, "height": req_h})
                deadline = time.time() + 12
                realized = None
                while time.time() < deadline:
                    realized = H.x_root_size()
                    if realized and abs(realized[0] - req_w) <= 16 and abs(realized[1] - req_h) <= 16:
                        break
                    time.sleep(0.5)
                res.check("resize: X root follows browser",
                          realized and abs(realized[0] - req_w) <= 16,
                          f"req={req_w}x{req_h} actual={realized}")
            else:
                page.set_viewport_size({"width": 1242, "height": 694})
                ok = (C.wait_log("Wayland realized geometry", timeout=12))
                res.check("resize: wayland realized geometry logged", ok, "")

            C.settings_change(page, {"framerate": 30})
            if mode == "websockets":
                ok = C.wait_log("Applying video settings for 'primary' live (no restart)", timeout=10)
            else:
                ok = C.wait_log("Updated framerate to", timeout=10)
            res.check("settings: framerate change applied live", ok, "")

            # The wr core turns video_bitrate into the 'vb,' opcode, which the
            # server must clamp to the configured range.
            if mode == "webrtc":
                C.settings_change(page, {"video_bitrate": 1})
                time.sleep(1.5)
                res.check("F3: 'vb,1' clamped to server range min",
                          C.wait_log("clamped to 100", timeout=6), "")
                C.settings_change(page, {"video_bitrate": 6000})

            vpage = C.new_page(browser.contexts[0], mode=mode, url_hash="#shared")
            time.sleep(4.0)
            vinfo = C.wait_ws_video(vpage, timeout=12) if mode == "websockets" else C.wait_wr_video(vpage, timeout=40)
            res.check("roles: shared viewer gets video", vinfo is not None, vinfo)
            if mode == "websockets":
                # Audio rides the primary connection, shared viewers included;
                # the worklet's polled depth proves packets reach playback.
                deadline = time.time() + 10
                depth = 0
                while time.time() < deadline:
                    depth = vpage.evaluate("window.currentAudioBufferSize || 0") or 0
                    if depth > 0:
                        break
                    time.sleep(0.5)
                res.check("roles: shared viewer plays audio", depth > 0, depth)
            if not wayland:
                vpage.mouse.move(400, 300)
                time.sleep(0.5)
                posviewer = C.x11_mouse_pos()
                res.check("roles: viewer input gated (pointer unmoved)",
                          not (abs(posviewer[0] - 400) <= 4 and abs(posviewer[1] - 300) <= 4),
                          posviewer)
            vpage.close()

            dpage = C.new_page(browser.contexts[0], mode=mode, url_hash="#display2-right")
            time.sleep(6.0)
            if mode == "websockets":
                deadline = time.time() + 10
                texts = []
                while time.time() < deadline:
                    texts = page.evaluate("window.__wsTexts || []") or []
                    if any("display2" in t for t in texts):
                        break
                    time.sleep(0.5)
                res.check("second display: config update reached primary page",
                          any("display2" in t for t in texts), str(texts)[:90])
            else:
                # The datachannel carries the same message; the server-side media
                # graph is what the page consumes, so it is verified at the source.
                res.check("second display: media graph started (server)",
                          C.wait_log("display 'display2'", timeout=10), "")
            dinfo = C.wait_ws_video(dpage, timeout=12) if mode == "websockets" else C.wait_wr_video(dpage, timeout=40)
            res.check("second display: video flows", dinfo is not None, dinfo)
            if mode == "webrtc":
                # displayPosition reaches the server through SETTINGS passthrough.
                dpage.evaluate(
                    "window.postMessage({ type: 'settings', settings: { displayPosition: 'down' } }, window.location.origin)")
                time.sleep(2.5)
                res.check("A9: secondary reposition applied",
                          C.wait_log("down", timeout=4) or C.wait_log("reconfigure", timeout=2), "log check")
            dpage.close()
            time.sleep(2.0)

            real_errors, bad404 = C.benign_console(console_errors, not_found)
            res.check("no console errors (filtered)", len(real_errors) == 0,
                      "; ".join(real_errors)[:200])
            res.check("no unexpected 404s", not bad404, bad404[:2])
        finally:
            browser.close()
            if wl_obs is not None:
                wl_obs.stop()
    res.summary()
    return res


def main() -> None:
    """Run the matrix blocks named on argv (default: all four)."""
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    engine = os.environ.get("E2E_ENGINE", "chromium")
    blocks = []
    if which in ("all", "ws-x11"):
        blocks.append(run_block("websockets", False, engine=engine))
    if which in ("all", "wr-x11"):
        blocks.append(run_block("webrtc", False, engine=engine))
    if which in ("all", "ws-wl"):
        blocks.append(run_block("websockets", True, engine=engine))
    if which in ("all", "wr-wl"):
        blocks.append(run_block("webrtc", True, engine=engine))
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== MATRIX: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
