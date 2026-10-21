# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""HTTP/HTTPS control-plane and static-content server.

Hosts the single aiohttp application every deployment fronts: the ``/api``
control-plane endpoints (status, health, streaming-mode switch, file
upload/browse, metrics), the packaged or user-supplied web frontend, and the
routes each streaming service registers for itself. ``CentralizedStreamServer``
supervises the active streaming service (websockets or webrtc) so only one owns
capture at a time, hot-reloads TLS certificates without dropping the listener,
and serves on either a TCP address/port or a Unix domain socket. On Wayland
deployments it also brings the pixelflux compositor socket up before any
session app starts so early-launched apps find ``WAYLAND_DISPLAY``.

All request handlers run on the event loop; disk-bound work (upload writes,
static-content extraction, metrics generation) is pushed to executor threads so
a slow disk never stalls streaming.
"""

import os
import ssl
import hmac
import errno
import json
import html
import stat
import ipaddress
import time
import shutil
import base64
import inspect
import pathlib
import asyncio
import concurrent.futures
import contextlib
import math
import mimetypes
import logging
import socket
import urllib.parse
import tempfile
import weakref
import zipfile
from collections import deque
from aiohttp import web
from aiohttp.abc import AbstractAccessLogger
from datetime import datetime, timedelta, timezone
from prometheus_client import generate_latest
from typing import Any, Awaitable, Callable, Deque, Dict, Iterator, List, Optional, Set, Tuple
try:
    # pyrefly: ignore[missing-import]
    import importlib_resources as importlib_resources  # pyright: ignore[reportMissingImports]
except ImportError:
    import importlib.resources as importlib_resources

from abc import ABCMeta, abstractmethod
from . import apps_catalog, audit, printing
from .settings import apps_repo_folder


logger = logging.getLogger("server")


# Idle chunked transfers older than this are reaped; generous relative to one
# ≤64 MiB slice, so only a transfer whose client is truly gone expires.
UPLOAD_PART_TTL_SECONDS: int = 3600

# Hidden staging sibling an upload is renamed from, so a destination is only
# ever replaced by a complete file.
UPLOAD_STAGING_PREFIX: str = ".selkies-upload-"

# The most a folder archive's source can hold, in bytes and in entries: the
# pack runs to completion before the first byte is served, so these bound the
# disk and CPU one request can spend ahead of the pacing that throttles the
# network side of the transfer.
ARCHIVE_MAX_SOURCE_BYTES: int = 8 << 30
ARCHIVE_MAX_ENTRIES: int = 100_000

# Folder packs run at most two at a time: each stages a full copy of the
# folder on disk, so an unbounded number of concurrent requests would multiply
# that staging area by however many of them arrive together.
ARCHIVE_PACK_POOL = concurrent.futures.ThreadPoolExecutor(
    max_workers=2, thread_name_prefix="selkies-archive")

# A file at least this large is packed as a zip64 entry, so one that grows
# past the 2 GiB `zipfile` writes into a plain entry while it is read still
# packs; smaller ones stay plain entries, which every unzipper reads.
ARCHIVE_ZIP64_FROM: int = 1 << 30

# A connection that carries no request is closed after this long. AppRunner's
# own default (3630 s) is set to outlast a reverse proxy's pooled connections,
# but lets a peer that connects and sends nothing hold a file descriptor for an
# hour; 75 s is run_app's value and stays above nginx's 60 s upstream timeout.
# aiohttp before 3.14 (3.13.5, the floor on Python 3.9) arms the timer only
# after a response, so there it closes only a connection idle after one.
IDLE_CONNECTION_TIMEOUT_SECONDS: float = 75.0


class PathOnlyAccessLogger(AbstractAccessLogger):
    """aiohttp access log whose request line and Referer carry no query.

    A secure-mode session token rides a query parameter when the page was
    opened with ``?token=``: on the data WebSocket URL, the file-manager
    listing, and the Referer of every request that page makes. The stock
    request-line atom (which logs ``path_qs``) would write it into the access
    log, and so would the Referer, so both are logged without their query. The
    line otherwise has the default shape: remote address, start time, method +
    path + version, status, body size, Referer, and User-Agent.
    """

    @property
    def enabled(self) -> bool:
        return self.logger.isEnabledFor(logging.INFO)

    def log(self, request: web.BaseRequest, response: web.StreamResponse, time: float) -> None:
        try:
            started = datetime.now().astimezone() - timedelta(seconds=time)
            self.logger.info(
                '%s [%s] "%s %s HTTP/%d.%d" %s %s "%s" "%s"',
                request.remote or "-",
                started.strftime("%d/%b/%Y:%H:%M:%S %z"),
                request.method,
                request.path,
                request.version.major,
                request.version.minor,
                response.status,
                response.body_length,
                request.headers.get("Referer", "-").split("?", 1)[0].split("#", 1)[0],
                request.headers.get("User-Agent", "-"),
            )
        except Exception:
            self.logger.exception("Error in logging")


def _scan_directory(path: str, include_parent: bool) -> List[Dict[str, Any]]:
    """One directory's entries, as the file index renders them.

    Raises:
        PermissionError: The directory cannot be read.
    """
    items: List[Dict[str, Any]] = []
    if include_parent:
        items.append({"name": "../", "size": "-", "mtime": "-", "is_dir": True})
    with os.scandir(path) as it:
        for entry in it:
            try:
                stats = entry.stat()
                mtime = datetime.fromtimestamp(stats.st_mtime).strftime(
                    "%Y-%m-%d %H:%M:%S"
                )
                is_dir = entry.is_dir()
            except OSError as e:
                logger.warning(f"Skipping unreadable directory entry {entry.name!r}: {e}")
                continue
            items.append({
                "name": entry.name + ("/" if is_dir else ""),
                "size": f"{stats.st_size / 1024:.1f} KB" if not is_dir else "-",
                "mtime": mtime,
                "is_dir": is_dir,
            })
    return items


# A gauged transfer moves in chunks sized to what its current rate drains in
# this fraction of a second, so each chunk's burst onto the link stays under
# the gauge's own inflation threshold at any rate (rationale in
# `_gauged_chunk_size`).
TRANSFER_CHUNK_DELAY_BUDGET: float = 0.0125
TRANSFER_CHUNK_MIN_BYTES: int = 8 * 1024
TRANSFER_CHUNK_MAX_BYTES: int = 1 << 20

# Below this a transfer is over before a queue could matter, and gauging it
# would only make a small file feel slow.
TRANSFER_MIN_GAUGED_BYTES: int = 4 * 1024 * 1024


# The web client's files whose staleness changes what a page does.
WEB_CODE = (".html", ".js", ".mjs", ".css", ".wasm")


class _AuditedFileResponse(web.FileResponse):
    """A file served by aiohttp, recorded for the audit as `event` once it
    went out: the whole file or the range asked for, with the bytes served. A
    download the client stops before the end raises out of `prepare` and is
    not recorded. With `remove`, a file that went out whole is deleted."""

    def __init__(self, path: pathlib.Path, served: str, event: str = "file.download",
                 remove: bool = False, **kwargs: Any) -> None:
        super().__init__(path, **kwargs)
        self._served = served
        self._event = event
        self._remove = path if remove else None

    async def prepare(self, request: web.BaseRequest) -> Any:
        writer = await super().prepare(request)
        if request.method == "GET" and self.status in (200, 206):
            audit.emit(self._event, filename=self._served,
                       size_bytes=self.content_length or 0, partial=self.status == 206)
            if self._remove is not None and self.status == 200:
                with contextlib.suppress(OSError):
                    os.unlink(self._remove)
        return writer


def _gauged_chunk_size(pacer: "TransferPacer") -> int:
    """The chunk a gauged transfer moves next, sized to the pacer's rate.

    A chunk is a burst the link absorbs at line rate whatever the average the
    bucket paces: an upload's read size is handed back to the client as
    receive window and sent at once, and a download's write lands in the
    socket whole. A fixed size would put several times the gauge's delay
    budget onto a slow link per chunk, and the gauge would read the
    transfer's own bursts as congestion and never let it grow; sizing to
    `TRANSFER_CHUNK_DELAY_BUDGET` of the current rate keeps the burst below
    `UplinkGauge.INFLATION_US` on any link.
    """
    return max(TRANSFER_CHUNK_MIN_BYTES, min(
        int(pacer.rate_bps * TRANSFER_CHUNK_DELAY_BUDGET),
        TRANSFER_CHUNK_MAX_BYTES))


# Session websocket to its uplink gauge state (ping clock + RTT floor).
# Module-level so the transports' loops reach it from `note_pong` without a
# supervisor reference; weak keys tie each entry to its socket's life.
_UPLINK_SESSIONS: "weakref.WeakKeyDictionary[Any, Dict[str, Any]]" = (
    weakref.WeakKeyDictionary())


def _uplink_session_state(ws: Any) -> Dict[str, Any]:
    """The uplink gauge state for one session websocket, created on first use."""
    state = _UPLINK_SESSIONS.get(ws)
    if state is None:
        state = {"pending": {}, "rtt_us": None, "seq": 0, "next_ping": 0,
                 "buckets": deque(maxlen=UplinkGauge.FLOOR_BUCKETS)}
        _UPLINK_SESSIONS[ws] = state
    return state


def uplink_rtt_ms(ws: Any) -> Optional[float]:
    """The round trip the uplink gauge last measured on `ws`, in milliseconds."""
    state = _UPLINK_SESSIONS.get(ws)
    rtt = state.get("rtt_us") if state else None
    return round(rtt / 1000, 1) if rtt else None


def note_pong(ws: Any, data: Any) -> None:
    """Resolve a received PONG frame against ``ws``'s uplink ping clock.

    The transports' message loops call this for every `WSMsgType.PONG` (their
    sockets run with autoping off so the frames reach them). A payload the
    clock did not send — an answer to an aiohttp heartbeat ping, or a peer's
    unsolicited pong — misses the pending map and is ignored. `answered` keeps
    when the newest answered ping was sent: everything written to the socket
    before it has reached the peer.
    """
    state = _UPLINK_SESSIONS.get(ws)
    if state is None:
        return
    try:
        sent = state["pending"].pop(bytes(data or b""), None)
    except (TypeError, ValueError):
        return
    if sent is None:
        return
    state["rtt_us"] = int((time.monotonic() - sent) * 1e6)
    state["seq"] += 1
    state["answered"] = max(state.get("answered", 0.0), sent)


async def ping_behind(ws: Any) -> None:
    """Ping `ws` behind everything written to it so far: its pong says all of that
    reached the peer (`note_pong` keeps when the newest answered ping went out)."""
    state = _uplink_session_state(ws)
    payload = state["next_ping"].to_bytes(8, "big")
    state["next_ping"] += 1
    pending = state["pending"]
    pending[payload] = time.monotonic()
    while len(pending) > UplinkGauge._PENDING_MAX:
        pending.pop(next(iter(pending)))
    await ws.ping(payload)


def _observe_rtt_floor(state: Dict[str, Any], rtt_us: int, now: float) -> int:
    """Fold one RTT sample into a session's floor history and return the
    current floor.

    The floor is the minimum over the last `UplinkGauge.FLOOR_BUCKETS` minute
    buckets (LEDBAT's base-delay history): a floor kept forever turns a route
    change onto a longer path into permanent congestion, while one re-measured
    from scratch mid-transfer would read the standing queue as the base and
    never see congestion again.
    """
    bucket = int(now // UplinkGauge.FLOOR_BUCKET_SECONDS)
    buckets = state["buckets"]
    if buckets and buckets[-1][0] == bucket:
        if rtt_us < buckets[-1][1]:
            buckets[-1][1] = rtt_us
    else:
        buckets.append([bucket, rtt_us])
    return min(b[1] for b in buckets)


class CongestionSteer:
    """The steer of one display's CBR target over congestion ticks, for both
    transports. A tick carries two kinds of evidence: the queue standing on the
    path (the WebSockets backpressure loop measures it from the display's frame
    round trip, `DataStreamingServer._steer_bitrate_to_link`, and WebRTC's loop
    from one-way delay, `RTCDtlsTransport.take_twcc_window`) and the loss the
    receiver reported (WebRTC's loop, from the same transport-wide-cc feedback).

    A standing queue backs the target off on the first tick that shows it: a
    delay verdict fires at a queue that is still small, where waiting a second
    tick lets it grow by the whole overshoot. The rate the path delivered while
    the queue stood is its capacity, so the target drops to HEADROOM of that
    rather than by a blind fraction, and lower still while the queue already
    written ahead drains: at the depth that empties it in DRAIN_S, never under
    DRAIN_FLOOR of the capacity, for as long as that takes at that rate. No cut
    follows until the drain and SETTLE_S more have passed, because the queue the
    cut is emptying reads as a queue meanwhile; cutting again on it would take
    the target to the floor while the path only drains. The target returns to
    HEADROOM of the capacity on the first tick past the drain that reads no
    queue and no loss past LOSS, and that return clears a loss strike: a queue
    the drain left keeps the drain's target through the hold, and the tick
    after the hold backs off again. Without a measured
    delivery the backoff is BACKOFF of the target, held for HOLD_S. A tick whose
    stream offered the path less than APP_LIMITED of the target, and whose
    path delivered at least APP_LIMITED_DELIVERED of it, a still screen's
    trickle, cuts nothing on a queue: what the path delivered is then what the
    stream sent rather than what the path carries, and a cut to it would put
    the next motion at the floor while draining nothing the stream queued. A
    stream the path delivers less of than it offers is filling the queue,
    whatever its target, and backs off.

    Loss backs the target off only on the second lossy tick in a row: one tick
    of a thin stream is too few packets for its loss fraction to mean anything.
    That backoff is BACKOFF of the target, held for HOLD_S, so the recovery does
    not climb straight back onto the loss that caused it.

    A clean tick outside a hold raises the target, by STEP a second while what
    the path delivers is far from the last capacity it showed and by STEP_NEAR
    once it is within NEAR of it, where the next queue waits: probing there
    slowly keeps the overshoot that finds it small. Delivered rates rather than
    targets are compared, since an encoder need not emit what it is asked for
    (a CBR encoder with a small buffer runs under its target, and content it
    cannot compress that far runs over). A path that delivers clear past the
    old capacity with no queue forgets it, since it now carries more. A raise
    also follows HEADROOM of the measured goodput where that is higher, never
    past the ceiling.
    """

    LOSS = 0.10
    BACKOFF = 0.7
    HOLD_S = 2.0
    STEP = 1.15
    STEP_NEAR = 1.05
    NEAR = 0.9
    HEADROOM = 0.85
    DRAIN_S = 1.0
    DRAIN_FLOOR = 0.5
    SETTLE_S = 0.5
    APP_LIMITED = 0.5
    APP_LIMITED_DELIVERED = 0.8

    def __init__(self) -> None:
        self.strikes = 0
        self.hold_until = 0.0
        self.capacity_kbps: Optional[float] = None
        self._cruise_kbps: Optional[float] = None
        self._drain_until = 0.0
        self._tick_at: Optional[float] = None

    def target(self, current: float, ceiling: float, floor: float,
               goodput_bps: float, loss: float, now: float,
               queue_s: Optional[float] = None,
               offered_bps: Optional[float] = None) -> float:
        """The next target in kbps for one tick.

        Args:
            current: The target in force, in kbps.
            ceiling: The configured target, which the steer never exceeds.
            floor: The lowest target allowed.
            goodput_bps: What the path delivered over the tick, or 0 when unmeasured.
            loss: The loss fraction the receiver reported over the tick.
            now: Monotonic time of the tick.
            queue_s: The queue standing on the path through the tick, in seconds:
                0 for a clean tick, None where the transport measures no delay.
            offered_bps: What the stream sent over the tick, or None when
                unmeasured.
        """
        dt = 1.0 if self._tick_at is None else min(max(now - self._tick_at, 0.0), 2.0)
        self._tick_at = now
        drained = (
            self._cruise_kbps is not None
            and now >= self._drain_until
            and not queue_s
            and loss <= self.LOSS
        )
        if drained:
            current, self._cruise_kbps = self._cruise_kbps, None
        if queue_s:
            self.strikes = 0
            limited = (offered_bps is not None
                       and offered_bps < current * 1_000 * self.APP_LIMITED
                       and (goodput_bps <= 0 or offered_bps <= goodput_bps / self.APP_LIMITED_DELIVERED))
            if now >= self.hold_until and not limited:
                current = self._queue_backoff(current, goodput_bps / 1_000, queue_s, now)
            return max(floor, min(ceiling, current))
        if drained:
            self.strikes = 0
            return max(floor, min(ceiling, current))
        if loss > self.LOSS:
            self.strikes += 1
            if self.strikes < 2:
                return max(floor, min(ceiling, current))
            self.strikes = 0
            self._cruise_kbps = None
            self.hold_until = now + self.HOLD_S
            return max(floor, min(ceiling, current * self.BACKOFF))
        self.strikes = 0
        if now < self.hold_until:
            return max(floor, min(ceiling, current))
        step = self.STEP
        capacity = self.capacity_kbps
        delivered = goodput_bps / 1_000
        if capacity is not None:
            if delivered * self.NEAR > capacity:
                self.capacity_kbps = None
            elif delivered >= capacity * self.NEAR:
                step = self.STEP_NEAR
        return max(floor, min(ceiling, max(current * step ** dt, delivered * self.HEADROOM)))

    def _queue_backoff(self, current: float, delivered_kbps: float, queue_s: float, now: float) -> float:
        """The target a standing queue of `queue_s` backs off to, with the drain it plans."""
        if delivered_kbps <= 0:
            self._cruise_kbps = None
            self.hold_until = now + self.HOLD_S
            return current * self.BACKOFF
        self.capacity_kbps = delivered_kbps
        fraction = max(self.DRAIN_FLOOR, self.HEADROOM - queue_s / self.DRAIN_S)
        self._drain_until = now + queue_s / (1.0 - fraction)
        self._cruise_kbps = delivered_kbps * self.HEADROOM
        self.hold_until = self._drain_until + self.SETTLE_S
        return min(current, delivered_kbps * fraction)


class LinkAllowance:
    """The rate (kbps) a display's still-screen cleanup may run at over its path, which pixelflux
    is told (`update_link_rate`): SHARE of the least the path showed over its last TICKS
    congestion ticks that measured it, `ceiling` (the most video_bitrate allows) over a path too
    fast to tell, and 0 before a measurement or while one of no room stands among them. A frame
    the size of the one that measured the path reads as crossing it at once now and then, several
    times what the path carries."""

    SHARE = 0.9
    TICKS = 20

    def __init__(self) -> None:
        self.rates: Deque[float] = deque(maxlen=self.TICKS)
        self.kbps: Optional[int] = None

    def note(self, bps: Optional[float], ceiling: float) -> Optional[int]:
        """Fold in a tick's reading of the path (None where the tick measured none); the
        allowance where it changed, for the encoder, else None."""
        if bps is not None:
            self.rates.append(bps)
        path = min(self.rates, default=None)
        kbps = (0 if not path else int(ceiling) if path == float('inf')
                else max(1, round(path * self.SHARE / 1000)))
        if kbps == self.kbps:
            return None
        self.kbps = kbps
        return kbps


class RateHold:
    """Whether a display's steered rate has held long enough for its page to
    keep: a rate that stays within HOLD_BAND of where it settled for HOLD_S is
    returned by `note` once, and a move out of the band starts over. The page
    sends it back when it next connects (`start_kbps`), so a restarted server
    starts that page's stream at what its path carried rather than at the
    configured rate and the queue a start at the configured rate builds."""

    HOLD_S = 10.0
    HOLD_BAND = 0.05

    def __init__(self) -> None:
        self.kbps: Optional[float] = None
        self.since = 0.0
        self.told = False

    def note(self, kbps: float, now: float) -> Optional[int]:
        """Fold in the rate in force at `now`; the rate to tell the page, or None."""
        if self.kbps is None or abs(kbps - self.kbps) > self.HOLD_BAND * self.kbps:
            self.kbps, self.since, self.told = kbps, now, False
            return None
        if self.told or now - self.since < self.HOLD_S:
            return None
        self.told = True
        return round(self.kbps)


class ConnectionVerdict:
    """Whether one page's connection is poor: the share of its display's
    frames that do not reach it, lost on the way past repair or held back for
    its link, judged over windows of WINDOW_S with the hysteresis of
    Moonlight's connection warning. A window at POOR_SHARE, or at TWICE_SHARE
    after one that was too, makes the verdict poor, and one at OK_SHARE or
    under makes it good again. A window that ends with fewer than MIN_FRAMES
    frames held a still screen or a caret (the capture is damage-gated) and is
    not judged, nor is the first, which a joining page's wait for its key frame
    takes up. `note` returns the verdict only when it changes, which is when
    the page is told."""

    WINDOW_S = 3.0
    POOR_SHARE = 0.30
    TWICE_SHARE = 0.15
    OK_SHARE = 0.05
    MIN_FRAMES = 15

    def __init__(self) -> None:
        self.poor = False
        self._since: Optional[float] = None
        self._frames = 0
        self._missed = 0
        self._judged = 0
        self._share = 0.0

    def note(self, frames: int, missed: int, now: float) -> Optional[bool]:
        """Count `frames` the display produced for the page and `missed` frames
        of those or earlier ones that will not reach it; the new verdict, or None."""
        if self._since is None:
            self._since = now
        self._frames += frames
        self._missed += missed
        if now - self._since < self.WINDOW_S:
            return None
        frames, missed = self._frames, self._missed
        self._since, self._frames, self._missed = now, 0, 0
        if frames < self.MIN_FRAMES:
            return None
        share, previous = min(1.0, missed / frames), self._share
        self._share = share
        self._judged += 1
        if self._judged == 1:
            return None
        if not self.poor and (share >= self.POOR_SHARE
                              or (share >= self.TWICE_SHARE and previous >= self.TWICE_SHARE)):
            self.poor = True
            return True
        if self.poor and share <= self.OK_SHARE:
            self.poor = False
            return False
        return None


def start_kbps(remembered: Any, lo_kbps: float, ceiling_kbps: float) -> Optional[float]:
    """The rate a steered stream starts at from the one its page remembered
    (`RateHold`): within the configured range and under the ceiling, or None
    where the page sent no positive number."""
    try:
        kbps = float(remembered)
    except (TypeError, ValueError):
        return None
    if not kbps > 0 or kbps == float("inf"):
        return None
    return max(lo_kbps, min(ceiling_kbps, kbps))


class UplinkGauge:
    """Congestion verdicts for one client's bulk transfer, timed end to end
    over that client's own session websocket(s).

    Nothing on this side can see where a transfer's queue stands: an
    upload's is in the client's kernel and first hop, a download's in
    whatever buffer sits ahead of the client's downlink — a reverse proxy,
    a tunnel, a bloated modem — and this host's own send queue reads empty
    while any of those absorbs its writes. But every session connection
    from that client (the data WebSocket, or the WebRTC signaling socket)
    crosses the same bottleneck as the transfer, in both directions. So the
    gauge sends a protocol ping every ``PING_INTERVAL`` on each session
    socket and times the pong: the ping queues behind a download on the way
    out and the pong behind an upload on the way back, and the round trip
    inflating past a per-session floor by more than ``INFLATION_US`` is the
    verdict a delay-based scavenger backs off on (LEDBAT's discipline: take
    what the link has spare, yield the moment anything else waits).
    Browsers answer pings in the network process, so no page or client-code
    cooperation is needed. The rejected alternatives: reading flat out to
    measure the client's rate, then settling on a fixed share of it, filled
    the very queue it existed to prevent, latched a one-shot estimate, and
    held idle-screen uploads to half the link; kernel srtt (TCP_INFO) and
    the unsent-queue ioctl on the transfer's own socket time or count only
    the first hop, so any relaying middlebox blinds them — and a fixed
    byte threshold on that queue reads every chunk written faster than the
    hop drains as congestion, which pinned downloads near the rate floor —
    where the pong crosses end to end.

    The end-to-end trip also spans both event loops, so a server too loaded
    to serve the stream reads as congestion and sheds the transfer first —
    load protection for free. ``INFLATION_US`` sits above scheduler jitter
    and below what a session feels: the queue the sawtooth sustains stays
    imperceptible.

    Ping payloads are per-session counters; the pong echoes its ping's
    payload (RFC 6455), so `note_pong` matches replies to send times through
    `_UPLINK_SESSIONS` and stray pongs — aiohttp's own heartbeat's among
    them — fall through unmatched. A ping failure drops that socket; a gauge
    with no sockets left reports not ``alive`` and the caller stops pacing —
    a vanished session leaves nothing to protect.
    """

    PING_INTERVAL: float = 0.2
    INFLATION_US: int = 25_000
    FLOOR_BUCKET_SECONDS: float = 60.0
    FLOOR_BUCKETS: int = 10
    _PENDING_MAX: int = 25

    def __init__(self, conns: List[List[Any]]) -> None:
        """``conns``: ``[websocket, session state, last-consumed seq]`` per
        session connection sharing the uploader's uplink."""
        self._conns = conns
        self._last_ping = 0.0
        self.inflation_us = 0

    @property
    def alive(self) -> bool:
        """Whether any session socket is left to gauge."""
        return bool(self._conns)

    async def sample(self) -> Optional[bool]:
        """Ping on cadence and return one verdict across the gauged sockets.

        Returns:
            True when any session's round trip stands above its floor by more
            than `INFLATION_US`, False when at least one session answered
            fresh and none congested, None when no fresh pong arrived since
            the last call (the caller holds its rate). `inflation_us` keeps
            the largest fresh excess over the floor, the queue's depth in
            time, for the caller to size its step by.
        """
        now = time.monotonic()
        if now - self._last_ping >= self.PING_INTERVAL:
            self._last_ping = now
            for conn in list(self._conns):
                try:
                    await ping_behind(conn[0])
                except Exception:
                    self._conns.remove(conn)
        verdict: Optional[bool] = None
        inflation = 0
        for conn in self._conns:
            _ws, state, last = conn
            if state["seq"] == last or state["rtt_us"] is None:
                continue
            conn[2] = state["seq"]
            rtt = state["rtt_us"]
            floor = _observe_rtt_floor(state, rtt, now)
            congested = rtt > floor + self.INFLATION_US
            inflation = max(inflation, rtt - floor)
            verdict = congested if verdict is None else (verdict or congested)
        if verdict is not None:
            self.inflation_us = inflation
        return verdict


def socket_gauge(ws: Any) -> UplinkGauge:
    """An `UplinkGauge` over one session websocket alone.

    For a sender pacing what it writes to that socket: the pong queues behind
    everything already written to it, so its round trip is what the bytes in
    front of it cost. The uploads' gauge instead spans every session socket of
    one client, because an upload's congestion is on a client uplink those
    sockets share rather than on any one of them.

    Args:
        ws: The session websocket; its ping clock and floor history live in
            `_UPLINK_SESSIONS`, so successive senders share one floor.
    """
    state = _uplink_session_state(ws)
    return UplinkGauge([[ws, state, state["seq"]]])


class TransferPacer:
    """Rate pacing for bulk traffic, one instance per allowance.

    Two modes. A static cap (``static_bps``) exists for links whose rate the
    operator already knows: one such pacer is shared by every file transfer
    on the server, so the link sees one source regardless of how many
    sockets a browser opens. Without one, an adaptive pacer holds a transfer
    inside an allowance that walks up while the path stays clear and down
    the moment it congests, fed `UplinkGauge` verdicts through
    `pace_verdict`: the file downloads share one, the file uploads another
    (their queues stand at opposite ends of the path), and a clipboard
    transfer paces its own against the socket it is writing to. No link
    estimate is needed, and a transfer with no session socket to gauge
    rides the static cap alone rather than being throttled blindly.
    """

    _CHUNK = 256 * 1024
    _RATE_FLOOR = 48 * 1024

    def __init__(self, static_bps: int = 0, adaptive: bool = False) -> None:
        self.static_bps = static_bps
        self.adaptive = adaptive
        self.rate_bps = static_bps or 256 * 1024
        self._tokens = self.rate_bps * 0.5
        self._ts = time.monotonic()
        self._congested = False
        self._probe_ceiling = None
        self._slow_start = True
        self._hold_until = 0.0
        self._last_inflation_us: Optional[int] = None

    @property
    def active(self) -> bool:
        return self.adaptive or self.static_bps > 0

    @property
    def _ceiling(self) -> int:
        """The static cap, or effectively unbounded when purely adaptive."""
        return self.static_bps or 64 * 1024 * 1024 * 1024

    async def pace(self, nbytes: int) -> None:
        """Sleep off what `nbytes` overdraws from the allowance as it stands.

        The static leg: no verdict is folded in, so on a purely adaptive
        pacer this holds the rate the verdicts last set. After a long idle
        gap the remembered rate is stale, so the multiplicative ramp is
        re-entered (TCP's restart after idle): a link that got faster
        meanwhile is rediscovered in chunks, not minutes, and one that got
        slower is cut by the first gauge sample. The deficit is slept off
        here and paid back by the next call's elapsed-time refill; zeroing
        the balance after the sleep would credit the slept interval twice
        and double the delivered rate.
        """
        if not self.active:
            return
        await self._bucket(nbytes)

    async def pace_verdict(self, nbytes: int, congested: Optional[bool],
                           inflation_us: Optional[int] = None) -> None:
        """Adaptive leg: fold one `UplinkGauge` verdict in, then drain.

        ``congested=None`` (no fresh sample) holds the rate and still drains
        the bucket; True/False are one `_gauge_backoff` step, with the
        gauge's measured inflation telling a draining queue from a standing
        one. The cut is gentle — a delay verdict fires at a bounded queue
        where a loss-like signal would mean one already overflowed — and the
        growth step is proportional rather than a fixed 8 KiB: verdicts
        arrive at the gauge's ping cadence, a few per second, and a fixed
        step at that cadence would take minutes to recover a fast link's
        post-cut rate. Together they hold the AIMD sawtooth's duty cycle
        near the line instead of near half of it.
        """
        if not self.active:
            return
        if self.adaptive and congested is not None:
            step = max(8 * 1024, int(self.rate_bps * 0.03))
            self._gauge_backoff(
                congested=congested, clear=not congested, cut=0.65, step=step,
                inflation_us=inflation_us)
        await self._bucket(nbytes)

    async def _bucket(self, nbytes: int) -> None:
        """Drain `nbytes` from the token bucket, sleeping off any overdraw."""
        now = time.monotonic()
        if self.adaptive and now - self._ts > 10:
            self._slow_start = True
        limit = min(self.rate_bps, self._ceiling)
        self._tokens = min(limit * 0.5, self._tokens + (now - self._ts) * limit)
        self._ts = now
        self._tokens -= nbytes
        if self._tokens < 0:
            await asyncio.sleep(-self._tokens / limit)

    def _gauge_backoff(self, congested: bool, clear: bool, cut: float,
                       step: int = 8 * 1024, inflation_us: Optional[int] = None) -> None:
        """One congestion-control step on the shared allowance: a congested
        sample multiplies the rate down; a clear one probes upward —
        multiplicatively while no congestion has ever been seen (the initial
        ramp toward an unknown link rate), additively by ``step`` after
        (fine-grained probing near the working point, TCP's post-ssthresh
        split; the caller sizes the step to its sample cadence).

        The recovery ceiling arms ONCE per congestion epoch, from the rate at
        the epoch's first congested sample (ssthresh semantics): arming it per
        chunk lets a sustained spike ratchet the ceiling toward the floor, and
        computing it from the post-backoff rate pins recovery below the rate
        itself. Reaching the ceiling releases it so clear stretches keep
        probing past the last congested rate; that sawtooth is what keeps a
        link that gets faster later reachable.

        A cut also pauses growth and further cuts for a drain window: a
        sample inside it, clear or congested, still reflects the queue that
        cut is draining. Resuming on a clear would keep the bottleneck queue
        standing, and the cut never relieves the stream sharing the link;
        cutting again on each congested sample would take a fat buffer's
        worth of samples to the floor before it empties, and the recovery
        from there is additive. Past the window a queue still standing draws
        another full cut, but one the gauge measures as shrinking is already
        draining at the rate in force and gets a quarter of it: the full cut
        would only trade the drain's last seconds for a recovery that takes
        many times longer. The epoch closes only on a clear
        sample past that window: ending it on one inside would let an
        oscillating gauge re-arm the ceiling from each freshly cut rate —
        the same ratchet, one flap at a time."""
        if congested:
            self._slow_start = False
            now = time.monotonic()
            draining = (inflation_us is not None and self._last_inflation_us is not None
                        and inflation_us < self._last_inflation_us)
            self._last_inflation_us = inflation_us
            if not self._congested:
                self._congested = True
                self._probe_ceiling = max(self.rate_bps, 2 * self._RATE_FLOOR)
            elif now < self._hold_until:
                return
            elif draining:
                cut = 1 - (1 - cut) / 4
            self.rate_bps = max(self.rate_bps * cut, self._RATE_FLOOR)
            self._hold_until = now + 1.5
            return
        self._last_inflation_us = None
        if not clear:
            return
        if time.monotonic() < self._hold_until:
            return
        self._congested = False
        ceiling = self._probe_ceiling
        if ceiling is not None and self.rate_bps >= ceiling:
            self._probe_ceiling = ceiling = None
        bound = min(
            ceiling if ceiling is not None else self.rate_bps * 4,
            self._ceiling,
        )
        if self._slow_start:
            self.rate_bps = min(self.rate_bps * 2, bound)
        else:
            self.rate_bps = min(self.rate_bps + step, bound)


def _staging_leaf_name() -> str:
    """A staging file name for an upload: fixed length and random rather than
    appended to the destination basename, so a filename that is legal but sits
    close to the filesystem's NAME_MAX still uploads, and another account
    sharing the directory cannot put a file or a FIFO where the upload will
    open it."""
    return f"{UPLOAD_STAGING_PREFIX}{os.urandom(8).hex()}.part"


def _upload_staging_path(dest: str) -> str:
    """Return a new staging file path for an upload to ``dest``.

    The staging file is a fixed-length hidden sibling living in the
    destination's own directory (`_staging_leaf_name` for the name).
    Staying in the same directory keeps the finalizing ``os.replace`` atomic
    and intra-filesystem.
    """
    return os.path.join(os.path.dirname(dest), _staging_leaf_name())


def _carry_destination_mode(
        staging: str, dest: str, dir_fd: Optional[int] = None) -> None:
    """Give the staged upload the permission bits of the file it is about to
    replace, so re-uploading over an existing file keeps its mode: an executable
    script stays executable and a private file stays private. A new destination,
    one that is not a regular file, or one another account owns keeps the
    staging file's creation mode, and the set-ID and sticky bits are never
    carried: a file another account planted under the name must not leave the
    upload writable by it, or a set-ID program of this user's that it can run.
    With ``dir_fd`` both names are read relative to the held descriptor of the
    directory they live in, never by a path that could have been swapped."""
    try:
        st = os.lstat(dest, dir_fd=dir_fd)
    except OSError:
        return
    if not stat.S_ISREG(st.st_mode) or st.st_uid != os.geteuid():
        return
    try:
        os.chmod(staging, stat.S_IMODE(st.st_mode) & 0o777, dir_fd=dir_fd)
    except OSError as e:
        logger.debug(f"Could not carry the mode of {dest} onto the staged upload: {e}")


def _finalize_upload(staging: str, dest: str, dir_fd: Optional[int] = None) -> None:
    """Rename a complete staged upload onto ``dest``, carrying its mode over.

    With ``dir_fd`` both names are relative to one held descriptor of the
    directory they live in, so the rename moves the object the confinement
    opened rather than re-walking a path that could have been swapped.

    Raises:
        OSError: The rename failed; the staged file is left for the caller.
    """
    _carry_destination_mode(staging, dest, dir_fd)
    os.replace(staging, dest, src_dir_fd=dir_fd, dst_dir_fd=dir_fd)


def _remove_quietly(path: str, dir_fd: Optional[int] = None) -> None:
    """Remove ``path``, ignoring a file that is already gone or cannot go."""
    try:
        os.remove(path, dir_fd=dir_fd)
    except OSError:
        pass


class ArchiveTooLarge(Exception):
    """A folder holds more than one archive may carry
    (`ARCHIVE_MAX_SOURCE_BYTES`, `ARCHIVE_MAX_ENTRIES`)."""


def _zip_date_time(mtime: float) -> Tuple[int, int, int, int, int, int]:
    """A modification time as a zip entry's local date and time, clamped to
    the years 1980 to 2107 that the entry can hold."""
    try:
        stamp = tuple(time.localtime(mtime)[:6])
    except (OverflowError, OSError, ValueError):
        stamp = (1980, 1, 1, 0, 0, 0)
    return min(max(stamp, (1980, 1, 1, 0, 0, 0)), (2107, 12, 31, 23, 59, 59))  # type: ignore[return-value]


class FileRootEscape(Exception):
    """A component of a file-manager path resolved outside the root while the
    descriptor walk was following it.

    The refusal the resolved string path gives statically, raised for the one
    case that path cannot see: a symlink planted or swapped into the name
    space between the path's validation and the act on it. The walk verified
    where each open landed from the descriptor itself and refused to hand the
    caller an object the confinement never saw."""


def _ipv6_loopback_redirect(request: web.Request, path: str) -> Optional[str]:
    """Where to serve a page that arrived on the IPv6 loopback from, or ``None``.

    A browser gathers no ICE host candidates for a page whose origin is ``::1``,
    so a WebRTC session opened from one never nominates a pair and the stream
    never starts. The IPv4 loopback reaches this same listener, so the page is
    served from there instead. Which family ``localhost`` resolves to is the
    browser's to choose -- Firefox picks ``::1`` since it took up Happy Eyeballs
    v3 -- and the page cannot tell from script which one it got.

    Only a browser that asked this host for itself is moved: the request has to
    have arrived on ``::1`` and to name a loopback in ``Host``, with no proxy
    header in front of it. A reverse proxy that forwards to ``[::1]`` carries the
    site's own name and is left alone, since redirecting it would send the
    browser to its own machine.
    """
    transport = request.transport
    sock = transport.get_extra_info("socket") if transport else None
    if sock is None or sock.family != socket.AF_INET6:
        return None
    sockname = transport.get_extra_info("sockname")
    if not sockname or sockname[0] != "::1":
        return None
    if request.url.host not in ("localhost", "::1"):
        return None
    if any(h in request.headers for h in ("X-Forwarded-For", "X-Forwarded-Host", "Forwarded")):
        return None
    # Where the browser goes is this process's to say: the literal host, the port
    # this socket listens on, and the caller's own route. Only the query rides
    # along, which a page opened with ``?token=`` needs (the browser keeps a
    # fragment across the redirect itself) and which cannot name another origin.
    scheme = "https" if request.secure else "http"
    query = f"?{request.query_string}" if request.query_string else ""
    return f"{scheme}://127.0.0.1:{int(sockname[1])}{path}{query}"


def _format_sockaddr(family: int, sockaddr: Any) -> str:
    """``host:port`` for a socket address, the host bracketed for IPv6."""
    host, port = sockaddr[0], sockaddr[1]
    return f"[{host}]:{port}" if family == socket.AF_INET6 else f"{host}:{port}"


async def _bind_listen_sockets(addr: str, port: int) -> List[socket.socket]:
    """Bind a listening socket on every address ``addr`` names.

    ``addr`` is a host name or IP literal, or a comma-separated list of them,
    each bound on every address it resolves to: ``localhost`` covers both
    loopback families that way. An address the host does not carry (the IPv6
    loopback with IPv6 disabled) is skipped while another binds; any other
    failure, or nothing binding at all, raises. IPv6 sockets are IPv6-only so
    ``::`` and ``0.0.0.0`` can share the port, the way asyncio binds them.

    Raises:
        OSError: When a name does not resolve, a bind fails for a reason other
            than the host lacking the address, or no address binds.
    """
    loop = asyncio.get_running_loop()
    candidates: List[Tuple[int, int, int, Any]] = []
    seen: set = set()
    for host in (h.strip() for h in addr.split(",")):
        if not host:
            continue
        try:
            infos = await loop.getaddrinfo(
                host, port, type=socket.SOCK_STREAM, flags=socket.AI_PASSIVE)
        except socket.gaierror as exc:
            raise OSError(f"cannot resolve listen address '{host}': {exc}") from exc
        for family, stype, proto, _, sockaddr in infos:
            # The list keeps resolution order; the set is what an address is tested against
            if (family, sockaddr) not in seen:
                seen.add((family, sockaddr))
                candidates.append((family, stype, proto, sockaddr))

    unavailable = (errno.EADDRNOTAVAIL, errno.EAFNOSUPPORT)
    socks: List[socket.socket] = []
    skipped: List[str] = []
    try:
        for family, stype, proto, sockaddr in candidates:
            where = _format_sockaddr(family, sockaddr)
            sock: Optional[socket.socket] = None
            try:
                sock = socket.socket(family, stype, proto)
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                if family == socket.AF_INET6 and hasattr(socket, "IPV6_V6ONLY"):
                    sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
                sock.bind(sockaddr)
                sock.setblocking(False)
            except OSError as exc:
                if sock is not None:
                    sock.close()
                if exc.errno not in unavailable:
                    raise OSError(exc.errno, f"cannot bind {where}: {exc.strerror}") from exc
                skipped.append(f"{where} ({exc.strerror})")
                continue
            socks.append(sock)
    except BaseException:
        for sock in socks:
            sock.close()
        raise
    if not socks:
        raise OSError(f"no address of '{addr}' is available to listen on: {', '.join(skipped)}")
    for note in skipped:
        logger.warning("Not listening on %s", note)
    return socks


def _frame_ancestors_directive(value: str) -> str:
    """The Content-Security-Policy that `frame_ancestors` asks for.

    Entries are comma- or space-separated; `self` and `none` become the quoted
    keywords, and an entry carrying a quote or a semicolon, which would end the
    directive, is dropped. A value naming nothing is this server's own origin:
    an image passing the variable through empty must not open the session to
    every page's frame, which only `*` asks for.
    """
    if not (value or "").replace(",", " ").split():
        value = "'self'"
    sources = []
    for item in (value or "").replace(",", " ").split():
        keyword = item.strip("'").lower()
        if keyword in ("self", "none"):
            sources.append(f"'{keyword}'")
        elif "'" not in item and ";" not in item:
            sources.append(item)
    return f"frame-ancestors {' '.join(sources)}" if sources else ""


def _unix_socket_is_live(path: str) -> bool:
    """Return True when something accepts a connection on ``path``, i.e. the
    socket file belongs to a running listener rather than being a leftover from
    a dead one. Anything other than a refusal counts as live: an error that does
    not prove the path is dead must not license removing it."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
        probe.settimeout(0.25)
        try:
            probe.connect(path)
        except (ConnectionRefusedError, FileNotFoundError):
            return False
        except OSError:
            return True
    return True


# sysexits.h EX_CONFIG: the settings are the problem and starting again will not
# change the answer, so a supervisor can stop instead of respawning forever.
EXIT_CONFIG_ERROR: int = 78
# Realm both 401 challenges name; the web client's 401 guard keys on the
# Bearer one as this server's own token verdict.
AUTH_REALM: str = "Selkies Restricted"
# Handshakes carrying their own token gate in the ws handlers; the auth
# middleware exempts exactly these paths, never a mere Upgrade claim.
WEBSOCKET_ROUTES: Tuple[str, ...] = ("/api/websockets", "/api/webrtc/signaling", "/api/ws")
# Mirror of the secure-mode session token for requests the client cannot put
# a header on (the file-manager iframe and its download links).
SESSION_TOKEN_COOKIE: str = "selkies_token"
# Subprotocols of a handshake presenting its session token without a URL: the
# one selected, and the prefix of the base64url token (``handshake_session_token``).
SESSION_TOKEN_PROTOCOL: str = "selkies"
SESSION_TOKEN_PROTOCOL_PREFIX: str = "selkies.token."
# Fallback carrier for the master token on the token and mode-switch
# endpoints, same ``Bearer <token>`` grammar as Authorization. A request has
# one Authorization header, so a caller behind a Basic login (a reverse
# proxy's, typically) must spend it on the Basic credentials and present the
# master token here instead; Authorization is still tried first.
MASTER_TOKEN_HEADER: str = "Selkies-Authorization"


def handshake_session_token(request: web.BaseRequest) -> Optional[str]:
    """The session token a WebSocket handshake presents, or None.

    A client that took its token from the page's fragment offers it as the
    ``selkies.token.<base64url>`` subprotocol beside ``selkies``, so no request
    line carries it; one that took it from the query presents it as ``?token=``
    on the socket URL. The subprotocol wins when both are there.

    Raises:
        web.HTTPBadRequest: The token subprotocol arrived without ``selkies``,
            or does not decode. aiohttp logs the offered subprotocols when it
            finds none it can select, which would write the token into the log,
            so such a handshake is refused before it gets that far.
    """
    offered = [
        protocol.strip()
        for header in request.headers.getall("Sec-WebSocket-Protocol", ())
        for protocol in header.split(",")
    ]
    carried = [p for p in offered if p.startswith(SESSION_TOKEN_PROTOCOL_PREFIX)]
    if not carried:
        return request.query.get("token") or None
    if SESSION_TOKEN_PROTOCOL not in offered:
        raise web.HTTPBadRequest(text="Session token subprotocol offered without the selkies protocol")
    encoded = carried[0][len(SESSION_TOKEN_PROTOCOL_PREFIX):]
    try:
        token = base64.b64decode(encoded + "=" * (-len(encoded) % 4), altchars=b"-_", validate=True).decode("utf-8")
    except ValueError:  # binascii.Error and UnicodeDecodeError both
        raise web.HTTPBadRequest(text="Undecodable session token subprotocol") from None
    return token or None

FILE_INDEX_HEADER: str = """<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <title>Desktop Files</title>
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <script>
        /* Mirror the dashboard's stored theme (both front ends are same
           origin, classic under "theme", wish under "vite-ui-theme"); wish's
           "system" and denied storage leave the pick to the media query. */
        (function () {
            var apply = function () {
                var v = null;
                try {
                    v = localStorage.getItem('theme') || localStorage.getItem('vite-ui-theme');
                } catch (e) { return; }
                if (v === 'system') { delete document.documentElement.dataset.theme; }
                else { document.documentElement.dataset.theme = v || 'dark'; }
            };
            apply();
            window.addEventListener('storage', apply);
        })();
    </script>
    <style>
        /* The dashboards' palette and type scale, so the file browser reads as
           part of the same session rather than a page from somewhere else.
           The script above stamps the dashboard's stored theme choice; with
           nothing stored it stamps the dark both dashboards default to. */
        :root {
            --accent: #a82a69;
            --accent-text: #a82a69;
            --accent-soft: rgba(168, 42, 105, 0.1);
            --bg: #f4f5f8;
            --surface: #ffffff;
            --surface-alt: #f6f7f9;
            --border-soft: #dfe2ea;
            --text: #23272f;
            --text-muted: #5b6478;
            --shadow: 0 1px 2px rgba(16, 20, 28, 0.06), 0 8px 24px rgba(16, 20, 28, 0.08);

            --container-max-width: 960px;
            --font-family: system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
            --border-radius: 10px;
        }
        @media (prefers-color-scheme: dark) {
            :root:not([data-theme="light"]) {
                --accent: #ec4899;
                --accent-text: #f9a8d4;
                --accent-soft: rgba(236, 72, 153, 0.14);
                --bg: #12161d;
                --surface: #181d26;
                --surface-alt: #1f2530;
                --border-soft: #2b3441;
                --text: #e6ebf3;
                --text-muted: #aeb9c9;
                --shadow: 0 1px 2px rgba(0, 0, 0, 0.4), 0 8px 24px rgba(0, 0, 0, 0.35);
            }
        }
        :root[data-theme="dark"] {
            --accent: #ec4899;
            --accent-text: #f9a8d4;
            --accent-soft: rgba(236, 72, 153, 0.14);
            --bg: #12161d;
            --surface: #181d26;
            --surface-alt: #1f2530;
            --border-soft: #2b3441;
            --text: #e6ebf3;
            --text-muted: #aeb9c9;
            --shadow: 0 1px 2px rgba(0, 0, 0, 0.4), 0 8px 24px rgba(0, 0, 0, 0.35);
        }
        * {
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }
        body {
            font-family: var(--font-family);
            background-color: var(--bg);
            color: var(--text);
            line-height: 1.55;
            padding: 24px 0 60px;
        }
        .page-container {
            max-width: var(--container-max-width);
            margin: 0 auto;
            padding: 0 20px;
            position: relative;
        }
        h1 {
            color: var(--text);
            font-size: 1.4rem;
            font-weight: 600;
            letter-spacing: -0.01em;
            margin-bottom: 16px;
            padding: 0 52px 16px 0;
            border-bottom: 1px solid var(--border-soft);
            word-break: break-all;
        }
        h1 a {
            color: var(--accent-text);
            text-decoration: none;
        }
        h1 a:hover {
            text-decoration: underline;
        }
        hr {
            display: none;
        }
        table {
            width: 100%;
            border-collapse: separate;
            border-spacing: 0;
            margin-top: 20px;
            font-size: 0.9rem;
            background-color: var(--surface);
            border: 1px solid var(--border-soft);
            border-radius: var(--border-radius);
            overflow: hidden;
            box-shadow: var(--shadow);
        }
        thead {
            background-color: var(--surface-alt);
        }
        th {
            color: var(--text-muted);
            font-weight: 600;
            font-size: 0.75rem;
            text-transform: uppercase;
            letter-spacing: 0.04em;
        }
        th, td {
            padding: 11px 16px;
            text-align: left;
            border-bottom: 1px solid var(--border-soft);
        }
        tbody tr {
            transition: background-color 0.15s ease-in-out;
        }
        tbody tr:hover {
            background-color: var(--accent-soft);
        }
        tbody tr:last-child td {
            border-bottom: none;
        }
        td {
            color: var(--text-muted);
        }
        td a {
            color: var(--text);
            text-decoration: none;
            display: inline-flex;
            align-items: center;
            font-weight: 500;
        }
        td a:hover {
            color: var(--accent-text);
            text-decoration: underline;
        }
        th a, th a:visited {
            color: var(--text-muted);
            text-decoration: none;
        }
        th a:hover {
            color: var(--accent-text);
            text-decoration: underline;
        }
        #reload-page-button {
            position: absolute;
            top: -2px;
            right: 20px;
            background-color: var(--surface);
            color: var(--text-muted);
            border: 1px solid var(--border-soft);
            border-radius: var(--border-radius);
            width: 34px;
            height: 34px;
            cursor: pointer;
            display: inline-flex;
            align-items: center;
            justify-content: center;
            transition: color 0.15s ease-in-out, border-color 0.15s ease-in-out;
            z-index: 10;
        }
        #reload-page-button:hover {
            color: var(--accent-text);
            border-color: var(--accent);
        }
        #reload-page-button:focus-visible {
            outline: none;
            box-shadow: 0 0 0 3px var(--accent-soft);
        }
        #reload-page-button svg {
            width: 18px;
            height: 18px;
            fill: currentColor;
        }
        td:nth-child(1) {
            word-break: break-all;
        }
        td:nth-child(2), th:nth-child(2) {
            white-space: nowrap;
            width: 180px;
        }
        td:nth-child(3), th:nth-child(3) {
            text-align: right;
            white-space: nowrap;
            width: 100px;
        }
        /* Masked rather than painted, so one glyph follows the theme's text
           color instead of needing a copy per palette. */
        td a::before {
            display: inline-block;
            content: '';
            width: 1.05em;
            height: 1.05em;
            margin-right: 0.7em;
            flex-shrink: 0;
            background-color: var(--text-muted);
            mask-repeat: no-repeat;
            mask-size: contain;
            mask-position: center;
            -webkit-mask-repeat: no-repeat;
            -webkit-mask-size: contain;
            -webkit-mask-position: center;
        }
        td a:hover::before {
            background-color: var(--accent-text);
        }
        td a[href="../"]::before {
            mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M20 11H7.83l5.59-5.59L12 4l-8 8 8 8 1.41-1.41L7.83 13H20v-2z"/></svg>');
            -webkit-mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M20 11H7.83l5.59-5.59L12 4l-8 8 8 8 1.41-1.41L7.83 13H20v-2z"/></svg>');
        }
        td a[href$="/"]:not([href="../"])::before {
            mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M10 4H4c-1.11 0-2 .89-2 2v12c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V8c0-1.1-.9-2-2-2h-8l-2-2z"/></svg>');
            -webkit-mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M10 4H4c-1.11 0-2 .89-2 2v12c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V8c0-1.1-.9-2-2-2h-8l-2-2z"/></svg>');
        }
        td a:not([href$="/"]):not([href="../"])::before {
            mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M14 2H6c-1.11 0-2 .9-2 2v16c0 1.1.89 2 2 2h12c1.1 0 2-.9 2-2V8l-6-6zm2 16H8v-2h8v2zm0-4H8v-2h8v2zm-3-5V3.5L18.5 9H13z"/></svg>');
            -webkit-mask-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M14 2H6c-1.11 0-2 .9-2 2v16c0 1.1.89 2 2 2h12c1.1 0 2-.9 2-2V8l-6-6zm2 16H8v-2h8v2zm0-4H8v-2h8v2zm-3-5V3.5L18.5 9H13z"/></svg>');
        }
        footer {
            text-align: center;
            margin-top: 32px;
            padding-top: 16px;
            border-top: 1px solid var(--border-soft);
            font-size: 0.8rem;
            color: var(--text-muted);
        }
        footer p {
            margin: 0;
        }
        @media (max-width: 768px) {
            body {
                font-size: 14px;
                padding: 12px 0 40px;
            }
            .page-container {
                padding: 0 12px;
            }
            h1 {
                font-size: 1.15rem;
                padding-right: 46px;
            }
            #reload-page-button {
                right: 12px;
            }
            table {
                display: block;
                overflow-x: auto;
                -webkit-overflow-scrolling: touch;
            }
            th, td {
                padding: 10px 12px;
                white-space: nowrap;
            }
            td:nth-child(1) {
                min-width: 200px;
            }
            td:nth-child(2), th:nth-child(2) {
                min-width: 150px;
                width: auto;
            }
            td:nth-child(3), th:nth-child(3) {
                min-width: 80px;
                width: auto;
            }
        }
    </style>
