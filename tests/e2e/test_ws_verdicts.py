#!/usr/bin/env python3
"""What a websockets client is told, and what it may do, at connect.

mk-access: in secure mode every tokened client receives its input-authority
verdict in the handshake — MK_ACCESS,1 for the mk-token holder (a controller
while no mk token is provisioned), MK_ACCESS,0 for everyone else — after the
MODE message that makes the page build the input context the verdict applies
to, the way WebRTC pushes it at channel open. A viewer holding the mk token
stays read-only with collab disabled. The session token never reaches the
server log.

no-resize: with dynamic resizing disabled nothing a SETTINGS carries resizes
the desktop an operator sized with `selkies-resize`: not the page's window in
the first one, not 16-pixel alignment, and not a manual resolution in the
first or a later one. The server keeps the desktop's current size and tells
the client the realized geometry to fit. With it enabled the same SETTINGS
does resize, which is what proves the check can see one.
"""
import asyncio
import json
import os
import subprocess
import sys
import time
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets

MASTER = "e2e-master-token"
CONTROL_TOKEN = "e2e-ctrl-Qx9"
VIEW_TOKEN = "e2e-view-Zt4"


def post_tokens(table: dict) -> int:
    """Replace the session token table through the Bearer-gated endpoint."""
    status, _ = H.curl("/api/tokens", method="POST", data=table,
                       headers={"Authorization": f"Bearer {MASTER}"})
    return status


async def handshake(query: str, seconds: float = 4.0) -> tuple:
    """Connect and collect the text messages of the handshake window.

    Returns:
        `(messages, close_code)`; the close code is None while the socket is
        still open when the window ends.
    """
    uri = f"ws://localhost:{H.PORT}/api/websockets{query}"
    messages = []
    close_code = None
    try:
        async with websockets.connect(uri, max_size=None) as ws:
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=0.5)
                except asyncio.TimeoutError:
                    continue
                if isinstance(msg, str):
                    messages.append(msg)
    except websockets.exceptions.ConnectionClosed as e:
        close_code = e.rcvd.code if e.rcvd else e.code
    except Exception as e:
        messages.append(f"ERROR {e!r}")
    return messages, close_code


def verdict(messages: list) -> tuple:
    """The AUTH_SUCCESS role, the MK_ACCESS verdict, and whether MODE preceded it."""
    role = mk = None
    mode_at = mk_at = None
    for i, m in enumerate(messages):
        if m.startswith("AUTH_SUCCESS,"):
            try:
                role = json.loads(m.split(",", 1)[1]).get("role")
            except ValueError:
                role = "unparsable"
        elif m.startswith("MODE ") and mode_at is None:
            mode_at = i
        elif m.startswith("MK_ACCESS,") and mk_at is None:
            mk_at = i
            mk = m.split(",", 1)[1].strip()
    ordered = mode_at is not None and mk_at is not None and mode_at < mk_at
    return role, mk, ordered


def run_mk_access() -> "H.Results":
    res = H.Results("mk-access")
    H.server_start(mode="websockets", wayland=False,
                   extra_env={"SELKIES_MASTER_TOKEN": MASTER})
    status = post_tokens({
        CONTROL_TOKEN: {"role": "controller", "slot": 1},
        VIEW_TOKEN: {"role": "viewer", "slot": None, "mk_control": True},
    })
    res.check("tokens provisioned (viewer holds mk)", status == 200, status)

    async def drive() -> None:
        msgs, code = await handshake(f"?token={VIEW_TOKEN}")
        role, mk, ordered = verdict(msgs)
        res.check("viewer holding mk: AUTH_SUCCESS names the viewer role", role == "viewer", msgs[:4])
        res.check("viewer holding mk: MK_ACCESS,1 in the handshake", mk == "1", msgs[:4])
        res.check("viewer holding mk: the verdict follows MODE", ordered, msgs[:4])
        await asyncio.sleep(1.0)

        msgs, code = await handshake(f"?token={CONTROL_TOKEN}")
        role, mk, ordered = verdict(msgs)
        res.check("controller outranked by the mk token: MK_ACCESS,0",
                  role == "controller" and mk == "0" and ordered, msgs[:4])
        await asyncio.sleep(1.0)

        status = post_tokens({
            CONTROL_TOKEN: {"role": "controller", "slot": 1},
            VIEW_TOKEN: {"role": "viewer", "slot": None},
        })
        res.check("tokens re-provisioned (no mk token)", status == 200, status)
        msgs, code = await handshake(f"?token={CONTROL_TOKEN}")
        role, mk, ordered = verdict(msgs)
        res.check("no mk token: a controller connects with MK_ACCESS,1",
                  role == "controller" and mk == "1" and ordered, msgs[:4])
        await asyncio.sleep(1.0)
        msgs, code = await handshake(f"?token={VIEW_TOKEN}")
        role, mk, ordered = verdict(msgs)
        res.check("no mk token: a viewer connects with MK_ACCESS,0",
                  role == "viewer" and mk == "0" and ordered, msgs[:4])
        await asyncio.sleep(1.0)

        msgs, code = await handshake("?token=" + VIEW_TOKEN[:-1], seconds=2.0)
        res.check("a token that is only a prefix of a provisioned one is refused",
                  code == 4001 and not any(m.startswith("AUTH_SUCCESS") for m in msgs),
                  f"close {code} {msgs[:2]}")

    asyncio.run(drive())
    log = H.server_log()
    res.check("session tokens never reach the server log",
              CONTROL_TOKEN not in log and VIEW_TOKEN not in log and VIEW_TOKEN[:-1] not in log, "")

    H.server_start(mode="websockets", wayland=False,
                   extra_env={"SELKIES_MASTER_TOKEN": MASTER, "SELKIES_ENABLE_COLLAB": "false"})
    status = post_tokens({
        CONTROL_TOKEN: {"role": "controller", "slot": 1},
        VIEW_TOKEN: {"role": "viewer", "slot": None, "mk_control": True},
    })
    res.check("tokens provisioned with collab disabled", status == 200, status)

    async def drive_collab_off() -> None:
        msgs, _ = await handshake(f"?token={VIEW_TOKEN}")
        role, mk, ordered = verdict(msgs)
        res.check("collab off: a viewer holding mk is told MK_ACCESS,0",
                  role == "viewer" and mk == "0" and ordered, msgs[:4])

    asyncio.run(drive_collab_off())
    res.summary()
    return res


