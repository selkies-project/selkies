# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""WebSockets-mode streaming server: the main Selkies application module.

Owns the data WebSocket plane for the websockets transport: per-client
connection handling (auth/roles, input dispatch, settings), the per-display
pixelflux video captures with their bounded per-client relays, the shared
pcmflux Opus audio fan-out (with the all-clients Opus+RED gate), microphone
forwarding, clipboard/cursor delivery, stats collection, and the display
layout/reconfiguration engine shared conceptually with the WebRTC transport
(parity between the two transports, and between X11 and Wayland, is a design
requirement).

Threading model: one asyncio event loop runs everything control-plane.
pixelflux/pcmflux deliver frames from their own native threads; those
callbacks never touch asyncio state directly — they hand zero-copy items to
the loop via ``call_soon_threadsafe``. Per-client video delivery is bounded
by ``_VideoRelay`` (drop-and-resync past a byte budget) and audio by a fixed
queue, so one slow client can never back pressure the shared pipeline.
Blocking native calls (capture start/stop, geometry reads) run on executor
threads. The Wayland path is subprocess-free by design: compositor output
management, DPI-as-output-scale, and cursor sizing all go through the
in-process pixelflux handle, never through forked tools.

Wire framing: text frames carry the control verbs; binary frames are typed
by their first byte — 0x01 Opus audio (pcmflux's native header, sent as-is),
0x03 JPEG and 0x04 H.264 video stripes from pixelflux, 0x02 client mic PCM,
`WS_OPCODE_WEBCAM` a webcam frame for the virtual camera, and 0x05 a gzip
wrapped control text. A client that sends `_gz,1` can inflate gzip: control
text at or above `WS_GZIP_MIN_BYTES` then goes out as 0x05 frames (small,
latency-critical messages such as input stay raw), and the server echoes the
verb so the client gzips its own large sends, which the handler inflates
back into text before dispatch. Video chunks carry a uint16 frame id that
the display's owning client acknowledges (`CLIENT_FRAME_ACK`); the ack
cadence sizes that display's backpressure window and the matched send
stamps feed its smoothed RTT.
"""
import asyncio
import inspect
import base64
import contextlib
import gzip
import hmac
import json
import logging
import os
import time
from collections import OrderedDict, deque
from datetime import datetime
from enum import Enum
from types import SimpleNamespace
from typing import Any, Awaitable, Callable, Optional, Tuple, Union

import psutil
from aiohttp import web, WSMsgType

from . import audio_config
from . import gpu_stats
from .audio_control import AudioControl, ensure_capture_sink
from .display_utils import (
    apply_common_capture_settings,
    parse_gpu_id,
    format_pixelflux_cursor,
    get_new_res,
    generate_xrandr_gtf_modeline,
    ensure_mode,
    resize_display,
    clear_selkies_monitors,
    replace_selkies_monitors,
    reconcile_realized_layout,
    read_realized_root,
    MultiMonitorWindowManager,
    wayland_output_id,
    session_screen_index,
    wayland_reposition_primary,
    grow_framebuffer,
    set_dpi,
    set_cursor_size,
    clamp_primary_feedback,
    compute_dual_layout,
    parse_resize_dims,
    cursor_size_for_dpi,
    align_dims_16,
)
from .input_handler import (
    BULK_DRAIN_TIMEOUT_S,
    WebRTCInput as InputHandler,
    CLIPBOARD_CHUNK_SIZE,
    VIEWER_ALLOWED_PREFIXES,
    VIEWER_COLLAB_EXTRA_PREFIXES,
    VIEWER_SILENT_DROP_PREFIXES,
    run_client_command,
)
from .settings import settings, SETTING_DEFINITIONS, WS_MAX_MESSAGE_BYTES, WS_MESSAGE_SIZE_HARD_CAP, build_client_settings_payload, effective_use_cpu, inflate_gz_bounded, sanitize_client_setting
from .settings import settings as app_settings
from .webcam import (
    MSG_WEBCAM_DISABLED,
    MSG_WEBCAM_KEYFRAME,
    WS_FLAG_KEYFRAME,
    WS_HEADER_LEN,
    WS_OPCODE_WEBCAM,
    get_shared_webcam,
    orientation_from_flags,
    webcam_uplink_allowed,
)
from .stream_server import BaseStreamingService, note_pong
from .webrtc_utils import Metrics

BACKPRESSURE_ALLOWED_DESYNC_MS = 2000
BACKPRESSURE_LATENCY_THRESHOLD_MS = 50
# Cap on RTT-based desync forgiveness: RTT rides the send->ack path backpressure
# bounds, so uncapped, a growing queue would loosen its own trigger. Real
# propagation delay is under a second; the rest is self-inflicted queue delay.
BACKPRESSURE_LATENCY_FORGIVENESS_MAX_MS = 1000
# Ack round trips above this measure a stalled path or an id collision, not the
# link; one such sample would skew the flat smoothing window for its lifetime.
RTT_SAMPLE_SANE_MAX_MS = 10000
BACKPRESSURE_CHECK_INTERVAL_S = 0.5
MAX_UINT16_FRAME_ID = 65535
FRAME_ID_SUSPICIOUS_GAP_THRESHOLD = (
    MAX_UINT16_FRAME_ID // 2
)
STALLED_CLIENT_TIMEOUT_SECONDS = 4.0
# Liveness bound for one send on the shared audio fan-out and the video relays:
# backlogs are bounded upstream, so a send this slow means a dead socket, which
# is dropped and never reused (the cancelled write tore its framing).
SHARED_STREAM_SEND_TIMEOUT_SECONDS = 1.0
# Per-client video backlog bound as seconds of stream at the configured bitrate
# (backlog is latency debt, so it tracks the rate), floored so low-bitrate
# streams still absorb transport jitter; see _VideoRelay.
VIDEO_RELAY_BUDGET_SECONDS = 2.0
VIDEO_RELAY_BUDGET_MIN_BYTES = 4 * 1024 * 1024
# Floor between one relay's keyframe re-requests while gated: pixelflux collapses
# concurrent requests into one flag, so this only bounds the IDR bitrate a
# hopeless client can add to the shared stream (~1 IDR/s).
VIDEO_RELAY_SYNC_FLOOR_SECONDS = 1.0
RTT_SMOOTHING_SAMPLES = 20
# RFC 2198 RED redundancy depth (distance=2) for the shared Opus audio stream.
AUDIO_RED_DISTANCE = 2
# Silence on the audio queue before the broadcast loop asks pcmflux whether its
# worker died, and the floor between the restarts that answer.
PCMFLUX_HEALTH_INTERVAL_SECONDS = 2.0
PCMFLUX_RESTART_FLOOR_SECONDS = 30.0
SENT_FRAME_TIMESTAMP_HISTORY_SIZE = 1000
# A resuming viewer bypasses the START_VIDEO throttle but not this floor: each
# resume forces an IDR, so rapid STOP/START must not be able to spam keyframes.
VIEWER_RESUME_MIN_INTERVAL_S = 1.0
TARGET_FRAMERATE = 60

UINPUT_MOUSE_SOCKET = settings.uinput_mouse_socket
ENABLE_CURSORS = bool(settings.enable_cursors[0])
DEBUG_CURSORS = bool(settings.debug_cursors[0])
ENABLE_RESIZE = bool(settings.enable_resize[0])
AUDIO_CHANNELS_DEFAULT = 2
# An operator override of the audio_bitrate enum reaches here as an arbitrary
# numeric string; a fractional value must not abort module import.
AUDIO_BITRATE_DEFAULT = int(float(settings.audio_bitrate))
PIXELFLUX_VIDEO_ENCODERS = ["jpeg", "h264enc", "h264enc-striped"]

LOGLEVEL = logging.INFO
logging.basicConfig(level=LOGLEVEL)
logger_selkies_gamepad = logging.getLogger("selkies_gamepad")
logger_app = logging.getLogger("app")
logger_app_resize = logging.getLogger("app_resize")
logger_input_handler = logging.getLogger("input_handler")
logger = logging.getLogger("main")
data_logger = logging.getLogger("data_websocket")

X11_CAPTURE_AVAILABLE = False
PCMFLUX_AVAILABLE = False
PCMFLUX_PLAYBACK_AVAILABLE = False

# pixelflux never reads this itself: the backend reaches each capture through
# the CaptureSettings use_wayland field.
IS_WAYLAND = bool(settings.wayland[0])

# Cursor base size in points at 96 DPI, or None for "auto", which disables every
# cursor-size override so a later DPI sync never stomps the compositor/DE choice.
CURSOR_SIZE: Optional[int] = settings.cursor_size if settings.cursor_size > 0 else None


def _scaling_dpi_bounds() -> tuple[int, int]:
    """Numeric span of the declared scaling_dpi stops.

    The 's,' DPI verb accepts any value in between (a client's device-pixel
    ratio need not land on a stop), but never outside it: the DPI is applied to
    the desktop and, when an explicit cursor size is configured, scales the
    cursor request with it.

    Returns:
        The `(min, max)` DPI bounds, falling back to `(96, 288)` when no stops
        are declared.
    """
    definition = next((s for s in SETTING_DEFINITIONS if s['name'] == 'scaling_dpi'), None)
    stops = [float(v) for v in (definition or {}).get('meta', {}).get('allowed', [])]
    return (int(min(stops)), int(max(stops))) if stops else (96, 288)


SCALING_DPI_MIN, SCALING_DPI_MAX = _scaling_dpi_bounds()
_EXPLICIT_GPU_ID = parse_gpu_id(settings.gpu_id)
GPU_ID_DEFAULT = _EXPLICIT_GPU_ID if _EXPLICIT_GPU_ID is not None and _EXPLICIT_GPU_ID >= 0 else 0

try:
    from pcmflux import (
        AudioCapture,
        AudioCaptureSettings,
        AudioPlayback,
        AudioPlaybackSettings,
    )
    PCMFLUX_AVAILABLE = True
    PCMFLUX_PLAYBACK_AVAILABLE = True
    data_logger.info("pcmflux library found. Audio capture + mic playback available.")
except (ImportError, RuntimeError) as e:
    # RuntimeError is pcmflux ABI/version skew: degrade instead of crashing at startup.
    AudioCapture = AudioCaptureSettings = None
    AudioPlayback = AudioPlaybackSettings = None
    PCMFLUX_AVAILABLE = False
    PCMFLUX_PLAYBACK_AVAILABLE = False
    data_logger.warning("pcmflux library not found. Audio capture is unavailable. (%s)", e)

try:
    from pixelflux import CaptureSettings, ScreenCapture

    X11_CAPTURE_AVAILABLE = True
    data_logger.info("pixelflux library found. Striped encoding modes available.")
except (ImportError, RuntimeError) as e:
    # RuntimeError is pixelflux ABI/version skew: degrade instead of crashing at startup.
    ScreenCapture = CaptureSettings = None
    X11_CAPTURE_AVAILABLE = False
    data_logger.warning(
        f"pixelflux library unavailable ({e}). Striped encoding modes unavailable."
    )

upload_path: str = str(getattr(settings, 'file_manager_path', '') or '~/Desktop')
# None when the directory could not be created (uploads disabled).
upload_dir_path: Optional[str] = os.path.expanduser(upload_path)

try:
    os.makedirs(upload_dir_path, exist_ok=True)
    logger.info(f"Upload directory ensured: {upload_dir_path}")
except OSError as e:
    logger.error(f"Could not create upload directory {upload_dir_path}: {e}")
    upload_dir_path = None

user_tokens: dict[str, dict] = {}
client_permissions: dict[Any, dict] = {}
active_mk_token: Optional[str] = None


def current_session_tokens() -> tuple[dict[str, dict], Optional[str]]:
    """Live control-plane token view, as provisioned via /api/tokens.

    Returns:
        The `(user_tokens mapping, active mk-token)` pair. Both transports
        authorize input against this.
    """
    return user_tokens, active_mk_token


def _perms_hold_input_authority(perms: Optional[dict], token: Optional[str] = None) -> bool:
    """Apply the single input-authority rule shared by both transports.

    While an mk token is active only its holder may drive keyboard/mouse (and
    the commands and clipboard that ride the same gate); otherwise any
    controller-role client may.

    Args:
        perms: The client's permission entry (may be None or empty).
        token: Covers callers holding a user_tokens entry, which carries no
            token field of its own.
    """
    perms = perms or {}
    if active_mk_token is not None:
        return (perms.get("token") if token is None else token) == active_mk_token
    return perms.get("role", "viewer") == "controller"


def _mk_access_verdict(perms: Optional[dict], token: Optional[str] = None) -> bool:
    """The MK_ACCESS verdict a tokened websockets client is told.

    Input authority under the mk-token rule, with a viewer additionally held
    to enable_collab: a read-only viewer must not attach an input context
    whose every message the gate then drops. The same verdict WebRTC pushes
    at data-channel open.

    Args:
        perms: The client's permission entry (may be None or empty).
        token: Covers callers holding a user_tokens entry, which carries no
            token field of its own.
    """
    perms = perms or {}
    if perms.get("role") != "controller" and not bool(app_settings.enable_collab[0]):
        return False
    return _perms_hold_input_authority(perms, token=token)


def _lookup_session_token(token: Optional[str]) -> Optional[dict]:
    """The user_tokens entry for a session token, compared in constant time.

    Every provisioned token is compared (no early exit), so the reply timing
    does not tell a probing client how far its guess matched or which table
    entry it resembles.

    Returns:
        The token's permission entry, or None when it is not provisioned.
    """
    if not token:
        return None
    candidate = token.encode("utf-8")
    match = None
    for known, perms in user_tokens.items():
        if hmac.compare_digest(candidate, known.encode("utf-8")):
            match = perms
    return match


# Set by the WebRTC service so a token update also reconciles live WebRTC peers;
# reconcile_clients() itself only walks websockets sockets.
webrtc_reconcile_hook: Optional[Callable[[], Awaitable[None]]] = None


# Below this, gzip does not pay for its CPU and latency-critical small messages
# must stay raw; matches the WebRTC data-channel threshold.
WS_GZIP_MIN_BYTES = 512

# From this size gzip runs on an executor thread: level-6 compression of a
# multi-MB clipboard would stall the loop for tens of milliseconds.
WS_GZIP_OFFLOAD_BYTES = 512 * 1024


def _path_is_within(directory: str, target: str) -> bool:
    """Return True if `target` is `directory` itself or strictly inside it.

    Compares on path-segment boundaries via os.path.commonpath rather than a
    bare string prefix (which would accept sibling dirs sharing a name prefix).
    Both arguments should already be absolute/realpath-resolved by the caller.
    """
    directory = os.path.abspath(directory)
    target = os.path.abspath(target)
    try:
        return os.path.commonpath([directory, target]) == directory
    except ValueError:
        return False


_background_tasks: set[asyncio.Task] = set()


def _spawn_background_task(coro, name: Optional[str] = None) -> asyncio.Task:
    """Run a fire-and-forget coroutine with a strong reference held until it
    finishes: the event loop keeps only weak references, so an unreferenced task
    can be garbage-collected mid-flight."""
    task = asyncio.create_task(coro, name=name)
    _background_tasks.add(task)
    task.add_done_callback(_background_tasks.discard)
    return task


def _close_abandoned_ws(client: web.WebSocketResponse) -> None:
    """Close a dropped socket in the background: close() can itself block
    draining the same paused transport that stalled the send, so it must
    never run inline on a broadcast path."""
    async def _close():
        try:
            await asyncio.wait_for(client.close(), timeout=2.0)
        except Exception:
            pass
    _spawn_background_task(_close())


async def _broadcast_to_clients(
    clients: set,
    message: Union[str, bytes, bytearray, memoryview],
    per_client_timeout: Optional[float] = None,
    only: Optional[int] = None,
) -> set:
    """Broadcast concurrently to all clients, removing only on clear connection errors.

    When per_client_timeout is set, a client whose send stalls past the bound
    is treated as dead: the send is cancelled and the socket is dropped and
    closed. A cancelled send_str may have left a half-written frame on the
    wire, so that socket must never be reused for later sends.

    Args:
        clients: The socket set to fan out over; dead sockets are removed from
            it in place.
        message: Text control message, or raw bytes for binary frames.
        per_client_timeout: Per-send liveness bound in seconds; None sends
            unbounded.
        only: Connection identity (`id(socket)`) to address alone, for an
            answer that belongs to one client rather than to the session. A
            requester that has since disconnected receives nothing.

    A message over WS_MESSAGE_SIZE_HARD_CAP is an upstream bug (large control
    payloads are segmented far below it) and is refused rather than sent: it
    would trip proxy/WS-stack frame limits and stall the socket. The gzip
    frame for gzip-capable clients is computed at most once per broadcast,
    on an executor thread from WS_GZIP_OFFLOAD_BYTES up, and shared by every
    recipient.

    Returns:
        The set of clients dropped by this call. Removal mutates the PASSED
        collection, so callers that fan out over a computed temporary set (the
        media senders) must subtract the returned set from their authoritative
        registry themselves — otherwise the dead socket re-enters the very next
        per-frame set.
    """
    if not clients:
        return set()
    recipients = clients if only is None else {c for c in clients if id(c) == only}
    if not recipients:
        return set()

    # len() is the byte count here: large control messages are base64/ASCII JSON.
    if len(message) > WS_MESSAGE_SIZE_HARD_CAP:
        data_logger.error(
            f"Refusing to broadcast a {len(message)}-byte WebSocket message "
            f"(hard cap {WS_MESSAGE_SIZE_HARD_CAP} bytes); message dropped."
        )
        return set()

    # Holds ready bytes or one shared executor future.
    gz_frame_holder = []
    loop = asyncio.get_running_loop()

    def _gzip_frame():
        return b"\x05" + gzip.compress(message.encode("utf-8"), 6)

    async def _send_one(client):
        if isinstance(message, (bytes, bytearray, memoryview)):
            await client.send_bytes(message)
        elif getattr(client, "_ws_gz", False) and len(message) >= WS_GZIP_MIN_BYTES:
            # No await between this test and the append: concurrent sends must
            # not double-compress.
            if not gz_frame_holder:
                if len(message) >= WS_GZIP_OFFLOAD_BYTES:
                    gz_frame_holder.append(loop.run_in_executor(None, _gzip_frame))
                else:
                    gz_frame_holder.append(_gzip_frame())
            frame = gz_frame_holder[0]
            if asyncio.isfuture(frame):
                # Shielded: one client's send timeout must not cancel the
                # compression the others are waiting on.
                frame = await asyncio.shield(frame)
                gz_frame_holder[0] = frame
            await client.send_bytes(frame)
        else:
            await client.send_str(message)

    # Single-recipient fast path (the common case), same removal semantics as below.
    if len(recipients) == 1:
        client = next(iter(recipients))
        if client.closed:
            clients.discard(client)
            return {client}
        try:
            if per_client_timeout is not None:
                await asyncio.wait_for(_send_one(client), timeout=per_client_timeout)
            else:
                await _send_one(client)
        except asyncio.TimeoutError:
            clients.discard(client)
            _close_abandoned_ws(client)
            return {client}
        except ConnectionResetError:
            clients.discard(client)
            return {client}
        except (OSError, RuntimeError) as result:
            if any(term in str(result).lower() for term in ['broken pipe', 'connection reset', 'closed']):
                clients.discard(client)
                return {client}
            data_logger.warning(f"Broadcast exception (client not removed): {type(result).__name__}: {result}")
        return set()

    client_task_pairs = []
    closed_clients = set()
    timed_out_clients = set()

    for client in recipients:
        if client.closed:
            closed_clients.add(client)
            continue
        if per_client_timeout is not None:
            task = asyncio.wait_for(_send_one(client), timeout=per_client_timeout)
        else:
            task = _send_one(client)
        client_task_pairs.append((client, task))

    tasks = [task for _, task in client_task_pairs]
    results = await asyncio.gather(*tasks, return_exceptions=True)
    for (client, _), result in zip(client_task_pairs, results):
        if isinstance(result, asyncio.CancelledError):
            # A BaseException the Exception branch never sees: without this the
            # client is neither delivered to nor dropped.
            data_logger.warning("Broadcast send was cancelled; dropping the socket.")
            timed_out_clients.add(client)
        elif isinstance(result, Exception):
            # TimeoutError first: on 3.11+ it subclasses OSError.
            if isinstance(result, asyncio.TimeoutError):
                timed_out_clients.add(client)
            elif isinstance(result, ConnectionResetError):
                closed_clients.add(client)
            elif isinstance(result, (OSError, RuntimeError)):
                err_msg = str(result).lower()
                if any(term in err_msg for term in ['broken pipe', 'connection reset', 'closed']):
                    closed_clients.add(client)
            else:
                data_logger.warning(f"Broadcast exception (client not removed): {type(result).__name__}: {result}")

    for client in timed_out_clients:
        _close_abandoned_ws(client)
    closed_clients |= timed_out_clients

    if closed_clients:
        clients -= closed_clients
    return closed_clients


class _VideoRelay:
    """Bounded video delivery for one (client, display) pair.

    The fan-out offers every encoded chunk synchronously and never awaits a
    socket; each relay's own task drains its backlog. One slow client can
    therefore neither pace the other clients nor back frames up into the
    shared pipeline or its socket transport (whose freed burst peaks the
    allocator retains, ratcheting RSS): past its byte budget (~
    VIDEO_RELAY_BUDGET_SECONDS of stream at the configured bitrate) it drops
    its backlog and skips ahead to the next keyframe, the standard
    broadcast-video contract. Keyframes are exempt from the budget (part of
    one is useless), so the true bound is budget plus one keyframe burst.

    H.264 chain safety is tracked per stripe ROW (wire-header y_start, bytes
    4:6): one capture frame can mix IDR and delta stripes (a lone stripe
    encoder re-init IDRs only its own row), so after any drop a row's delta
    chunks stay gated until that row's own IDR arrives — a delivered delta
    otherwise decodes against a reference the client never received. The
    wire type byte (offset 1) is stamped from the encoder's ACTUAL output
    picture type on every backend, and a requested recovery IDR covers every
    row (force_idr_all), so gated rows converge on the next request. JPEG
    chunks (0x03) have no reference chain: never gated, and a drop only
    costs a repaint request. A fresh relay starts fully gated, so a joining
    client waits for a keyframe instead of decoding mid-GOP garbage.

    Attributes:
        budget: Skip-ahead byte bound for the backlog (`_video_relay_budget`).
        backlog: Undrained fan-out items, oldest first.
        backlog_bytes: Payload bytes held in `backlog`.
        live_rows: Stripe rows whose IDR was accepted into the current backlog;
            only their delta chunks are chain-continuous for this client.
        stopped: Set by `stop`; the drain task exits after its in-flight send.
    """

    __slots__ = ('server', 'display_id', 'ws', 'budget', 'backlog',
                 'backlog_bytes', 'live_rows', 'stopped', '_wake', '_task',
                 '_next_sync_req')

    def __init__(self, server: "DataStreamingServer", display_id: str,
                 ws: web.WebSocketResponse, budget: int) -> None:
        self.server = server
        self.display_id = display_id
        self.ws = ws
        self.budget = budget
        self.backlog: deque = deque()
        self.backlog_bytes = 0
        self.live_rows: set[int] = set()
        self.stopped = False
        self._wake = asyncio.Event()
        self._task: Optional[asyncio.Task] = None
        self._next_sync_req = 0.0

    def start(self) -> None:
        self._task = asyncio.create_task(
            self._run(), name=f"VideoRelay:{self.display_id}")

    def stop(self) -> None:
        """Graceful: an in-flight send completes — cancelling mid-frame would
        tear the websocket framing on a socket that stays open for control."""
        self.stopped = True
        self.backlog.clear()
        self.backlog_bytes = 0
        self._wake.set()

    def flush_for_gate(self) -> None:
        """ACK backpressure engaged: drop the undrained backlog and gate every
        row, so the client resumes only at the IDR that
        _set_backpressure_enabled requests when the gate lifts."""
        if self.backlog or self.live_rows:
            self.backlog.clear()
            self.backlog_bytes = 0
            self.live_rows.clear()

    def _want_sync(self) -> bool:
        """Rate-limit this relay's keyframe (re)requests to the sync floor."""
        now = time.monotonic()
        if now >= self._next_sync_req:
            self._next_sync_req = now + VIDEO_RELAY_SYNC_FLOOR_SECONDS
            return True
        return False

    def offer(self, item: dict) -> bool:
        """Accept, drop, or gate one encoded chunk.

        Runs on the event loop and never awaits.

        Args:
            item: The fan-out item (`data` memoryview, `owner` frame,
                `frame_id`).

        Returns:
            True when the caller should request a keyframe (data was dropped
            that only a sync point recovers).
        """
        data = item['data']
        size = len(data)
        is_h264 = size >= 10 and data[0] == 0x04
        is_idr = is_h264 and data[1] == 0x01
        dropped = False
        if (not is_idr and self.backlog
                and self.backlog_bytes + size > self.budget):
            self.backlog.clear()
            self.backlog_bytes = 0
            self.live_rows.clear()
            dropped = True
        deliver = True
        if is_h264:
            row = (data[4] << 8) | data[5]
            if is_idr:
                self.live_rows.add(row)
            elif row not in self.live_rows:
                deliver = False
                dropped = True
        if deliver:
            self.backlog.append(item)
            self.backlog_bytes += size
            self._wake.set()
        return dropped and self._want_sync()

    async def _run(self) -> None:
        """Drain the backlog onto the socket until stopped or the socket dies."""
        try:
            while True:
                if self.stopped:
                    return
                if not self.backlog:
                    self._wake.clear()
                    await self._wake.wait()
                    continue
                item = self.backlog.popleft()
                data = item['data']
                self.backlog_bytes -= len(data)
                # Stamped before the await, and only for the display's
                # registered client: that is what the ACK RTT math measures.
                ds = self.server.display_clients.get(self.display_id)
                if ds is not None and ds.get('ws') is self.ws:
                    fid = item['frame_id']
                    ds['sent_timestamps'][fid] = time.monotonic()
                    ds['last_sent_frame_id'] = fid
                    ds['has_sent_any_frame'] = True
                    if len(ds['sent_timestamps']) > SENT_FRAME_TIMESTAMP_HISTORY_SIZE:
                        ds['sent_timestamps'].popitem(last=False)
                try:
                    await asyncio.wait_for(
                        self.ws.send_bytes(data),
                        timeout=SHARED_STREAM_SEND_TIMEOUT_SECONDS,
                    )
                except asyncio.TimeoutError:
                    # Checked before OSError: on 3.11+ TimeoutError subclasses it.
                    data_logger.warning(
                        f"Video relay for '{self.display_id}' send stalled past "
                        f"{SHARED_STREAM_SEND_TIMEOUT_SECONDS}s; dropping client.")
                    self.server.clients.discard(self.ws)
                    _close_abandoned_ws(self.ws)
                    return
                except (ConnectionResetError, OSError, RuntimeError):
                    self.server.clients.discard(self.ws)
                    return
                self.server._bytes_sent_in_interval += len(data)
        finally:
            group = self.server.video_relay_groups.get(self.display_id)
            if group is not None and group.get(self.ws) is self:
                del group[self.ws]


class SelkiesAppError(Exception):
    """Application-level error raised for unrecoverable streaming conditions."""

class RateControlMode(str, Enum):
    """H.264 rate-control mode: constant bitrate or constant quality (CRF)."""
    CBR = "cbr"
    CRF = "crf"

class SelkiesStreamingApp:
    """Session-level streaming state shared across transports.

    Holds the display geometry, encoder/framerate/bitrate defaults, and the
    clipboard/cursor delivery helpers that broadcast over the data websocket.
    The heavy lifting (captures, relays, reconfiguration) lives in
    DataStreamingServer; this object is the small shared surface that the
    input handler and both transports address.

    Attributes:
        display_width: Primary display geometry, seeded from a configured
            manual resolution (even-masked), else 1024x768, for captures that
            start before any client sized the display; on Wayland the capture
            start resizes the compositor output to it.
        display_height: See `display_width`.
        audio_channels: Configured capture channel count (surround captures
            as multistream Opus).
        audio_bitrate: Session Opus bitrate in bps.
        encoder: Session default encoder for later-registered displays.
        framerate: Session default framerate for later-registered displays.
        last_cursor_sent: Cached cursor payload replayed to joining clients.
        pipeline_running: Cleared by `stop_pipeline`.
        server_enable_resize: Whether clients may resize the primary display.
    """

    def __init__(
        self,
        async_event_loop: asyncio.AbstractEventLoop,
        framerate: int,
        encoder: str,
        data_streaming_server: Optional["DataStreamingServer"] = None,
        mode: str = "websockets",
    ) -> None:
        self.server_enable_resize = ENABLE_RESIZE
        self.mode = mode
        self.display_width = 1024
        self.display_height = 768
        if settings.is_manual_resolution_mode[0]:
            manual_w = int(settings.manual_width or 0)
            manual_h = int(settings.manual_height or 0)
            if manual_w > 0 and manual_h > 0:
                self.display_width = manual_w - (manual_w % 2)
                self.display_height = manual_h - (manual_h % 2)
        self.pipeline_running = False
        self.async_event_loop = async_event_loop
        self.audio_channels = int(getattr(settings, 'audio_channels', AUDIO_CHANNELS_DEFAULT)
                                  or AUDIO_CHANNELS_DEFAULT)
        self.gpu_id = GPU_ID_DEFAULT
        self.audio_bitrate = AUDIO_BITRATE_DEFAULT
        self.encoder = encoder
        self.framerate = framerate
        self.last_cursor_sent = None
        self.data_streaming_server = data_streaming_server

    async def send_ws_clipboard_data(
        self,
        data: Union[str, bytes],
        mime_type: str = "text/plain",
        reply_to: Optional[str] = None,
        conn_id: Optional[int] = None,
    ) -> None:
        """Send clipboard data to the session's clients, multipart when large.

        Args:
            data: Clipboard text (str) or binary payload (bytes).
            mime_type: The payload's MIME type; anything but "text/plain" is
                treated as binary and gated on enable_binary_clipboard.
            reply_to: Set to the requesting verb (e.g. "cr") when this send
                answers a client fetch rather than announcing a server-side
                clipboard change. A `clipboard_reply,<verb>` frame then
                precedes the payload frames on the same ordered socket, so
                clients can treat the payload cache-only without time
                heuristics. Legacy clients route the unknown verb to their
                input module, which ignores it.
            conn_id: Connection that asked for this payload. An answer goes to
                that client alone: every other one already holds the content or
                is about to be told of a change, and a tagged reply they did
                not ask for is read as their own fetch and cached without ever
                reaching their clipboard.

        Payload frames get the bulk tolerance the data channel's drain allows
        (a slow link is not a dead client); control frames keep the liveness
        bound, since one stalled client must not wedge clipboard delivery for
        all.
        """
        if not (self.data_streaming_server and self.data_streaming_server.clients):
            data_logger.warning("Cannot send clipboard: no clients or server not ready.")
            return
        try:
            is_binary = mime_type != "text/plain"
            if is_binary and not self.data_streaming_server.enable_binary_clipboard:
                data_logger.warning(
                    f"Attempted to send binary clipboard data ({mime_type}) but feature is disabled on server."
                )
                return
            if reply_to:
                await _broadcast_to_clients(
                    self.data_streaming_server.clients,
                    f"clipboard_reply,{reply_to}", per_client_timeout=2.0,
                    only=conn_id)
            data_bytes = data.encode('utf-8') if not is_binary and isinstance(data, str) else data
            total_size = len(data_bytes)
            if total_size < CLIPBOARD_CHUNK_SIZE:
                encoded_data = base64.b64encode(data_bytes).decode('ascii')
                if is_binary:
                    message = f"clipboard_binary,{mime_type},{encoded_data}"
                else:
                    message = f"clipboard,{encoded_data}"
                await _broadcast_to_clients(self.data_streaming_server.clients, message,
                                            per_client_timeout=BULK_DRAIN_TIMEOUT_S, only=conn_id)
            else:
                data_logger.info(f"Sending large clipboard data ({mime_type}, {total_size} bytes) via multipart.")
                start_message = f"clipboard_start,{mime_type},{total_size}"
                clients = self.data_streaming_server.clients

                async def deliver(cid: int) -> None:
                    """One pipeline per client, a chunk at a time: a slow link
                    paces only its own transfer and a dead one drops out of the
                    set without touching anyone else's."""
                    if await _broadcast_to_clients(clients, start_message,
                                                   per_client_timeout=2.0, only=cid):
                        return
                    offset = 0
                    while offset < total_size:
                        chunk = data_bytes[offset:offset + CLIPBOARD_CHUNK_SIZE]
                        data_message = "clipboard_data," + base64.b64encode(chunk).decode('ascii')
                        if await _broadcast_to_clients(clients, data_message,
                                                       per_client_timeout=BULK_DRAIN_TIMEOUT_S, only=cid):
                            return
                        offset += len(chunk)
                        await asyncio.sleep(0)
                    await _broadcast_to_clients(clients, "clipboard_finish",
                                                per_client_timeout=2.0, only=cid)

                recipients = [id(c) for c in list(clients) if conn_id is None or id(c) == conn_id]
                await asyncio.gather(*(deliver(cid) for cid in recipients))
                data_logger.info("Finished sending multi-part clipboard data.")
        except Exception as e:
            data_logger.error(f"Failed to send clipboard data: {e}", exc_info=True)

    def send_ws_cursor_data(self, data: dict) -> None:
        """Broadcast a cursor-change payload to all clients.

        Thread-safe: called from pixelflux's cursor thread, so the broadcast is
        scheduled onto the event loop via run_coroutine_threadsafe rather than
        awaited. The payload is also cached as last_cursor_sent so late-joining
        clients receive the current cursor at connect.
        """
        self.last_cursor_sent = data
        if (
            self.data_streaming_server
            and hasattr(self.data_streaming_server, "clients")
            and self.data_streaming_server.clients
            and self.async_event_loop
            and self.async_event_loop.is_running()
        ):

            msg_str = json.dumps(data)
            msg_to_broadcast = f"cursor,{msg_str}"
            clients_ref = self.data_streaming_server.clients

            async def _broadcast_cursor_helper():
                """Bounded: cursor changes arrive at high rate, and a stalled
                client would otherwise accumulate one blocked coroutine each."""
                await _broadcast_to_clients(clients_ref, msg_to_broadcast, per_client_timeout=2.0)

            asyncio.run_coroutine_threadsafe(
                _broadcast_cursor_helper(), self.async_event_loop
            )
        else:
            data_logger.warning("Cannot broadcast cursor data: no clients connected or server not ready.")

    def send_system_action(self, action: str) -> None:
        """Broadcast a system action (e.g. ``command_error,<text>``) to clients."""
        if (
            self.data_streaming_server
            and getattr(self.data_streaming_server, "clients", None)
            and self.async_event_loop
            and self.async_event_loop.is_running()
        ):
            msg = "system," + json.dumps({"action": action})
            clients_ref = self.data_streaming_server.clients

            async def _broadcast_system_helper():
                await _broadcast_to_clients(clients_ref, msg, per_client_timeout=2.0)

            asyncio.run_coroutine_threadsafe(
                _broadcast_system_helper(), self.async_event_loop
            )

    async def stop_pipeline(self) -> None:
        """Stop all pipelines by reconciling displays against current state."""
        logger_app.info("Stopping pipelines (generic call)...")
        if self.data_streaming_server:
            await self.data_streaming_server.reconfigure_displays()
        self.pipeline_running = False
        logger_app.info("Pipelines stop signal processed.")

    stop_ws_pipeline = stop_pipeline

    def set_framerate(self, framerate: Union[int, float]) -> None:
        """Store the session default framerate; applies at the next pipeline (re)start."""
        self.framerate = int(framerate)
        logger_app.info(
            f"Framerate for {self.encoder} set to {self.framerate}. Restart pipeline if active."
        )


class DataStreamingServer(BaseStreamingService):
    """The websockets-transport streaming service.

    Owns the data WebSocket plane end to end: connection auth and roles,
    input/settings/control dispatch, per-display pixelflux captures with their
    per-client `_VideoRelay` fan-out, ACK-driven backpressure, the shared
    pcmflux audio broadcast (with its all-clients Opus+RED gate), microphone
    forwarding, stats collectors, and the display layout/reconfiguration
    engine (X11 xrandr monitors or Wayland compositor outputs).

    Concurrency contracts: `_reconfigure_lock` serializes reconfiguration and
    audio pipeline start/stop (with `_reconfigure_pending` coalescing requests
    that arrive during a hold); `_video_capture_lock` serializes per-display
    capture start/stop underneath it. Native capture objects are persistent
    per display so restarts keep the encoder backend warm.

    Attributes:
        clients: Every connected data socket, the audio and control fan-out set.
        display_clients: Display id to its owning socket and per-display state
            (geometry, tunables, frame-id/ACK/RTT bookkeeping, `video_active`,
            `backpressure_task`).
        display_layouts: Display id to its `{x, y, w, h}` rectangle in the
            union desktop, as last computed by a reconfigure pass.
        capture_instances: Display id to `{module, callback, settings}` for a
            running capture; callback and settings are retained so a resize
            re-targets the live module and a reconfigure can judge whether the
            running session is structurally compatible with the desired one.
        video_relay_groups: Display id to `{ws: _VideoRelay}`; the dict's
            presence marks the capture as delivering, and relays are created
            lazily by the fan-out.
        video_paused_clients: Sockets that sent STOP_VIDEO (hidden tab) —
            any shared client, not viewers alone — excluded from the primary
            video fan-out until their next START_VIDEO while capture, control,
            cursor and audio keep running.
        _persistent_capture_modules: One ScreenCapture per display id for the
            server's lifetime, so a restart does not re-initialise the backend
            (NVENC session, CUDA context, compositor handle).
        _wayland_ctl_module: Fallback pixelflux handle for output management
            when no primary module exists yet (any handle reaches the backend).
        _host_output_capacity: Host-capture mode only: how many outputs the
            host compositor exposes; None until a query answers.
        RECONNECT_GRACE_S: How long a disconnected display's entry (and its
            running capture) waits for the page to come back before teardown.
        BACKPRESSURE_QUEUE_SIZE: Audio queue depth (chunks dropped past it);
            video is bounded per client by the relay byte budget instead.
        audio_redundancy_by_ws: Per-socket Opus+RED capability from the
            `audioRedundancy` settings field.
        _active_audio_red_distance: RED distance the running audio pipeline
            was started with.
        _pcmflux_reported_failure: `(module id, reason)` of the failure already
            logged for the current audio run.
        _shared_stats_ws: Instance-wide dict the singleton collectors write and
            the per-connection stats senders read.
        _shared_network_stats: Instance-wide bandwidth/latency holder so every
            connection reads one consistent value.
        _gpu_available: Cached once behind `_gpu_probe_lock`, so concurrent
            first connections do not each spawn a blocking nvidia-smi probe.
        _last_keyframe_request: Display id to monotonic time of the last IDR
            request; `_last_keyframe_log` and `_keyframe_log_suppressed`
            throttle only the log line, never the request.
        _wm_swap: Swaps in a multi-monitor-capable window manager on X11.
    """

    def __init__(self, supervisor: Optional[Any] = None) -> None:
        super().__init__("websockets")
        self.data_ws: Optional[web.WebSocketResponse] = (
            None
        )
        self.clients: set[web.WebSocketResponse] = set()
        self.app = None
        self.cli_args = settings
        self.is_secure_mode = False
        self.input_handler = None
        self._tasks_to_run = []
        self.RECONNECT_DEBOUNCE_MS = 500
        self.RECONNECT_GRACE_S = 3.0
        self._display_teardown_tasks = set()
        self.MAX_RECENT_CLIENTS = 1000
        self.last_connection_times = OrderedDict()
        self._latest_client_render_fps = 0.0
        self._last_time_client_ok = 0.0
        self._client_acknowledged_frame_id = -1
        self._last_client_acknowledged_frame_id_update_time = 0.0
        self._previous_ack_id_for_stall_check = -1
        self._previous_sent_id_for_stall_check = -1
        self._sent_frames_log = deque()
        self.rc_mode = RateControlMode.CRF
        self.config_gate = asyncio.Event()
        self.shutdown_event = asyncio.Event()
        self._shutdown_called = False
        self.supervisor = supervisor
        
        def get_initial_value(setting_name: str):
            """Get the correct initial integer/bool from a processed setting."""
            processed_value = getattr(self.cli_args, setting_name)
            setting_def = next((s for s in SETTING_DEFINITIONS if s['name'] == setting_name), None)
            if not setting_def: return None

            if setting_def['type'] == 'range':
                min_val, max_val = processed_value
                return min_val if min_val == max_val else setting_def.get('meta', {}).get('default_value')
            elif setting_def['type'] == 'bool':
                return processed_value[0]
            return processed_value

        self._initial_video_crf = get_initial_value('video_crf')
        self.video_crf = self._initial_video_crf
        self._initial_video_fullcolor = get_initial_value('video_fullcolor')
        self.video_fullcolor = self._initial_video_fullcolor
        self._initial_video_streaming_mode = get_initial_value('video_streaming_mode')
        self.video_streaming_mode = self._initial_video_streaming_mode
        self.capture_cursor = False
        self._initial_jpeg_quality = get_initial_value('jpeg_quality')
        self.jpeg_quality = self._initial_jpeg_quality
        self._initial_paint_over_jpeg_quality = get_initial_value('paint_over_jpeg_quality')
        self.paint_over_jpeg_quality = self._initial_paint_over_jpeg_quality
        self._initial_video_paintover_crf = get_initial_value('video_paintover_crf')
        self.video_paintover_crf = self._initial_video_paintover_crf
        self._initial_video_paintover_burst_frames = get_initial_value('video_paintover_burst_frames')
        self.video_paintover_burst_frames = self._initial_video_paintover_burst_frames
        self._initial_use_cpu = get_initial_value('use_cpu')
        self.use_cpu = self._initial_use_cpu
        self._initial_use_paint_over_quality = get_initial_value('use_paint_over_quality')
        self.use_paint_over_quality = self._initial_use_paint_over_quality
        self._initial_video_bitrate = get_initial_value('video_bitrate')
        self.video_bitrate = self._initial_video_bitrate

        self._system_monitor_task_ws = None
        self._gpu_monitor_task_ws = None
        self._network_monitor_task_ws = None
        self._shared_stats_ws = {}
        self._shared_network_stats = {}
        self._gpu_available = None
        self._gpu_probe_lock = asyncio.Lock()
        self.uinput_mouse_socket = UINPUT_MOUSE_SOCKET
        self.js_socket_path = settings.js_socket_path
        self.enable_clipboard = settings.enable_clipboard
        self.enable_binary_clipboard = self.cli_args.enable_binary_clipboard[0]
        self.enable_cursors = ENABLE_CURSORS
        self.cursor_size = CURSOR_SIZE
        self.cursor_scale = 1.0
        self.cursor_debug = DEBUG_CURSORS
        self._last_adjustment_timestamp = 0.0
        self.client_settings_received = asyncio.Event()
        self._reconfigure_lock = asyncio.Lock()
        self._video_capture_lock = asyncio.Lock()
        self._is_reconfiguring = False
        self._reconfigure_pending = False
        self._bytes_sent_in_interval = 0
        self._last_bandwidth_calc_time = time.monotonic()
        self.last_start_video_request_times = {}
        self.last_viewer_keyframe_request_times = {}
        self.video_paused_clients = set()
        self._deferred_viewer_rejoins = {}
        self.allowed_desync_ms = BACKPRESSURE_ALLOWED_DESYNC_MS
        self.latency_threshold_for_adjustment_ms = BACKPRESSURE_LATENCY_THRESHOLD_MS
        self.backpressure_check_interval_s = BACKPRESSURE_CHECK_INTERVAL_S
        self.BACKPRESSURE_QUEUE_SIZE = getattr(settings, 'backpressure_queue_size', 120)
        self._last_client_frame_id_report_time = 0.0
        self.capture_loop = None

        self.display_clients = {}
        self.video_relay_groups = {}
        self.capture_instances = {}
        self.display_layouts = {}
        self._persistent_capture_modules = {}
        self._wayland_ctl_module = None
        self._host_output_capacity = None
        self._last_keyframe_request = {}
        self._last_keyframe_log = {}
        self._keyframe_log_suppressed = {}

        self.audio_device_name = self.cli_args.audio_device_name
        self.pcmflux_module = None
        self.is_pcmflux_capturing = False
        self.pcmflux_settings = None
        self._pcmflux_reported_failure = None
        self._pcmflux_last_restart = 0.0
        self.audio_redundancy_by_ws = {}
        self.audio_redundancy_enabled = bool(settings.audio_redundancy[0])
        self._active_audio_red_distance = 0
        # The vendored WebRTC RedOpusEncoder reads its depth from audio_config,
        # so one control (0 = plain Opus) drives both transports.
        _red_dist = getattr(settings, "audio_redundancy_distance", AUDIO_RED_DISTANCE)
        audio_config.set_red_distance(
            _red_dist if self.audio_redundancy_enabled else 0
        )
        self.pcmflux_callback = None
        self.pcmflux_audio_queue = None
        self.pcmflux_send_task = None
        self.pcmflux_capture_loop = None

        self._last_display_count = 0
        self._wm_swap = MultiMonitorWindowManager()

    def initialize(self) -> None:
        """Create the SelkiesStreamingApp and InputHandler and wire their callbacks.

        Must be called before run(). Also resolves secure vs legacy mode (a set
        master token closes the config gate until tokens are provisioned, and
        governs who holds input authority) and installs the WebRTC-dialect live
        verbs (`_arg_fps`, `vb`, `ab`, `_rc`, `_crf`) so both transports honor
        the same per-key tunables. With metrics enabled, the registry-global
        Prometheus gauges are fed server-side (ACK-derived client fps and
        smoothed RTT from the backpressure loop, GPU from the stats collector)
        and by the shared `_f,`/`_l,` verbs when a client reports directly.
        """
        self.is_secure_mode = bool(self.cli_args.master_token)
        if self.is_secure_mode:
            logger.info("Secure Mode ENABLED (SELKIES_MASTER_TOKEN is set).")
        else:
            logger.info("Legacy Mode ENABLED (SELKIES_MASTER_TOKEN is not set).")
            self.config_gate.set()

        global TARGET_FRAMERATE
        processed_framerate = settings.framerate
        min_fr, max_fr = processed_framerate
        if min_fr == max_fr:
            TARGET_FRAMERATE = min_fr
        else:
            fr_def = next((s for s in SETTING_DEFINITIONS if s['name'] == 'framerate'), None)
            TARGET_FRAMERATE = fr_def['meta']['default_value'] if fr_def else 60

        initial_encoder = settings.encoder

        if not settings.debug[0]:
            logging.getLogger("pulsectl_asyncio").setLevel(logging.WARNING)

        logger.info(f"Initializing DataStreamingServer with encoder: {initial_encoder}, Framerate: {TARGET_FRAMERATE}")

        event_loop = asyncio.get_running_loop()
        self.app = SelkiesStreamingApp(
            event_loop,
            framerate=TARGET_FRAMERATE,
            encoder=initial_encoder,
            mode="websockets",
        )
        self.app.server_enable_resize = ENABLE_RESIZE
        self.app.last_resize_success = True
        self.app.data_streaming_server = self
        logger.info(
            f"SelkiesStreamingApp initialized: encoder={self.app.encoder}, display={self.app.display_width}x{self.app.display_height}"
        )

        if settings.enable_rate_control[0]:
            self.rc_mode = RateControlMode(settings.rate_control_mode)

        # The normalized policy string (true/false/in/out); the input handler gates
        # directions off it.
        clipboard_mode = settings.enable_clipboard

        self.input_handler = InputHandler(
            self.app,
            self.uinput_mouse_socket,
            self.js_socket_path,
            clipboard_mode,
            str(settings.enable_binary_clipboard[0]).lower(),
            self.enable_cursors,
            self.cursor_size,
            1.0,
            self.cursor_debug,
            data_server_instance=self,
            is_wayland=IS_WAYLAND,
            wayland_socket_index=settings.wayland_socket_index,
            app_wayland_display=(settings.app_wayland_display
                                 or settings.wayland_host_display),
            uinput_gamepad=settings.uinput_gamepad,
        )

        self.input_handler.on_clipboard_read = self.app.send_ws_clipboard_data
        self.input_handler.on_set_fps = self._handle_opcode_fps
        self.input_handler.on_video_encoder_bit_rate = self._handle_opcode_video_bitrate
        self.input_handler.on_audio_encoder_bit_rate = self._handle_opcode_audio_bitrate
        self.input_handler.on_update_rate_control_mode = self._handle_opcode_rate_control
        self.input_handler.on_update_crf = self._handle_opcode_crf
        self.metrics = None
        if settings.enable_metrics_http[0]:
            self.metrics = Metrics()
            self.input_handler.on_client_fps = (
                lambda fps: self.metrics.set_fps(fps) if self.metrics else None
            )
            self.input_handler.on_client_latency = (
                lambda latency: self.metrics.set_latency(latency) if self.metrics else None
            )
        self.input_handler.on_mouse_pointer_visible = self.set_native_cursor_rendering

        if ENABLE_RESIZE:
            self.input_handler.on_resize = lambda res_str, display_id='primary': on_resize_handler(
                res_str, self.app, self, display_id
            )
        else:
            # Only the resolution is frozen: a DPI sync still scales the desktop,
            # applied in this transport's own message loop.
            self.input_handler.on_resize = lambda res_str, display_id='primary': logger.warning("Resize disabled.")
        logger.info("DataStreamingServer initialization complete.")

    async def set_native_cursor_rendering(self, enabled: bool) -> None:
        """Compose the cursor into the captured video (vs the client-drawn overlay).

        Applies to every display's capture. Reached both from the
        SET_NATIVE_CURSOR_RENDERING message and the shared input protocol's
        pointer-visibility toggle ("p,N"), which map to the same tunable.
        """
        if self.capture_cursor == enabled:
            data_logger.info(f"Native cursor rendering: value {enabled} is already set.")
            return
        self.capture_cursor = enabled
        if len(self.capture_instances) > 0:
            data_logger.info("Cursor rendering changed, triggering display reconfiguration.")
            await self.reconfigure_displays()

    def _opcode_display_module(self, display_id: str) -> Optional[Any]:
        """The display's live ScreenCapture module, or None if not capturing."""
        inst = self.capture_instances.get(display_id)
        return inst.get('module') if inst else None

    def _track_capture_settings(self, display_id: str, fresh: Optional[Any] = None,
                                **live_fields: Any) -> None:
        """Record what the display's running capture is actually configured with.

        Pass `fresh` after rebuilding the whole settings object, or individual
        fields after a targeted rate update.

        The tracked object is what _video_relay_budget sizes new relays from and
        what a layout-following reconfigure re-pushes to the module, so a live
        change that skipped it would be applied to the encoder and then silently
        reverted.
        """
        inst = self.capture_instances.get(display_id)
        if inst is None:
            return
        if fresh is not None:
            inst['settings'] = fresh
            return
        cs = inst.get('settings')
        if cs is None:
            return
        for name, value in live_fields.items():
            setattr(cs, name, value)

    async def _handle_opcode_fps(self, fps: Any, display_id: str = 'primary') -> None:
        """Live framerate for the shared '_arg_fps' verb (WebRTC-mode parity):
        sanitize against the server range, store, and live-update the display's
        capture; a stopped display applies the new rate at its next START_VIDEO."""
        sanitized = sanitize_client_setting("framerate", fps, self.cli_args, data_logger)
        if sanitized is None:
            return
        if display_id == 'primary':
            # Only the primary controller moves the session default later displays seed from.
            self.app.set_framerate(sanitized)
            data_logger.info(f"Session default framerate updated to {int(sanitized)} for new displays.")
        display_state = self.display_clients.get(display_id)
        if display_state is not None:
            display_state["framerate"] = sanitized
        module = self._opcode_display_module(display_id)
        if module is not None:
            try:
                module.update_framerate(float(sanitized))
                self._track_capture_settings(display_id, target_fps=float(sanitized))
                data_logger.info(f"Applied framerate live via '_arg_fps': {sanitized} fps for '{display_id}'")
            except Exception as e:
                data_logger.warning(f"Live framerate update failed for '{display_id}' ({e}).")

    async def _handle_opcode_video_bitrate(self, bitrate: Any, display_id: str = "primary") -> None:
        """Live video bitrate (kbps) for the 'vb' verb, sanitized exactly like
        the SETTINGS path so locked server ranges cannot be bypassed."""
        sanitized = sanitize_client_setting("video_bitrate", bitrate, self.cli_args, data_logger)
        if sanitized is None:
            return
        display_state = self.display_clients.get(display_id)
        if display_state is not None:
            display_state["video_bitrate"] = sanitized
        if display_id == 'primary':
            self._initial_video_bitrate = sanitized
            data_logger.info(f"Session default video_bitrate updated to {int(sanitized)} kbps for new displays.")
        module = self._opcode_display_module(display_id)
        if module is not None:
            kbps = int(round(float(sanitized)))
            try:
                module.update_video_bitrate(kbps)
                self._track_capture_settings(display_id, video_bitrate_kbps=kbps)
                data_logger.info(f"Applied video bitrate live via 'vb': {kbps} kbps for '{display_id}'")
            except Exception as e:
                data_logger.warning(f"Live bitrate update failed for '{display_id}' ({e}).")

    async def _handle_opcode_audio_bitrate(self, bitrate: Any) -> None:
        """Live Opus bitrate (bps) for the 'ab' verb; same live-retarget with
        restart fallback as the SETTINGS path."""
        sanitized = sanitize_client_setting("audio_bitrate", bitrate, self.cli_args, data_logger)
        if sanitized is None:
            return
        self.app.audio_bitrate = sanitized
        for display_state in self.display_clients.values():
            display_state["audio_bitrate"] = self.app.audio_bitrate
        if self.is_pcmflux_capturing and self.pcmflux_module:
            try:
                self.pcmflux_module.update_audio_bitrate(int(self.app.audio_bitrate))
                data_logger.info(f"Applied audio bitrate live: {self.app.audio_bitrate} bps")
            except Exception as e:
                data_logger.warning(f"Live audio bitrate update failed ({e}); restarting audio pipeline.")
                # Under the guard like every audio start/stop, and re-checked there:
                # a concurrent guarded op must not orphan a second AudioCapture.
                async with self._reconfigure_guard():
                    if self.is_pcmflux_capturing:
                        await self._stop_pcmflux_pipeline()
                        await self._start_pcmflux_pipeline()

    async def _handle_opcode_rate_control(self, mode: Any, display_id: str = 'primary') -> None:
        """Rate-control switch for the '_rc' verb: structural like the SETTINGS
        path (the encoder session must be rebuilt), honoring the server's
        enable_rate_control lock and a stopped display's start gating."""
        enable_rate_control, _ = self.cli_args.enable_rate_control
        if not enable_rate_control:
            data_logger.debug("Server has rate control disabled. Ignoring '_rc' change.")
            return
        # Resolved by value: a duplicate enum class from another module must not
        # leave a stray 'RateControlMode.CBR' repr.
        mode_str = (mode.value if hasattr(mode, "value") else str(mode)).split(".")[-1].lower()
        sanitized = sanitize_client_setting("rate_control_mode", mode_str, self.cli_args, data_logger)
        if sanitized not in ("cbr", "crf"):
            return
        display_state = self.display_clients.get(display_id)
        if display_state is None:
            return
        if display_state.get("rate_control_mode") == sanitized:
            return
        display_state["rate_control_mode"] = sanitized
        if display_id == 'primary':
            self.rc_mode = RateControlMode(sanitized)
            data_logger.info(f"Session default rate_control_mode updated to {sanitized} for new displays.")
        if not display_state.get('video_active', True):
            return
        layout = self.display_layouts.get(display_id)
        if layout is None:
            return
        restart_ok = False
        async with self._reconfigure_guard():
            if display_state.get('video_active', True):
                data_logger.info(f"Applied rate-control via '_rc': {sanitized} for '{display_id}'. Restarting its capture stream.")
                await self._stop_capture_for_display(display_id)
                await self._start_capture_for_display(
                    display_id=display_id,
                    width=layout['w'], height=layout['h'],
                    x_offset=layout['x'], y_offset=layout['y']
                )
                await self._start_backpressure_task_if_needed(display_id)
                self._schedule_idr_for_display(display_id)
                await self._broadcast_live_server_settings(display_id)
                if IS_WAYLAND:
                    await self._sync_wayland_realized_geometry(display_id)
                restart_ok = self._opcode_display_module(display_id) is not None
        if not restart_ok:
            data_logger.warning(f"Rate-control restart failed for '{display_id}'; falling back to full reconfiguration.")
            await self.reconfigure_displays()

    async def _handle_opcode_crf(self, crf: Any, display_id: str = 'primary') -> None:
        """Live CRF for the '_crf' verb; rides the tunables path like SETTINGS."""
        sanitized = sanitize_client_setting("video_crf", crf, self.cli_args, data_logger)
        if sanitized is None:
            return
        display_state = self.display_clients.get(display_id)
        if display_state is not None:
            display_state["video_crf"] = sanitized
        module = self._opcode_display_module(display_id)
        layout = self.display_layouts.get(display_id)
        if module is not None and layout is not None:
            try:
                fresh = self._get_capture_settings(
                    display_id, layout['w'], layout['h'], layout['x'], layout['y']
                )
                module.update_tunables(fresh)
                self._track_capture_settings(display_id, fresh=fresh)
                data_logger.info(f"Applied CRF live via '_crf': {sanitized} for '{display_id}'")
            except Exception as e:
                data_logger.warning(f"Live CRF update failed for '{display_id}' ({e}).")

    async def broadcast_display_config(self) -> None:
        """Broadcast the current display roster to all clients."""
        if not self.clients:
            return
        
        connected_displays = list(self.display_clients.keys())
        payload = {
            "type": "display_config_update",
            "displays": connected_displays
        }
        message_str = f"DISPLAY_CONFIG_UPDATE,{json.dumps(payload)}"
        
        data_logger.info(f"Broadcasting display config update: {message_str}")
        # Bounded: callers hold _reconfigure_lock.
        await _broadcast_to_clients(self.clients, message_str, per_client_timeout=2.0)

    def refresh_cursor_cache(self) -> Optional[dict]:
        """Refresh and return the cached cursor payload for late-joining clients."""
        if not self.app:
            return None

        cursor_data = None
        if self.input_handler and hasattr(self.input_handler, "get_current_cursor_data"):
            cursor_data = self.input_handler.get_current_cursor_data()

        if cursor_data is not None:
            self.app.last_cursor_sent = cursor_data

        return self.app.last_cursor_sent

    async def send_current_cursor(self, websocket: web.WebSocketResponse, raddr: Any) -> None:
        """Send the current cursor image to one client (used at connect/resume)."""
        cursor_data = self.refresh_cursor_cache()
        if not cursor_data:
            return

        data_logger.info(f"Sending current cursor to client {raddr}")
        try:
            msg_str = json.dumps(cursor_data)
            await websocket.send_str(f"cursor,{msg_str}")
        except Exception as e:
            data_logger.warning(f"Failed to send current cursor to client {raddr}: {e}")

    def _pcmflux_audio_callback(self, frame: Any) -> None:
        """Queue one encoded audio frame for broadcast.

        Called from pcmflux's capture thread with an AudioFrame, so it never
        touches asyncio state directly: the enqueue is scheduled onto the loop
        with call_soon_threadsafe, and loop/queue references are snapshotted
        because teardown can null them concurrently.
        """
        if self.is_pcmflux_capturing and frame is not None and self.pcmflux_audio_queue is not None:
            if len(frame) > 0:
                loop = self.pcmflux_capture_loop
                q = self.pcmflux_audio_queue
                if loop is None or q is None or loop.is_closed():
                    return
                # Zero-copy: the AudioFrame owns the buffer (header included) and
                # frees it once the queue item drops.
                item = {'data': memoryview(frame), 'owner': frame}
                def _do_put():
                    try:
                        q.put_nowait(item)
                    except asyncio.QueueFull:
                        pass
                # The loop can close between the check above and here; the
                # RuntimeError would surface in pcmflux's C thread.
                try:
                    loop.call_soon_threadsafe(_do_put)
                except RuntimeError:
                    pass
    
    def _check_pcmflux_health(self) -> None:
        """Restart the audio pipeline when its pcmflux worker died after start.

        pcmflux's start handshake answers while the worker is still starting,
        so a backend that gives up afterwards (its retry ladder spent, a
        mid-run reconnect budget exhausted) surfaces only through
        `last_error`; the broadcast loop asks here whenever its queue stays
        silent. The failure is logged once per run, and the restart — the same
        stop/start the audio toggles use — runs as its own task (the stop
        cancels the broadcast loop) no more often than the restart floor.
        Older pcmflux builds without the attribute never report one.
        """
        module = self.pcmflux_module
        if module is None or not self.is_pcmflux_capturing:
            return
        error = getattr(module, "last_error", None)
        if not error:
            return
        failure = (id(module), str(error))
        if self._pcmflux_reported_failure != failure:
            self._pcmflux_reported_failure = failure
            data_logger.error(f"pcmflux audio capture failed after start: {error}")
        now = time.monotonic()
        if now - self._pcmflux_last_restart < PCMFLUX_RESTART_FLOOR_SECONDS:
            return
        self._pcmflux_last_restart = now
        _spawn_background_task(self._restart_failed_pcmflux(module), name="pcmflux-restart")

    async def _restart_failed_pcmflux(self, failed_module: Any) -> None:
        """Stop and start the audio pipeline under the reconfigure guard, unless
        the failed capture was already replaced or stopped meanwhile."""
        async with self._reconfigure_guard():
            if self.pcmflux_module is not failed_module or not self.is_pcmflux_capturing:
                return
            data_logger.info("Restarting the audio pipeline after its capture failed.")
            await self._stop_pcmflux_pipeline()
            await self._start_pcmflux_pipeline()

    async def _pcmflux_send_audio_chunks(self) -> None:
        """Broadcast queued Opus audio chunks to the primary-viewer sockets.

        Runs as a long-lived task. Secondary-display sockets are excluded (they
        render video only; audio rides the primary connection), and sends are
        bounded so one stalled socket cannot freeze the shared stream. A queue
        that stays silent past the health interval is the cue to ask pcmflux
        whether the capture worker died (_check_pcmflux_health).
        """
        data_logger.info("pcmflux audio chunk broadcasting task started.")
        try:
            while True:
                try:
                    item = await asyncio.wait_for(
                        self.pcmflux_audio_queue.get(), timeout=PCMFLUX_HEALTH_INTERVAL_SECONDS)
                except asyncio.TimeoutError:
                    self._check_pcmflux_health()
                    continue

                secondary_websockets = {
                    client_info.get('ws')
                    for did, client_info in self.display_clients.items()
                    if did != 'primary' and client_info.get('ws')
                }
                primary_viewers = self.clients - secondary_websockets

                if not primary_viewers:
                    self.pcmflux_audio_queue.task_done()
                    continue

                # A zero-copy view over the AudioFrame, header included; sent as-is.
                message_to_send = item['data']
                self._bytes_sent_in_interval += len(message_to_send) * len(primary_viewers)
                dropped = await _broadcast_to_clients(
                    primary_viewers, message_to_send,
                    per_client_timeout=SHARED_STREAM_SEND_TIMEOUT_SECONDS,
                )
                if dropped:
                    # primary_viewers is a per-chunk temporary; the drop must reach the registry.
                    self.clients -= dropped

                self.pcmflux_audio_queue.task_done()
        except asyncio.CancelledError:
            data_logger.info("pcmflux audio chunk broadcasting task cancelled.")
        finally:
            data_logger.info("pcmflux audio chunk broadcasting task finished.")

    def _compute_audio_red_distance(self) -> int:
        """RED distance for the shared audio broadcast.

        WS is TCP, but the sender still drops frames under backpressure
        (pcmflux delivery ring drop-oldest, and this server's audio queue drops
        on overflow), and RED lets the client recover those within the
        redundancy distance.

        Returns:
            The configured distance only when the server allows it AND there is
            at least one client AND every connected client advertised
            audioRedundancy; otherwise 0 (plain frames, which decode
            everywhere) — a single non-capable or legacy (field-absent) client
            falls the whole stream back.
        """
        if not self.audio_redundancy_enabled:
            return 0
        if not self.clients:
            return 0
        for ws in self.clients:
            if not self.audio_redundancy_by_ws.get(ws):
                return 0
        return getattr(settings, "audio_redundancy_distance", AUDIO_RED_DISTANCE)

    async def _regate_audio_redundancy(self) -> None:
        """Recompute the RED gate for the shared audio stream and, if it flipped
        while capturing, restart the pipeline so the new red_distance takes
        effect. Callers hold the reconfigure guard (pipeline start/stop must be
        serialized against reconfigure_displays). A missing app means teardown
        (the last client leaving drops RED to 0, and the disconnect path stops
        the pipeline itself), so no restart is attempted then."""
        desired = self._compute_audio_red_distance()
        if desired == self._active_audio_red_distance:
            return
        if not self.is_pcmflux_capturing:
            return
        if not self.app:
            return
        data_logger.info(
            f"Audio RED gate changed ({self._active_audio_red_distance} -> {desired}); "
            "restarting audio pipeline."
        )
        await self._stop_pcmflux_pipeline()
        await self._start_pcmflux_pipeline()

    async def _start_pcmflux_pipeline(self) -> bool:
        """Start the pcmflux audio capture and the shared broadcast task.

        Resolves the RED distance for the current client set at start, so a
        gate change while running requires a restart (see
        _regate_audio_redundancy). Callers serialize via the reconfigure guard.

        Returns:
            True when capturing afterwards (already-running counts); False when
            audio is disabled, pcmflux is unavailable, or the start failed (a
            partial start is cleaned up).
        """
        if not settings.audio_enabled[0]:
            data_logger.info("Audio is disabled by server settings. Not starting pipeline.")
            return False
        if not PCMFLUX_AVAILABLE:
            data_logger.error("Cannot start audio pipeline: pcmflux library not available.")
            return False
        if self.is_pcmflux_capturing:
            data_logger.info("pcmflux audio pipeline is already capturing.")
            return True
        if not self.app:
            data_logger.error("Cannot start pcmflux: self.app (SelkiesStreamingApp instance) is not available.")
            return False
        
        self.pcmflux_capture_loop = self.capture_loop or asyncio.get_running_loop()
        if not self.pcmflux_capture_loop:
            data_logger.error("Cannot start pcmflux: asyncio event loop not found.")
            return False

        await ensure_capture_sink(self.audio_device_name)
        data_logger.info("Starting pcmflux audio pipeline...")
        try:
            capture_settings = AudioCaptureSettings()
            device_name_bytes = self.audio_device_name.encode('utf-8') if self.audio_device_name else None
            capture_settings.device_name = device_name_bytes
            capture_settings.sample_rate = 48000
            capture_settings.channels = self.app.audio_channels
            capture_settings.opus_bitrate = int(self.app.audio_bitrate)
            # The frame duration is the capture-side latency floor; PulseAudio
            # fragments are kept no larger than one frame.
            frame_ms = float(getattr(settings, 'audio_frame_duration_ms', '20') or 20)
            capture_settings.frame_duration_ms = frame_ms
            capture_settings.use_vbr = True
            capture_settings.use_silence_gate = False
            capture_settings.latency_ms = int(min(10, frame_ms))
            capture_settings.debug_logging = self.cli_args.debug[0]
            # pcmflux's native [0x01,0x00] header goes on the wire; no Python prepend/copy.
            capture_settings.omit_audio_header = False
            red_distance = self._compute_audio_red_distance()
            capture_settings.red_distance = red_distance
            self._active_audio_red_distance = red_distance
            self.pcmflux_settings = capture_settings

            data_logger.info(f"pcmflux settings: device='{self.audio_device_name}', "
                             f"bitrate={capture_settings.opus_bitrate}, channels={capture_settings.channels}, "
                             f"red_distance={red_distance}")

            self.pcmflux_callback = self._pcmflux_audio_callback
            self.pcmflux_module = AudioCapture()
            self.pcmflux_audio_queue = asyncio.Queue(maxsize=getattr(self, 'BACKPRESSURE_QUEUE_SIZE', 120))

            await self.pcmflux_capture_loop.run_in_executor(
                None, self.pcmflux_module.start_capture, self.pcmflux_settings, self.pcmflux_callback
            )

            # The start handshake answers before the worker is up; a run that
            # already died reports through last_error.
            state = getattr(self.pcmflux_module, "state", "running")
            error = getattr(self.pcmflux_module, "last_error", None)
            if state == "failed" or error:
                raise RuntimeError(f"capture {state}: {error}")
            self.is_pcmflux_capturing = True
            if self.pcmflux_send_task is None or self.pcmflux_send_task.done():
                self.pcmflux_send_task = asyncio.create_task(self._pcmflux_send_audio_chunks())

            data_logger.info(f"pcmflux audio capture state: {state}.")
            return True
        except Exception as e:
            data_logger.error(f"Failed to start pcmflux audio pipeline: {e}", exc_info=True)
            await self._stop_pcmflux_pipeline()
            return False

    async def _stop_pcmflux_pipeline(self) -> bool:
        """Stop the audio capture and broadcast task; idempotent.

        The capturing flag is cleared first so the capture-thread callback
        stops queueing chunks before the queue is dropped.
        """
        if not self.is_pcmflux_capturing and not self.pcmflux_module:
            return True
        
        data_logger.info("Stopping pcmflux audio pipeline...")
        self.is_pcmflux_capturing = False

        if self.pcmflux_send_task:
            self.pcmflux_send_task.cancel()
            try:
                await self.pcmflux_send_task
            except asyncio.CancelledError:
                pass
            self.pcmflux_send_task = None
        
        if self.pcmflux_module:
            try:
                if self.pcmflux_capture_loop:
                    await self.pcmflux_capture_loop.run_in_executor(
                        None, self.pcmflux_module.stop_capture
                    )
            except Exception as e:
                data_logger.error(f"Error during pcmflux stop_capture: {e}")
            finally:
                del self.pcmflux_module
                self.pcmflux_module = None
        
        self.pcmflux_audio_queue = None
        data_logger.info("pcmflux audio pipeline stopped.")
        return True

    async def shutdown_pipelines(self) -> None:
        """Stop all capture pipelines; the ONLY way pipelines are stopped programmatically.

        Deadlock-proof by construction: reconfigure_displays() self-acquires
        the reconfigure lock, so it runs first and outside the guard; the
        audio/backpressure teardown then runs under the guard (a
        disconnect/connect race could otherwise tear down audio a new client
        just started), and none of the awaited teardowns re-acquire the lock.
        """
        logger.info("Initiating unified pipeline shutdown...")
        await self.reconfigure_displays()
        async with self._reconfigure_guard():
            await self._stop_pcmflux_pipeline()
            if self.display_clients:
                stop_bp_tasks = [
                    self._ensure_backpressure_task_is_stopped(disp_id)
                    for disp_id in self.display_clients.keys()
                ]
                await asyncio.gather(*stop_bp_tasks, return_exceptions=True)
            if self.pcmflux_send_task and not self.pcmflux_send_task.done():
                self.pcmflux_send_task.cancel()
                try:
                    await self.pcmflux_send_task
                except asyncio.CancelledError:
                    pass
        logger.info("Unified pipeline shutdown complete.")

    async def _ensure_backpressure_task_is_stopped(self, display_id: str, notify: bool = True) -> bool:
        """Cancel and clean up the backpressure task for a specific display.

        Args:
            display_id: The display whose task to stop.
            notify: When True and a task was actually running, reset the frame
                ids and notify the client(s).

        Returns:
            Whether the pipeline-reset notification was sent, so callers that
            must guarantee a reset (capture stop) can send it exactly once
            themselves when no task was running.
        """
        display_state = self.display_clients.get(display_id)
        if not display_state:
            return False

        task_was_running = False
        task = display_state.get('backpressure_task')
        if task and not task.done():
            data_logger.debug(f"Ensuring frame backpressure task for '{display_id}' is stopped.")
            task.cancel()
            try:
                await task
                task_was_running = True
            except asyncio.CancelledError:
                data_logger.debug(f"Backpressure task for '{display_id}' cancelled successfully.")
                task_was_running = True
            except Exception as e_cancel:
                data_logger.error(f"Error awaiting cancellation for '{display_id}' backpressure task: {e_cancel}")
            display_state['backpressure_task'] = None

        display_state['backpressure_enabled'] = True

        if task_was_running and notify:
            data_logger.info(f"Backpressure task for '{display_id}' was stopped. Resetting its frame IDs.")
            await self._reset_frame_ids_and_notify(display_id)
            return True
        return False

    async def _reset_frame_ids_and_notify(self, display_id: str) -> None:
        """Reset a display's frame-id state and send PIPELINE_RESETTING.

        For the primary display the reset is broadcast to ALL clients (shared
        viewers decode the same stream); a secondary notifies only its own
        socket. Every id-keyed artifact (send stamps, RTT samples, the fps
        estimator's baseline) resets with the numbering, since a stale stamp
        matched by a NEW id of the same value manufactures a giant RTT sample
        that poisons the smoothed estimate. The notify sends are bounded: this
        runs under _video_capture_lock, so one stalled client must not wedge
        every display's start/stop; a timed-out socket is dropped and closed
        rather than reused, and the id state was already reset either way.
        """
        display_state = self.display_clients.get(display_id)
        if not display_state:
            return

        data_logger.info(f"Resetting frame IDs for display '{display_id}'.")
        display_state['last_sent_frame_id'] = 0
        display_state['has_sent_any_frame'] = False
        display_state['acknowledged_frame_id'] = -1
        sent_ts = display_state.get('sent_timestamps')
        if sent_ts is not None:
            sent_ts.clear()
        rtt_samples = display_state.get('rtt_samples')
        if rtt_samples is not None:
            rtt_samples.clear()
        display_state['smoothed_rtt'] = 0.0
        display_state.pop('_fps_sample_acked', None)
        display_state.pop('_fps_sample_time', None)
        
        message = f"PIPELINE_RESETTING {display_id}"
        
        if display_id == 'primary' and self.clients:
            data_logger.info(f"Broadcasting primary pipeline reset to all {len(self.clients)} clients: {message}")
            await _broadcast_to_clients(self.clients, message, per_client_timeout=2.0)
        else:
            websocket = display_state.get('ws')
            if websocket:
                try:
                    await asyncio.wait_for(websocket.send_str(message), timeout=2.0)
                except asyncio.TimeoutError:
                    data_logger.warning(f"Timed out notifying client for '{display_id}' of reset; dropping socket.")
                    self.clients.discard(websocket)
                    _close_abandoned_ws(websocket)
                except (ConnectionResetError, OSError, RuntimeError):
                    data_logger.warning(f"Could not notify client for '{display_id}' of reset; connection closed.")
        
        display_state['backpressure_enabled'] = True
        display_state['last_ack_update_time'] = time.monotonic()

    async def _start_backpressure_task_if_needed(self, display_id: str) -> None:
        """Start the backpressure task for a specific display if not already running.

        Backend-agnostic: frame ids, ACKs, and RTT flow identically on Wayland.
        A task restart is not a pipeline death, so it never notifies: the
        capture (re)start handles stream freshness, and the client reset
        belongs to the capture-stop path alone.
        """
        display_state = self.display_clients.get(display_id)
        if not display_state:
            data_logger.error(f"Cannot start backpressure task: display '{display_id}' not found.")
            return

        await self._ensure_backpressure_task_is_stopped(display_id, notify=False)

        task = display_state.get('backpressure_task')
        if not task or task.done():
            new_task = asyncio.create_task(self._run_frame_backpressure_logic(display_id))
            display_state['backpressure_task'] = new_task
            data_logger.info(f"New frame backpressure task started for display '{display_id}'.")
        else:
            data_logger.warning(f"Backpressure task for '{display_id}' was already running. Not starting a new one.")

    def _active_primary_consumers(self, exclude: Optional[web.WebSocketResponse] = None) -> set:
        """Sockets still consuming the primary broadcast.

        Every client except the secondary displays' owners, minus the paused
        ones. A paused viewer with a deferred rejoin pending counts as active —
        it keeps the capture alive under a waking viewer, while genuinely
        hidden ones let an all-tabs-hidden session stop encoding.

        Args:
            exclude: Drops the socket whose STOP_VIDEO is in flight.
        """
        secondary_ws = {
            info.get('ws')
            for did, info in self.display_clients.items()
            if did != 'primary'
        }
        waking = set(self._deferred_viewer_rejoins)
        consumers = self.clients - secondary_ws - (self.video_paused_clients - waking)
        if exclude is not None:
            consumers.discard(exclude)
        return consumers

    def _primary_reconnect_pending(self) -> bool:
        """Whether the primary display entry is being held for a socket that is
        already gone: the reconnect grace keeps the capture warm so a reloading
        page resumes on it instead of paying a full pipeline rebuild."""
        entry = self.display_clients.get('primary')
        return entry is not None and entry.get('ws') not in self.clients

    async def _stop_primary_if_unconsumed(self, reason: str) -> None:
        """Stop the primary capture once nothing decodes it — the last unpaused
        consumer hid its tab or disconnected. Hiding and disconnecting take the
        same verdict here; only a pending reconnect grace keeps the capture warm.
        A resume restarts it (START_VIDEO from the display owner, or the viewer
        path's capture ensure)."""
        if 'primary' not in self.capture_instances:
            return
        if self._active_primary_consumers() or self._primary_reconnect_pending():
            return
        data_logger.info(f"{reason} Stopping the 'primary' capture.")
        primary_entry = self.display_clients.get('primary')
        if primary_entry is not None:
            primary_entry['video_active'] = False
        await self._stop_capture_for_display('primary')

    def _cancel_deferred_rejoin(self, websocket: web.WebSocketResponse) -> None:
        """Drop a pending deferred rejoin for this socket. A STOP_VIDEO (or a
        disconnect) arriving after a throttled resume supersedes it: the rejoin
        would otherwise un-pause a tab that is hidden again, and the socket would
        keep counting as a live consumer until it fired."""
        rejoin_task = self._deferred_viewer_rejoins.pop(websocket, None)
        if rejoin_task is not None:
            rejoin_task.cancel()

    def _schedule_deferred_viewer_rejoin(self, websocket: web.WebSocketResponse, delay: float) -> None:
        """Rejoin a rapid-resume-throttled viewer once the resume floor passes.
        The client already believes it resumed, so a silent discard would leave
        the socket paused until its stall watchdog; at most one deferred rejoin
        is pending per socket."""
        if websocket in self._deferred_viewer_rejoins:
            return

        async def _rejoin():
            try:
                await asyncio.sleep(max(0.05, delay))
                if websocket not in self.clients or websocket not in self.video_paused_clients:
                    return
                self.last_start_video_request_times[websocket] = time.monotonic()
                self.video_paused_clients.discard(websocket)
                try:
                    await websocket.send_str("PIPELINE_RESETTING primary")
                except (ConnectionResetError, OSError, RuntimeError):
                    return
                # A controller tab-hide can tear the capture down mid-resume; an IDR
                # request is then a no-op and the reset decoder would wait forever.
                if 'primary' in self.capture_instances:
                    self._schedule_idr_for_display('primary')
                else:
                    await self._ensure_viewer_capture()
            finally:
                self._deferred_viewer_rejoins.pop(websocket, None)

        self._deferred_viewer_rejoins[websocket] = asyncio.create_task(_rejoin())

    def _video_relay_budget(self, display_id: str, fallback: int) -> int:
        """Skip-ahead byte budget for one client's video relay.

        VIDEO_RELAY_BUDGET_SECONDS of stream at the display's CURRENT
        configured bitrate (1 kbps = 125 B/s), floored so low-bitrate streams
        keep absorbing transport jitter. Read from the live capture settings
        at relay creation so in-place restarts (settings changes that reuse
        the capture callback) are honored.

        Args:
            display_id: The display whose configured bitrate sizes the budget.
            fallback: Covers the start window before capture_instances is
                registered.
        """
        inst = self.capture_instances.get(display_id)
        cs = inst.get('settings') if inst else None
        if cs is None:
            return fallback
        kbps = int(getattr(cs, 'video_bitrate_kbps', 0) or 0)
        return max(VIDEO_RELAY_BUDGET_MIN_BYTES,
                   int(kbps * 125 * VIDEO_RELAY_BUDGET_SECONDS))

    def _close_video_relays(self, display_id: str) -> None:
        """Stop every per-client video relay for this display. Graceful: each
        relay finishes its in-flight send and its task removes itself."""
        group = self.video_relay_groups.pop(display_id, None)
        if group:
            for relay in list(group.values()):
                relay.stop()

    def _schedule_idr_for_display(self, display_id: str) -> None:
        """Ask the encoder for a fresh keyframe on this display.

        request_idr_frame is non-blocking in pixelflux (an atomic flag or a
        channel send) and idempotent, so it runs inline on the event loop.
        """
        instance = self.capture_instances.get(display_id)
        module = instance.get('module') if instance else None
        if module:
            try:
                module.request_idr_frame()
            except Exception:
                pass

    def _second_screen_availability(self) -> tuple[bool, str]:
        """Whether this session can actually attach a second display.

        The admin flag gates first; past it, X11 and the self-composited
        Wayland backend mint another output on demand, while host capture is
        bounded by the host compositor's real output count (unknown until the
        first capture start establishes the host session).

        Returns:
            `(available, reason)`; the reason is empty when available.
        """
        enabled, _ = self.cli_args.second_screen
        if not enabled:
            return False, "Second screens are disabled on this server."
        if not IS_WAYLAND or not (self.cli_args.wayland_host_display or '').strip():
            return True, ""
        capacity = self._host_output_capacity
        if capacity is None or capacity < 0:
            return False, "The host compositor's outputs are not known yet."
        if capacity < 2:
            return False, "The host compositor has a single output, so a second display has nothing to capture."
        return True, ""

    async def _refresh_second_screen_capacity(self) -> bool:
        """Host-capture mode only: re-read how many outputs the host exposes.

        Returns:
            True when the answer changed, i.e. the second-screen availability
            that clients were told may have flipped.
        """
        if not IS_WAYLAND or not (self.cli_args.wayland_host_display or '').strip():
            return False
        module = self._wayland_control_module()
        if module is None or not hasattr(module, 'output_capacity'):
            return False
        try:
            capacity = int(await asyncio.to_thread(module.output_capacity))
        except Exception as e:
            data_logger.warning(f"Wayland output capacity query failed: {e}")
            return False
        changed = capacity != self._host_output_capacity
        self._host_output_capacity = capacity
        return changed

    def _settings_payload_for_display(self, display_id: str) -> dict:
        """Client settings snapshot as it applies to one display.

        build_client_settings_payload() publishes boot config, so the encoder is
        patched to the one this display is actually captured with (its stored
        pick, else the session default a fresh capture would use): clients key
        their wire-format demux off this value and drop every chunk of any other
        format. Also carried: `ws_max_message_bytes` (transport capacity, so
        the client sizes multipart chunks to the whole frame), `app_terminal`
        (the terminal the apps panel launches in, absent when none is
        installed), and `second_screen` and `ui_sidebar_show_apps` as effective
        availability — the admin flag and what the backend can actually do — so
        dashboards never offer a display the server would immediately kill, nor
        an apps panel whose every button would fail.
        """
        payload = build_client_settings_payload()
        live_encoder = (self.display_clients.get(display_id) or {}).get('encoder') or self.app.encoder
        if live_encoder and isinstance(payload.get('encoder'), dict):
            payload['encoder'] = dict(payload['encoder'])
            payload['encoder']['value'] = live_encoder
        payload['ws_max_message_bytes'] = {"value": WS_MAX_MESSAGE_BYTES}
        terminal = self.input_handler.app_terminal() if self.input_handler else None
        if terminal:
            payload['app_terminal'] = {"value": terminal}
        available, _ = self._second_screen_availability()
        entry = payload.get('second_screen')
        if isinstance(entry, dict) and entry.get('value') and not available:
            payload['second_screen'] = dict(entry, value=False)
        apps = payload.get('ui_sidebar_show_apps')
        if (isinstance(apps, dict) and apps.get('value')
                and self.input_handler and not self.input_handler.apps_available()):
            payload['ui_sidebar_show_apps'] = dict(apps, value=False)
        return payload

    async def _broadcast_live_server_settings(self, display_id: str) -> None:
        """Re-announce server settings after the given display changed its live encoder.

        The handshake payload holds boot config only, so every connected client —
        shared viewers included — must re-key its wire-format demux or it drops
        the new mode's chunks forever. Routed like broadcast_stream_resolution:
        each display's own socket gets its own encoder and every remaining socket
        gets the primary's, since shared viewers render the primary stream and one
        display's encoder applied on another page keys that page to a format its
        own stream never sends.
        """
        try:
            messages = {}

            def message_for(did):
                if did not in messages:
                    messages[did] = json.dumps({
                        "type": "server_settings",
                        "displayId": did,
                        "settings": self._settings_payload_for_display(did),
                    })
                return messages[did]

            per_socket = {}
            for did, client in self.display_clients.items():
                ws = client.get('ws')
                if ws is not None:
                    per_socket[ws] = message_for(did)
            primary_message = message_for('primary')
        except Exception as e:
            data_logger.warning(f"Could not build live server settings broadcast: {e}")
            return
        groups = {}
        for ws in self.clients:
            groups.setdefault(per_socket.get(ws) or primary_message, set()).add(ws)
        data_logger.info(
            f"Re-announcing live server settings after the '{display_id}' capture restart "
            f"to {len(self.clients)} client(s)."
        )
        for message_str, sockets in groups.items():
            # Bounded: runs under _reconfigure_lock; a frozen client is dropped, not waited on.
            dropped = await _broadcast_to_clients(sockets, message_str, per_client_timeout=2.0)
            # The fan-out ran over a computed set; mirror the drop into the registry.
            for ws in dropped:
                self.clients.discard(ws)

    def _set_backpressure_enabled(self, display_id: str, display_state: dict, enabled: bool) -> None:
        """Update the backpressure flag, requesting an IDR when it lifts.

        While backpressure was active, delta frames were dropped, so on the
        False->True (LIFTED) transition the client needs a keyframe to resync;
        otherwise it decodes deltas against a reference it never received.
        """
        prev_enabled = display_state.get('backpressure_enabled', True)
        display_state['backpressure_enabled'] = enabled
        if enabled and not prev_enabled:
            self._schedule_idr_for_display(display_id)

    async def _run_frame_backpressure_logic(self, display_id: str) -> None:
        """The core backpressure and latency calculation loop for a single display.

        Every BACKPRESSURE_CHECK_INTERVAL_S it compares the last sent and last
        acked frame ids (uint16 circular distance), sized by the client's
        measured consumption rate and forgiving capped propagation delay, and
        flips the display's backpressure flag: a stalled or lagging client
        stops receiving delta frames, and the lift requests an IDR resync.
        Also feeds the Prometheus fps/latency gauges for the primary display.
        """
        data_logger.info(f"Frame-based backpressure logic task started for display '{display_id}'.")
        display_state = None
        try:
            if self.client_settings_received:
                await self.client_settings_received.wait()
            data_logger.info(f"Client settings received, proceeding with backpressure loop for '{display_id}'.")

            while True:
                await asyncio.sleep(self.backpressure_check_interval_s)

                display_state = self.display_clients.get(display_id)
                if not display_state:
                    data_logger.warning(f"Backpressure task for '{display_id}' exiting: display no longer exists.")
                    break
                
                if display_id not in self.capture_instances:
                    if not display_state.get('backpressure_enabled', True):
                        data_logger.info(f"Backpressure LIFTED for '{display_id}' (video pipeline is not active).")
                    self._set_backpressure_enabled(display_id, display_state, True)
                    continue

                current_server_frame_id = display_state.get('last_sent_frame_id', 0)
                last_client_acked_frame_id = display_state.get('acknowledged_frame_id', -1)

                if last_client_acked_frame_id == -1:
                    if not display_state.get('backpressure_enabled', True):
                         data_logger.info(f"Backpressure LIFTED for '{display_id}' (client ACK is -1).")
                    self._set_backpressure_enabled(display_id, display_state, True)
                    display_state['last_ack_update_time'] = time.monotonic()
                    continue

                configured_fps = display_state.get('framerate', 60)
                if configured_fps <= 0:
                    configured_fps = 60
                client_fps = self._estimate_client_fps(
                    display_state, last_client_acked_frame_id, configured_fps, time.monotonic()
                )
                if display_id == 'primary' and getattr(self, 'metrics', None) is not None:
                    self.metrics.set_fps(client_fps)
                    self.metrics.set_latency(display_state.get('smoothed_rtt', 0.0))

                server_id, client_id = current_server_frame_id, last_client_acked_frame_id

                # Circular distance, so the suspicious-gap test is not tripped at the uint16 wrap.
                wrapped = (server_id - client_id) % (MAX_UINT16_FRAME_ID + 1)

                if wrapped > FRAME_ID_SUSPICIOUS_GAP_THRESHOLD:
                    self._set_backpressure_enabled(display_id, display_state, True)
                    display_state['last_ack_update_time'] = time.monotonic()
                    continue

                # Distinguish 'no frame sent yet' from the counter legitimately wrapping to 0.
                if not display_state.get('has_sent_any_frame', False):
                    continue

                frame_desync = wrapped
                allowed_desync_frames = (self.allowed_desync_ms / 1000.0) * client_fps
                # Capped: the RTT estimate rides the queue this loop bounds and must
                # not out-grow the trigger it feeds.
                current_rtt_ms = min(
                    display_state.get('smoothed_rtt', 0.0),
                    BACKPRESSURE_LATENCY_FORGIVENESS_MAX_MS,
                )
                latency_adjustment_frames = (current_rtt_ms / 1000.0) * client_fps if current_rtt_ms > self.latency_threshold_for_adjustment_ms else 0
                effective_desync_frames = frame_desync - latency_adjustment_frames

                time_since_last_ack = time.monotonic() - display_state.get('last_ack_update_time', time.monotonic())
                
                if time_since_last_ack > STALLED_CLIENT_TIMEOUT_SECONDS:
                    if display_state.get('backpressure_enabled', True):
                        data_logger.warning(f"Client stall for '{display_id}': No ACK in {time_since_last_ack:.1f}s. Forcing backpressure.")
                    self._set_backpressure_enabled(display_id, display_state, False)
                elif effective_desync_frames > allowed_desync_frames:
                    if display_state.get('backpressure_enabled', True):
                        data_logger.warning(f"Backpressure TRIGGERED for '{display_id}'. S:{server_id}, C:{client_id} (EffDesync:{effective_desync_frames:.1f}f > Allowed:{allowed_desync_frames:.1f}f).")
                    self._set_backpressure_enabled(display_id, display_state, False)
                else:
                    if not display_state.get('backpressure_enabled', True):
                        data_logger.info(f"Backpressure LIFTED for '{display_id}'. S:{server_id}, C:{client_id} (EffDesync:{effective_desync_frames:.1f}f <= Allowed:{allowed_desync_frames:.1f}f).")
                    self._set_backpressure_enabled(display_id, display_state, True)

        except asyncio.CancelledError:
            data_logger.info(f"Backpressure logic task for '{display_id}' cancelled.")
        finally:
            if display_state:
                display_state['backpressure_enabled'] = True
            data_logger.info(f"Backpressure logic task for '{display_id}' finished.")

    def _estimate_client_fps(self, display_state: dict, acked_id: int,
                             configured_fps: Union[int, float], now: float) -> float:
        """Measured client FPS from acked-frame cadence, clamped to `[1.0, configured_fps]`.

        Sizes the backpressure window so a client rendering below the
        configured rate gets a correctly scaled one. The estimate updates only
        from healthy (unthrottled) intervals with forward progress and holds
        otherwise: during active backpressure the ack rate reflects the
        throttling, not the client, and following it would latch low fps ->
        tighter window -> stuck backpressure. `now` is passed in so the
        estimator is deterministic to test.
        """
        prev_id = display_state.get('_fps_sample_acked')
        prev_t = display_state.get('_fps_sample_time')
        est = display_state.get('_measured_client_fps', float(configured_fps))
        sending = display_state.get('backpressure_enabled', True)
        if prev_id is None or prev_t is None:
            display_state['_fps_sample_acked'] = acked_id
            display_state['_fps_sample_time'] = now
            display_state['_measured_client_fps'] = float(configured_fps)
            return float(configured_fps)
        dt = now - prev_t
        if dt >= 0.25:
            # Circular forward distance over the uint16 ack space.
            delta = (acked_id - prev_id) % (MAX_UINT16_FRAME_ID + 1)
            display_state['_fps_sample_acked'] = acked_id
            display_state['_fps_sample_time'] = now
            if sending and 0 < delta <= FRAME_ID_SUSPICIOUS_GAP_THRESHOLD:
                inst = delta / dt
                est = 0.4 * inst + 0.6 * est
        est = max(1.0, min(est, float(configured_fps)))
        display_state['_measured_client_fps'] = est
        return est

    async def broadcast_stream_resolution(self) -> None:
        """Send each display's realized resolution to the socket rendering that
        display, and the primary's to every remaining socket (shared viewers
        render the primary stream). The payload names its display: applying the
        primary's resolution on a secondary page rescales that page's canvas and
        input mapping, so clicks land at primary-scaled coordinates."""
        per_socket = {}
        for did, client in self.display_clients.items():
            ws = client.get('ws')
            width, height = client.get('width', 0), client.get('height', 0)
            if ws is not None and width > 0 and height > 0:
                per_socket[ws] = json.dumps({
                    "type": "stream_resolution",
                    "width": width,
                    "height": height,
                    "displayId": did,
                })
        primary_client = self.display_clients.get('primary')
        primary_message = per_socket.get(primary_client.get('ws')) if primary_client else None
        if not per_socket and not primary_message:
            data_logger.warning("Cannot broadcast stream resolution: no display has realized dimensions.")
            return

        groups = {}
        for ws in self.clients:
            message_str = per_socket.get(ws) or primary_message
            if message_str:
                groups.setdefault(message_str, set()).add(ws)
        for message_str, sockets in groups.items():
            data_logger.info(f"Broadcasting stream resolution to {len(sockets)} client(s): {message_str}")
            # Bounded: runs under _reconfigure_lock; a frozen client is dropped, not waited on.
            dropped = await _broadcast_to_clients(sockets, message_str, per_client_timeout=2.0)
            # The fan-out ran over a computed set; mirror the drop into the registry.
            for ws in dropped:
                self.clients.discard(ws)

    async def _sync_wayland_realized_geometry(self, display_id: str, broadcast: bool = True) -> None:
        """Reconcile a display's state with the compositor's realized geometry.

        Reads back what the pixelflux compositor actually realized on this
        display's output (it may even-mask dimensions or keep the old mode on a
        GBM allocation failure), folds it into display state/layouts and
        broadcasts stream_resolution so the client reconciles its canvas and
        input mapping — the Wayland counterpart of the X11 reconfigure path's
        realized clamp + broadcast. The read also acts as a barrier: the
        compositor answers it only after any queued capture (re)start finished.

        Args:
            display_id: The display to reconcile.
            broadcast: False defers the fan-out to a caller that broadcasts
                once for every display (the reconfigure pass).
        """
        if not IS_WAYLAND:
            return
        inst = self.capture_instances.get(display_id)
        module = inst.get('module') if inst else None
        if module is None or not hasattr(module, 'get_realized_geometry'):
            return
        try:
            geom = await asyncio.to_thread(
                module.get_realized_geometry, wayland_output_id(display_id))
        except Exception as e:
            data_logger.warning(f"Wayland realized-geometry read failed for '{display_id}': {e}")
            return
        if geom is None:
            # A timeout is unknown geometry, not zero: the prior state stays.
            data_logger.warning(
                f"Wayland realized-geometry read for '{display_id}' timed out; state left unreconciled.")
            return
        w, h, scale = geom
        if w <= 0 or h <= 0:
            return
        client = self.display_clients.get(display_id)
        if client is not None:
            client['width'], client['height'] = w, h
            if scale > 0:
                client['scale'] = scale
        layout = getattr(self, 'display_layouts', {}).get(display_id)
        if layout is not None:
            layout['w'], layout['h'] = w, h
        if display_id == 'primary' and self.app is not None:
            self.app.display_width = w
            self.app.display_height = h
        data_logger.info(
            f"Wayland realized geometry for '{display_id}': {w}x{h} @ scale {scale}")
        if broadcast:
            await self.broadcast_stream_resolution()

    async def _current_primary_geometry(self) -> Optional[tuple]:
        """The primary display's size as the server realizes it right now.

        What a connection that may not resize the desktop streams: the
        primary's rectangle of an extended layout while a secondary display is
        connected (the X root then spans every display), else the root window
        (RandR) on X11 or compositor output 0 on Wayland — read live, so a
        desktop resized between connections (selkies-resize) is streamed at
        its new size rather than the last connection's.

        Returns:
            `(width, height)`, or None when the geometry cannot be read.
        """
        layout = getattr(self, 'display_layouts', {}).get('primary')
        if (layout and layout.get('w', 0) > 0 and layout.get('h', 0) > 0
                and any(did != 'primary' for did in self.display_clients)):
            return layout['w'], layout['h']
        if IS_WAYLAND:
            module = self._wayland_control_module()
            if module is None or not hasattr(module, 'get_realized_geometry'):
                return None
            try:
                geom = await asyncio.to_thread(
                    module.get_realized_geometry, wayland_output_id('primary'))
            except Exception as e:
                data_logger.warning(f"Wayland primary geometry read failed: {e}")
                return None
            if geom is None:
                data_logger.warning("Wayland primary geometry read timed out; size unknown.")
                return None
            w, h, _scale = geom
        else:
            w, h = await read_realized_root((0, 0))
        return (w, h) if w > 0 and h > 0 else None

    async def _apply_wayland_cursor_size(self, dpi: Union[int, float]) -> None:
        """Wayland counterpart of the X11 per-DPI cursor resize: the compositor
        reloads its theme cursor (composited overlay and named-cursor delivery
        both re-render) at the DPI-scaled size, live, no capture restart."""
        if CURSOR_SIZE is None:
            return
        module = self._wayland_control_module()
        setter = getattr(module, 'set_cursor_size', None) if module else None
        if setter is None:
            data_logger.warning("Wayland cursor resize unavailable (no set_cursor_size).")
            return
        size = cursor_size_for_dpi(dpi, CURSOR_SIZE)
        try:
            if await asyncio.to_thread(setter, size):
                data_logger.info(f"Wayland cursor size set to {size} (DPI {dpi}).")
            else:
                data_logger.warning(f"Wayland compositor refused cursor size {size}.")
        except Exception as e:
            data_logger.warning(f"Wayland cursor resize failed: {e}")

    def _update_cursor_cap(self, dpi: Union[int, float]) -> None:
        """Scale the remote-cursor delivery cap with a new DPI, on both backends.

        Tracks the DPI on the input handler and re-derives its cap from the
        DPI-scaled maximum sprite size (the python cursor monitor downscales
        shapes past it; the desktop cursor itself was just resized for the
        same DPI). Running captures take the cap live through pixelflux's
        tunables path, so the sprite pixelflux's own cursor monitor delivers
        follows without a capture restart; later (re)starts thread it through
        CaptureSettings. On Wayland the compositor's composited cursor follows
        the output scale on its own (set_cursor_size re-derives its theme
        pixel size on DPI changes).
        """
        ih = self.input_handler
        if ih is None:
            return
        try:
            ih.system_dpi = float(dpi)
            ih.cursor_size_cap = int(ih.max_cursor_size * float(dpi) / 96.0)
        except Exception as e:
            data_logger.debug(f"cursor cap update skipped: {e}")
            return
        updated = 0
        for display_id, inst in list(self.capture_instances.items()):
            module, cs = inst.get('module'), inst.get('settings')
            if module is None or cs is None:
                continue
            try:
                cs.cursor_size_cap = int(ih.cursor_size_cap)
                module.update_tunables(cs)
                updated += 1
            except Exception as e:
                data_logger.debug(f"Live cursor cap update skipped for '{display_id}': {e}")
        data_logger.info(
            f"Cursor size cap {ih.cursor_size_cap}px for DPI {dpi} "
            f"({updated} live capture(s) updated).")

    def _parse_settings_payload(self, payload_str: str) -> dict:
        """Parse a SETTINGS JSON payload into typed values (absent keys become None).

        `audioRedundancy` advertises Opus+RED de-RED capability for the audio
        path; `keyboardLayout` is an optional xkb layout hint (`de`, `ch(fr)`)
        that becomes the compositor's base layout on Wayland and is
        informational on X11.

        Raises:
            json.JSONDecodeError: When the payload is not valid JSON.
        """
        settings_data = json.loads(payload_str)
        parsed: dict[str, Any] = {}

        def get_int(k):
            v = settings_data.get(k)
            if v is None:
                return None
            # A float-yielding value ("29.7") truncates rather than failing the whole payload.
            return int(float(v))

        def get_number(k):
            v = settings_data.get(k)
            if v is None:
                return None
            value = float(v)
            return int(value) if value.is_integer() else value

        def get_bool(k):
            v = settings_data.get(k)
            return str(v).lower() == "true" if v is not None else None

        def get_str(k):
            v = settings_data.get(k)
            return str(v) if v is not None else None
        parsed["framerate"] = get_int("framerate")
        parsed["video_crf"] = get_int("video_crf")
        parsed["encoder"] = get_str("encoder")
        parsed["video_fullcolor"] = get_bool("video_fullcolor")
        parsed["video_streaming_mode"] = get_bool("video_streaming_mode")
        parsed["is_manual_resolution_mode"] = get_bool(
            "is_manual_resolution_mode"
        )
        parsed["manual_width"] = get_int(
            "manual_width"
        )
        parsed["manual_height"] = get_int(
            "manual_height"
        )
        parsed["audio_bitrate"] = get_int("audio_bitrate")
        parsed["initialClientWidth"] = get_int(
            "initialClientWidth"
        )
        parsed["initialClientHeight"] = get_int(
            "initialClientHeight"
        )
        parsed["jpeg_quality"] = get_int("jpeg_quality")
        parsed["paint_over_jpeg_quality"] = get_int(
            "paint_over_jpeg_quality"
        )
        parsed["use_cpu"] = get_bool("use_cpu")
        parsed["video_paintover_crf"] = get_int("video_paintover_crf")
        parsed["video_paintover_burst_frames"] = get_int("video_paintover_burst_frames")
        parsed["use_paint_over_quality"] = get_bool("use_paint_over_quality")
        parsed["scaling_dpi"] = get_int("scaling_dpi")
        parsed["enable_binary_clipboard"] = get_bool("enable_binary_clipboard")
        parsed["displayId"] = get_str("displayId") or "primary"
        parsed["displayPosition"] = get_str("displayPosition")
        parsed["rate_control_mode"] = get_str("rate_control_mode")
        parsed["video_bitrate"] = get_number("video_bitrate")
        parsed["force_aligned_resolution"] = get_bool("force_aligned_resolution")
        parsed["audioRedundancy"] = get_bool("audioRedundancy")
        parsed["keyboardLayout"] = get_str("keyboardLayout")
        data_logger.debug(f"Parsed client settings: {parsed}")
        return parsed

    async def _apply_client_settings(
        self,
        websocket_obj: web.WebSocketResponse,
        settings: dict,
        is_initial_settings: bool,
        client_role: str = "controller",
    ) -> None:
        """Sanitize and apply one client's SETTINGS payload to its display.

        Controller-only (a viewer's payload is ignored). Under
        _reconfigure_lock it resolves the target geometry (server-forced
        manual, client manual, the initial client size, or — with dynamic
        resizing disabled — the primary's current size), stores sanitized
        per-display tunables (primary updates also become session seeds for
        later displays), applies DPI/cursor/keyboard-layout side effects, and
        applies video changes live where possible — only structural switches
        (encoder, use_cpu, fullcolor, rate-control, Wayland capture scale)
        restart the display's capture. Dimensional or initial changes trigger a
        full reconfigure AFTER the lock is released (reconfigure_displays
        self-acquires it).

        Args:
            websocket_obj: The sending socket (used only for logging identity).
            settings: The parsed payload from _parse_settings_payload.
            is_initial_settings: True for the connection's first SETTINGS,
                which sizes the display and always reconfigures.
            client_role: "controller" or "viewer".
        """
        if client_role == "viewer":
            _viewer_raddr = client_permissions.get(websocket_obj, {}).get("remote_address", "unknown")
            data_logger.info(f"Ignoring SETTINGS payload from viewer {_viewer_raddr}.")
            return

        display_id = settings.get("displayId", "primary")
        if display_id not in self.display_clients:
            data_logger.error(f"Cannot apply settings for unknown display_id '{display_id}'")
            return
        display_state = self.display_clients[display_id]
        data_logger.info(
            f"Applying and sanitizing client settings for '{display_id}' (initial={is_initial_settings})"
        )
        def sanitize_value(name, client_value):
            """One-transport wrapper over the shared sanitizer (settings.py)."""
            return sanitize_client_setting(name, client_value, self.cli_args, data_logger)
        try:
            async with self._reconfigure_lock:
                old_settings = display_state.copy()
                old_display_width = display_state.get("width", 0)
                old_display_height = display_state.get("height", 0)
                old_position = display_state.get('position', 'right')
                new_position = settings.get("displayPosition", "right")
                target_w = None
                target_h = None
                keeps_current_geometry = False
                server_is_manual, _ = self.cli_args.is_manual_resolution_mode
                client_wants_manual = sanitize_value("is_manual_resolution_mode", settings.get("is_manual_resolution_mode"))
                if server_is_manual:
                    data_logger.info(f"Server override is active. Forcing manual resolution from server configuration for display '{display_id}'.")
                    try:
                        w_val = self.cli_args.manual_width
                        h_val = self.cli_args.manual_height
                        target_w = int(w_val[0] if isinstance(w_val, (list, tuple)) else w_val)
                        target_h = int(h_val[0] if isinstance(h_val, (list, tuple)) else h_val)
                        data_logger.info(f"Server override: Applying manual resolution {target_w}x{target_h}.")
                    except (ValueError, TypeError, IndexError) as e:
                        data_logger.error(f"Server override failed: Could not parse manual resolution from server config. Error: {e}. Falling back.")
                        target_w = 1024
                        target_h = 768
                elif client_wants_manual:
                    data_logger.info(f"Client has requested manual resolution mode for display '{display_id}'.")
                    target_w = sanitize_value("manual_width", settings.get("manual_width"))
                    target_h = sanitize_value("manual_height", settings.get("manual_height"))
                elif is_initial_settings and display_id == 'primary' and not getattr(
                        self.app, 'server_enable_resize', True):
                    # The page's window size is a resize like any later r, message;
                    # the reconfigure's stream_resolution broadcast tells the client to fit.
                    keeps_current_geometry = True
                    current = await self._current_primary_geometry()
                    if current is not None:
                        target_w, target_h = current
                    data_logger.info(
                        f"Primary initial size {settings.get('initialClientWidth')}x"
                        f"{settings.get('initialClientHeight')} ignored: dynamic resizing "
                        f"disabled; keeping the desktop at {current or 'its current size'}."
                    )
                elif is_initial_settings:
                    target_w = settings.get("initialClientWidth")
                    target_h = settings.get("initialClientHeight")
                    # Client dimensions must not reach xrandr --fb unbounded.
                    if isinstance(target_w, int):
                        target_w = max(1, min(target_w, 7680))
                    if isinstance(target_h, int):
                        target_h = max(1, min(target_h, 4320))
                if not isinstance(target_w, int) or target_w <= 0:
                    target_w = old_display_width if old_display_width > 0 else 1024
                if not isinstance(target_h, int) or target_h <= 0:
                    target_h = old_display_height if old_display_height > 0 else 768
                if target_w % 2 != 0: target_w -= 1
                if target_h % 2 != 0: target_h -= 1
                if settings.get("force_aligned_resolution") is not None:
                    display_state["force_aligned_resolution"] = sanitize_value(
                        "force_aligned_resolution", settings.get("force_aligned_resolution")
                    )
                if server_is_manual:
                    # A server-forced resolution follows the server's own toggle only.
                    apply_alignment = self.cli_args.force_aligned_resolution[0]
                elif keeps_current_geometry:
                    # Aligning the desktop's own size would resize it.
                    apply_alignment = False
                else:
                    apply_alignment = display_state["force_aligned_resolution"]
                if apply_alignment:
                    aligned_w, aligned_h = align_dims_16(target_w, target_h)
                    if aligned_w != target_w or aligned_h != target_h:
                        data_logger.info(
                            f"Aligning resolution for '{display_id}' from {target_w}x{target_h} to {aligned_w}x{aligned_h} (16-pixel alignment)."
                        )
                    target_w, target_h = aligned_w, aligned_h
                resolution_actually_changed = (target_w != old_display_width or target_h != old_display_height)
                position_actually_changed = (new_position != old_position)
                if resolution_actually_changed or position_actually_changed:
                    display_state['width'] = target_w
                    display_state['height'] = target_h
                    display_state['position'] = new_position
                    if display_id == 'primary':
                        self.app.display_width = target_w
                        self.app.display_height = target_h
                # Only keys the payload carries: sanitizing an absent (None) key
                # would reset the stored choice to the server default on every partial update.
                for key in ("encoder", "framerate", "video_crf", "video_fullcolor",
                            "video_streaming_mode", "jpeg_quality", "paint_over_jpeg_quality",
                            "use_paint_over_quality", "video_paintover_crf",
                            "video_paintover_burst_frames", "video_bitrate"):
                    if settings.get(key) is not None:
                        display_state[key] = sanitize_value(key, settings.get(key))
                if settings.get("use_cpu") is not None or settings.get("encoder") is not None:
                    # The request is stored apart from the effective flag, so a spell on a
                    # CPU-only encoder does not pin the display to software afterwards.
                    if settings.get("use_cpu") is not None:
                        display_state["use_cpu_requested"] = sanitize_value(
                            "use_cpu", settings.get("use_cpu"))
                    was_use_cpu = display_state["use_cpu"]
                    display_state["use_cpu"] = effective_use_cpu(
                        display_state["encoder"],
                        display_state.get("use_cpu_requested"),
                        self._initial_use_cpu)
                    if display_state["use_cpu"] != was_use_cpu:
                        data_logger.info(
                            f"Software encoding {'enabled' if display_state['use_cpu'] else 'disabled'} "
                            f"for encoder '{display_state['encoder']}'")
                if settings.get("audio_bitrate") is not None:
                    self.app.audio_bitrate = sanitize_value("audio_bitrate", settings.get("audio_bitrate"))
                    display_state["audio_bitrate"] = self.app.audio_bitrate
                enable_rate_control, _ = self.cli_args.enable_rate_control
                if enable_rate_control and settings.get("rate_control_mode") is not None:
                    display_state["rate_control_mode"] = sanitize_value("rate_control_mode", settings.get("rate_control_mode"))

                if display_id == 'primary':
                    session_seeds = {
                        'encoder': ('app_encoder',),
                        'framerate': ('app_framerate',),
                        'video_crf': ('video_crf', '_initial_video_crf'),
                        'video_bitrate': ('video_bitrate', '_initial_video_bitrate'),
                        'video_fullcolor': ('video_fullcolor', '_initial_video_fullcolor'),
                        'video_streaming_mode': ('video_streaming_mode', '_initial_video_streaming_mode'),
                        'jpeg_quality': ('jpeg_quality', '_initial_jpeg_quality'),
                        'paint_over_jpeg_quality': ('paint_over_jpeg_quality', '_initial_paint_over_jpeg_quality'),
                        'use_cpu': ('use_cpu', '_initial_use_cpu'),
                        'use_paint_over_quality': ('use_paint_over_quality', '_initial_use_paint_over_quality'),
                        'video_paintover_crf': ('video_paintover_crf', '_initial_video_paintover_crf'),
                        'video_paintover_burst_frames': ('video_paintover_burst_frames', '_initial_video_paintover_burst_frames'),
                    }
                    # The use_cpu seed is the client's request: seeding the effective flag
                    # would pin every later display to software after one CPU-only encoder.
                    seed_sources = {'use_cpu': 'use_cpu_requested'}
                    for key, targets in session_seeds.items():
                        if settings.get(key) is None:
                            continue
                        value = display_state.get(seed_sources.get(key, key))
                        if value is None:
                            continue
                        for attr in targets:
                            if attr == 'app_framerate':
                                self.app.set_framerate(int(value))
                            elif attr == 'app_encoder':
                                self.app.encoder = value
                                # Written through: transport services re-seed from the
                                # settings singleton on a mode switch.
                                app_settings.encoder = value
                                app_settings._encoder_client_set = True
                            else:
                                setattr(self, attr, value)
                        data_logger.info(f"Session default {key} updated to {value} for new displays.")
                    if enable_rate_control and settings.get('rate_control_mode') is not None:
                        self.rc_mode = RateControlMode(display_state['rate_control_mode'])
                        data_logger.info(
                            f"Session default rate_control_mode updated to {self.rc_mode.value} for new displays."
                        )

                if self.input_handler and settings.get("enable_binary_clipboard") is not None:
                    self.enable_binary_clipboard = sanitize_value("enable_binary_clipboard", settings.get("enable_binary_clipboard"))
                    await self.input_handler.update_binary_clipboard_setting(self.enable_binary_clipboard)
                if self.input_handler:
                    kb_layout = settings.get("keyboardLayout")
                    if kb_layout:
                        await self.input_handler.apply_client_keyboard_layout(kb_layout)
                if settings.get("scaling_dpi") is not None:
                    new_dpi = sanitize_value("scaling_dpi", settings.get("scaling_dpi"))
                else:
                    # Partial SETTINGS keeps the display's current DPI.
                    new_dpi = old_settings.get("scaling_dpi")
                if app_settings._overridden.get("scaling_dpi", False):
                    # An operator-set DPI (CLI/env) governs the desktop.
                    if new_dpi is not None and new_dpi != old_settings.get("scaling_dpi"):
                        data_logger.info("Ignoring client DPI sync: scaling_dpi is operator-overridden.")
                    new_dpi = old_settings.get("scaling_dpi")
                if new_dpi is not None and new_dpi != old_settings.get("scaling_dpi"):
                    data_logger.info(f"DPI changed from {old_settings.get('scaling_dpi')} to {new_dpi}. Applying system-level change.")
                    if not IS_WAYLAND:
                        await set_dpi(new_dpi)
                        if CURSOR_SIZE is not None:
                            new_cursor_size = cursor_size_for_dpi(new_dpi, CURSOR_SIZE)
                            await set_cursor_size(new_cursor_size)
                        self._update_cursor_cap(new_dpi)
                    if IS_WAYLAND:
                        # Only what the session compositor leaves becomes the capture
                        # scale, which the 'scale' restart trigger below reads.
                        display_state['scale'] = (
                            await self.input_handler.realize_wayland_dpi(
                                new_dpi, session_screen_index(display_id),
                                (display_state.get('width'), display_state.get('height')))
                            if self.input_handler else float(new_dpi) / 96.0)
                        self._update_cursor_cap(new_dpi)
                        await self._apply_wayland_cursor_size(new_dpi)

                display_state["scaling_dpi"] = new_dpi
                dimensional_change = resolution_actually_changed or position_actually_changed

                video_params_list = [
                    'encoder', 'framerate', 'video_crf', 'video_fullcolor', 'video_streaming_mode',
                    'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu', 'video_paintover_crf',
                    'video_paintover_burst_frames', 'use_paint_over_quality', 'rate_control_mode', 'video_bitrate'
                ]
                if IS_WAYLAND:
                    video_params_list.append('scale')

                video_params_changed = any(
                    display_state.get(key) != old_settings.get(key)
                    for key in video_params_list
                )
                audio_bitrate_changed = self.app.audio_bitrate != old_settings.get('audio_bitrate')
                if audio_bitrate_changed and self.is_pcmflux_capturing:
                    # Atomic in pcmflux; the pipeline keeps running.
                    try:
                        self.pcmflux_module.update_audio_bitrate(int(self.app.audio_bitrate))
                        data_logger.info(f"Applied audio bitrate live: {self.app.audio_bitrate} bps")
                    except Exception as e:
                        data_logger.warning(f"Live audio bitrate update failed ({e}); restarting audio pipeline.")
                        await self._stop_pcmflux_pipeline()
                        await self._start_pcmflux_pipeline()
                needs_fallback_reconfigure = False
                if not (is_initial_settings or dimensional_change) and video_params_changed:
                    restart_video_params = ['encoder', 'use_cpu', 'video_fullcolor', 'rate_control_mode']
                    if IS_WAYLAND:
                        # A capture scale change reconfigures the output, which the
                        # live-tunables path cannot apply.
                        restart_video_params.append('scale')
                    video_restart_needed = any(
                        display_state.get(k) != old_settings.get(k) for k in restart_video_params
                    )
                    module = self.capture_instances.get(display_id, {}).get('module')
                    if not video_restart_needed and module is not None:
                        data_logger.info(f"Applying video settings for '{display_id}' live (no restart).")
                        try:
                            layout = self.display_layouts.get(display_id) or {
                                'w': display_state.get('width', 0), 'h': display_state.get('height', 0),
                                'x': 0, 'y': 0,
                            }
                            fresh = self._get_capture_settings(
                                display_id, layout['w'], layout['h'], layout['x'], layout['y']
                            )
                            module.update_framerate(float(display_state.get('framerate') or self.app.framerate))
                            module.update_video_bitrate(int(round(float(display_state.get('video_bitrate') or 0))))
                            module.update_tunables(fresh)
                            self._track_capture_settings(display_id, fresh=fresh)
                        except Exception as e:
                            data_logger.warning(
                                f"Live video settings update failed for '{display_id}' ({e}); restarting its capture."
                            )
                            video_restart_needed = True
                    if video_restart_needed or module is None:
                        # A STOP_VIDEO'd display stays stopped; the next START_VIDEO
                        # builds its capture from the stored values.
                        if not display_state.get('video_active', True):
                            data_logger.info(
                                f"Video parameters changed for '{display_id}' while its stream "
                                "is stopped; deferring the restart to the next START_VIDEO."
                            )
                        elif display_id in self.display_layouts:
                            data_logger.info(
                                f"Video parameters changed for '{display_id}'. "
                                "Restarting its capture stream without reconfiguring displays."
                            )
                            layout = self.display_layouts[display_id]
                            await self._stop_capture_for_display(display_id)
                            await self._start_capture_for_display(
                                display_id=display_id,
                                width=layout['w'], height=layout['h'],
                                x_offset=layout['x'], y_offset=layout['y']
                            )
                            await self._start_backpressure_task_if_needed(display_id)
                            # A static screen must still repaint (the Wayland damage tracker
                            # stays warm across a stop/start) and clients must relearn the encoder.
                            self._schedule_idr_for_display(display_id)
                            await self._broadcast_live_server_settings(display_id)
                            if IS_WAYLAND:
                                await self._sync_wayland_realized_geometry(display_id)
                        else:
                            data_logger.warning(
                                f"Cannot restart capture for '{display_id}': no layout found. "
                                "Triggering full reconfiguration as a fallback."
                            )
                            needs_fallback_reconfigure = True
        except BaseException:
            # A raise skips the pending re-check below; a reconfigure coalesced
            # during the hold must not be stranded.
            if self._reconfigure_pending:
                await self.reconfigure_displays()
            raise
        if is_initial_settings or dimensional_change:
            data_logger.info(
                f"Initial setup or dimensional change detected for '{display_id}'. "
                "Performing full display reconfiguration."
            )
            await self.reconfigure_displays()
        elif needs_fallback_reconfigure or self._reconfigure_pending:
            await self.reconfigure_displays()
        if is_initial_settings and self.client_settings_received and not self.client_settings_received.is_set():
            self.client_settings_received.set()

    def _report_client_presence(self) -> None:
        """Tell the supervisor whether any client is connected (idle shutdown gate)."""
        if self.supervisor:
            self.supervisor.set_clients_present(bool(self.clients))

    def _holds_input_authority(self, websocket: web.WebSocketResponse,
                               perms: Optional[dict] = None) -> bool:
        """Whether this socket may drive keyboard/mouse input. `perms` supplies the
        entry for a socket already removed from client_permissions."""
        if perms is None:
            perms = client_permissions.get(websocket)
        return _perms_hold_input_authority(perms)

    async def ws_handler(
        self,
        websocket: web.WebSocketResponse,
        remote_address: tuple,
        token: str = "",
        query_role: str = "",
        query_slot: Optional[str] = None,
    ) -> None:
        """Run one data-WebSocket connection from handshake to cleanup.

        The connection's whole lifecycle lives here: auth (token in secure
        mode, query role/slot in legacy mode), reconnect rate-limiting, the
        handshake pushes (MODE, the secure-mode MK_ACCESS verdict, display
        roster, cursor, server settings), the message dispatch loop (SETTINGS,
        ACKs, video/audio start/stop, resize, DPI, mic PCM, and the shared
        input protocol), and the finally-block teardown: input-state release
        gated on departing input authority, RED re-gate, deferred display
        teardown behind the reconnect grace, and last-client
        pipeline/collector shutdown.

        Held keys, modifiers and pointer buttons are one global desktop state,
        so a departing socket force-releases them only if it could drive input
        AND its state is now unowned: the primary display's owner always
        qualifies, anything else only as the last input-capable client — a
        shared viewer or a second display's window leaving must not drop the
        keys or the in-progress drag of a client that is still connected. Keys
        the gate leaves alone belong to a connected client, and a crashed
        client's are healed by the input handler's heartbeat stale-sweep.

        Args:
            websocket: The prepared WebSocket.
            remote_address: `(ip, port)` of the peer.
            token: Auth token (secure mode only).
            query_role: Legacy-mode role request ("viewer" caps the role).
            query_slot: Legacy-mode gamepad slot request ("2".."4").
        """
        if self.is_secure_mode:
            await self.config_gate.wait()
            permissions = _lookup_session_token(token)
            if permissions is None:
                data_logger.warning(f"Rejecting connection from {remote_address}: Missing or invalid token.")
                await websocket.close(code=4001, message=b"Invalid authentication token")
                return

            client_permissions[websocket] = {
                "token": token,
                "role": permissions.get("role"),
                "slot": permissions.get("slot"),
                "remote_address": remote_address,
                "data_server": self,
            }
            data_logger.info(f"Client {remote_address} authenticated with token. Role: {permissions.get('role')}, Slot: {permissions.get('slot')}")
            auth_success_payload = json.dumps({
                "role": permissions.get("role"),
                "slot": permissions.get("slot"),
            })
            await websocket.send_str(f"AUTH_SUCCESS,{auth_success_payload}")
        else:
            role = "controller"
            slot = None
            if query_role == "viewer":
                role = "viewer"
            if query_slot is not None:
                try:
                    slot_num = int(query_slot)
                    if 2 <= slot_num <= 4:
                        slot = slot_num
                except (ValueError, TypeError):
                    pass
            # Enforcement, not link visibility: the WebRTC signaling server refuses
            # these outright, so a disabled shared/player page is refused here too.
            refusal = None
            if role == "viewer" and slot is None and not getattr(self.cli_args, 'enable_shared', (True,))[0]:
                refusal = "Strict shared clients are not enabled."
            elif slot is not None and not getattr(self.cli_args, f'enable_player{slot}', (True,))[0]:
                refusal = f"Player slot {slot} is not enabled."
            if refusal:
                data_logger.warning(f"Refusing legacy client {remote_address}: {refusal}")
                try:
                    await websocket.send_str(f"KILL {refusal}")
                    await websocket.close(code=1008, message=refusal.encode())
                except (ConnectionResetError, OSError, RuntimeError):
                    pass
                return
            client_permissions[websocket] = {"token": None, "role": role, "slot": slot, "remote_address": remote_address}
            data_logger.info(f"Legacy client {remote_address} connected. Role: {role}, Slot: {slot}")

        global TARGET_FRAMERATE
        current_time = time.monotonic()
        ip_address, _ = remote_address
        last_time = self.last_connection_times.get(ip_address)
        if last_time:
            elapsed_ms = (current_time - last_time) * 1000
            if elapsed_ms < self.RECONNECT_DEBOUNCE_MS:
                data_logger.warning(
                    f"Client {ip_address} reconnecting too quickly ({elapsed_ms:.1f}ms). Rejecting connection."
                )
                client_permissions.pop(websocket, None)
                await websocket.close(code=4029, message=b"Rate limited: reconnecting too quickly")
                return
        self.last_connection_times[ip_address] = current_time
        if len(self.last_connection_times) > self.MAX_RECENT_CLIENTS:
            self.last_connection_times.popitem(last=False)
        raddr = remote_address
        data_logger.info(f"Data WebSocket connected from {raddr}")
        self.clients.add(websocket)
        self._report_client_presence()
        self.data_ws = (
            websocket 
        )
        self.capture_loop = self.capture_loop or asyncio.get_running_loop()
        initial_settings_processed = False

        client_display_id = None

        try:
            await websocket.send_str(f"MODE {self.mode}")
        except (ConnectionResetError, OSError, RuntimeError):
            self.clients.discard(websocket)
            client_permissions.pop(websocket, None)
            if self.data_ws is websocket:
                self.data_ws = None
            return

        if self.is_secure_mode:
            # After MODE, which makes the page build the input context this verdict
            # applies to (a viewer holding the mk token attaches on 1, an outranked
            # controller detaches on 0).
            granted = _mk_access_verdict(client_permissions.get(websocket))
            try:
                await websocket.send_str("MK_ACCESS,1" if granted else "MK_ACCESS,0")
            except (ConnectionResetError, OSError, RuntimeError):
                pass

        # A page joining after a secondary attached must learn the roster now, not
        # at the next reconfigure.
        try:
            displays = list(self.display_clients.keys())
            await websocket.send_str(
                f"DISPLAY_CONFIG_UPDATE,{json.dumps({'type': 'display_config_update', 'displays': displays})}"
            )
        except (ConnectionResetError, OSError, RuntimeError):
            pass

        await self.send_current_cursor(websocket, raddr)

        # Which display this socket renders is only known from its first SETTINGS;
        # the primary's live encoder is what a viewer renders and a later display seeds from.
        await self._refresh_second_screen_capacity()
        server_settings_payload = {
            "type": "server_settings",
            "settings": self._settings_payload_for_display('primary'),
        }
        try:
            await websocket.send_str(json.dumps(server_settings_payload))
        except (ConnectionResetError, OSError, RuntimeError):
            self.clients.discard(websocket)
            if self.data_ws is websocket:
                self.data_ws = None
            return

        self._last_adjustment_time = self._last_time_client_ok = time.monotonic()
        self._active_pipeline_last_sent_frame_id = 0
        self._client_acknowledged_frame_id = -1
        self._last_client_acknowledged_frame_id_update_time = time.monotonic()
        self._previous_ack_id_for_stall_check = -1
        self._previous_sent_id_for_stall_check = -1
        self._last_client_stable_report_time = time.monotonic()
        # Per-connection sender over the instance-wide singleton collectors.
        stats_sender_task_ws = None
        # Blocks on client_settings_received, which may never be set: cancelled
        # with the connection.
        start_audio_task_ws = None

        mic_setup_done = False
        # Mic chunks arrive tens of times a second and each setup retry is a batch
        # of sound-server operations.
        mic_setup_retry_at = 0.0
        mic_disabled_sent = False
        mic_error = False
        webcam_disabled_sent = False
        pa_module_index = None
        # Only the loader of module-virtual-source unloads it; a reused source is
        # left for the other transport.
        pa_module_owned = False
        # Per connection, so module ownership and teardown follow the socket.
        mic_control: Optional[AudioControl] = None

        # pcmflux AudioPlayback: a GIL-released, non-blocking enqueue into a
        # stream on its own thread.
        mic_playback = None

        if not self.input_handler:
            logger.error(
                f"Data WS handler for {raddr}: Critical - self.input_handler (global) is not set. Input processing will fail."
            )

        gpu_id_for_stats = getattr(self.app, "gpu_id", GPU_ID_DEFAULT)
        # Stats must describe the GPU the pipeline captures/encodes on.
        dri_node_for_stats = str(getattr(self.cli_args, "encode_dri", "") or "")

        try:
            # This socket is in the audio fan-out before its SETTINGS (a viewer never
            # sends one): absent means not RED-capable, so re-gate a mid-capture join.
            if self.is_pcmflux_capturing:
                async with self._reconfigure_guard():
                    await self._regate_audio_redundancy()

            if self._gpu_available is None:
                async with self._gpu_probe_lock:
                    if self._gpu_available is None:
                        self._gpu_available = bool(
                            await asyncio.get_running_loop().run_in_executor(
                                None, gpu_stats.get_gpus
                            )
                        )

            if (
                self._system_monitor_task_ws is None
                or self._system_monitor_task_ws.done()
            ):
                self._system_monitor_task_ws = asyncio.create_task(
                    _collect_system_stats_ws(self._shared_stats_ws)
                )
            if self._gpu_available and (
                self._gpu_monitor_task_ws is None
                or self._gpu_monitor_task_ws.done()
            ):
                self._gpu_monitor_task_ws = asyncio.create_task(
                    _collect_gpu_stats_ws(
                        self._shared_stats_ws,
                        gpu_id=gpu_id_for_stats,
                        dri_node=dri_node_for_stats,
                        metrics=getattr(self, 'metrics', None),
                    )
                )
            stats_sender_task_ws = asyncio.create_task(
                _send_stats_periodically_ws(
                    websocket, self._shared_stats_ws, self
                )
            )
            if self._network_monitor_task_ws is None or self._network_monitor_task_ws.done():
                self._network_monitor_task_ws = asyncio.create_task(
                    _collect_network_stats_ws(self._shared_network_stats, self)
                )

            # An unlocked default-off microphone only sets the client toggle: a
            # runtime enable must not need a reconnect, so setup still runs.
            _mic_on, _mic_locked = settings.microphone_enabled
            if not settings.audio_enabled[0] or (not _mic_on and _mic_locked):
                data_logger.info("Audio/microphone disabled in settings. Skipping PulseAudio setup.")
            else:
                # The bounded connect keeps a missing sound server from stalling the
                # handshake before the client can claim its display.
                mic_control = AudioControl("selkies-mic-handler")
                if await mic_control.open():
                    data_logger.info(
                        f"Sound server control ready for the microphone ({mic_control.backend}).")
                else:
                    data_logger.error("Sound server control unavailable; microphone forwarding disabled.")
                    mic_error = True

            async for msg in websocket:
                # autoping is off: answer PING here, feed PONG to the uplink gauge.
                if msg.type == WSMsgType.PING:
                    await websocket.pong(msg.data)
                    continue
                if msg.type == WSMsgType.PONG:
                    note_pong(websocket, msg.data)
                    continue
                # A 0x05 frame is gzip-wrapped control text: inflated into a TEXT
                # message so the dispatch below (permission checks included) sees it as such.
                if (msg.type == WSMsgType.BINARY and msg.data
                        and msg.data[0] == 0x05):
                    try:
                        _text = inflate_gz_bounded(msg.data[1:])
                    except ValueError as e:
                        data_logger.warning(f"Dropping client gzip frame: {e}")
                        continue
                    except Exception:
                        data_logger.warning("Dropping undecodable client gzip frame.")
                        continue
                    msg = SimpleNamespace(type=WSMsgType.TEXT, data=_text)

                if msg.type == WSMsgType.BINARY:
                    if not msg.data:
                        continue
                    data = msg.data
                    msg_type = data[0]
                    # A webcam frame is handed over whole with an offset, never
                    # sliced into a copy.
                    payload = data[1:] if msg_type != WS_OPCODE_WEBCAM else b""
                    # Opcode 0x02 carries mic PCM.
                    if msg_type == 0x02:
                        # Mirrors the text-input gate, collab escape hatch included,
                        # so both transports gate the mixer alike.
                        mic_perms = client_permissions.get(websocket) or {}
                        mic_ok = mic_perms.get("role") != "viewer" or (
                            settings.enable_collab[0]
                            and active_mk_token is not None
                            and mic_perms.get("token") == active_mk_token
                        )
                        if not mic_ok:
                            if not mic_disabled_sent:
                                mic_disabled_sent = True
                                data_logger.info(
                                    f"Dropping microphone data from view-only client {remote_address}.")
                                try:
                                    await websocket.send_str("MICROPHONE_DISABLED")
                                except (ConnectionResetError, OSError, RuntimeError):
                                    pass
                            continue
                        # Only a locked-off microphone refuses data: an unlocked
                        # default-off is the client toggle, and data means it is on.
                        if mic_error or not settings.audio_enabled[0] or (
                                not settings.microphone_enabled[0] and settings.microphone_enabled[1]):
                            if not mic_disabled_sent:
                                mic_disabled_sent = True
                                data_logger.info("Microphone is disabled/errored. Sending MICROPHONE_DISABLED to client.")
                                try:
                                    await websocket.send_str("MICROPHONE_DISABLED")
                                except (ConnectionResetError, OSError, RuntimeError):
                                    pass
                            continue
                        if mic_control is None:
                            if len(payload) > 0:
                                data_logger.warning(
                                    "Sound server control not connected. Skipping microphone data."
                                )
                            continue

                        if not mic_setup_done:
                            if time.monotonic() < mic_setup_retry_at:
                                continue
                            data_logger.info(
                                "Performing PulseAudio/PipeWire virtual microphone setup check..."
                            )
                            pa_module_index, pa_module_owned = await mic_control.ensure_virtual_microphone(
                                self.audio_device_name, self.is_pcmflux_capturing
                            )
                            mic_setup_done = pa_module_index is not None
                            if not mic_setup_done:
                                mic_setup_retry_at = time.monotonic() + 5.0

                        if not mic_setup_done or not payload:
                            if not mic_setup_done and len(payload) > 0:
                                data_logger.warning(
                                    "Mic setup not complete, skipping mic data."
                                )
                            continue

                        if not PCMFLUX_PLAYBACK_AVAILABLE:
                            if not mic_error:
                                mic_error = True
                                data_logger.error(
                                    "pcmflux AudioPlayback unavailable; microphone forwarding disabled."
                                )
                            continue

                        # The stream is created once (blocking connect, offloaded); each
                        # chunk is a GIL-released, non-blocking write that drops oldest inside.
                        try:
                            if mic_playback is None:
                                _pb = AudioPlayback()
                                ps = AudioPlaybackSettings()
                                ps.device_name = b"input"
                                ps.sample_rate = 24000
                                ps.channels = 1
                                ps.latency_ms = 40
                                await asyncio.to_thread(_pb.start, ps)
                                # Published only after a successful start, so a failed
                                # one is retried on the next chunk.
                                mic_playback = _pb
                            mic_playback.write(payload)
                        except Exception as e_rust_mic:
                            data_logger.error(
                                f"Rust mic playback error: {e_rust_mic}", exc_info=False
                            )
                            # Torn down so the next chunk reopens a fresh stream.
                            if mic_playback is not None:
                                _dead = mic_playback
                                mic_playback = None
                                try:
                                    await asyncio.to_thread(_dead.stop)
                                except Exception:
                                    pass

                    elif msg_type == WS_OPCODE_WEBCAM:
                        # One encoded webcam frame, [opcode][codec][flags][payload]
                        # (webcam.py), gated like the microphone; the whole message
                        # goes to pixelflux with the payload offset, never copied.
                        cam_perms = client_permissions.get(websocket) or {}
                        cam_collab = (
                            settings.enable_collab[0]
                            and active_mk_token is not None
                            and cam_perms.get("token") == active_mk_token
                        )
                        if not webcam_uplink_allowed(cam_perms.get("role") == "viewer", cam_collab):
                            if not webcam_disabled_sent:
                                webcam_disabled_sent = True
                                try:
                                    await websocket.send_str(MSG_WEBCAM_DISABLED)
                                except (ConnectionResetError, OSError, RuntimeError):
                                    pass
                            continue
                        if len(data) <= WS_HEADER_LEN:
                            continue
                        cam = get_shared_webcam()
                        if cam.needs_ensure(data[1]) and await cam.ensure(data[1]) is None:
                            continue
                        cam_rotation, cam_flip = orientation_from_flags(data[2])
                        flags = cam.push(data, data[1], bool(data[2] & WS_FLAG_KEYFRAME),
                                         WS_HEADER_LEN, cam_rotation, cam_flip)
                        if cam.keyframe_wanted(flags):
                            try:
                                await websocket.send_str(MSG_WEBCAM_KEYFRAME)
                            except (ConnectionResetError, OSError, RuntimeError):
                                pass

                elif msg.type == WSMsgType.TEXT:
                    message = msg.data
                    if message == "_gz,1":
                        # Echoed so the client gzips its own large sends too.
                        websocket._ws_gz = True
                        try:
                            await websocket.send_str("_gz,1")
                        except Exception:
                            pass
                        continue
                    perms = client_permissions.get(websocket)
                    if perms and perms.get("role") == "viewer":
                        # Authority lists shared with the WebRTC gate: the collab extras
                        # need enable_collab on, even for a viewer holding the mk token.
                        allowed_viewer_prefixes: tuple[str, ...] = VIEWER_ALLOWED_PREFIXES
                        if settings.enable_collab[0] and active_mk_token and perms.get("token") == active_mk_token:
                            allowed_viewer_prefixes = allowed_viewer_prefixes + VIEWER_COLLAB_EXTRA_PREFIXES
                        if not message.startswith(allowed_viewer_prefixes):
                            # A viewer's blur/visibility noise (kr would clobber the
                            # controller's held modifiers) is refused silently: a warning
                            # per blur floods the log.
                            if not message.startswith(VIEWER_SILENT_DROP_PREFIXES):
                                data_logger.warning(f"DENIED unauthorized message from viewer {remote_address}: {message[:100]}...")
                            continue

                    if message.startswith("SETTINGS,"):
                        try:
                            _, payload_str = message.split(",", 1)
                            parsed_settings = self._parse_settings_payload(payload_str)
                            display_id = parsed_settings.get("displayId", "primary")
                            self.audio_redundancy_by_ws[websocket] = bool(
                                parsed_settings.get("audioRedundancy")
                            )

                            client_perms = client_permissions.get(websocket)
                            client_role = client_perms.get("role") if client_perms else "controller"

                            if client_role == 'viewer':
                                data_logger.info(f"Viewer client {remote_address} sent initial SETTINGS. Syncing with current stream state.")
                                if not initial_settings_processed:
                                    initial_settings_processed = True

                                if 'primary' not in self.capture_instances:
                                    await self._ensure_viewer_capture()

                                await self.broadcast_stream_resolution()

                                # Only the joining viewer is reset; the IDR opens its keyframe
                                # gate now, since an infinite GOP schedules none.
                                data_logger.info("Sending PIPELINE_RESETTING to the new viewer and requesting an IDR.")
                                try:
                                    await websocket.send_str("PIPELINE_RESETTING primary")
                                except (ConnectionResetError, OSError, RuntimeError):
                                    pass
                                self._schedule_idr_for_display('primary')

                                continue

                            if display_id != 'primary':
                                # The published setting can lag a host-side change; re-read first.
                                await self._refresh_second_screen_capacity()
                                available, reason = self._second_screen_availability()
                                if not available:
                                    data_logger.warning(
                                        f"Client from {remote_address} attempted to connect as secondary display ('{display_id}'), "
                                        f"but it is unavailable: {reason} Rejecting connection."
                                    )
                                    try:
                                        await websocket.send_str(f"KILL {reason}")
                                        await websocket.close(code=1008, message=b"Second screen unavailable")
                                    except (ConnectionResetError, OSError, RuntimeError):
                                        pass
                                    return
                            client_display_id = display_id
                            if display_id in ['primary', 'display2']:
                                existing_client_info = self.display_clients.get(display_id)
                                if existing_client_info:
                                    old_ws = existing_client_info.get('ws')
                                    if old_ws and old_ws is not websocket and not old_ws.closed:
                                        kill_reason = f"a new {display_id} client connected connection killed"
                                        old_ws_raddr = client_permissions.get(old_ws, {}).get("remote_address", "unknown")
                                        data_logger.warning(
                                            f"Killing old client for '{display_id}' at {old_ws_raddr}. Reason: {kill_reason}"
                                        )
                                        # Handed over before the close yields: the superseded
                                        # handler only tears down an entry its socket still owns,
                                        # and must not stop the capture being taken over.
                                        existing_client_info['ws'] = websocket
                                        try:
                                            # The superseded socket is the one most likely frozen;
                                            # unbounded, the takeover would hang here.
                                            await asyncio.wait_for(old_ws.send_str(f"KILL {kill_reason}"), timeout=2.0)
                                            await asyncio.wait_for(
                                                old_ws.close(code=1000, message=b"Superseded by new client"),
                                                timeout=2.0,
                                            )
                                        except asyncio.TimeoutError:
                                            _close_abandoned_ws(old_ws)
                                        except (ConnectionResetError, OSError, RuntimeError):
                                            data_logger.info(f"Old client for '{display_id}' was already disconnected.")
                                        except Exception as e:
                                            data_logger.error(f"Error while killing old client for '{display_id}': {e}")
                            if display_id != 'primary':
                                old_secondary_id = None
                                for existing_id, client_data in self.display_clients.items():
                                    if existing_id != 'primary' and client_data.get('ws') is not websocket:
                                        old_secondary_id = existing_id
                                        break
                                
                                if old_secondary_id:
                                    data_logger.warning(
                                        f"New secondary display '{display_id}' connected. "
                                        f"Deactivating old secondary '{old_secondary_id}'."
                                    )
                                    old_secondary_client = self.display_clients.get(old_secondary_id)
                                    if old_secondary_client:
                                        await self._stop_capture_for_display(old_secondary_id)
                                        old_secondary_client['video_active'] = False
                                        old_ws = old_secondary_client.get('ws')
                                        if old_ws:
                                            try:
                                                await asyncio.wait_for(old_ws.send_str("VIDEO_STOPPED"), timeout=2.0)
                                            except asyncio.TimeoutError:
                                                _close_abandoned_ws(old_ws)
                                            except (ConnectionResetError, OSError, RuntimeError):
                                                pass
                            if display_id not in self.display_clients:
                                data_logger.info(f"Registering new client for display: {display_id}")
                                self.display_clients[display_id] = {
                                    'ws': websocket, 
                                    'width': 0, 'height': 0, 'position': 'right',
                                    'acknowledged_frame_id': -1,
                                    'last_sent_frame_id': 0,
                                    'has_sent_any_frame': False,
                                    'sent_timestamps': OrderedDict(),
                                    'rtt_samples': deque(maxlen=RTT_SMOOTHING_SAMPLES),
                                    'smoothed_rtt': 0.0,
                                    'backpressure_enabled': True,
                                    'backpressure_task': None,
                                    'last_ack_update_time': time.monotonic(),
                                    'video_active': True,
                                    'encoder': self.app.encoder,
                                    'framerate': self.app.framerate,
                                    'video_crf': self._initial_video_crf,
                                    'video_fullcolor': self._initial_video_fullcolor,
                                    'video_streaming_mode': self._initial_video_streaming_mode,
                                    'jpeg_quality': self._initial_jpeg_quality,
                                    'paint_over_jpeg_quality': self._initial_paint_over_jpeg_quality,
                                    'use_cpu': effective_use_cpu(
                                        self.app.encoder, None, self._initial_use_cpu),
                                    'video_paintover_crf': self._initial_video_paintover_crf,
                                    'video_paintover_burst_frames': self._initial_video_paintover_burst_frames,
                                    'use_paint_over_quality': self._initial_use_paint_over_quality,
                                     'rate_control_mode': self.rc_mode.value,
                                     'video_bitrate': self._initial_video_bitrate,
                                     'force_aligned_resolution': self.cli_args.force_aligned_resolution[0],
                                     # The sanitizer's normalized form (an enum, so str): a
                                     # str-vs-int mismatch would read the first SETTINGS as
                                     # a DPI change.
                                     'scaling_dpi': str(int(float(getattr(app_settings, "scaling_dpi", "96") or 96))),
                                     # Replaced below on Wayland; the X11 capture has no scale.
                                     'scale': 1.0,
                                }
                                if IS_WAYLAND and self.input_handler is not None:
                                    # The ladder runs from the configured DPI before any client
                                    # sync, so the first capture starts at the intended scale.
                                    self.display_clients[display_id]['scale'] = (
                                        await self.input_handler.realize_wayland_dpi(
                                            getattr(app_settings, "scaling_dpi", "96") or 96,
                                            session_screen_index(display_id)))
                            else:
                                data_logger.info(f"Client is taking over existing display '{display_id}'. Updating state for new connection.")
                                display_state = self.display_clients[display_id]
                                display_state['ws'] = websocket
                                # Only a page's first SETTINGS reactivates video; a later one
                                # must not resurrect a stream stopped with STOP_VIDEO.
                                if not initial_settings_processed:
                                    display_state['video_active'] = True
                                display_state['acknowledged_frame_id'] = -1
                                display_state['last_ack_update_time'] = time.monotonic()
                                display_state['sent_timestamps'].clear()
                                display_state['rtt_samples'].clear()
                                display_state['smoothed_rtt'] = 0.0
                                # A warm takeover keeps the capture; no reconfigure runs when
                                # the dimensions are unchanged, so the reset and IDR go here.
                                try:
                                    await websocket.send_str(f"PIPELINE_RESETTING {display_id}")
                                except (ConnectionResetError, OSError, RuntimeError):
                                    pass
                                self._schedule_idr_for_display(display_id)
 
                            await self._apply_client_settings(
                                websocket,
                                parsed_settings,
                                not initial_settings_processed,
                                client_role
                            )
                            if not initial_settings_processed:
                                initial_settings_processed = True
                                data_logger.info("Initial client settings message processed by ws_handler.")
                                video_is_active = len(self.capture_instances) > 0
                                if not video_is_active:
                                    data_logger.error("FATAL: Initial reconfiguration completed, but video pipeline did not start.")
                                async with self._reconfigure_guard():
                                    audio_is_active = self.is_pcmflux_capturing
                                    if not audio_is_active and PCMFLUX_AVAILABLE and display_id == 'primary':
                                        data_logger.info("Initial setup: Primary client connected, audio not active, attempting start.")
                                        await self._start_pcmflux_pipeline()
                                    elif not PCMFLUX_AVAILABLE and not audio_is_active:
                                         data_logger.warning("Initial setup: Audio pipeline (server-to-client) cannot be started (pcmflux not available).")
                                    else:
                                        # A newly joined client may flip the shared RED gate.
                                        await self._regate_audio_redundancy()

                        except json.JSONDecodeError:
                            data_logger.error(f"SETTINGS JSON decode error: {message}")
                        except Exception as e_set:
                            data_logger.error(
                                f"Error processing SETTINGS: {e_set}", exc_info=True
                            )

                    elif message.startswith("CLIENT_FRAME_ACK"):
                        try:
                            parts = message.split(" ", 2)
                            acked_frame_id = -1
                            target_display_id = client_display_id
                            if not target_display_id:
                                continue
                            if len(parts) >= 2:
                                acked_frame_id = int(parts[-1])
                            else:
                                raise ValueError("ACK message has too few parts.")
                            # The -1 sentinel is server-internal: accepted from the wire it
                            # would disable backpressure and the stall detector.
                            if not (0 <= acked_frame_id <= MAX_UINT16_FRAME_ID):
                                raise ValueError("ACK frame id outside uint16 wire space.")

                            # Only the registered client acks the frames the relay stamped;
                            # a viewer's ack would throttle the controller against a stream
                            # it never got.
                            display_state = self.display_clients.get(target_display_id)
                            if display_state and display_state.get('ws') is websocket:
                                display_state['acknowledged_frame_id'] = acked_frame_id
                                display_state['last_ack_update_time'] = time.monotonic()
                                
                                sent_ts = display_state.get('sent_timestamps')
                                if sent_ts and acked_frame_id in sent_ts:
                                    send_time = sent_ts.pop(acked_frame_id)
                                    rtt_sample_ms = (time.monotonic() - send_time) * 1000.0
                                    # An id collision (uint16, reset on restarts) is not a
                                    # round trip.
                                    if 0 <= rtt_sample_ms <= RTT_SAMPLE_SANE_MAX_MS:
                                        rtt_samples = display_state.get('rtt_samples')
                                        if rtt_samples is not None:
                                            rtt_samples.append(rtt_sample_ms)
                                            if rtt_samples:
                                                display_state['smoothed_rtt'] = sum(rtt_samples) / len(rtt_samples)
                        except (IndexError, ValueError):
                            data_logger.warning(f"Malformed CLIENT_FRAME_ACK from {raddr}: {message}")

                    elif message == "START_VIDEO":
                        was_paused = websocket in self.video_paused_clients
                        perms = client_permissions.get(websocket)
                        if perms and perms.get("role") == "viewer":
                            # Monotonic: a clock jump must not wedge the floor or the throttle.
                            now = time.monotonic()
                            if was_paused:
                                # A resume (a real state change) bypasses the throttle but
                                # keeps the IDR floor; throttled, it stays paused this cycle.
                                last_req_time = self.last_start_video_request_times.get(websocket, 0)
                                if now - last_req_time < VIEWER_RESUME_MIN_INTERVAL_S:
                                    data_logger.warning(f"Throttled rapid resume from viewer {remote_address}; deferring its rejoin.")
                                    self._schedule_deferred_viewer_rejoin(
                                        websocket,
                                        VIEWER_RESUME_MIN_INTERVAL_S - (now - last_req_time),
                                    )
                                    continue
                                self.last_start_video_request_times[websocket] = now
                            else:
                                # Short: a stalled viewer re-requests via its watchdog and
                                # must not wait long for a resync.
                                last_req_time = self.last_start_video_request_times.get(websocket, 0)
                                if now - last_req_time < 5.0:
                                    data_logger.warning(f"Throttled START_VIDEO request from viewer {remote_address}. Ignoring.")
                                    continue
                                self.last_start_video_request_times[websocket] = now

                        # After the throttle decision, so a throttled resume stays paused;
                        # role-agnostic, or a paused collaborator never rejoins.
                        if was_paused:
                            self.video_paused_clients.discard(websocket)
                            data_logger.info(f"START_VIDEO from resuming client ({remote_address}): rejoining its video feed.")

                        display_entry = self.display_clients.get(client_display_id) if client_display_id else None
                        if display_entry is not None and display_entry.get('ws') is not websocket:
                            # A superseded connection (reload overlap) must not drive its
                            # successor's stream.
                            data_logger.info(f"Ignoring START_VIDEO for '{client_display_id}' from a superseded connection.")
                        elif display_entry is not None:
                            data_logger.info(f"Received START_VIDEO for '{client_display_id}'. Starting its stream.")
                            display_state = display_entry
                            # Landing on a capture that kept running continues mid-GOP: the
                            # socket needs the reset + IDR regardless of pause state.
                            resumed_onto_live_capture = (
                                client_display_id in self.capture_instances
                            )
                            # No await between this write and the capture start below: the
                            # flag and the lock-serialized start stay atomic vs a reconfigure.
                            display_state['video_active'] = True
                            if hasattr(self, 'display_layouts') and client_display_id in self.display_layouts:
                                layout = self.display_layouts[client_display_id]
                                data_logger.info(f"Found existing layout for '{client_display_id}'. Starting capture with: {layout}")
                                try:
                                    started = await self._start_capture_for_display(
                                        display_id=client_display_id,
                                        width=layout['w'], height=layout['h'],
                                        x_offset=layout['x'], y_offset=layout['y']
                                    )
                                    if not started:
                                        # A full reconfigure instead of a false VIDEO_STARTED.
                                        data_logger.warning(f"Capture start failed for '{client_display_id}'; reconfiguring.")
                                        await self.reconfigure_displays()
                                    else:
                                        await self._start_backpressure_task_if_needed(client_display_id)
                                        if resumed_onto_live_capture:
                                            try:
                                                await websocket.send_str(f"PIPELINE_RESETTING {client_display_id}")
                                            except (ConnectionResetError, OSError, RuntimeError):
                                                pass
                                        await websocket.send_str("VIDEO_STARTED")
                                        # The client clears its cursor canvas on tab hide.
                                        await self.send_current_cursor(websocket, remote_address)
                                except Exception as e:
                                    data_logger.error(f"Failed to restart individual stream for '{client_display_id}': {e}", exc_info=True)
                                    await self.reconfigure_displays()
                            else:
                                data_logger.warning(f"No layout found for '{client_display_id}' on START_VIDEO. Performing full reconfiguration.")
                                await self.reconfigure_displays()
                                # VIDEO_STARTED only for a live capture; the client would
                                # otherwise believe a stream runs with no pipeline behind it.
                                started = False
                                inst = self.capture_instances.get(client_display_id)
                                module = inst.get('module') if inst else None
                                if module is not None:
                                    try:
                                        started = bool(module.is_capturing)
                                    except Exception:
                                        started = False
                                if started:
                                    await websocket.send_str("VIDEO_STARTED")
                                    await self.send_current_cursor(websocket, remote_address)
                                else:
                                    data_logger.warning(f"Reconfigure did not start a live capture for '{client_display_id}'; not acking VIDEO_STARTED.")
                        else:
                            # A shared client needs a decode entry point (its own reset plus
                            # an IDR), not a pipeline rebuild, unless nothing runs.
                            if 'primary' in self.capture_instances:
                                data_logger.info(f"START_VIDEO from shared client ({remote_address}): sending reset + IDR.")
                                try:
                                    await websocket.send_str("PIPELINE_RESETTING primary")
                                except (ConnectionResetError, OSError, RuntimeError):
                                    pass
                                self._schedule_idr_for_display('primary')
                                # The client clears its cursor canvas on tab hide.
                                await self.send_current_cursor(websocket, remote_address)
                            else:
                                data_logger.info(f"START_VIDEO from shared client ({remote_address}) with no active capture. Starting primary capture.")
                                if await self._ensure_viewer_capture():
                                    try:
                                        await websocket.send_str("PIPELINE_RESETTING primary")
                                    except (ConnectionResetError, OSError, RuntimeError):
                                        pass
                                    self._schedule_idr_for_display('primary')
                                    await self.send_current_cursor(websocket, remote_address)
                                else:
                                    # A no-op with zero display clients.
                                    await self.reconfigure_displays()

                    elif message == "STOP_VIDEO":
                        stop_entry = self.display_clients.get(client_display_id) if client_display_id else None
                        if stop_entry is not None and stop_entry.get('ws') is not websocket:
                            # A dying page's tab-hide STOP_VIDEO can arrive after the reloaded
                            # page already owns the display.
                            data_logger.info(f"Ignoring STOP_VIDEO for '{client_display_id}' from a superseded connection.")
                            try:
                                await websocket.send_str("VIDEO_STOPPED")
                            except (ConnectionResetError, OSError, RuntimeError):
                                pass
                        elif stop_entry is not None:
                            self._cancel_deferred_rejoin(websocket)
                            # The controller hiding its tab must not stop an encoder shared
                            # viewers still consume; only its own socket pauses then.
                            remaining_viewers = (
                                self._active_primary_consumers(exclude=websocket)
                                if client_display_id == 'primary' else set()
                            )
                            if remaining_viewers:
                                data_logger.info(
                                    f"STOP_VIDEO for 'primary' with {len(remaining_viewers)} shared "
                                    "viewer(s) attached: pausing the controller, keeping the capture."
                                )
                                self.video_paused_clients.add(websocket)
                            else:
                                data_logger.info(f"Received STOP_VIDEO for '{client_display_id}'. Stopping stream.")
                                stop_entry['video_active'] = False
                                await self._stop_capture_for_display(client_display_id)
                            try:
                                await websocket.send_str("VIDEO_STOPPED")
                            except (ConnectionResetError, OSError, RuntimeError):
                                pass
                        else:
                            self._cancel_deferred_rejoin(websocket)
                            self.video_paused_clients.add(websocket)
                            data_logger.info(f"STOP_VIDEO from shared client ({remote_address}): pausing its video feed.")
                            await self._stop_primary_if_unconsumed(
                                "Last unpaused consumer of 'primary' hid its tab."
                            )
                            try:
                                await websocket.send_str("VIDEO_STOPPED")
                            except (ConnectionResetError, OSError, RuntimeError):
                                pass

                    elif message == "REQUEST_KEYFRAME":
                        # Viewers get a stricter per-socket throttle: any number of them
                        # share one stream.
                        perms = client_permissions.get(websocket)
                        if perms and perms.get("role") == "viewer":
                            now = time.monotonic()
                            last = self.last_viewer_keyframe_request_times.get(websocket, 0.0)
                            if now - last < 1.0:
                                continue
                            self.last_viewer_keyframe_request_times[websocket] = now
                        target_display_id = client_display_id or 'primary'
                        instance = self.capture_instances.get(target_display_id)
                        module = instance.get('module') if instance else None
                        if module:
                            now = time.monotonic()
                            if now - self._last_keyframe_request.get(target_display_id, 0.0) >= 0.25:
                                self._last_keyframe_request[target_display_id] = now
                                # The log line is throttled harder than the request: a
                                # decode-resync loop would fill the journal at 4 lines/s.
                                if now - self._last_keyframe_log.get(target_display_id, 0.0) >= 5.0:
                                    suppressed = self._keyframe_log_suppressed.get(target_display_id, 0)
                                    suffix = f" (+{suppressed} further requests suppressed)" if suppressed else ""
                                    self._keyframe_log_suppressed[target_display_id] = 0
                                    self._last_keyframe_log[target_display_id] = now
                                    data_logger.info(f"Keyframe requested by {remote_address} for '{target_display_id}'.{suffix}")
                                else:
                                    self._keyframe_log_suppressed[target_display_id] = \
                                        self._keyframe_log_suppressed.get(target_display_id, 0) + 1
                                # Non-blocking in pixelflux (atomic flag / channel send).
                                module.request_idr_frame()

                    elif message == "START_AUDIO":
                        async def _handle_start_audio_request():
                            await self.client_settings_received.wait()
                            async with self._reconfigure_guard():
                                data_logger.info(
                                    "Received START_AUDIO command from client for server-to-client audio."
                                )
                                if not settings.audio_enabled[0]:
                                    data_logger.info("START_AUDIO: Audio is disabled by server settings. Sending AUDIO_DISABLED.")
                                    # Its own task: a departed requester must end it quietly.
                                    try:
                                        await websocket.send_str("AUDIO_DISABLED")
                                    except (ConnectionResetError, OSError, RuntimeError):
                                        pass
                                    return
                                if PCMFLUX_AVAILABLE:
                                    started = False
                                    if not self.is_pcmflux_capturing:
                                        data_logger.info("START_AUDIO: Starting pcmflux audio pipeline.")
                                        started = await self._start_pcmflux_pipeline()
                                    else:
                                        started = True
                                        data_logger.info("START_AUDIO: pcmflux audio pipeline already active.")
                                    if started:
                                        await _broadcast_to_clients(self.clients, "AUDIO_STARTED", per_client_timeout=2.0)
                                else:
                                    data_logger.warning("START_AUDIO: Cannot start server-to-client audio (pcmflux not available).")
                                    try:
                                        await websocket.send_str("AUDIO_DISABLED")
                                    except (ConnectionResetError, OSError, RuntimeError):
                                        pass
                        # A re-request supersedes the pending one; disconnect cancels it.
                        if start_audio_task_ws and not start_audio_task_ws.done():
                            start_audio_task_ws.cancel()
                        start_audio_task_ws = asyncio.create_task(_handle_start_audio_request())

                    elif message == "STOP_AUDIO":
                        async with self._reconfigure_guard():
                            data_logger.info("Received STOP_AUDIO")
                            if self.is_pcmflux_capturing:
                                await self._stop_pcmflux_pipeline()
                            if self.clients:
                                await _broadcast_to_clients(self.clients, "AUDIO_STOPPED", per_client_timeout=2.0)

                    elif message.startswith("r,"):
                        # Bounded: this loop is what would process the initial SETTINGS, so
                        # an unbounded wait deadlocks when a client sends r, first.
                        try:
                            await asyncio.wait_for(self.client_settings_received.wait(), timeout=15.0)
                        except asyncio.TimeoutError:
                            data_logger.warning("Ignoring resize request received before initial SETTINGS.")
                            continue
                        raddr = remote_address
                        
                        parts = message.split(',')
                        if len(parts) != 3:
                            data_logger.warning(f"Malformed resize request from {raddr}: {message}")
                            continue
                        
                        target_res_str = parts[1]
                        display_id = parts[2]

                        client_info = self.display_clients.get(display_id)
                        if not client_info:
                            data_logger.warning(f"Resize request for unknown display_id '{display_id}' from {raddr}. Ignoring.")
                            continue
                        
                        current_res_str = f"{client_info.get('width', 0)}x{client_info.get('height', 0)}"

                        if target_res_str == current_res_str:
                            data_logger.info(f"Received redundant resize request for {display_id} ({target_res_str}). No action taken.")
                            continue
                        data_logger.info(f"Received resize request for {display_id}: {target_res_str} from {raddr}")

                        await on_resize_handler(target_res_str, self.app, self, display_id)

                    elif message.startswith("SET_NATIVE_CURSOR_RENDERING,"):
                        try:
                            await asyncio.wait_for(self.client_settings_received.wait(), timeout=15.0)
                        except asyncio.TimeoutError:
                            data_logger.warning("Ignoring SET_NATIVE_CURSOR_RENDERING before initial SETTINGS.")
                            continue
                        try:
                            new_capture_cursor_str = message.split(",")[1].strip().lower()
                            new_capture_cursor = new_capture_cursor_str in ("1", "true")
                            data_logger.info(f"Received SET_NATIVE_CURSOR_RENDERING: {new_capture_cursor}")
                            await self.set_native_cursor_rendering(new_capture_cursor)
                        except (IndexError, ValueError) as e:
                            data_logger.warning(f"Malformed SET_NATIVE_CURSOR_RENDERING message: {message}, error: {e}")

                    elif message.startswith("s,"):
                        try:
                            await asyncio.wait_for(self.client_settings_received.wait(), timeout=15.0)
                        except asyncio.TimeoutError:
                            data_logger.warning("Ignoring DPI sync received before initial SETTINGS.")
                            continue
                        try:
                            dpi_value_str = message.split(",")[1]
                            # Fractional DPI is legal on the shared verb; the desktop
                            # property is integral and bounded (see _scaling_dpi_bounds).
                            dpi_value = min(SCALING_DPI_MAX,
                                            max(SCALING_DPI_MIN, int(round(float(dpi_value_str)))))
                            if app_settings._overridden.get("scaling_dpi", False):
                                # An operator-set DPI (CLI/env) governs the desktop.
                                data_logger.info("Ignoring client DPI sync: scaling_dpi is operator-overridden.")
                                continue

                            data_logger.info(f"Received DPI setting from client: {dpi_value}")

                            if not IS_WAYLAND:
                                if await set_dpi(dpi_value):
                                    data_logger.info(f"Successfully set DPI to {dpi_value}")
                                else:
                                    data_logger.error(f"Failed to set DPI to {dpi_value}")
                                self._update_cursor_cap(dpi_value)

                            if IS_WAYLAND and client_display_id:
                                # A nested session scales its own screen (capture stays 1.0);
                                # a plain session scales the capture output, re-read on restart.
                                entry = self.display_clients.get(client_display_id)
                                size = ((entry or {}).get('width'), (entry or {}).get('height'))
                                scale_val = await self.input_handler.realize_wayland_dpi(
                                    dpi_value, session_screen_index(client_display_id), size)
                                capture_scale_changed = (
                                    entry is not None and entry.get('scale') != scale_val)
                                if entry is not None:
                                    entry['scale'] = scale_val
                                self._update_cursor_cap(dpi_value)
                                # A STOP_VIDEO'd display stays stopped; the next START_VIDEO
                                # applies the stored scale.
                                video_active = self.display_clients.get(client_display_id, {}).get('video_active', True)
                                if video_active and capture_scale_changed:
                                    data_logger.info(f"Wayland: restarting capture at scale {scale_val} for {client_display_id}")
                                    await self._stop_capture_for_display(client_display_id)
                                    if hasattr(self, 'display_layouts') and client_display_id in self.display_layouts:
                                        layout = self.display_layouts[client_display_id]
                                        await self._start_capture_for_display(
                                            display_id=client_display_id,
                                            width=layout['w'], height=layout['h'],
                                            x_offset=layout['x'], y_offset=layout['y']
                                        )
                                        await self._start_backpressure_task_if_needed(client_display_id)
                                        await self._sync_wayland_realized_geometry(client_display_id)

                            # Stored where SETTINGS stores its own DPI, or a later partial
                            # SETTINGS re-applies a DPI the desktop has moved off.
                            dpi_state = self.display_clients.get(client_display_id)
                            if dpi_state is not None:
                                dpi_state["scaling_dpi"] = dpi_value

                            if CURSOR_SIZE is not None:
                                if IS_WAYLAND:
                                    await self._apply_wayland_cursor_size(dpi_value)
                                else:
                                    new_cursor_size = cursor_size_for_dpi(dpi_value, CURSOR_SIZE)

                                    data_logger.info(f"Attempting to set cursor size to: {new_cursor_size} (based on DPI {dpi_value})")
                                    if await set_cursor_size(new_cursor_size):
                                        data_logger.info(f"Successfully set cursor size to {new_cursor_size}")
                                    else:
                                        data_logger.error(f"Failed to set cursor size to {new_cursor_size}")

                        except ValueError:
                            data_logger.error(f"Invalid DPI value in message: {message}")
                        except IndexError:
                            data_logger.error(f"Malformed DPI message: {message}")
                        except Exception as e_dpi:
                            data_logger.error(f"Error processing DPI message '{message}': {e_dpi}", exc_info=True)

                    elif message.startswith("cmd,"):
                        if not settings.command_enabled[0]:
                            data_logger.warning("Received 'cmd' message, but command execution is disabled by server settings.")
                            continue

                        if self.is_secure_mode and not self._holds_input_authority(websocket):
                            data_logger.warning(f"BLOCK (Secure Mode): 'cmd' from {remote_address} dropped; client does not hold input authority.")
                            continue

                        toks = message.split(',')
                        if len(toks) > 1:
                            command_to_run = ",".join(toks[1:])
                            data_logger.info(f"Attempting to execute command: '{command_to_run}'")

                            async def _send_cmd_status(action, ws=websocket):
                                try:
                                    await ws.send_str(
                                        "system," + json.dumps({"action": action}))
                                except Exception:
                                    pass

                            async def _notify_cmd_error(text):
                                await _send_cmd_status(f"command_error,{text}")

                            async def _notify_cmd_done(cmd):
                                await _send_cmd_status(f"command_done,{cmd}")

                            await run_client_command(
                                command_to_run, data_logger, notify=_notify_cmd_error,
                                env=self.input_handler.app_launch_env() if self.input_handler else None,
                                done=_notify_cmd_done)
                        else:
                            data_logger.warning("Received 'cmd' message without a command string.")

                    else:
                        if message.startswith("js,") and self.is_secure_mode:
                            perms = client_permissions.get(websocket)
                            if not perms or not perms.get("token"):
                                data_logger.warning(f"BLOCK (Secure Mode): Gamepad input from {remote_address} dropped. Client has no token/perms.")
                                continue
                            
                            token = perms.get("token")
                            current_perms = user_tokens.get(token)
                            server_slot = current_perms.get("slot") if current_perms else None

                            if server_slot is None:
                                data_logger.warning(f"BLOCK (Secure Mode): Gamepad input from {remote_address} dropped. Client token has no assigned slot.")
                                continue
                            try:
                                client_index = int(message.split(',')[2])
                                if (int(server_slot) - 1) != client_index:
                                    data_logger.warning(f"BLOCK (Secure Mode): Gamepad input from {remote_address} dropped. Client sent for index {client_index}, but is assigned slot {server_slot}.")
                                    continue
                            except (IndexError, ValueError):
                                data_logger.warning(f"BLOCK (Secure Mode): Malformed gamepad message from {remote_address}: {message}")
                                continue

                        # maxsplit=1: a full split of an 8 MiB clipboard chunk stalls the loop.
                        # 'cr' is exempt: every client sends it at connect, before it can hold
                        # authority, and the handler direction-gates it itself.
                        if self.is_secure_mode and message.split(',', 1)[0] in ["kd", "ku", "kh", "kr", "m", "m2", "co", "cws", "cbs", "cwd", "cbd", "cwe", "cbe", "cw", "cb", "REQUEST_CLIPBOARD"]:
                            if not self._holds_input_authority(websocket):
                                continue

                        if self.input_handler and hasattr(
                            self.input_handler, "on_message"
                        ):
                            # conn_id keeps the clipboard debounce per connection, not per display.
                            await self.input_handler.on_message(message, client_display_id, conn_id=id(websocket))

        except (ConnectionResetError, OSError, RuntimeError) as e:
            data_logger.info(f"Data WS disconnected from {raddr}: {e}")
        except Exception as e_main_loop:
            data_logger.error(
                f"Error in Data WS handler for {raddr}: {e_main_loop}", exc_info=True
            )
        finally:
            self.last_start_video_request_times.pop(websocket, None)
            self.last_viewer_keyframe_request_times.pop(websocket, None)
            self.video_paused_clients.discard(websocket)
            self._cancel_deferred_rejoin(websocket)
            departing_perms = client_permissions.pop(websocket, None) or {}
            # Dropped first: the authority and consumer verdicts below must see
            # the remaining clients only.
            self.clients.discard(websocket)
            data_logger.info(f"Cleaning up Data WS handler for {raddr} (Display ID: {client_display_id})...")
            # A tab that dies mid-press never sends 'js,d'; the button would stay
            # stuck on the virtual pad.
            if self.input_handler and hasattr(self.input_handler, "release_gamepads_for_conn"):
                try:
                    await self.input_handler.release_gamepads_for_conn(id(websocket))
                except Exception as e:
                    data_logger.warning(f"Gamepad release on disconnect failed: {e}")

            # The release rule is in the docstring: primary owner, or last input-capable client.
            _primary_entry = self.display_clients.get('primary')
            departing_input_authority = self._holds_input_authority(websocket, departing_perms) and (
                (_primary_entry is not None and _primary_entry.get('ws') is websocket)
                or not any(self._holds_input_authority(ws) for ws in self.clients)
            )

            # A tab that dies mid-drag never sends the button-up mask.
            if (
                self.input_handler
                and departing_input_authority
                and hasattr(self.input_handler, "release_mouse_buttons")
            ):
                try:
                    await self.input_handler.release_mouse_buttons()
                except Exception as e:
                    data_logger.warning(f"Mouse button release on disconnect failed: {e}")

            # Now rather than at the next fan-out: an idle capture (JPEG, streaming
            # off) sends no chunk to prune a dead relay, which pins its buffers.
            for relay_group in self.video_relay_groups.values():
                stale_relay = relay_group.pop(websocket, None)
                if stale_relay is not None:
                    stale_relay.stop()
            if self.data_ws is websocket:
                self.data_ws = None
            # A departing non-capable client may let the rest enable RED.
            self.audio_redundancy_by_ws.pop(websocket, None)
            if self.is_pcmflux_capturing:
                async with self._reconfigure_guard():
                    await self._regate_audio_redundancy()

            disconnected_display_id = None
            for disp_id, client_info in self.display_clients.items():
                if client_info.get('ws') is websocket:
                    disconnected_display_id = disp_id
                    break

            if disconnected_display_id:
                # Deferred: a reloading page takes the entry over with its capture warm;
                # tearing down here would serialize its startup behind this reconfigure
                # on the lock (seconds of black stream per reload).
                data_logger.info(
                    f"Client for '{disconnected_display_id}' disconnected. Deferring display teardown by {self.RECONNECT_GRACE_S:.0f}s for a possible reconnect."
                )

                async def _teardown_if_unclaimed(did=disconnected_display_id, dead_ws=websocket):
                    disconnect_ts = time.monotonic()
                    deadline = disconnect_ts + 15.0
                    while True:
                        await asyncio.sleep(self.RECONNECT_GRACE_S)
                        entry = self.display_clients.get(did)
                        if entry is None or entry.get('ws') is not dead_ws:
                            data_logger.info(f"Display '{did}' was claimed by a new connection during the grace period; teardown skipped.")
                            return
                        # A connection newer than the disconnect may still be mid-handshake
                        # (audio setup precedes its claim): held until the deadline.
                        latest_connect = max(self.last_connection_times.values(), default=0.0)
                        if latest_connect > disconnect_ts and time.monotonic() < deadline:
                            continue
                        break
                    entry = self.display_clients.get(did)
                    if entry is None or entry.get('ws') is not dead_ws:
                        data_logger.info(f"Display '{did}' was claimed by a new connection during the grace period; teardown skipped.")
                        return
                    del self.display_clients[did]
                    data_logger.info(f"Client for '{did}' did not return within the grace period. Removing and triggering full display reconfiguration.")
                    await self.reconfigure_displays()
                    # A viewer-started capture has no owning display client, so the
                    # reconfigure never stops it.
                    await self._stop_primary_if_unconsumed(
                        "No unpaused consumer of 'primary' left after the grace period."
                    )
                    if not self.clients:
                        data_logger.info("Last client gone after the grace period. Tearing down singleton collectors and pipelines.")
                        for _singleton_attr in (
                            "_network_monitor_task_ws",
                            "_system_monitor_task_ws",
                            "_gpu_monitor_task_ws",
                        ):
                            _singleton_task = getattr(self, _singleton_attr, None)
                            if _singleton_task and not _singleton_task.done():
                                _singleton_task.cancel()
                            setattr(self, _singleton_attr, None)
                        self.capture_cursor = False
                        self._last_keyframe_request.clear()
                        self._shared_stats_ws.clear()
                        self._shared_network_stats.clear()
                        # Self-acquires _reconfigure_lock; it must not be held here.
                        await self.shutdown_pipelines()

                _teardown_task = asyncio.create_task(_teardown_if_unclaimed())
                self._display_teardown_tasks.add(_teardown_task)
                _teardown_task.add_done_callback(self._display_teardown_tasks.discard)
            else:
                data_logger.info(f"Unregistered client at {raddr} disconnected. No display reconfiguration needed.")
                # Nothing else stops the primary capture for a socket owning no display.
                await self._stop_primary_if_unconsumed(
                    "Last unpaused consumer of 'primary' disconnected."
                )

            # Per-connection tasks only; cancelling the singleton collectors here
            # would break the remaining clients.
            monitor_tasks = [
                stats_sender_task_ws,
                start_audio_task_ws,
            ]
            for _task_to_cancel in monitor_tasks:
                if not _task_to_cancel:
                    continue
                _task_to_cancel.cancel()
                # Awaited unconditionally: a task that already failed on the dying
                # socket has its exception retrieved here, never propagated.
                try:
                    await _task_to_cancel
                except asyncio.CancelledError:
                    pass
                except Exception as e_conn_task:
                    data_logger.debug(
                        f"Per-connection task for {raddr} ended with an error: {e_conn_task}"
                    )

            # stop() joins the playback thread; offloaded so a slow PA disconnect
            # cannot block the loop.
            _mic_playback = locals().get("mic_playback")
            if _mic_playback is not None:
                try:
                    await asyncio.to_thread(_mic_playback.stop)
                    data_logger.debug(f"Stopped Rust mic playback for {raddr}.")
                except Exception as e_mic_pb:
                    data_logger.error(f"Error stopping Rust mic playback for {raddr}: {e_mic_pb}")

            if mic_control is not None:
                if pa_module_index is not None and pa_module_owned:
                    data_logger.info(
                        f"Unloading PulseAudio module {pa_module_index} for virtual mic (client: {raddr})."
                    )
                    await mic_control.unload_module(pa_module_index)
                await mic_control.aclose()
                data_logger.debug(f"Closed sound server control connection for {raddr}.")


            if self.input_handler and departing_input_authority:
                try:
                    await self.input_handler.reset_keyboard()
                    data_logger.info(f"Keyboard reset completed ({raddr}) disconnect.")
                except Exception as e_reset:
                    data_logger.warning(f"Failed to reset keyboard after client disconnect: {e_reset}")

            # A display-owning socket's last-client teardown ran in the grace task above.
            if disconnected_display_id is None and not self.clients:
                 data_logger.info(f"Last client ({raddr}) disconnected. All pipelines should have been stopped by reconfigure_displays.")
                 # Each ref is nulled: cancel is async, and a fast reconnect must restart them.
                 for _singleton_attr in (
                     "_network_monitor_task_ws",
                     "_system_monitor_task_ws",
                     "_gpu_monitor_task_ws",
                 ):
                     _singleton_task = getattr(self, _singleton_attr, None)
                     if _singleton_task and not _singleton_task.done():
                         _singleton_task.cancel()
                     setattr(self, _singleton_attr, None)
                 self.capture_cursor = False
                 self._last_keyframe_request.clear()
                 # A reconnect must not briefly read a dead collector's stale stats.
                 self._shared_stats_ws.clear()
                 self._shared_network_stats.clear()
                 # Self-acquires _reconfigure_lock; it must not be held here.
                 await self.shutdown_pipelines()

            data_logger.info(f"Data WS handler for {raddr} finished all cleanup.")

    async def _run_detached_command(self, cmd_list: list[str], description: str) -> None:
        """Run a command detached from the server process: its own session
        (start_new_session) survives our exit and our signals, with no shell in
        between."""
        data_logger.info(f"Running detached command ({description}): {' '.join(cmd_list)}")
        try:
            await asyncio.create_subprocess_exec(
                *cmd_list,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
                start_new_session=True,
            )
        except Exception as e:
            data_logger.error(f"Failed to run detached command ({description}): {e}")

    async def _run_command(self, cmd: list[str], description: str, best_effort: bool = False) -> bool:
        """Run an external command (10s bound) and log its output/errors.

        Args:
            cmd: The argv list (no shell).
            description: Label used in log lines.
            best_effort: Logs a non-zero exit at DEBUG instead of ERROR — for
                delete-if-exists cleanups that fail only because the target is
                already gone.

        Returns:
            True on a zero exit within the timeout.
        """
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE
            )
            try:
                stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=10.0)
            except asyncio.TimeoutError:
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
                await proc.wait()
                log = data_logger.debug if best_effort else data_logger.error
                log(f"Timed out ({description}) after 10s; killed.")
                return False
            if proc.returncode != 0:
                log = data_logger.debug if best_effort else data_logger.error
                log(
                    f"Failed ({description}). RC: {proc.returncode}, "
                    f"Stderr: {stderr.decode().strip()}"
                )
                return False
            return True
        except Exception as e:
            log = data_logger.debug if best_effort else data_logger.error
            log(f"Exception during '{description}': {e}", exc_info=not best_effort)
            return False

    def _wayland_control_module(self) -> Optional[Any]:
        """A pixelflux handle for compositor output management (any ScreenCapture
        reaches the shared Wayland backend); prefers the primary's persistent
        module so no extra instance exists in the common case."""
        module = self._persistent_capture_modules.get('primary')
        if module is not None:
            return module
        if ScreenCapture is None:
            return None
        if self._wayland_ctl_module is None:
            self._wayland_ctl_module = ScreenCapture()
        return self._wayland_ctl_module

    async def _drop_wayland_secondary(self, display_id: str, reason: str) -> None:
        """Refuse a secondary display that cannot stream: destroy its compositor
        output (Wayland; no-op on X11 where the control module is absent), stop
        its capture, unregister it, and kill its client with the reason."""
        module = self._wayland_control_module()
        if module is not None:
            try:
                await asyncio.to_thread(module.destroy_output, wayland_output_id(display_id))
            except Exception:
                pass
        await self._stop_capture_for_display(display_id)
        dropped_client = self.display_clients.pop(display_id, None)
        getattr(self, 'display_layouts', {}).pop(display_id, None)
        dropped_ws = dropped_client.get('ws') if dropped_client else None
        data_logger.error(f"Secondary display '{display_id}' dropped on Wayland: {reason}")
        if dropped_ws is not None:
            try:
                await asyncio.wait_for(dropped_ws.send_str(f"KILL {reason}"), timeout=2.0)
                await asyncio.wait_for(
                    dropped_ws.close(code=1008, message=b"Secondary display unrealizable"),
                    timeout=2.0,
                )
            except asyncio.TimeoutError:
                _close_abandoned_ws(dropped_ws)
            except (ConnectionResetError, OSError, RuntimeError):
                pass

    async def _reanchor_wayland_primary(self, layouts: dict, keep_ids: set[str]) -> None:
        """Collapse an unrealizable Wayland arrangement: primary back at the
        origin (layout + capture rebuild) — the Wayland mirror of the X11
        re-anchor when the extension does not fit the realized root."""
        primary_layout = layouts.get('primary')
        if primary_layout:
            primary_layout['x'], primary_layout['y'] = 0, 0
        if 'primary' in keep_ids:
            keep_ids.discard('primary')
            await self._stop_capture_for_display('primary')

    async def _apply_wayland_output_layout(self, layouts: dict, keep_ids: set[str]) -> None:
        """Retire and move compositor outputs for the computed union layout.

        The Wayland counterpart of the X11 monitor/framebuffer apply, split
        around the primary's capture start: pixelflux refuses any placement
        that overlaps a live output, while a capture start resizes output 0
        unvalidated, so a secondary moving into room a shrinking primary gives
        up can only be created once output 0 has shrunk. This pass therefore
        only removes and moves: stale and moved secondaries are destroyed (a
        secondary reposition is a destroy + recreate; its capture dies with the
        output and the start loop rebuilds it) and the primary (output 0) is
        moved to its layout offset ('left'/'up' place it off-origin; teardown
        re-anchors it at 0,0). A primary move the compositor refuses is retried
        with every secondary output destroyed — a secondary that shrinks can
        leave the primary's new offset inside its old rectangle, and the
        outputs come back in _create_wayland_outputs anyway — and only then is
        the arrangement void: primary back at the origin, every secondary
        dropped. _create_wayland_outputs, run by the start loop right after
        the primary's capture start, creates the secondary outputs.

        Args:
            layouts: display_id to layout rect; mutated when a display has to
                be dropped (the primary move refused), killing its client like
                the X11 path.
            keep_ids: The keep-alive capture set; mutated alongside `layouts`.
        """
        module = self._wayland_control_module()
        if module is None:
            return
        try:
            outputs = {o[0]: o for o in await asyncio.to_thread(module.list_outputs)}
        except Exception as e:
            data_logger.error(f"Wayland list_outputs failed: {e}")
            outputs = {}
        wanted = {wayland_output_id(did): did for did in layouts if did != 'primary'}
        for oid in list(outputs):
            if oid != 0 and oid not in wanted:
                data_logger.info(f"Destroying stale Wayland output {oid}.")
                await asyncio.to_thread(module.destroy_output, oid)
                outputs.pop(oid, None)

        async def recreate_later(oid: int, did: str, why: str) -> None:
            data_logger.info(f"Wayland output {oid} {why}; recreating it.")
            await asyncio.to_thread(module.destroy_output, oid)
            keep_ids.discard(did)
            await self._stop_capture_for_display(did)
            outputs.pop(oid, None)

        for oid, did in wanted.items():
            layout = layouts[did]
            existing = outputs.get(oid)
            if existing is not None and (existing[1], existing[2]) != (layout['x'], layout['y']):
                await recreate_later(oid, did, f"moves to +{layout['x']}+{layout['y']}")
        primary_layout = layouts.get('primary')
        target = (primary_layout['x'], primary_layout['y']) if primary_layout else (0, 0)
        existing0 = outputs.get(0)
        current = (existing0[1], existing0[2]) if existing0 is not None else (0, 0)
        if target != current:
            moved = await wayland_reposition_primary(module, target[0], target[1])
            if not moved and any(outputs.get(oid) is not None for oid in wanted):
                for oid, did in wanted.items():
                    if outputs.get(oid) is not None:
                        await recreate_later(oid, did, "blocks the primary's move")
                moved = await wayland_reposition_primary(module, target[0], target[1])
            if not moved:
                await wayland_reposition_primary(module, 0, 0)
                await self._reanchor_wayland_primary(layouts, keep_ids)
                for did in [d for d in list(layouts) if d != 'primary']:
                    del layouts[did]
                    keep_ids.discard(did)
                    await self._drop_wayland_secondary(
                        did, "The compositor cannot move the primary output for this arrangement."
                    )
        # Which of the session's own screens a capture drives has just changed.
        if self.input_handler:
            self.input_handler.resync_session_screens()

    async def _create_wayland_outputs(self, layouts: dict, keep_ids: set[str]) -> None:
        """Give every laid-out secondary its compositor output, at its layout rectangle.

        The second half of the Wayland layout apply, run once the primary's
        capture start has sized output 0 (see _apply_wayland_output_layout).
        A display whose output the compositor cannot create is dropped like the
        X11 path's unrealizable display, and when the arrangement was built
        around it the primary returns to the origin — its capture follows the
        moved output live, so only the layout and the tracked capture offset
        change.

        Args:
            layouts: display_id to layout rect; mutated when a display is
                dropped, killing its client.
            keep_ids: The keep-alive capture set; mutated alongside `layouts`.
        """
        module = self._wayland_control_module()
        if module is None:
            return
        wanted = {wayland_output_id(did): did for did in layouts if did != 'primary'}
        if not wanted:
            return
        try:
            outputs = {o[0]: o for o in await asyncio.to_thread(module.list_outputs)}
        except Exception as e:
            data_logger.error(f"Wayland list_outputs failed: {e}")
            outputs = {}
        primary_layout = layouts.get('primary')
        created_any = False
        for oid, did in wanted.items():
            if did not in layouts or outputs.get(oid) is not None:
                continue
            layout = layouts[did]
            client = self.display_clients.get(did) or {}
            scale = float(client.get('scale', 1.0) or 1.0)
            created = False
            try:
                created = bool(await asyncio.to_thread(
                    module.create_output, oid,
                    layout['w'], layout['h'], layout['x'], layout['y'], scale,
                ))
            except Exception as e:
                data_logger.error(f"Wayland create_output {oid} failed: {e}")
            if created:
                created_any = True
                continue
            del layouts[did]
            keep_ids.discard(did)
            await self._drop_wayland_secondary(
                did, "The compositor cannot create an output for this display."
            )
            if primary_layout and (primary_layout['x'], primary_layout['y']) != (0, 0):
                if await wayland_reposition_primary(module, 0, 0):
                    primary_layout['x'], primary_layout['y'] = 0, 0
                    self._track_capture_settings('primary', capture_x=0, capture_y=0)
        if created_any and self.input_handler:
            self.input_handler.resync_session_screens()

    async def _stop_capture_for_display(self, display_id: str) -> None:
        """Stop one display's capture, serialized against any concurrent start/stop."""
        async with self._video_capture_lock:
            await self._stop_capture_for_display_impl(display_id)

    async def _stop_capture_for_display_impl(self, display_id: str) -> None:
        """Stop the capture, relays, and backpressure task for one display.

        Callers hold _video_capture_lock. Guarantees exactly one
        PIPELINE_RESETTING per real capture stop: clients rebuild their video
        sinks/decoders only on that message, including when no backpressure
        task ran (viewer-only captures, stops before the task armed) — without
        it a resumed stream plays into the stale sink and freezes silently.
        """
        data_logger.info(f"Stopping all streams for display '{display_id}'...")
        reset_sent = await self._ensure_backpressure_task_is_stopped(display_id)
        capture_info = self.capture_instances.pop(display_id, None)
        if capture_info:
            capture_module = capture_info.get('module')
            if capture_module:
                await asyncio.to_thread(capture_module.stop_capture)
        self._close_video_relays(display_id)
        if capture_info and not reset_sent:
            await self._reset_frame_ids_and_notify(display_id)

        data_logger.info(f"Successfully stopped all streams for display '{display_id}'.")
 
    @contextlib.asynccontextmanager
    async def _reconfigure_guard(self):
        """Hold _reconfigure_lock for a direct critical section (audio pipeline
        ops) and, on release, run any reconfigure coalesced meanwhile.

        reconfigure_displays()'s own re-run loop only consumes requests that
        arrive through it; a reconfigure coalesced during a direct hold would
        otherwise be stranded (orphaning a disconnected display's capture).
        The re-check runs on raising exits too, and outside the lock, since
        reconfigure_displays() only coalesces while the lock is held.
        """
        try:
            async with self._reconfigure_lock:
                yield
        finally:
            if self._reconfigure_pending:
                await self.reconfigure_displays()

    async def reconfigure_displays(self) -> None:
        """Rebuild the virtual desktop layout for ALL connected clients.

        Called on connect, disconnect, or settings change. Starts capture
        pipelines only for clients with video_active True. Self-serializing:
        a call while a pass is running coalesces (last-write-wins) into one
        follow-up pass instead of queueing, so state converges on the latest
        request without a reconfigure storm.
        """
        if self._reconfigure_lock.locked():
            self._reconfigure_pending = True
            data_logger.info("Reconfiguration already in progress; coalescing this request.")
            return
        while True:
            async with self._reconfigure_lock:
                self._reconfigure_pending = False
                self._is_reconfiguring = True
                data_logger.info("Starting display reconfiguration...")
                try:
                    await self._reconfigure_displays_locked()
                except Exception as e:
                    data_logger.error(f"A critical error occurred during display reconfiguration: {e}", exc_info=True)
                finally:
                    self._last_display_count = len(self.display_clients)
                    self._is_reconfiguring = False
                    data_logger.info("Reconfiguration process complete (state unlocked).")
            if not self._reconfigure_pending:
                break

    async def _signal_all_displays_stopped(self) -> None:
        """Send VIDEO_STOPPED to clients on a reconfiguration abort without
        clearing video_active: a transient abort (zero size, no screen_name, a
        failed newmode) must not permanently stop healthy displays, so the
        next successful reconfigure auto-restarts them. `stop_signaled` marks
        a client that now discards frames until that reconfigure sends it
        VIDEO_STARTED."""
        # Snapshot: the sends await, and a concurrent connect/disconnect would
        # change the dict mid-iteration.
        for display_id, client_data in list(self.display_clients.items()):
            # The entry may have been removed during a prior iteration's await.
            if self.display_clients.get(display_id) is not client_data:
                continue
            ws = client_data.get('ws')
            if ws:
                try:
                    # Bounded: runs under _reconfigure_lock; a frozen client is dropped,
                    # not waited on.
                    await asyncio.wait_for(ws.send_str("VIDEO_STOPPED"), timeout=2.0)
                    client_data['stop_signaled'] = True
                except asyncio.TimeoutError:
                    _close_abandoned_ws(ws)
                except (ConnectionResetError, OSError, RuntimeError):
                    pass

    async def _signal_display_stopped(self, display_id: str) -> None:
        """Tell one display's client the pipeline stopped, keeping video_active so a
        later successful reconfigure restarts it. The single-display form of
        _signal_all_displays_stopped, used when a primary capture did not come up (a
        failed start, or a host compositor that died) so the client sees a truthful
        verdict instead of a page frozen on "Waiting for stream"."""
        client_data = self.display_clients.get(display_id)
        ws = client_data.get('ws') if client_data else None
        if not ws:
            return
        try:
            await asyncio.wait_for(ws.send_str("VIDEO_STOPPED"), timeout=2.0)
            client_data['stop_signaled'] = True
        except asyncio.TimeoutError:
            _close_abandoned_ws(ws)
        except (ConnectionResetError, OSError, RuntimeError):
            pass

    async def _reconfigure_displays_locked(self) -> None:
        """One reconfiguration pass. Must only be called by reconfigure_displays()
        with _reconfigure_lock held; early returns here abort just this pass.

        The pass: optionally swap in a multi-monitor-capable WM (X11), compute
        the union layout from all display clients, decide per running capture
        whether it can follow the new layout live (structurally identical
        sessions retune in place; the rest are stopped and rebuilt), realize
        the layout (xrandr monitors + framebuffer on X11; on Wayland the
        compositor outputs, whose secondaries are created only after the
        primary's capture start has sized output 0), clamp everything to what
        the server actually realized — dropping displays that cannot exist —
        then (re)start the active captures and broadcast the resulting
        resolutions and roster. A capture that did not come up yields a
        verdict rather than a page stuck on "Waiting for stream": a secondary
        is dropped (X11 parity), the primary is told the stream stopped with
        video_active kept so the next successful pass restarts it — which
        also surfaces a dead host compositor.
        """
        current_display_count = len(self.display_clients)
        await self._wm_swap.ensure_for(current_display_count, IS_WAYLAND)
        if not self.display_clients:
            for display_id in list(self.capture_instances.keys()):
                await self._stop_capture_for_display(display_id)
            data_logger.warning("No display clients connected. Video pipelines remain stopped.")
            if not IS_WAYLAND:
                await clear_selkies_monitors()
            else:
                # The primary output persists; only the secondaries are retired.
                await self._apply_wayland_output_layout({}, set())
            return
        data_logger.info("Calculating new extended desktop layout from ALL clients...")
        layouts = {}
        total_width = 0
        total_height = 0
        primary_client = self.display_clients.get('primary')
        secondary_client = None
        secondary_id = None
        for display_id, client in self.display_clients.items():
            if display_id != 'primary':
                secondary_client = client
                secondary_id = display_id
                break
        if primary_client and not secondary_client:
            p_w, p_h = primary_client.get('width', 0), primary_client.get('height', 0)
            if p_w > 0 and p_h > 0:
                layouts['primary'] = {'x': 0, 'y': 0, 'w': p_w, 'h': p_h}
                total_width, total_height = p_w, p_h
        elif primary_client and secondary_client:
            p_w, p_h = primary_client.get('width', 0), primary_client.get('height', 0)
            s_w, s_h = secondary_client.get('width', 0), secondary_client.get('height', 0)
            position = secondary_client.get('position', 'right')
            if position not in ('right', 'left', 'up', 'down'):
                data_logger.warning(f"Invalid display position '{position}'; falling back to 'right'.")
                position = 'right'
            # Auto-resize feedback guard, shared with the WebRTC layout engine.
            p_w, p_h = clamp_primary_feedback(
                (p_w, p_h), getattr(self, 'display_layouts', None), position
            )
            if p_w > 0 and p_h > 0 and s_w > 0 and s_h > 0:
                computed, total_width, total_height = compute_dual_layout(
                    (p_w, p_h), (s_w, s_h), position
                )
                layouts['primary'] = computed['primary']
                layouts[secondary_id] = computed['secondary']
        if total_width == 0 or total_height == 0:
            data_logger.error("Calculated total display size is zero. Aborting reconfiguration.")
            await self._signal_all_displays_stopped()
            return
        # The single-display total still needs the xrandr framebuffer alignment.
        total_width = (total_width + 7) & ~7
        self.display_layouts = layouts
        data_logger.info(f"Layout calculated: Total Size={total_width}x{total_height}. Layouts: {layouts}")

        keep_ids = set()
        async with self._video_capture_lock:
            for did in list(self.capture_instances.keys()):
                inst = self.capture_instances[did]
                module = inst.get('module')
                client = self.display_clients.get(did)
                wanted = did in layouts and client is not None and client.get('video_active', False)
                alive = False
                if wanted and module is not None:
                    try:
                        alive = bool(module.is_capturing)
                    except Exception:
                        alive = False
                structural_ok = False
                if alive:
                    old_cs, layout = inst.get('settings'), layouts[did]
                    try:
                        fresh = self._get_capture_settings(did, layout['w'], layout['h'], layout['x'], layout['y'])
                        structural_ok = old_cs is not None and all(
                            getattr(fresh, k) == getattr(old_cs, k)
                            for k in ('output_mode', 'use_cpu',
                                      'video_fullframe', 'video_fullcolor', 'video_cbr_mode')
                        )
                        if structural_ok:
                            inst['settings'] = fresh
                    except Exception:
                        structural_ok = False
                if structural_ok:
                    keep_ids.add(did)
                else:
                    await self._stop_capture_for_display_impl(did)

        if not IS_WAYLAND:
            curr_res, _, available_resolutions, _, screen_name = await get_new_res("1x1")
            if not screen_name:
                data_logger.error("CRITICAL: Could not determine screen name from xrandr. Aborting.")
                await self._signal_all_displays_stopped()
                return
            total_mode_str = f"{total_width}x{total_height}"
            if total_mode_str not in available_resolutions:
                data_logger.info(f"Mode {total_mode_str} not found. Creating it.")
                # Native first: a mode made by per-invocation xrandr dies with its
                # connection on some servers (Xvfb).
                if not await ensure_mode(total_mode_str):
                    try:
                        _, modeline_params = await generate_xrandr_gtf_modeline(total_mode_str)
                        await self._run_command(["xrandr", "--newmode", total_mode_str] + modeline_params.split(), "create new mode")
                        await self._run_command(["xrandr", "--addmode", screen_name, total_mode_str], "add new mode")
                    except Exception as e:
                        data_logger.error(f"FATAL: Could not create extended mode {total_mode_str}: {e}. Aborting.")
                        await self._signal_all_displays_stopped()
                        return
            if keep_ids:
                # Grow first, shrink after: a region outside the root fails the grab and
                # kills the capture thread. The grow runs whenever the union exceeds the
                # current root, or the re-target pins clamped (its grab never fails).
                try:
                    cur_w, cur_h = (int(v) for v in curr_res.lower().replace(" ", "").split("x"))
                except (ValueError, AttributeError):
                    cur_w, cur_h = total_width, total_height
                union_w, union_h = max(cur_w, total_width), max(cur_h, total_height)
                grew = True
                if (union_w, union_h) != (cur_w, cur_h):
                    grew = await grow_framebuffer(union_w, union_h)
                if not grew:
                    # At the framebuffer's bound a layout past the current root would
                    # pin clamped at re-target; those captures restart after the mode-set.
                    for did in sorted(list(keep_ids)):
                        layout = layouts[did]
                        if (layout['x'] + layout['w'] > cur_w
                                or layout['y'] + layout['h'] > cur_h):
                            data_logger.warning(
                                f"Framebuffer grow refused (still {cur_w}x{cur_h}); "
                                f"capture '{did}' restarts after the mode-set.")
                            keep_ids.discard(did)
                            await self._stop_capture_for_display(did)
                for did in sorted(keep_ids):
                    layout = layouts[did]
                    module = self.capture_instances[did]['module']
                    try:
                        module.update_capture_region(layout['x'], layout['y'], layout['w'], layout['h'])
                        data_logger.info(f"Re-targeted live capture '{did}' to {layout} (no restart).")
                    except Exception as e:
                        data_logger.warning(f"Live re-target failed for '{did}' ({e}); restarting it.")
                        keep_ids.discard(did)
                        await self._stop_capture_for_display(did)
            data_logger.info("Swapping logical monitors to the new layout...")
            # Monitors go in before the framebuffer change, at their final rectangles
            # and under a server grab: window managers re-tile on every root
            # ConfigureNotify and must never see a monitor-less or partial set.
            await replace_selkies_monitors(layouts, screen_name=screen_name)
            # A mode change is the dominant cost of a reconfigure (CRTC reprogram,
            # every client repaints), so a same-size reload skips it. A live
            # re-target that grew the framebuffer above still shrinks here.
            curr_norm = (curr_res or "").lower().replace(" ", "")
            if curr_norm == total_mode_str:
                data_logger.info(f"Screen already at {total_mode_str}; skipping redundant framebuffer/mode-set.")
            else:
                if not await resize_display(total_mode_str):
                    # Some servers refuse runtime modes but honor a plain framebuffer
                    # grow (RRSetScreenSize); captures and pointer warps address the root.
                    if await grow_framebuffer(total_width, total_height):
                        data_logger.info(f"Mode-set for {total_mode_str} failed; grew the framebuffer instead.")
                    else:
                        data_logger.error(f"Applying mode {total_mode_str} failed; clamping to the realized size below.")
            # The X server is the authority: a driver can refuse the size and leave
            # the root as it was, and a region outside the root grabs garbage.
            realized_w, realized_h = await read_realized_root((total_width, total_height))
            if (realized_w, realized_h) != (total_width, total_height):
                data_logger.warning(
                    f"Realized screen size {realized_w}x{realized_h} differs from target "
                    f"{total_width}x{total_height}; clamping display layouts to it."
                )
                offsets = {d: (l['x'], l['y']) for d, l in layouts.items()}
                fit = reconcile_realized_layout(layouts, realized_w, realized_h)
                if fit.reanchored:
                    data_logger.error(
                        f"Primary at +{offsets['primary'][0]}+{offsets['primary'][1]} does not fit "
                        f"the realized {realized_w}x{realized_h} root; re-anchored at the origin."
                    )
                    if 'primary' in keep_ids:
                        # Re-targeted to the void offset above; rebuilt at the re-anchored region.
                        keep_ids.discard('primary')
                        await self._stop_capture_for_display('primary')
                for did in fit.dropped:
                    data_logger.error(
                        f"Display '{did}' at +{offsets[did][0]}+{offsets[did][1]} does not fit the "
                        f"realized {realized_w}x{realized_h} root; dropping it. The X server "
                        "must allow a framebuffer covering all displays (e.g. a larger Xvfb "
                        "-screen) for extended layouts."
                    )
                    keep_ids.discard(did)
                    await self._stop_capture_for_display(did)
                    dropped_client = self.display_clients.get(did)
                    dropped_ws = dropped_client.get('ws') if dropped_client else None
                    if dropped_ws is not None:
                        try:
                            await asyncio.wait_for(
                                dropped_ws.send_str(
                                    "KILL The X server cannot extend the desktop to fit this display."
                                ),
                                timeout=2.0,
                            )
                            await asyncio.wait_for(
                                dropped_ws.close(code=1008, message=b"Extended layout unrealizable"),
                                timeout=2.0,
                            )
                        except asyncio.TimeoutError:
                            _close_abandoned_ws(dropped_ws)
                        except (ConnectionResetError, OSError, RuntimeError):
                            pass
                for did in fit.clamped:
                    layout = layouts[did]
                    data_logger.warning(
                        f"Display '{did}': layout clamped to {layout['w']}x{layout['h']} "
                        "inside the realized root."
                    )
                    client_data = self.display_clients.get(did)
                    if client_data:
                        client_data['width'], client_data['height'] = layout['w'], layout['h']
                    if did == 'primary':
                        self.app.display_width = layout['w']
                        self.app.display_height = layout['h']
                    # A kept capture was re-targeted to the pre-clamp region.
                    inst = self.capture_instances.get(did)
                    if did in keep_ids and inst and inst.get('module'):
                        try:
                            inst['module'].update_capture_region(
                                layout['x'], layout['y'], layout['w'], layout['h']
                            )
                            inst['settings'] = self._get_capture_settings(
                                did, layout['w'], layout['h'], layout['x'], layout['y']
                            )
                        except Exception as e:
                            data_logger.warning(
                                f"Re-target to clamped region failed for '{did}' ({e}); restarting it."
                            )
                            keep_ids.discard(did)
                            await self._stop_capture_for_display(did)
                # One atomic re-swap (RRSetMonitor cannot redefine a name in place);
                # a root that merely came back larger needs none, since every swap re-tiles.
                if fit.dropped or fit.reanchored or fit.clamped:
                    await replace_selkies_monitors(layouts, screen_name=screen_name)
        else:
            await self._apply_wayland_output_layout(layouts, keep_ids)
        data_logger.info("Starting separate capture instances for each ACTIVE display region...")
        # The primary first: on Wayland its capture start sizes output 0.
        for display_id in sorted(layouts, key=lambda did: did != 'primary'):
            if display_id not in layouts:
                continue
            layout = layouts[display_id]
            client_data = self.display_clients.get(display_id)
            if client_data and client_data.get('video_active', False):
                try:
                    if display_id in keep_ids:
                        # Kept live: the current rates/tunables are pushed so settings drift rides.
                        inst = self.capture_instances[display_id]
                        module, fresh = inst['module'], inst['settings']
                        if IS_WAYLAND:
                            # A start on the live capture reconfigures it in place,
                            # serialized against a concurrent teardown.
                            async with self._video_capture_lock:
                                await asyncio.to_thread(module.start_capture, inst['callback'], fresh)
                        else:
                            module.update_framerate(float(fresh.target_fps))
                            module.update_video_bitrate(int(fresh.video_bitrate_kbps))
                            module.update_tunables(fresh)
                        data_logger.info(f"Capture '{display_id}' followed the new layout live (no restart).")
                    else:
                        data_logger.info(f"Client '{display_id}' is active. Starting its capture.")
                        await self._start_capture_for_display(
                            display_id=display_id,
                            width=layout['w'], height=layout['h'],
                            x_offset=layout['x'], y_offset=layout['y']
                        )
                    await self._start_backpressure_task_if_needed(display_id)
                    # A client told VIDEO_STOPPED by a transient abort still discards frames.
                    if client_data.get('stop_signaled'):
                        ws = client_data.get('ws')
                        if ws:
                            try:
                                await asyncio.wait_for(ws.send_str("VIDEO_STARTED"), timeout=2.0)
                            except asyncio.TimeoutError:
                                _close_abandoned_ws(ws)
                            except (ConnectionResetError, OSError, RuntimeError):
                                pass
                        client_data['stop_signaled'] = False
                except Exception as e:
                    data_logger.error(
                        f"Failed to start capture for display '{display_id}' during reconfiguration. "
                        f"This display will not stream. Error: {e}", exc_info=False
                    )
            else:
                data_logger.info(f"Client '{display_id}' is connected but not active. Skipping video start.")
            if IS_WAYLAND and display_id == 'primary':
                await self._create_wayland_outputs(layouts, keep_ids)
        for display_id in list(layouts.keys()):
            client_data = self.display_clients.get(display_id)
            if not (client_data and client_data.get('video_active', False)):
                continue
            if IS_WAYLAND:
                # Barrier: the read answers only after the queued start finished,
                # so is_capturing is authoritative below.
                await self._sync_wayland_realized_geometry(display_id, broadcast=False)
            inst = self.capture_instances.get(display_id)
            module = inst.get('module') if inst else None
            capturing = False
            if module is not None:
                try:
                    capturing = bool(module.is_capturing)
                except Exception:
                    capturing = False
            if not capturing:
                last_error = self._wayland_capture_last_error(module, display_id)
                if display_id == 'primary':
                    data_logger.error(
                        "Primary capture is not live after reconfiguration"
                        + (f": {last_error}." if last_error else "."))
                    await self._signal_display_stopped(display_id)
                else:
                    await self._drop_wayland_secondary(
                        display_id,
                        last_error or "The capture pipeline could not start for this "
                        "display (encoder session or GPU resources exhausted).",
                    )
        await self.broadcast_stream_resolution()
        await self.broadcast_display_config()
        data_logger.info("Display reconfiguration finished successfully.")


    async def _ensure_viewer_capture(self) -> bool:
        """Start the primary capture for a shared/player viewer when no display-
        owning client is connected (fresh server, or the controller left): the
        desktop exists regardless, so a lone viewer must not wait on a controller
        ("Waiting for stream..." forever). Captures the CURRENT desktop geometry —
        viewers never resize anything; the next controller's settings re-layout
        as usual.

        Returns:
            True when the primary capture is running afterwards.
        """
        if 'primary' in self.capture_instances:
            return True
        layout = getattr(self, 'display_layouts', {}).get('primary')
        if layout:
            w, h, x, y = layout['w'], layout['h'], layout['x'], layout['y']
        else:
            w, h = self.app.display_width, self.app.display_height
            x = y = 0
            if not IS_WAYLAND:
                # On X11 the desktop size is external truth; on Wayland the start sizes the output.
                try:
                    curr_res = (await get_new_res(f"{w}x{h}"))[0]
                    w, h = map(int, curr_res.split('x'))
                except Exception as e:
                    data_logger.warning(f"Viewer capture: desktop geometry query failed ({e}); using {w}x{h}.")
            if hasattr(self, 'display_layouts'):
                self.display_layouts['primary'] = {'w': w, 'h': h, 'x': x, 'y': y}
        started = False
        try:
            started = bool(await self._start_capture_for_display(
                'primary', width=w, height=h, x_offset=x, y_offset=y))
        except Exception as e:
            data_logger.error(f"Viewer-driven capture start failed: {e}", exc_info=True)
        if started:
            await self._start_backpressure_task_if_needed('primary')
            # Built from session defaults: a client keyed to the departed
            # controller's encoder would otherwise drop every chunk.
            await self._broadcast_live_server_settings('primary')
            # The audio fan-out is shared, so a lone viewer must not wait for a controller either.
            if PCMFLUX_AVAILABLE and settings.audio_enabled[0] and not self.is_pcmflux_capturing:
                try:
                    async with self._reconfigure_guard():
                        await self._start_pcmflux_pipeline()
                except Exception as e:
                    data_logger.error(f"Viewer-driven audio start failed: {e}", exc_info=True)
        return started

    async def _start_capture_for_display(self, display_id: str, width: int, height: int,
                                         x_offset: int, y_offset: int) -> bool:
        """Start (or confirm) one display's capture, serialized under _video_capture_lock.

        Also refreshes second-screen capacity afterwards: a capture start is
        what establishes the host session in host-capture mode, so the host's
        output count can first become known — or change — here.

        Returns:
            True when a live capture exists for the display afterwards.
        """
        async with self._video_capture_lock:
            started = await self._start_capture_for_display_impl(display_id, width, height, x_offset, y_offset)
        if started and await self._refresh_second_screen_capacity():
            await self._broadcast_live_server_settings(display_id)
        return started

    async def _start_capture_for_display_impl(self, display_id: str, width: int, height: int,
                                              x_offset: int, y_offset: int) -> bool:
        """Start a capture instance for one display region.

        Callers hold _video_capture_lock. Builds the CaptureSettings, installs
        the zero-copy frame callback (which fans chunks out to the per-client
        relays via call_soon_threadsafe) and the pixelflux cursor handler, and
        starts the persistent ScreenCapture module (reused across restarts so
        the encoder backend stays warm). A genuinely capturing existing
        instance is left alone (an IDR is nudged for rejoining clients); a
        stale one is rebuilt.

        Returns:
            True on success; False when the start failed (reported so callers
            do not ack a false VIDEO_STARTED).

        Raises:
            SelkiesAppError: When the pixelflux library is unavailable.
        """
        # Before CaptureSettings() dies on a bare TypeError far from the import warning.
        if not X11_CAPTURE_AVAILABLE:
            raise SelkiesAppError(
                "Cannot start capture: the pixelflux library failed to import "
                "(see the startup warning for the underlying error)."
            )
        existing = self.capture_instances.get(display_id)
        if existing is not None:
            module = existing.get('module')
            alive = True
            if module is not None:
                try:
                    alive = bool(module.is_capturing)
                except Exception:
                    # Unknown state: assumed alive rather than churn a healthy stream.
                    alive = True
            if alive:
                if module is not None:
                    try:
                        module.request_idr_frame()
                    except Exception:
                        pass
                data_logger.info(f"Capture instance for '{display_id}' already running; requested IDR.")
                return True
            data_logger.warning(f"Capture instance for '{display_id}' is stale (not capturing); rebuilding.")
            await self._stop_capture_for_display_impl(display_id)

        data_logger.info(
            f"Preparing to start capture for display='{display_id}': "
            f"Res={width}x{height}, Offset={x_offset}x{y_offset}"
        )

        try:
            settings = self._get_capture_settings(display_id, width, height, x_offset, y_offset)

            # Fallback for relays created before capture_instances registers this display.
            relay_budget = max(
                VIDEO_RELAY_BUDGET_MIN_BYTES,
                int(int(getattr(settings, 'video_bitrate_kbps', 0) or 0)
                    * 125 * VIDEO_RELAY_BUDGET_SECONDS),
            )

            def queue_data_for_display(frame):
                """pixelflux frame callback, on its native thread.

                Wraps the frame zero-copy and hands the fan-out to the event
                loop. The frame owns its native buffer (every pixelflux mode
                emits its wire header natively, JPEG's 0x03 included) and is
                kept as `owner` behind the memoryview because aiohttp may
                retain a view past send_bytes; every relay backlog shares the
                one item, and the buffer frees when the last holder releases
                it. An oversized chunk is an upstream bug and is refused, as
                emitting it would trip proxy/WS-stack frame limits.
                """
                if frame is None:
                    return
                try:
                    if not len(frame):
                        return
                    if len(frame) > WS_MESSAGE_SIZE_HARD_CAP:
                        data_logger.error(
                            f"Refusing to relay a {len(frame)}-byte video chunk "
                            f"(hard cap {WS_MESSAGE_SIZE_HARD_CAP} bytes); chunk dropped.")
                        return

                    item = {'data': memoryview(frame), 'owner': frame,
                            # Only the low 16 bits go on the wire and come back in
                            # ACKs; masked here so RTT lookups match past frame 65535.
                            'frame_id': frame.frame_id & 0xFFFF}

                    def do_fanout():
                        """Offer the chunk to each target socket's relay, on the loop."""
                        group = self.video_relay_groups.get(display_id)
                        # No group means the capture is stopping; the buffer frees with the frame.
                        if group is None:
                            return
                        pc_ws = None
                        if display_id == 'primary':
                            secondary_ws = {
                                ci.get('ws')
                                for did, ci in self.display_clients.items()
                                if did != 'primary' and ci.get('ws')
                            }
                            targets = (self.clients - secondary_ws
                                       - self.video_paused_clients)
                            keep = set(targets)
                            ps = self.display_clients.get('primary')
                            pc_ws = ps.get('ws') if ps else None
                            if (pc_ws is not None and pc_ws in targets
                                    and not ps.get('backpressure_enabled', True)):
                                # ACK backpressure throttles the controller only; its relay
                                # stays warm but gated, resuming at the IDR the lift requests.
                                targets.discard(pc_ws)
                                relay = group.get(pc_ws)
                                if relay is not None:
                                    relay.flush_for_gate()
                        else:
                            ci = self.display_clients.get(display_id)
                            ws = ci.get('ws') if ci else None
                            keep = {ws} if ws is not None else set()
                            if ws is not None and ci.get('backpressure_enabled', True):
                                targets = {ws}
                            else:
                                targets = set()
                                relay = group.get(ws) if ws is not None else None
                                if relay is not None:
                                    relay.flush_for_gate()
                        # A socket gone for good (disconnect, pause, demotion to
                        # secondary) takes its relay with it; gated sockets stay in keep.
                        if len(group) > len(keep):
                            for ws in [w for w in group if w not in keep]:
                                group.pop(ws).stop()
                        need_sync = False
                        for ws in targets:
                            relay = group.get(ws)
                            if relay is None:
                                relay = _VideoRelay(
                                    self, display_id, ws,
                                    self._video_relay_budget(display_id, relay_budget))
                                group[ws] = relay
                                relay.start()
                            if relay.offer(item):
                                need_sync = True
                        if need_sync:
                            self._schedule_idr_for_display(display_id)

                    self.capture_loop.call_soon_threadsafe(do_fanout)

                except Exception as e:
                    data_logger.error(f"Error in capture callback for {display_id}: {e}", exc_info=False)

            def pixelflux_cursor_handler(msg_type, data_bytes, hot_x, hot_y):
                try:
                    # An auto cursor_size is None; the formatter needs a fallback
                    # dimension, the same 24 the WebRTC handler uses.
                    size = int(self.cursor_size or 0)
                    payload = format_pixelflux_cursor(
                        msg_type, data_bytes, hot_x, hot_y, size if size > 0 else 24)
                    if payload is not None:
                        self.app.send_ws_cursor_data(payload)
                except Exception as e:
                    data_logger.error(f"Error handling pixelflux cursor: {e}")

            self.video_relay_groups[display_id] = {}
            data_logger.info(
                f"Video relays for '{display_id}': skip-ahead budget "
                f"{relay_budget} bytes/client.")


            capture_module = self._persistent_capture_modules.get(display_id)
            if capture_module is None:
                capture_module = ScreenCapture()
                self._persistent_capture_modules[display_id] = capture_module
            else:
                data_logger.info(
                    f"Reusing ScreenCapture instance for '{display_id}' (backend kept warm)."
                )

            # pixelflux is the cursor source on both backends (an older pixelflux
            # stashes this harmlessly and the python monitor keeps delivering).
            capture_module.set_cursor_callback(pixelflux_cursor_handler)

            await self.capture_loop.run_in_executor(
                None,
                capture_module.start_capture,
                queue_data_for_display,
                settings
            )

            self.capture_instances[display_id] = {
                'module': capture_module,
                'callback': queue_data_for_display,
                'settings': settings,
            }
            # The X11 start already raised on failure; a Wayland start only
            # enqueues a command, so its outcome is read back here.
            live, last_error = await self._wayland_start_verdict(capture_module, display_id)
            if not live:
                data_logger.error(
                    f"Capture did not start for '{display_id}': "
                    f"{last_error or 'the compositor reported no live pipeline'}.")
                self._close_video_relays(display_id)
                self.capture_instances.pop(display_id, None)
                return False
            if last_error:
                data_logger.warning(
                    f"Capture started for '{display_id}' with a caveat: {last_error}")
            data_logger.info(f"SUCCESS: Capture started for '{display_id}'.")
            return True

        except Exception as e:
            data_logger.error(f"Failed to start capture for '{display_id}': {e}", exc_info=True)
            self._close_video_relays(display_id)
            return False

    async def _wayland_start_verdict(self, module: Any, display_id: str) -> Tuple[bool, Optional[str]]:
        """Read the truthful outcome of a Wayland capture start.

        The compositor processes StartCapture asynchronously, so a fresh start's
        real result is not known when ``start_capture`` returns. ``get_realized_geometry``
        is answered only once the queued start ran, so it doubles as a barrier that
        makes ``is_capturing`` and ``capture_state`` authoritative; ``capture_state``
        then reports whether a live pipeline exists and, if it degraded or failed, why.

        Non-Wayland or an older pixelflux without the readback returns ``(True, None)``
        -- the X11 path already surfaces its failures by raising.

        Returns:
            ``(is_live, last_error)``: whether a live capture exists, and the reason a
            start failed or a caveat a degraded-but-live start came up with.
        """
        if not IS_WAYLAND or not hasattr(module, 'get_realized_geometry'):
            return True, None
        try:
            await asyncio.to_thread(module.get_realized_geometry, wayland_output_id(display_id))
        except Exception as e:
            data_logger.warning(f"Wayland start barrier failed for '{display_id}': {e}")
        last_error = None
        state_getter = getattr(module, 'capture_state', None)
        if state_getter is not None:
            try:
                _state, last_error = await asyncio.to_thread(
                    state_getter, wayland_output_id(display_id))
            except Exception:
                last_error = None
        live = False
        try:
            live = bool(module.is_capturing)
        except Exception:
            live = False
        return live, last_error

    def _wayland_capture_last_error(self, module: Any, display_id: str) -> Optional[str]:
        """The reason a Wayland capture failed, or a caveat a live one came up with, or None.

        Read straight from ``capture_state`` (no command round-trip); the caller is
        responsible for any ordering barrier. Returns None on an older pixelflux that
        does not expose the outcome.
        """
        getter = getattr(module, 'capture_state', None) if module is not None else None
        if getter is None:
            return None
        try:
            _state, last_error = getter(wayland_output_id(display_id))
            return last_error
        except Exception:
            return None

    def _get_capture_settings(self, display_id: str, width: int, height: int,
                              x: int, y: int) -> Any:
        """Build a pixelflux CaptureSettings for a specific display region.

        Per-display stored tunables win; each falls back to its session
        default, which is what a viewer-driven primary capture (no
        display-owning client) runs on entirely.

        Returns:
            A populated pixelflux CaptureSettings (typed Any because pixelflux
            is an optional import).

        Raises:
            SelkiesAppError: For an unknown non-primary display_id.
        """
        display_state = self.display_clients.get(display_id)
        if not display_state:
            if display_id == 'primary':
                display_state = {}
            else:
                raise SelkiesAppError(f"Cannot get capture settings for unknown display_id '{display_id}'")

        cs = CaptureSettings()
        cs.capture_width = width
        cs.capture_height = height
        cs.capture_x = x
        cs.capture_y = y
        encoder = display_state.get('encoder', self.app.encoder)
        if encoder == "jpeg":
            cs.output_mode = 0
            cs.jpeg_quality = display_state.get('jpeg_quality', self._initial_jpeg_quality)
            cs.paint_over_jpeg_quality = display_state.get('paint_over_jpeg_quality', self._initial_paint_over_jpeg_quality)
        else:
            cs.output_mode = 1
        ih = getattr(self, 'input_handler', None)
        apply_common_capture_settings(
            cs, self.cli_args,
            is_wayland=IS_WAYLAND,
            display_name=display_id,
            scale=display_state.get('scale', 1.0),
            framerate=display_state.get('framerate', self.app.framerate),
            encoder=encoder,
            use_cpu=display_state.get(
                'use_cpu', effective_use_cpu(encoder, None, self._initial_use_cpu)),
            cbr=display_state.get('rate_control_mode', self.rc_mode.value) == 'cbr',
            bitrate_kbps=display_state.get('video_bitrate', self._initial_video_bitrate),
            crf=display_state.get('video_crf', self._initial_video_crf),
            paintover_crf=display_state.get('video_paintover_crf', self._initial_video_paintover_crf),
            paintover_burst=display_state.get('video_paintover_burst_frames', self._initial_video_paintover_burst_frames),
            fullcolor=display_state.get('video_fullcolor', self._initial_video_fullcolor),
            streaming=display_state.get('video_streaming_mode', self._initial_video_streaming_mode),
            use_paint_over_quality=display_state.get('use_paint_over_quality', self._initial_use_paint_over_quality),
            capture_cursor=self.capture_cursor,
            cursor_size_cap_hint=int(getattr(ih, 'cursor_size_cap', 0) or 0),
        )
        return cs
    
    async def run(self) -> None:
        """Start the server's components and block until shutdown is signaled.

        Spawns the input handler's connect/clipboard/cursor tasks, applies the
        configured startup DPI and cursor size, then waits on shutdown_event;
        cleanup always runs via shutdown() on the way out.
        """
        self._shutdown_called = False
        self.initialize()

        logger.info("Starting DataStreamingServer...")
        
        self._tasks_to_run = []
        if hasattr(self.input_handler, "connect"):
            self._tasks_to_run.append(
                asyncio.create_task(self.input_handler.connect(), name="InputConnect")
            )
        if hasattr(self.input_handler, "start_clipboard"):
            self.input_handler.clipboard_monitor_task = asyncio.create_task(
                self.input_handler.start_clipboard(), name="ClipboardMon"
            )
            self._tasks_to_run.append(self.input_handler.clipboard_monitor_task)
        if hasattr(self.input_handler, "start_cursor_monitor"):
            self._tasks_to_run.append(
                asyncio.create_task(self.input_handler.start_cursor_monitor(), name="CursorMon")
            )
        if hasattr(self.input_handler, "probe_apps_runner"):
            self._tasks_to_run.append(
                asyncio.create_task(self.input_handler.probe_apps_runner(), name="AppsProbe")
            )

        # 96 is unity: no churn when nothing diverges.
        startup_dpi = int(float(getattr(settings, "scaling_dpi", "96") or 96))
        if startup_dpi != 96:
            if IS_WAYLAND:
                if self.input_handler is not None:
                    await self.input_handler.realize_wayland_dpi(startup_dpi)
            else:
                await set_dpi(startup_dpi)

        # The Wayland compositor gets its cursor size via CaptureSettings instead.
        if not IS_WAYLAND and settings.cursor_size > 0:
            await set_cursor_size(cursor_size_for_dpi(int(settings.scaling_dpi), CURSOR_SIZE))

        try:
            await self.shutdown_event.wait()
        except asyncio.CancelledError:
            logger.info("Main application task was cancelled.")
        except Exception as e_main:
            logger.critical(f"Critical error in main execution: {e_main}", exc_info=True)
        finally:
            logger.info("Main loop ending or interrupted. Performing cleanup...")
            await self.shutdown()

    async def shutdown(self) -> None:
        """Shut down all components and release resources; idempotent.

        Closes every client socket first (code 4000) so handlers exit and no
        stray capture keeps encoding for a page that can no longer receive,
        then stops pipelines while display state still exists to address
        their tasks, cancels auxiliary tasks, stops the input handler, drops
        the persistent capture modules, and unregisters the registry-global
        Prometheus gauges (re-entering this mode after a switch would
        otherwise fail on duplicated timeseries). The close carries no KILL
        verb: KILL is the client's terminal verdict (it clears the reconnect
        timer and drops its onclose handler), whereas a shutdown is usually a
        mode switch every page must recover from — a bare close leaves the
        client's reconnect/mode-flip loop armed, which converges the tabs.
        """
        if self._shutdown_called:
            logger.info("Shutdown already called, skipping")
            return
        self._shutdown_called = True
        logger.info("DataStreamingServer shutdown initiated...")

        sockets_to_close = set(self.clients)
        for info in self.display_clients.values():
            ws = info.get('ws')
            if ws is not None:
                sockets_to_close.add(ws)

        async def _close_one(sock):
            try:
                await asyncio.wait_for(sock.close(code=4000, message=b"server shutting down"), timeout=1.0)
            except Exception:
                _close_abandoned_ws(sock)

        if sockets_to_close:
            await asyncio.gather(
                *[_close_one(s) for s in sockets_to_close], return_exceptions=True
            )

        try:
            await self.shutdown_pipelines()
        except Exception as e:
            logger.error(f"Pipeline shutdown during server shutdown failed: {e}")

        self.clients.clear()
        self.video_paused_clients.clear()
        self._report_client_presence()
        self.display_clients.clear()

        all_tasks_for_cleanup = [
            t for t in self._tasks_to_run
            if t and not t.done()
        ]

        for task in all_tasks_for_cleanup:
            logger.debug(f"Cancelling task: {task.get_name()}")
            task.cancel()

        if all_tasks_for_cleanup:
            await asyncio.gather(*all_tasks_for_cleanup, return_exceptions=True)
            logger.info("Auxiliary tasks cancellation complete.")

        if self.input_handler:
            logger.info("Stopping InputHandler components...")
            if hasattr(self.input_handler, "stop_clipboard"):
                self.input_handler.stop_clipboard()
            if hasattr(self.input_handler, "stop_cursor_monitor"):
                self.input_handler.stop_cursor_monitor()
            if hasattr(self.input_handler, "disconnect") and inspect.iscoroutinefunction(
                self.input_handler.disconnect
            ):
                await self.input_handler.disconnect()

        self._persistent_capture_modules.clear()

        if self.metrics:
            try:
                await asyncio.to_thread(self.metrics.unregister)
            except Exception as e:
                logger.exception(f"Error unregistering metrics: {e}")
            self.metrics = None

        self.app = None
        self.input_handler = None
        logger.info("DataStreamingServer shutdown complete.")

    async def start(self) -> None:
        self.shutdown_event.clear()
        await self.run()

    async def stop(self) -> None:
        self.shutdown_event.set()

    def register_routes(self, api_prefix: str, main_router: web.UrlDispatcher) -> None:
        """Register the data WebSocket and token endpoints on the shared router.

        Both live under /api so ONE nginx `location /api` (with the WebSocket
        upgrade) fronts every dynamic path — control endpoints, this data
        socket, and the WebRTC signaling socket alike; everything the browser
        needs is proxied through /api.
        """
        main_router.add_get(f'{api_prefix}/api/websockets{{slash:/?}}', self.data_ws_handler)
        main_router.add_post(f'{api_prefix}/api/tokens', self.handle_tokens)

    async def handle_tokens(self, request: web.Request) -> web.StreamResponse:
        """Accept a full replacement of the session's token/permission table.

        Provisioning is transport-independent: user_tokens/active_mk_token
        govern authority for both the websockets and WebRTC gates, so tokens
        are accepted in any active mode (unlike the data WS endpoint, which is
        mode-gated). Secure mode is read from settings.master_token, not
        self.is_secure_mode, which is only set once the websockets service's
        initialize() runs (never in WebRTC mode). Opens the config gate on
        first provision and reconciles live clients against the new table.
        """
        if not settings.master_token:
            return web.json_response({"error": "Server not in secure mode"}, status=404)

        global user_tokens, active_mk_token
        try:
            new_token_data = await request.json()
            if not isinstance(new_token_data, dict): raise ValueError("Payload must be a JSON object")
            # The whole payload is validated before global auth state changes.
            for tkn, perms in new_token_data.items():
                if not isinstance(perms, dict):
                    raise ValueError(f"Token entry for {tkn!r} must be a JSON object")
        except (json.JSONDecodeError, ValueError) as e:
            return web.Response(status=400, text=f"Bad Request: {e}")

        new_mk_owner = None
        for tkn, perms in new_token_data.items():
            if perms.get("mk_control", False):
                new_mk_owner = tkn
                break
        user_tokens = new_token_data
        active_mk_token = new_mk_owner
        logger.info(f"Updated user tokens. Now tracking {len(user_tokens)} tokens.")
        if not self.config_gate.is_set():
            self.config_gate.set()
            logger.info("Configuration gate is now open. WebSocket server will accept connections.")
        _spawn_background_task(reconcile_clients())
        return web.Response(status=200, text="OK")

    async def data_ws_handler(self, request: web.Request) -> web.StreamResponse:
        """aiohttp entry point: upgrade to a WebSocket and hand off to ws_handler.

        Refuses when the websockets transport is not the active mode. A
        view-only basic-auth credential caps the role at viewer no matter what
        the query string asks for (legacy, non-secure mode); secure mode leaves
        the ceiling unset and lets the token govern.
        """
        if self.supervisor.current_mode != self.mode:
            return web.Response(status=409, text="WebSocket mode is inactive")

        token = ""
        if self.cli_args.master_token:
            token = request.query.get('token') 
            if not token:
                return web.Response(status=401, text="Token missing in secure mode")

        # compress=False: the frames are already H.264/JPEG/Opus. heartbeat:
        # protocol pings reap a silently dead peer, as the signaling sockets' probes
        # do. autoping=False: the loop answers PING and feeds PONG to the uplink gauge.
        ws = web.WebSocketResponse(compress=False, max_msg_size=WS_MAX_MESSAGE_BYTES, heartbeat=30, autoping=False)
        await ws.prepare(request)

        peername = request.transport.get_extra_info('peername')
        remote_address = peername[:2] if peername else (request.remote, 0)
        query_role = request.query.get('role', '')
        query_slot = request.query.get('slot')
        if request.get("auth_role_ceiling") == "viewer":
            query_role = "viewer"
            query_slot = None
        try:
            await self.ws_handler(ws, remote_address, token, query_role=query_role, query_slot=query_slot)
        finally:
            self._report_client_presence()
        return ws

    def uplink_session_conns(self) -> list[tuple[Any, Optional[str], Optional[str]]]:
        """``(websocket, session token, peer ip)`` per connected data socket,
        for the supervisor's upload uplink gauge."""
        conns = []
        for ws in list(self.clients):
            perms = client_permissions.get(ws) or {}
            addr = perms.get("remote_address")
            conns.append((ws, perms.get("token"), addr[0] if addr else None))
        return conns


