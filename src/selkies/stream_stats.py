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
rate and cost of the encode, how long a frame took from its capture to the wire
(`send_ms`, the delivery the encode figures cannot see: the event loop, the relay,
the pacer), the rate the video was sent at and its peak against the CBR target, and over
WebSockets the round trip and whether the server is holding frames back (a WebRTC
page measures its own link). It
flows only to a connection that asked for it (the `_stats,1` verb a page sends
while its stats are on screen, `_stats,0` when they leave), so a session nobody
inspects sends nothing periodic. A shared viewer is sent neither message.

A pixelflux build without the report leaves `stream_info` unsent and the encode
figures out of `stream_stats`; nothing else depends on them.
"""

import asyncio
import collections
import glob
import logging
import os
import time
from typing import Any, Awaitable, Callable, Deque, Dict, List, Optional

from .settings import canonical_encoder, codec_for_encoder, settings, software_video_path

logger = logging.getLogger("stats")

STATS_VERB = "_stats"
# How long after a capture start its description is first read, and how often after that.
SETTLE_S = 1.0
WATCH_S = 3.0
# Two counter reads further apart than this span a time nobody watched, not a rate.
RATE_WINDOW_MAX_S = 5.0
# Two closer than this hold too few frames to be one: a rate needs frames to count.
RATE_WINDOW_MIN_S = 0.25
# The bytes sent are counted in buckets this long, and the stream's peak rate is the
# fullest of them over the last PEAK_WINDOW_S: a key frame's burst, which a second's
# average spreads out of sight.
PEAK_BUCKET_NS = 250_000_000
PEAK_WINDOW_S = 10.0


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


def hardware_expected(encoder: str, use_cpu: bool, gpu: bool,
                      backends: Optional[Dict[str, Dict[str, Any]]] = None) -> bool:
    """Whether a session with this encoder should be encoding on a GPU: there is one,
    it has an engine for the codec, and the session asked for it, which JPEG and the
    striped encoder never do and any other does unless software encoding is on.

    Args:
        backends: The startup probe's `AppSettings.encoder_backends`. A codec the
            probe found no hardware backend for is encoded in software by the codec's
            choice, as a server with no GPU is; None, a probe that has not run,
            leaves the engine assumed.
    """
    if not gpu or canonical_encoder(encoder) == "jpeg" or software_video_path(encoder, use_cpu):
        return False
    return backends is None or bool((backends.get(codec_for_encoder(encoder)) or {}).get("hardware"))


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
        # The window's extremes: pixelflux's, merged over every read since the
        # window began, and the frames sent: count, total, least, most, in ms.
        self._peaks: Dict[str, int] = {}
        self._sends: Optional[List[float]] = None
        self._buckets: Deque[List[int]] = collections.deque()

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
        self._restart_window()

    def note_send(self, capture_ns: int, size: int) -> None:
        """One frame on its way to the display's controller: `size` bytes captured at
        `capture_ns` (`time.monotonic_ns()`, zero where the capture stamped none),
        noted when the bytes are handed to the socket over WebSockets and when the
        frame's last packet leaves the pacer over WebRTC. Runs on the event loop."""
        now = time.monotonic_ns()
        if capture_ns > 0:
            ms = (now - capture_ns) / 1e6
            sends = self._sends
            if sends is None:
                self._sends = [1, ms, ms, ms]
            else:
                sends[0] += 1
                sends[1] += ms
                if ms < sends[2]:
                    sends[2] = ms
                if ms > sends[3]:
                    sends[3] = ms
        start = now - now % PEAK_BUCKET_NS
        buckets = self._buckets
        if buckets and buckets[-1][0] == start:
            buckets[-1][1] += size
            return
        buckets.append([start, size])
        while buckets[0][0] <= start - PEAK_WINDOW_S * 1e9:
            buckets.popleft()

    def _restart_window(self) -> None:
        self._peaks = {}
        self._sends = None

    def _merge_peaks(self, totals: Dict[str, int]) -> None:
        """Fold one read's extremes into the window's: pixelflux starts its own again
        on every read, so a read that does not end the window must keep them."""
        peaks = self._peaks
        low = totals.get("pipeline_min_ns", 0)
        if low and (not peaks.get("pipeline_min_ns") or low < peaks["pipeline_min_ns"]):
            peaks["pipeline_min_ns"] = low
        for key in ("pipeline_max_ns", "frame_max_bytes"):
            if totals.get(key, 0) > peaks.get(key, 0):
                peaks[key] = totals[key]

    def sent_mbps(self) -> Dict[str, float]:
        """The video sent over the last `PEAK_WINDOW_S`, in Mbit/s: `video_mbps` on
        average, over as much of the window as the stream has run, at least a second,
        and `peak_mbps`, its fullest `PEAK_BUCKET_NS`. Video alone, which is what the
        encoder's rate is set for and the page's received figure, audio and all, is
        not. Empty where nothing was sent in it."""
        now = time.monotonic_ns()
        recent = [b for b in self._buckets if b[0] > now - PEAK_WINDOW_S * 1e9]
        if not recent:
            return {}
        bits = sum(b[1] for b in recent) * 8
        span = max(1.0, min(PEAK_WINDOW_S, (now - recent[0][0]) / 1e9))
        return {"video_mbps": round(bits / span / 1e6, 2),
                "peak_mbps": round(max(b[1] for b in recent) * 8 / (PEAK_BUCKET_NS / 1e9) / 1e6, 2)}

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
            info["hardware_expected"] = hardware_expected(encoder, use_cpu, gpu, settings.encoder_backends())
            if info == self.info:
                continue
            self.info = info
            logger.debug(f"Display '{self.display_id}' stream description: {info}")
            await self._publish(self.display_id, info)

    def rates(self) -> Dict[str, float]:
        """The encode's rate and cost since the last call: frames a second, the
        milliseconds a frame spent encoding and from capture to the end of its
        encode, on average and at the extremes, the largest frame, and the
        milliseconds from capture to the wire (`note_send`). Empty without a
        capture that counts, on the first call after a start or a spell
        unwatched, which only takes the baseline, and within `RATE_WINDOW_MIN_S`
        of the last call, which leaves the window growing."""
        module = self._module
        if module is None or not hasattr(module, "stream_stats"):
            return {}
        try:
            totals = module.stream_stats()
        except Exception:
            return {}
        if totals:
            self._merge_peaks(totals)
        now = time.monotonic()
        last, last_at = self._totals, self._totals_at
        elapsed = now - last_at
        if totals and last and totals["frames"] >= last["frames"] and elapsed < RATE_WINDOW_MIN_S:
            return {}
        self._totals, self._totals_at = totals, now
        peaks, sends = self._peaks, self._sends
        self._restart_window()
        if not totals or not last or totals["frames"] < last["frames"] or not 0 < elapsed <= RATE_WINDOW_MAX_S:
            return {}
        frames = totals["frames"] - last["frames"]
        rates = {"encoded_fps": round(frames / elapsed, 1)}
        if frames:
            rates["encode_ms"] = round((totals["encode_ns"] - last["encode_ns"]) / frames / 1e6, 2)
            rates["pipeline_ms"] = round((totals["pipeline_ns"] - last["pipeline_ns"]) / frames / 1e6, 2)
            if peaks.get("pipeline_max_ns"):
                rates["pipeline_min_ms"] = round(peaks.get("pipeline_min_ns", 0) / 1e6, 2)
                rates["pipeline_max_ms"] = round(peaks["pipeline_max_ns"] / 1e6, 2)
            if peaks.get("frame_max_bytes"):
                rates["frame_max_kb"] = round(peaks["frame_max_bytes"] / 1000, 1)
        if sends:
            rates["send_ms"] = round(sends[1] / sends[0], 2)
            rates["send_min_ms"] = round(sends[2], 2)
            rates["send_max_ms"] = round(sends[3], 2)
        rates.update(self.sent_mbps())
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