# The desktop a pinned server keeps: a laptop panel, which 16-pixel alignment
# would cut to 1920x1072.
PANEL = (1920, 1080)
# What the page asks for, aligned, so alignment alone never moves it.
WANT = (1280, 720)


def _settings_payload(width: int, height: int, **extra: Any) -> dict:
    return {
        "displayId": "primary", "initialClientWidth": width, "initialClientHeight": height,
        "manual_resolution": False, "framerate": 30, "encoder": "jpeg",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right", **extra,
    }


def _manual_payload(width: int, height: int) -> dict:
    """A page in manual mode: the manual size in place of its window's."""
    return _settings_payload(width, height, manual_resolution=True,
                             manual_width=width, manual_height=height)


async def send_settings(*payloads: dict, seconds: float = 8.0) -> tuple:
    """Connect and send each SETTINGS in turn, `seconds` apart so the capture
    runs and any reconfigure lands before the root is read.

    Returns:
        `(the first stream_resolution payload or None, the root size read
        after each SETTINGS)`.
    """
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    resolution = None
    roots = []
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        for payload in payloads:
            await ws.send("SETTINGS," + json.dumps(payload))
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=0.5)
                except asyncio.TimeoutError:
                    continue
                if resolution is None and isinstance(msg, str) and msg.startswith("{"):
                    try:
                        reply = json.loads(msg)
                    except ValueError:
                        continue
                    if reply.get("type") == "stream_resolution":
                        resolution = reply
            roots.append(H.x_root_size())
        await ws.send("STOP_VIDEO")
        await asyncio.sleep(0.5)
    return resolution, roots


def resize_desktop(width: int, height: int) -> tuple:
    """Size the test display as an operator sizes a desktop Selkies may not
    resize (`selkies-resize`, docs/native.md).

    Returns:
        The root size realized.
    """
    subprocess.run([H.PYTHON, "-m", "selkies.display_utils", f"{width}x{height}"],
                   env={**os.environ, "DISPLAY": H.require_display()},
                   capture_output=True, timeout=60)
    return H.x_root_size()


def run_no_resize() -> "H.Results":
    res = H.Results("no-resize")
    start = H.x_root_size()
    root = resize_desktop(*PANEL)
    res.check("the desktop is sized to the panel", root == PANEL, root)

    H.server_start(mode="websockets", wayland=False,
                   extra_env={"SELKIES_ENABLE_RESIZE": "false"})
    resolution, roots = asyncio.run(send_settings(
        _settings_payload(*WANT),
        _settings_payload(*WANT, force_aligned_resolution=True),
        _manual_payload(*WANT)))
    res.check("resize disabled: the desktop keeps its size on first SETTINGS",
              roots[0] == root, f"root {root} -> {roots[0]}")
    res.check("resize disabled: the client is told the realized geometry",
              resolution is not None and (resolution.get("width"), resolution.get("height")) == root,
              resolution)
    res.check("resize disabled: 16-pixel alignment keeps the desktop's size",
              roots[1] == root, f"root {root} -> {roots[1]}")
    res.check("resize disabled: a manual resolution keeps the desktop's size",
              roots[2] == root, f"root {root} -> {roots[2]}, asked {WANT}")
    resize_desktop(*root)
    _, roots = asyncio.run(send_settings(_manual_payload(*WANT)))
    res.check("resize disabled: a page connecting in manual mode keeps the desktop's size",
              roots[0] == root, f"root {root} -> {roots[0]}, asked {WANT}")
    log = H.server_log()
    res.check("resize disabled: the server logs the ignored initial size",
              "dynamic resizing disabled" in log, "")
    res.check("resize disabled: the capture still starts",
              "Capture started for 'primary'" in log, "")

    H.server_start(mode="websockets", wayland=False,
                   extra_env={"SELKIES_ENABLE_RESIZE": "true"})
    _, roots = asyncio.run(send_settings(_settings_payload(*WANT)))
    res.check("resize enabled: the same SETTINGS resizes the desktop",
              roots[0] == WANT, f"root {root} -> {roots[0]}, wanted {WANT}")
    # Put the shared display back the way it was found, once no server is
    # left to re-apply its own layout on the way out.
    H.server_stop()
    restored = resize_desktop(*start)
    res.check("display restored", restored == start, restored)
    res.summary()
    return res


BLOCKS = {"mk-access": run_mk_access, "no-resize": run_no_resize}


def main(selectors: list) -> bool:
    ok = True
    for name in selectors or list(BLOCKS):
        try:
            ok = not BLOCKS[name]().failed() and ok
        finally:
            H.server_stop()
    return ok


if __name__ == "__main__":
    sys.exit(0 if main(sys.argv[1:]) else 1)