async def _collect_system_stats_ws(shared_data: dict, interval_seconds: float = 1) -> None:
    """Singleton collector: poll CPU/memory into the shared stats dict.

    One instance serves every connection's stats sender (per-connection
    collectors would mean N psutil polls per second).
    """
    data_logger.debug(
        f"System monitor loop (WS mode) started, interval: {interval_seconds}s"
    )
    try:
        while True:
            cpu = psutil.cpu_percent()
            mem = psutil.virtual_memory()
            shared_data["system"] = {
                "type": "system_stats",
                "timestamp": datetime.now().isoformat(),
                "cpu_percent": cpu,
                "mem_total": mem.total,
                "mem_used": mem.used,
            }
            await asyncio.sleep(interval_seconds)
    except asyncio.CancelledError:
        data_logger.info("System monitor (WS) cancelled.")
    except Exception as e:
        data_logger.error(f"System monitor (WS) error: {e}", exc_info=True)


async def _collect_gpu_stats_ws(
    shared_data: dict,
    gpu_id: int = 0,
    interval_seconds: float = 1,
    dri_node: str = "",
    metrics: Optional[Metrics] = None,
) -> None:
    """Singleton collector: poll the pipeline's GPU into the shared stats dict.

    Args:
        shared_data: The instance-wide stats dict the per-connection senders read.
        gpu_id: Index into the unfiltered GPU list.
        interval_seconds: Poll interval.
        dri_node: When set and it filters to exactly one GPU, that GPU wins
            over the index — stats must describe the GPU the pipeline
            captures/encodes on.
        metrics: Optional Prometheus gauges fed alongside the dict.
    """
    data_logger.debug(
        f"GPU monitor loop (WS mode) for GPU {gpu_id} (node {dri_node or 'any'}), "
        f"interval: {interval_seconds}s"
    )
    def _pick(gpus):
        """The pipeline's GPU: a dri_node match is exactly it; the index applies
        only to the unfiltered list."""
        idx = 0 if (dri_node and len(gpus) == 1) else gpu_id
        return gpus[idx] if 0 <= idx < len(gpus) else None

    try:
        # get_gpus() may spawn or block on vendor tools.
        gpus = await asyncio.to_thread(gpu_stats.get_gpus, dri_node)
        if not gpus:
            data_logger.warning("No GPUs detected for GPU monitor (WS).")
            return
        if _pick(gpus) is None:
            data_logger.error(f"Invalid GPU ID {gpu_id} for GPU monitor (WS).")
            return

        while True:
            try:
                gpus = await asyncio.to_thread(gpu_stats.get_gpus, dri_node)
                gpu = _pick(gpus) if gpus else None
                if gpu is None:
                    data_logger.error(f"GPU {gpu_id} no longer available.")
                    break
                shared_data["gpu"] = {
                    "type": "gpu_stats",
                    "timestamp": datetime.now().isoformat(),
                    "gpu_id": gpu_id,
                    "load": gpu.load,
                    # Dashboards read gpu_percent (0..100), the field the WebRTC data
                    # channel sends; load stays the 0..1 fraction for other consumers.
                    "gpu_percent": gpu.load * 100,
                    "memory_total": gpu.memoryTotal * 1024 * 1024,
                    "memory_used": gpu.memoryUsed * 1024 * 1024,
                }
                if metrics is not None:
                    metrics.set_gpu_utilization(gpu.load * 100)
            except asyncio.CancelledError:
                raise
            except Exception as e_gpu_stat:
                data_logger.error(
                    f"GPU monitor (WS): Error getting stats for ID {gpu_id}: {e_gpu_stat}"
                )
                await asyncio.sleep(interval_seconds * 2)
            await asyncio.sleep(interval_seconds)
    except asyncio.CancelledError:
        data_logger.info("GPU monitor (WS) cancelled.")
    except Exception as e:
        data_logger.error(f"GPU monitor (WS) error: {e}", exc_info=True)

