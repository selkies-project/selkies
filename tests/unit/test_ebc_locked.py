#!/usr/bin/env python3
"""The `_ebc` verb against the operator's binary-clipboard policy.

A controller's page toggles binary clipboard formats with `_ebc`, which reaches
the input handler through the dispatch both transports share rather than
through a SETTINGS payload. It has to meet the same rule those payloads do: a
setting the operator locked keeps the operator's value, and an unlocked one
follows the page.

Drives the message dispatch in a child interpreter per policy; no browser, no
server, no display.
"""
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

SRC = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src")
CHILD = """
import asyncio, os, sys
sys.path.insert(0, %r)
from selkies.input_handler import WebRTCInput

async def main():
    handler = object.__new__(WebRTCInput)
    handler.enable_binary_clipboard = "false"
    async def update(enabled):
        handler.enable_binary_clipboard = "true" if enabled else "false"
    handler.update_binary_clipboard_setting = update
    pending = []
    handler._spawn_task = lambda coro: pending.append(asyncio.ensure_future(coro))
    await handler.on_message("_ebc,true", "primary")
    await asyncio.gather(*pending)
    print(handler.enable_binary_clipboard)

asyncio.run(main())
""" % SRC


def gate_after_ebc_true(policy: str) -> str:
    env = {**os.environ, "SELKIES_ENABLE_BINARY_CLIPBOARD": policy}
    out = subprocess.run([sys.executable, "-c", CHILD], env=env, capture_output=True, text=True, timeout=60)
    return (out.stdout.strip().splitlines() or [out.stderr.strip()[-200:]])[-1]


def main() -> "H.Results":
    res = H.Results("ebc-locked")
    got = gate_after_ebc_true("false|locked")
    res.check("a locked-off binary clipboard stays off when a page asks for it", got == "false", got)
    got = gate_after_ebc_true("false")
    res.check("an unlocked one follows the page", got == "true", got)
    return res


if __name__ == "__main__":
    sys.exit(0 if main().summary() else 1)
