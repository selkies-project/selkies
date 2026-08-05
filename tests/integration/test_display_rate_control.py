#!/usr/bin/env python3
"""D4b: after a client switches rate_control_mode to 'cbr' via SETTINGS or the
'_rc' verb on the primary, a newly-seeded display2 must inherit cbr, not the
server default (crf). Verifies via the display2 capture's CBR-mode log line."""
import asyncio
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets


def _settings(display_id, **over):
    base = {
        "displayId": display_id, "initialClientWidth": 1280, "initialClientHeight": 720,
        "is_manual_resolution_mode": False, "framerate": 60, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": 4000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    }
    base.update(over)
    return base


def loglen():
    return len(H.server_log())


async def read_ws(ws, seconds):
    end = time.time() + seconds
    while time.time() < end:
        try:
            await asyncio.wait_for(ws.recv(), timeout=0.5)
        except Exception:
            return


def drive_cbr(use_opcode):
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    results = {}

    async def impl():
        async with websockets.connect(uri, max_size=None) as p1:
            await asyncio.wait_for(p1.recv(), timeout=10)
            await p1.send("SETTINGS," + json.dumps(_settings("primary")))
            await asyncio.sleep(3.0)
            await read_ws(p1, 1.0)
            if use_opcode:
                await p1.send("_rc,cbr")
            else:
                await p1.send("SETTINGS," + json.dumps(_settings("primary", rate_control_mode="cbr")))
            await asyncio.sleep(3.0)
            await read_ws(p1, 1.0)
            # second display: connected AFTER the rate control was set
            mark2 = loglen()
            async with websockets.connect(uri, max_size=None) as p2:
                await asyncio.wait_for(p2.recv(), timeout=10)
                await p2.send("SETTINGS," + json.dumps(_settings("display2")))
                await asyncio.sleep(3.0)
                seg = H.server_log()[mark2:]
                lines = [l for l in seg.splitlines() if "Stream settings active" in l]
                results['d2'] = lines[-1] if lines else seg[-200:]
                await read_ws(p2, 0.5)
            await read_ws(p1, 0.5)
    asyncio.new_event_loop().run_until_complete(impl())
    return results


def main():
    for use_opcode in (False, True):
        H.server_start(mode="websockets", wayland=False, extra_env={"SELKIES_ENABLE_RATE_CONTROL": "true"})
        label = "SETTINGS" if not use_opcode else "_rc-opcode"
        res = H.Results(f"d4b-{label}")
        r = drive_cbr(use_opcode)
        seg = r.get('d2', '')
        res.check(f"display2 inherits cbr ({label})", "CBR" in seg, seg)
        res.summary()

if __name__ == "__main__":
    main()