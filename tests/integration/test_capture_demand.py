#!/usr/bin/env python3
"""The demand loop end to end below the browser: does the session ask a page to capture
only while something in the desktop is using the device?

A real server, a real websocket client, a real process opening the virtual camera through
the interposer, and a real recorder on the virtual microphone. What this catches that the
unit suites cannot: the watcher polls sinks the fakes stand in for, so a sink that stops
reporting a reader, a hold-off that never elapses, or a demand published to nobody leaves
the page holding the user's camera with the light on and nothing asking for it back.
"""
import asyncio
import os
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import websockets  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ADDON = os.path.join(ROOT, "addons", "v4l2-interposer")
INTERPOSER = os.path.join(ADDON, "selkies_v4l2_interposer.so")
DEVICE = "/dev/video%s" % os.environ.get("SELKIES_WEBCAM_DEVICE", "0")
VIRTUAL_MIC = "SelkiesVirtualMic"
# The camera passes a reader on at the poll that finds it and holds off before letting go,
# so a window has to outlast the release hold-off plus the device re-check floor.
HOLD_OFF_WINDOW = 14.0
MIC_HOLD_OFF_WINDOW = 22.0


async def demands(ws, seconds: float, subject: str = "") -> list:
    """Every CAPTURE_DEMAND the server sends in this window, for `subject` when given."""
    out, end = [], time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            msg = await asyncio.wait_for(ws.recv(), timeout=max(0.1, end - time.monotonic()))
        except asyncio.TimeoutError:
            break
        except Exception:
            break
        if isinstance(msg, str) and msg.startswith("CAPTURE_DEMAND"):
            if not subject or f" {subject} " in msg:
                out.append(msg.strip())
    return out


async def run(res: H.Results) -> None:
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as ws:
        # An unread device asks for nothing at all; the checks below are what prove the
        # server answers when something does read it.
        idle = await demands(ws, 6.0)
        res.check("no capture is asked for while nothing uses the device",
                  not any(m.endswith("1") for m in idle), str(idle))

        # /dev/video0 is the interposer's own default, not a guess about this host's
        # loopback nodes: SELKIES_WEBCAM_DEVICE picks another index and neither the
        # server nor this suite sets it, so the two agree by construction. The camera
        # socket is in the server's runtime directory, which an application of its
        # session shares.
        opener = subprocess.Popen(
            [sys.executable, "-c",
             f"import os,time;f=os.open({DEVICE!r},os.O_RDWR);time.sleep(40);os.close(f)"],
            env=dict(os.environ, LD_PRELOAD=INTERPOSER, XDG_RUNTIME_DIR=H.RUNTIME_DIR),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            held = await demands(ws, 8.0, "webcam")
            res.check("the camera is asked for when an application opens the device",
                      any(m.endswith("1") for m in held), str(held))
        finally:
            opener.kill()
            opener.wait()
        released = await demands(ws, HOLD_OFF_WINDOW, "webcam")
        res.check("and released once the device is closed",
                  any(m.endswith("0") for m in released), str(released))

        # PipeWire publishes a virtual source as `output.<name>` and PulseAudio as the
        # bare name; ask the server which one it made rather than guessing, or the two
        # microphone checks fail for the wrong reason on half the hosts.
        listed = subprocess.run(["pactl", "list", "short", "sources"],
                                capture_output=True, text=True).stdout
        source = next((n for n in ("output." + VIRTUAL_MIC, VIRTUAL_MIC)
                       if n in listed), None)
        if source is None:
            res.check("the virtual microphone source exists", False, listed[:160])
            return
        recorder = subprocess.Popen(
            ["parec", "-d", source, "--raw"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            held = await demands(ws, 10.0, "microphone")
            res.check("the microphone is asked for when an application records",
                      any(m.endswith("1") for m in held), str(held))
        finally:
            recorder.kill()
            recorder.wait()
        released = await demands(ws, MIC_HOLD_OFF_WINDOW, "microphone")
        res.check("and released once the recording stops",
                  any(m.endswith("0") for m in released), str(released))


def main() -> int:
    res = H.Results("capture-demand")
    if shutil.which("parec") is None:
        print("FAIL  [capture-demand] parec is required by this suite and is not installed")
        return 1
    subprocess.run(["make", "-C", ADDON], check=True, stdout=subprocess.DEVNULL)
    H.pulse_setup()
    server = H.server_start(extra_env={
        "SELKIES_WEBCAM_ENABLED": "true",
        "SELKIES_WEBCAM_ON_START": "demand",
        "SELKIES_MICROPHONE_ENABLED": "true",
        "SELKIES_MICROPHONE_ON_START": "demand",
    })
    try:
        asyncio.run(run(res))
    finally:
        H.server_stop()
        server.wait(timeout=10)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
