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
import json
import html
import stat
import hashlib
import time
import shutil
import base64
import pathlib
import asyncio
import array
import math
import mimetypes
import struct
import logging
import socket
import urllib.parse
import tempfile

try:
    import fcntl
except ImportError:
    fcntl = None
from aiohttp import web
from aiohttp.abc import AbstractAccessLogger
from datetime import datetime, timedelta
from prometheus_client import generate_latest
from typing import Any, Awaitable, Callable, Dict, List, Optional, Set, Tuple
try:
    # pyrefly: ignore[missing-import]
    import importlib_resources as importlib_resources  # pyright: ignore[reportMissingImports]
except ImportError:
    import importlib.resources as importlib_resources

from abc import ABCMeta, abstractmethod


logger = logging.getLogger("stream_server")


# Idle chunked transfers older than this are reaped; generous relative to one
# ≤64 MiB slice, so only a transfer whose client is truly gone expires.
UPLOAD_PART_TTL_SECONDS: int = 3600

# Hidden staging sibling an upload is renamed from, so a destination is only
# ever replaced by a complete file.
UPLOAD_STAGING_PREFIX: str = ".selkies-upload-"


class PathOnlyAccessLogger(AbstractAccessLogger):
    """aiohttp access log whose request line carries the path without its query.

    The secure-mode session token rides the data WebSocket URL as a query
    parameter, so the stock request-line atom (which logs ``path_qs``) would
    write credentials into the access log. The line otherwise has the default
    shape: remote address, start time, method + path + version, status, body
    size, Referer and User-Agent.
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
                request.headers.get("Referer", "-"),
                request.headers.get("User-Agent", "-"),
            )
        except Exception:
            self.logger.exception("Error in logging")


def _sock_unsent_bytes(sock: Any) -> Optional[int]:
    """Bytes queued in the socket's send buffer that TCP has not yet
    transmitted, or None.

    On a saturated link this is where the queue actually lives — the classic
    bufferbloat gauge (RTT) stays flat when the delay piles up below the
    sender, which is exactly what a metered first hop produces. The ioctl is
    SIOCOUTQNSD (not-sent data only): SIOCOUTQ would also count in-flight
    unacked bytes, which scale with the path's bandwidth-delay product, so a
    healthy long-RTT link would read as permanently congested.
    """
    try:
        buf = array.array("i", [0])
        fcntl.ioctl(sock.fileno(), 0x894B, buf, True)
        return buf[0]
    except (OSError, AttributeError, TypeError, ValueError):
        return None


def _sock_rtt_us(sock: Any) -> Optional[int]:
    """Smoothed RTT of a TCP socket in microseconds, or None when unavailable."""
    try:
        info = sock.getsockopt(socket.IPPROTO_TCP, socket.TCP_INFO, 104)
    except Exception:
        return None
    if len(info) < 72:
        return None
    # struct tcp_info: 8 u8 fields then 15 u32s precede tcpi_rtt, a u32 at
    # byte offset 68 (offset 24 holds tcpi_unacked).
    r68 = struct.unpack_from("I", info, 68)[0]
    return r68 or None


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


class UplinkAllowance:
    """Read pacing for a client upload, sized from what that client can send.

    An upload allowed to run at the client's full uplink rate stands a queue
    in the first hop, and everything else the session sends — input, feedback,
    the acknowledgements the video stream's own delivery depends on — waits
    behind it. That wait is what a user reports as the session breaking down
    mid-transfer, and it is bounded by the buffer rather than by the file, so
    it does not improve as the transfer proceeds.

    Nothing on this side can measure the client's uplink directly, and reading
    flat out to find it would fill the very queue this exists to keep empty —
    and would measure the buffers it drained rather than the link. So the
    allowance steps up instead, and watches for the one moment the arithmetic
    is honest: while a transfer is paced below the client's rate its data
    banks up here and reads never wait, so a read that DOES wait means the
    backlog is gone and what arrives from then on is arriving at the link's
    own rate. That window is the measurement, and the transfer settles below
    it — the link keeps an idle share, the queue drains, and the session's
    traffic crosses at its unloaded delay.

    Reads are what apply the rate, since a paused read closes the receive
    window and the data waits in the client's kernel, so no client
    cooperation is needed.

    Attributes:
        SHARE: Settled share of the client's link, half. On a modeled 6 Mbit/s
            uplink behind a 1 MiB first-hop buffer with a session streaming
            beside the transfer, a small request took 1421 ms unpaced, 1116 ms
            at a three-quarter share and 2 ms (its unloaded time) at a half,
            for 60% of the unpaced transfer rate.
        STARVED_FRACTION: Share of a window spent waiting for the client before
            the backlog counts as gone and the window's rate as the link's own.
        DRAIN_SHARE: Share held for `DRAIN_SECONDS` after the measurement to
            empty the queue the ramp left standing, before settling at `SHARE`.
        MIN_PACED_BYTES: Below this a transfer is over before a queue could
            matter, and pacing it would only make a small file feel slow.
        READ_BYTES: One read's worth of receive window, and so the burst the
            client answers with: small enough that it cannot itself become the queue.
    """

    SHARE: float = 0.5
    WINDOW_SECONDS: float = 0.5
    GROWTH: float = 1.5
    START_BPS: int = 256 * 1024
    STARVED_FRACTION: float = 0.15
    DRAIN_SHARE: float = 0.45
    DRAIN_SECONDS: float = 2.5
    FLOOR_BPS: int = 64 * 1024
    MIN_PACED_BYTES: int = 4 * 1024 * 1024
    READ_BYTES: int = 64 * 1024

    def transfer(self) -> Dict[str, Any]:
        """State for one upload: its own ramp, its own bucket."""
        return {"rate_bps": float(self.START_BPS), "seen": 0, "window_start": None,
                "phase": "ramp", "starved": 0.0, "tokens": 0.0, "ts": 0.0,
                "drain_until": 0.0, "capacity_bps": 0.0}

    def _step(self, state: Dict[str, Any], now: float) -> None:
        """Close one window: step up while data is banked, settle when it runs out.

        A ramp window that starved still counted buffered bytes, so the next
        window is the measurement. Measuring leaves the link's queue standing,
        and at the steady share it would take most of a transfer to come back
        out, so a wider drain share clears it first.
        """
        elapsed = now - state["window_start"]
        observed = state["seen"] / elapsed if elapsed > 0 else 0.0
        starved = state["starved"]
        state["window_start"] = now
        state["seen"] = 0
        state["starved"] = 0.0
        if state["phase"] == "ramp":
            if starved < elapsed * self.STARVED_FRACTION:
                state["rate_bps"] *= self.GROWTH
                return
            state["phase"] = "measure"
            return
        if state["phase"] == "measure":
            state["capacity_bps"] = observed
            state["phase"] = "drain"
            state["drain_until"] = now + self.DRAIN_SECONDS
            state["rate_bps"] = max(self.FLOOR_BPS, observed * self.DRAIN_SHARE)
            return
        if state["phase"] == "drain" and now >= state["drain_until"]:
            state["phase"] = "steady"
            state["rate_bps"] = max(self.FLOOR_BPS,
                                    state["capacity_bps"] * self.SHARE)

    async def pace(self, nbytes: int, state: Dict[str, Any],
                   waited: float = 0.0) -> None:
        """Account one read of `nbytes` that spent `waited` seconds arriving.

        The deficit is slept off here and repaid by the next call's
        elapsed-time refill, as in `TransferPacer`.
        """
        now = time.monotonic()
        if state["window_start"] is None:
            state["window_start"] = now
            state["ts"] = now
        state["seen"] += nbytes
        state["starved"] += waited
        if now - state["window_start"] >= self.WINDOW_SECONDS:
            self._step(state, now)
        rate = state["rate_bps"]
        state["tokens"] = min(rate * 0.25,
                              state["tokens"] + (now - state["ts"]) * rate)
        state["ts"] = now
        state["tokens"] -= nbytes
        if state["tokens"] < 0:
            await asyncio.sleep(-state["tokens"] / rate)


class TransferPacer:
    """File-transfer pacing shared across all transfers of one server.

    Two modes. A static cap (rate_bps) exists for links whose rate the operator
    already knows. Without one, the pacer instead holds every download inside a
    shared allowance that adapts to the bottleneck: the download socket's
    unsent-queue depth is the gauge (RTT inflation where the queue ioctl is
    unavailable), so a rate that builds a queue (bufferbloat = the video
    stream stalls) is walked back, and one that drains cleanly earns headroom.
    No link estimate is needed. A connection offering neither gauge gives the
    adaptive mode nothing to react to, so it is left unpaced rather than
    throttled blindly; the static cap still applies.

    Transfers share ONE rate across concurrent downloads and uploads on
    purpose: the link sees one source regardless of how many sockets a
    browser opens. Upload reads have no usable gauge at all — the client's
    uplink queue stands in the client's kernel, invisible to this side — so
    they ride only the static-cap leg, ungauged (see connection_state).
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

    @property
    def active(self) -> bool:
        return self.adaptive or self.static_bps > 0

    @property
    def _ceiling(self) -> int:
        """The static cap, or effectively unbounded when purely adaptive."""
        return self.static_bps or 64 * 1024 * 1024 * 1024

    def connection_state(self, gauged: bool = True) -> Dict[str, Any]:
        """Per-transfer gauge state. The RTT floor is a property of one
        connection's path: sharing it would let a nearby client's short base
        RTT make a distant client's read as permanent congestion. Upload
        reads pass gauged=False: with no honest congestion signal they pace
        against the static cap alone and pass untouched in adaptive-only
        mode, like any other gaugeless connection."""
        return {"rtt_floor_us": None, "gauged": gauged}

    async def pace(self, sock: Any, nbytes: int, conn: Dict[str, Any]) -> None:
        """Sample the gauge and sleep off what `nbytes` overdraws.

        After a long idle gap the remembered rate is stale, so the
        multiplicative ramp is re-entered (TCP's restart after idle): a link
        that got faster meanwhile is rediscovered in chunks, not minutes, and
        one that got slower is cut by the first gauge sample. The deficit is
        slept off here and paid back by the next call's elapsed-time refill;
        zeroing the balance after the sleep would credit the slept interval
        twice and double the delivered rate.
        """
        if not self.active:
            return
        if self.adaptive and conn["gauged"]:
            outq = _sock_unsent_bytes(sock) if sock is not None else None
            if outq is not None:
                self._gauge_backoff(
                    congested=outq > 192 * 1024, clear=outq < 96 * 1024, cut=0.6)
            else:
                rtt = _sock_rtt_us(sock) if sock is not None else None
                if rtt:
                    floor = conn["rtt_floor_us"] = (
                        rtt if conn["rtt_floor_us"] is None
                        else min(conn["rtt_floor_us"], rtt)
                    )
                    self._gauge_backoff(
                        congested=rtt > floor + 8000, clear=True, cut=0.5)
                else:
                    conn["gauged"] = False
        if self.adaptive and not conn["gauged"] and not self.static_bps:
            return
        now = time.monotonic()
        if self.adaptive and now - self._ts > 10:
            self._slow_start = True
        limit = min(self.rate_bps, self._ceiling)
        self._tokens = min(limit * 0.5, self._tokens + (now - self._ts) * limit)
        self._ts = now
        self._tokens -= nbytes
        if self._tokens < 0:
            await asyncio.sleep(-self._tokens / limit)

    def _gauge_backoff(self, congested: bool, clear: bool, cut: float) -> None:
        """One congestion-control step on the shared allowance: a congested
        sample multiplies the rate down; a clear one probes upward —
        multiplicatively while no congestion has ever been seen (the initial
        ramp toward an unknown link rate), additively after (fine-grained
        probing near the working point, TCP's post-ssthresh split).

        The recovery ceiling arms ONCE per congestion epoch, from the rate at
        the epoch's first congested sample (ssthresh semantics): arming it per
        chunk lets a sustained spike ratchet the ceiling toward the floor, and
        computing it from the post-backoff rate pins recovery below the rate
        itself. Reaching the ceiling releases it so clear stretches keep
        probing past the last congested rate; that sawtooth is what keeps a
        link that gets faster later reachable.

        A cut also pauses growth for a drain window: resuming on the first
        clear sample keeps the bottleneck queue standing, and the cut never
        relieves the stream sharing the link. The epoch closes only on a
        clear sample past that window: a clear inside the hold still reflects
        the pre-cut queue draining, and ending the epoch there would let an
        oscillating gauge re-arm the ceiling from each freshly cut rate — the
        same ratchet, one flap at a time."""
        if congested:
            self._slow_start = False
            if not self._congested:
                self._congested = True
                self._probe_ceiling = max(self.rate_bps, 2 * self._RATE_FLOOR)
            self.rate_bps = max(self.rate_bps * cut, self._RATE_FLOOR)
            self._hold_until = time.monotonic() + 1.5
            return
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
            self.rate_bps = min(self.rate_bps + 8 * 1024, bound)