async def _collect_network_stats_ws(shared_data: dict, server_instance: DataStreamingServer,
                                    interval_seconds: float = 2) -> None:
    """Singleton collector: derive sent-bandwidth and smoothed latency.

    Must be the single instance-wide task: it consumes and resets the server's
    _bytes_sent_in_interval counter, so per-connection copies would race it.
    """
    data_logger.debug(
        f"Network monitor loop (WS mode) started, interval: {interval_seconds}s"
    )
    try:
        while True:
            await asyncio.sleep(interval_seconds)
            current_time = time.monotonic()
            elapsed_time = current_time - server_instance._last_bandwidth_calc_time
            if elapsed_time > 0:
                current_mbps = (server_instance._bytes_sent_in_interval * 8) / elapsed_time / 1_000_000
            else:
                current_mbps = 0.0
            server_instance._bytes_sent_in_interval = 0
            server_instance._last_bandwidth_calc_time = current_time
            
            primary_client = server_instance.display_clients.get('primary')
            latency_ms = primary_client.get('smoothed_rtt', 0.0) if primary_client else 0.0

            shared_data["network"] = {
                "type": "network_stats",
                "timestamp": datetime.now().isoformat(),
                "bandwidth_mbps": round(current_mbps, 2),
                "latency_ms": round(latency_ms, 1),
            }
    except asyncio.CancelledError:
        data_logger.info("Network monitor (WS) cancelled.")
    except Exception as e:
        data_logger.error(f"Network monitor (WS) error: {e}", exc_info=True)

