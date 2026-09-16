# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""The one place the server's logging is configured.

Every Selkies module logs through a short logger name that says which part
of the server spoke (`main`, `server`, `websockets`, `webrtc`, `signaling`,
`display`, `input`, `gamepad`, `audio`, `printing`, `webcam`, `stats`,
`audit`), rendered as `LEVEL:name:message` like pixelflux's own bracketed
`[X11]`/`[Wayland]` lines beside them. INFO is the story of a session as
an operator reads it back from a user: the settings the server came up
with, what a client connected as, the capture and encoder path each display
took, and every later change or failure; the mechanics behind those events
(each reconfiguration step, each broadcast, each task) are DEBUG. Debug mode
(`--debug`) opens DEBUG on every Selkies logger, and the same flag reaches
pixelflux as `debug_logging`.

Two sources would bury a debug run on their own and are held back even
then. Pillow logs every PNG chunk it writes at DEBUG, and the cursor path
encodes a PNG per cursor change, so the `PIL` logger is pinned at WARNING.
The vendored WebRTC and ICE stacks log every STUN check, DTLS record and
state change; `PacedFilter` on the root handler lets the first record of
each message template through and then one per `PACE_PERIOD_S` seconds,
carrying the count of the records it held back, while the per-packet RTP
and SCTP loggers stay at INFO outright.
"""

import logging
import time
from typing import Dict, Tuple

PACED_PREFIXES = ("selkies.webrtc.", "selkies.ice.")
PACE_PERIOD_S = 5.0
_PACKET_LOGGERS = (
    "selkies.webrtc.rtcrtpsender",
    "selkies.webrtc.rtcrtpreceiver",
    "selkies.webrtc.rtcsctptransport",
)


class PacedFilter(logging.Filter):
    """Rate-limit DEBUG records from the chatty vendored stacks by template.

    Records whose logger name starts with one of `PACED_PREFIXES` pass
    the first time their message template is seen and then once per
    `period` seconds; a record that passes after held-back repeats says
    how many it stands for. Every other record, and every record at INFO
    or above, passes untouched.
    """

    def __init__(self, period: float = PACE_PERIOD_S) -> None:
        super().__init__()
        self.period = period
        self._seen: Dict[Tuple[str, str], Tuple[float, int]] = {}

    def filter(self, record: logging.LogRecord) -> bool:
        if record.levelno >= logging.INFO or not record.name.startswith(PACED_PREFIXES):
            return True
        key = (record.name, str(record.msg))
        now = time.monotonic()
        last, held = self._seen.get(key, (None, 0))
        if last is not None and now - last < self.period:
            self._seen[key] = (last, held + 1)
            return False
        self._seen[key] = (now, 0)
        if held:
            record.msg = f"{record.msg} (+{held} alike in the last {self.period:.0f}s)"
        return True


def configure_logging(debug: bool) -> None:
    """Install the root handler and the level policy for this process.

    Args:
        debug: Open DEBUG on every Selkies logger; INFO otherwise.
    """
    logging.basicConfig(level=logging.DEBUG if debug else logging.INFO, force=True)
    for name in ("websockets", "aiohttp", "PIL", "pulsectl_asyncio"):
        logging.getLogger(name).setLevel(logging.WARNING)
    if not debug:
        return
    for name in _PACKET_LOGGERS:
        logging.getLogger(name).setLevel(logging.INFO)
    for handler in logging.getLogger().handlers:
        handler.addFilter(PacedFilter())