</head>
<body>
    <div class="page-container">
        <button id="reload-page-button" title="Reload Page">
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">
                <path d="M17.65 6.35C16.2 4.9 14.21 4 12 4c-4.42 0-7.99 3.58-7.99 8s3.57 8 7.99 8c3.73 0 6.84-2.55 7.73-6h-2.08c-.82 2.33-3.04 4-5.65 4-3.31 0-6-2.69-6-6s2.69-6 6-6c1.66 0 3.14.69 4.22 1.78L13 11h7V4l-2.35 2.35z"/>
            </svg>
        </button>

        <h1>
"""

FILE_INDEX_FOOTER: str = """    </div> <!-- closes .page-container -->
    <footer>
        <p>&copy; <script>document.write(new Date().getFullYear())</script> Selkies.</p>
    </footer>

    <script>
        const reloadButton = document.getElementById('reload-page-button');
        if (reloadButton) {
            reloadButton.addEventListener('click', function() {
                window.location.reload();
            });
        }

        function processDirectoryListing() {
            // Shared with the nginx fancyindex footer (addons/selkies-web-core/
            // nginx/footer.html). The mount is derived from the pathname rather
            // than assumed: /api/files/ or the legacy /files/, either of them
            // possibly behind a deployment subfolder or a fronting proxy.
            const path = window.location.pathname;
            // Normalized before searching: behind a fronting proxy the root
            // can surface without its trailing slash, and both needles end
            // in one, so the raw path would simply never match.
            const normPath = path.endsWith('/') ? path : path + '/';
            // The OUTERMOST match wins, not a fixed preference: preferring
            // /api/files/ unconditionally mistook /files/api/files/ (a
            // subdirectory literally named api/files under the legacy mount)
            // for the /api/files/ root.
            const idxApi = normPath.indexOf('/api/files/');
            const idxLegacy = normPath.indexOf('/files/');
            let webPathPrefix = '/api/files/';
            let idx = idxApi;
            if (idxLegacy !== -1 && (idxApi === -1 || idxLegacy < idxApi)) {
                webPathPrefix = '/files/';
                idx = idxLegacy;
            }
            const injectedPathPrefix = window.__SELKIES_INJECTED_PATH_PREFIX__ || '';
            const diskPathPrefix = injectedPathPrefix || '';
            const h1 = document.querySelector('h1');

            if (h1 && idx !== -1) {
                const text = h1.textContent;
                const j = text.indexOf(webPathPrefix);
                if (j !== -1) {
                    const tail = text.slice(j + webPathPrefix.length);
                    h1.textContent = diskPathPrefix.replace(/\\/+$/, '') + '/' + tail;
                }
            }

            // File links are forced to save (directories and sort links keep
            // their behavior): nginx-served listings send no Content-Disposition,
            // so a click in the dashboard's iframe would render inline. A
            // secure-mode token in the query rides every link, keeping the
            // listing navigable where the token cookie cannot follow.
            const sessionToken = new URLSearchParams(window.location.search).get('token');
            document.querySelectorAll('table#list td a').forEach(function(a) {
                const href = a.getAttribute('href') || '';
                if (!href || href.startsWith('?')) return;
                // A sorted listing carries its sort in every link's query,
                // the parent's included, so a directory is told by the path
                // ahead of it, and the token joins a query already there.
                if (!href.split('?')[0].endsWith('/')) {
                    a.setAttribute('download', '');
                }
                if (sessionToken) {
                    a.setAttribute('href', href + (href.includes('?') ? '&' : '?')
                        + 'token=' + encodeURIComponent(sessionToken));
                }
            });

            // At the root the parent row is an escape hatch out of the tree.
            // On the nginx /files/ mount its ../ resolves to /, the desktop
            // page, whose load registers a fresh primary client and kills
            // the running session; on /api/files/ it merely 404s at /api/.
            // Wrong either way, so remove the row outright, leaving no way
            // for a later style or script to bring it back.
            const isAtRoot = idx !== -1 && idx + webPathPrefix.length === normPath.length;
            if (isAtRoot) {
                const parentLink = document.querySelector('table#list a[href^="../"]');
                if (parentLink) {
                    const parentRow = parentLink.closest('tr');
                    if (parentRow) {
                        parentRow.remove();
                    }
                }
            }
        }

        let attempts = 0;
        const maxAttempts = 20;
        const intervalId = setInterval(function() {
            attempts++;
            const h1 = document.querySelector('h1');
            const table = document.getElementById('list');

            if (h1 && table) {
                clearInterval(intervalId);
                processDirectoryListing();
            } else if (attempts >= maxAttempts) {
                clearInterval(intervalId);
                processDirectoryListing();
            }
        }, 100);

    </script>