def _upload_staging_path(dest: str, token: str) -> str:
    """Return the staging file path for an upload to ``dest``.

    The staging file is a fixed-length hidden sibling living in the
    destination's own directory. Its name is derived from ``token`` instead of
    being appended to the destination basename, so a filename that is legal but
    sits close to the filesystem's NAME_MAX still uploads; staying in the same
    directory keeps the finalizing ``os.replace`` atomic and intra-filesystem.
    """
    return os.path.join(os.path.dirname(dest), f"{UPLOAD_STAGING_PREFIX}{token}.part")


def _upload_staging_token(dest: str) -> str:
    """Return the staging token every slice of a chunked transfer to ``dest``
    resolves to, so the .part file is found again across the separate requests
    that append to it."""
    return hashlib.sha256(os.fsencode(dest)).hexdigest()[:16]


def _carry_destination_mode(staging: str, dest: str) -> None:
    """Give the staged upload the permission bits of the file it is about to
    replace, so re-uploading over an existing file keeps its mode: an executable
    script stays executable and a private file stays private. A new destination,
    or one that is not a regular file, keeps the staging file's creation mode."""
    try:
        st = os.lstat(dest)
    except OSError:
        return
    if not stat.S_ISREG(st.st_mode):
        return
    try:
        os.chmod(staging, stat.S_IMODE(st.st_mode))
    except OSError as e:
        logger.debug(f"Could not carry the mode of {dest} onto the staged upload: {e}")


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