async def _send_stats_periodically_ws(
    websocket: web.WebSocketResponse,
    shared_data: dict,
    server_instance: DataStreamingServer,
    interval_seconds: float = 5,
) -> None:
    """Per-connection sender: push the singleton collectors' stats to one socket.

    Reads (never pops) the shared dicts, since many per-connection senders
    share the same collectors; ends itself when the socket dies.
    """
    try:
        while True:
            await asyncio.sleep(interval_seconds)
            system_stats = shared_data.get("system")
            gpu_stats = shared_data.get("gpu")
            network_stats = server_instance._shared_network_stats.get("network")
            try:
                if not websocket:
                    data_logger.info("Stats sender: WS closed or invalid.")
                    break
                if system_stats:
                    await websocket.send_str(json.dumps(system_stats))
                if gpu_stats:
                    await websocket.send_str(json.dumps(gpu_stats))
                if network_stats:
                    await websocket.send_str(json.dumps(network_stats))
            except (ConnectionResetError, OSError, RuntimeError):
                data_logger.info("Stats sender: WS connection closed.")
                break
            except Exception as e_send:
                data_logger.error(f"Stats sender: Error sending: {e_send}")
    except asyncio.CancelledError:
        data_logger.info("Stats sender (WS) cancelled.")
    except Exception as e:
        data_logger.error(f"Stats sender (WS) error: {e}", exc_info=True)

