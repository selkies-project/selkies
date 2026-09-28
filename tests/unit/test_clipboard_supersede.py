#!/usr/bin/env python3
"""A newer session copy supersedes one still crossing the link, on both transports.

A client writes its local clipboard only while the user is still there to
allow it, so a copy made while a large one is crossing has to reach it at
once, and the older payload has nothing left to land: it gets no further
chunks, and the client drops a payload whose chunks stop at the next start.
Over WebRTC a small payload used to go out between an older transfer's chunks,
and the client completed the older one over it. A tagged reply to the
connect-time fetch is only cached, so it is neither superseded nor supersedes.
An announcement reaches the controllers alone: a viewer's page never takes the
session's clipboard, so a copy has no business on a viewer's link, while a
viewer's own fetch is still answered. The sockets and channels record what
they are sent; pacing is a short sleep.
"""
import asyncio
import json
import os
import sys
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import webrtc_engine as rte  # noqa: E402
from selkies import websockets_mode as wsm  # noqa: E402

results = []


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [clip-supersede] {label}  {str(detail)[:160]}", flush=True)


class Socket:
    """A client's session socket that takes a moment per frame."""

    closed = False

    def __init__(self) -> None:
        self.frames: list = []

    async def send_str(self, message: str) -> None:
        await asyncio.sleep(0.002)
        self.frames.append(message.split(",", 1)[0])

    async def send_bytes(self, message: bytes) -> None:
        await asyncio.sleep(0.002)
        self.frames.append("bytes")


class Channel:
    """A peer's open data channel, as the clipboard sender uses one."""

    readyState = "open"
    transport = None

    def __init__(self) -> None:
        self.sent: list = []

    def send(self, payload) -> None:
        self.sent.append(json.loads(payload)["type"])


async def no_wait(*_args, **_kwargs) -> None:
    return None


async def drain(*_args, **_kwargs) -> bool:
    await asyncio.sleep(0.002)
    return True


async def websockets_case() -> None:
    wsm._bulk_pace = no_wait
    wsm._await_bulk_window = no_wait
    wsm.socket_gauge = lambda ws: None
    app = wsm.SelkiesStreamingApp.__new__(wsm.SelkiesStreamingApp)
    sock = Socket()
    app.data_streaming_server = SimpleNamespace(clients={sock}, enable_binary_clipboard=True)
    big = bytes(range(256)) * 800
    chunks = -(-len(big) // wsm.CLIPBOARD_CHUNK_SIZE)

    older = asyncio.create_task(app.send_ws_clipboard_data(big, "image/png"))
    await asyncio.sleep(0.005)
    await app.send_ws_clipboard_data(b"newer", "image/png")
    await older
    check("websockets: a newer copy goes out at once and the older one gets no further chunks",
          sock.frames[0] == "clipboard_start" and sock.frames[-1] == "clipboard_binary"
          and "clipboard_finish" not in sock.frames
          and sock.frames.count("clipboard_data") < chunks, sock.frames)

    sock.frames.clear()
    reply = asyncio.create_task(app.send_ws_clipboard_data(big, "image/png", reply_to="cr"))
    await asyncio.sleep(0.005)
    await app.send_ws_clipboard_data(b"newer", "image/png")
    await reply
    check("websockets: a tagged reply completes before the copy that followed it",
          sock.frames[:2] == ["clipboard_reply", "clipboard_start"]
          and sock.frames.count("clipboard_data") == chunks
          and sock.frames[-2:] == ["clipboard_finish", "clipboard_binary"], sock.frames)

    viewer = Socket()
    app.data_streaming_server.clients = {sock, viewer}
    wsm.client_permissions[viewer] = {"role": "viewer"}
    try:
        sock.frames.clear()
        await app.send_ws_clipboard_data("a copy", "text/plain")
        await app.send_ws_clipboard_data("asked for", "text/plain", reply_to="cr", conn_id=id(viewer))
        check("websockets: a viewer is announced no copy, and is answered what it asked",
              sock.frames == ["clipboard"] and viewer.frames == ["clipboard_reply", "clipboard"],
              (sock.frames, viewer.frames))
    finally:
        wsm.client_permissions.pop(viewer, None)


async def webrtc_case() -> None:
    rte.drain_data_channel = drain
    app = rte.RTCApp.__new__(rte.RTCApp)
    channel = Channel()
    app.peer_connections = {"peer": {"peer_conn": SimpleNamespace(connectionState="connected"),
                                     "data_channel": channel,
                                     "client_type": rte.ClientType.CONTROLLER}}
    big = bytes(range(256)) * 800
    chunks = -(-len(big) // rte.get_adjusted_chunk_size(app.peer_connections))

    older = asyncio.create_task(app.send_clipboard_data(big, "image/png"))
    await asyncio.sleep(0.005)
    await app.send_clipboard_data(b"newer", "image/png")
    await older
    check("webrtc: a newer copy is the last thing sent and the older one gets no further chunks",
          channel.sent[0] == "clipboard-msg-start" and channel.sent[-1] == "clipboard-msg"
          and "clipboard-msg-end" not in channel.sent
          and channel.sent.count("clipboard-msg-data") < chunks, channel.sent)

    channel.sent.clear()
    reply = asyncio.create_task(app.send_clipboard_data(big, "image/png", reply_to="cr"))
    await asyncio.sleep(0.005)
    await app.send_clipboard_data(b"newer", "image/png")
    await reply
    check("webrtc: a tagged reply completes before the copy that followed it",
          channel.sent.count("clipboard-msg-data") == chunks
          and channel.sent[-2:] == ["clipboard-msg-end", "clipboard-msg"], channel.sent)

    viewer = Channel()
    app.peer_connections["viewer"] = {"peer_conn": SimpleNamespace(connectionState="connected"),
                                      "data_channel": viewer, "client_type": rte.ClientType.VIEWER}
    channel.sent.clear()
    await app.send_clipboard_data(b"a copy", "image/png")
    await app.send_clipboard_data(b"asked for", "image/png", reply_to="cr", peer_id="viewer")
    check("webrtc: a viewer is announced no copy, and is answered what it asked",
          channel.sent == ["clipboard-msg"] and viewer.sent == ["clipboard-msg"], (channel.sent, viewer.sent))


async def main() -> None:
    await websockets_case()
    await webrtc_case()


asyncio.run(main())
failed = [label for label, ok in results if not ok]
print(f"[clip-supersede] {len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
