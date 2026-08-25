#!/usr/bin/env python3
"""Protocol-level e2e: raw WS client drives SETTINGS / STOP_VIDEO / opcodes to
verify A1 (settings while stopped must not restart capture) and F4/F5 (the
WebRTC-dialect opcodes apply live on the websockets transport, sanitized).

Also covers the `cmd` opcode the dashboards' apps panel posts on: the command
names it builds are bare, so the server has to resolve them on its own PATH,
and a launch that fails has to come back as command_error or the optimistic
install/remove in the UI never rolls back."""
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


def command_stub(path: str, sentinel: str) -> None:
    """Write an executable standing in for the image's selkies-proot wrapper.

    Args:
        path: File to create; its directory goes on the server's PATH.
        sentinel: File the stub writes its arguments to when it runs.
    """
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as fh:
        fh.write('#!/bin/sh\necho "$@" > "{}"\n'.format(sentinel))
    os.chmod(path, 0o755)


def run() -> "H.Results":
    """Drive the raw-WS opcode sequence and record each protocol check."""
    res = H.Results("protocol")
    stub_dir = os.path.join(H.WORKDIR, "cmd-stub-bin")
    sentinel = os.path.join(H.WORKDIR, "cmd-stub.args")
    command_stub(os.path.join(stub_dir, "selkies-proot"), sentinel)
    if os.path.exists(sentinel):
        os.unlink(sentinel)
    H.server_start(mode="websockets", wayland=False,
                   extra_env={"SELKIES_COMMAND_ENABLED": "true",
                              "PATH": stub_dir + os.pathsep + os.environ.get("PATH", "")})

    async def drive():
        uri = f"ws://localhost:{H.PORT}/api/websockets"
        async with websockets.connect(uri, max_size=None) as ws:
            mode_msg = await asyncio.wait_for(ws.recv(), timeout=10)
            assert mode_msg.startswith("MODE "), mode_msg[:40]
            await ws.send("SETTINGS," + json.dumps(_settings_payload()))
            await asyncio.sleep(4.0)
            txt = H.server_log()
            res.check("protocol: capture started from initial SETTINGS",
                      "Capture started for 'primary'" in txt, "")

            st = loglen()
            await ws.send("STOP_VIDEO")
            await asyncio.sleep(2.5)
            res.check("protocol: STOP_VIDEO stops capture",
                      wait_log_from(st, "Stopping all streams for display 'primary'", 6), "")

            st = loglen()
            await ws.send("SETTINGS," + json.dumps(_settings_payload(framerate=30)))
            ok = wait_log_from(st, "deferring the restart", 8)
            res.check("A1: settings change defers restart while stopped", ok, "")
            res.check("A1: no capture restart while stopped",
                      not wait_log_from(st, "Preparing to start capture", 1) or ok, "")
            res.check("A1: capture stays stopped after deferral",
                      wait_log_from(st, "SUCCESS: Capture started", 2) is False, "")

            await ws.send("START_VIDEO")
            res.check("protocol: START_VIDEO resumes capture",
                      wait_log_from(loglen()-1000, "Capture started", 8)
                      or wait_log_from(st, "Capture started", 8), "")

            st = loglen()
            await ws.send("_arg_fps,33")
            ok = wait_log_from(st, "framerate", 6)
            res.check("F4: '_arg_fps,33' applied", ok, "")

            st = loglen()
            await ws.send("vb,1")
            ok = wait_log_from(st, "clamped to 100", 6)
            res.check("F5: 'vb,1' clamped to range min", ok, "")

            st = loglen()
            await ws.send("vb,3912")
            ok = wait_log_from(st, "3912", 6)
            res.check("F5: 'vb,3912' live bitrate applied", ok, "")

            # websockets defaults h264enc to CRF, so CBR is the structural change.
            st = loglen()
            await ws.send("_rc,cbr")
            ok = wait_log_from(st, "Applied rate-control via '_rc'", 8) or \
                 wait_log_from(st, "Restarting its capture stream", 8)
            res.check("F5: '_rc,cbr' triggers structural restart", ok, "")
            # Back to CRF so the run ends in the transport's default mode.
            st = loglen()
            await ws.send("_rc,crf")
            await asyncio.sleep(5)
            res.check("F5: '_rc,crf' toggles back to CRF",
                      wait_log_from(st, "Applied rate-control via '_rc': crf", 8), "")

            await ws.send("cmd,selkies-proot install demo-app")
            deadline = time.time() + 10
            while time.time() < deadline and not os.path.exists(sentinel):
                await asyncio.sleep(0.25)
            got = ""
            if os.path.exists(sentinel):
                with open(sentinel) as fh:
                    got = fh.read().strip()
            res.check("cmd: a bare command name resolves on the server's PATH",
                      got == "install demo-app", got)

            # command_error carries the echoed command, which is what the
            # dashboards match their pending action against.
            await ws.send("cmd,selkies-proot-absent install demo-app")
            err = ""
            deadline = time.time() + 10
            while time.time() < deadline and not err:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=2.0)
                except asyncio.TimeoutError:
                    continue
                if isinstance(msg, str) and msg.startswith("system,"):
                    action = json.loads(msg[len("system,"):]).get("action", "")
                    if action.startswith("command_error,"):
                        err = action
            res.check("cmd: a missing command reports command_error to the client",
                      err.endswith(": selkies-proot-absent install demo-app")
                      and "127" in err, err)

            # A bystander that receives a fetch reply reads it as its own and
            # caches the content unwritten, suppressing the next real change. It
            # joins as a viewer: a second controller would take the display over.
            async with websockets.connect(uri + "?role=viewer", max_size=None) as other:
                await asyncio.wait_for(other.recv(), timeout=10)
                await ws.send("cr")
                # Read for a fixed window rather than to a gap: a viewer is
                # being streamed to, so there is always another frame coming.
                stray = []
                deadline = time.monotonic() + 3.0
                while time.monotonic() < deadline:
                    try:
                        seen = await asyncio.wait_for(other.recv(), timeout=0.3)
                    except asyncio.TimeoutError:
                        continue
                    if isinstance(seen, str) and seen.startswith("clipboard"):
                        stray.append(seen)
                res.check("clipboard: a fetch is answered to its requester alone",
                          not stray, f"{stray[:2]}")

            await ws.send("STOP_VIDEO")
            await asyncio.sleep(1.0)

    asyncio.run(drive())
    res.summary()
    return res


if __name__ == "__main__":
    r = run()
    sys.exit(0 if not r.failed() else 1)