async def on_resize_handler(
    res_str: str,
    current_app_instance: SelkiesStreamingApp,
    data_server_instance: Optional[DataStreamingServer] = None,
    display_id: str = 'primary',
) -> None:
    """Handle a client resize request for one display.

    Honors the enable_resize gate (primary only — a secondary's resize is its
    layout bring-up and must stay allowed, WebRTC parity) and the server's
    manual-resolution override, applies 16-pixel alignment when the display
    asked for it, then either updates a lone Wayland display's capture in place
    (with a realized-geometry read-back) or triggers a full layout
    reconfiguration.

    Args:
        res_str: The requested `{width}x{height}` string.
        current_app_instance: The shared app whose primary geometry mirrors the
            display state.
        data_server_instance: The owning server; without it only the gate
            checks run.
        display_id: The display being resized.
    """
    logger_app_resize.info(f"on_resize_handler for display '{display_id}' with resolution: {res_str}")
    if (display_id == 'primary'
            and not getattr(current_app_instance, 'server_enable_resize', True)):
        logger_app_resize.warning(f"Primary resize to {res_str} ignored: dynamic resizing disabled.")
        return
    if data_server_instance:
        server_is_manual, _ = data_server_instance.cli_args.is_manual_resolution_mode
        if server_is_manual:
            logger_app_resize.warning(
                f"Client attempted to resize to {res_str} but server is in manual resolution mode. Request ignored."
            )
            return
    try:
        dims = parse_resize_dims(res_str)
        if dims is None:
            logger_app_resize.error(f"Invalid resize request: {res_str}. Ignoring.")
            return
        target_w, target_h = dims

        if data_server_instance and display_id in data_server_instance.display_clients:
            client_info = data_server_instance.display_clients[display_id]
            if client_info.get('force_aligned_resolution'):
                aligned_w, aligned_h = align_dims_16(target_w, target_h)
                if aligned_w != target_w or aligned_h != target_h:
                    logger_app_resize.info(
                        f"Aligning resize request for '{display_id}' from {target_w}x{target_h} to {aligned_w}x{aligned_h} (16-pixel alignment)."
                    )
                target_w, target_h = aligned_w, aligned_h
            if client_info.get('width') == target_w and client_info.get('height') == target_h:
                logger_app_resize.info(f"Redundant resize request for {display_id} to {target_w}x{target_h}. No action.")
                return

            client_info['width'] = target_w
            client_info['height'] = target_h
            
            if display_id == 'primary':
                current_app_instance.display_width = target_w
                current_app_instance.display_height = target_h

            if IS_WAYLAND and (display_id != 'primary'
                               or len(data_server_instance.display_clients) > 1):
                # An extended layout's union arrangement changes (a primary resize
                # moves the secondary's offset), so the full pass runs.
                logger_app_resize.info(
                    f"Wayland Resize: '{display_id}' to {target_w}x{target_h} via layout reconfiguration."
                )
                await data_server_instance.reconfigure_displays()
            elif IS_WAYLAND:
                logger_app_resize.info(f"Wayland Resize: Updating {display_id} to {target_w}x{target_h}.")
                if display_id in data_server_instance.display_layouts:
                    data_server_instance.display_layouts[display_id]['w'] = target_w
                    data_server_instance.display_layouts[display_id]['h'] = target_h

                # A start on the live capture resizes it in place (pixelflux keeps
                # a compatible encoder session); stop+start only when it isn't running.
                inst = data_server_instance.capture_instances.get(display_id)
                module = inst.get('module') if inst else None
                alive = False
                if module is not None:
                    try:
                        alive = bool(module.is_capturing)
                    except Exception:
                        alive = False
                if alive and inst.get('callback'):
                    settings = data_server_instance._get_capture_settings(display_id, target_w, target_h, 0, 0)
                    # An in-place restart racing a teardown corrupts the instance.
                    async with data_server_instance._video_capture_lock:
                        await asyncio.to_thread(module.start_capture, inst['callback'], settings)
                    inst['settings'] = settings
                else:
                    await data_server_instance._stop_capture_for_display(display_id)
                    await data_server_instance._start_capture_for_display(display_id, target_w, target_h, 0, 0)
                await data_server_instance._sync_wayland_realized_geometry(display_id)
            else:
                logger_app_resize.info(f"Display client '{display_id}' dimensions updated to {target_w}x{target_h}. Triggering reconfiguration.")
                await data_server_instance.reconfigure_displays()
        else:
            logger_app_resize.error(f"Cannot resize: display_id '{display_id}' not found in connected clients.")
    except ValueError:
        logger_app_resize.error(f"Invalid resolution format in resize request: {res_str}")
    except Exception as e:
        logger_app_resize.error(f"Error during resize handling for '{res_str}': {e}", exc_info=True)