</body>
</html>
"""


class BaseStreamingService(metaclass=ABCMeta):
    """Interface a streaming service (websockets or webrtc) implements so the
    supervisor can start, stop, and route to either interchangeably."""

    def __init__(self, name: str) -> None:
        self.mode = name

    @abstractmethod
    async def start(self) -> None:
        """Set up resources and run the service's loops until stopped."""

    @abstractmethod
    async def stop(self) -> None:
        """Clean up resources and stop the service's loops."""

    @abstractmethod
    def register_routes(self, api_prefix: str, main_router: web.UrlDispatcher) -> None:
        """Register the service's absolute paths directly on the main router.

        Args:
            api_prefix: Deployment subfolder prefix (empty when serving at root).
            main_router: The application's router to register routes on.
        """
        pass

    @abstractmethod
    async def announce_print_document(self, name: str, size: int) -> None:
        """Tell the controller pages that `name` waits in the print spool."""

    @abstractmethod
    async def sessions(self) -> List[Dict[str, Any]]:
        """The pages connected to this transport: `id`, `transport`, `role`,
        `slot`, `display`, `connected_at`, and `rtt_ms`, the same keys on both."""

    @abstractmethod
    async def disconnect_session(self, session_id: str) -> bool:
        """Close the page `id` names; False when it is not connected here."""

    def uplink_session_conns(self) -> List[Tuple[Any, Optional[str], Optional[str]]]:
        """``(websocket, session token, peer ip)`` per connected client
        session, for upload uplink gauging (`UplinkGauge`).

        The websocket must send ``ping()`` frames, and the service's message
        loop must run with autoping off, answer PING itself, and hand every
        PONG to `note_pong` — the gauge's clock is dead without that. Both
        transports implement this; an upload gauged on one side but not the
        other is a parity bug. Default: nothing to gauge.
        """
        return []


