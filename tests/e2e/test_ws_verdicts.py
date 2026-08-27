#!/usr/bin/env python3
"""What a websockets client is told, and what it may do, at connect.

mk-access: in secure mode every tokened client receives its input-authority
verdict in the handshake — MK_ACCESS,1 for the mk-token holder (a controller
while no mk token is provisioned), MK_ACCESS,0 for everyone else — after the
MODE message that makes the page build the input context the verdict applies
to, the way WebRTC pushes it at channel open. A viewer holding the mk token
stays read-only with collab disabled. The session token never reaches the
server log.

no-resize: with dynamic resizing disabled the first SETTINGS does not resize
the desktop to the page's window; the server keeps the desktop's current size
and tells the client the realized geometry to fit. With it enabled the same
SETTINGS does resize, which is what proves the check can see one.
"""
import asyncio
import json
import os
import sys
import time

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


def _settings_payload(width: int, height: int) -> dict:
    return {
        "displayId": "primary", "initialClientWidth": width, "initialClientHeight": height,
        "manual_resolution": False, "framerate": 30, "encoder": "jpeg",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    }


async def first_settings(width: int, height: int, seconds: float = 12.0) -> tuple:
    """Connect, send the first SETTINGS, and wait for the stream_resolution
    reply; the socket is kept open for `seconds` so the capture runs.

    Returns:
        `(stream_resolution payload or None, messages)`.
    """
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    messages = []
    resolution = None
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send("SETTINGS," + json.dumps(_settings_payload(width, height)))
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=0.5)
            except asyncio.TimeoutError:
                continue
            if not isinstance(msg, str):
                continue
            messages.append(msg)
            if msg.startswith("{"):
                try:
                    payload = json.loads(msg)
                except ValueError:
                    continue
                if payload.get("type") == "stream_resolution" and resolution is None:
                    resolution = payload
        await ws.send("STOP_VIDEO")
        await asyncio.sleep(0.5)
    return resolution, messages


def run_no_resize() -> "H.Results":
    res = H.Results("no-resize")
    root_w, root_h = H.x_root_size()
    # A different, aligned size the page would ask for.
    want_w = max(640, (root_w - 256) & ~15)
    want_h = max(480, (root_h - 128) & ~15)
    if (want_w, want_h) == (root_w, root_h):
        want_w, want_h = root_w - 64, root_h - 64

    H.server_start(mode="websockets", wayland=False,
                   extra_env={"SELKIES_ENABLE_RESIZE": "false"})
    resolution, messages = asyncio.run(first_settings(want_w, want_h))
    after = H.x_root_size()
    res.check("resize disabled: the desktop keeps its size on first SETTINGS",
              after == (root_w, root_h), f"root {root_w}x{root_h} -> {after[0]}x{after[1]}")
    res.check("resize disabled: the client is told the realized geometry",
              resolution is not None and (resolution.get("width"), resolution.get("height")) == (root_w, root_h),
              resolution)
    log = H.server_log()
    res.check("resize disabled: the server logs the ignored initial size",
              "dynamic resizing disabled" in log, "")
    res.check("resize disabled: the capture still starts",
              "Capture started for 'primary'" in log, "")

    H.server_start(mode="websockets", wayland=False,
                   extra_env={"SELKIES_ENABLE_RESIZE": "true"})
    resolution, messages = asyncio.run(first_settings(want_w, want_h))
    after = H.x_root_size()
    res.check("resize enabled: the same SETTINGS resizes the desktop",
              after == (want_w, want_h), f"root {root_w}x{root_h} -> {after[0]}x{after[1]}, wanted {want_w}x{want_h}")
    # Put the shared display back the way it was found.
    asyncio.run(first_settings(root_w, root_h, seconds=6.0))
    res.check("display restored", H.x_root_size() == (root_w, root_h), H.x_root_size())
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
