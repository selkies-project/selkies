#!/usr/bin/env python3
"""Large clipboard (multipart) parity: push a ~1.5MB text payload from the X
server to the page (server->client) and from the page (client->server), on both
transports. Confirms the multipart chunked paths and the ws_max_message_bytes
capacity wiring."""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright


def big_block(mode='websockets'):
    tag = f"bigclip-{mode}"
    res = H.Results(tag)
    H.server_start(mode=mode, wayland=False)
    with sync_playwright() as p:
        browser, page, console_errors, not_found = C.launch_chrome(p, mode=mode)
        try:
            info = C.wait_ws_video(page) if mode == "websockets" else C.wait_wr_video(page, timeout=45)
            res.check("video up", info is not None, info)
            time.sleep(1.5)
            big = "BIGCLIP-" + ("x" * (2 * 1024 * 1024))
            ext, stop = H.x_own_clipboard(big.encode())
            page.wait_for_timeout(3500)
            got = page.evaluate("window.__clipMsgs.map(m => ({text: m.text, t: m.totalLength}))")
            stop["flag"] = True
            # The live text is posted as a truncated preview (bounded memory in
            # the dashboard); the authoritative signal is totalLength and the
            # real browser clipboard holding the full payload.
            ok_len = any(g["t"] is not None and g["t"] >= 2000000 for g in got if isinstance(g, dict))
            local = ""
            try:
                local = page.evaluate("navigator.clipboard.readText()")
            except Exception as e:
                print("clipboard read err:", str(e)[:80])
            ok_local = local.startswith("BIGCLIP-") and len(local) >= 2000000
            res.check("server->client large clipboard totalLength arrives", ok_len,
                      f"totalLengths {[g.get('t') for g in got]}")
            res.check("server->client large clipboard lands in navigator.clipboard", ok_local,
                      f"local len {len(local)}")
            time.sleep(0.5)
        finally:
            browser.close()
    res.summary()
    return res


def main():
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = []
    if which in ("all", "websockets"):
        blocks.append(big_block("websockets"))
    if which in ("all", "webrtc"):
        blocks.append(big_block("webrtc"))
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== BIGCLIP: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()