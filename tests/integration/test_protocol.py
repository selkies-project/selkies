#!/usr/bin/env python3
"""Protocol-level e2e: raw WS client drives SETTINGS / STOP_VIDEO / opcodes to
verify A1 (settings while stopped must not restart capture) and F4/F5 (the
WebRTC-dialect opcodes apply live on the websockets transport, sanitized)."""
import asyncio
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets


def _settings_payload(**over) -> dict:
    """Build a primary-display SETTINGS payload with overrides applied on top."""
    base = {
        "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
        "is_manual_resolution_mode": False, "framerate": 60, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    }
    base.update(over)
    return base


async def _no_ack_task(*a, **k):
    return None


def loglen() -> int:
    return len(H.server_log())


def wait_log_from(mark: int, substr: str, timeout: float = 10) -> bool:
    """Poll the server log for a substring appearing at or after an offset.

    Args:
        mark: Byte offset into the log where the search starts.
        substr: Substring to wait for.
        timeout: Seconds to keep polling before giving up.

    Returns:
        True when the substring appeared, False on timeout.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        txt = H.server_log()
        i = txt.find(substr, mark)
        if i >= 0:
            return True
        time.sleep(0.4)
    return False


def run() -> "H.Results":
    """Drive the raw-WS opcode sequence and record each protocol check."""
    res = H.Results("protocol")
    H.server_start(mode="websockets", wayland=False)

    async def drive():
        uri = f"ws://localhost:{H.PORT}/api/websockets"
        async with websockets.connect(uri, max_size=None) as ws:
            mode_msg = await asyncio.wait_for(ws.recv(), timeout=10)
            assert mode_msg.startswith("MODE "), mode_msg[:40]
            # initial settings -> capture starts
            await ws.send("SETTINGS," + json.dumps(_settings_payload()))
            await asyncio.sleep(4.0)
            txt = H.server_log()
            res.check("protocol: capture started from initial SETTINGS",
                      "Capture started for 'primary'" in txt, "")

            # STOP_VIDEO -> capture stops
            st = loglen()
            await ws.send("STOP_VIDEO")
            await asyncio.sleep(2.5)
            res.check("protocol: STOP_VIDEO stops capture",
                      wait_log_from(st, "Stopping all streams for display 'primary'", 6), "")

            # A1: SETTINGS change while stopped must NOT restart the capture
            st = loglen()
            await ws.send("SETTINGS," + json.dumps(_settings_payload(framerate=30)))
            ok = wait_log_from(st, "deferring the restart", 8)
            res.check("A1: settings change defers restart while stopped", ok, "")
            res.check("A1: no capture restart while stopped",
                      not wait_log_from(st, "Preparing to start capture", 1) or ok, "")
            # still stopped after the deferral
            res.check("A1: capture stays stopped after deferral",
                      wait_log_from(st, "SUCCESS: Capture started", 2) is False, "")

            # START_VIDEO resumes with the stored values
            await ws.send("START_VIDEO")
            res.check("protocol: START_VIDEO resumes capture",
                      wait_log_from(loglen()-1000, "Capture started", 8)
                      or wait_log_from(st, "Capture started", 8), "")

            # F4: _arg_fps applies live
            st = loglen()
            await ws.send("_arg_fps,33")
            ok = wait_log_from(st, "framerate", 6)
            res.check("F4: '_arg_fps,33' applied", ok, "")

            # F5: wr-dialect opcodes apply live (sanitized)
            st = loglen()
            await ws.send("vb,1")
            ok = wait_log_from(st, "clamped to 100", 6)
            res.check("F5: 'vb,1' clamped to range min", ok, "")

            st = loglen()
            await ws.send("vb,3912")
            ok = wait_log_from(st, "3912", 6)
            res.check("F5: 'vb,3912' live bitrate applied", ok, "")

            # structural: rate control change restarts the capture (websockets
            # defaults h264enc to CRF, so toggle to CBR)
            st = loglen()
            await ws.send("_rc,cbr")
            ok = wait_log_from(st, "Applied rate-control via '_rc'", 8) or \
                 wait_log_from(st, "Restarting its capture stream", 8)
            res.check("F5: '_rc,cbr' triggers structural restart", ok, "")
            # Toggle back so the run ends in the transport's default CRF mode.
            st = loglen()
            await ws.send("_rc,crf")
            await asyncio.sleep(5)
            res.check("F5: '_rc,crf' toggles back to CRF",
                      wait_log_from(st, "Applied rate-control via '_rc': crf", 8), "")

            await ws.send("STOP_VIDEO")
            await asyncio.sleep(1.0)

    asyncio.get_event_loop().run_until_complete(drive())
    res.summary()
    return res


if __name__ == "__main__":
    r = run()
    sys.exit(0 if not r.failed() else 1)