# Realm both 401 challenges name; the web client's 401 guard keys on the
# Bearer one as this server's own token verdict.
AUTH_REALM: str = "Selkies Restricted"
# Handshakes carrying their own token gate in the ws handlers; the auth
# middleware exempts exactly these paths, never a mere Upgrade claim.
WEBSOCKET_ROUTES: Tuple[str, ...] = ("/api/websockets", "/api/webrtc/signaling", "/api/ws")
# Mirror of the secure-mode session token for requests the client cannot put
# a header on (the file-manager iframe and its download links).
SESSION_TOKEN_COOKIE: str = "selkies_token"

FILE_INDEX_HEADER: str = """<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta http-equiv="x-ua-compatible" content="IE=edge">
    <title>Desktop Files</title>
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <style>
        :root {
            --page-bg: #282c34;
            --text-color: #abb2bf;
            --header-color: #F59DC4;
            --border-color: #3a3f47;
            --table-header-bg: #3a3f47;
            --table-row-hover-bg: #454b54;
            --link-color: #F59DC4;
            --link-hover-color: #F5AECE;
            --shadow-color: rgba(0, 0, 0, 0.5);

            --container-max-width: 960px;
            --font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
            --border-radius: 8px;
        }
        * {
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }
        body {
            font-family: var(--font-family);
            background-color: var(--page-bg);
            color: var(--text-color);
            line-height: 1.6;
            padding-top: 20px;
            padding-bottom: 60px;
        }
        .page-container {
            max-width: var(--container-max-width);
            margin: 0 auto;
            padding: 0 20px;
            position: relative;
        }
        h1 {
            color: var(--header-color);
            font-size: 2em;
            font-weight: 300;
            margin-bottom: 15px;
            padding-bottom: 15px;
            border-bottom: 1px solid var(--border-color);
            padding-right: 50px;
        }
        hr {
            display: none;
        }
        table {
            width: 100%;
            border-collapse: collapse;
            margin-top: 25px;
            font-size: 0.95em;
            border-radius: var(--border-radius);
            overflow: hidden;
            box-shadow: 0 4px 10px var(--shadow-color);
        }
        thead {
            background-color: var(--table-header-bg);
        }
        th {
            color: var(--header-color);
            font-weight: 600;
            text-transform: uppercase;
            font-size: 0.85em;
            letter-spacing: 0.05em;
        }
        th, td {
            padding: 12px 15px;
            text-align: left;
            border-bottom: 1px solid var(--border-color);
        }
        tbody tr {
            transition: background-color 0.2s ease-in-out;
        }
        tbody tr:hover {
            background-color: var(--table-row-hover-bg);
        }
        tbody tr:last-child td {
            border-bottom: none;
        }
        td a {
            color: var(--link-color);
            text-decoration: none;
            display: inline-flex;
            align-items: center;
            transition: color 0.2s ease-in-out;
        }
        td a:hover {
            color: var(--link-hover-color);
            text-decoration: underline;
        }
        th a {
            color: var(--link-color);
            text-decoration: none;
            transition: color 0.2s ease-in-out;
        }
        th a:hover {
            color: var(--link-hover-color);
            text-decoration: underline;
        }
        th a:visited {
            color: var(--link-color);
        }
        th a:visited:hover {
            color: var(--link-hover-color);
        }
        #reload-page-button {
            position: absolute;
            top: 0;
            right: 0;
            background-color: transparent;
            color: var(--text-color);
            border: none;
            border-radius: var(--border-radius);
            padding: 8px;
            cursor: pointer;
            display: inline-flex;
            align-items: center;
            justify-content: center;
            transition: color 0.2s ease-in-out, transform 0.2s ease-in-out;
            z-index: 10;
        }
        #reload-page-button:hover {
            color: var(--link-hover-color);
            transform: rotate(45deg);
        }
        #reload-page-button svg {
            width: 20px;
            height: 20px;
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
        td a::before {
            display: inline-block;
            content: '';
            width: 1.1em;
            height: 1.1em;
            margin-right: 0.75em;
            vertical-align: middle;
            background-repeat: no-repeat;
            background-size: contain;
            background-position: center;
            flex-shrink: 0;
        }
        td a[href="../"]::before {
            background-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="%23abb2bf"><path d="M20 11H7.83l5.59-5.59L12 4l-8 8 8 8 1.41-1.41L7.83 13H20v-2z"/></svg>');
        }
        td a[href$="/"]:not([href="../"])::before {
            background-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="%23abb2bf"><path d="M10 4H4c-1.11 0-2 .89-2 2v12c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V8c0-1.1-.9-2-2-2h-8l-2-2z"/></svg>');
        }
        td a:not([href$="/"]):not([href="../"])::before {
            background-image: url('data:image/svg+xml;charset=UTF-8,<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="%23abb2bf"><path d="M14 2H6c-1.11 0-2 .9-2 2v16c0 1.1.89 2 2 2h12c1.1 0 2-.9 2-2V8l-6-6zm2 16H8v-2h8v2zm0-4H8v-2h8v2zm-3-5V3.5L18.5 9H13z"/></svg>');
        }
        footer {
            text-align: center;
            margin-top: 40px;
            padding-top: 20px;
            border-top: 1px solid var(--border-color);
            font-size: 0.85em;
            color: var(--text-color);
            opacity: 0.7;
        }
        footer p {
            margin: 0;
        }
        @media (max-width: 768px) {
            body {
                font-size: 14px;
                padding-top: 10px;
                padding-bottom: 40px;
            }
            .page-container {
                padding: 0 10px;
            }
            h1 {
                font-size: 1.6em;
                padding-right: 40px;
            }
            #reload-page-button {
                top: -2px;
                right: 0px;
            }
            #reload-page-button svg {
                width: 18px;
                height: 18px;
            }
            th, td {
                padding: 10px 8px;
            }
            table {
                display: block;
                overflow-x: auto;
                white-space: nowrap;
                -webkit-overflow-scrolling: touch;
            }
            th, td {
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
            // nginx/footer.html): the listing is mounted at /api/files/ (the
            // scheme the dashboards iframe) or the legacy /files/, optionally
            // behind a deployment subfolder or fronting proxy — derive the
            // mount from the pathname rather than assuming a fixed prefix.
            const path = window.location.pathname;
            let webPathPrefix = '/api/files/';
            let idx = path.indexOf(webPathPrefix);
            if (idx === -1) {
                webPathPrefix = '/files/';
                idx = path.indexOf(webPathPrefix);
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

            // The listing exists to download files: force file links to save
            // (directories keep navigating and the thead sort links keep
            // sorting). nginx-served listings send no Content-Disposition
            // header, so without this a click inside the dashboard's
            // file-browser iframe renders the file inline instead.
            // A secure-mode session token in the query (the dashboards open
            // the listing with the page's token) rides every link, so the
            // listing stays navigable where the token cookie cannot follow.
            const sessionToken = new URLSearchParams(window.location.search).get('token');
            document.querySelectorAll('table#list td a').forEach(function(a) {
                const href = a.getAttribute('href') || '';
                if (!href || href.startsWith('?')) return;
                if (!href.endsWith('/')) {
                    a.setAttribute('download', '');
                }
                if (sessionToken) {
                    a.setAttribute('href', href + '?token=' + encodeURIComponent(sessionToken));
                }
            });

            const isAtRoot = idx !== -1 && path.endsWith(webPathPrefix);
            if (isAtRoot) {
                const parentLink = document.querySelector('table#list a[href^="../"]');
                if (parentLink) {
                    const parentRow = parentLink.closest('tr');
                    if (parentRow) {
                        parentRow.style.display = 'none';
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
        transfer_pacer: Download pacing, congestion-controlled by default; a
            static cap only when the operator knows the link rate. A zero limit
            with adaptive off leaves downloads on the unthrottled FileResponse path.
        uplink_allowance: Holds uploads, which have no congestion gauge on this
            side, below the rate the client demonstrates; None when congestion
            control is off or a static cap already answers the question.
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
        self.site: Optional[web.BaseSite] = None
        self.cert_watcher: Optional[asyncio.Task] = None
        self.ssl_context: Optional[ssl.SSLContext] = None
        self.static_fs_path: str = ""
        limit_mbps = float(self.settings.file_transfer_limit_mbps or 0.0)
        if not math.isfinite(limit_mbps) or limit_mbps < 0:
            logger.warning(
                f"Ignoring file_transfer_limit_mbps={limit_mbps!r}: not a usable rate."
            )
            limit_mbps = 0.0
        self.transfer_pacer = TransferPacer(
            static_bps=int(limit_mbps * 125000),
            adaptive=bool(self.settings.file_transfer_cc[0]),
        )
        self.uplink_allowance = (
            UplinkAllowance()
            if self.settings.file_transfer_cc[0] and not limit_mbps else None)
        self.upload_dir = pathlib.Path(
            os.path.expanduser(self.settings.file_manager_path)
        ).resolve()
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
            raise FileNotFoundError(
                f"HTTPS enabled but certificate file not found at "
                f"{getattr(self.settings, 'https_cert', '<unset>')}"
            )

        logger.info(
            "Creating TLS context with certificate=%s key=%s", cert_pem, key_pem
        )
        sslctx = ssl.create_default_context(purpose=ssl.Purpose.CLIENT_AUTH)
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
        """Return the most recent modification time of the cert and key files."""
        cert_pem, key_pem = self._get_https_certs()
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
        next poll.
        """
        reload_interval = getattr(self.settings, "cert_reload_interval", 30)
        if reload_interval <= 0:
            logger.info("Automatic certificate reloading is disabled (interval=0)")
            return

        current_site = self.site
        last_mtime = self._get_cert_mtime()
        logger.info(
            "Certificate reload watcher started (interval=%ds, initial mtime=%.0f)",
            reload_interval,
            last_mtime,
        )

        while True:
            await asyncio.sleep(reload_interval)
            try:
                new_mtime = self._get_cert_mtime()
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
                new_ssl_context = self._create_ssl_context()
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

            try:
                await current_site.stop()
                logger.info("Old %s stopped.", self._site_kind())
            except Exception as exc:
                logger.warning("Error stopping old %s: %s", self._site_kind(), exc)

            try:
                new_site = self._build_site(new_ssl_context)
                await new_site.start()
                current_site = new_site
                self.site = new_site
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

        The Bearer header is what scripts send; the ``?token=`` query is what
        URLs the client navigates to rather than fetches carry (the page itself,
        the file-manager listing it opens); the cookie is the mirror the client
        keeps for requests it can put neither on. The cookie value is tried as
        sent and URL-decoded, since the client stores it encoded.

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
        from .selkies import _lookup_session_token

        if self._check_master_token(request.headers.get("Authorization"), settings.master_token):
            return "controller", "header"
        for source, token in self._session_token_carriers(request):
            perms = _lookup_session_token(token)
            if perms is not None:
                role = "controller" if perms.get("role") == "controller" else "viewer"
                return role, source
        return None

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
        and mode-switch control endpoints accept the Bearer master token (a
        mode switch not so authenticated is held to the same Origin rule as
        the upgrades, since a browser attaches cached Basic credentials to a
        cross-site POST); in secure mode every other API route accepts a
        session token (Bearer header, ``?token=`` query, or the client's
        cookie), which is the only credential when Basic auth is off; and
        everything else falls through to Basic Auth when enabled. A cookie-
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
            if not self._check_master_token(auth_header, settings.master_token):
                return self._bearer_challenge()
            return await handler(request)

        is_control_path = path == f"{api_prefix}/api/switch"
        if settings.master_token and is_control_path:
            if self._check_master_token(auth_header, settings.master_token):
                return await handler(request)
            if not settings.enable_basic_auth[0]:
                return self._bearer_challenge()
        if is_control_path and not self._is_origin_allowed(request, settings):
            logger.warning(
                "Rejected mode switch from disallowed Origin: %r",
                request.headers.get("Origin", ""),
            )
            return web.Response(status=403, text="Forbidden origin")

        if (settings.master_token and not is_ws_handshake and not token_path
                and not is_control_path and path.startswith(f"{api_prefix}/api/")):
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
        """
        if not self.settings.enable_basic_auth[0]:
            return
        if self.settings.was_provided("basic_auth_password"):
            return
        logger.error(
            "Basic authentication is enabled but no password was set. Set one with "
            "--basic-auth-password, or the SELKIES_BASIC_AUTH_PASSWORD, PASSWORD or "
            "PASSWD environment variable; or serve without a login by passing "
            "--enable-basic-auth=false."
        )
        raise SystemExit(1)

    async def switch_to_mode(self, mode_name: str) -> None:
        """Stop the active streaming service and start ``mode_name`` in its place.

        Serialized under the supervisor lock so two switches can never overlap;
        switching to the already-active mode is a no-op. The service reads
        the settings at start, so the encoder knob is brought in line with the
        transport first (a websockets-only encoder such as jpeg or striped
        h264enc cannot ride the WebRTC pipeline, and a switch back restores
        the operator's menu and value) and only then does an unpinned
        rate-control mode resolve, since its websockets default depends on
        the resolved encoder, the same order as `_post_process_settings`.

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
            self.settings.resolve_rate_control_default()
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
                    f"Timeout while stopping '{self.current_mode}'. Cancelling task."
                )
                self.active_task.cancel()
                try:
                    await self.active_task
                except asyncio.CancelledError:
                    logger.info(
                        f"Task cancelled after timeout for '{self.current_mode}'."
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

        The control-plane endpoints that change host or session state — the
        streaming-mode switch and file uploads — refuse those requests the way
        the streaming plane refuses input from a viewer. Read-only endpoints
        stay available: a viewer is already watching the session.
        """
        return request.get("auth_role_ceiling") == "viewer"

    async def handle_switch(self, request: web.Request) -> web.Response:
        """POST /api/switch: change the active streaming mode.

        Refused for view-only credentials and when dual mode is disabled.
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

    async def _stream_upload_body(self, request: web.Request, path: str, append: bool) -> int:
        """Stream a request body to ``path`` with executor-thread writes.

        Creates/truncates the file when ``append`` is False, appends when True;
        O_NOFOLLOW blocks a planted symlink either way. Enforces the declared
        Content-Length.

        Reads pace against the shared transfer allowance: a paused read fills
        aiohttp's flow-control buffer, the TCP window closes, and the client's
        uplink is freed for the input/feedback traffic the stream depends on.
        That allowance has no gauge in this direction, so the operator's
        static cap is all of it; the uplink allowance beside it holds an
        unconfigured session below the client's own rate, skipped for a
        transfer too small to stand a queue. Read granularity is what the
        client is handed back as receive window, and it sends every byte of
        it at once, so a paced transfer is read in small steps: a megabyte at
        a time would refill the link's queue in one burst however slowly the
        average is paced.

        Returns:
            The byte count written.

        Raises:
            Exception: Propagated from the read/write path after the handle is
                closed; the caller owns removal of the target file.
        """
        declared = request.content_length
        loop = asyncio.get_running_loop()
        pacer = self.transfer_pacer
        pace_conn = pacer.connection_state(gauged=False) if pacer.active else None
        uplink = self.uplink_allowance
        uplink_state = (
            uplink.transfer()
            if uplink is not None and (declared or 0) >= uplink.MIN_PACED_BYTES
            else None)
        flags = os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW | (os.O_APPEND if append else os.O_TRUNC)
        fd = os.open(path, flags, 0o644)
        fh = os.fdopen(fd, "wb")
        written = 0
        try:
            read_size = (uplink.READ_BYTES if uplink_state is not None else 1 << 20)
            while True:
                waited = time.monotonic()
                chunk = await request.content.read(read_size)
                waited = time.monotonic() - waited
                if not chunk:
                    break
                if declared is not None and written + len(chunk) > declared:
                    raise ValueError("body exceeds declared Content-Length")
                if pace_conn is not None:
                    await pacer.pace(None, len(chunk), pace_conn)
                if uplink_state is not None:
                    await uplink.pace(len(chunk), uplink_state, waited)
                await loop.run_in_executor(None, fh.write, chunk)
                written += len(chunk)
            await loop.run_in_executor(None, fh.close)
        except Exception:
            try:
                fh.close()
            except Exception:
                pass
            raise
        return written

    def _discard_chunked_upload(self, dest: str, part_path: str) -> None:
        """Drop a chunked transfer's tracking entry and its on-disk .part file."""
        self._chunked_uploads.pop(dest, None)
        try:
            os.remove(part_path)
        except OSError:
            pass

    def _expire_stale_chunked_uploads(self) -> None:
        """Reap transfers idle past UPLOAD_PART_TTL_SECONDS (entry + .part file)."""
        now = time.monotonic()
        for key in [k for k, s in self._chunked_uploads.items()
                    if now - s["ts"] > UPLOAD_PART_TTL_SECONDS and not s["busy"]]:
            stale = self._chunked_uploads.pop(key)
            try:
                os.remove(stale["part"])
            except OSError:
                pass
            logger.info(f"Expired stale chunked upload: {key}")

    async def handle_upload(self, request: web.Request) -> web.Response:
        """Stream a client file upload to the file-manager directory over HTTP.

        Available in every streaming mode and not bounded by the data-channel /
        WebSocket per-message size, so it saturates the link where the per-chunk
        SCTP path cannot. The destination path (relative to the file-manager
        root) arrives URL-encoded in the X-Upload-Path header; the body streams
        straight to disk on the executor, so the event loop keeps serving the
        stream during the transfer. Path safety mirrors the data-channel path:
        no traversal outside the root, and O_NOFOLLOW blocks a planted symlink.

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
          Slices accumulate in the staging sibling this destination derives
          (_upload_staging_path). Offset 0 (re)creates it — which is also how a
          stale one from an abandoned transfer for the same path gets replaced —
          and non-zero offsets must exactly continue the tracked transfer (same
          id, offset equal to the bytes already banked, matching staged size) or
          the transfer is discarded with 409. The final slice validates the
          accumulated size against X-Upload-Total and renames the staged file
          onto the destination atomically. Transfers idle past
          UPLOAD_PART_TTL_SECONDS are expired on the next chunked request.

        Both shapes carry the mode of the file they replace onto the replacement
        and are refused for view-only credentials (the view-only password, a
        viewer-role session token).
        """
        if self._viewer_ceiling(request):
            return web.json_response(
                {"status": "error", "message": "View-only credentials cannot upload files"},
                status=403,
            )
        settings = request.app["settings"]
        if "upload" not in settings.file_transfers:
            return web.json_response({"status": "error", "message": "uploads disabled"}, status=403)
        root = getattr(settings, "file_manager_path", "") or ""
        if not root:
            return web.json_response({"status": "error", "message": "uploads disabled"}, status=403)
        root = os.path.expanduser(root)
        rel = urllib.parse.unquote(request.headers.get("X-Upload-Path", "") or "")
        sane = os.path.normpath(rel.strip("/\\"))
        parts = [c for c in sane.split(os.sep) if c and c != "."]
        if not parts or ".." in parts:
            return web.json_response({"status": "error", "message": "invalid upload path"}, status=400)
        dest = os.path.join(root, *parts)
        real_root = os.path.realpath(root)
        parent = os.path.realpath(os.path.dirname(dest))
        try:
            within = os.path.commonpath([real_root, parent]) == real_root
        except ValueError:
            within = False
        if not within:
            return web.json_response({"status": "error", "message": "path escape rejected"}, status=400)
        try:
            os.makedirs(parent, exist_ok=True)
        except OSError as e:
            return web.json_response({"status": "error", "message": f"mkdir failed: {e}"}, status=500)

        upload_id = request.headers.get("X-Upload-Id")
        offset_header = request.headers.get("X-Upload-Offset")
        if (upload_id is None) != (offset_header is None):
            return web.json_response(
                {"status": "error", "message": "X-Upload-Id and X-Upload-Offset must be sent together"},
                status=400,
            )

        if upload_id is None:
            staging = _upload_staging_path(dest, os.urandom(8).hex())
            try:
                written = await self._stream_upload_body(request, staging, append=False)
            except Exception as e:
                try:
                    os.remove(staging)
                except OSError:
                    pass
                return web.json_response({"status": "error", "message": str(e)}, status=400)
            _carry_destination_mode(staging, dest)
            try:
                os.replace(staging, dest)
            except OSError as e:
                try:
                    os.remove(staging)
                except OSError:
                    pass
                return web.json_response({"status": "error", "message": f"finalize failed: {e}"}, status=500)
            logger.info(f"HTTP upload finished: {dest} ({written} bytes)")
            return web.json_response({"status": "success", "bytes": written})

        try:
            offset = int(offset_header or "")
            total = int(request.headers["X-Upload-Total"]) if "X-Upload-Total" in request.headers else -1
        except ValueError:
            return web.json_response({"status": "error", "message": "malformed chunk headers"}, status=400)
        if offset < 0 or ("X-Upload-Total" in request.headers and total < 0):
            return web.json_response({"status": "error", "message": "malformed chunk headers"}, status=400)
        final = request.headers.get("X-Upload-Final") == "1"
        part_path = _upload_staging_path(dest, _upload_staging_token(dest))

        self._expire_stale_chunked_uploads()

        state = self._chunked_uploads.get(dest)
        if offset == 0:
            if state is not None and state["busy"]:
                return web.json_response(
                    {"status": "error", "message": "another chunk for this path is in flight"},
                    status=409,
                )
            state = {"id": upload_id, "offset": 0, "ts": time.monotonic(),
                     "part": part_path, "busy": False}
            self._chunked_uploads[dest] = state
        else:
            if state is not None and state["busy"]:
                # Never discard here: the in-flight writer owns the .part.
                return web.json_response(
                    {"status": "error", "message": "another chunk for this path is in flight"},
                    status=409,
                )
            try:
                part_size = os.path.getsize(part_path)
            except OSError:
                part_size = -1
            if (state is None or state["id"] != upload_id
                    or state["offset"] != offset or part_size != offset):
                self._discard_chunked_upload(dest, part_path)
                return web.json_response(
                    {"status": "error",
                     "message": f"chunk sequence mismatch at offset {offset}; transfer discarded"},
                    status=409,
                )

        state["busy"] = True
        try:
            written = await self._stream_upload_body(request, part_path, append=offset > 0)
        except Exception as e:
            self._discard_chunked_upload(dest, part_path)
            return web.json_response({"status": "error", "message": str(e)}, status=400)
        state["busy"] = False
        state["offset"] = offset + written
        state["ts"] = time.monotonic()

        if not final:
            return web.json_response({"status": "success", "bytes": state["offset"], "complete": False})

        received = state["offset"]
        if total >= 0 and received != total:
            self._discard_chunked_upload(dest, part_path)
            return web.json_response(
                {"status": "error", "message": f"size mismatch: received {received}, expected {total}"},
                status=400,
            )
        _carry_destination_mode(part_path, dest)
        try:
            os.replace(part_path, dest)
        except OSError as e:
            self._discard_chunked_upload(dest, part_path)
            return web.json_response({"status": "error", "message": f"finalize failed: {e}"}, status=500)
        self._chunked_uploads.pop(dest, None)
        logger.info(f"HTTP chunked upload finished: {dest} ({received} bytes)")
        return web.json_response({"status": "success", "bytes": received, "complete": True})

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

        logger.info("Defaulting to packaged web files.")
        try:
            package_path = importlib_resources.files(self.STATIC_CONTENT_PATH)
            self.web_files_ctx = tempfile.TemporaryDirectory(prefix="selkies_web")
            temp_path = pathlib.Path(self.web_files_ctx.name)
            await asyncio.to_thread(self._copy_traversable, package_path, temp_path)

            if (temp_path / "index.html").exists():
                logger.info(f"Using extracted package path from temp dir: {temp_path}")
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

        Pacing applies to plain GETs only: HEAD must answer instantly
        (StreamResponse.write is not empty-body-aware, so it would read and
        pace the whole file), a Range request is a resume or a seek that
        pacing a partial stream buys nothing for while FileResponse
        negotiates the 206, and a conditional request exists to be answered
        with 304/412 from validators only FileResponse computes. Those fall
        through to FileResponse's sendfile path; the paced write loop is what
        keeps a large download from queueing ahead of the video stream on a
        bottleneck link.
        """
        if "download" not in self.settings.file_transfers:
            return web.Response(status=403, text="Forbidden: downloads disabled")
        rel_path = request.match_info.get("path", "").lstrip("/")
        base = str(self.upload_dir)
        parts = [c for c in os.path.normpath(rel_path).split(os.sep) if c and c != "."]
        if ".." in parts:
            return web.Response(
                status=403, text="Forbidden: Directory Traversal detected"
            )
        full_path = pathlib.Path(os.path.realpath(os.path.join(base, *parts)))
        try:
            within = os.path.commonpath([base, str(full_path)]) == base
        except ValueError:
            within = False
        if not within:
            return web.Response(
                status=403, text="Forbidden: Directory Traversal detected"
            )

        if not full_path.exists():
            return web.Response(status=404, text="Not Found")

        if full_path.is_file():
            filename = full_path.name.encode("ascii", "replace").decode().replace('"', "_")
            quoted = urllib.parse.quote(full_path.name)
            conditional = any(
                name in request.headers
                for name in ("If-Modified-Since", "If-None-Match",
                             "If-Range", "If-Unmodified-Since")
            )
            if (self.transfer_pacer.active and request.method == "GET"
                    and "Range" not in request.headers and not conditional):
                sock = request.transport.get_extra_info("socket") if request.transport else None
                conn = self.transfer_pacer.connection_state()
                logger.info(
                    f"Download '{full_path.name}': pacing active "
                    f"(limit={self.transfer_pacer.rate_bps/125000:.1f} Mbit/s, "
                    f"adaptive={self.transfer_pacer.adaptive})")
                size = (await asyncio.to_thread(full_path.stat)).st_size
                content_type = (
                    mimetypes.guess_type(full_path.name)[0]
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
                fh = await asyncio.to_thread(open, full_path, "rb")
                try:
                    while True:
                        chunk = await asyncio.to_thread(fh.read, TransferPacer._CHUNK)
                        if not chunk:
                            break
                        await self.transfer_pacer.pace(sock, len(chunk), conn)
                        await response.write(chunk)
                finally:
                    await asyncio.to_thread(fh.close)
                await response.write_eof()
                return response
            return web.FileResponse(
                full_path,
                headers={
                    "Content-Disposition":
                        f'attachment; filename="{filename}"; filename*=UTF-8\'\'{quoted}'
                },
            )

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
            escaped_name = html.escape(item["name"])
            escaped_mtime = html.escape(item["mtime"])
            escaped_size = html.escape(item["size"])
            rows += f"""
            <tr>
                <td><a href="{urllib.parse.quote(item["name"])}">{escaped_name}</a></td>
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
        unset) still gets downloads instead of a 404; and the Prometheus
        registry is process-global, so one mode-agnostic endpoint serves both
        streaming modes.

        Returns:
            The configured application (also stored on ``self.app``).
        """
        self._require_configured_credentials()

        self.app = web.Application(middlewares=[self._auth_middleware])
        self.app["supervisor"] = self
        self.app["settings"] = self.settings

        api_prefix = self.settings.subfolder
        if api_prefix:
            logger.info(f"Prepending api prefix: {api_prefix!r} to router handlers")

        routes = [
            web.get(f"{api_prefix}/api/status", self.handle_status),
            web.get(f"{api_prefix}/api/health", self.handle_health),
            web.post(f"{api_prefix}/api/switch", self.handle_switch),
            web.post(f"{api_prefix}/api/upload", self.handle_upload),
            web.get(f"{api_prefix}/api/files/{{path:.*}}", self.fancy_index_handler),
        ]
        if self.settings.enable_metrics_http[0]:
            routes.append(web.get(f"{api_prefix}/api/metrics", self.handle_metrics))
        self.app.add_routes(routes)

        for service in self.services.values():
            service.register_routes(api_prefix, self.app.router)

        self.static_fs_path = await self._get_static_content_path()
        if self.static_fs_path:
            async def index_handler(_: web.Request) -> web.FileResponse:
                return web.FileResponse(os.path.join(self.static_fs_path, "index.html"))

            self.app.router.add_get(f"{api_prefix}/", index_handler)
            self.app.router.add_static(
                f"{api_prefix}/", self.static_fs_path, name="static"
            )
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

    def _build_site(self, ssl_context: Optional[ssl.SSLContext] = None) -> web.BaseSite:
        """Build the aiohttp site for the configured listener: a Unix domain
        socket when ``unix_socket`` is set, otherwise the TCP addr/port pair."""
        sock_path = self._unix_socket_path()
        if sock_path:
            parent = os.path.dirname(sock_path)
            if parent:
                os.makedirs(parent, exist_ok=True)
            self._clear_stale_unix_socket(sock_path)
            return web.UnixSite(self.runner, path=sock_path, ssl_context=ssl_context)
        return web.TCPSite(
            self.runner,
            host=self.settings.addr,
            port=self.settings.port,
            ssl_context=ssl_context,
        )

    def _site_kind(self) -> str:
        return "UnixSite" if self._unix_socket_path() else "TCPSite"

    def _site_endpoint(self) -> str:
        sock_path = self._unix_socket_path()
        if sock_path:
            return f"unix://{sock_path}"
        https = getattr(self.settings, "enable_https", (False,))[0]
        return f"{'https' if https else 'http'}://{self.settings.addr}:{self.settings.port}"

    async def start_server(self) -> None:
        """Start the HTTP/HTTPS server and, under HTTPS, the cert-reload watcher."""
        if not self.app:
            await self.initialize_app()

        https = getattr(self.settings, "enable_https", (False,))[0]
        if https:
            try:
                self.ssl_context = self._create_ssl_context()
            except Exception as exc:
                logger.error("Failed to create SSL context at startup: %s", exc)
                raise

        self.runner = web.AppRunner(self.app, access_log_class=PathOnlyAccessLogger)
        await self.runner.setup()

        try:
            self.site = self._build_site(self.ssl_context)
        except Exception as exc:
            logger.error("Cannot bind %s: %s", self._site_endpoint(), exc)
            raise

        logger.info("Selkies server running on %s", self._site_endpoint())
        await self.site.start()

        if https:
            self.cert_watcher = asyncio.create_task(self._watch_and_reload_certs())

    async def stop_server(self) -> None:
        """Stop the server gracefully: cert watcher, active service, listener,
        extracted web files, and the runner, in that order."""
        if self.cert_watcher and not self.cert_watcher.done():
            self.cert_watcher.cancel()
            try:
                await self.cert_watcher
            except asyncio.CancelledError:
                pass

        await self._stop_service()

        if self.web_files_ctx:
                self.web_files_ctx.cleanup()
        if self.site:
            await self.site.stop()
            self._remove_own_unix_socket()
        if self.runner:
            await self.runner.cleanup()
            logger.info("Server cleanup complete.")

    async def run(self) -> None:
        """Start the server and serve until cancelled, then clean up."""
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