async def reconcile_clients() -> None:
    """Reconcile live connections against the current token table.

    Disconnects clients whose token was revoked or whose role changed, pushes
    ROLE_UPDATE for slot-only changes, and re-announces MK_ACCESS to every
    surviving tokened client (with a cursor resend for the new holder). Ends by
    invoking the WebRTC reconcile hook so live WebRTC peers get the same
    treatment — this function itself only walks websockets sockets.
    """
    global user_tokens, client_permissions
    connected_websockets = list(client_permissions.keys())
    current_tokens = user_tokens.copy()
    for ws in connected_websockets:
        if ws.closed:
            continue
        perms = client_permissions.get(ws)
        if not perms or perms.get("token") is None:
            continue
        token = perms["token"]
        remote_address = perms.get('remote_address', 'unknown')
        new_perms = current_tokens.get(token)
        should_disconnect = False
        reason = ""
        if not new_perms:
            should_disconnect, reason = True, "Token revoked"
        else:
            old_role, old_slot = perms.get("role"), perms.get("slot")
            new_role = new_perms.get("role")
            new_slot = new_perms.get("slot")

            if old_role != new_role:
                should_disconnect, reason = True, "Permissions changed significantly"
            
            elif old_slot != new_slot:
                data_logger.info(f"Updating client {remote_address} for slot change: {old_slot} -> {new_slot}")
                update_payload = json.dumps({"role": new_role, "slot": new_slot})
                update_message = f"ROLE_UPDATE,{update_payload}"
                try:
                    await ws.send_str(update_message)
                    client_permissions[ws]['role'] = new_role
                    client_permissions[ws]['slot'] = new_slot
                except (ConnectionResetError, OSError, RuntimeError):
                    data_logger.warning(f"Could not send role update to {remote_address}, connection closed.")
        if should_disconnect:
            data_logger.info(f"Disconnecting client {remote_address} due to: {reason}")
            try:
                await ws.close(code=4002, message=reason.encode())
            except (ConnectionResetError, OSError, RuntimeError):
                pass
            # new_perms is None for a revoked token.
            continue
        has_mk_access = _mk_access_verdict(new_perms, token=token)
        mk_msg = "MK_ACCESS,1" if has_mk_access else "MK_ACCESS,0"
        try:
            await ws.send_str(mk_msg)
            if has_mk_access:
                data_server = perms.get("data_server")
                if data_server:
                    await data_server.send_current_cursor(ws, remote_address)
        except (ConnectionResetError, OSError, RuntimeError):
            pass
    if webrtc_reconcile_hook is not None:
        try:
            await webrtc_reconcile_hook()
        except Exception:
            data_logger.exception("WebRTC peer reconcile failed")
