#!/usr/bin/env python3
"""Malformed or premature SCTP packets from a peer are dropped.

Each packet reaches the association the way the DTLS transport hands it over;
an exception escaping there ends the DTLS receive loop, and a loop that never
returns stalls the event loop for every client. After each, a HEARTBEAT must
still be answered.
"""
import asyncio
import os
import struct
import sys
import threading
import types

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
from google_crc32c import value as crc32c
from selkies.webrtc import rtcsctptransport as sctp


class Dtls:
    state = "connected"
    transport = types.SimpleNamespace(role="controlled")

    def __init__(self) -> None:
        self.sent: list = []

    def _register_data_receiver(self, receiver) -> None:
        pass

    def _unregister_data_receiver(self, receiver) -> None:
        pass

    async def _send_data(self, data: bytes) -> None:
        self.sent.append(data)


def packet(tag: int, chunks: bytes) -> bytes:
    header = struct.pack("!HHL", 5000, 5000, tag)
    return header + struct.pack("<L", crc32c(header + b"\0\0\0\0" + chunks)) + chunks


def chunk(kind: int, body: bytes) -> bytes:
    return struct.pack("!BBH", kind, 0, 4 + len(body)) + body + b"\0" * (-len(body) % 4)


async def association(established: bool) -> tuple:
    dtls = Dtls()
    assoc = sctp.RTCSctpTransport(dtls, port=5000)
    await assoc.start(None, 5000)
    if established:
        init = struct.pack("!LLHHL", 1, 131072, 16, 16, 1000)
        await assoc._handle_data(packet(0, chunk(1, init)))
        cookie = dict(sctp.parse_packet(dtls.sent[-1])[3][0].params)[sctp.SCTP_STATE_COOKIE]
        await assoc._handle_data(packet(assoc._local_verification_tag, chunk(10, cookie)))
    return assoc, dtls


async def answers_heartbeat(assoc, dtls) -> bool:
    before = len(dtls.sent)
    await assoc._handle_data(packet(assoc._local_verification_tag, chunk(4, struct.pack("!HH", 1, 8) + b"live")))
    return any(isinstance(c, sctp.HeartbeatAckChunk)
               for data in dtls.sent[before:] for c in sctp.parse_packet(data)[3])


def deliver(established: bool, raw: bytes) -> tuple:
    """Hand one packet to a fresh association in a thread of its own, so a
    loop that never returns fails the check instead of the suite."""
    outcome: dict = {}

    async def run() -> None:
        assoc, dtls = await association(established)
        state = assoc._association_state
        await assoc._handle_data(raw(assoc) if callable(raw) else raw)
        outcome.update(state=assoc._association_state == state, alive=await answers_heartbeat(assoc, dtls),
                       sent=dtls.sent)

    def target() -> None:
        try:
            asyncio.run(run())
        except Exception as exc:
            outcome["error"] = f"{type(exc).__name__}: {exc}"

    thread = threading.Thread(target=target, daemon=True)
    thread.start()
    thread.join(5)
    if thread.is_alive():
        return False, "still running after 5 s", []
    if "error" in outcome:
        return False, outcome["error"], []
    ok = outcome["state"] and outcome["alive"]
    return ok, "dropped, association answers" if ok else outcome, outcome["sent"]


def main() -> bool:
    res = H.Results("sctp-hostile")
    tag = lambda body: lambda assoc: packet(assoc._local_verification_tag, body)  # noqa: E731
    data = lambda tsn: chunk(0, struct.pack("!LHHL", tsn, 0, 0, 51) + b"x")  # noqa: E731

    ok, detail, _ = deliver(False, tag(chunk(0, b"abc")))
    res.check("a DATA chunk shorter than its header is dropped", ok, detail)
    ok, detail, _ = deliver(False, packet(0, chunk(1, struct.pack("!LLHHL", 1, 131072, 16, 16, 1000) + b"\0\x07\0\0")))
    res.check("an INIT parameter claiming zero length is dropped without stalling", ok, detail)
    ok, detail, _ = deliver(False, tag(data(1000)))
    res.check("DATA before INIT named the peer's first TSN is dropped", ok, detail)
    ok, detail, _ = deliver(False, tag(chunk(192, struct.pack("!L", 1000))))
    res.check("FORWARD TSN before INIT is dropped", ok, detail)
    ok, detail, sent = deliver(True, tag(data(1000 + 70000)))
    gaps = [c.gaps for p in sent for c in sctp.parse_packet(p)[3] if isinstance(c, sctp.SackChunk)]
    res.check("DATA past what a gap ack block can name is dropped", ok and not any(gaps), f"SACK gaps {gaps}" if ok else detail)
    ok, detail, _ = deliver(True, tag(chunk(130, struct.pack("!HH", 13, 4 + 13) + struct.pack("!LLLB", 1, 0, 999, 0))))
    res.check("a RECONFIG parameter too short for its fields is dropped", ok, detail)
    whole = lambda tsn, seq, ppid, body: struct.pack("!BBH", 0, 3, 16 + len(body)) + struct.pack(  # noqa: E731
        "!LHHL", tsn, 1, seq, ppid) + body + b"\0" * (-len(body) % 4)
    opening = struct.pack("!BBHLHH", 3, 0, 0, 0, 2, 0) + b"\xff\xcf"
    ok, detail, sent = deliver(True, tag(whole(1000, 0, 50, opening) + whole(1001, 1, 51, b"\xcf\x00\xff")))
    acks = [c for p in sent for c in sctp.parse_packet(p)[3] if isinstance(c, sctp.DataChunk) and c.protocol == 50]
    res.check("a channel's label and a string message not UTF-8 are taken, not raised", ok and len(acks) == 1,
              f"{len(acks)} DATA_CHANNEL_ACK" if ok else detail)
    ok, detail, sent = deliver(True, tag(data(1000)))
    acked = [c.cumulative_tsn for p in sent for c in sctp.parse_packet(p)[3] if isinstance(c, sctp.SackChunk)]
    res.check("in-window DATA is still received and acknowledged", ok and 1000 in acked, f"SACK cumulative TSNs {acked}")
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
