# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Process-wide audio-encoding knobs.

The server resolves its audio settings once and publishes them here; the
vendored WebRTC codec classes (constructed deep inside the codec factory,
where no settings object reaches) read them at construction time. This module
stays dependency-free so the WebSocket-only path can publish values without
importing the WebRTC stack.
"""

from typing import Optional

# RED redundancy depth for RedOpusEncoder: None = the encoder's own default,
# 0 = primary-only (plain Opus), N = carry N redundant blocks per packet.
red_distance: Optional[int] = None


def set_red_distance(distance: int) -> None:
    """Publish the RED redundancy depth, clamped to non-negative."""
    global red_distance
    red_distance = max(0, int(distance))
