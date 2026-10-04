#!/usr/bin/env python3
"""The 4:4:4 and 10-bit capabilities a client's hello carries through SESSION_START.

The signaling relay appends `fullcolor=<codec,...>` and `tenbit=<format,...>` after the
optional client token; the server's signaling client reads them into `fullcolor_codecs`
and `tenbit_codecs`, keeps the token apart from both, and leaves a field `None` for a
client that said nothing of it, in every line length the protocol allows. The page
size a hello names is two whole numbers or nothing.
"""
import asyncio
import os
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))

from selkies.webrtc_signaling_client import WebRTCSignalingClient  # noqa: E402


def started(line: str) -> dict:
    client = WebRTCSignalingClient("ws://localhost:1/signaling")
    seen = {}

    async def on_session_start(peer_id, client_type, client_token, display_id, display_position,
                               fullcolor_codecs=None, tenbit_codecs=None):
        seen.update(peer_id=peer_id, client_type=client_type, client_token=client_token,
                    display_id=display_id, display_position=display_position,
                    fullcolor_codecs=fullcolor_codecs, tenbit_codecs=tenbit_codecs)

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


def test_ten_bit_beside_full_color() -> None:
    seen = started("SESSION_START p1 controller primary right tok fullcolor=h264 tenbit=av1,vp9,vp9444")
    assert seen["client_token"] == "tok"
    assert seen["fullcolor_codecs"] == ["h264"]
    assert seen["tenbit_codecs"] == ["av1", "vp9", "vp9444"]
    seen = started("SESSION_START p1 controller primary right tenbit=")
    assert seen["client_token"] is None
    assert seen["fullcolor_codecs"] is None and seen["tenbit_codecs"] == []
    assert started("SESSION_START p1 viewer primary right tok")["tenbit_codecs"] is None


def test_silence_is_none() -> None:
    assert started("SESSION_START p1 viewer primary right tok")["fullcolor_codecs"] is None
    assert started("SESSION_START p1 viewer primary right")["fullcolor_codecs"] is None
    legacy = started("SESSION_START p1 viewer")
    assert legacy["fullcolor_codecs"] is None and legacy["display_id"] == "primary"


def test_page_size() -> None:
    from selkies.webrtc_signaling_server import hello_page_size
    assert hello_page_size([1280, 720]) == (1280, 720)
    for bad in (None, [1280], [1280, 720, 1], [0, 720], [1280, 9000], [1280.0, 720], ["1280", "720"],
                [True, 720], "1280x720"):
        assert hello_page_size(bad) is None, bad


if __name__ == "__main__":
    test_capability_beside_the_token()
    test_capability_without_a_token()
    test_ten_bit_beside_full_color()
    test_silence_is_none()
    test_page_size()
    print("ok")

