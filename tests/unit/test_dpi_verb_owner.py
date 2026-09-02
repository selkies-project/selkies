#!/usr/bin/env python3
"""The DPI sync verb from a secondary display is refused at the shared dispatch.

One desktop carries one DPI and the primary display's page owns it (the
SETTINGS payload already says so on both transports). A page also re-derives
its DPI from a live device pixel ratio change -- its window restored on a
screen of another density -- and the WebRTC core sends that as the bare `s`
verb over the data channel, which reaches the desktop through the input
dispatch alone. A secondary's has to stop there too, or two windows on screens
of different densities rescale the whole session at every restore.

Drives the message dispatch directly; no browser, no server, no display.
"""
import asyncio
import logging
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.input_handler import WebRTCInput

REFUSED = "the desktop DPI follows the primary display"


class Log(logging.Handler):
    def __init__(self) -> None:
        super().__init__()
        self.lines: list = []

    def emit(self, record: logging.LogRecord) -> None:
        self.lines.append(record.getMessage())


async def scenario(res: "H.Results") -> None:
    handler = object.__new__(WebRTCInput)
    applied: list = []
    handler.on_scaling_ratio = lambda dpi: applied.append(dpi)
    log = Log()
    logger = logging.getLogger("webrtc_input")
    logger.addHandler(log)
    logger.setLevel(logging.DEBUG)
    try:
        await handler.on_message("s,192", "primary")
        res.check("the primary's sync reaches the desktop", applied == [192.0], applied)

        await handler.on_message("s,96", "display2")
        res.check("a secondary's sync does not", applied == [192.0], applied)
        res.check("and it says so",
                  any(REFUSED in line and "'display2'" in line for line in log.lines),
                  log.lines[-1:])

        await handler.on_message("s,96.5", "primary")
        res.check("a fractional sync from the primary still applies",
                  applied == [192.0, 96.5], applied)

        await handler.on_message("s,abc", "primary")
        res.check("a malformed sync is rejected before anything looks at who sent it",
                  applied == [192.0, 96.5] and
                  any("Rejecting scaling change" in line for line in log.lines), applied)
    finally:
        logger.removeHandler(log)


def main() -> "H.Results":
    res = H.Results("dpi-verb-owner")
    asyncio.run(scenario(res))
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