class CentralizedStreamServer:
    """Supervisor that owns the aiohttp application and the streaming services.

    Exactly one registered service (websockets or webrtc) is active at a time;
    ``switch_to_mode`` serializes transitions under a lock so capture is never
    owned twice. The supervisor also serves the control-plane API, the static
    frontend, file uploads/downloads, and — when HTTPS is enabled — hot-reloads
    certificates by rebuilding the listening site without restarting the app.

    On Wayland the constructor brings the pixelflux compositor up before any
    capture or session app starts and mirrors its socket name (the external
    compositor's, in host-capture mode) into `WAYLAND_DISPLAY`, so every child
    spawned with a copied environment reaches it.

    Attributes:
        transfer_cap: The operator's static file-transfer cap, one bucket
            shared by every download and upload; inactive when unset.
        download_pacer: The downloads' adaptive allowance, fed per-chunk
            verdicts from an `UplinkGauge` over the downloader's own session
            socket.
        upload_pacer: The uploads' adaptive allowance, fed the same way from
            the uploader's session socket. The two directions queue at
            opposite ends of the path, so one's verdicts say nothing about
            the other's rate.
        _chunked_uploads: In-flight chunked uploads by destination path: transfer
            id, next expected offset, `.part` path, last-activity stamp, and a
            busy flag that refuses interleaved writes to the same destination.
    """

    def __init__(
        self,
        settings: Any,
        services: Optional[Dict[str, BaseStreamingService]] = None,
    ) -> None:
        self.settings = settings
        self.services = services or {}
        self.current_mode: Optional[str] = None
        self.lock = asyncio.Lock()
        self.active_task: Optional[asyncio.Task] = None

        self.app: Optional[web.Application] = None
        self.runner: Optional[web.AppRunner] = None
        self.sites: List[web.BaseSite] = []
        self.cert_watcher: Optional[asyncio.Task] = None
        self.tokens_watcher: Optional[asyncio.Task] = None
        self.ssl_context: Optional[ssl.SSLContext] = None
        self.static_fs_path: str = ""
        limit_mbps = float(self.settings.file_transfer_limit_mbps or 0.0)
        if not math.isfinite(limit_mbps) or limit_mbps < 0:
            logger.warning(
                f"Ignoring file_transfer_limit_mbps={limit_mbps!r}: not a usable rate."
            )
            limit_mbps = 0.0
        self.transfer_cap = TransferPacer(static_bps=int(limit_mbps * 125000))
        self.download_pacer = TransferPacer(adaptive=True)
        self.upload_pacer = TransferPacer(adaptive=True)
        self.upload_dir = pathlib.Path(
            os.path.expanduser(self.settings.file_manager_path)
        ).resolve()
        self.print_spool = pathlib.Path(
            printing.spool_path(self.settings.print_spool_path)
        ).resolve()
        self.print_watcher: Optional[printing.SpoolWatcher] = None
        self.print_queue: Optional[printing.PrintQueue] = None
        self._recording_audio: Optional[Any] = None
        self._chunked_uploads: Dict[str, Dict[str, Any]] = {}
        self.web_files_ctx: Optional[tempfile.TemporaryDirectory] = None

        self._clients_present: bool = False
        self._client_hook_tasks: Set[asyncio.Task] = set()

        if bool(self.settings.wayland[0]):
            try:
                from pixelflux import ensure_wayland_display
                socket_name = ensure_wayland_display(
                    width=int(self.settings.manual_width or 0),
                    height=int(self.settings.manual_height or 0),
                    render_node=self.settings.render_dri or "",
                    auto_gpu=str(self.settings.auto_gpu or ""),
                    cursor_size=int(self.settings.cursor_size),
                )
                if socket_name:
                    host_display = str(
                        getattr(self.settings, "wayland_host_display", "") or "")
                    os.environ["WAYLAND_DISPLAY"] = host_display or socket_name
                    logger.info(f"Wayland compositor socket: {socket_name}")
                else:
                    logger.warning(
                        "Wayland compositor socket did not come up within its "
                        "startup window; WAYLAND_DISPLAY left unchanged."
                    )
            except ImportError:
                logger.warning("pixelflux unavailable; Wayland display not initialized.")

        self.STREAMING_MODE_WEBRTC = "webrtc"
        self.STREAMING_MODE_WEBSOCKETS = "websockets"
        self.STATIC_CONTENT_PATH = "selkies.selkies_web"
        self.MIME_TYPES = {
            "html": "text/html",
            "js": "text/javascript",
            "css": "text/css",
            "json": "application/json",
            "png": "image/png",
            "jpg": "image/jpeg",
            "ico": "image/x-icon",
            "svg": "image/svg+xml",
        }

    def set_clients_present(self, present: bool) -> None:
        """Record client presence and fire the configured presence hook.

        Both streaming modes report here; the ``run_after_connect`` /
        ``run_after_disconnect`` hook command runs when the first client
        connects or the last one disconnects.

        Args:
            present: Whether at least one client is currently connected.
        """
        if present == self._clients_present:
            return
        self._clients_present = present
        cmd = (self.settings.run_after_connect if present
               else self.settings.run_after_disconnect)
        if cmd:
            # Hold a strong reference: the loop only keeps weak refs to tasks.
            task = asyncio.create_task(self._run_client_hook(cmd, present))
            self._client_hook_tasks.add(task)
            task.add_done_callback(self._client_hook_tasks.discard)

    async def _run_client_hook(self, cmd: str, present: bool) -> None:
        """Run a presence hook shell command, killing it if it wedges."""
        name = "run_after_connect" if present else "run_after_disconnect"
        try:
            proc = await asyncio.create_subprocess_shell(cmd)
            try:
                returncode = await asyncio.wait_for(proc.wait(), timeout=300)
            except asyncio.TimeoutError:
                logger.warning(f"{name} command timed out after 300s; killing: {cmd}")
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
                returncode = await proc.wait()
            if returncode != 0:
                logger.warning(f"{name} command exited with status {returncode}: {cmd}")
        except OSError as e:
            logger.error(f"Failed to run {name} command {cmd!r}: {e}")

    def _b64_decode(self, data: str) -> str:
        return base64.b64decode(data).decode("utf-8")

    def _get_https_certs(self) -> Tuple[Optional[str], Optional[str]]:
        """Return absolute paths of the configured cert and key files, each None
        when unset or missing on disk."""
        https_cert = getattr(self.settings, "https_cert", None)
        https_key = getattr(self.settings, "https_key", None)
        cert_pem = (
            os.path.abspath(https_cert)
            if https_cert and os.path.isfile(https_cert)
            else None
        )
        key_pem = (
            os.path.abspath(https_key)
            if https_key and os.path.isfile(https_key)
            else None
        )
        return cert_pem, key_pem

    def _self_signed_candidates(self) -> List[Tuple[str, str]]:
        """Where a generated pair may live, most preferred first.

        The configured path leads so an operator who pointed at one gets the
        pair there; the state directory catches a user install, which for the
        default `ssl-cert-snakeoil` path means anyone not running as root.

        Returns:
            `(certificate, key)` absolute path pairs; a pair with no key path
            is dropped, since a generated key is always written separately.
        """
        candidates = []
        configured = getattr(self.settings, "https_cert", None)
        if configured:
            candidates.append((os.path.abspath(configured),
                               os.path.abspath(getattr(self.settings, "https_key", "") or "")))
        state = os.path.join(
            os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state"),
            "selkies")
        candidates.append((os.path.join(state, "selkies.pem"),
                           os.path.join(state, "selkies.key")))
        return [(cert, key) for cert, key in candidates if cert and key]

    def _self_signed_names(self) -> Tuple[list, list]:
        """The names a generated certificate is issued for.

        Loopback under every spelling, since that is what a published container
        port is reached by, plus the host's own name. Its routable address is
        deliberately absent: the pair outlives the address a container is given
        on each run, and one naming last week's is worse than one claiming none.

        Returns:
            `(dns names, ip addresses)`, both ready for `SubjectAlternativeName`.
        """
        from cryptography import x509

        dns = ["localhost"]
        hostname = socket.gethostname()
        if hostname and hostname != "localhost":
            dns.append(hostname)
        addresses = [ipaddress.ip_address("127.0.0.1"), ipaddress.ip_address("::1")]
        return ([x509.DNSName(name) for name in dns],
                [x509.IPAddress(address) for address in addresses])

    def _self_signed_pair_usable(self, cert_path: str, key_path: str) -> bool:
        """Whether an existing generated pair can be served again as it is.

        Reused only if it loads, has not expired, and names loopback by address:
        one issued before those SANs fails hostname verification at 127.0.0.1,
        and keeping it would carry that forward for the life of the install.
        Anything unreadable counts as absent, so a truncated file is replaced.

        Args:
            cert_path: Certificate to examine.
            key_path: Its private key.

        Returns:
            True when the pair should be reused instead of regenerated.
        """
        from cryptography import x509

        if not (os.path.isfile(cert_path) and os.path.isfile(key_path)):
            return False
        try:
            with open(cert_path, "rb") as handle:
                cert = x509.load_pem_x509_certificate(handle.read())
            # not_valid_after_utc where cryptography has it, else the naive
            # attribute it replaced, read as UTC.
            expiry = getattr(cert, "not_valid_after_utc", None)
            now = datetime.now(tz=timezone.utc)
            if expiry is None:
                expiry = cert.not_valid_after.replace(tzinfo=timezone.utc)
            if expiry <= now:
                return False
            addresses = cert.extensions.get_extension_for_class(
                x509.SubjectAlternativeName).value.get_values_for_type(x509.IPAddress)
            if ipaddress.ip_address("127.0.0.1") not in addresses:
                return False
            context = ssl.create_default_context(purpose=ssl.Purpose.CLIENT_AUTH)
            context.load_cert_chain(cert_path, keyfile=key_path)
        except Exception:
            return False
        return True

    def _make_self_signed_cert(self) -> Tuple[str, str]:
        """Return a self-signed certificate and key, writing one where none is usable.

        Turning HTTPS on is otherwise a two-step job — make a certificate, then
        point at it — and browsers gate the clipboard, gamepads, pointer lock,
        and the camera on a secure context, so the step is in everyone's way.
        The configured paths are used when their directory is writable, which
        for the default `ssl-cert-snakeoil` pair means running as root; a user
        install falls back to its own state directory. That directory is not a
        path `_get_https_certs` consults, so the pair already there is found
        here instead — and reused, since minting one per start would invalidate
        the exception the browser was told to make on every restart.

        Returns:
            The certificate and key paths, both absolute.

        Raises:
            OSError: No candidate directory could be written to.
        """
        from cryptography import x509
        from cryptography.hazmat.primitives import hashes, serialization
        from cryptography.hazmat.primitives.asymmetric import ec

        candidates = self._self_signed_candidates()
        for cert_path, key_path in candidates:
            if self._self_signed_pair_usable(cert_path, key_path):
                logger.info(
                    "HTTPS is running on the self-signed certificate already at %s.",
                    cert_path)
                return cert_path, key_path

        key = ec.generate_private_key(ec.SECP256R1())
        name = x509.Name([x509.NameAttribute(x509.NameOID.COMMON_NAME, "localhost")])
        dns_names, addresses = self._self_signed_names()
        now = datetime.now(tz=timezone.utc)
        cert = (
            x509.CertificateBuilder()
            .subject_name(name)
            .issuer_name(name)
            .public_key(key.public_key())
            .serial_number(x509.random_serial_number())
            .not_valid_before(now - timedelta(days=1))
            .not_valid_after(now + timedelta(days=3650))
            .add_extension(x509.SubjectAlternativeName(dns_names + addresses), critical=False)
            # Not a CA: a key sitting in the session container is a far worse
            # thing to put in a trust store than a leaf is to except.
            .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
            .sign(key, hashes.SHA256())
        )
        errors = []
        for cert_path, key_path in candidates:
            try:
                os.makedirs(os.path.dirname(cert_path), exist_ok=True)
                os.makedirs(os.path.dirname(key_path), exist_ok=True)
                with open(cert_path, "wb") as handle:
                    handle.write(cert.public_bytes(serialization.Encoding.PEM))
                fd = os.open(key_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
                with os.fdopen(fd, "wb") as handle:
                    handle.write(key.private_bytes(
                        encoding=serialization.Encoding.PEM,
                        format=serialization.PrivateFormat.PKCS8,
                        encryption_algorithm=serialization.NoEncryption()))
                # Set after the write: O_CREAT leaves an existing file's mode alone.
                os.chmod(key_path, 0o600)
            except OSError as error:
                errors.append(f"{cert_path}: {error}")
                continue
            logger.warning(
                "No certificate at the configured path, so HTTPS runs on a self-signed one "
                "written to %s, issued for %s and no other address. Browsers will warn once "
                "until it is trusted; a certificate from an authority, or a reverse proxy "
                "terminating TLS, avoids that.",
                cert_path,
                ", ".join([n.value for n in dns_names] + [str(a.value) for a in addresses]))
            return cert_path, key_path
        raise OSError("could not write a self-signed certificate: " + "; ".join(errors))

    def _create_ssl_context(self) -> Optional[ssl.SSLContext]:
        """Build the server TLS context from the configured cert/key.

        Returns:
            The loaded context, or None when HTTPS is disabled.

        Raises:
            FileNotFoundError: HTTPS is enabled but the certificate is missing.
        """
        enable_https = getattr(self.settings, "enable_https", None)
        if not enable_https or not enable_https[0]:
            return None

        cert_pem, key_pem = self._get_https_certs()
        if not cert_pem:
            cert_pem, key_pem = self._make_self_signed_cert()

        logger.debug(
            "Creating TLS context with certificate=%s key=%s", cert_pem, key_pem
        )
        sslctx = ssl.create_default_context(purpose=ssl.Purpose.CLIENT_AUTH)
        sslctx.minimum_version = ssl.TLSVersion.TLSv1_2
        sslctx.check_hostname = False
        sslctx.verify_mode = ssl.CERT_NONE
        try:
            sslctx.load_cert_chain(cert_pem, keyfile=key_pem if key_pem else None)
        except Exception:
            logger.error(
                "Certificate or private key file not found or incorrect. "
                'To use a self-signed certificate, install the package "ssl-cert" '
                'and add the group "ssl-cert" to your user in Debian-based '
                "distributions, or generate a new certificate with root using: "
                "openssl req -x509 -newkey rsa:4096 "
                "-keyout /etc/ssl/private/ssl-cert-snakeoil.key "
                "-out /etc/ssl/certs/ssl-cert-snakeoil.pem -days 3650 -nodes "
                '-subj "/CN=localhost"'
            )
            raise
        return sslctx

    def _get_cert_mtime(self) -> float:
        """Return the most recent modification time of the cert and key files.

        Falls back to the generated pair actually being served, so the reload
        watcher is not blind to it when the configured path holds nothing.
        """
        cert_pem, key_pem = self._get_https_certs()
        if not cert_pem:
            for candidate_cert, candidate_key in self._self_signed_candidates():
                if os.path.isfile(candidate_cert):
                    cert_pem, key_pem = candidate_cert, candidate_key
                    break
        if not cert_pem:
            return 0.0
        try:
            cert_mtime = os.stat(cert_pem).st_mtime
            key_mtime = os.stat(key_pem).st_mtime if key_pem else 0.0
            return max(cert_mtime, key_mtime)
        except OSError:
            return 0.0

    async def _watch_and_reload_certs(self) -> None:
        """Poll the TLS cert/key mtimes and swap in a new listening site on change.

        The new SSL context is built before the old site is stopped so a bad
        certificate never takes the server offline, and the recorded mtime only
        advances once the new site is up, so a failed reload is retried on the
        next poll. Both the stat and the context build run off the loop: they
        read files that may sit on a network mount, and this runs while the
        session is streaming.
        """
        reload_interval = getattr(self.settings, "cert_reload_interval", 30)
        if reload_interval <= 0:
            logger.debug("Automatic certificate reloading is disabled (interval=0)")
            return

        current_sites = self.sites
        last_mtime = await asyncio.to_thread(self._get_cert_mtime)
        logger.debug(
            "Certificate reload watcher started (interval=%ds, initial mtime=%.0f)",
            reload_interval,
            last_mtime,
        )

        while True:
            await asyncio.sleep(reload_interval)
            try:
                new_mtime = await asyncio.to_thread(self._get_cert_mtime)
            except Exception as exc:
                logger.warning("Could not stat cert/key files: %s", exc)
                continue

            if new_mtime <= last_mtime:
                continue

            logger.info(
                "Certificate change detected (mtime %.0f -> %.0f), reloading…",
                last_mtime,
                new_mtime,
            )
            try:
                new_ssl_context = await asyncio.to_thread(self._create_ssl_context)
            except Exception as exc:
                logger.error(
                    "Failed to create new SSL context, keeping old certificate: %s",
                    exc,
                )
                continue

            if new_ssl_context is None:
                logger.error(
                    "New SSL context is None (HTTPS disabled?), keeping old site."
                )
                continue

            for site in current_sites:
                try:
                    await site.stop()
                except Exception as exc:
                    logger.warning("Error stopping old %s: %s", self._site_kind(), exc)
            current_sites = self.sites = []
            logger.info("Old %s stopped.", self._site_kind())

            try:
                current_sites = await self._start_sites(new_ssl_context)
                last_mtime = new_mtime
                logger.info(
                    "New %s started with reloaded certificates on %s",
                    self._site_kind(),
                    self._site_endpoint(),
                )
            except Exception as exc:
                logger.critical(
                    "Failed to start new %s: %s. HTTPS server may be down; "
                    "will retry on the next certificate poll.",
                    self._site_kind(),
                    exc,
                )

    @staticmethod
    def _check_master_token(auth_header: Optional[str], master_token: Any) -> bool:
        """Timing-safe check of a ``Bearer <master_token>`` header, compared as
        UTF-8 bytes so non-ASCII tokens are safe."""
        if not auth_header or not auth_header.startswith("Bearer "):
            return False
        parts = auth_header.split()
        if len(parts) < 2:
            return False
        return hmac.compare_digest(
            parts[1].encode("utf-8"), str(master_token).encode("utf-8")
        )

    @staticmethod
    def _basic_auth_challenge(text: str = "Invalid Credentials") -> web.Response:
        """A 401 that re-opens the browser's login prompt.

        Wrong credentials carry the challenge too: without one the browser renders this
        body as a page instead of asking again, so a mistyped password ends the session.
        """
        return web.Response(
            status=401,
            headers={
                "WWW-Authenticate": f'Basic realm="{AUTH_REALM}", charset="UTF-8"'
            },
            text=text,
        )

    @staticmethod
    def _bearer_challenge(text: str = "Unauthorized") -> web.Response:
        """A 401 for a route that wants a token.

        Names the Bearer scheme so the web client's 401 guard leaves it alone: a
        reload cannot change the token a page holds, whereas a Basic challenge
        is exactly what a reload re-presents. Browsers show no prompt for it.
        """
        return web.Response(
            status=401,
            headers={"WWW-Authenticate": f'Bearer realm="{AUTH_REALM}"'},
            text=text,
        )

    @staticmethod
    def _session_token_carriers(request: web.Request) -> List[Tuple[str, str]]:
        """The session-token carriers a request presents, most explicit first.

        The Bearer header is what scripts send; the ``?token=`` query is what a
        page opened with one carries on the URLs it navigates to rather than
        fetches (a file the manager downloads), which a page holding its token
        in the fragment leaves to the cookie; the cookie is the mirror the
        client keeps for requests it can put neither on. The cookie value is
        tried as sent and URL-decoded, since the client stores it encoded.

        Returns:
            ``(source, token)`` pairs, source being "header", "query" or "cookie".
        """
        carriers: List[Tuple[str, str]] = []
        auth_header = request.headers.get("Authorization", "")
        if auth_header.startswith("Bearer "):
            parts = auth_header.split()
            if len(parts) >= 2:
                carriers.append(("header", parts[1]))
        query_token = request.query.get("token")
        if query_token:
            carriers.append(("query", query_token))
        cookie = request.cookies.get(SESSION_TOKEN_COOKIE)
        if cookie:
            carriers.append(("cookie", cookie))
            decoded = urllib.parse.unquote(cookie)
            if decoded != cookie:
                carriers.append(("cookie", decoded))
        return carriers

    def _session_token_verdict(
        self, request: web.Request, settings: Any
    ) -> Optional[Tuple[str, str]]:
        """Authenticate an API request by token in secure mode.

        The master token (Bearer) counts as a controller, as does a
        controller-role session token; a viewer-role token is tagged so the
        handlers that refuse view-only credentials refuse it too. Session
        tokens are looked up in the same constant-time table lookup the
        WebSocket handshakes use.

        Returns:
            ``(role_ceiling, source)`` for the first carrier holding a valid
            token, or None when none does.
        """
        # Resolved per call: selkies imports this module, so the token table
        # it owns cannot be imported at module load.
        from .sessions import _lookup_session_token

        if self._check_master_token(request.headers.get("Authorization"), settings.master_token):
            return "controller", "header"
        for source, token in self._session_token_carriers(request):
            perms = _lookup_session_token(token)
            if perms is not None:
                role = "controller" if perms.get("role") == "controller" else "viewer"
                return role, source
        return None

    @classmethod
    def _fetched_cross_site(cls, request: web.Request, settings: Any) -> bool:
        """Whether a browser sent this request for another site's page.

        A GET from another site's image, link, or form carries no Origin but
        does carry `Sec-Fetch-Site`, and the browser attaches a cached Basic
        login to it, so a route whose GET changes something holds it to this:
        an Origin the operator allows (`_is_origin_allowed`) still passes, and
        clients that are not browsers send neither header.
        """
        if request.headers.get("Sec-Fetch-Site") in ("cross-site", "same-site"):
            return not (request.headers.get("Origin") and cls._is_origin_allowed(request, settings))
        return not cls._is_origin_allowed(request, settings)

    @staticmethod
    def _is_origin_allowed(request: web.Request, settings: Any) -> bool:
        """Return whether a browser request's Origin is permitted.

        Applied to WebSocket upgrades and to the mode-switch POST. Empty
        ``allowed_origins`` means same-origin only (plus non-browser clients
        that send no Origin); ``*`` allows any; otherwise the Origin must be
        listed or match the Host header. A forwarded Host without a port
        (nginx's ``proxy_set_header Host $host``, as the bundled config does)
        is matched by hostname: a browser Origin carries any non-default port,
        so a strict netloc comparison would reject every same-origin
        connection reached via an explicit port such as the :6080 mapping.
        """
        origin = request.headers.get("Origin")
        if not origin:
            return True
        allowed = {
            o.strip()
            for o in (getattr(settings, "allowed_origins", "") or "").split(",")
            if o.strip()
        }
        if "*" in allowed or origin in allowed:
            return True
        host = request.headers.get("Host")
        if host:
            try:
                origin_parts = urllib.parse.urlsplit(origin)
                if origin_parts.netloc == host:
                    return True
                host_parts = urllib.parse.urlsplit("//" + host)
                if (
                    host_parts.port is None
                    and origin_parts.hostname
                    and origin_parts.hostname == host_parts.hostname
                ):
                    return True
            except ValueError:
                pass
        return False

    @web.middleware
    async def _auth_middleware(
        self,
        request: web.Request,
        handler: Callable[[web.Request], Awaitable[web.StreamResponse]],
    ) -> web.StreamResponse:
        """Global auth guard for every route on the server: see ``_authorize``.

        A refusal is deliberately taken before the request's body is read -- an
        unauthenticated upload must not be paid for -- so the connection is closed
        with it. Leaving those bytes unread on a keep-alive connection has the
        server parse them as the next request's method, which fails that request
        and every later one on the same connection.
        """
        response = await self._authorize(request, handler)
        if response.status >= 400 and request.body_exists and request.can_read_body:
            response.force_close()
        return response

    async def _authorize(
        self,
        request: web.Request,
        handler: Callable[[web.Request], Awaitable[web.StreamResponse]],
    ) -> web.StreamResponse:
        """Authorize one request and hand it to `handler`, or refuse it.

        Layered gates, in order: cross-site WebSocket upgrades are rejected by
        Origin; health/liveness endpoints pass without credentials; the token
        endpoint and the control endpoints (the mode switch, a session
        disconnect, a recording start or stop) accept the Bearer master token,
        trying `Authorization` first and the `MASTER_TOKEN_HEADER` fallback
        second for callers whose Authorization header a Basic login in front
        already owns (a
        mode switch not so authenticated is held to the same Origin rule as
        the upgrades, since a browser attaches cached Basic credentials to a
        cross-site POST); in secure mode every other API route accepts a
        session token (Bearer header, ``?token=`` query, or the client's
        cookie), which is the only credential when Basic auth is off; the mode
        switch takes the master token ahead of the Origin rule and otherwise
        authenticates like any other API route, its handler refusing every
        role but controller; and everything else falls through to Basic Auth
        when enabled. A cookie-
        carried token on a state-changing request is held to the Origin rule
        too, since the browser attaches it; and with a master token set, the
        WebSocket handshakes skip Basic (a browser cannot attach fresh Basic
        credentials to a handshake, so it would add only an undebuggable
        401). A request that authenticates with the view-only password or a
        viewer-role token is tagged with ``auth_role_ceiling = "viewer"`` for
        downstream handlers to enforce.
        """
        settings = request.app["settings"]
        auth_header = request.headers.get("Authorization")
        path = request.path
        is_ws_upgrade = request.headers.get("Upgrade", "").lower() == "websocket"
        # Match the exact route, not a suffix, so /foo/tokens isn't treated as control-plane.
        api_prefix = settings.subfolder
        is_ws_handshake = is_ws_upgrade and path.rstrip("/") in {
            f"{api_prefix}{route}" for route in WEBSOCKET_ROUTES}
        if is_ws_upgrade:
            if not self._is_origin_allowed(request, settings):
                logger.warning(
                    "Rejected WebSocket upgrade from disallowed Origin: %r",
                    request.headers.get("Origin", ""),
                )
                return web.Response(status=403, text="Forbidden origin")
        if path in (f"{api_prefix}/api/status", f"{api_prefix}/api/health"):
            return await handler(request)
        token_path = path == f"{api_prefix}/api/tokens"
        if settings.master_token and token_path:
            if not any(
                self._check_master_token(header, settings.master_token)
                for header in (auth_header, request.headers.get(MASTER_TOKEN_HEADER))
            ):
                return self._bearer_challenge()
            return await handler(request)

        # The operator's own paths, ahead of the Origin rule a browser is held
        # to; a session token reaches them through the verdict below.
        is_control_path = path == f"{api_prefix}/api/switch" or (
            request.method in ("POST", "DELETE")
            and (path == f"{api_prefix}/api/recording"
                 or path.startswith(f"{api_prefix}/api/sessions/")))
        if settings.master_token and is_control_path:
            if any(
                self._check_master_token(header, settings.master_token)
                for header in (auth_header, request.headers.get(MASTER_TOKEN_HEADER))
            ):
                return await handler(request)
        if is_control_path and not self._is_origin_allowed(request, settings):
            logger.warning(
                "Rejected %s %s from disallowed Origin: %r",
                request.method, path, request.headers.get("Origin", ""),
            )
            return web.Response(status=403, text="Forbidden origin")

        if (settings.master_token and not is_ws_handshake and not token_path
                and path.startswith(f"{api_prefix}/api/")):
            verdict = self._session_token_verdict(request, settings)
            if verdict is not None:
                ceiling, source = verdict
                if (source == "cookie" and request.method not in ("GET", "HEAD")
                        and not self._is_origin_allowed(request, settings)):
                    logger.warning(
                        "Rejected cookie-authenticated %s %s from disallowed Origin: %r",
                        request.method, path, request.headers.get("Origin", ""),
                    )
                    return web.Response(status=403, text="Forbidden origin")
                request["auth_role_ceiling"] = ceiling
                return await handler(request)
            if not settings.enable_basic_auth[0]:
                return self._bearer_challenge()

        if not settings.enable_basic_auth[0]:
            logger.debug("Basic auth not enabled, forwarding to routers")
            return await handler(request)
        if is_ws_handshake and settings.master_token:
            return await handler(request)
        if not auth_header or not auth_header.startswith("Basic "):
            if is_ws_upgrade:
                # A WS handshake gets no challenge/retry: without this log the
                # rejection is invisible on both ends.
                logger.warning(
                    "Rejected WebSocket upgrade from %s: basic auth is enabled and the "
                    "handshake carried no Authorization header (browsers only attach "
                    "cached credentials; set a master token or disable basic auth for "
                    "browser clients behind proxies).",
                    request.remote,
                )
            return self._basic_auth_challenge("Authorization Required")
        try:
            auth_decoded = self._b64_decode(auth_header[6:])
            if ":" not in auth_decoded:
                return self._basic_auth_challenge()
            username, password = auth_decoded.split(":", 1)
            # Compare as UTF-8 bytes; hmac.compare_digest rejects non-ASCII str.
            user_ok = hmac.compare_digest(
                username.encode("utf-8"), str(settings.basic_auth_user).encode("utf-8")
            )
            pw = password.encode("utf-8")
            main_ok = hmac.compare_digest(
                pw, str(settings.basic_auth_password).encode("utf-8")
            )
            # Both comparisons run so reply timing never reveals which password was sent.
            viewonly_secret = str(getattr(settings, "basic_auth_viewonly_password", "") or "")
            viewonly_ok = bool(viewonly_secret) and hmac.compare_digest(
                pw, viewonly_secret.encode("utf-8")
            )
            if not (user_ok and (main_ok or viewonly_ok)):
                logger.warning(
                    f"Invalid credentials provided for user: {settings.basic_auth_user}"
                )
                return self._basic_auth_challenge()
            request["auth_role_ceiling"] = (
                "viewer" if (viewonly_ok and not main_ok) else "controller"
            )
        except Exception:
            return self._basic_auth_challenge()
        return await handler(request)

    def _require_configured_credentials(self) -> None:
        """Refuse to serve a login that nobody chose a password for.

        The built-in password is a placeholder, not a credential: reaching this point
        still carrying it means no password was set anywhere, and the server would
        otherwise put an unconfigured login on the network. What counts is that a value
        was supplied, on the command line or in the environment — not what the value is.
        An image that ships its own default is choosing it deliberately, the way most
        container images do, and rejecting known-weak values here would break every one
        of them while stopping nobody who meant it.

        Raises:
            SystemExit: With `EXIT_CONFIG_ERROR`, which says the settings are the
                problem and that starting again will not change the answer — a
                supervisor respawning this forever would otherwise leave a
                container that looks healthy and never answers.
        """
        if not self.settings.enable_basic_auth[0]:
            return
        if self.settings.was_provided("basic_auth_password"):
            return
        logger.error(
            "Basic authentication is enabled but no password was set. Set one with "
            "--basic-auth-password, or the SELKIES_BASIC_AUTH_PASSWORD, PASSWORD, or "
            "PASSWD environment variable; or serve without a login by passing "
            "--enable-basic-auth=false."
        )
        raise SystemExit(EXIT_CONFIG_ERROR)

    def _require_one_listen_setting(self) -> None:
        """Refuse `--public` beside an explicit `--addr`: both choose the TCP
        listen addresses, and letting one win in silence would leave the
        operator who typed the other believing it took effect.

        Raises:
            SystemExit: With `EXIT_CONFIG_ERROR`, as for missing credentials.
        """
        if self.settings.public[0] and self.settings.was_provided("addr"):
            logger.error(
                "--public and --addr (SELKIES_PUBLIC, SELKIES_ADDR) were both given: "
                "--public listens on every interface, --addr on the addresses it "
                "names. Pass one of them."
            )
            raise SystemExit(EXIT_CONFIG_ERROR)

    async def switch_to_mode(self, mode_name: str) -> None:
        """Stop the active streaming service and start ``mode_name`` in its place.

        Serialized under the supervisor lock so two switches can never overlap;
        switching to the already-active mode is a no-op. The service reads
        the settings at start, so the encoder knob is brought in line with the
        transport before it starts (a websockets-only encoder such as jpeg or
        striped h264enc cannot ride the WebRTC pipeline, and a switch back
        restores the operator's menu and value), and only then does an unset
        paint-over resolve, since its default depends on the resolved encoder
        (JPEG keeps it under Turbo), the order `_post_process_settings` keeps.

        Args:
            mode_name: Registered service name ("websockets" or "webrtc").

        Raises:
            ValueError: ``mode_name`` is not a registered service.
        """
        if mode_name not in self.services:
            raise ValueError(f"Service {mode_name} not found")

        async with self.lock:
            if self.current_mode == mode_name:
                logger.info(f"Mode {mode_name} is already active.")
                return

            await self._stop_service()
            logger.info(f"Starting service: {mode_name}")
            self.settings.mode = mode_name
            self.settings.apply_webrtc_encoder_filter()
            self.settings.resolve_paint_over_default()
            self.settings.resolve_cpu_encode_threading_default()
            service = self.services[mode_name]
            task = asyncio.create_task(service.start())
            self.active_task = task
            self.current_mode = mode_name

            def _on_service_done(finished: asyncio.Task, mode: str = mode_name) -> None:
                """Clear the stale mode when the service dies unexpectedly; the
                exception is retrieved so asyncio never logs it as unretrieved."""
                if finished.cancelled():
                    return
                exc = finished.exception()
                if exc is not None:
                    logger.error(f"Service '{mode}' terminated unexpectedly: {exc!r}")
                    if self.active_task is finished:
                        self.current_mode = None
                        self.active_task = None

            task.add_done_callback(_on_service_done)

    async def _stop_service(self) -> None:
        """Stop the active service, escalating to a forced cancel on timeout.

        The grace period lets a teardown that includes ~2 s gamepad-close
        waits finish before a forced cancel that would leak resources.
        """
        if not self.current_mode:
            return
        logger.info(f"Stopping service: {self.current_mode}")

        await self.services[self.current_mode].stop()
        if self.active_task:
            try:
                await asyncio.wait_for(self.active_task, timeout=15)
            except asyncio.TimeoutError:
                logger.warning(
                    f"Timeout while stopping '{self.current_mode}'. Canceling task."
                )
                self.active_task.cancel()
                try:
                    await self.active_task
                except asyncio.CancelledError:
                    logger.info(
                        f"Task canceled after timeout for '{self.current_mode}'."
                    )
                except Exception as e:
                    logger.warning(f"Service task raised during forced stop: {e!r}")
        self.current_mode = None
        self.active_task = None

    def _get_status(self) -> Dict[str, Any]:
        """The /api/status body.

        `enable_dual_mode` is surfaced here so the dashboard can render the
        WebSocket/WebRTC toggle from this early, transport-independent probe:
        serverSettings only arrives once a stream connects, so a WebRTC
        session that never comes up would otherwise strand the user with no
        way back to WebSockets.
        """
        return {
            "current_mode": self.current_mode,
            "available_modes": list(self.services.keys()),
            "enable_dual_mode": bool(
                getattr(self.settings, "enable_dual_mode", (False,))[0]
            ),
        }

    @staticmethod
    def _viewer_ceiling(request: web.Request) -> bool:
        """Return whether the credential that authenticated this request caps it
        at the viewer role (the view-only basic-auth password, or a viewer-role
        session token in secure mode).

        Every endpoint that changes host or session state refuses those
        requests, the way the streaming plane refuses input from a viewer, and
        so does every one that hands out more than a viewer watches: the files
        under the file-manager directory and printed documents. What the
        session shows anyway (its pages, a screenshot, whether a recording
        runs) stays available.
        """
        return request.get("auth_role_ceiling") == "viewer"

    async def handle_switch(self, request: web.Request) -> web.Response:
        """POST /api/switch: change the active streaming mode.

        A controller may switch: it already drives the desktop, so withholding
        the transport from it protects nothing. What the credential does decide
        is who counts as one — view-only credentials and viewer-role tokens are
        refused here, and the master token remains the operator's way in for a
        deployment that hands out tokens it does not want switching for
        everyone (the switch restarts the service under every connected page).
        Refused as well when dual mode is disabled.
        """
        if self._viewer_ceiling(request):
            return web.json_response(
                {"status": "error", "message": "View-only credentials cannot switch streaming mode"},
                status=403,
            )
        dual_mode = getattr(self.settings, "enable_dual_mode", (False,))[0]
        if not dual_mode:
            return web.json_response(
                {"status": "error", "message": "Dual streaming mode disabled"},
                status=403,
            )

        try:
            data = await request.json()
            if not isinstance(data, dict):
                raise ValueError("Request body must be a JSON object")
            target_mode = data.get("mode")
            await self.switch_to_mode(target_mode)
            return web.json_response({"status": "success", "mode": target_mode})
        except Exception as e:
            return web.json_response({"status": "error", "message": str(e)}, status=400)

    def _session_gauge(self, request: web.Request) -> Optional[UplinkGauge]:
        """The `UplinkGauge` for one transfer request, upload or download, or
        None when nothing can be gauged (the transfer then must not be
        throttled blindly).

        The gauge wants the requester's own session connections — another
        client's socket crosses a different path, and its jitter would
        throttle a transfer that cannot be queuing there. The active service
        lists every client session; the requester's are narrowed by session
        token when the request carries one, else by peer address, and when
        neither discriminates the whole list is gauged: with a floor per
        session, a foreign session can only ever add false congestion, which
        slows the transfer and never harms a stream.

        Gauge state (ping clock and floor history) lives in
        `_UPLINK_SESSIONS`, keyed weakly by session websocket, so the separate
        requests of a chunked transfer's slices share one floor and a
        mid-transfer slice never re-baselines against its own standing queue.
        """
        service = self.services.get(self.current_mode) if self.current_mode else None
        if service is None:
            return None
        conns = service.uplink_session_conns()
        if not conns:
            return None
        tokens = {token for _source, token in self._session_token_carriers(request)}
        if tokens:
            matched = [c for c in conns if c[1] and c[1] in tokens]
            if matched:
                conns = matched
        if len(conns) > 1 and request.remote:
            matched = [c for c in conns if c[2] == request.remote]
            if matched:
                conns = matched
        entries = []
        for ws, _token, _ip in conns:
            state = _uplink_session_state(ws)
            entries.append([ws, state, state["seq"]])
        return UplinkGauge(entries) if entries else None

    async def _stream_upload_body(
            self, request: web.Request, leaf: str, dir_fd: int, append: bool) -> int:
        """Stream a request body into the file `leaf` names in the open
        directory `dir_fd`, opening, writing, and closing it on the executor.

        Creates the file when ``append`` is False, refusing one that already
        exists, and appends when True. `O_NOFOLLOW` blocks a symlink planted
        over the leaf's name, and no component above it is re-walked by name:
        the `dir_fd` is the descriptor the path confinement opened, so a
        symlink swapped in over an intermediate component cannot redirect the
        write outside the root. Enforces the declared Content-Length.

        Reads pace against two allowances: a paused read fills aiohttp's
        flow-control buffer, the TCP window closes, and the client's uplink is
        freed for the input/feedback traffic the stream depends on. The
        operator's static cap is the shared `transfer_cap` bucket; congestion
        control is `upload_pacer` fed one `UplinkGauge` verdict per read, so
        the transfer takes whatever the uplink has spare and backs off the
        moment the session's own delay inflates. A transfer below
        `TRANSFER_MIN_GAUGED_BYTES` is over before a queue could matter and
        skips the gauge, as does one with no gauge-able session socket; a
        gauge whose sockets all vanish mid-transfer stops pacing — the
        session it protected is gone. A gauged transfer's reads are sized to
        the current rate (`_gauged_chunk_size`), since the read size is what
        the client is handed back as receive window and sends at once.

        Args:
            request: The upload request whose body is streamed.
            leaf: The staging file's name in the directory `dir_fd` opens.
            dir_fd: The held descriptor of the destination's directory.
            append: Append to the file instead of creating it.

        Returns:
            The byte count written.

        Raises:
            Exception: Propagated from the read/write path after the handle is
                closed; the caller owns removal of the target file.
        """
        declared = request.content_length
        loop = asyncio.get_running_loop()
        cap = self.transfer_cap if self.transfer_cap.active else None
        gauge = (
            self._session_gauge(request)
            if (declared or 0) >= TRANSFER_MIN_GAUGED_BYTES
            else None)
        flags = os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW | (os.O_APPEND if append else os.O_EXCL)
        fh = await loop.run_in_executor(
            None, lambda: os.fdopen(os.open(leaf, flags, 0o644, dir_fd=dir_fd), "wb"))
        written = 0
        try:
            while True:
                read_size = (
                    _gauged_chunk_size(self.upload_pacer) if gauge is not None
                    else 1 << 20)
                chunk = await request.content.read(read_size)
                if not chunk:
                    break
                if declared is not None and written + len(chunk) > declared:
                    raise ValueError("body exceeds declared Content-Length")
                if cap is not None:
                    await cap.pace(len(chunk))
                if gauge is not None:
                    verdict = await gauge.sample()
                    if gauge.alive:
                        await self.upload_pacer.pace_verdict(len(chunk), verdict, gauge.inflation_us)
                    else:
                        gauge = None
                await loop.run_in_executor(None, fh.write, chunk)
                written += len(chunk)
            await loop.run_in_executor(None, fh.close)
        except Exception:
            try:
                await loop.run_in_executor(None, fh.close)
            except Exception:
                pass
            raise
        return written

    def _discard_chunked_upload(self, dest: str, part_path: str) -> None:
        """Drop a chunked transfer's tracking entry and its on-disk .part file.

        The entry and the file it names go together with nothing awaited
        between, so a restart on the same path cannot slip its own entry in
        between and lose it; the removal blocks the loop the once a discard
        costs, which a transfer's slices do not.
        """
        self._chunked_uploads.pop(dest, None)
        _remove_quietly(part_path)

    def _expire_stale_chunked_uploads(self) -> None:
        """Reap transfers idle past UPLOAD_PART_TTL_SECONDS (entry + .part file)."""
        now = time.monotonic()
        for key in [k for k, s in self._chunked_uploads.items()
                    if now - s["ts"] > UPLOAD_PART_TTL_SECONDS and not s["busy"]]:
            self._discard_chunked_upload(key, self._chunked_uploads[key]["part"])
            logger.debug(f"Expired stale chunked upload: {key}")

    async def handle_upload(self, request: web.Request) -> web.Response:
        """Stream a client file upload to the file-manager directory over HTTP.

        Available in every streaming mode and not bounded by the data-channel /
        WebSocket per-message size, so it saturates the link where the per-chunk
        SCTP path cannot. The destination path (relative to the file-manager
        root) arrives URL-encoded in the X-Upload-Path header. The body streams
        to disk on the executor, and so do the directory creation, the staging
        open, and the plain path's rename onto the destination, since a network
        filesystem takes tens of milliseconds over each and a directory of
        small files pays it once per file, which the event loop would otherwise
        take from the stream it serves. A chunked transfer is tracked by its
        destination, so its finalize and its discards stay synchronous to keep
        the rename and the entry's removal atomic against a restart on the same
        path; each is a once-per-transfer cost, not a per-slice one. Path safety
        mirrors the data-channel path: no traversal outside the root, and the
        write lands in a directory descriptor the confinement walked and
        verified, with `O_NOFOLLOW` on the staging leaf itself.

        Two request shapes share the endpoint:

        - Plain: one POST carrying the whole file, no chunk headers — staged in a
          hidden sibling of the destination and renamed onto it when the body is
          complete.
        - Chunked (the client slices files above its 64 MiB threshold so no
          single request body exceeds a fronting proxy's per-request cap, e.g.
          Cloudflare's 100 MB): sequential POSTs for the same X-Upload-Path,
          each also carrying
            X-Upload-Id:     opaque client-chosen transfer id
            X-Upload-Offset: absolute byte offset of this slice
            X-Upload-Total:  final file size in bytes
            X-Upload-Final:  "1" on the last slice
          Slices accumulate in a staging sibling that offset 0 creates
          (_upload_staging_path) and the transfer's tracking entry names; offset
          0 also removes the one an abandoned transfer to the same path left,
          and non-zero offsets must exactly continue the tracked transfer (same
          id, offset equal to the bytes already banked, matching staged size) or
          the transfer is discarded with 409. The final slice validates the
          accumulated size against X-Upload-Total and renames the staged file
          onto the destination atomically. Transfers idle past
          UPLOAD_PART_TTL_SECONDS are expired on the next chunked request.

        Both shapes carry the mode of a file of this user's they replace onto
        the replacement (`_carry_destination_mode`) and are refused for
        view-only credentials (the view-only password, a viewer-role session
        token).
        """
        settings = request.app["settings"]
        rel = urllib.parse.unquote(request.headers.get("X-Upload-Path", "") or "")

        def failed(message: str, status: int = 400) -> web.Response:
            """Refuse the transfer, recording the attempt and why it was refused."""
            audit.emit("file.upload.error", filename=rel, error=message)
            return web.json_response({"status": "error", "message": message}, status=status)

        if self._viewer_ceiling(request):
            return failed("View-only credentials cannot upload files", 403)
        if "upload" not in settings.file_transfers:
            return failed("uploads disabled", 403)
        root = getattr(settings, "file_manager_path", "") or ""
        if not root:
            return failed("uploads disabled", 403)
        root = os.path.normpath(os.path.expanduser(root))
        parts = self._file_parts(rel)
        if not parts:
            return failed("invalid upload path")
        dest = os.path.normpath(os.path.join(root, *parts))
        if not dest.startswith(os.path.join(root, "")):
            return failed("invalid upload path")
        # The destination's name in its directory, which the rename below acts
        # on through the directory's held descriptor.
        leaf = os.path.basename(dest)
        name = "/".join(parts)

        upload_id = request.headers.get("X-Upload-Id")
        offset_header = request.headers.get("X-Upload-Offset")
        if (upload_id is None) != (offset_header is None):
            return failed("X-Upload-Id and X-Upload-Offset must be sent together")

        total = -1
        if upload_id is not None:
            try:
                offset = int(offset_header or "")
                total = int(request.headers["X-Upload-Total"]) if "X-Upload-Total" in request.headers else -1
            except ValueError:
                return failed("malformed chunk headers")
            if offset < 0 or ("X-Upload-Total" in request.headers and total < 0):
                return failed("malformed chunk headers")

        # The write lands in a directory held open by the confinement
        # (`_open_dir_fd`): every component down to it was opened relative to
        # the one before and verified from the descriptor it opened to, so a
        # symlink planted over an intermediate name after the request's path
        # was validated cannot redirect the body or the final rename outside
        # the root, and `O_NOFOLLOW` on the staging leaf does the same for the
        # file itself.
        try:
            parent_fd = await asyncio.to_thread(self._open_dir_fd, parts[:-1], True)
        except FileRootEscape:
            return failed("path escape rejected")
        except OSError as e:
            return failed(f"mkdir failed: {e}", 500)
        try:
            if upload_id is None:
                staging = _staging_leaf_name()
                try:
                    written = await self._stream_upload_body(
                        request, staging, parent_fd, append=False)
                except Exception as e:
                    await asyncio.to_thread(_remove_quietly, staging, parent_fd)
                    return failed(str(e))
                try:
                    await asyncio.to_thread(_finalize_upload, staging, leaf, parent_fd)
                except OSError as e:
                    await asyncio.to_thread(_remove_quietly, staging, parent_fd)
                    return failed(f"finalize failed: {e}", 500)
                logger.info(f"HTTP upload finished: {dest} ({written} bytes)")
                audit.emit("file.upload.end", filename=name, size_bytes=written)
                return web.json_response({"status": "success", "bytes": written})

            self._expire_stale_chunked_uploads()
            final = request.headers.get("X-Upload-Final") == "1"
            state = self._chunked_uploads.get(dest)
            if offset == 0:
                if state is not None and state["busy"]:
                    return web.json_response(
                        {"status": "error", "message": "another chunk for this path is in flight"},
                        status=409,
                    )
                if state is not None:
                    self._discard_chunked_upload(dest, state["part"])
                state = {"id": upload_id, "offset": 0, "ts": time.monotonic(),
                         "part": _upload_staging_path(dest), "busy": False}
                self._chunked_uploads[dest] = state
            else:
                if state is not None and state["busy"]:
                    # Never discard here: the in-flight writer owns the .part.
                    return web.json_response(
                        {"status": "error", "message": "another chunk for this path is in flight"},
                        status=409,
                    )
                try:
                    part_size = os.path.getsize(state["part"]) if state is not None else -1
                except OSError:
                    part_size = -1
                if (state is None or state["id"] != upload_id
                        or state["offset"] != offset or part_size != offset):
                    if state is not None:
                        self._discard_chunked_upload(dest, state["part"])
                    return failed(f"chunk sequence mismatch at offset {offset}; transfer discarded", 409)
            part_path = state["part"]
            part_leaf = os.path.basename(part_path)

            # Claimed before the body's awaits so a slice for this path that arrives
            # meanwhile is refused, not interleaved onto the shared .part.
            state["busy"] = True
            try:
                written = await self._stream_upload_body(
                    request, part_leaf, parent_fd, append=offset > 0)
            except Exception as e:
                self._discard_chunked_upload(dest, part_path)
                return failed(str(e))
            state["busy"] = False
            state["offset"] = offset + written
            state["ts"] = time.monotonic()

            if not final:
                return web.json_response({"status": "success", "bytes": state["offset"], "complete": False})

            received = state["offset"]
            if total >= 0 and received != total:
                self._discard_chunked_upload(dest, part_path)
                return failed(f"size mismatch: received {received}, expected {total}")
            # Synchronous, like the removal: a transfer is tracked by its destination,
            # so a restart on the same path cannot run between the rename and the entry's removal.
            try:
                _finalize_upload(part_leaf, leaf, parent_fd)
            except OSError as e:
                self._discard_chunked_upload(dest, part_path)
                return failed(f"finalize failed: {e}", 500)
            self._chunked_uploads.pop(dest, None)
            logger.info(f"HTTP chunked upload finished: {dest} ({received} bytes)")
            audit.emit("file.upload.end", filename=name, size_bytes=received)
            return web.json_response({"status": "success", "bytes": received, "complete": True})
        finally:
            try:
                os.close(parent_fd)
            except OSError:
                pass

    @staticmethod
    def _file_parts(rel: str) -> Optional[List[str]]:
        """Split a root-relative path into its name components, or None when
        it escapes the root. Empty and ``.`` components are dropped, and a
        ``..`` surviving the normalization is the escape; a path that
        normalizes to nothing is no components, which is the root itself. A
        NUL byte names nothing on the filesystem and is refused with the
        escapes, rather than raising out of the calls that would see it."""
        if "\x00" in rel:
            return None
        rel = rel.strip("/\\")
        parts = [c for c in os.path.normpath(rel).split(os.sep) if c and c != "."]
        if rel and ".." in parts:
            return None
        return parts

    def _resolve_file_path(self, rel: str) -> Optional[str]:
        """Resolve a root-relative path to a filesystem path at or under the
        file-manager directory. Returns None when the path escapes the root. An
        empty path resolves to the root itself, which browse may name and mkdir
        refuses through `_at_file_root`.

        The confinement is the `realpath`-then-prefix comparison the static
        path checker recognizes, the prefix suffixed with the separator and
        the root itself allowed, so a sibling whose name extends the root's is
        not read as inside it. The endpoints that act rather than list do not
        act on the returned string: they open the directory through
        `_open_dir_fd` and `_open_resolved_fd`, whose descriptor holds the
        object this check validated."""
        parts = self._file_parts(rel)
        if parts is None:
            return None
        base = str(self.upload_dir)
        if not parts:
            return base
        parent = os.path.realpath(os.path.join(base, *parts[:-1]))
        full = os.path.join(parent, parts[-1])
        # After following in-directory symlinks the entry must still sit at or
        # under the root; a leaf symlink out of the tree is refused too.
        root = os.path.join(base, "")
        for resolved in (parent, os.path.realpath(full)):
            if resolved != base and not resolved.startswith(root):
                return None
        return full

    def _open_dir_fd(self, parts: List[str], create: bool = False) -> int:
        """Open the directory `parts` names under the file-manager root as a
        file descriptor, descending one component per `os.open`, each named
        relative to the descriptor before it and verified from the descriptor
        it opened to rather than by the name it went in under.

        A symlinked component is followed the way the resolved string path
        followed it, and what makes that safe where the string was not is the
        verification: `/proc/self/fd` names where each open really landed, the
        kernel's own record of the object, which a name swapped after the open
        cannot change. A component that resolves outside the root — planted
        before the walk or in the instant of it — ends the walk with
        `FileRootEscape` instead of handing the caller an object to act
        outside with. With `create`, a missing component is made in place
        first, the way `os.makedirs` made the parents of an upload or a new
        folder; what the walk itself created is inside the descriptor it came
        from, so it needs no verification. POSIX only: `dir_fd` does not exist
        on Windows, and neither does this server.

        Args:
            parts: Name components under the root; empty names the root.
            create: Make a missing intermediate directory.

        Returns:
            A descriptor for the directory, the caller's to close.

        Raises:
            FileRootEscape: A component resolved outside the root.
            OSError: A component is missing or is not a directory.
        """
        base = str(self.upload_dir)
        root = os.path.join(base, "")
        fd = os.open(base, os.O_RDONLY | os.O_DIRECTORY)
        try:
            for part in parts:
                if part in ("", ".", "..") or os.sep in part or (
                        os.altsep and os.altsep in part):
                    raise ValueError(f"not a name component: {part!r}")
                try:
                    opened = os.open(part, os.O_RDONLY | os.O_DIRECTORY, dir_fd=fd)
                except FileNotFoundError:
                    if not create:
                        raise
                    # A concurrent request may make it first; either way it is there now.
                    with contextlib.suppress(FileExistsError):
                        os.mkdir(part, dir_fd=fd)
                    opened = os.open(part, os.O_RDONLY | os.O_DIRECTORY, dir_fd=fd)
                try:
                    resolved = os.readlink(f"/proc/self/fd/{opened}")
                except OSError:
                    resolved = ""
                if resolved != base and not resolved.startswith(root):
                    os.close(opened)
                    raise FileRootEscape(part)
                os.close(fd)
                fd = opened
            return fd
        except BaseException:
            os.close(fd)
            raise

    def _open_resolved_fd(self, parts: List[str]) -> Optional[Tuple[int, str]]:
        """Open the directory `parts` names under the file-manager root as a
        file descriptor, and return it with the canonical path it opened to,
        or None when that sits outside the root.

        The walk to the leaf verifies every intermediate component from the
        descriptor it opened (`_open_dir_fd`); the leaf itself is opened
        following symlinks and verified the same way, so an in-tree link to
        another directory in the tree stays listable the way the resolved
        string path allowed it while one resolving outside is refused. The
        verification reads `/proc/self/fd`, the kernel's own record of where
        the open landed, which a name swapped after the open cannot change:
        the caller acts on the verified object through the descriptor, never
        on a name that could have been swapped since.

        Args:
            parts: Name components under the root; empty names the root.

        Returns:
            The descriptor, the caller's to close, and its canonical path, or
            None when the entry resolved outside the root.

        Raises:
            NotADirectoryError: The leaf is not a directory. `O_DIRECTORY`
                refuses it before anything is read, so a FIFO cannot hold the
                open (and the worker thread making it) waiting for a writer.
            OSError: The leaf is missing or cannot be opened.
        """
        try:
            fd = self._open_dir_fd(parts[:-1])
        except FileRootEscape:
            return None
        try:
            if parts:
                opened = os.open(parts[-1], os.O_RDONLY | os.O_DIRECTORY, dir_fd=fd)
                os.close(fd)
                fd = opened
        except BaseException:
            os.close(fd)
            raise
        try:
            resolved = os.readlink(f"/proc/self/fd/{fd}")
        except OSError:
            os.close(fd)
            return None
        base = str(self.upload_dir)
        if resolved != base and not resolved.startswith(os.path.join(base, "")):
            os.close(fd)
            return None
        return fd, resolved

    def _at_file_root(self, target: str) -> bool:
        """Whether `target` is the file-manager directory itself rather than an entry within it.

        An empty request path, and the `.` and `a/..` spellings that normalize to
        one, all resolve to the root, so an endpoint that acts rather than lists
        refuses it: a directory is only ever created inside another, and creating
        the one the manager is rooted at is not an operation a client can mean.
        Browse may still name the root, which is the one directory the manager
        lists.
        """
        base = os.path.normcase(os.path.abspath(str(self.upload_dir)))
        return os.path.normcase(os.path.abspath(target)) == base

    def _viewer_or_disabled_files(self, request: web.Request) -> Optional[str]:
        """The message for a refused file-modification request, or None when the
        session is allowed to modify files. Uploads double as the writable
        toggle: a deployment that disables uploads serves the directory as
        read-only, and view-only credentials never modify files."""
        if self._viewer_ceiling(request):
            return "View-only credentials cannot modify files"
        if "upload" not in self.settings.file_transfers:
            return "File modifications disabled"
        if not str(self.upload_dir):
            return "File modifications disabled"
        return None

    async def _json_body(self, request: web.Request) -> Optional[Dict[str, Any]]:
        try:
            data = await request.json()
        except Exception:
            return None
        return data if isinstance(data, dict) else None

    def _browse_dir(self, parts: List[str], rel: str) -> Optional[List[Dict[str, Any]]]:
        """Open the directory the request names and stat it into JSON entries.

        Returns None when the entry resolved outside the root at open time,
        the race-only spelling of the refusal `_resolve_file_path` gives
        statically; a non-directory raises `NotADirectoryError`, which the
        handler answers like the missing case. The listing reads the pinned
        directory through its `/proc/self/fd` path — the descriptor the
        confinement opened, not a name that could have been swapped since the
        path was validated. Runs on the executor so a huge directory cannot
        stall the event loop that is streaming frames.

        Args:
            parts: Name components under the root; empty names the root.
            rel: The root-relative spelling the request used, for the entries'
                `path` fields.

        Returns:
            The entries, or None when the open landed outside the root.
        """
        opened = self._open_resolved_fd(parts)
        if opened is None:
            return None
        dir_fd, _ = opened
        try:
            if not stat.S_ISDIR(os.fstat(dir_fd).st_mode):
                raise NotADirectoryError(errno.ENOTDIR)
            entries: List[Dict[str, Any]] = []
            with os.scandir(f"/proc/self/fd/{dir_fd}") as it:
                for entry in it:
                    try:
                        st = entry.stat()
                        is_dir = entry.is_dir()
                    except OSError:
                        continue
                    rel_name = f"{rel}/{entry.name}".strip("/") if rel else entry.name
                    entries.append({
                        "name": entry.name,
                        "path": rel_name,
                        "is_dir": is_dir,
                        "size": None if is_dir else st.st_size,
                        "mtime": datetime.fromtimestamp(st.st_mtime).isoformat(),
                    })
        finally:
            os.close(dir_fd)
        entries.sort(key=lambda e: (not e["is_dir"], e["name"].lower()))
        return entries

    async def handle_files_browse(self, request: web.Request) -> web.Response:
        """JSON directory listing for the shared file manager. The listing is
        read-only, but it carries both gates of the HTML index: the download
        permission, and the viewer ceiling, since a viewer watches the desktop
        and never reaches the files on it.

        The listing reads the descriptor the confinement opened
        (`_open_resolved_fd`), never a path re-walked after the check, so a
        symlink swapped in after the path was validated cannot turn the
        listing into one of a directory outside the root."""
        if self._viewer_ceiling(request):
            return web.json_response({"status": "error", "message": "View-only credentials cannot list files"}, status=403)
        if "download" not in self.settings.file_transfers:
            return web.json_response({"status": "error", "message": "downloads disabled"}, status=403)
        rel_query = request.query.get("path", "")
        parts = self._file_parts(rel_query)
        if parts is None or self._resolve_file_path(rel_query) is None:
            return web.json_response({"status": "error", "message": "invalid path"}, status=400)
        rel = "/".join(parts)
        try:
            entries = await asyncio.to_thread(self._browse_dir, parts, rel)
        except (FileNotFoundError, NotADirectoryError):
            return web.json_response({"status": "error", "message": "not a directory"}, status=404)
        except PermissionError:
            return web.json_response({"status": "error", "message": "permission denied"}, status=403)
        if entries is None:
            return web.json_response({"status": "error", "message": "invalid path"}, status=400)
        return web.json_response({"status": "ok", "path": rel, "entries": entries})

    async def handle_files_mkdir(self, request: web.Request) -> web.Response:
        """POST /api/file-manager/mkdir: create a directory in the shared file
        manager. Refused for view-only credentials and where uploads are
        disabled, for a path that escapes the root, and for every spelling of
        the root itself, which nothing the manager offers has a use for: a
        directory is only ever created inside another. Refused to another
        site's page too (`_fetched_cross_site`): a form posting `text/plain`
        needs no preflight, `request.json` reads its body all the same, and the
        browser attaches a cached Basic login to it.

        The directory is made inside a descriptor of its parent that the
        confinement walked (`_open_dir_fd`), so a symlink planted over an
        intermediate name after the path was validated cannot move the
        creation outside the root. A name already taken is the `mkdir`'s own
        `FileExistsError`, not a check ahead of it that a concurrent request
        could invalidate."""
        refused = self._viewer_or_disabled_files(request)
        if refused:
            return web.json_response({"status": "error", "message": refused}, status=403)
        if self._fetched_cross_site(request, request.app["settings"]):
            logger.warning("Refused a new folder to another site's page (Origin %r)",
                           request.headers.get("Origin", ""))
            return web.json_response({"status": "error", "message": "Forbidden origin"}, status=403)
        data = await self._json_body(request)
        rel = (data or {}).get("path", "")
        if not isinstance(rel, str) or not rel.strip("/\\"):
            return web.json_response({"status": "error", "message": "invalid path"}, status=400)
        target = self._resolve_file_path(rel)
        if target is None or self._at_file_root(target):
            return web.json_response({"status": "error", "message": "invalid path"}, status=400)
        parts = self._file_parts(rel) or []

        def make() -> None:
            parent_fd = self._open_dir_fd(parts[:-1], create=True)
            try:
                os.mkdir(parts[-1], dir_fd=parent_fd)
            finally:
                os.close(parent_fd)

        try:
            await asyncio.to_thread(make)
        except FileRootEscape:
            return web.json_response({"status": "error", "message": "invalid path"}, status=400)
        except FileExistsError:
            return web.json_response({"status": "error", "message": "already exists"}, status=409)
        except NotADirectoryError:
            return web.json_response({"status": "error", "message": "a file is in the way"}, status=409)
        except OSError as e:
            return web.json_response({"status": "error", "message": str(e)}, status=500)
        audit.emit("file.mkdir", filename=rel)
        return web.json_response({"status": "ok"})

    async def handle_print_document(self, request: web.Request) -> web.StreamResponse:
        """GET /api/print/<name>: hand a printed document to the page that
        prints it, and take it out of the spool once it went out whole.
        Refused to view-only credentials: printing is the session owner's act.
        Refused to another site's page too (`_fetched_cross_site`), which would
        take the document out of the spool with the session's cached login."""
        if not self.settings.printing_enabled[0]:
            return web.Response(status=403, text="Forbidden: printing disabled")
        if self._fetched_cross_site(request, self.settings):
            logger.warning("Refused a printed document to another site's page (Origin %r)",
                           request.headers.get("Origin", ""))
            return web.Response(status=403, text="Forbidden origin")
        if self._viewer_ceiling(request):
            return web.Response(status=403, text="View-only credentials cannot take printed documents")
        name = printing.document_name(request.match_info.get("name", ""))
        base = str(self.print_spool)
        full = os.path.normpath(os.path.join(base, name)) if name else ""
        if not full.startswith(base + os.sep) or not os.path.isfile(full):
            return web.Response(status=404, text="No such document")
        return _AuditedFileResponse(pathlib.Path(full), name, event="print.document", remove=True,
                                    headers={"Content-Disposition": "inline"})

    async def handle_apps_catalog(self, request: web.Request) -> web.StreamResponse:
        """GET /api/apps/metadata.yml: the catalog of the proot-apps local
        repository the session installs from (`apps_catalog`). Nothing is
        served where no repository is configured, since the page then reads
        the remote catalog; a configured directory that is missing is the
        same answer, and the panel reports the catalog as failed to load
        rather than offering installs the runner cannot make."""
        text = apps_catalog.catalog_text(apps_repo_folder())
        if text is None:
            return web.Response(status=404, text="No local apps repository")
        return web.Response(text=text, content_type="text/yaml",
                            headers={"Cache-Control": "no-cache"})

    async def handle_apps_icon(self, request: web.Request) -> web.StreamResponse:
        """GET /api/apps/img/<name>: one icon of the local repository's
        catalog, served from its `metadata/img` directory and nowhere else."""
        path = apps_catalog.icon_path(apps_repo_folder(), request.match_info.get("name", ""))
        if path is None:
            return web.Response(status=404, text="No such icon")
        return web.FileResponse(pathlib.Path(path))

    def _active_service(self) -> Optional[BaseStreamingService]:
        return self.services.get(self.current_mode) if self.current_mode else None

    async def handle_sessions(self, request: web.Request) -> web.Response:
        """GET /api/sessions: the pages connected to the active transport."""
        service = self._active_service()
        return web.json_response({"sessions": await service.sessions() if service else []})

    async def handle_session_delete(self, request: web.Request) -> web.Response:
        """DELETE /api/sessions/<id>: close that page's connection. Refused to
        view-only credentials, like every change to the session."""
        if self._viewer_ceiling(request):
            return web.Response(status=403, text="View-only credentials cannot disconnect a session")
        service = self._active_service()
        if service is None or not await service.disconnect_session(request.match_info["id"]):
            return web.Response(status=404, text="No such session")
        return web.Response(status=204)

    async def handle_recording(self, request: web.Request) -> web.Response:
        """GET, POST, and DELETE /api/recording: the MP4 recording pixelflux
        makes of the session, H.264 with the session's audio as an Opus track
        when audio is on and pcmflux is installed. POST starts one into the
        file-manager directory unless the body names a path, DELETE stops it
        and GET reports on the current or last one. One recording at a time;
        starting a second or stopping none is a conflict."""
        try:
            import pixelflux
        except ImportError:
            return web.Response(status=501, text="pixelflux is not installed")
        if request.method == "GET":
            return web.json_response(pixelflux.recording_status() or {"active": False})
        if self._viewer_ceiling(request):
            return web.Response(status=403, text="View-only credentials cannot record")
        try:
            if request.method == "POST":
                body = await request.json() if request.can_read_body else {}
                path = str((body or {}).get("path") or "")
                if not path:
                    path = "recording-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + ".mp4"
                target = self._recording_target(path)
                if target is None:
                    logger.warning(f"Refused a recording outside the file-manager directory: {path!r}")
                    return web.Response(status=400, text="Recording path leaves the file-manager directory")
                path = target
                if (pixelflux.recording_status() or {}).get("active"):
                    raise RuntimeError("a recording is already active")
                audio_socket = await self._start_recording_audio(pixelflux)
                args = (path, None, audio_socket) if audio_socket else (path,)
                try:
                    status = await asyncio.to_thread(pixelflux.start_recording, *args)
                except RuntimeError:
                    await self._stop_recording_audio()
                    raise
                audit.emit("recording.start", filename=status.get("path", path))
            else:
                status = await asyncio.to_thread(pixelflux.stop_recording)
                await self._stop_recording_audio()
                audit.emit("recording.stop", filename=status.get("path", ""),
                           size_bytes=status.get("bytes", 0), duration_s=status.get("duration_s", 0.0),
                           frames=status.get("frames", 0))
        except json.JSONDecodeError:
            return web.Response(status=400, text="Body is not JSON")
        except RuntimeError as exc:
            logger.warning(f"Recording request refused: {exc}")
            return web.Response(status=409, text="Recording request refused; the server log names the reason")
        return web.json_response(status)

    def _recording_target(self, name: str) -> Optional[str]:
        """Where a requested recording name writes, or `None` when it leaves the
        file-manager directory. The name is a path relative to that directory:
        an absolute path, a traversal segment, and a symlinked parent pointing
        outside are all refused, since the session token that reaches this
        endpoint carries no authority over the rest of the filesystem."""
        base = os.path.realpath(self.upload_dir)
        if os.path.isabs(name):
            return None
        target = os.path.realpath(os.path.join(base, name))
        if not target.startswith(base + os.sep):
            return None
        return target

    async def _start_recording_audio(self, pixelflux: Any) -> str:
        """The Ogg Opus socket a recording's audio track is read from, served
        by a pcmflux capture of the session's sink that runs for the recorder
        alone with no Python callback, so no frame passes through Python.
        Unlike the live streams' captures it encodes silence rather than
        gating it, so the track spans the recording instead of stopping at
        every silence. Empty for a video-only recording: audio off, a
        pixelflux or pcmflux without the socket, or a capture that does not
        start."""
        try:
            if not self.settings.audio_enabled[0] or \
                    "audio_socket" not in inspect.signature(pixelflux.start_recording).parameters:
                return ""
            from pcmflux import AudioCapture
            from .audio_control import ensure_capture_sink, opus_capture_settings
            capture_settings = opus_capture_settings(self.settings.audio_device_name, self.settings.audio_channels,
                                                     int(self.settings.audio_bitrate), 20.0)
            capture_settings.use_silence_gate = False
            runtime = os.environ.get("XDG_RUNTIME_DIR") or tempfile.gettempdir()
            capture_settings.output_socket = os.path.join(runtime, f"selkies-record-audio-{os.getpid()}.sock")
        except (ImportError, AttributeError, ValueError):
            return ""
        capture = AudioCapture()
        try:
            await ensure_capture_sink(self.settings.audio_device_name)
            await asyncio.to_thread(capture.start_capture, capture_settings)
            if capture.state == "failed" or capture.last_error:
                raise RuntimeError(f"capture {capture.state}: {capture.last_error}")
        except (RuntimeError, OSError) as exc:
            logger.warning(f"Recording without audio: {exc}")
            await asyncio.to_thread(capture.stop_capture)
            return ""
        self._recording_audio = capture
        return capture_settings.output_socket

    async def _stop_recording_audio(self) -> None:
        capture, self._recording_audio = self._recording_audio, None
        if capture is not None:
            await asyncio.to_thread(capture.stop_capture)

    async def handle_screenshot(self, request: web.Request) -> web.Response:
        """GET /api/screenshot?display=<name>: a PNG of that display with the
        cursor drawn in, by pixelflux. On X11 the root, which holds every
        display."""
        try:
            import pixelflux
            screenshot = pixelflux.screenshot_png
        except (ImportError, AttributeError):
            return web.Response(status=501, text="pixelflux without screenshots")
        display = request.query.get("display", "primary")
        output = 0
        if self.settings.wayland[0] and display != "primary":
            from .display_utils import wayland_output_id
            output = wayland_output_id(display)
        try:
            png = await asyncio.to_thread(screenshot, output)
        except RuntimeError as exc:
            logger.warning(f"Screenshot of display {display!r} refused: {exc}")
            return web.Response(status=404, text="No such display")
        return web.Response(body=png, content_type="image/png")

    def pending_print_documents(self) -> List[Tuple[str, int]]:
        """The documents no page has taken yet, for a page that connects now."""
        return printing.pending(str(self.print_spool)) if self.print_watcher else []

    async def _announce_print_document(self, name: str, size: int) -> None:
        service = self.services.get(self.current_mode) if self.current_mode else None
        if service is not None:
            await service.announce_print_document(name, size)

    async def handle_status(self, _: web.Request) -> web.Response:
        """GET /api/status: current mode, available modes, dual-mode flag."""
        status = self._get_status()
        return web.json_response(status)

    async def handle_health(self, _: web.Request) -> web.Response:
        """GET /api/health: liveness probe, always 200."""
        return web.Response(text="OK")

    async def handle_metrics(self, request: web.Request) -> web.Response:
        """Prometheus exposition of the process-global registry."""
        data = await asyncio.to_thread(generate_latest)
        return web.Response(
            body=data,
            content_type='text/plain; version=1.0.0',
            headers={
                'Cache-Control': 'no-cache, no-store, must-revalidate',
                'Pragma': 'no-cache',
                'Expires': '0'
            }
        )

    async def _get_static_content_path(self) -> str:
        """Resolve the directory the static frontend is served from.

        A configured ``web_root`` wins when it holds an ``index.html``;
        otherwise the packaged web files are extracted to a temporary directory
        (importlib traversables are not guaranteed to be real files, and aiohttp
        static routes need a filesystem path).

        Returns:
            The directory path, or an empty string when no usable content exists.
        """
        web_path = ""

        web_root = getattr(self.settings, "web_root", "")
        if web_root:
            web_path = os.path.expanduser(web_root)
            if os.path.isdir(web_path) and os.path.isfile(os.path.join(web_path, "index.html")):
                logger.info(f"Using custom web_root directory: {web_path}")
                return web_path
            logger.warning(f"web_root directory {web_path} not found or missing index.html")

        logger.debug("Defaulting to packaged web files.")
        try:
            package_path = importlib_resources.files(self.STATIC_CONTENT_PATH)
            self.web_files_ctx = tempfile.TemporaryDirectory(prefix="selkies_web")
            temp_path = pathlib.Path(self.web_files_ctx.name)
            await asyncio.to_thread(self._copy_traversable, package_path, temp_path)

            if (temp_path / "index.html").exists():
                logger.debug(f"Using extracted package path from temp dir: {temp_path}")
                return str(temp_path)
            else:
                logger.warning("Packaged web content missing index.html")
                self.web_files_ctx.cleanup()
        except Exception as e:
            logger.error(f"Failed to extract packaged web files: {e}")
            # Unset when extraction failed before its assignment.
            if self.web_files_ctx is not None:
                try:
                    self.web_files_ctx.cleanup()
                except Exception:
                    pass

        return ""

    def _copy_traversable(self, src: Any, dst: pathlib.Path) -> None:
        """Recursively copy a Traversable (file or directory) to a filesystem path."""
        if src.is_file():
            with src.open('rb') as f_src, open(dst, 'wb') as f_dst:
                shutil.copyfileobj(f_src, f_dst)
        elif src.is_dir():
            dst.mkdir(exist_ok=True)
            for child in src.iterdir():
                self._copy_traversable(child, dst / child.name)

    async def _serve_paced_download(
        self,
        request: web.Request,
        path: pathlib.Path,
        *,
        audit_name: str,
        download_name: Optional[str] = None,
        allow_fallback: bool = True,
    ) -> web.StreamResponse:
        """Stream `path` to the page as an attachment with the download pacing,
        or answer with `_AuditedFileResponse` when nothing is paced and
        `allow_fallback` allows it.

        `download_name` is the name the attachment is offered under and
        `audit_name` the one the audit trail records, distinct because a
        generated archive is built under a temporary name and offered under the
        folder's. `allow_fallback` is False for a file that exists only for the
        length of the transfer: FileResponse opens it after the handler returns,
        by which time the temporary is gone. Such a file is never handed to
        FileResponse, so a request the streaming path cannot answer (a HEAD, a
        `Range` resume, a conditional revalidation) is refused with 405.

        The operator's static cap is the shared `transfer_cap` bucket, and
        congestion control is `download_pacer` fed one `UplinkGauge` verdict per
        chunk over the downloader's own session socket, so the transfer takes
        whatever the downlink has spare and backs off the moment the session's
        delay inflates. The gauge is end to end, so a reverse proxy or a fat
        modem buffer absorbing this host's writes still shows as the round trip
        it costs the session. Chunks are sized to the current rate
        (`_gauged_chunk_size`): each lands on the link as a burst, and a fixed
        256 KiB would read as congestion on any link that cannot swallow it
        inside the gauge's delay budget. A file below
        `TRANSFER_MIN_GAUGED_BYTES` skips the gauge, as does a request with no
        gauge-able session socket; with no cap either, nothing is paced.

        Pacing applies to plain GETs only: HEAD must answer instantly
        (StreamResponse.write is not empty-body-aware, so it would read and
        pace the whole file), a Range request is a resume or a seek that
        pacing a partial stream buys nothing for while FileResponse
        negotiates the 206, and a conditional request exists to be answered
        with 304/412 from validators only FileResponse computes.

        A client that cancels a download closes its connection mid-stream,
        which surfaces as a connection error from the write; that is the
        transfer's normal end, logged as such and never a handler failure.
        """
        filename = (download_name or path.name).encode("ascii", "replace").decode().replace('"', "_")
        quoted = urllib.parse.quote(download_name or path.name)
        conditional = any(
            name in request.headers
            for name in ("If-Modified-Since", "If-None-Match",
                         "If-Range", "If-Unmodified-Since")
        )
        size = (await asyncio.to_thread(path.stat)).st_size
        cap = self.transfer_cap if self.transfer_cap.active else None
        gauge = (
            self._session_gauge(request)
            if size >= TRANSFER_MIN_GAUGED_BYTES else None)
        if not ((cap is not None or gauge is not None or not allow_fallback)
                and request.method == "GET"
                and "Range" not in request.headers and not conditional):
            if not allow_fallback:
                return web.Response(status=405, headers={"Allow": "GET"})
            return _AuditedFileResponse(
                path, audit_name,
                headers={
                    "Content-Disposition":
                        f'attachment; filename="{filename}"; filename*=UTF-8\'\'{quoted}'
                },
            )
        logger.info(
            f"Download '{filename}' ({size} bytes): "
            f"{'gauged' if gauge is not None else 'ungauged'}"
            + (f", capped at {cap.static_bps / 125000:.1f} Mbit/s"
               if cap is not None else ""))
        content_type = (
            mimetypes.guess_type(download_name or path.name)[0]
            or "application/octet-stream"
        )
        response = web.StreamResponse(
            status=200,
            headers={
                "Content-Type": content_type,
                "Content-Length": str(size),
                "Accept-Ranges": "bytes",
                "Content-Disposition":
                    f'attachment; filename="{filename}"; filename*=UTF-8\'\'{quoted}',
            },
        )
        await response.prepare(request)
        fh = await asyncio.to_thread(open, path, "rb")
        sent = 0
        try:
            while True:
                chunk_size = (
                    _gauged_chunk_size(self.download_pacer)
                    if gauge is not None else 1 << 20)
                chunk = await asyncio.to_thread(fh.read, chunk_size)
                if not chunk:
                    break
                if cap is not None:
                    await cap.pace(len(chunk))
                if gauge is not None:
                    verdict = await gauge.sample()
                    if gauge.alive:
                        await self.download_pacer.pace_verdict(len(chunk), verdict, gauge.inflation_us)
                    else:
                        gauge = None
                await response.write(chunk)
                sent += len(chunk)
            await response.write_eof()
            audit.emit("file.download", filename=audit_name, size_bytes=size, partial=False)
        except ConnectionError as e:
            logger.info(
                f"Download '{filename}' stopped by the client "
                f"after {sent} of {size} bytes: {e}")
        finally:
            await asyncio.to_thread(fh.close)
        return response

    @staticmethod
    def _archive_source_bounds(dir_fd: int) -> Tuple[int, int]:
        """Walk the directory `dir_fd` holds without reading file contents and
        return the total size and count of the regular files an archive of it
        would carry, stopping as soon as either is past its bound.

        The walk is the one `_archive_files` makes (`os.fwalk`: a symlinked
        directory is never descended, and only regular files count), by
        `lstat` rather than an open; an entry that vanishes or turns
        unreadable mid-walk is skipped rather than failing the request.
        """
        total = 0
        count = 0
        for _current, _directories, filenames, current_fd in os.fwalk(".", dir_fd=dir_fd):
            for name in filenames:
                try:
                    st = os.stat(name, dir_fd=current_fd, follow_symlinks=False)
                except OSError:
                    continue
                if not stat.S_ISREG(st.st_mode):
                    continue
                total += st.st_size
                count += 1
                if total > ARCHIVE_MAX_SOURCE_BYTES or count > ARCHIVE_MAX_ENTRIES:
                    return total, count
        return total, count

    @staticmethod
    def _archive_files(dir_fd: int) -> Iterator[Tuple[str, int, os.stat_result]]:
        """Yield each regular file under the directory `dir_fd` holds: its
        name relative to that directory, an open descriptor of it (the
        caller's to close), and its status.

        `os.fwalk` never descends a symlinked directory, and each file is
        opened by name relative to its own directory's descriptor with
        `O_NOFOLLOW`, so no symlink, planted before the walk or swapped in
        during it, is followed out of the tree. `O_NONBLOCK` keeps a FIFO from
        holding the open, and anything the descriptor does not show to be a
        regular file is left out: a FIFO or a socket would wedge the read, and
        a device would inflate the archive without carrying the tree's
        content. An entry that vanishes or cannot be read is skipped.
        """
        for current, _directories, filenames, current_fd in os.fwalk(".", dir_fd=dir_fd):
            for name in filenames:
                try:
                    fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=current_fd)
                except OSError:
                    continue
                try:
                    st = os.fstat(fd)
                except OSError:
                    os.close(fd)
                    continue
                if not stat.S_ISREG(st.st_mode):
                    os.close(fd)
                    continue
                yield os.path.normpath(os.path.join(current, name)), fd, st

    @classmethod
    def _build_directory_archive(cls, dir_fd: int) -> str:
        """Write the regular files under the directory `dir_fd` holds to a zip
        archive in the system temporary directory, and return its path.

        Every file is read through the descriptor `_archive_files` opened, so
        a link inside the tree pointing out of it cannot put content outside
        the root into an archive the user asked for the tree's contents of,
        and neither can one swapped in while the pack runs. The pack holds to
        the bounds `_archive_source_bounds` checked as it reads, so a file that
        grows between the two cannot carry the archive past them. A
        modification time a zip entry cannot hold (before 1980 or after 2107)
        is clamped rather than failing the pack.

        Raises:
            ArchiveTooLarge: The files read past `ARCHIVE_MAX_SOURCE_BYTES` or
                `ARCHIVE_MAX_ENTRIES`.
        """
        fd, staged = tempfile.mkstemp(prefix="selkies-files-", suffix=".zip")
        os.close(fd)
        carried = 0
        entries = 0
        try:
            with zipfile.ZipFile(staged, "w", zipfile.ZIP_DEFLATED) as archive:
                for name, file_fd, st in cls._archive_files(dir_fd):
                    with os.fdopen(file_fd, "rb") as source:
                        entries += 1
                        if entries > ARCHIVE_MAX_ENTRIES:
                            raise ArchiveTooLarge(name)
                        info = zipfile.ZipInfo(name, _zip_date_time(st.st_mtime))
                        info.compress_type = zipfile.ZIP_DEFLATED
                        info.external_attr = (stat.S_IFREG | stat.S_IMODE(st.st_mode)) << 16
                        info.file_size = st.st_size
                        with archive.open(info, "w", force_zip64=st.st_size >= ARCHIVE_ZIP64_FROM) as sink:
                            while True:
                                chunk = source.read(1 << 20)
                                if not chunk:
                                    break
                                carried += len(chunk)
                                if carried > ARCHIVE_MAX_SOURCE_BYTES:
                                    raise ArchiveTooLarge(name)
                                sink.write(chunk)
        except BaseException:
            with contextlib.suppress(OSError):
                os.remove(staged)
            raise
        return staged

    def _pack_directory(self, parts: List[str]) -> Optional[str]:
        """Open the directory `parts` names under the file-manager root, bound
        it, and pack it (`_build_directory_archive`); return the archive's
        path, or None when the directory resolved outside the root.

        Runs whole on one `ARCHIVE_PACK_POOL` worker with the directory's
        descriptor: the descriptor is opened, walked, and closed by the same
        call, so a request abandoned while the pack runs cannot close it, or
        have its number reused, under the walk. The bound comes first, so a
        folder past `ARCHIVE_MAX_SOURCE_BYTES` or `ARCHIVE_MAX_ENTRIES` is
        refused before a byte is staged.

        Raises:
            ArchiveTooLarge: The folder holds more than one archive may carry.
            OSError: The directory is missing, is not one, or cannot be opened.
        """
        opened = self._open_resolved_fd(parts)
        if opened is None:
            return None
        dir_fd, _ = opened
        try:
            total, count = self._archive_source_bounds(dir_fd)
            if total > ARCHIVE_MAX_SOURCE_BYTES or count > ARCHIVE_MAX_ENTRIES:
                raise ArchiveTooLarge("/".join(parts))
            return self._build_directory_archive(dir_fd)
        finally:
            os.close(dir_fd)

    async def handle_files_zip(self, request: web.Request) -> web.StreamResponse:
        """GET /api/file-manager/zip?path=<dir>: hand a directory to the page
        as one zip archive.

        A folder has no single file to fetch, so the archive is what a folder's
        Download button asks for. `zipfile` cannot stream, so the folder is
        packed to a temporary file first and the response starts once the whole
        archive exists; the wait before the first byte is that packing, and the
        temporary is unlinked once the transfer ends however it ends. Symlinks
        are not followed (`_archive_files`). The root is refused like every
        endpoint that acts rather than lists, so a folder download is always a
        folder within the manager and never the whole served tree in one
        request. View-only credentials are refused the archive like the listing
        and the bytes (`_viewer_ceiling`): a viewer watches the desktop, it
        does not take the files on it. Another site's page is refused too
        (`_fetched_cross_site`): an image or a link there would spend the pack
        on the session's cached login, and the page's own Download button is
        a same-origin navigation.

        The folder is bounded before anything is staged, and a folder past
        `ARCHIVE_MAX_SOURCE_BYTES` or `ARCHIVE_MAX_ENTRIES` is refused with
        413, because the pack runs to completion ahead of the first byte and
        the pacing below only throttles the network side. The pack runs on a
        pool of two (`ARCHIVE_PACK_POOL`), so concurrent requests queue for it
        rather than each staging a full copy. The folder is opened through the
        confinement's descriptor and walked from it (`_pack_directory`), so
        the pack reads the object the check opened rather than a name a
        symlink planted between the two could have swapped.

        The archive rides the same pacing the file download does
        (`_serve_paced_download`), so a folder the size of a video directory
        still backs off with the session's delay instead of taking whatever the
        downlink accepts. It is streamed rather than handed to FileResponse,
        since the temporary does not outlive the handler, and the route is GET
        alone for the same reason: a HEAD, a Range, or a conditional request
        would want FileResponse to answer from a file that is gone, and each is
        refused before the folder is packed rather than after it.
        """
        if self._viewer_ceiling(request):
            return web.Response(status=403, text="View-only credentials cannot download files")
        if "download" not in self.settings.file_transfers:
            return web.Response(status=403, text="Forbidden: downloads disabled")
        if self._fetched_cross_site(request, request.app["settings"]):
            logger.warning("Refused a folder archive to another site's page (Origin %r)",
                           request.headers.get("Origin", ""))
            return web.Response(status=403, text="Forbidden origin")
        rel = str(request.query.get("path", ""))
        target = self._resolve_file_path(rel)
        if target is None or self._at_file_root(target):
            return web.json_response({"status": "error", "message": "not found"}, status=404)
        if any(name in request.headers for name in (
                "Range", "If-Modified-Since", "If-None-Match", "If-Range", "If-Unmodified-Since")):
            return web.Response(status=405, headers={"Allow": "GET"})
        staged: Optional[str] = None
        try:
            try:
                staged = await asyncio.get_running_loop().run_in_executor(
                    ARCHIVE_PACK_POOL, self._pack_directory, self._file_parts(rel) or [])
            except ArchiveTooLarge:
                return web.json_response(
                    {"status": "error",
                     "message": "the folder holds too much to pack as one archive"},
                    status=413)
            except OSError:
                staged = None
            if staged is None:
                return web.json_response({"status": "error", "message": "not found"}, status=404)
            name = os.path.basename(os.path.normpath(rel))
            return await self._serve_paced_download(
                request, pathlib.Path(staged),
                audit_name=rel, download_name=f"{name}.zip",
                allow_fallback=False)
        finally:
            if staged is not None:
                await asyncio.to_thread(os.remove, staged)

    async def fancy_index_handler(self, request: web.Request) -> web.StreamResponse:
        """GET /api/files/...: serve the file-manager tree for download.

        Files are served with an attachment disposition, since inline types
        (text, images, PDF) would otherwise render inside the dashboard's
        file-browser iframe instead of downloading; directories render a
        styled HTML listing, redirected to their slash-terminated URL so the
        relative links resolve one level down. The index exists solely to
        download files, so the listing itself is gated behind the same
        "download" transfer permission as the bytes. Path validation rejects
        any traversal before touching the filesystem and re-checks the
        symlink-resolved path against the root.

        A download paces like an upload, mirrored: `_serve_paced_download`
        carries the mechanism and its conditions.

        View-only credentials are refused the listing and the bytes alike: a
        viewer watches the desktop, it does not take the files on it.
        """
        if self._viewer_ceiling(request):
            return web.Response(status=403, text="View-only credentials cannot list or download files")
        if "download" not in self.settings.file_transfers:
            return web.Response(status=403, text="Forbidden: downloads disabled")
        rel_path = request.match_info.get("path", "").lstrip("/")
        base = str(self.upload_dir)
        parts = self._file_parts(rel_path)
        if parts is None:
            return web.Response(
                status=403, text="Forbidden: Directory Traversal detected"
            )
        resolved = os.path.realpath(os.path.join(base, *parts))
        if resolved != base and not resolved.startswith(os.path.join(base, "")):
            return web.Response(
                status=403, text="Forbidden: Directory Traversal detected"
            )
        full_path = pathlib.Path(resolved)

        if not full_path.exists():
            return web.Response(status=404, text="Not Found")

        if full_path.is_file():
            return await self._serve_paced_download(
                request, full_path, audit_name="/".join(parts))

        if not request.path.endswith("/"):
            # Exactly one leading slash, so the location is never protocol-relative.
            location = "/" + request.path.lstrip("/") + "/"
            if request.query_string:
                location += "?" + request.query_string
            raise web.HTTPMovedPermanently(location)

        try:
            items = await asyncio.to_thread(
                _scan_directory, full_path, full_path != self.upload_dir)
        except PermissionError:
            return web.Response(status=403, text="Permission Denied")

        items.sort(key=lambda x: (not x["is_dir"], x["name"].lower()))

        rows = ""
        for item in items:
            # A name that is not UTF-8 comes back from scandir with surrogates,
            # which neither quote() nor the response's encoding takes.
            raw_name = item["name"].encode("utf-8", "surrogateescape")
            escaped_name = html.escape(raw_name.decode("utf-8", "replace"))
            escaped_mtime = html.escape(item["mtime"])
            escaped_size = html.escape(item["size"])
            rows += f"""
            <tr>
                <td><a href="{urllib.parse.quote(raw_name)}">{escaped_name}</a></td>
                <td>{escaped_mtime}</td>
                <td>{escaped_size}</td>
            </tr>"""

        # The header template leaves the H1 open; the current path closes it here.
        escaped_rel_path = html.escape(rel_path)
        current_display_path = f"/api/files/{escaped_rel_path}"

        # json.dumps yields a safely quoted/escaped JavaScript string literal.
        js_safe_upload_dir = json.dumps(str(self.upload_dir))
        path_injection = f"<script>window.__SELKIES_INJECTED_PATH_PREFIX__ = {js_safe_upload_dir};</script>"

        html_content = f"""
        {FILE_INDEX_HEADER}
        {current_display_path}</h1>
        {path_injection}
        <table id="list">
            <thead>
                <tr>
                    <th>File Name</th>
                    <th>Date</th>
                    <th>Size</th>
                </tr>
            </thead>
            <tbody>
                {rows}
            </tbody>
        </table>
        {FILE_INDEX_FOOTER}
        """

        return web.Response(text=html_content, content_type="text/html")

    async def initialize_app(self) -> web.Application:
        """Build the aiohttp application: auth middleware, API routes, service
        routes, and the static frontend.

        Every control-plane endpoint lives under `/api` so a fronting proxy
        routes them, present and future, with one rule. The file-browser API
        serves the file-manager directory independently of the static content,
        so a deployment whose frontend is served elsewhere (nginx, `web_root`
        unset) still gets downloads instead of a 404; the apps catalog of a
        proot-apps local repository is served the same way; and the
        Prometheus registry is process-global, so one mode-agnostic endpoint
        serves both streaming modes.

        Returns:
            The configured application (also stored on ``self.app``).
        """
        self._require_configured_credentials()
        self._require_one_listen_setting()

        self.app = web.Application(middlewares=[self._auth_middleware])
        self.app["supervisor"] = self
        self.app["settings"] = self.settings
        frame_policy = _frame_ancestors_directive(self.settings.frame_ancestors)
        if frame_policy:
            async def _send_frame_policy(request: web.Request, response: web.StreamResponse) -> None:
                response.headers["Content-Security-Policy"] = frame_policy
            self.app.on_response_prepare.append(_send_frame_policy)

        api_prefix = self.settings.subfolder
        if api_prefix:
            logger.debug(f"Prepending api prefix: {api_prefix!r} to router handlers")

        routes = [
            web.get(f"{api_prefix}/api/status", self.handle_status),
            web.get(f"{api_prefix}/api/health", self.handle_health),
            web.post(f"{api_prefix}/api/switch", self.handle_switch),
            web.post(f"{api_prefix}/api/upload", self.handle_upload),
            # The file manager's JSON API, apart from /api/files/ so that none
            # of its names shadows a file or folder of the same name there.
            web.get(f"{api_prefix}/api/file-manager/browse", self.handle_files_browse),
            web.post(f"{api_prefix}/api/file-manager/mkdir", self.handle_files_mkdir),
            web.get(f"{api_prefix}/api/file-manager/zip", self.handle_files_zip,
                    allow_head=False),
            web.get(f"{api_prefix}/api/files/{{path:.*}}", self.fancy_index_handler),
            web.get(f"{api_prefix}/api/print/{{name}}", self.handle_print_document),
            web.get(f"{api_prefix}/api/apps/metadata.yml", self.handle_apps_catalog),
            web.get(f"{api_prefix}/api/apps/img/{{name}}", self.handle_apps_icon),
            web.get(f"{api_prefix}/api/sessions", self.handle_sessions),
            web.delete(f"{api_prefix}/api/sessions/{{id}}", self.handle_session_delete),
            web.get(f"{api_prefix}/api/recording", self.handle_recording),
            web.post(f"{api_prefix}/api/recording", self.handle_recording),
            web.delete(f"{api_prefix}/api/recording", self.handle_recording),
            web.get(f"{api_prefix}/api/screenshot", self.handle_screenshot),
        ]
        if self.settings.enable_metrics_http[0]:
            routes.append(web.get(f"{api_prefix}/api/metrics", self.handle_metrics))
        self.app.add_routes(routes)

        for service in self.services.values():
            service.register_routes(api_prefix, self.app.router)

        self.static_fs_path = await self._get_static_content_path()
        if self.static_fs_path:
            async def index_handler(request: web.Request) -> web.FileResponse:
                moved = _ipv6_loopback_redirect(request, f"{api_prefix}/")
                if moved:
                    raise web.HTTPFound(moved)
                return web.FileResponse(os.path.join(self.static_fs_path, "index.html"))

            index = self.app.router.add_get(f"{api_prefix}/", index_handler)
            static = self.app.router.add_static(
                f"{api_prefix}/", self.static_fs_path, name="static"
            )

            async def _revalidate_fixed_names(request: web.Request, response: web.StreamResponse) -> None:
                """Have a browser revalidate, on every load, the entry page and
                every other file of the web client's code whose name does not
                change with its content.

                With validators alone a browser may reuse such a file by
                heuristic freshness, so after an upgrade or a change of web
                root it loads the old page, or the old core under a new page;
                the validators keep each revalidation a 304. What the build
                names by content hash, under `assets/`, is left to cache, and
                so are icons and the manifest, which no page's behavior hangs
                on and which Firefox would revalidate on every load.
                """
                resource = request.match_info.route.resource
                name = request.match_info.get("filename", "")
                if resource is index.resource or (
                        resource is static and not name.startswith("assets/") and name.endswith(WEB_CODE)):
                    response.headers.setdefault("Cache-Control", "no-cache")
            self.app.on_response_prepare.append(_revalidate_fixed_names)
        else:
            logger.warning("Unable to find web content, skipping web routers handlers")
        return self.app

    def _unix_socket_path(self) -> str:
        return str(getattr(self.settings, "unix_socket", "") or "").strip()

    def _clear_stale_unix_socket(self, sock_path: str) -> None:
        """Remove a leftover socket file so the bind cannot fail with EADDRINUSE.

        Only a socket inode that nothing accepts on is removed. Unlinking a live
        one would leave the instance that owns it serving an inode no client can
        reach, so a path still in use — or occupied by anything that is not a
        socket — aborts the start instead."""
        try:
            mode = os.stat(sock_path).st_mode
        except FileNotFoundError:
            return
        except OSError as exc:
            raise RuntimeError(
                f"Cannot inspect unix socket path '{sock_path}': {exc}"
            ) from exc
        if not stat.S_ISSOCK(mode):
            raise RuntimeError(
                f"Unix socket path '{sock_path}' exists and is not a socket; "
                "refusing to remove it."
            )
        if _unix_socket_is_live(sock_path):
            raise RuntimeError(
                f"Another server is already listening on '{sock_path}'."
            )
        try:
            os.unlink(sock_path)
        except FileNotFoundError:
            pass
        except OSError as exc:
            raise RuntimeError(
                f"Cannot remove stale unix socket '{sock_path}': {exc}"
            ) from exc

    def _remove_own_unix_socket(self) -> None:
        """Drop this listener's socket file on shutdown so nothing is left behind
        for the next start to clear; the runtime does not unlink it on every
        supported Python version. A path something is accepting on again belongs
        to another instance and is left alone."""
        sock_path = self._unix_socket_path()
        if not sock_path:
            return
        try:
            if not stat.S_ISSOCK(os.stat(sock_path).st_mode):
                return
            if _unix_socket_is_live(sock_path):
                return
            os.unlink(sock_path)
        except OSError:
            pass

    async def _build_sites(self, ssl_context: Optional[ssl.SSLContext] = None) -> List[web.BaseSite]:
        """The aiohttp sites of the configured listener: one on a Unix domain
        socket when ``unix_socket`` is set, otherwise one per address ``addr``
        binds on ``port``, bound here so a listen address the host lacks can
        be skipped rather than failing the start."""
        sock_path = self._unix_socket_path()
        if sock_path:
            parent = os.path.dirname(sock_path)
            if parent:
                os.makedirs(parent, exist_ok=True)
            self._clear_stale_unix_socket(sock_path)
            return [web.UnixSite(self.runner, path=sock_path, ssl_context=ssl_context)]
        socks = await _bind_listen_sockets(self.settings.addr, self.settings.port)
        return [web.SockSite(self.runner, sock, ssl_context=ssl_context) for sock in socks]

    async def _start_sites(self, ssl_context: Optional[ssl.SSLContext] = None) -> List[web.BaseSite]:
        """Bind and start the configured listener, publishing it as ``self.sites``.

        A start that fails part-way stops what came up so nothing is left
        listening under a site the server does not track.

        Raises:
            OSError: When the listener cannot be bound.
        """
        sites = await self._build_sites(ssl_context)
        started: List[web.BaseSite] = []
        try:
            for site in sites:
                await site.start()
                started.append(site)
        except BaseException:
            for site in started:
                await site.stop()
            for site in sites[len(started):]:
                sock = getattr(site, "_sock", None)
                if sock is not None:
                    sock.close()
            raise
        self.sites = sites
        return sites

    def _site_kind(self) -> str:
        return "Unix socket listener" if self._unix_socket_path() else "TCP listener"

    def _site_endpoint(self) -> str:
        """Where the server is reached: every bound address once it listens,
        the configured one before that."""
        sock_path = self._unix_socket_path()
        if sock_path:
            return f"unix://{sock_path}"
        if self.sites:
            return ", ".join(site.name for site in self.sites)
        return f"{self.settings.addr} port {self.settings.port}"

    async def start_server(self) -> None:
        """Start the HTTP/HTTPS server, the tokens-file watcher when one is set,
        and, under HTTPS, the cert-reload watcher."""
        if not self.app:
            await self.initialize_app()

        https = getattr(self.settings, "enable_https", (False,))[0]
        if https:
            try:
                self.ssl_context = self._create_ssl_context()
            except Exception as exc:
                logger.error("Failed to create SSL context at startup: %s", exc)
                raise

        tokens = self.services.get("websockets")
        if getattr(self.settings, "tokens_file", "") and tokens is not None:
            self.tokens_watcher = asyncio.create_task(tokens.watch_tokens_file())

        self.runner = web.AppRunner(self.app, access_log_class=PathOnlyAccessLogger,
                                    keepalive_timeout=IDLE_CONNECTION_TIMEOUT_SECONDS)
        await self.runner.setup()

        try:
            await self._start_sites(self.ssl_context)
        except Exception as exc:
            logger.error("Cannot bind %s: %s", self._site_endpoint(), exc)
            raise
        logger.info("Selkies server running on %s", self._site_endpoint())

        if https:
            self.cert_watcher = asyncio.create_task(self._watch_and_reload_certs())
        if self.settings.printing_enabled[0]:
            self.print_watcher = printing.SpoolWatcher(
                str(self.print_spool), asyncio.get_running_loop(), self._announce_print_document)
            try:
                self.print_watcher.start()
            except OSError as exc:
                # Watching the spool is one inotify instance, which a host can run out of.
                logger.warning("Printing is off: the spool cannot be watched (%s).", exc)
                self.print_watcher = None
            else:
                self.print_queue = printing.PrintQueue(str(self.print_spool))
                await self.print_queue.start()

    async def stop_server(self) -> None:
        """Stop the server gracefully: the cert and tokens-file watchers, active
        service, listener, extracted web files, and the runner, in that order."""
        for watcher in (self.cert_watcher, self.tokens_watcher):
            if watcher and not watcher.done():
                watcher.cancel()
                try:
                    await watcher
                except asyncio.CancelledError:
                    pass
        if self.print_queue:
            await self.print_queue.stop()
            self.print_queue = None
        if self.print_watcher:
            self.print_watcher.stop()
            self.print_watcher = None

        await self._stop_service()

        if self.web_files_ctx:
                self.web_files_ctx.cleanup()
        for site in self.sites:
            await site.stop()
        if self.sites:
            self._remove_own_unix_socket()
        self.sites = []
        if self.runner:
            await self.runner.cleanup()
            logger.debug("Server cleanup complete.")

    async def run(self) -> None:
        """Start the server and serve until canceled, then clean up."""
        try:
            await self.start_server()
            await asyncio.Future()
        except asyncio.CancelledError:
            logger.info("Shutdown signal received...")
        finally:
            await self.stop_server()

    def register_service(self, name: str, service: BaseStreamingService) -> None:
        """Register a streaming service under ``name`` for later activation."""
        self.services[name] = service
