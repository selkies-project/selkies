#!/usr/bin/env python3
"""The image clipboard, both directions, against a live session.

The dashboard's upload button is the only way an image the user did not copy
reaches the session clipboard, and choosing a file blurs the page and refocuses
it -- which fires the focus-driven local sync. Whether the image survives that
is the whole feature, so the checks read the session's own clipboard rather
than the message that carried the image to the core.

Usage: python3 tests/e2e/test_clipboard_image.py [websockets|webrtc|wayland]
"""
import os
import struct
import subprocess
import sys
import threading
import time
import zlib

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402

DASH = os.path.join(H.REPO, "addons/selkies-dashboard/dist")
WL_SOCKET = "wayland-1"


def png(seed: int) -> bytes:
    """An 8x8 PNG whose pixels follow `seed`, so two of them never compare equal."""
    side = 8
    raw = b"".join(b"\x00" + bytes([(seed + x) % 256, (seed * 3) % 256, x * 7 % 256] * side)
                   for x in range(side))

    def chunk(kind: bytes, body: bytes) -> bytes:
        payload = kind + body
        return (struct.pack(">I", len(body)) + payload
                + struct.pack(">I", zlib.crc32(payload) & 0xFFFFFFFF))

    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", side, side, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw))
            + chunk(b"IEND", b""))


def _wl_env() -> dict:
    return {**os.environ, "WAYLAND_DISPLAY": WL_SOCKET,
            "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", H.WORKDIR)}


def session_image(wayland: bool) -> tuple:
    """The session clipboard's offered targets and its image bytes."""
    if wayland:
        listed = subprocess.run(["wl-paste", "-l"], capture_output=True, text=True,
                                timeout=8, env=_wl_env())
        targets = [t.strip() for t in listed.stdout.splitlines() if t.strip()]
        mime = next((t for t in targets if t.startswith("image/")), None)
        if mime is None:
            return targets, None, None
        got = subprocess.run(["wl-paste", "-t", mime], capture_output=True,
                             timeout=10, env=_wl_env())
        return targets, mime, got.stdout

    from selkies.Xlib import display as xdisp, X
    from selkies.Xlib.protocol import event as xevent
    d = xdisp.Display(H.require_display())
    try:
        scr = d.screen()
        win = scr.root.create_window(0, 0, 1, 1, 0, scr.root_depth,
                                     window_class=X.InputOutput)
        clip = d.get_atom("CLIPBOARD")
        prop = d.get_atom("SELKIES_IMAGE_PROBE")

        def convert(atom, timeout=8.0):
            win.convert_selection(clip, atom, prop, X.CurrentTime)
            d.flush()
            end = time.monotonic() + timeout
            while time.monotonic() < end:
                if not d.pending_events():
                    time.sleep(0.02)
                    continue
                ev = d.next_event()
                if isinstance(ev, xevent.SelectionNotify):
                    if ev.property == X.NONE:
                        return None
                    value = win.get_full_property(prop, X.AnyPropertyType)
                    win.delete_property(prop)
                    d.flush()
                    return value.value if value is not None else None
            return None

        offered = convert(d.get_atom("TARGETS"))
        targets = [d.get_atom_name(a) for a in (offered or [])]
        mime = next((t for t in targets if t.startswith("image/")), None)
        if mime is None:
            return targets, None, None
        return targets, mime, bytes(bytearray(convert(d.get_atom(mime)) or b""))
    finally:
        d.close()


def own_session_image(data: bytes, wayland: bool) -> dict:
    """Put `data` on the session clipboard as image/png, as an application would.

    Returns:
        A handle whose `stop` key ends the X owner; a no-op on Wayland, where
        wl-copy holds the selection itself.
    """
    if wayland:
        proc = subprocess.Popen(["wl-copy", "-t", "image/png"],
                                stdin=subprocess.PIPE, env=_wl_env())
        proc.communicate(data, timeout=10)
        return {"stop": lambda: None}

    from selkies.Xlib import display as xdisp, X
    from selkies.Xlib.protocol import event as xevent
    d = xdisp.Display(H.require_display())
    scr = d.screen()
    win = scr.root.create_window(0, 0, 1, 1, 0, scr.root_depth, window_class=X.InputOutput)
    clip = d.get_atom("CLIPBOARD")
    targets = d.get_atom("TARGETS")
    image = d.get_atom("image/png")
    win.set_selection_owner(clip, X.CurrentTime)
    d.flush()
    state = {"flag": False}

    def serve():
        try:
            deadline = time.monotonic() + 60.0
            while not state["flag"] and time.monotonic() < deadline:
                if not d.pending_events():
                    time.sleep(0.005)
                    continue
                ev = d.next_event()
                if not isinstance(ev, xevent.SelectionRequest):
                    continue
                if ev.target == targets:
                    ev.requestor.change_property(ev.property, targets, 32, [targets, image])
                elif ev.target == image:
                    ev.requestor.change_property(ev.property, image, 8, data)
                else:
                    ev.requestor.send_event(xevent.SelectionNotify(
                        time=ev.time, requestor=ev.requestor, selection=ev.selection,
                        target=ev.target, property=X.NONE), propagate=False)
                    d.flush()
                    continue
                ev.requestor.send_event(xevent.SelectionNotify(
                    time=ev.time, requestor=ev.requestor, selection=ev.selection,
                    target=ev.target, property=ev.property), propagate=False)
                d.flush()
        finally:
            try:
                d.close()
            except Exception:
                pass

    threading.Thread(target=serve, daemon=True).start()

    def stop():
        state["flag"] = True

    return {"stop": stop}


