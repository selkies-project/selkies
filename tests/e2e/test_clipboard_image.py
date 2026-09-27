#!/usr/bin/env python3
"""The image clipboard, both directions, against a live session.

The dashboards' Upload Image button is the only way an image the user did not
copy reaches the session clipboard. Choosing the file takes the window's focus
while the dialog is open -- which closes the Wish panel's menu -- and gives it
back as the dialog closes, which fires the focus-driven local sync. Whether the
image survives that is the whole feature, so the checks read the session's own
clipboard rather than the message that carried the image to the core, in both
dashboards.

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
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
import test_dashboards as TD  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402

DASHES = {"classic": H.CLASSIC_DIST, "wish": H.WISH_DIST}
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
            "XDG_RUNTIME_DIR": H.RUNTIME_DIR}


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


def upload(page, dashboard: str, name: str, mime: str, data: bytes, wayland: bool) -> tuple:
    """Upload `data` through the dashboard's button and read the session clipboard back.

    The dialog's own refocus as it closes fires the focus read, which is what
    the upload has to survive.
    """
    if not TD.pick_clipboard_image(page, dashboard, {"name": name, "mimeType": mime, "buffer": data}):
        return None
    page.evaluate("window.dispatchEvent(new Event('focus'))")
    time.sleep(5.0)
    return session_image(wayland)


def block(mode: str, wayland: bool, dashboard: str) -> "H.Results":
    """One transport, backend, and dashboard: upload out, session copies in."""
    tag = f"clipimage-{'wl' if wayland else mode}-{dashboard}"
    res = H.Results(tag)
    checks(res, tag, mode, wayland, dashboard)
    res.summary()
    return res


def checks(res: "H.Results", tag: str, mode: str, wayland: bool, dashboard: str) -> None:
    """The checks of one block, each recorded in `res`."""
    uploaded = png(23)
    H.server_start(mode=mode, wayland=wayland, web_root=DASHES[dashboard],
                   extra_env={"SELKIES_DEBUG": "true"})
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
        owners = []
        try:
            time.sleep(12.0)
            # Something the user copied locally and has not synced: the value
            # the focus read would put back over the upload.
            page.evaluate("navigator.clipboard.writeText('local text, not the image')")
            time.sleep(1.0)
            got = upload(page, dashboard, "clip.png", "image/png", uploaded, wayland)
            if got is None:
                res.skip(f"{tag}: the upload path", "no Upload Image button in the panel")
                return
            targets, mime, data = got
            res.check("an uploaded image reaches the session clipboard",
                      data == uploaded, f"{mime} {len(data) if data else 0} bytes, offered {targets}")

            if dashboard != "classic":
                return

            copied = png(91)
            owners.append(own_session_image(copied, wayland))
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
            owners.pop()["stop"]()
            time.sleep(1.0)
            owners.append(own_session_image(copied, wayland))
            time.sleep(4.0)
            again = H.server_log().count("Clipboard changed. Sending content")
            res.check("the same image copied again is sent again",
                      again > sends, f"{sends} sends, then {again}")

        finally:
            for owner in owners:
                owner["stop"]()
            browser.close()


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "websockets"
    mode, wayland = ("websockets", True) if which == "wayland" else (which, False)
    results = [block(mode, wayland, dashboard) for dashboard in DASHES]
    H.server_stop()
    failed = sum(len(r.failed()) for r in results)
    print(f"\n=== CLIPBOARD IMAGE: {'FAIL' if failed else 'PASS'} ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
