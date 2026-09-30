#!/usr/bin/env python3
"""Bundled DTLS records reach an established WebRTC data channel immediately.

The sender combines two complete authenticated application records into one
loopback UDP datagram and holds any later application datagrams. The receiver
must deliver both messages without a retransmission or another application
record waking its DTLS reader. ICE, DTLS, and SCTP otherwise run unchanged.
"""
import asyncio
import os
import sys
from unittest.mock import patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
from selkies.webrtc import RTCConfiguration, RTCPeerConnection
from unit.test_dtls_records import records


async def scenario(res: H.Results) -> None:
    """Exercise the normal connected data-channel delivery path on loopback."""
    left = RTCPeerConnection(RTCConfiguration(iceServers=[]))
    right = RTCPeerConnection(RTCConfiguration(iceServers=[]))
    opened, complete = asyncio.Event(), asyncio.Event()
    received = []
    expected = ["first-synthetic-input", "second-synthetic-input"]
    held = []
    bundles, later = 0, 0
    channel = left.createDataChannel("input")
    channel.on("open", opened.set)

    @right.on("datachannel")
    def connected(remote) -> None:
        """Observe exact ordered fixture messages without recording contents."""
        @remote.on("message")
        def message(data: str) -> None:
            received.append(data)
            if len(received) >= 2:
                complete.set()

    try:
        with patch("selkies.ice.ice.get_host_addresses", return_value=["127.0.0.1"]):
            await left.setLocalDescription(await left.createOffer())
            await right.setRemoteDescription(left.localDescription)
            await right.setLocalDescription(await right.createAnswer())
            await left.setRemoteDescription(right.localDescription)
        await asyncio.wait_for(opened.wait(), 10)
        original = left.sctp.transport.transport._send

        async def coalesce(data: bytes) -> None:
            """Send two authenticated records once, retaining later records."""
            nonlocal bundles, later
            if data and data[0] == 23:
                parsed = records(data)
                assert len(parsed) == 1 and parsed[0]["kind"] == 23
                if bundles:
                    later += 1
                    return
                held.append(data)
                if len(held) == 2:
                    bundled = b"".join(held)
                    assert len(bundled) <= 1200
                    bundles += 1
                    await original(bundled)
                return
            await original(data)

        left.sctp.transport.transport._send = coalesce
        for message in expected:
            channel.send(message)
        try:
            await asyncio.wait_for(complete.wait(), 2)
        except asyncio.TimeoutError:
            pass
        res.check("one datagram contains two authenticated application records", bundles == 1 and len(held) == 2)
        res.check("both input messages arrive before any later application datagram", received == expected,
                  dict(delivered=len(received), later_datagrams_held=later))
        left.sctp.transport.transport._send = original
    finally:
        await asyncio.gather(left.close(), right.close())


def main() -> bool:
    res = H.Results("dtls-record-delivery")
    asyncio.run(asyncio.wait_for(scenario(res), 20))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
