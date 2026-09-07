#!/usr/bin/env python3
"""The 4:4:4 capability a client's hello carries through SESSION_START.

The signalling relay appends `fullcolor=<codec,...>` after the optional client token; the
server's signalling client reads it into `fullcolor_codecs`, keeps the token apart from it,
and leaves the field `None` for a client that said nothing, in every line length the
protocol allows.
"""
import asyncio
import os
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))

from selkies.webrtc_signaling import WebRTCSignalingClient  # noqa: E402


def started(line: str) -> dict:
    client = WebRTCSignalingClient("ws://localhost:1/signaling")
    seen = {}

    async def on_session_start(peer_id, client_type, client_token, display_id, display_position,
                               fullcolor_codecs=None):
        seen.update(peer_id=peer_id, client_type=client_type, client_token=client_token,
                    display_id=display_id, display_position=display_position,
                    fullcolor_codecs=fullcolor_codecs)

    client.on_session_start = on_session_start
    asyncio.run(client._process_message(line))
    return seen


def test_capability_beside_the_token() -> None:
    seen = started("SESSION_START p1 controller primary right tok fullcolor=h264,vp9")
    assert seen["client_token"] == "tok"
    assert seen["fullcolor_codecs"] == ["h264", "vp9"]
    assert (seen["display_id"], seen["display_position"]) == ("primary", "right")


def test_capability_without_a_token() -> None:
    seen = started("SESSION_START p1 controller second left fullcolor=")
    assert seen["client_token"] is None
    assert seen["fullcolor_codecs"] == []


def test_silence_is_none() -> None:
    assert started("SESSION_START p1 viewer primary right tok")["fullcolor_codecs"] is None
    assert started("SESSION_START p1 viewer primary right")["fullcolor_codecs"] is None
    legacy = started("SESSION_START p1 viewer")
    assert legacy["fullcolor_codecs"] is None and legacy["display_id"] == "primary"


if __name__ == "__main__":
    test_capability_beside_the_token()
    test_capability_without_a_token()
    test_silence_is_none()
    print("ok")