def open_clipboard_panel(page) -> bool:
    """Open the classic dashboard's clipboard section; False when it is absent."""
    if not page.evaluate("!!document.querySelector('.sidebar.is-open')"):
        page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        time.sleep(0.8)
    header = page.locator('.sidebar-section-header:has-text("Clipboard")')
    if header.count() == 0:
        return False
    header.first.click()
    time.sleep(0.6)
    return page.locator('input[type="file"][accept="image/*"]').count() > 0


def block(mode: str, wayland: bool) -> "H.Results":
    """One transport and backend: upload out, session copy in."""
    tag = f"clipimage-{'wl' if wayland else mode}"
    res = H.Results(tag)
    uploaded = png(23)
    H.server_start(mode=mode, wayland=wayland, web_root=DASH)
    with sync_playwright() as p:
        browser = C.chromium_launch(p)
        ctx = browser.new_context(viewport={"width": 1440, "height": 900},
                                  device_scale_factor=1)
        ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
        try:
            ctx.grant_permissions(["clipboard-read", "clipboard-write"], origin=H.BASE_URL)
        except Exception:
            pass
        page = ctx.new_page()
        page.goto(H.BASE_URL, wait_until="load")
        owner = None
        try:
            time.sleep(12.0)
            # Something the user copied locally and has not synced: the value
            # the focus read would put back over the upload.
            page.evaluate("navigator.clipboard.writeText('local text, not the image')")
            time.sleep(1.0)
            if not open_clipboard_panel(page):
                res.skip(f"{tag}: the upload path", "no clipboard image picker in the panel")
                return res

            picker = page.locator('input[type="file"][accept="image/*"]').first
            picker.set_input_files({"name": "clip.png", "mimeType": "image/png",
                                    "buffer": uploaded})
            # What the file picker itself does to the page as it closes.
            page.evaluate("window.dispatchEvent(new Event('focus'))")
            time.sleep(5.0)
            targets, mime, got = session_image(wayland)
            res.check("an uploaded image reaches the session clipboard",
                      got == uploaded, f"{mime} {len(got) if got else 0} bytes, offered {targets}")

            copied = png(91)
            owner = own_session_image(copied, wayland)
            # Two gestures: the write is refused without a user activation, and
            # the payload has to have arrived before the one that lands it.
            for _ in range(2):
                page.mouse.move(300, 300)
                page.mouse.down()
                page.mouse.up()
                time.sleep(2.5)
            local = page.evaluate("""async () => {
              try {
                const items = await navigator.clipboard.read();
                for (const item of items) {
                  for (const type of item.types) {
                    if (!type.startsWith('image/')) continue;
                    const blob = await item.getType(type);
                    return { type, size: (await blob.arrayBuffer()).byteLength };
                  }
                }
                return null;
              } catch (err) { return 'read failed: ' + err.name; }
            }""")
            # The browser re-encodes what it writes, so the size is its own;
            # that an image is there at all is what the push had to achieve.
            res.check("a session image reaches the local clipboard",
                      isinstance(local, dict) and local.get("size", 0) > 0, local)

            # Copying the same image again is a fresh copy, not this server's
            # own write coming back: a client that failed to apply the first
            # one has nothing else to wait for. Counted in the log, since the
            # client suppresses a local write of content it already holds.
            sends = H.server_log().count("Clipboard changed. Sending content")
            owner["stop"]()
            time.sleep(1.0)
            owner = own_session_image(copied, wayland)
            time.sleep(4.0)
            again = H.server_log().count("Clipboard changed. Sending content")
            res.check("the same image copied again is sent again",
                      again > sends, f"{sends} sends, then {again}")
        finally:
            if owner:
                owner["stop"]()
            browser.close()
    res.summary()
    return res


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "websockets"
    if which == "wayland":
        results = [block("websockets", True)]
    else:
        results = [block(which, False)]
    H.server_stop()
    failed = sum(len(r.failed()) for r in results)
    print(f"\n=== CLIPBOARD IMAGE: {'FAIL' if failed else 'PASS'} ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
