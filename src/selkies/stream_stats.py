# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""What a controller's page is told about its stream, the same on both transports.

Two messages carry it. `stream_info` describes the path a display's capture took
-- the capture backend and whether it is zero-copy, the encoder and whether it is
hardware, the GPU behind it, and the reason a faster path was declined -- as
pixelflux reports it (`ScreenCapture.stream_info`), plus `gpu_present`, whether
the server is exposed a GPU at all, and `hardware_expected`, whether the session
asked for hardware on a server that has one: a software session somebody chose,
and one on a host with no GPU, which is an ordinary deployment, are told apart
from one that fell back. It is sent once when the capture settles and again when
it changes, since a session can demote itself mid-stream.

`stream_stats` carries the numbers that move: the host's CPU, memory, and GPU, the
rate and cost of the encode, and over WebSockets the round trip and whether the
server is holding frames back (a WebRTC page measures its own link). It
flows only to a connection that asked for it (the `_stats,1` verb a page sends
while its stats are on screen, `_stats,0` when they leave), so a session nobody
inspects sends nothing periodic. A shared viewer is sent neither message.

A pixelflux build without the report leaves `stream_info` unsent and the encode
figures out of `stream_stats`; nothing else depends on them.
"""

import asyncio
import glob
import logging
import os
import time
from typing import Any, Awaitable, Callable, Dict, Optional

from .settings import canonical_encoder, software_video_path

logger = logging.getLogger("stats")

STATS_VERB = "_stats"
# How long after a capture start its description is first read, and how often after that.
SETTLE_S = 1.0
WATCH_S = 3.0
# Two counter reads further apart than this span a time nobody watched, not a rate.
RATE_WINDOW_MAX_S = 5.0


def stats_request(message: str) -> Optional[bool]:
    """The subscription a `_stats,<0|1>` message asks for, or None for any other message."""
    verb, _, value = message.partition(",")
    if verb != STATS_VERB:
        return None
    return value.strip() == "1"


def gpu_present() -> bool:
    """Whether a GPU is exposed to this host or container: a DRM render node, or the
    NVIDIA control device of a container given the card without its graphics stack,
    where NVENC still encodes. A GPU on the machine that the container was not
    given is, from in here, no GPU."""
    return bool(glob.glob("/dev/dri/renderD*")) or os.path.exists("/dev/nvidiactl")


def hardware_expected(encoder: str, use_cpu: bool, gpu: bool) -> bool:
    """Whether a session with this encoder should be encoding on a GPU: there is one,
    and the session asked for it, which JPEG and the striped encoder never do and any
    other does unless software encoding is on."""
    return gpu and canonical_encoder(encoder) != "jpeg" and not software_video_path(encoder, use_cpu)


class StreamWatch:
    """Follows one display's capture: publishes its description on every change and
    differences its counters for whoever is watching the numbers.

    Attributes:
        info: The description last published, for a page that connects later.
    """

    def __init__(self, display_id: str,
                 publish: Callable[[str, Dict[str, Any]], Awaitable[None]]) -> None:
        self.display_id = display_id
        self.info: Optional[Dict[str, Any]] = None
        self._publish = publish
        self._task: Optional["asyncio.Task[None]"] = None
        self._module: Any = None
        self._totals: Optional[Dict[str, int]] = None
        self._totals_at = 0.0

    def follow(self, module: Any, encoder: str, use_cpu: bool) -> None:
        """Start following `module`, the capture that now serves the display with
        this encoder and software-encoding choice."""
        self.stop()
        if not hasattr(module, "stream_info"):
            return
        self._module = module
        self._task = asyncio.ensure_future(self._watch(module, encoder, use_cpu))

    def stop(self) -> None:
        """Stop following; the last description is forgotten with the capture."""
        if self._task is not None:
            self._task.cancel()
            self._task = None
        self._module = None
        self._totals = None
        self.info = None

    async def _watch(self, module: Any, encoder: str, use_cpu: bool) -> None:
        gpu = gpu_present()
        delay = SETTLE_S
        while True:
            await asyncio.sleep(delay)
            try:
                info = await asyncio.to_thread(module.stream_info)
            except Exception as e:
                logger.debug(f"Stream description of '{self.display_id}' unreadable: {e}")
                return
            if not info or not info.get("encoder"):
                continue
            delay = WATCH_S
            info["gpu_present"] = gpu
            info["hardware_expected"] = hardware_expected(encoder, use_cpu, gpu)
            if info == self.info:
                continue
            self.info = info
            logger.debug(f"Display '{self.display_id}' stream description: {info}")
            await self._publish(self.display_id, info)

    def rates(self) -> Dict[str, float]:
        """The encode's rate and cost since the last call: frames a second, and the
        milliseconds a frame spent encoding and from capture to the end of its
        encode. Empty without a capture that counts, and on the
        first call after a start or a spell unwatched, which only takes the baseline."""
        module = self._module
        if module is None or not hasattr(module, "stream_stats"):
            return {}
        try:
            totals = module.stream_stats()
        except Exception:
            return {}
        now = time.monotonic()
        last, last_at = self._totals, self._totals_at
        self._totals, self._totals_at = totals, now
        elapsed = now - last_at
        if not totals or not last or totals["frames"] < last["frames"] or not 0 < elapsed <= RATE_WINDOW_MAX_S:
            return {}
        frames = totals["frames"] - last["frames"]
        rates = {"encoded_fps": round(frames / elapsed, 1)}
        if frames:
            rates["encode_ms"] = round((totals["encode_ns"] - last["encode_ns"]) / frames / 1e6, 2)
            rates["pipeline_ms"] = round((totals["pipeline_ns"] - last["pipeline_ns"]) / frames / 1e6, 2)
        return rates


def host_stats(monitor: Any) -> Dict[str, Any]:
    """The host half of a `stream_stats` message from a `ResourceMonitor`'s last sample."""
    stats: Dict[str, Any] = {}
    system = getattr(monitor, "system", None)
    if system:
        stats.update(cpu_percent=system["cpu_percent"], mem_total=system["mem_total"],
                     mem_used=system["mem_used"])
    gpu = getattr(monitor, "gpu", None)
    if gpu:
        stats.update(gpu_percent=gpu["gpu_percent"], gpu_mem_total=gpu["memory_total"],
                     gpu_mem_used=gpu["memory_used"])
    return stats
