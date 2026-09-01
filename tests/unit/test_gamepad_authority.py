#!/usr/bin/env python3
"""A client drives the gamepad slot it holds and no other.

The gamepad index is a field of the client's own message, so nothing about it
is trustworthy until the connection is consulted. The rule is one shared
policy (`gamepad_slot_denied`) applied by both transports: a viewer holding
player slot N drives index N-1 alone, a `#shared` viewer holding no slot drives
none, a master token's provisioned slot governs every role, and a legacy
controller — which already holds keyboard and mouse — stays unrestricted. The
websockets handshake gives a legacy controller no slot while the signaling
HELLO has it claim slot 1 as its registry identity, so the WebRTC gate is
checked to govern the same client the same way on both transports.
"""
import asyncio
import os
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)

# The server modules read their settings at import; keep the shell's out of it.
for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]
os.environ["SELKIES_FILE_MANAGER_PATH"] = tempfile.mkdtemp(prefix="selkies-gp-auth-")

import helpers as H  # noqa: E402

from selkies.input_handler import gamepad_slot_denied  # noqa: E402
import selkies.selkies as S  # noqa: E402
from selkies.rtc import ClientType, RTCApp  # noqa: E402
from selkies.settings import settings as app_settings  # noqa: E402

CONNECT = "js,c,{},UFJPQkU=,6,17"
BUTTON = "js,b,{},0,1"
DISCONNECT = "js,d,{}"


def policy(res: H.Results) -> None:
    """The shared rule, over every (role, slot, index) the transports produce."""
    res.check("a shared viewer holding no slot drives no gamepad",
              all(gamepad_slot_denied(m.format(i), "viewer", None, False)
                  for m in (CONNECT, BUTTON, DISCONNECT) for i in range(4)))
    res.check("a viewer holding slot 2 drives index 1",
              not any(gamepad_slot_denied(m.format(1), "viewer", 2, False)
                      for m in (CONNECT, BUTTON, DISCONNECT)))
    res.check("a viewer holding slot 2 drives no other index",
              all(gamepad_slot_denied(BUTTON.format(i), "viewer", 2, False)
                  for i in (0, 2, 3)))
    res.check("a viewer's connect cannot claim another slot's association",
              gamepad_slot_denied(CONNECT.format(0), "viewer", 2, False))
    res.check("a viewer's disconnect cannot reset another slot's pad",
              gamepad_slot_denied(DISCONNECT.format(0), "viewer", 2, False))
    res.check("a legacy controller is unrestricted",
              not any(gamepad_slot_denied(BUTTON.format(i), "controller", None, False)
                      for i in range(4)))
    res.check("a token with no slot drives no gamepad whatever its role",
              gamepad_slot_denied(BUTTON.format(0), "controller", None, True)
              and gamepad_slot_denied(BUTTON.format(0), "viewer", None, True))
    res.check("a token's slot governs a controller too",
              gamepad_slot_denied(BUTTON.format(1), "controller", 1, True)
              and not gamepad_slot_denied(BUTTON.format(0), "controller", 1, True))
    res.check("a non-gamepad message is not this gate's business",
              not gamepad_slot_denied("kd,65", "viewer", None, True))
    res.check("a malformed or absent index is refused, not read as slot 0",
              gamepad_slot_denied("js,b,x,0,1", "viewer", 2, False)
              and gamepad_slot_denied("js,b", "viewer", 2, False))
    # int() accepts surrounding whitespace and a sign, so the comparison has to
    # hold for a spelling of the index the client chose rather than its digits.
    res.check("an index spelled oddly is compared by value",
              not gamepad_slot_denied("js,b, 01 ,0,1", "viewer", 2, False))


async def webrtc(res: H.Results) -> None:
    """The WebRTC gate, whose slot comes from the signaling claim or the token."""
    loop = asyncio.get_running_loop()
    app = RTCApp(async_event_loop=loop, encoder="h264enc", stun_servers=[], turn_servers=[])

    res.check("WebRTC: a strict viewer (slot -1) drives no gamepad",
              app._gamepad_denied(BUTTON.format(0), ClientType.VIEWER, None, -1))
    res.check("WebRTC: a player-2 viewer drives index 1 alone",
              not app._gamepad_denied(BUTTON.format(1), ClientType.VIEWER, None, 2)
              and app._gamepad_denied(BUTTON.format(0), ClientType.VIEWER, None, 2))
    # The websockets handshake gives a legacy controller no slot at all; the
    # signaling HELLO has it claim 1. Same client, same role, same verdict.
    res.check("WebRTC: a legacy controller's claimed slot 1 does not pin it",
              not any(app._gamepad_denied(BUTTON.format(i), ClientType.CONTROLLER, None, 1)
                      for i in range(4)))

    tokens_before, mk_before = S.user_tokens, S.active_mk_token
    master_before = app_settings.master_token
    try:
        app_settings.master_token = "unit-master"
        S.user_tokens = {"tok-p2": {"role": "viewer", "slot": 2},
                         "tok-none": {"role": "controller", "slot": None}}
        res.check("WebRTC secure: the token's slot wins over the claim",
                  app._gamepad_denied(BUTTON.format(0), ClientType.VIEWER, "tok-p2", 1)
                  and not app._gamepad_denied(BUTTON.format(1), ClientType.VIEWER, "tok-p2", 1))
        res.check("WebRTC secure: a slotless token drives no gamepad",
                  app._gamepad_denied(BUTTON.format(0), ClientType.CONTROLLER, "tok-none", 1))
        res.check("WebRTC secure: an unknown token drives no gamepad",
                  app._gamepad_denied(BUTTON.format(0), ClientType.VIEWER, "gone", 2))
        # Read per message, not held from connect, so a re-slot lands at once.
        S.user_tokens["tok-p2"] = {"role": "viewer", "slot": 3}
        res.check("WebRTC secure: a re-slotted token takes effect on the next message",
                  app._gamepad_denied(BUTTON.format(1), ClientType.VIEWER, "tok-p2", 1)
                  and not app._gamepad_denied(BUTTON.format(2), ClientType.VIEWER, "tok-p2", 1))
    finally:
        S.user_tokens, S.active_mk_token = tokens_before, mk_before
        app_settings.master_token = master_before


def run() -> H.Results:
    res = H.Results("gamepad-authority")
    policy(res)
    asyncio.run(webrtc(res))
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not run().failed() else 1)
