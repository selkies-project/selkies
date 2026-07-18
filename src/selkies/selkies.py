# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
import aiofiles
import asyncio
import base64
import contextlib
import gzip
import json
import logging
import os
import re
import shlex
import time
from asyncio import subprocess
from collections import OrderedDict, deque
from datetime import datetime
from enum import Enum
from shutil import which
from types import SimpleNamespace

import psutil
from aiohttp import web, WSMsgType

from . import audio_config
from . import gpu_stats
from .display_utils import (
    parse_dri_node_to_index,
    parse_gpu_id,
    format_pixelflux_cursor,
    get_new_res,
    generate_xrandr_gtf_modeline,
    ensure_mode,
    resize_display,
    clear_selkies_monitors,
    replace_selkies_monitors,
    current_wm_name,
    wait_for_wm,
    wayland_output_id,
    wayland_reposition_primary,
    grow_framebuffer,
    set_dpi,
    set_cursor_size,
    clamp_primary_feedback,
    parse_resize_dims,
    cursor_size_for_dpi,
    align_dims_16,
)
from .input_handler import (
    WebRTCInput as InputHandler,
    CLIPBOARD_CHUNK_SIZE,
    VIEWER_ALLOWED_PREFIXES,
    VIEWER_COLLAB_EXTRA_PREFIXES,
    VIEWER_SILENT_DROP_PREFIXES,
)
from .settings import settings, SETTING_DEFINITIONS, WS_MAX_MESSAGE_BYTES, WS_MESSAGE_SIZE_HARD_CAP, build_client_settings_payload, inflate_gz_bounded, sanitize_client_setting
from .settings import settings as app_settings
from .stream_server import BaseStreamingService

# Constants
BACKPRESSURE_ALLOWED_DESYNC_MS = 2000
BACKPRESSURE_LATENCY_THRESHOLD_MS = 50
# Ceiling on the RTT-based desync forgiveness. RTT is measured through the same
# send->ack path backpressure bounds, so a growing frame queue inflates the very
# term that loosens the trigger — unbounded, that feedback loop lets the server
# run tens of seconds ahead of the client. Real propagation delay worth forgiving
# is under a second; anything above is self-inflicted queue delay.
BACKPRESSURE_LATENCY_FORGIVENESS_MAX_MS = 1000
# Discard ack round-trip samples above this: they measure a stalled/backlogged
# path (or an id collision), not the link, and one such sample skews the flat
# smoothing window for its whole lifetime.
RTT_SAMPLE_SANE_MAX_MS = 10000
BACKPRESSURE_CHECK_INTERVAL_S = 0.5
MAX_UINT16_FRAME_ID = 65535
FRAME_ID_SUSPICIOUS_GAP_THRESHOLD = (
    MAX_UINT16_FRAME_ID // 2
)
STALLED_CLIENT_TIMEOUT_SECONDS = 4.0
# Per-client send bound for the shared audio/video fan-out (20ms/frame-cadence
# streams): a socket that cannot take a frame for this long is dropped so one
# black-holed client cannot freeze the broadcast; backlog stays under the
# BACKPRESSURE_QUEUE_SIZE the producers drop against.
SHARED_STREAM_SEND_TIMEOUT_SECONDS = 1.0
RTT_SMOOTHING_SAMPLES = 20
# RFC 2198 RED redundancy depth (distance=2) for the shared Opus audio stream.
AUDIO_RED_DISTANCE = 2
SENT_FRAME_TIMESTAMP_HISTORY_SIZE = 1000
# A resuming (unpausing) viewer bypasses the 30s START_VIDEO throttle, but not
# faster than this: each resume forces an IDR resync, so rapid STOP/START must
# not be usable to spam keyframes. Well below a human tab-switch cadence.
VIEWER_RESUME_MIN_INTERVAL_S = 1.0
TARGET_FRAMERATE = 60

UINPUT_MOUSE_SOCKET = ""
JS_SOCKET_PATH = "/tmp"
ENABLE_CLIPBOARD = True
ENABLE_BINARY_CLIPBOARD = False
ENABLE_CURSORS = True
DEBUG_CURSORS = False
ENABLE_RESIZE = bool(settings.enable_resize[0])
AUDIO_CHANNELS_DEFAULT = 2
AUDIO_BITRATE_DEFAULT = 320000
PIXELFLUX_VIDEO_ENCODERS = ["jpeg", "h264enc", "h264enc-striped", "openh264enc"]

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

# Backend switch from the settings pipeline (SELKIES_WAYLAND or --wayland).
# pixelflux never reads these itself:
# the choice reaches it per capture via the CaptureSettings use_wayland field.
IS_WAYLAND = bool(settings.wayland[0])

# Cursor base size in points at 96 DPI (DPI changes scale from it): the
# cursor_size setting (SELKIES_CURSOR_SIZE / XCURSOR_SIZE) when explicit,
# else the X11 default. The GPU-stats index likewise follows --gpu-id.
CURSOR_SIZE = settings.cursor_size if settings.cursor_size > 0 else 32
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
except (ImportError, RuntimeError):
    # ImportError = missing; RuntimeError = pcmflux ABI/version skew. Degrade
    # to "audio capture unavailable" rather than crash at startup.
    AudioCapture = AudioCaptureSettings = None
    AudioPlayback = AudioPlaybackSettings = None
    PCMFLUX_AVAILABLE = False
    PCMFLUX_PLAYBACK_AVAILABLE = False
    data_logger.warning("pcmflux library not found. Audio capture is unavailable.")

try:
    import pulsectl_asyncio

    PULSEAUDIO_AVAILABLE = True
except ImportError:
    PULSEAUDIO_AVAILABLE = False
    data_logger.warning(
        "pulsectl_asyncio not found. Microphone forwarding will be disabled."
    )

try:
    from pixelflux import CaptureSettings, ScreenCapture

    X11_CAPTURE_AVAILABLE = True
    data_logger.info("pixelflux library found. Striped encoding modes available.")
except (ImportError, RuntimeError) as e:
    # ImportError = missing; RuntimeError = pixelflux ABI/version skew. Degrade
    # to "striped encoding unavailable" rather than crash at startup.
    ScreenCapture = CaptureSettings = None
    X11_CAPTURE_AVAILABLE = False
    data_logger.warning(
        f"pixelflux library unavailable ({e}). Striped encoding modes unavailable."
    )

upload_path = str(getattr(settings, 'file_manager_path', '') or '~/Desktop')
upload_dir_path = os.path.expanduser(upload_path)

try:
    os.makedirs(upload_dir_path, exist_ok=True)
    logger.info(f"Upload directory ensured: {upload_dir_path}")
except OSError as e:
    logger.error(f"Could not create upload directory {upload_dir_path}: {e}")
    upload_dir_path = None

user_tokens = {}
client_permissions = {}
active_mk_token = None


def current_session_tokens():
    """Live control-plane token view — (user_tokens mapping, active mk-token) — as
    provisioned via /api/tokens. Both transports authorize input against this."""
    return user_tokens, active_mk_token


async def _poll_pa_object(list_coro, valid_names, poll_interval=0.1, timeout=2.0):
    """Poll a PulseAudio list coroutine (sink_list/source_list) until an object
    with one of valid_names appears (PipeWire creates them asynchronously)."""
    for _ in range(int(timeout / poll_interval)):
        for obj in await list_coro():
            if obj.name in valid_names:
                return obj
        await asyncio.sleep(poll_interval)
    return None


async def provision_virtual_microphone(pulse, audio_device_name, is_pcmflux_capturing):
    """Provision the SelkiesVirtualMic control plane shared by both transports.

    Creates the 'input'/'output' null sinks, loads module-virtual-source bridging
    input.monitor -> a recordable source, and forces the system default sink/source
    so an app recording the default source hears the client's forwarded mic. The
    PCM data plane (pcmflux AudioPlayback into the 'input' sink) belongs to the
    caller; this is the control plane only.

    Idempotent: an existing SelkiesVirtualMic is reused, so the websockets 0x02
    mic path and the WebRTC 'input' playback never double-load the module when both
    are live. Returns (module_index, owns_module); owns_module is True only when
    THIS call loaded the module, so a caller that merely reused an existing source
    never unloads it out from under the other transport on teardown. Returns
    (None, False) when the module load could not be verified.
    """
    virtual_source_name = "SelkiesVirtualMic"
    master_monitor = "input.monitor"
    # PipeWire prepends "output." to virtual sources
    valid_source_names = [virtual_source_name, f"output.{virtual_source_name}"]

    input_sink = "input"
    output_sink = audio_device_name.strip().split(".monitor")[0] if audio_device_name else "output"
    for sink_name in (input_sink, output_sink):
        sink_exists = any(s.name == sink_name for s in await pulse.sink_list())
        if not sink_exists:
            data_logger.info(f"Sink '{sink_name}' not found. Attempting to create...")
            await pulse.module_load("module-null-sink", f"sink_name={sink_name}")
            if await _poll_pa_object(pulse.sink_list, [sink_name]):
                data_logger.info(f"Successfully created and verified sink '{sink_name}'.")
            else:
                data_logger.error(f"Loaded module-null-sink for '{sink_name}' but it failed to appear in sink list.")
        else:
            data_logger.info(f"Sink '{sink_name}' already exists. Skipping creation.")

    try:
        await pulse.sink_default_set(output_sink)
        data_logger.info(f"Set system default sink to '{output_sink}'.")
    except Exception as e:
        data_logger.warning(f"Could not set default sink to '{output_sink}': {e}")

    target_device_names = [audio_device_name]
    if "auto_null.monitor" not in target_device_names:
        # Pipewire's default virtual sink is auto_null
        target_device_names.append("auto_null.monitor")

    existing_source_info = None
    for source_obj in await pulse.source_list():
        if source_obj.name in valid_source_names:
            existing_source_info = source_obj
            break

    module_index = None
    owns_module = False
    if existing_source_info:
        data_logger.info(
            f"Virtual source '{existing_source_info.name}' (Index: {existing_source_info.index}) already exists."
        )
        actual_master = existing_source_info.proplist.get("device.master_device")
        if actual_master == master_monitor:
            data_logger.info(f"Existing source correctly linked to '{master_monitor}'.")
        else:
            data_logger.warning(
                f"Existing source '{existing_source_info.name}' linked to '{actual_master}' not '{master_monitor}'. Manual fix may be needed."
            )
        module_index = existing_source_info.owner_module
        try:
            await pulse.source_default_set(existing_source_info.name)
        except Exception:
            pass
    else:
        data_logger.info(
            f"Virtual source '{virtual_source_name}' not found. Attempting to load module..."
        )
        load_args = f"source_name={virtual_source_name} master={master_monitor}"
        module_index = await pulse.module_load("module-virtual-source", load_args)
        owns_module = True
        data_logger.info(f"Loaded module-virtual-source with index {module_index} for '{virtual_source_name}'.")
        new_source_info = await _poll_pa_object(pulse.source_list, valid_source_names)
        if new_source_info:
            data_logger.info(
                f"Successfully verified creation of source '{new_source_info.name}' (Index: {new_source_info.index})."
            )
            # Force the system default source to SelkiesVirtualMic so apps record from it
            try:
                await pulse.source_default_set(new_source_info.name)
                data_logger.info(f"Set system default source to '{new_source_info.name}'.")
            except Exception as e:
                data_logger.warning(f"Could not set default source via pulsectl_asyncio: {e}")
        else:
            data_logger.error(
                f"Loaded module {module_index} but failed to find source '{virtual_source_name}'."
            )
            if module_index is not None:
                try:
                    await pulse.module_unload(module_index)
                except Exception as unload_err:
                    data_logger.error(f"Failed to unload module {module_index}: {unload_err}")
            return None, False

    if is_pcmflux_capturing:
        # pcmflux may attach to the wrong source; move it onto a valid capture target
        # so the encoded/forwarded audio graph stays intact.
        try:
            current_source_list = await pulse.source_list()
            source_outputs = await pulse.source_output_list()
            pcmflux_output = None
            for output in source_outputs:
                if hasattr(output, 'proplist') and output.proplist.get('application.name') == 'pcmflux':
                    pcmflux_output = output
                    break
            if pcmflux_output:
                connected_source = None
                for source in current_source_list:
                    if source.index == pcmflux_output.source:
                        connected_source = source
                        break
                if connected_source and connected_source.name not in target_device_names:
                    data_logger.warning(
                        f"pcmflux connected to wrong source '{connected_source.name}', looking for a valid target in {target_device_names}..."
                    )
                    correct_source = None
                    for source in current_source_list:
                        if source.name in target_device_names:
                            correct_source = source
                            break
                    if correct_source:
                        await pulse.source_output_move(pcmflux_output.index, correct_source.index)
                        data_logger.info(
                            f"Successfully moved pcmflux from '{connected_source.name}' to '{correct_source.name}'"
                        )
                    else:
                        data_logger.error(
                            f"Could not find any valid source {target_device_names} to move pcmflux to"
                        )
                elif connected_source:
                    data_logger.info(f"pcmflux correctly connected to '{connected_source.name}'")
            else:
                data_logger.debug("Could not find pcmflux in source outputs")
        except Exception as e:
            data_logger.error(f"Error checking/fixing pcmflux source: {e}")

    data_logger.info(f"Virtual microphone '{virtual_source_name}' is ready for microphone forwarding.")
    return module_index, owns_module


# Only WS control-text messages at least this large are gzip-wrapped (opcode 0x05)
# for gzip-capable clients. Below it, the compression saving doesn't pay for the CPU
# and — crucially — latency-critical small data (input, status verbs) is left raw.
# Matches the WebRTC DataChannel threshold so both transports behave identically.
WS_GZIP_MIN_BYTES = 512


def _path_is_within(directory, target):
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


def _close_abandoned_ws(client):
    """Close a dropped socket in the background: close() can itself block
    draining the same paused transport that stalled the send, so it must
    never run inline on a broadcast path."""
    async def _close():
        try:
            await asyncio.wait_for(client.close(), timeout=2.0)
        except Exception:
            pass
    asyncio.create_task(_close())


async def _broadcast_to_clients(clients, message, per_client_timeout=None):
    """Broadcast concurrently to all clients - only remove on clear connection errors.

    When per_client_timeout is set, a client whose send stalls past the bound is
    treated as dead: the send is cancelled and the socket is dropped and closed. A
    cancelled send_str may have left a half-written frame on the wire, so that socket
    must never be reused for later sends.

    Returns the set of clients dropped by this call. Removal mutates the PASSED
    collection, so callers that fan out over a computed temporary set (the media
    senders) must subtract the returned set from their authoritative registry
    themselves — otherwise the dead socket re-enters the very next per-frame set."""
    if not clients:
        return set()

    # Hard per-frame ceiling, both text and binary. Nothing legitimate reaches
    # it — large control payloads (clipboard) are segmented far below
    # WS_MAX_MESSAGE_BYTES before they get here — so an oversized message is an
    # upstream bug, and emitting it would trip proxy/WS-stack frame limits and
    # stall the socket. Refuse loudly instead of sending. (len() equals the
    # byte count for every large control message: they are base64/ASCII-JSON.)
    if len(message) > WS_MESSAGE_SIZE_HARD_CAP:
        data_logger.error(
            f"Refusing to broadcast a {len(message)}-byte WebSocket message "
            f"(hard cap {WS_MESSAGE_SIZE_HARD_CAP} bytes); message dropped."
        )
        return set()

    # The gzip frame is identical for every gzip-capable client, so compute it
    # at most once per broadcast instead of once per client (a large clipboard
    # payload compressed N times stalls the loop N times). The check-and-set is
    # synchronous — no await between the None test and the assignment — so
    # concurrent _send_one coroutines never double-compress.
    gz_frame_holder = []

    async def _send_one(client):
        if isinstance(message, (bytes, bytearray, memoryview)):
            # Binary control frames pass through untouched (media has its own
            # send path; this branch never carries the incompressible video/audio).
            await client.send_bytes(message)
        elif getattr(client, "_ws_gz", False) and len(message) >= WS_GZIP_MIN_BYTES:
            # Compressible control text (cursor PNGs, clipboard, stats) to a
            # gzip-capable client: send a 0x05-tagged gzip frame. Small,
            # latency-critical messages (input, status verbs) stay raw text —
            # they are below the threshold, so compression never touches them.
            if not gz_frame_holder:
                gz_frame_holder.append(b"\x05" + gzip.compress(message.encode("utf-8"), 6))
            await client.send_bytes(gz_frame_holder[0])
        else:
            await client.send_str(message)

    # Single-viewer fast path (the common case): skip the pair-list/gather/zip machinery,
    # using the same connection-error removal semantics as the multi-client path below.
    if len(clients) == 1:
        client = next(iter(clients))
        if client.closed:
            clients.discard(client)
            return {client}
        try:
            if per_client_timeout is not None:
                await asyncio.wait_for(_send_one(client), timeout=per_client_timeout)
            else:
                await _send_one(client)
        except asyncio.TimeoutError:
            # Stalled send was cancelled; the socket is no longer safe to reuse.
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

    for client in clients:
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
        if isinstance(result, Exception):
            # A stalled (cancelled) send may have corrupted the stream: drop + close.
            # Check TimeoutError first: on 3.11+ it is a subclass of OSError.
            if isinstance(result, asyncio.TimeoutError):
                timed_out_clients.add(client)
            # Only remove on clear connection closure errors
            elif isinstance(result, ConnectionResetError):
                closed_clients.add(client)
            # For OSError and RuntimeError, check if they're connection-related
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

class SelkiesAppError(Exception):
    pass

class RateControlMode(str, Enum):
    CBR = "cbr"
    CRF = "crf"

class SelkiesStreamingApp:
    def __init__(
        self,
        async_event_loop,
        framerate,
        encoder,
        data_streaming_server=None,
        mode="websockets",
    ):
        self.server_enable_resize = ENABLE_RESIZE
        self.mode = mode
        # Fallback geometry for capture paths that run before any client has
        # sized the display (viewer-driven capture on a controller-less server).
        # A configured manual resolution is the server's declared geometry, so
        # seed from it: on Wayland the fallback isn't cosmetic — the capture
        # start actively resizes the compositor output to these dimensions.
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
        # Honor the configured channel count (surround captures as multistream Opus).
        self.audio_channels = int(getattr(settings, 'audio_channels', AUDIO_CHANNELS_DEFAULT)
                                  or AUDIO_CHANNELS_DEFAULT)
        self.gpu_id = GPU_ID_DEFAULT
        self.audio_bitrate = AUDIO_BITRATE_DEFAULT
        self.encoder = encoder
        self.framerate = framerate
        self.last_cursor_sent = None
        self.data_streaming_server = data_streaming_server

    async def send_ws_clipboard_data(self, data, mime_type="text/plain", reply_to=None):
        """
        Asynchronously sends clipboard data to all clients, handling multipart for large data.

        reply_to: set to the requesting verb (e.g. "cr") when this send answers a
        client fetch rather than announcing a server-side clipboard change. A
        "clipboard_reply,<verb>" frame then precedes the payload frames on the
        same ordered socket, so clients can treat the payload cache-only without
        time heuristics. Legacy clients route the unknown verb to their input
        module, which ignores it.
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
                    f"clipboard_reply,{reply_to}", per_client_timeout=2.0)
            data_bytes = data.encode('utf-8') if not is_binary and isinstance(data, str) else data
            total_size = len(data_bytes)
            if total_size < CLIPBOARD_CHUNK_SIZE:
                encoded_data = base64.b64encode(data_bytes).decode('ascii')
                if is_binary:
                    message = f"clipboard_binary,{mime_type},{encoded_data}"
                else:
                    message = f"clipboard,{encoded_data}"
                # Bounded: the clipboard monitor task calls this, and one
                # stalled client must not wedge clipboard delivery for all.
                await _broadcast_to_clients(self.data_streaming_server.clients, message, per_client_timeout=2.0)
            else:
                data_logger.info(f"Sending large clipboard data ({mime_type}, {total_size} bytes) via multipart.")
                start_message = f"clipboard_start,{mime_type},{total_size}"
                await _broadcast_to_clients(self.data_streaming_server.clients, start_message, per_client_timeout=2.0)
                offset = 0
                while offset < total_size:
                    chunk = data_bytes[offset:offset + CLIPBOARD_CHUNK_SIZE]
                    encoded_chunk = base64.b64encode(chunk).decode('ascii')
                    data_message = f"clipboard_data,{encoded_chunk}"
                    await _broadcast_to_clients(self.data_streaming_server.clients, data_message, per_client_timeout=2.0)
                    offset += len(chunk)
                    await asyncio.sleep(0)
                await _broadcast_to_clients(self.data_streaming_server.clients, "clipboard_finish", per_client_timeout=2.0)
                data_logger.info("Finished sending multi-part clipboard data.")
        except Exception as e:
            data_logger.error(f"Failed to send clipboard data: {e}", exc_info=True)

    def send_ws_cursor_data(self, data):
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
                # Bounded: cursor changes arrive from a pixelflux thread at high
                # rate; a stalled client would otherwise accumulate one blocked
                # coroutine per cursor change.
                await _broadcast_to_clients(clients_ref, msg_to_broadcast, per_client_timeout=2.0)

            asyncio.run_coroutine_threadsafe(
                _broadcast_cursor_helper(), self.async_event_loop
            )
        else:
            data_logger.warning("Cannot broadcast cursor data: no clients connected or server not ready.")

    async def stop_pipeline(self):
        logger_app.info("Stopping pipelines (generic call)...")
        if self.data_streaming_server:
            await self.data_streaming_server.reconfigure_displays()
        self.pipeline_running = False
        logger_app.info("Pipelines stop signal processed.")

    stop_ws_pipeline = stop_pipeline

    def set_framerate(self, framerate):
        self.framerate = int(framerate)
        logger_app.info(
            f"Framerate for {self.encoder} set to {self.framerate}. Restart pipeline if active."
        )


class DataStreamingServer(BaseStreamingService):
    """Handles the data WebSocket connection for input, stats, and control messages."""

    def __init__(self, supervisor = None):
        super().__init__("websockets")
        self.data_ws = (
            None
        )
        self.clients = set()
        self.app = None
        self.cli_args = settings
        self.is_secure_mode = False
        self.input_handler = None
        self._tasks_to_run = []
        self.RECONNECT_DEBOUNCE_MS = 500
        # Reload/reconnect grace: how long a disconnected display's entry (and
        # its running capture) waits for the page to come back before teardown.
        self.RECONNECT_GRACE_S = 3.0
        self._display_teardown_tasks = set()
        self.MAX_RECENT_CLIENTS = 1000
        self.last_connection_times = OrderedDict()
        self._latest_client_render_fps = 0.0
        self._last_time_client_ok = 0.0
        self._client_acknowledged_frame_id = -1
        self._frame_backpressure_task = None
        self._last_client_acknowledged_frame_id_update_time = 0.0
        self._previous_ack_id_for_stall_check = -1
        self._previous_sent_id_for_stall_check = -1
        self._sent_frames_log = deque()
        self.rc_mode = RateControlMode.CRF
        self.config_gate = asyncio.Event()
        self.shutdown_event = asyncio.Event()
        self._shutdown_called = False
        self.supervisor = supervisor
        
        def get_initial_value(setting_name):
            """Helper to get the correct initial integer/bool from a processed setting."""
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
        # Instance-wide holder so all connections read one consistent bandwidth value.
        self._shared_network_stats = {}
        # Cached once; avoids a blocking nvidia-smi probe per connection.
        self._gpu_available = None
        # Serializes the first-connection GPU probe so concurrent first
        # connections don't both spawn nvidia-smi and defeat the cache above.
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
        # Serializes per-display video start/stop so a START can't overlap a
        # still-finishing STOP. Distinct from _reconfigure_lock,
        # which the reconfigure pass holds while calling start/stop.
        self._video_capture_lock = asyncio.Lock()
        self._is_reconfiguring = False
        # Set when a reconfiguration is requested while one is already running, so
        # the latest state is reconciled afterwards instead of being dropped.
        self._reconfigure_pending = False
        self._bytes_sent_in_interval = 0
        self._last_bandwidth_calc_time = time.monotonic()
        # Frame-based backpressure settings
        self.last_start_video_request_times = {}
        self.last_viewer_keyframe_request_times = {}
        # Shared clients that sent STOP_VIDEO (hidden tab): excluded from the
        # primary VIDEO broadcast until their next START_VIDEO, while capture —
        # and their control/cursor/audio — keep running. Holds any paused shared
        # client (viewer or collaborator), not viewers alone. Discarded on
        # disconnect in the ws handler's cleanup and cleared on shutdown.
        self.video_paused_clients = set()
        self._deferred_viewer_rejoins = {}
        self.allowed_desync_ms = BACKPRESSURE_ALLOWED_DESYNC_MS
        self.latency_threshold_for_adjustment_ms = BACKPRESSURE_LATENCY_THRESHOLD_MS
        self.backpressure_check_interval_s = BACKPRESSURE_CHECK_INTERVAL_S
        # Per-stream video/audio queue depth (frames dropped past this under backpressure).
        self.BACKPRESSURE_QUEUE_SIZE = getattr(settings, 'backpressure_queue_size', 120)
        self._backpressure_send_frames_enabled = True
        self._last_client_frame_id_report_time = 0.0
        self.capture_loop = None

        self.display_clients = {}
        self.video_chunk_queues = {}
        self.capture_instances = {}
        self.display_layouts = {}
        # One ScreenCapture per display_id, kept for the server's lifetime: start/stop
        # cycles reuse the object so a reconfigure doesn't re-initialise the capture
        # backend (NVENC session/CUDA context, Wayland compositor handle) every time.
        self._persistent_capture_modules = {}
        # Fallback pixelflux handle for Wayland output management when no
        # primary capture module exists yet (any handle reaches the shared backend).
        self._wayland_ctl_module = None
        self._last_keyframe_request = {}  # display_id -> monotonic, rate-limits client IDR requests

        # pcmflux audio capture state
        self.audio_device_name = self.cli_args.audio_device_name
        self.pcmflux_module = None
        self.is_pcmflux_capturing = False
        self.pcmflux_settings = None
        # Opus+RED redundancy gate for the shared audio broadcast: per-connection
        # client capability (advertised via the "audioRedundancy" settings field)
        # and the red_distance currently applied to the running pipeline. RED is
        # enabled only when EVERY connected client is capable (one shared buffer);
        # a single non-capable client falls the whole stream back to plain frames.
        self.audio_redundancy_by_ws = {}
        self.audio_redundancy_enabled = bool(settings.audio_redundancy[0])
        self._active_audio_red_distance = 0
        # The vendored WebRTC RedOpusEncoder reads its depth from audio_config at
        # per-connection construction; publish the resolved setting (0 disables
        # redundancy => primary-only, i.e. plain Opus) so one control drives both
        # the WebRTC and WebSocket paths.
        _red_dist = getattr(settings, "audio_redundancy_distance", AUDIO_RED_DISTANCE)
        audio_config.set_red_distance(
            _red_dist if self.audio_redundancy_enabled else 0
        )
        self.pcmflux_callback = None
        self.pcmflux_audio_queue = None
        self.pcmflux_send_task = None
        self.pcmflux_capture_loop = None

        # State for window manager swapping
        self._last_display_count = 0
        self._is_wm_swapped = False
        self._wm_swap_is_supported = None

    def initialize(self):
        """
        Initialize the DataStreamingServer components including SelkiesStreamingApp and InputHandler.
        This should be called before run().
        """
        self.is_secure_mode = bool(self.cli_args.master_token)
        if self.is_secure_mode:
            logger.info("Secure Mode ENABLED (SELKIES_MASTER_TOKEN is set).")
            settings.encoder = "h264enc"
            for s_def in SETTING_DEFINITIONS:
                if s_def['name'] == 'encoder':
                    s_def['meta']['allowed'] = ['h264enc']
                    s_def['default'] = 'h264enc'
                    break
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

        if not settings.debug[0] and PULSEAUDIO_AVAILABLE:
            logging.getLogger("pulsectl_asyncio").setLevel(logging.WARNING)

        logger.info(f"Initializing DataStreamingServer with encoder: {initial_encoder}, Framerate: {TARGET_FRAMERATE}")

        event_loop = asyncio.get_running_loop()
        # Create SelkiesStreamingApp
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

        # The single clipboard policy string (true/false/in/out), normalized by
        # the settings pipeline; the input handler gates directions off it.
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
        )

        self.input_handler.on_clipboard_read = self.app.send_ws_clipboard_data
        # The websockets app has one global framerate; the display_id the shared
        # protocol threads through is only meaningful to the WebRTC service.
        self.input_handler.on_set_fps = (
            lambda fps, display_id='primary': self.app.set_framerate(fps)
        )
        # The shared input protocol's pointer-visibility toggle ("p,N") is the
        # same tunable as SET_NATIVE_CURSOR_RENDERING (both fire on pointer lock).
        self.input_handler.on_mouse_pointer_visible = self.set_native_cursor_rendering

        if ENABLE_RESIZE:
            self.input_handler.on_resize = lambda res_str, display_id='primary': on_resize_handler(
                res_str, self.app, self, display_id
            )
        else:
            self.input_handler.on_resize = lambda res_str, display_id='primary': logger.warning("Resize disabled.")
            self.input_handler.on_scaling_ratio = lambda scale_val: logger.warning(
                "Scaling disabled."
            )
        logger.info("DataStreamingServer initialization complete.")

    async def set_native_cursor_rendering(self, enabled: bool):
        """Compose the X cursor into the captured video (vs the client-drawn
        overlay); applies to every display's capture. Reached both from the
        SET_NATIVE_CURSOR_RENDERING message and the shared input protocol's
        pointer-visibility toggle ("p,N"), which map to the same tunable."""
        if self.capture_cursor == enabled:
            data_logger.info(f"Native cursor rendering: value {enabled} is already set.")
            return
        self.capture_cursor = enabled
        if len(self.capture_instances) > 0:
            data_logger.info("Cursor rendering changed, triggering display reconfiguration.")
            await self.reconfigure_displays()

    async def broadcast_display_config(self):
        """Broadcasts the current display configuration to all clients."""
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

    def refresh_cursor_cache(self):
        if not self.app:
            return None

        cursor_data = None
        if self.input_handler and hasattr(self.input_handler, "get_current_cursor_data"):
            cursor_data = self.input_handler.get_current_cursor_data()

        if cursor_data is not None:
            self.app.last_cursor_sent = cursor_data

        return self.app.last_cursor_sent

    async def send_current_cursor(self, websocket, raddr):
        cursor_data = self.refresh_cursor_cache()
        if not cursor_data:
            return

        data_logger.info(f"Sending current cursor to client {raddr}")
        try:
            msg_str = json.dumps(cursor_data)
            await websocket.send_str(f"cursor,{msg_str}")
        except Exception as e:
            data_logger.warning(f"Failed to send current cursor to client {raddr}: {e}")

    def _pcmflux_audio_callback(self, frame):
        """
        Callback passed to pcmflux, called from its capture thread with an AudioFrame.
        """
        if self.is_pcmflux_capturing and frame is not None and self.pcmflux_audio_queue is not None:
            if len(frame) > 0:
                # Snapshot loop+queue: this runs on pcmflux's capture thread, which
                # can null either out during teardown.
                loop = self.pcmflux_capture_loop
                q = self.pcmflux_audio_queue
                if loop is None or q is None or loop.is_closed():
                    return
                # Zero-copy: keep the AudioFrame as owner; its native buffer (incl.
                # pcmflux's [0x01,0x00] header) frees once the queue item drops.
                item = {'data': memoryview(frame), 'owner': frame}
                def _do_put():
                    try:
                        q.put_nowait(item)
                    except asyncio.QueueFull:
                        pass  # drop rather than grow unbounded
                # Guard the schedule: the loop can close between the check above and
                # here, and the RuntimeError would surface in pcmflux's C thread.
                try:
                    loop.call_soon_threadsafe(_do_put)
                except RuntimeError:
                    pass  # loop closed mid-teardown; drop the chunk
    
    async def _pcmflux_send_audio_chunks(self):
        """
        Async task to broadcast Opus audio chunks from the queue to WebSocket clients.
        """
        data_logger.info("pcmflux audio chunk broadcasting task started.")
        try:
            while True:
                item = await self.pcmflux_audio_queue.get()

                secondary_websockets = {
                    client_info.get('ws')
                    for did, client_info in self.display_clients.items()
                    if did != 'primary' and client_info.get('ws')
                }
                primary_viewers = self.clients - secondary_websockets

                if not primary_viewers:
                    self.pcmflux_audio_queue.task_done()
                    continue

                # item['data'] is a zero-copy memoryview over the AudioFrame's native
                # buffer (already includes pcmflux's [0x01,0x00] header); send as-is.
                message_to_send = item['data']
                self._bytes_sent_in_interval += len(message_to_send) * len(primary_viewers)
                # Bounded sends: one stalled socket must not freeze the shared
                # audio stream (a timed-out client is dropped and closed).
                dropped = await _broadcast_to_clients(
                    primary_viewers, message_to_send,
                    per_client_timeout=SHARED_STREAM_SEND_TIMEOUT_SECONDS,
                )
                if dropped:
                    # primary_viewers is a per-chunk temporary: propagate the drop
                    # to the authoritative registry, or the dead socket re-enters
                    # the fan-out on the very next chunk.
                    self.clients -= dropped

                self.pcmflux_audio_queue.task_done()
        except asyncio.CancelledError:
            data_logger.info("pcmflux audio chunk broadcasting task cancelled.")
        finally:
            data_logger.info("pcmflux audio chunk broadcasting task finished.")

    def _compute_audio_red_distance(self):
        """RED distance for the shared audio broadcast. WS is TCP, but the sender still
        drops frames under backpressure (pcmflux delivery ring drop-oldest, and this
        server's audio queue drops on overflow), and RED lets the client recover those
        within the redundancy distance. Enabled only when the server allows it AND there
        is at least one client AND every connected client advertised audioRedundancy; a
        single non-capable (or legacy, field-absent) client falls the whole stream back
        to plain frames (0), which decodes everywhere."""
        if not self.audio_redundancy_enabled:
            return 0
        if not self.clients:
            return 0
        for ws in self.clients:
            if not self.audio_redundancy_by_ws.get(ws):
                return 0
        return getattr(settings, "audio_redundancy_distance", AUDIO_RED_DISTANCE)

    async def _regate_audio_redundancy(self):
        """Recompute the RED gate for the shared audio stream and, if it flipped
        while capturing, restart the pipeline so the new red_distance takes
        effect. Callers hold the reconfigure guard (pipeline start/stop must be
        serialized against reconfigure_displays)."""
        desired = self._compute_audio_red_distance()
        if desired == self._active_audio_red_distance:
            return
        if not self.is_pcmflux_capturing:
            return
        # During teardown the last client leaving drops RED to 0, but the app is already
        # gone and the disconnect path stops the pipeline itself. Skip the pointless
        # stop+restart here so it doesn't fail with a spurious "self.app not available"
        # ERROR; a genuine gate change with clients still present keeps self.app set.
        if not self.app:
            return
        data_logger.info(
            f"Audio RED gate changed ({self._active_audio_red_distance} -> {desired}); "
            "restarting audio pipeline."
        )
        await self._stop_pcmflux_pipeline()
        await self._start_pcmflux_pipeline()

    async def _start_pcmflux_pipeline(self):
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

        data_logger.info("Starting pcmflux audio pipeline...")
        try:
            capture_settings = AudioCaptureSettings()
            device_name_bytes = self.audio_device_name.encode('utf-8') if self.audio_device_name else None
            capture_settings.device_name = device_name_bytes
            capture_settings.sample_rate = 48000
            capture_settings.channels = self.app.audio_channels
            capture_settings.opus_bitrate = int(self.app.audio_bitrate)
            # Frame duration bounds the capture-side latency floor (a frame must fill
            # before it can be encoded, and the client buffers a fixed number of
            # frames); ask PulseAudio for fragments no larger than one frame.
            frame_ms = float(getattr(settings, 'audio_frame_duration_ms', '20') or 20)
            capture_settings.frame_duration_ms = frame_ms
            capture_settings.use_vbr = True
            capture_settings.use_silence_gate = False
            capture_settings.latency_ms = int(min(10, frame_ms))
            capture_settings.debug_logging = self.cli_args.debug[0]
            # Keep pcmflux's native [0x01,0x00] header (no Python prepend/copy).
            capture_settings.omit_audio_header = False
            # Opus+RED redundancy for the shared broadcast: enabled only when
            # every connected client is capable.
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

            self.is_pcmflux_capturing = True
            if self.pcmflux_send_task is None or self.pcmflux_send_task.done():
                self.pcmflux_send_task = asyncio.create_task(self._pcmflux_send_audio_chunks())
            
            data_logger.info("pcmflux audio capture started successfully.")
            return True
        except Exception as e:
            data_logger.error(f"Failed to start pcmflux audio pipeline: {e}", exc_info=True)
            await self._stop_pcmflux_pipeline() # Attempt cleanup on failure
            return False

    async def _stop_pcmflux_pipeline(self):
        if not self.is_pcmflux_capturing and not self.pcmflux_module:
            return True
        
        data_logger.info("Stopping pcmflux audio pipeline...")
        self.is_pcmflux_capturing = False # Prevent new items from being queued

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

    async def shutdown_pipelines(self):
        """
        A unified, deadlock-proof method to stop all capture pipelines.
        This should be the ONLY way pipelines are programmatically stopped.
        """
        logger.info("Initiating unified pipeline shutdown...")
        await self.reconfigure_displays()
        # Serialize the audio/backpressure teardown against reconfigure: a
        # disconnect/connect race could otherwise let this tear down audio a new
        # client just started. The guard is NOT held at this call site, and none
        # of the awaited teardowns re-acquire the reconfigure lock, so this is
        # deadlock-free. reconfigure_displays() above self-acquires the lock, so
        # it must stay outside this block.
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

    async def _ensure_backpressure_task_is_stopped(self, display_id: str):
        """Safely cancels and cleans up the backpressure task for a specific display."""
        display_state = self.display_clients.get(display_id)
        if not display_state:
            return

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

        if task_was_running:
            data_logger.info(f"Backpressure task for '{display_id}' was stopped. Resetting its frame IDs.")
            await self._reset_frame_ids_and_notify(display_id)

    async def _reset_frame_ids_and_notify(self, display_id: str):
        """
        Resets frame IDs for a display. If it's the primary display,
        it broadcasts the reset to ALL clients.
        """
        display_state = self.display_clients.get(display_id)
        if not display_state:
            return

        data_logger.info(f"Resetting frame IDs for display '{display_id}'.")
        display_state['last_sent_frame_id'] = 0
        display_state['has_sent_any_frame'] = False
        display_state['acknowledged_frame_id'] = -1
        # Frame numbering restarts at 0, so every artifact keyed by id must go
        # with it: a stale send stamp matched by a NEW id of the same value
        # manufactures a giant RTT "sample" that poisons the smoothed estimate
        # (and with it the backpressure forgiveness and the dashboard latency).
        sent_ts = display_state.get('sent_timestamps')
        if sent_ts is not None:
            sent_ts.clear()
        rtt_samples = display_state.get('rtt_samples')
        if rtt_samples is not None:
            rtt_samples.clear()
        display_state['smoothed_rtt'] = 0.0
        # Re-prime the client-fps estimator: its next sample would otherwise
        # span the reset (old high id vs new low id).
        display_state.pop('_fps_sample_acked', None)
        display_state.pop('_fps_sample_time', None)
        
        message = f"PIPELINE_RESETTING {display_id}"
        
        # This runs under _video_capture_lock, which serializes ALL video control.
        # Bound each notify send so one stalled client can't wedge every display's
        # start/stop globally. A client we time out on gets its (possibly half-written)
        # socket dropped and closed rather than reused; the frame-id state was already
        # reset above, which is what matters for correctness.
        if display_id == 'primary' and self.clients:
            data_logger.info(f"Broadcasting primary pipeline reset to all {len(self.clients)} clients: {message}")
            await _broadcast_to_clients(self.clients, message, per_client_timeout=2.0)
        else:
            websocket = display_state.get('ws')
            if websocket:
                try:
                    await asyncio.wait_for(websocket.send_str(message), timeout=2.0)
                except asyncio.TimeoutError:
                    # wait_for cancelled the in-flight send; the socket may hold a
                    # half-written frame, so drop and close it instead of reusing it.
                    data_logger.warning(f"Timed out notifying client for '{display_id}' of reset; dropping socket.")
                    self.clients.discard(websocket)
                    _close_abandoned_ws(websocket)
                except (ConnectionResetError, OSError, RuntimeError):
                    data_logger.warning(f"Could not notify client for '{display_id}' of reset; connection closed.")
        
        display_state['backpressure_enabled'] = True
        display_state['last_ack_update_time'] = time.monotonic()

    async def _start_backpressure_task_if_needed(self, display_id: str):
        """Starts the backpressure task for a specific display if not already running."""
        # --- Disable for wayland mode ---
        if IS_WAYLAND:
            return

        display_state = self.display_clients.get(display_id)
        if not display_state:
            data_logger.error(f"Cannot start backpressure task: display '{display_id}' not found.")
            return

        await self._ensure_backpressure_task_is_stopped(display_id)

        task = display_state.get('backpressure_task')
        if not task or task.done():
            new_task = asyncio.create_task(self._run_frame_backpressure_logic(display_id))
            display_state['backpressure_task'] = new_task
            data_logger.info(f"New frame backpressure task started for display '{display_id}'.")
        else:
            data_logger.warning(f"Backpressure task for '{display_id}' was already running. Not starting a new one.")

    def _schedule_deferred_viewer_rejoin(self, websocket, delay: float):
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
                self._schedule_idr_for_display('primary')
            finally:
                self._deferred_viewer_rejoins.pop(websocket, None)

        self._deferred_viewer_rejoins[websocket] = asyncio.create_task(_rejoin())

    def _schedule_idr_for_display(self, display_id: str):
        """Ask the encoder for a fresh keyframe on this display, off the event loop."""
        instance = self.capture_instances.get(display_id)
        module = instance.get('module') if instance else None
        if module:
            # Non-blocking in pixelflux (an atomic flag / channel send), so it can run
            # inline; a keyframe request is idempotent.
            try:
                module.request_idr_frame()
            except Exception:
                pass

    async def _broadcast_live_server_settings(self, display_id: str):
        """Re-announce server settings carrying the display's LIVE encoder.

        The handshake payload holds boot config only; after an encoder switch
        every connected client — shared viewers included — must re-key its
        wire-format demux, or it drops the new mode's chunks forever.
        """
        try:
            payload = build_client_settings_payload()
            live_encoder = self.display_clients.get(display_id, {}).get('encoder')
            if live_encoder and isinstance(payload.get('encoder'), dict):
                payload['encoder'] = dict(payload['encoder'])
                payload['encoder']['value'] = live_encoder
            payload['ws_max_message_bytes'] = {"value": WS_MAX_MESSAGE_BYTES}
            msg = json.dumps({"type": "server_settings", "settings": payload})
        except Exception as e:
            data_logger.warning(f"Could not build live server settings broadcast: {e}")
            return
        # Bounded broadcast: this runs while _apply_client_settings holds
        # _reconfigure_lock, so a frozen client's full socket buffer must be
        # dropped, not waited on.
        await _broadcast_to_clients(self.clients, msg, per_client_timeout=2.0)

    def _set_backpressure_enabled(self, display_id: str, display_state: dict, enabled: bool):
        """Update the backpressure flag, requesting an IDR when it lifts.

        While backpressure was active, delta frames were dropped, so on the
        False->True (LIFTED) transition the client needs a keyframe to resync;
        otherwise it decodes deltas against a reference it never received.
        """
        prev_enabled = display_state.get('backpressure_enabled', True)
        display_state['backpressure_enabled'] = enabled
        if enabled and not prev_enabled:
            self._schedule_idr_for_display(display_id)

    async def _run_frame_backpressure_logic(self, display_id: str):
        """The core backpressure and latency calculation loop for a single display."""
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

                # Size the backpressure window by the client's measured consumption
                # (acked-frame cadence) so a client rendering below the configured rate
                # gets a correctly scaled window. Sampled only while sends are unthrottled;
                # during active backpressure the ack rate reflects our throttling, not the
                # client, so the last healthy estimate is held to avoid a low-fps ->
                # tighter-window -> stuck-backpressure latch.
                configured_fps = display_state.get('framerate', 60)
                if configured_fps <= 0:
                    configured_fps = 60
                client_fps = self._estimate_client_fps(
                    display_state, last_client_acked_frame_id, configured_fps, time.monotonic()
                )

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
                # Forgive propagation delay, capped: the RTT estimate rides the same
                # queue this loop bounds, so it must never be able to out-grow the
                # trigger it feeds (queue -> higher RTT -> looser trigger -> queue).
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

    def _estimate_client_fps(self, display_state, acked_id, configured_fps, now):
        """Measured client FPS from acked-frame cadence, clamped to [1.0, configured_fps].

        Updates the running estimate only from healthy (unthrottled) intervals with
        forward progress; otherwise holds the last value. `now` is passed in so the
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

    async def broadcast_stream_resolution(self):
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
            # Bounded: this runs under _reconfigure_lock, and one frozen client
            # must not wedge display control for everyone.
            dropped = await _broadcast_to_clients(sockets, message_str, per_client_timeout=2.0)
            # The fan-out ran over a computed group set; mirror removals into
            # the authoritative registry.
            for ws in dropped:
                self.clients.discard(ws)

    async def _sync_wayland_realized_geometry(self, display_id, broadcast=True):
        """Read back what the pixelflux compositor actually realized on this
        display's output (it may even-mask dimensions or keep the old mode on a
        GBM allocation failure), fold it into display state/layouts and
        broadcast stream_resolution so the client reconciles its canvas and
        input mapping — the Wayland counterpart of the X11 reconfigure path's
        realized clamp + broadcast. The read also acts as a barrier: the
        compositor answers it only after any queued capture (re)start finished.
        `broadcast=False` defers the fan-out to a caller that broadcasts once
        for every display (the reconfigure pass)."""
        if not IS_WAYLAND:
            return
        inst = self.capture_instances.get(display_id)
        module = inst.get('module') if inst else None
        if module is None or not hasattr(module, 'get_realized_geometry'):
            return
        try:
            w, h, scale = await asyncio.to_thread(
                module.get_realized_geometry, wayland_output_id(display_id))
        except Exception as e:
            data_logger.warning(f"Wayland realized-geometry read failed for '{display_id}': {e}")
            return
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

    async def _apply_wayland_cursor_size(self, dpi):
        """Wayland counterpart of the X11 per-DPI cursor resize: the compositor
        reloads its theme cursor (composited overlay and named-cursor delivery
        both re-render) at the DPI-scaled size, live, no capture restart."""
        if CURSOR_SIZE <= 0:
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

    def _update_wayland_cursor_cap(self, dpi):
        """Wayland parity with X11's per-DPI cursor re-derive: track the DPI on
        the input handler and scale the remote-cursor delivery cap with it, so
        the next capture (re)start threads the raised cap through
        CaptureSettings. The compositor's composited cursor follows the output
        scale on its own (set_cursor_size re-derives its theme pixel size on
        DPI changes)."""
        ih = self.input_handler
        if ih is None:
            return
        try:
            ih.system_dpi = float(dpi)
            ih.cursor_size_cap = int(ih.max_cursor_size * float(dpi) / 96.0)
        except Exception as e:
            data_logger.debug(f"cursor cap update skipped: {e}")

    def _parse_settings_payload(self, payload_str: str) -> dict:
        settings_data = json.loads(payload_str)
        parsed = {}

        def get_int(k):
            v = settings_data.get(k)
            return int(v) if v is not None else None

        def get_number(k):
            # int when integral, float otherwise (sub-Mbps bitrates).
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
        # Client advertises Opus+RED de-RED capability for the WS audio path.
        parsed["audioRedundancy"] = get_bool("audioRedundancy")
        # Optional client keyboard-layout hint (e.g. "de", "ch(fr)"): becomes
        # the compositor's base xkb layout on Wayland, informational on X11.
        parsed["keyboardLayout"] = get_str("keyboardLayout")
        data_logger.debug(f"Parsed client settings: {parsed}")
        return parsed

    async def _apply_client_settings(
        self, websocket_obj, settings: dict, is_initial_settings: bool, client_role: str = "controller"
    ):

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
                elif is_initial_settings:
                    target_w = settings.get("initialClientWidth")
                    target_h = settings.get("initialClientHeight")
                    # Cap client-supplied dimensions so they cannot reach xrandr --fb unbounded.
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
                display_state["force_aligned_resolution"] = sanitize_value(
                    "force_aligned_resolution", settings.get("force_aligned_resolution")
                )
                if server_is_manual:
                    # A server-forced resolution may only be altered by the
                    # server's own setting, never by a client-side toggle.
                    apply_alignment = getattr(self.cli_args, "force_aligned_resolution")[0]
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
                display_state["encoder"] = sanitize_value("encoder", settings.get("encoder"))
                display_state["framerate"] = sanitize_value("framerate", settings.get("framerate"))
                display_state["video_crf"] = sanitize_value("video_crf", settings.get("video_crf"))
                display_state["video_fullcolor"] = sanitize_value("video_fullcolor", settings.get("video_fullcolor"))
                display_state["video_streaming_mode"] = sanitize_value("video_streaming_mode", settings.get("video_streaming_mode"))
                display_state["jpeg_quality"] = sanitize_value("jpeg_quality", settings.get("jpeg_quality"))
                display_state["paint_over_jpeg_quality"] = sanitize_value("paint_over_jpeg_quality", settings.get("paint_over_jpeg_quality"))
                display_state["use_paint_over_quality"] = sanitize_value("use_paint_over_quality", settings.get("use_paint_over_quality"))
                display_state["video_paintover_crf"] = sanitize_value("video_paintover_crf", settings.get("video_paintover_crf"))
                display_state["video_paintover_burst_frames"] = sanitize_value("video_paintover_burst_frames", settings.get("video_paintover_burst_frames"))
                if display_state["encoder"] in ["jpeg", "h264enc-striped", "openh264enc"]:
                    display_state["use_cpu"] = True
                    data_logger.info(f"Forcing use_cpu=True because encoder is '{display_state['encoder']}'")
                else:
                    display_state["use_cpu"] = sanitize_value("use_cpu", settings.get("use_cpu"))
                self.app.audio_bitrate = sanitize_value("audio_bitrate", settings.get("audio_bitrate"))
                display_state["audio_bitrate"] = self.app.audio_bitrate
                display_state["video_bitrate"] = sanitize_value("video_bitrate", settings.get("video_bitrate"))
                enable_rate_control, _ = self.cli_args.enable_rate_control
                if enable_rate_control:
                    display_state["rate_control_mode"] = sanitize_value("rate_control_mode", settings.get("rate_control_mode"))
            
                if self.input_handler:
                    self.enable_binary_clipboard = sanitize_value("enable_binary_clipboard", settings.get("enable_binary_clipboard"))
                    await self.input_handler.update_binary_clipboard_setting(self.enable_binary_clipboard)
                    kb_layout = settings.get("keyboardLayout")
                    if kb_layout:
                        await self.input_handler.apply_client_keyboard_layout(kb_layout)
                new_dpi = sanitize_value("scaling_dpi", settings.get("scaling_dpi"))
                if app_settings._overridden.get("scaling_dpi", False):
                    # An operator-set DPI (CLI/env) governs the desktop: client
                    # DPI syncs must not clobber it.
                    if new_dpi is not None and new_dpi != old_settings.get("scaling_dpi"):
                        data_logger.info("Ignoring client DPI sync: scaling_dpi is operator-overridden.")
                    new_dpi = old_settings.get("scaling_dpi")
                if new_dpi is not None and new_dpi != old_settings.get("scaling_dpi"):
                    data_logger.info(f"DPI changed from {old_settings.get('scaling_dpi')} to {new_dpi}. Applying system-level change.")
                    await set_dpi(new_dpi)
                    if CURSOR_SIZE > 0 and not IS_WAYLAND:
                        new_cursor_size = cursor_size_for_dpi(new_dpi, CURSOR_SIZE)
                        await set_cursor_size(new_cursor_size)
                    if IS_WAYLAND:
                        # DPI realizes as the pixelflux compositor output scale
                        # (set_dpi is a no-op on Wayland). Applied unconditionally
                        # — WebRTC-mode parity; the capture restart below (via the
                        # scaling_dpi restart trigger) reads it. No compositor
                        # detection: pixelflux owns the session and implements no
                        # wlr-output-management, so external tools cannot set it.
                        display_state['scale'] = float(new_dpi) / 96.0
                        self._update_wayland_cursor_cap(new_dpi)
                        await self._apply_wayland_cursor_size(new_dpi)

                display_state["scaling_dpi"] = new_dpi
                dimensional_change = resolution_actually_changed or position_actually_changed

                video_params_list = [
                    'encoder', 'framerate', 'video_crf', 'video_fullcolor', 'video_streaming_mode',
                    'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu', 'video_paintover_crf',
                    'video_paintover_burst_frames', 'use_paint_over_quality', 'rate_control_mode', 'video_bitrate'
                ]
                if IS_WAYLAND:
                    video_params_list.append('scaling_dpi')

                video_params_changed = any(
                    display_state.get(key) != old_settings.get(key)
                    for key in video_params_list
                )
                audio_bitrate_changed = self.app.audio_bitrate != old_settings.get('audio_bitrate')
                if audio_bitrate_changed and self.is_pcmflux_capturing:
                    # Live Opus retarget (atomic in pcmflux); the pipeline keeps running.
                    try:
                        self.pcmflux_module.update_audio_bitrate(int(self.app.audio_bitrate))
                        data_logger.info(f"Applied audio bitrate live: {self.app.audio_bitrate} bps")
                    except Exception as e:
                        data_logger.warning(f"Live audio bitrate update failed ({e}); restarting audio pipeline.")
                        await self._stop_pcmflux_pipeline()
                        await self._start_pcmflux_pipeline()
                needs_fallback_reconfigure = False
                if not (is_initial_settings or dimensional_change) and video_params_changed:
                    # Only structural switches rebuild the encoder session; rate and
                    # per-frame tunables apply to the live capture with no restart.
                    restart_video_params = ['encoder', 'use_cpu', 'video_fullcolor', 'rate_control_mode']
                    if IS_WAYLAND:
                        # A compositor scale change reconfigures the output; the
                        # live-tunables path cannot apply it.
                        restart_video_params.append('scaling_dpi')
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
                            module.update_framerate(float(display_state.get('framerate') or self.app.framerate))
                            module.update_video_bitrate(int(round(float(display_state.get('video_bitrate') or 0) * 1000)))
                            module.update_tunables(self._get_capture_settings(
                                display_id, layout['w'], layout['h'], layout['x'], layout['y']
                            ))
                        except Exception as e:
                            data_logger.warning(
                                f"Live video settings update failed for '{display_id}' ({e}); restarting its capture."
                            )
                            video_restart_needed = True
                    if video_restart_needed or module is None:
                        data_logger.info(
                            f"Video parameters changed for '{display_id}'. "
                            "Restarting its capture stream without reconfiguring displays."
                        )
                        if display_id in self.display_layouts:
                            layout = self.display_layouts[display_id]
                            await self._stop_capture_for_display(display_id)
                            await self._start_capture_for_display(
                                display_id=display_id,
                                width=layout['w'], height=layout['h'],
                                x_offset=layout['x'], y_offset=layout['y']
                            )
                            await self._start_backpressure_task_if_needed(display_id)
                            # The restarted capture must repaint even on a static
                            # screen (the Wayland damage tracker stays warm across
                            # a stop/start), and clients on the old wire format
                            # need to relearn the encoder: force a keyframe and
                            # re-announce the live settings to every client.
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
            # A raise skips the pending re-check below; consume a reconfigure
            # coalesced during the hold so it is not stranded.
            if self._reconfigure_pending:
                await self.reconfigure_displays()
            raise
        # reconfigure_displays() self-acquires the lock; call it OUTSIDE.
        if is_initial_settings or dimensional_change:
            data_logger.info(
                f"Initial setup or dimensional change detected for '{display_id}'. "
                "Performing full display reconfiguration."
            )
            await self.reconfigure_displays()
        elif needs_fallback_reconfigure or self._reconfigure_pending:
            # _reconfigure_pending: a reconfigure was coalesced while we held _reconfigure_lock
            # above; consume it here so it isn't stranded.
            await self.reconfigure_displays()
        if is_initial_settings and self.client_settings_received and not self.client_settings_received.is_set():
            self.client_settings_received.set()

    def _report_client_presence(self):
        if self.supervisor:
            self.supervisor.set_clients_present(bool(self.clients))

    async def ws_handler(self, websocket: web.WebSocketResponse, remote_address, token = "", query_role = "", query_slot = None):
        if self.is_secure_mode:
            await self.config_gate.wait()
            if not token or token not in user_tokens:
                data_logger.warning(f"Rejecting connection from {remote_address}: Missing or invalid token.")
                await websocket.close(code=4001, message=b"Invalid authentication token")
                return

            permissions = user_tokens[token]
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
            # Legacy mode: role/slot are passed via query params
            # (URL fragments are never transmitted to the server per HTTP spec)
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

        await self.send_current_cursor(websocket, raddr)

        server_settings_payload = {
            "type": "server_settings",
            "settings": build_client_settings_payload(),
        }
        # Transport capacity, not a user setting: lets the client size multipart
        # chunks (clipboard, uploads) to the whole frame.
        server_settings_payload["settings"]["ws_max_message_bytes"] = {
            "value": WS_MAX_MESSAGE_BYTES
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

        self._backpressure_send_frames_enabled = True
        # Per-connection stats sender; the system/gpu/network collectors it reads
        # from are instance-wide singletons created/torn down on self.
        stats_sender_task_ws = None
        # Per-connection START_AUDIO worker: it blocks on client_settings_received
        # (which may never be set), so it must be cancelled with the connection
        # rather than left to run audio ops for a departed client.
        start_audio_task_ws = None

        mic_setup_done = False
        mic_disabled_sent = False
        mic_error = False
        pa_module_index = None
        # Only the caller that loaded module-virtual-source unloads it on teardown
        # (parity with the WebRTC path): a reused pre-existing source is left for
        # the other transport.
        pa_module_owned = False
        pulse = None

        # Mic PCM data plane is Rust-owned (pcmflux AudioPlayback): a GIL-released,
        # non-blocking enqueue into a playback stream on its own thread. pulsectl stays
        # the control plane (sink/virtual-source setup and routing) only.
        mic_playback = None

        if not self.input_handler:
            logger.error(
                f"Data WS handler for {raddr}: Critical - self.input_handler (global) is not set. Input processing will fail."
            )

        gpu_id_for_stats = getattr(self.app, "gpu_id", GPU_ID_DEFAULT)
        # Stats must describe the GPU the pipeline captures/encodes on.
        dri_node_for_stats = (
            getattr(getattr(self.app, "cli_args", None), "encode_dri", "") or ""
        )

        pulse = None
        try:
            # This socket is already in the shared audio fan-out but has not sent
            # SETTINGS, so its RED capability is unknown (absent => not capable):
            # re-gate so a mid-capture join is not fed RED it may not decode.
            # (Shared viewers/legacy clients never send SETTINGS at all.)
            if self.is_pcmflux_capturing:
                async with self._reconfigure_guard():
                    await self._regate_audio_redundancy()

            # Probe the GPU once behind a lock (spawns nvidia-smi): the lock stops
            # concurrent first-connects defeating the _gpu_available cache.
            if self._gpu_available is None:
                async with self._gpu_probe_lock:
                    if self._gpu_available is None:
                        self._gpu_available = bool(
                            await asyncio.get_running_loop().run_in_executor(
                                None, gpu_stats.get_gpus
                            )
                        )

            # System/GPU/network collectors are singletons (per-connection would mean
            # N psutil/NVML polls/sec); they write a shared dict that senders read, not pop.
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
                    )
                )
            stats_sender_task_ws = asyncio.create_task(
                _send_stats_periodically_ws(
                    websocket, self._shared_stats_ws, self
                )
            )
            # Stats sender is per-connection (local, cancelled in finally): an
            # instance-wide ref would be wrong under multiple concurrent WS clients.
            # Single instance-wide collector; per-connection tasks would race the byte counters.
            if self._network_monitor_task_ws is None or self._network_monitor_task_ws.done():
                self._network_monitor_task_ws = asyncio.create_task(
                    _collect_network_stats_ws(self._shared_network_stats, self)
                )

            if PULSEAUDIO_AVAILABLE:
                # microphone_enabled only picks the client-side default (off): the sink
                # handler idles until mic data arrives, and a runtime enable must not
                # need a reconnect. A LOCKED-off microphone still skips the setup.
                _mic_on, _mic_locked = settings.microphone_enabled
                if not settings.audio_enabled[0] or (not _mic_on and _mic_locked):
                    data_logger.info("Audio/microphone disabled in settings. Skipping PulseAudio setup.")
                else:
                    try:
                        data_logger.info("Attempting to establish PulseAudio connection...")
                        pulse = pulsectl_asyncio.PulseAsync("selkies-mic-handler")
                        # Bounded: this runs on the handshake's critical path, and a
                        # missing PulseAudio otherwise stalls every new connection for
                        # the library's full retry cycle before the client can even
                        # claim its display (mic setup degrades the same either way).
                        await asyncio.wait_for(pulse.connect(), timeout=2.0)
                        data_logger.info("PulseAudio connection established.")
                    except Exception as e_pa_conn:
                        data_logger.error(
                            f"Initial PulseAudio connection failed: {e_pa_conn}",
                            exc_info=True,
                        )
                        mic_error = True
            else:
                mic_error = True

            async for msg in websocket:
                # Client->server gzip: a 0x05-tagged binary frame carries a gzip'd text
                # message (large control like clipboard). Inflate it back into a normal
                # TEXT message so the unchanged dispatch below — permission checks and
                # all — handles it. Media/mic/file binary and small text are never wrapped.
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
                    msg_type, payload = msg.data[0], msg.data[1:]
                    if msg_type == 0x02:  # Mic data
                        # Accept mic data unless audio is off or the microphone is
                        # administratively LOCKED off; an unlocked default-off only
                        # sets the client toggle, and the client sends data exactly
                        # when the user turned that toggle on.
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
                        if not PULSEAUDIO_AVAILABLE:
                            if len(payload) > 0:
                                data_logger.warning(
                                    "PulseAudio library not available. Skipping microphone data."
                                )
                            continue
                        if pulse is None:
                            if len(payload) > 0:
                                data_logger.warning(
                                    "PulseAudio client not connected. Skipping microphone data."
                                )
                            continue

                        if not mic_setup_done:
                            data_logger.info(
                                "Performing PulseAudio/PipeWire virtual microphone setup check..."
                            )
                            try:
                                pa_module_index, pa_module_owned = await provision_virtual_microphone(
                                    pulse, self.audio_device_name, self.is_pcmflux_capturing
                                )
                                mic_setup_done = pa_module_index is not None
                            except Exception as e_pa_setup:
                                data_logger.error(
                                    f"PulseAudio mic setup error: {e_pa_setup}",
                                    exc_info=True,
                                )
                                mic_setup_done = False
                                if pa_module_index is not None and pa_module_owned:
                                    try:
                                        data_logger.info(
                                            f"Attempting to unload module {pa_module_index} due to setup error."
                                        )
                                        await pulse.module_unload(pa_module_index)
                                    except Exception as e_unload_err:
                                        data_logger.error(
                                            f"Error unloading module {pa_module_index} after setup failure: {e_unload_err}"
                                        )
                                pa_module_index = None
                                pa_module_owned = False
                                continue

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

                        # Rust owns the PA playback stream on its own thread. Create it
                        # once (blocking connect handshake, offloaded), then hand each
                        # chunk off with a GIL-released, non-blocking write (drop-oldest
                        # inside). No asyncio task/queue/executor/reassembly buffer.
                        try:
                            if mic_playback is None:
                                _pb = AudioPlayback()
                                ps = AudioPlaybackSettings()
                                ps.device_name = b"input"
                                ps.sample_rate = 24000
                                ps.channels = 1
                                ps.latency_ms = 40
                                await asyncio.to_thread(_pb.start, ps)
                                mic_playback = _pb  # only after a successful start
                            mic_playback.write(payload)
                        except Exception as e_rust_mic:
                            data_logger.error(
                                f"Rust mic playback error: {e_rust_mic}", exc_info=False
                            )
                            # Drop this chunk; tear down so the next chunk reopens a
                            # fresh stream.
                            if mic_playback is not None:
                                _dead = mic_playback
                                mic_playback = None
                                try:
                                    await asyncio.to_thread(_dead.stop)
                                except Exception:
                                    pass

                elif msg.type == WSMsgType.TEXT:
                    message = msg.data
                    if message == "_gz,1":
                        # Capability handshake: this client can inflate gzip, so
                        # large control text may be sent as 0x05 gzip frames. Echo it
                        # back so the client also gzips its large client->server sends
                        # (the server always inflates via the 0x05 transform above).
                        websocket._ws_gz = True
                        try:
                            await websocket.send_str("_gz,1")
                        except Exception:
                            pass
                        continue
                    perms = client_permissions.get(websocket)
                    if perms and perms.get("role") == "viewer":
                        # The two-tier authority lists are shared with the WebRTC
                        # gate (input_handler): read-only viewers get the base set;
                        # read-write collaboration (a token-authenticated viewer
                        # drives keyboard/mouse/clipboard) is gated by enable_collab —
                        # when off, the viewer stays read-only even with a valid mk
                        # token.
                        allowed_viewer_prefixes = list(VIEWER_ALLOWED_PREFIXES)
                        if settings.enable_collab[0] and active_mk_token and perms.get("token") == active_mk_token:
                            allowed_viewer_prefixes.extend(VIEWER_COLLAB_EXTRA_PREFIXES)
                        if not any(message.startswith(prefix) for prefix in allowed_viewer_prefixes):
                            # Executing a viewer's blur/visibility lifecycle noise
                            # (kr would clobber the controller's held modifiers) is
                            # refused, but silently — warning per blur floods the log.
                            if not message.startswith(VIEWER_SILENT_DROP_PREFIXES):
                                data_logger.warning(f"DENIED unauthorized message from viewer {remote_address}: {message[:100]}...")
                            continue

                    if message.startswith("SETTINGS,"):
                        try:
                            _, payload_str = message.split(",", 1)
                            parsed_settings = self._parse_settings_payload(payload_str)
                            display_id = parsed_settings.get("displayId", "primary")
                            # Track this client's audio-RED capability for the
                            # shared-broadcast gate (absent field => not capable).
                            self.audio_redundancy_by_ws[websocket] = bool(
                                parsed_settings.get("audioRedundancy")
                            )

                            client_perms = client_permissions.get(websocket)
                            client_role = client_perms.get("role") if client_perms else "controller"

                            if client_role == 'viewer':
                                data_logger.info(f"Viewer client {remote_address} sent initial SETTINGS. Syncing with current stream state.")
                                if not initial_settings_processed:
                                    initial_settings_processed = True

                                # A viewer joining a controller-less server still needs
                                # the primary stream running before the IDR below.
                                if 'primary' not in self.capture_instances:
                                    await self._ensure_viewer_capture()

                                await self.broadcast_stream_resolution()

                                # Reset only the JOINING viewer (already-decoding clients
                                # must not be disrupted), then request an IDR so its
                                # keyframe gate opens immediately — with an infinite GOP
                                # there is no scheduled keyframe to wait for.
                                data_logger.info("Sending PIPELINE_RESETTING to the new viewer and requesting an IDR.")
                                try:
                                    await websocket.send_str("PIPELINE_RESETTING primary")
                                except (ConnectionResetError, OSError, RuntimeError):
                                    pass
                                self._schedule_idr_for_display('primary')

                                continue

                            if display_id != 'primary':
                                second_screen_enabled, _ = self.cli_args.second_screen
                                if not second_screen_enabled:
                                    data_logger.warning(
                                        f"Client from {remote_address} attempted to connect as secondary display ('{display_id}'), "
                                        "but second screens are disabled by server settings. Rejecting connection."
                                    )
                                    try:
                                        await websocket.send_str("KILL Second screens are disabled on this server.")
                                        await websocket.close(code=1008, message=b"Second screens disabled")
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
                                        # Hand the display entry to this socket BEFORE the close
                                        # below yields: the superseded handler's cleanup only tears
                                        # down an entry its own socket still owns, so without the
                                        # handoff it deletes the display and stops the capture this
                                        # connection is taking over.
                                        existing_client_info['ws'] = websocket
                                        try:
                                            # The superseded socket is exactly the one most
                                            # likely frozen (that is why the user reconnected);
                                            # bound it or the takeover session hangs here.
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
                                    'use_cpu': self._initial_use_cpu,
                                    'video_paintover_crf': self._initial_video_paintover_crf,
                                    'video_paintover_burst_frames': self._initial_video_paintover_burst_frames,
                                    'use_paint_over_quality': self._initial_use_paint_over_quality,
                                    'rate_control_mode': self.rc_mode.value,
                                    'video_bitrate': self._initial_video_bitrate,
                                    # Seed from the configured DPI so a Wayland
                                    # first load captures at the intended scale
                                    # before any client DPI sync arrives.
                                    'scale': float(getattr(app_settings, "scaling_dpi", "96") or 96) / 96.0,
                                }
                            else:
                                data_logger.info(f"Client is taking over existing display '{display_id}'. Updating state for new connection.")
                                display_state = self.display_clients[display_id]
                                display_state['ws'] = websocket
                                display_state['video_active'] = True
                                display_state['acknowledged_frame_id'] = -1
                                display_state['last_ack_update_time'] = time.monotonic()
                                display_state['sent_timestamps'].clear()
                                display_state['rtt_samples'].clear()
                                display_state['smoothed_rtt'] = 0.0
                                # A warm takeover keeps the running capture: hand the
                                # rejoining page a decoder reset and a keyframe, since
                                # no reconfigure runs when its dimensions are unchanged.
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
                                        # Audio already running for a prior client: a
                                        # newly joined client may flip the shared RED
                                        # gate (e.g. a non-capable viewer forces plain
                                        # frames), so re-gate under the same guard.
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
                            # Sent ids are masked to uint16 (& 0xFFFF), so any ack
                            # outside that wire space is a protocol violation. The
                            # -1 'no ACK yet' sentinel is server-internal: accepting
                            # it (or any out-of-range int) from the wire would let a
                            # client disable backpressure and the stall detector, or
                            # skew the circular desync arithmetic.
                            if not (0 <= acked_frame_id <= MAX_UINT16_FRAME_ID):
                                raise ValueError("ACK frame id outside uint16 wire space.")

                            display_state = self.display_clients.get(target_display_id)
                            if display_state:
                                display_state['acknowledged_frame_id'] = acked_frame_id
                                display_state['last_ack_update_time'] = time.monotonic()
                                
                                sent_ts = display_state.get('sent_timestamps')
                                if sent_ts and acked_frame_id in sent_ts:
                                    send_time = sent_ts.pop(acked_frame_id)
                                    rtt_sample_ms = (time.monotonic() - send_time) * 1000.0
                                    # The id space is uint16 and restarts on pipeline
                                    # resets, so an ack can match a stamp from a much
                                    # older frame; such a "sample" is id-collision
                                    # arithmetic, not a round trip.
                                    if 0 <= rtt_sample_ms <= RTT_SAMPLE_SANE_MAX_MS:
                                        rtt_samples = display_state.get('rtt_samples')
                                        if rtt_samples is not None:
                                            rtt_samples.append(rtt_sample_ms)
                                            if rtt_samples:
                                                display_state['smoothed_rtt'] = sum(rtt_samples) / len(rtt_samples)
                        except (IndexError, ValueError):
                            data_logger.warning(f"Malformed CLIENT_FRAME_ACK from {raddr}: {message}")

                    elif message == "START_VIDEO":
                        # Resuming from pause (tab visible again) un-pauses ANY
                        was_paused = websocket in self.video_paused_clients
                        perms = client_permissions.get(websocket)
                        if perms and perms.get("role") == "viewer":
                            # monotonic (not wall-clock) so an NTP/clock jump
                            # can't wedge the resume floor or the 30s throttle.
                            now = time.monotonic()
                            if was_paused:
                                # Resume bypasses the 30s redundant-request
                                # throttle (a real state change), but keeps a short
                                # floor: each resume forces an IDR resync, so rapid
                                # STOP/START must not spam keyframes. A throttled
                                # rapid resume stays paused this cycle (not rejoined
                                # below) and retries after the floor.
                                last_req_time = self.last_start_video_request_times.get(websocket, 0)
                                if now - last_req_time < VIEWER_RESUME_MIN_INTERVAL_S:
                                    # The client already flipped to "resumed": a
                                    # silent discard would leave this socket
                                    # paused until its watchdog. Rejoin after the
                                    # floor instead of dropping the resume.
                                    data_logger.warning(f"Throttled rapid resume from viewer {remote_address}; deferring its rejoin.")
                                    self._schedule_deferred_viewer_rejoin(
                                        websocket,
                                        VIEWER_RESUME_MIN_INTERVAL_S - (now - last_req_time),
                                    )
                                    continue
                                self.last_start_video_request_times[websocket] = now
                            else:
                                # Short floor only: a viewer whose stream stalled
                                # re-requests via its watchdog and must not wait
                                # tens of seconds for a resync.
                                last_req_time = self.last_start_video_request_times.get(websocket, 0)
                                if now - last_req_time < 5.0:
                                    data_logger.warning(f"Throttled START_VIDEO request from viewer {remote_address}. Ignoring.")
                                    continue
                                self.last_start_video_request_times[websocket] = now

                        # Un-pause AFTER the throttle decision, so a throttled rapid
                        # resume stays paused (no rejoin, no resync). Role-agnostic:
                        # a non-viewer collaborator that paused must also rejoin, or
                        # it stays out of the broadcast forever.
                        if was_paused:
                            self.video_paused_clients.discard(websocket)
                            data_logger.info(f"START_VIDEO from resuming client ({remote_address}): rejoining its video feed.")

                        display_entry = self.display_clients.get(client_display_id) if client_display_id else None
                        if display_entry is not None and display_entry.get('ws') is not websocket:
                            # Display-wide toggles are honored only from the socket that
                            # currently owns the display: a superseded connection (page
                            # reload overlap) must not drive its successor's stream.
                            data_logger.info(f"Ignoring START_VIDEO for '{client_display_id}' from a superseded connection.")
                        elif display_entry is not None:
                            data_logger.info(f"Received START_VIDEO for '{client_display_id}'. Starting its stream.")
                            display_state = display_entry
                            # A resume onto a capture that kept running for the
                            # shared viewers continues mid-GOP: this socket needs
                            # the viewer resume contract (decoder reset + IDR).
                            resumed_onto_live_capture = (
                                was_paused and client_display_id in self.capture_instances
                            )
                            # Keep this intent flag write sync-adjacent to the capture start below:
                            # under cooperative asyncio there is no await between them, so the flag
                            # and the lock-serialized capture op stay atomic vs a concurrent
                            # reconfigure. Do not insert an await before the start call.
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
                                        # Capture failed to start (logged in the impl). Fall back to a
                                        # full reconfigure instead of falsely telling the client VIDEO_STARTED.
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
                                        # Resend cursor: the client clears its cursor canvas on tab hide.
                                        await self.send_current_cursor(websocket, remote_address)
                                except Exception as e:
                                    data_logger.error(f"Failed to restart individual stream for '{client_display_id}': {e}", exc_info=True)
                                    await self.reconfigure_displays()
                            else:
                                data_logger.warning(f"No layout found for '{client_display_id}' on START_VIDEO. Performing full reconfiguration.")
                                await self.reconfigure_displays()
                                # Only ack VIDEO_STARTED if the reconfigure actually brought up a
                                # live capture for this display; otherwise the client believes the
                                # stream is running with no pipeline behind it.
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
                            # A shared client (re)joining needs only a decode entry point,
                            # not a pipeline rebuild: reset just that client and request an
                            # IDR from the running capture. Rebuild only if nothing runs.
                            if 'primary' in self.capture_instances:
                                data_logger.info(f"START_VIDEO from shared client ({remote_address}): sending reset + IDR.")
                                try:
                                    await websocket.send_str("PIPELINE_RESETTING primary")
                                except (ConnectionResetError, OSError, RuntimeError):
                                    pass
                                self._schedule_idr_for_display('primary')
                                # Shared clients clear their cursor canvas on tab hide too.
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
                                    # Owner-driven fallback; harmless no-op with zero display clients.
                                    await self.reconfigure_displays()

                    elif message == "STOP_VIDEO":
                        stop_entry = self.display_clients.get(client_display_id) if client_display_id else None
                        if stop_entry is not None and stop_entry.get('ws') is not websocket:
                            # A dying page's tab-hide STOP_VIDEO can arrive on the superseded
                            # socket after the reloaded page already owns the display: honor
                            # display-wide stops only from the owning socket.
                            data_logger.info(f"Ignoring STOP_VIDEO for '{client_display_id}' from a superseded connection.")
                            try:
                                await websocket.send_str("VIDEO_STOPPED")
                            except (ConnectionResetError, OSError, RuntimeError):
                                pass
                        elif stop_entry is not None:
                            # The primary's capture feeds every shared viewer:
                            # the controller hiding its tab must not stop the
                            # encoder while viewers still consume the broadcast
                            # — pause just the controller's socket instead (the
                            # fan-out already excludes video_paused_clients).
                            display_sockets = {
                                info.get('ws') for info in self.display_clients.values()
                            }
                            remaining_viewers = (
                                self.clients - display_sockets - self.video_paused_clients
                            ) if client_display_id == 'primary' else set()
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
                            # A shared client pausing (the web client sends
                            # STOP_VIDEO on tab hide): drop just this socket from
                            # the primary video broadcast. Capture keeps running
                            # for everyone else, and the socket stays connected for
                            # control, cursor, and audio.
                            self.video_paused_clients.add(websocket)
                            data_logger.info(f"STOP_VIDEO from shared client ({remote_address}): pausing its video feed.")
                            try:
                                await websocket.send_str("VIDEO_STOPPED")
                            except (ConnectionResetError, OSError, RuntimeError):
                                pass

                    elif message == "REQUEST_KEYFRAME":
                        # Client requests an IDR (e.g. decoder recreated, viewer resync).
                        # Rate-limited per display; viewers get a stricter per-socket
                        # throttle since any number of them can share one stream.
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
                                data_logger.info(f"Keyframe requested by {remote_address} for '{target_display_id}'.")
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
                                    await websocket.send_str("AUDIO_DISABLED")
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
                                    await websocket.send_str("AUDIO_DISABLED")
                        # Track per-connection: a re-request supersedes the pending
                        # one, and disconnect cleanup cancels whatever is in flight.
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
                        # Bounded: this is THE loop that would process the initial
                        # SETTINGS; waiting on it unbounded here deadlocks the
                        # session if a client emits r, first. Resolutions are
                        # re-asserted after SETTINGS anyway, so dropping is safe.
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
                            dpi_value = int(dpi_value_str)
                            if app_settings._overridden.get("scaling_dpi", False):
                                # An operator-set DPI (CLI/env) governs the desktop:
                                # client DPI syncs must not clobber it.
                                data_logger.info("Ignoring client DPI sync: scaling_dpi is operator-overridden.")
                                continue

                            scale_val = float(dpi_value) / 96.0

                            data_logger.info(f"Received DPI setting from client: {dpi_value} (Scale: {scale_val})")

                            if await set_dpi(dpi_value):
                                data_logger.info(f"Successfully set DPI to {dpi_value}")
                            else:
                                data_logger.error(f"Failed to set DPI to {dpi_value}")

                            if IS_WAYLAND and client_display_id:
                                # DPI realizes as the pixelflux compositor output
                                # scale, always (WebRTC-mode parity): thread it into
                                # display state and restart the capture so the
                                # compositor reconfigures its output. (No kwin /
                                # wlr-randr detection: pixelflux owns the session
                                # and implements no wlr-output-management, so no
                                # external tool can apply the scale.)
                                if client_display_id in self.display_clients:
                                    self.display_clients[client_display_id]['scale'] = scale_val
                                self._update_wayland_cursor_cap(dpi_value)
                                data_logger.info(f"Wayland: restarting stream with scale {scale_val} for {client_display_id}")
                                await self._stop_capture_for_display(client_display_id)
                                if hasattr(self, 'display_layouts') and client_display_id in self.display_layouts:
                                    layout = self.display_layouts[client_display_id]
                                    await self._start_capture_for_display(
                                        display_id=client_display_id,
                                        width=layout['w'], height=layout['h'],
                                        x_offset=layout['x'], y_offset=layout['y']
                                    )
                                    await self._start_backpressure_task_if_needed(client_display_id)
                                    # Close the realized-geometry loop: the compositor
                                    # may clamp (even-masking, GBM degrade); the client
                                    # reconciles from stream_resolution like X11.
                                    await self._sync_wayland_realized_geometry(client_display_id)

                            if CURSOR_SIZE > 0:
                                if IS_WAYLAND:
                                    await self._apply_wayland_cursor_size(dpi_value)
                                else:
                                    new_cursor_size = cursor_size_for_dpi(dpi_value, CURSOR_SIZE)

                                    data_logger.info(f"Attempting to set cursor size to: {new_cursor_size} (based on DPI {dpi_value})")
                                    if await set_cursor_size(new_cursor_size):
                                        data_logger.info(f"Successfully set cursor size to {new_cursor_size}")
                                    else:
                                        data_logger.error(f"Failed to set cursor size to {new_cursor_size}")
                            else:
                                data_logger.warning("CURSOR_SIZE is not positive. Skipping cursor size adjustment based on DPI.")

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

                        # Secure mode: 'cmd' needs input authority (active mk-token
                        # holder, or a controller when no mk-token is set).
                        if self.is_secure_mode:
                            cmd_perms = client_permissions.get(websocket)
                            cmd_token = cmd_perms.get("token") if cmd_perms else None
                            if active_mk_token is not None:
                                if cmd_token != active_mk_token:
                                    data_logger.warning(f"BLOCK (Secure Mode): 'cmd' from {remote_address} dropped; client is not the active controller.")
                                    continue
                            else:
                                cmd_role = cmd_perms.get("role") if cmd_perms else "viewer"
                                if cmd_role != "controller":
                                    data_logger.warning(f"BLOCK (Secure Mode): 'cmd' from {remote_address} dropped; client is not a controller.")
                                    continue

                        toks = message.split(',')
                        if len(toks) > 1:
                            command_to_run = ",".join(toks[1:])
                            data_logger.info(f"Attempting to execute command: '{command_to_run}'")
                            home_directory = os.path.expanduser("~")
                            try:
                                process = await subprocess.create_subprocess_shell(
                                    command_to_run,
                                    stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL,
                                    cwd=home_directory
                                )
                                data_logger.info(f"Successfully launched command: '{command_to_run}' with PID {process.pid}")
                            except Exception as e:
                                data_logger.error(f"Failed to launch command '{command_to_run}': {e}")
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

                        # maxsplit=1: the gate needs only the verb, and a clipboard chunk can be
                        # WS_MAX_MESSAGE_BYTES (8 MiB) — a full split here (then again in the
                        # dispatcher) would stall the event loop for milliseconds per large paste.
                        # 'cr' (read-only clipboard fetch) is exempt: every client sends it at
                        # connect before it can hold input authority, and the handler itself
                        # direction-gates it on enable_clipboard (out). Viewer-role drops above
                        # still apply.
                        if self.is_secure_mode and message.split(',', 1)[0] in ["kd", "ku", "kh", "kr", "m", "m2", "co", "cws", "cbs", "cwd", "cbd", "cwe", "cbe", "cw", "cb", "REQUEST_CLIPBOARD"]:
                            perms = client_permissions.get(websocket)
                            token = perms.get("token") if perms else None
                            
                            if active_mk_token is not None:
                                if token != active_mk_token:
                                    continue
                            else:
                                role = perms.get("role") if perms else "viewer"
                                if role != "controller":
                                    continue
 
                        if self.input_handler and hasattr(
                            self.input_handler, "on_message"
                        ):
                            # Pass a per-connection id so clipboard debounce is per-connection,
                            # not per-display (one client's copy must not suppress another's read).
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
            rejoin_task = self._deferred_viewer_rejoins.pop(websocket, None)
            if rejoin_task is not None:
                rejoin_task.cancel()
            client_permissions.pop(websocket, None)
            data_logger.info(f"Cleaning up Data WS handler for {raddr} (Display ID: {client_display_id})...")

            self.clients.discard(websocket)
            if self.data_ws is websocket:
                self.data_ws = None
            # Drop this client's RED capability and re-gate: a departing
            # non-capable client may now let the remaining clients enable RED
            # (or the last client leaving resets the gate to 0).
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
                # Defer the display teardown by a short grace window: a reloading
                # page reconnects within a second or two and takes the entry over
                # with its capture still warm (the takeover path hands it a reset
                # and an IDR). Tearing down here would serialize the successor's
                # startup behind this handler's reconfigure/shutdown on the
                # reconfigure lock — seconds of black stream on every reload. If
                # nobody claims the display, the teardown runs after the grace.
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
                        # A connection newer than the disconnect is a page that may
                        # still be mid-handshake (its audio setup runs before it can
                        # claim the display): hold the teardown for it until the
                        # deadline, so an unclaimed display still tears down.
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
                    # reconfigure above never stops it: stop it when the last
                    # consumer leaves (no-op when a controller's reconfigure did).
                    if not self.clients and not self.display_clients and 'primary' in self.capture_instances:
                        data_logger.info("Last consumer disconnected; stopping viewer-started primary capture.")
                        await self._stop_capture_for_display('primary')
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
                        # shutdown_pipelines() -> reconfigure_displays() acquires
                        # _reconfigure_lock itself; it must not be held here.
                        await self.shutdown_pipelines()

                _teardown_task = asyncio.create_task(_teardown_if_unclaimed())
                self._display_teardown_tasks.add(_teardown_task)
                _teardown_task.add_done_callback(self._display_teardown_tasks.discard)
            else:
                data_logger.info(f"Unregistered client at {raddr} disconnected. No display reconfiguration needed.")
                # A viewer-started capture has no owning display client, so
                # nothing else stops it: stop it when the last consumer leaves.
                if not self.clients and not self.display_clients and 'primary' in self.capture_instances:
                    data_logger.info("Last consumer disconnected; stopping viewer-started primary capture.")
                    await self._stop_capture_for_display('primary')

            # Cancel only the per-connection tasks; the singleton collectors
            # are torn down on last-client disconnect (cancelling here breaks remaining clients).
            monitor_tasks = [
                stats_sender_task_ws,
                start_audio_task_ws,
            ]
            for _task_to_cancel in monitor_tasks:
                if _task_to_cancel and not _task_to_cancel.done():
                    _task_to_cancel.cancel()
                    try:
                        await _task_to_cancel
                    except asyncio.CancelledError:
                        pass

            if (
                self._frame_backpressure_task
                and not self._frame_backpressure_task.done()
            ):
                if (
                    not self.clients
                ):
                    data_logger.info(
                        f"Last client ({raddr}) disconnected. Cancelling frame backpressure task."
                    )
                else:
                    data_logger.info(
                        f"Client {raddr} disconnected, but other clients remain. Frame backpressure task continues."
                    )

            # Rust mic playback (if used) owns its PA stream on a worker thread; stop()
            # joins it and releases the sink deterministically (UAF-safe internally).
            # Offload the join so a slow PA disconnect can't block the event loop.
            _mic_playback = locals().get("mic_playback")
            if _mic_playback is not None:
                try:
                    await asyncio.to_thread(_mic_playback.stop)
                    data_logger.debug(f"Stopped Rust mic playback for {raddr}.")
                except Exception as e_mic_pb:
                    data_logger.error(f"Error stopping Rust mic playback for {raddr}: {e_mic_pb}")

            if "pulse" in locals() and locals()["pulse"]:
                _local_pulse = locals()["pulse"]
                if (
                    "pa_module_index" in locals()
                    and locals()["pa_module_index"] is not None
                    and locals().get("pa_module_owned")
                ):
                    _local_pa_module_index = locals()["pa_module_index"]
                    try:
                        data_logger.info(
                            f"Unloading PulseAudio module {_local_pa_module_index} for virtual mic (client: {raddr})."
                        )
                        await _local_pulse.module_unload(_local_pa_module_index)
                    except Exception as e_unload_final:
                        data_logger.error(
                            f"Error unloading PulseAudio module {_local_pa_module_index} for {raddr}: {e_unload_final}"
                        )
                try:
                    _local_pulse.close()
                    data_logger.debug(f"Closed PulseAudio connection for {raddr}.")
                except Exception as e_pulse_close:
                    data_logger.error(
                        f"Error closing PulseAudio connection for {raddr}: {e_pulse_close}"
                    )


            if self.input_handler:
                try:
                    await self.input_handler.reset_keyboard()
                    data_logger.info(f"Keyboard reset completed ({raddr}) disconnect.")
                except Exception as e_reset:
                    data_logger.warning(f"Failed to reset keyboard after client disconnect: {e_reset}")

            # For a display-owning socket this teardown is deferred into the
            # reconnect-grace task above; run it here only for clients that
            # never owned a display (viewers, unregistered sockets).
            if disconnected_display_id is None and not self.clients:
                 data_logger.info(f"Last client ({raddr}) disconnected. All pipelines should have been stopped by reconfigure_displays.")
                 # Tear down the singleton collectors only on last client; null each
                 # ref so a fast reconnect restarts them (cancel is async).
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
                 # Last client gone: clear per-display IDR timestamps so the dict
                 # can't grow unbounded across distinct display ids.
                 self._last_keyframe_request.clear()
                 # Clear the singleton stats dicts too (collectors now use get(), not
                 # pop()), so a reconnect doesn't briefly read stale/dead-collector stats.
                 self._shared_stats_ws.clear()
                 self._shared_network_stats.clear()
                 # shutdown_pipelines() -> reconfigure_displays() acquires
                 # _reconfigure_lock itself; it must not be held here.
                 await self.shutdown_pipelines()

            data_logger.info(f"Data WS handler for {raddr} finished all cleanup.")

    async def _run_detached_command(self, cmd_list: list, description: str):
        """Runs a command via the shell using 'nohup ... &' to detach it from the server process."""
        quoted_cmd = ' '.join(shlex.quote(c) for c in cmd_list)
        shell_command = f"nohup {quoted_cmd} &"
        data_logger.info(f"Running detached command ({description}): {shell_command}")
        try:
            await asyncio.create_subprocess_shell(
                shell_command,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL
            )
        except Exception as e:
            data_logger.error(f"Failed to run detached command '{shell_command}': {e}")

    async def _run_command(self, cmd, description, best_effort=False):
        """Helper to run a shell command and log its output/errors. best_effort=True
        logs a non-zero exit at DEBUG instead of ERROR — for delete-if-exists cleanups
        that fail only because the target is already gone."""
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

    def _wayland_control_module(self):
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

    async def _drop_wayland_secondary(self, display_id: str, reason: str):
        """Refuse a secondary display the compositor cannot realize: destroy its
        output (if any), stop its capture, unregister it, and kill its client —
        the Wayland counterpart of the X11 unrealizable-extension drop."""
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

    async def _reanchor_wayland_primary(self, layouts, keep_ids):
        """Collapse an unrealizable Wayland arrangement: primary back at the
        origin (layout + capture rebuild) — the Wayland mirror of the X11
        re-anchor when the extension does not fit the realized root."""
        primary_layout = layouts.get('primary')
        if primary_layout:
            primary_layout['x'], primary_layout['y'] = 0, 0
        if 'primary' in keep_ids:
            keep_ids.discard('primary')
            await self._stop_capture_for_display('primary')

    async def _apply_wayland_output_layout(self, layouts, keep_ids):
        """Realize the computed union layout as compositor outputs — the Wayland
        counterpart of the X11 monitor/framebuffer apply. The primary (output 0)
        is sized by its capture start and MOVED here to its layout offset
        ('left'/'up' place it off-origin; teardown re-anchors it at 0,0); each
        secondary gets a real output at its layout rectangle, created here
        BEFORE the capture start loop binds a capture to it. Ordering keeps the
        output rectangles disjoint: moved/stale secondaries are destroyed
        first, then the primary repositions, then secondaries are created.
        Mutates `layouts`/`keep_ids` when a display has to be dropped (output
        creation or the primary move refused), killing its client like the X11
        path."""
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
        for oid, did in wanted.items():
            layout = layouts[did]
            existing = outputs.get(oid)
            if existing is not None and (existing[1], existing[2]) != (layout['x'], layout['y']):
                # A secondary reposition is a destroy + recreate (its capture
                # dies with the output; the start loop rebuilds it). Destroyed
                # before the primary moves so their rectangles never overlap.
                data_logger.info(
                    f"Wayland output {oid} moves to +{layout['x']}+{layout['y']}; recreating it."
                )
                await asyncio.to_thread(module.destroy_output, oid)
                keep_ids.discard(did)
                await self._stop_capture_for_display(did)
                outputs.pop(oid, None)
        primary_layout = layouts.get('primary')
        target = (primary_layout['x'], primary_layout['y']) if primary_layout else (0, 0)
        existing0 = outputs.get(0)
        current = (existing0[1], existing0[2]) if existing0 is not None else (0, 0)
        if target != current:
            if not await wayland_reposition_primary(module, target[0], target[1]):
                # The primary must stay usable above all else: keep it at the
                # origin and drop every secondary — the arrangement is void.
                await self._reanchor_wayland_primary(layouts, keep_ids)
                for did in [d for d in list(layouts) if d != 'primary']:
                    del layouts[did]
                    keep_ids.discard(did)
                    await self._drop_wayland_secondary(
                        did, "The compositor cannot move the primary output for this arrangement."
                    )
                return
        for oid, did in wanted.items():
            if did not in layouts:
                continue
            layout = layouts[did]
            client = self.display_clients.get(did) or {}
            scale = float(client.get('scale', 1.0) or 1.0)
            if outputs.get(oid) is not None:
                continue
            created = False
            try:
                created = bool(await asyncio.to_thread(
                    module.create_output, oid,
                    layout['w'], layout['h'], layout['x'], layout['y'], scale,
                ))
            except Exception as e:
                data_logger.error(f"Wayland create_output {oid} failed: {e}")
            if not created:
                del layouts[did]
                keep_ids.discard(did)
                await self._drop_wayland_secondary(
                    did, "The compositor cannot create an output for this display."
                )
                if target != (0, 0):
                    # The secondary this arrangement was built around is gone;
                    # put the primary back at the origin.
                    await wayland_reposition_primary(module, 0, 0)
                    await self._reanchor_wayland_primary(layouts, keep_ids)

    async def _stop_capture_for_display(self, display_id: str):
        # Serialize against any concurrent start/stop for any display.
        async with self._video_capture_lock:
            await self._stop_capture_for_display_impl(display_id)

    async def _stop_capture_for_display_impl(self, display_id: str):
        """Stops the capture, sender, and backpressure tasks for a single, specific display."""
        data_logger.info(f"Stopping all streams for display '{display_id}'...")
        await self._ensure_backpressure_task_is_stopped(display_id)
        capture_info = self.capture_instances.pop(display_id, None)
        if capture_info:
            capture_module = capture_info.get('module')
            if capture_module:
                await asyncio.to_thread(capture_module.stop_capture)
            sender_task = capture_info.get('sender_task')
            if sender_task and not sender_task.done():
                sender_task.cancel()
        self.video_chunk_queues.pop(display_id, None)

        data_logger.info(f"Successfully stopped all streams for display '{display_id}'.")
 
    @contextlib.asynccontextmanager
    async def _reconfigure_guard(self):
        """Hold _reconfigure_lock for a DIRECT critical section (e.g. audio pipeline ops) and,
        on release, honor any reconfigure that was coalesced (via _reconfigure_pending) while
        we held it. reconfigure_displays()'s own re-run loop only consumes requests that arrive
        THROUGH reconfigure_displays(); without this, a reconfigure coalesced during a direct
        hold would be stranded (e.g. orphaning a disconnected display's capture)."""
        try:
            async with self._reconfigure_lock:
                yield
        finally:
            # Must run on raising exits too, and outside the lock:
            # reconfigure_displays() only coalesces while the lock is held.
            if self._reconfigure_pending:
                await self.reconfigure_displays()

    async def reconfigure_displays(self):
        """
        Central logic to create a virtual desktop for ALL connected clients.
        It then starts capture pipelines ONLY for clients with 'video_active' = True.
        This is called on connect, disconnect, or settings change.
        """
        if self._reconfigure_lock.locked():
            # A pass is already running; flag it to run again with the latest
            # state once it finishes (last-write-wins coalescing).
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
            # Run another pass if requests were coalesced while the lock was
            # held; checked on every exit path (a pass may return early).
            if not self._reconfigure_pending:
                break

    async def _signal_all_displays_stopped(self):
        """Send VIDEO_STOPPED to clients on a reconfiguration abort WITHOUT clearing
        video_active: a transient abort (zero size / no screen_name / failed newmode)
        must not permanently stop healthy displays, so the next successful reconfigure
        auto-restarts the streams the user actually wanted."""
        # Snapshot items: send_str() awaits inside the loop and a concurrent
        # connect/disconnect would otherwise raise "dict changed size during iteration".
        for display_id, client_data in list(self.display_clients.items()):
            # Re-validate against the live dict: the entry may have been removed
            # by a disconnect during a prior iteration's await.
            if self.display_clients.get(display_id) is not client_data:
                continue
            ws = client_data.get('ws')
            if ws:
                try:
                    # Bounded: this runs under _reconfigure_lock; a frozen
                    # client's socket gets dropped, never waited on.
                    await asyncio.wait_for(ws.send_str("VIDEO_STOPPED"), timeout=2.0)
                    # Remember that this client was told the pipeline stopped (it sets
                    # isVideoPipelineActive=false and discards frames). A later successful
                    # reconfigure must send VIDEO_STARTED so it re-accepts frames.
                    client_data['stop_signaled'] = True
                except asyncio.TimeoutError:
                    # The cancelled send may have left a half-written frame; the
                    # socket must not be reused.
                    _close_abandoned_ws(ws)
                except (ConnectionResetError, OSError, RuntimeError):
                    pass

    async def _reconfigure_displays_locked(self):
        """One reconfiguration pass. Must only be called by reconfigure_displays()
        with _reconfigure_lock held; early returns here abort just this pass."""
        current_display_count = len(self.display_clients)
        if not IS_WAYLAND and self._wm_swap_is_supported is None:
            if which("xfce4-session") or which("startplasma-x11"):
                self._wm_swap_is_supported = True
            else:
                self._wm_swap_is_supported = False
        if (not IS_WAYLAND and current_display_count > 1 and self._wm_swap_is_supported and not self._is_wm_swapped):
            if "openbox" in (await current_wm_name()).lower():
                data_logger.info("Multi-monitor setup: Openbox already manages the session; no WM swap.")
            else:
                data_logger.info("Multi-monitor setup: switching to Openbox.")
                # Openbox resolves its stock config chain (user/system rc.xml):
                # a hand-written minimal config would strip the stock <mouse>
                # bindings (titlebar double-click maximize, middle-click,
                # menus) that the compiled-in defaults do not cover, leaving
                # presses on decorations dead.
                await self._run_detached_command(["openbox", "--replace"], "switch to openbox")
                # The takeover must finish BEFORE the layout applies: the
                # incoming WM snapshots the monitor set it starts against, and
                # a snapshot taken mid-swap re-tiles maximized windows across
                # the whole framebuffer.
                if not await wait_for_wm("openbox"):
                    data_logger.warning("Openbox takeover not confirmed; applying layout anyway.")
            self._is_wm_swapped = True
        if not self.display_clients:
            # Nothing to lay out: stop everything that is still running.
            for display_id in list(self.capture_instances.keys()):
                await self._stop_capture_for_display(display_id)
            data_logger.warning("No display clients connected. Video pipelines remain stopped.")
            if not IS_WAYLAND:
                # X11 only: tear down the xrandr virtual monitors. In Wayland the compositor
                # output is managed by pixelflux (resized on capture start), so there is nothing
                # to delmonitor here and running xrandr against the X server is wrong.
                await clear_selkies_monitors()
            else:
                # No clients left: retire every secondary compositor output (the
                # primary output persists; its capture is already stopped above).
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
                if position == 'right':
                    layouts['primary'] = {'x': 0, 'y': 0, 'w': p_w, 'h': p_h}
                    layouts[secondary_id] = {'x': p_w, 'y': 0, 'w': s_w, 'h': s_h}
                    total_width, total_height = p_w + s_w, max(p_h, s_h)
                elif position == 'left':
                    layouts[secondary_id] = {'x': 0, 'y': 0, 'w': s_w, 'h': s_h}
                    layouts['primary'] = {'x': s_w, 'y': 0, 'w': p_w, 'h': p_h}
                    total_width, total_height = p_w + s_w, max(p_h, s_h)
                elif position == 'down':
                    layouts['primary'] = {'x': 0, 'y': 0, 'w': p_w, 'h': p_h}
                    layouts[secondary_id] = {'x': 0, 'y': p_h, 'w': s_w, 'h': s_h}
                    total_width, total_height = max(p_w, s_w), p_h + s_h
                elif position == 'up':
                    layouts[secondary_id] = {'x': 0, 'y': 0, 'w': s_w, 'h': s_h}
                    layouts['primary'] = {'x': 0, 'y': s_h, 'w': p_w, 'h': p_h}
                    total_width, total_height = max(p_w, s_w), p_h + s_h
        if total_width == 0 or total_height == 0:
            data_logger.error("Calculated total display size is zero. Aborting reconfiguration.")
            await self._signal_all_displays_stopped()
            return
        aligned_total_width = (total_width + 7) & ~7
        if aligned_total_width != total_width:
            data_logger.info(f"Aligned total width from {total_width} to {aligned_total_width} for xrandr.")
            total_width = aligned_total_width
        self.display_layouts = layouts
        data_logger.info(f"Layout calculated: Total Size={total_width}x{total_height}. Layouts: {layouts}")

        # Decide, per running capture, whether it can follow the new layout in place:
        # it must still be laid out, wanted, alive, and structurally identical (same
        # encoder/chroma/RC mode -- geometry, rate and tunables all retune live). The
        # rest is stopped here and rebuilt by the start loop below.
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
                            for k in ('output_mode', 'use_cpu', 'use_openh264',
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
                # Native first: a mode made on display_utils' retained connection
                # outlives this call, which per-invocation xrandr cannot guarantee
                # (its mode dies with its own connection on some servers, e.g. Xvfb).
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
                # Re-target live captures while the framebuffer covers BOTH the old and
                # new regions (grow first, shrink after): a region outside the root
                # would fail the grab and kill the capture thread.
                try:
                    cur_w, cur_h = (int(v) for v in curr_res.lower().replace(" ", "").split("x"))
                except (ValueError, AttributeError):
                    cur_w, cur_h = total_width, total_height
                union_w, union_h = max(cur_w, total_width), max(cur_h, total_height)
                if (union_w, union_h) != (total_width, total_height):
                    await grow_framebuffer(union_w, union_h)
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
            # Monitors go in BEFORE the framebuffer change, at their final
            # rectangles, swapped under a server grab: window managers re-tile
            # maximized windows on every root ConfigureNotify (the swap itself
            # emits one, so does the resize below), and must never observe a
            # monitor-less or partial set or windows teleport to the primary.
            # The realized clamp below re-swaps if the server refuses the size.
            await replace_selkies_monitors(layouts, screen_name=screen_name)
            # Skip the framebuffer/mode-set when the screen is already at the
            # target size. A real xrandr mode change is the dominant cost of a
            # reconfigure — it reprograms the CRTC and forces every client to
            # repaint — so on a same-size reconnect/reload it is pure churn.
            # (If a live re-target grew the framebuffer just above, curr_res no
            # longer equals the target, so this correctly still runs to shrink.)
            curr_norm = (curr_res or "").lower().replace(" ", "")
            if curr_norm == total_mode_str:
                data_logger.info(f"Screen already at {total_mode_str}; skipping redundant framebuffer/mode-set.")
            else:
                # Native-first mode+framebuffer apply; the realized-size clamp
                # below reconciles state if the X server refuses it.
                if not await resize_display(total_mode_str):
                    # Some servers refuse runtime mode creation/attachment but
                    # still honor a plain framebuffer grow (RRSetScreenSize):
                    # the output keeps its mode while captures and pointer
                    # warps address the enlarged root.
                    if await grow_framebuffer(total_width, total_height):
                        data_logger.info(f"Mode-set for {total_mode_str} failed; grew the framebuffer instead.")
                    else:
                        data_logger.error(f"Applying mode {total_mode_str} failed; clamping to the realized size below.")
            # The X server, not the request, is the authority on the realized
            # geometry: a driver can reject the mode or framebuffer size and
            # leave the root at its old dimensions, and a capture region
            # outside the root fails or grabs garbage. Re-read the root and
            # clamp the layouts — and the per-display state the resolution
            # broadcast reports — to what was actually realized, so clients
            # render the stream that really exists.
            realized_res, _, _, _, _ = await get_new_res("1x1")
            try:
                realized_w, realized_h = (
                    int(v) for v in (realized_res or "").lower().replace(" ", "").split("x")
                )
            except (ValueError, AttributeError):
                realized_w, realized_h = total_width, total_height
            if (realized_w > 0 and realized_h > 0
                    and (realized_w, realized_h) != (total_width, total_height)):
                data_logger.warning(
                    f"Realized screen size {realized_w}x{realized_h} differs from target "
                    f"{total_width}x{total_height}; clamping display layouts to it."
                )
                # A primary that no longer fits at its offset (secondary placed
                # left/up on a server that refused the grow — fully outside OR
                # truncated to a sliver) must stay usable above all else:
                # re-anchor it at the origin. The arrangement is void then, so
                # every secondary is dropped below.
                primary_layout = layouts.get('primary')
                extension_unrealizable = bool(primary_layout) and (
                    (primary_layout['x'] > 0 and primary_layout['x'] + primary_layout['w'] > realized_w)
                    or (primary_layout['y'] > 0 and primary_layout['y'] + primary_layout['h'] > realized_h)
                )
                if extension_unrealizable:
                    data_logger.error(
                        f"Primary at +{primary_layout['x']}+{primary_layout['y']} does not fit the "
                        f"realized {realized_w}x{realized_h} root; re-anchoring it at the origin."
                    )
                    primary_layout['x'], primary_layout['y'] = 0, 0
                    if 'primary' in keep_ids:
                        # The kept capture was re-targeted to the void offset;
                        # rebuild it at the re-anchored region instead.
                        keep_ids.discard('primary')
                        await self._stop_capture_for_display('primary')
                for did, layout in list(layouts.items()):
                    if did != 'primary' and (extension_unrealizable
                                             or layout['x'] >= realized_w
                                             or layout['y'] >= realized_h):
                        # The region lies entirely outside the realized root (or
                        # the arrangement collapsed): no capture can grab it and
                        # no pointer warp can reach it (clicks would clamp to
                        # the primary's edge). Refuse the display instead of
                        # leaving a broken sliver laid out.
                        data_logger.error(
                            f"Display '{did}' at +{layout['x']}+{layout['y']} does not fit the "
                            f"realized {realized_w}x{realized_h} root; dropping it. The X server "
                            "must allow a framebuffer covering all displays (e.g. a larger Xvfb "
                            "-screen) for extended layouts."
                        )
                        del layouts[did]
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
                        continue
                    clamped_w = max(2, min(layout['w'], realized_w - layout['x']) & ~1)
                    clamped_h = max(2, min(layout['h'], realized_h - layout['y']) & ~1)
                    if (clamped_w, clamped_h) == (layout['w'], layout['h']):
                        continue
                    data_logger.warning(
                        f"Display '{did}': layout {layout['w']}x{layout['h']} clamped to "
                        f"{clamped_w}x{clamped_h} inside the realized root."
                    )
                    layout['w'], layout['h'] = clamped_w, clamped_h
                    client_data = self.display_clients.get(did)
                    if client_data:
                        client_data['width'], client_data['height'] = clamped_w, clamped_h
                    if did == 'primary':
                        self.app.display_width = clamped_w
                        self.app.display_height = clamped_h
                    # A capture kept alive across this pass was re-targeted to the
                    # pre-clamp region; move it inside the realized root or rebuild it.
                    inst = self.capture_instances.get(did)
                    if did in keep_ids and inst and inst.get('module'):
                        try:
                            inst['module'].update_capture_region(
                                layout['x'], layout['y'], clamped_w, clamped_h
                            )
                            inst['settings'] = self._get_capture_settings(
                                did, clamped_w, clamped_h, layout['x'], layout['y']
                            )
                        except Exception as e:
                            data_logger.warning(
                                f"Re-target to clamped region failed for '{did}' ({e}); restarting it."
                            )
                            keep_ids.discard(did)
                            await self._stop_capture_for_display(did)
                # One atomic re-swap to the clamped layouts (dropped displays'
                # monitors disappear with it) — RRSetMonitor cannot redefine an
                # existing name in place.
                await replace_selkies_monitors(layouts, screen_name=screen_name)
        else:
            # Wayland: realize the layout as compositor outputs before the start
            # loop binds a capture to each of them.
            await self._apply_wayland_output_layout(layouts, keep_ids)
        data_logger.info("Starting separate capture instances for each ACTIVE display region...")
        for display_id, layout in layouts.items():
            client_data = self.display_clients.get(display_id)
            if client_data and client_data.get('video_active', False):
                try:
                    if display_id in keep_ids:
                        # Already re-targeted (X11) or restartable live (Wayland): push the
                        # current rates/tunables so settings drift rides along, no rebuild.
                        inst = self.capture_instances[display_id]
                        module, fresh = inst['module'], inst['settings']
                        if IS_WAYLAND:
                            # A start on the live capture reconfigures it in place.
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
                    # If this client was previously told VIDEO_STOPPED (during a transient
                    # abort), it is still discarding frames. Now that capture restarted,
                    # tell it VIDEO_STARTED so it re-enables its pipeline.
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
        if IS_WAYLAND:
            for display_id in list(layouts.keys()):
                client_data = self.display_clients.get(display_id)
                if not (client_data and client_data.get('video_active', False)):
                    continue
                # Barrier + reconcile: the compositor answers the geometry read
                # only after the queued capture start finished, so is_capturing
                # is authoritative afterwards (the broadcast below fans out once).
                await self._sync_wayland_realized_geometry(display_id, broadcast=False)
                inst = self.capture_instances.get(display_id)
                module = inst.get('module') if inst else None
                capturing = False
                if module is not None:
                    try:
                        capturing = bool(module.is_capturing)
                    except Exception:
                        capturing = False
                if not capturing and display_id != 'primary':
                    await self._drop_wayland_secondary(
                        display_id,
                        "The compositor could not start a capture for this display "
                        "(encoder session or GPU resources exhausted).",
                    )
        await self.broadcast_stream_resolution()
        await self.broadcast_display_config()
        data_logger.info("Display reconfiguration finished successfully.")


    async def _video_chunk_sender(self, display_id: str):
        """
        Pulls data from a specific queue, records send timestamp, and sends to the correct client(s).
        """
        data_logger.info(f"Video chunk sender started for display '{display_id}'.")
        queue = self.video_chunk_queues.get(display_id)
        if not queue:
            data_logger.error(f"Cannot start sender for '{display_id}': Queue not found.")
            return

        try:
            while True:
                chunk_info = await queue.get()
                data_chunk = chunk_info['data']
                frame_id = chunk_info['frame_id']
                if display_id == 'primary':
                    secondary_websockets = {
                        client_info.get('ws')
                        for did, client_info in self.display_clients.items()
                        if did != 'primary' and client_info.get('ws')
                    }
                    # Hidden-tab clients (STOP_VIDEO) are excluded from the VIDEO
                    # broadcast only; they stay connected for control/cursor/audio
                    # and rejoin on START_VIDEO with a reset + IDR. (The audio
                    # sender deliberately does NOT subtract this set.)
                    primary_viewers = (self.clients - secondary_websockets
                                       - self.video_paused_clients)

                    if not primary_viewers:
                        queue.task_done()
                        continue
                    # Backpressure throttles the primary CONTROLLER only; shared viewers must
                    # keep receiving frames (don't starve them on one backed-up client). Drop
                    # just the controller from the send set when it's backed up; skip entirely
                    # only when nothing is left to send to.
                    primary_state = self.display_clients.get('primary')
                    pc_ws = primary_state.get('ws') if primary_state is not None else None
                    controller_backed_up = (primary_state is not None
                                            and not primary_state.get('backpressure_enabled', True))
                    if controller_backed_up:
                        send_targets = {ws for ws in primary_viewers if ws is not pc_ws}
                    else:
                        send_targets = primary_viewers
                    if not send_targets:
                        queue.task_done()
                        continue
                    now = time.monotonic()
                    # Only the 'primary' entry feeds the backpressure task; record only when the
                    # controller actually receives this frame (O(1), no scan).
                    if pc_ws is not None and pc_ws in send_targets:
                        primary_state['sent_timestamps'][frame_id] = now
                        primary_state['last_sent_frame_id'] = frame_id
                        primary_state['has_sent_any_frame'] = True
                        if len(primary_state['sent_timestamps']) > SENT_FRAME_TIMESTAMP_HISTORY_SIZE:
                            primary_state['sent_timestamps'].popitem(last=False)
                    try:
                        # Bounded sends: a stalled shared viewer must not freeze
                        # primary video for the controller and other viewers.
                        dropped = await _broadcast_to_clients(
                            send_targets, data_chunk,
                            per_client_timeout=SHARED_STREAM_SEND_TIMEOUT_SECONDS,
                        )
                        self._bytes_sent_in_interval += len(data_chunk) * len(send_targets)
                        if dropped:
                            # send_targets is a per-frame temporary: propagate the
                            # drop to the authoritative registry, or the dead socket
                            # re-enters the fan-out on the very next frame.
                            self.clients -= dropped
                    except Exception as e:
                        data_logger.error(f"Error during primary broadcast: {e}")

                else:
                    client_info = self.display_clients.get(display_id)
                    if not client_info or not client_info.get('ws') or not client_info.get('backpressure_enabled', True):
                        queue.task_done()
                        continue
                    websocket = client_info['ws']
                    now = time.monotonic()
                    client_info['sent_timestamps'][frame_id] = now
                    client_info['last_sent_frame_id'] = frame_id
                    client_info['has_sent_any_frame'] = True
                    if len(client_info['sent_timestamps']) > SENT_FRAME_TIMESTAMP_HISTORY_SIZE:
                        client_info['sent_timestamps'].popitem(last=False)
                    try:
                        # Bounded send, matching the primary path: a wedged secondary
                        # socket must not pin this sender inside the await, where the
                        # backpressure gate (checked only at loop top) can never
                        # preempt it. A cancelled send may leave a half-written
                        # frame, so the socket is dropped and closed, not reused.
                        await asyncio.wait_for(
                            websocket.send_bytes(data_chunk),
                            timeout=SHARED_STREAM_SEND_TIMEOUT_SECONDS,
                        )
                        self._bytes_sent_in_interval += len(data_chunk)
                    except asyncio.TimeoutError:
                        # Checked before OSError: on 3.11+ TimeoutError subclasses it.
                        data_logger.warning(
                            f"Client for '{display_id}' send stalled past "
                            f"{SHARED_STREAM_SEND_TIMEOUT_SECONDS}s; dropping."
                        )
                        _close_abandoned_ws(websocket)
                        break
                    except (ConnectionResetError, OSError, RuntimeError):
                        data_logger.warning(f"Client for '{display_id}' connection closed during send.")
                        break
                queue.task_done()
        except asyncio.CancelledError:
            data_logger.info(f"Video chunk sender for '{display_id}' cancelled.")
        finally:
            data_logger.info(f"Video chunk sender for '{display_id}' finished.")

    async def _ensure_viewer_capture(self):
        """Start the primary capture for a shared/player viewer when no display-
        owning client is connected (fresh server, or the controller left): the
        desktop exists regardless, so a lone viewer must not wait on a controller
        ("Waiting for stream..." forever). Captures the CURRENT desktop geometry —
        viewers never resize anything; the next controller's settings re-layout
        as usual."""
        if 'primary' in self.capture_instances:
            return True
        layout = getattr(self, 'display_layouts', {}).get('primary')
        if layout:
            w, h, x, y = layout['w'], layout['h'], layout['x'], layout['y']
        else:
            w, h = self.app.display_width, self.app.display_height
            x = y = 0
            if not IS_WAYLAND:
                # On X11 the desktop size is external truth; the app defaults may
                # not match it. (On Wayland the capture start sizes the output.)
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
        return started

    async def _start_capture_for_display(self, display_id: str, width: int, height: int, x_offset: int, y_offset: int):
        # Serialize against any concurrent start/stop so the capture_instances
        # guard+insert can't race a still-finishing op.
        async with self._video_capture_lock:
            return await self._start_capture_for_display_impl(display_id, width, height, x_offset, y_offset)

    async def _start_capture_for_display_impl(self, display_id: str, width: int, height: int, x_offset: int, y_offset: int):
        """
        Starts a capture instance by creating the required CaptureSettings
        object and providing a callback with the correct signature.
        """
        # Guard before anything touches the pixelflux classes: with the library
        # missing, _get_capture_settings would otherwise die on CaptureSettings()
        # with a bare TypeError long after the informative import warning scrolled by.
        if not X11_CAPTURE_AVAILABLE:
            raise SelkiesAppError(
                "Cannot start capture: the pixelflux library failed to import "
                "(see the startup warning for the underlying error)."
            )
        existing = self.capture_instances.get(display_id)
        if existing is not None:
            # Only treat an existing instance as "started" if it is genuinely capturing.
            # A stale/dead instance (encoder loop stopped after a tab-sleep/GPU hiccup)
            # would otherwise let us ack VIDEO_STARTED with no live pipeline. Rebuild if dead.
            module = existing.get('module')
            alive = True
            if module is not None:
                try:
                    alive = bool(module.is_capturing)
                except Exception:
                    alive = True  # can't tell -> don't churn a possibly-healthy stream
            if alive:
                # Nudge a fresh IDR so a reconnecting/woken client gets a decodable frame
                # immediately instead of stalling until the next scheduled keyframe.
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

        sender_task = None
        try:
            settings = self._get_capture_settings(display_id, width, height, x_offset, y_offset)

            def queue_data_for_display(frame):
                if frame is None:
                    return
                try:
                    if not len(frame):
                        return

                    # Zero-copy: the frame owns its native buffer and frees it once the
                    # last view/reference drops. Keep the frame itself as `owner` so every
                    # later return/exception (and the WS transport) frees it by dropping it.
                    queue = self.video_chunk_queues.get(display_id)
                    if not queue:
                        return  # frame drops here -> buffer freed
                    # memoryview(frame) is a zero-copy view; it (and the frame) stay alive
                    # until the queue item and every WS transport release them.
                    item_to_queue = {'data': memoryview(frame), 'owner': frame,
                                     # Only the low 16 bits go on the wire (uint16), which
                                     # is what the client ACKs; mask here so sent_timestamps
                                     # RTT lookups and the uint16 circular-distance
                                     # backpressure math keep matching past frame 65535.
                                     'frame_id': frame.frame_id & 0xFFFF}

                    def do_put():
                        try:
                            queue.put_nowait(item_to_queue)
                        except asyncio.QueueFull:
                            # Dropping ANY frame breaks the delta reference chain, and
                            # dropping the newest wedges the stream on stale backlog.
                            # Drain it, keep the fresh frame, and resync with an IDR
                            # so the client never renders against lost references.
                            try:
                                while True:
                                    queue.get_nowait()  # dropped item -> buffer freed
                            except asyncio.QueueEmpty:
                                pass
                            try:
                                queue.put_nowait(item_to_queue)
                            except asyncio.QueueFull:
                                pass
                            self._schedule_idr_for_display(display_id)

                    self.capture_loop.call_soon_threadsafe(do_put)

                except Exception as e:
                    data_logger.error(f"Error in capture callback for {display_id}: {e}", exc_info=False)

            def pixelflux_cursor_handler(msg_type, data_bytes, hot_x, hot_y):
                try:
                    payload = format_pixelflux_cursor(
                        msg_type, data_bytes, hot_x, hot_y, self.cursor_size)
                    if payload is not None:
                        self.app.send_ws_cursor_data(payload)
                except Exception as e:
                    data_logger.error(f"Error handling pixelflux cursor: {e}")

            queue_size = getattr(self, 'BACKPRESSURE_QUEUE_SIZE', 120)
            self.video_chunk_queues[display_id] = asyncio.Queue(maxsize=queue_size)
            sender_task = asyncio.create_task(self._video_chunk_sender(display_id))
            
            capture_module = self._persistent_capture_modules.get(display_id)
            if capture_module is None:
                capture_module = ScreenCapture()
                self._persistent_capture_modules[display_id] = capture_module
            else:
                data_logger.info(
                    f"Reusing ScreenCapture instance for '{display_id}' (backend kept warm)."
                )

            # pixelflux is the cursor source on both backends (compositor on
            # Wayland, XFixes monitor on X11; an older X11-only pixelflux
            # stashes this harmlessly and the python monitor keeps delivering).
            # No hide is emitted on a capture (re)start: it would blank a
            # reconnecting client's cursor and poison the resend cache.
            capture_module.set_cursor_callback(pixelflux_cursor_handler)

            await self.capture_loop.run_in_executor(
                None,
                capture_module.start_capture,
                queue_data_for_display,
                settings
            )

            self.capture_instances[display_id] = {
                'module': capture_module,
                'sender_task': sender_task,
                # Retained for live geometry changes: a resize re-targets the running
                # module (X11 region update / Wayland live re-start) instead of
                # rebuilding it, and the settings tell reconfigure whether the running
                # session is structurally compatible with the desired one.
                'callback': queue_data_for_display,
                'settings': settings,
            }
            data_logger.info(f"SUCCESS: Capture started for '{display_id}'.")
            return True

        except Exception as e:
            data_logger.error(f"Failed to start capture for '{display_id}': {e}", exc_info=True)
            if display_id in self.video_chunk_queues:
                del self.video_chunk_queues[display_id]
            if sender_task is not None and not sender_task.done():
                sender_task.cancel()
            return False  # signal failure so callers don't report a false VIDEO_STARTED

    def _get_capture_settings(self, display_id, width, height, x, y):
        """Helper to create CaptureSettings for a specific display region."""
        display_state = self.display_clients.get(display_id)
        if not display_state:
            if display_id == 'primary':
                # Viewer-driven capture (no display-owning client connected):
                # every per-client knob below falls back to its server default.
                display_state = {}
            else:
                raise SelkiesAppError(f"Cannot get capture settings for unknown display_id '{display_id}'")

        cs = CaptureSettings()
        cs.capture_width = width
        cs.capture_height = height
        cs.capture_x = x
        cs.capture_y = y
        if IS_WAYLAND:
            cs.scale = display_state.get('scale', 1.0)
            # Binds this capture to its compositor output ('display2' -> output 2);
            # per-display IDR/rate/tunable calls route through the same id.
            cs.display_id = wayland_output_id(display_id)
        cs.target_fps = float(display_state.get('framerate', self.app.framerate))
        cs.capture_cursor = self.capture_cursor
        cs.debug_logging = self.cli_args.debug[0]
        
        encoder = display_state.get('encoder', self.app.encoder)
        if encoder == "jpeg":
            cs.output_mode = 0
            cs.jpeg_quality = display_state.get('jpeg_quality', self._initial_jpeg_quality)
            cs.paint_over_jpeg_quality = display_state.get('paint_over_jpeg_quality', self._initial_paint_over_jpeg_quality)
        else: # H.264 modes
            cs.output_mode = 1
            cs.video_crf = display_state.get('video_crf', self._initial_video_crf)
            cs.video_paintover_crf = display_state.get('video_paintover_crf', self._initial_video_paintover_crf)
            cs.video_paintover_burst_frames = display_state.get('video_paintover_burst_frames', self._initial_video_paintover_burst_frames)
            cs.video_fullcolor = display_state.get('video_fullcolor', self._initial_video_fullcolor)
            cs.video_streaming_mode = display_state.get('video_streaming_mode', self._initial_video_streaming_mode)
            cs.video_fullframe = (encoder in ("h264enc", "openh264enc"))
            cs.use_openh264 = (encoder == "openh264enc")
            rc_mode = display_state.get('rate_control_mode', settings.rate_control_mode)
            cs.video_cbr_mode = (rc_mode == 'cbr')
            video_bitrate = display_state.get('video_bitrate', self._initial_video_bitrate)
            cs.video_bitrate_kbps = int(round(float(video_bitrate) * 1000))  # Convert Mbps to kbps
            # 0 = infinite GOP (on-demand keyframes only).
            cs.keyframe_interval_s = float(getattr(settings, 'keyframe_interval', 0) or 0)
            # CBR QP clamp (0 = encoder default).
            cs.video_min_qp = int(getattr(settings, 'video_min_qp', 0) or 0)
            cs.video_max_qp = int(getattr(settings, 'video_max_qp', 0) or 0)

        cs.use_paint_over_quality = display_state.get('use_paint_over_quality', self._initial_use_paint_over_quality)
        cs.paint_over_trigger_frames = 15
        cs.damage_block_threshold = 10
        cs.damage_block_duration = 20
        cs.use_cpu = display_state.get('use_cpu', self._initial_use_cpu)
        # Zero-copy delivery: aiohttp may retain a memoryview of the encoded buffer past
        # send_bytes, so we keep the owning StripeFrame alive behind any live view/slice; it
        # frees the buffer once the last reference drops. pixelflux frames always own their
        # buffer (JPEG emits its 0x03 wire header natively like H.264), so every mode is zero-copy.

        # Forward explicit --encode-dri as an authoritative PATH; --gpu-id picks the
        # encoder device by index when no path is given. Unset, pixelflux defaults to
        # ID 0 — the first GPU — unless AUTO_GPU affinity aims it elsewhere.
        dri_node = self.cli_args.encode_dri
        gid = parse_gpu_id(getattr(self.cli_args, 'gpu_id', ''))
        if dri_node:
            cs.encode_node_path = dri_node.encode('utf-8')
            cs.encode_node_index = parse_dri_node_to_index(dri_node)
        elif gid is not None:
            # >= 0 picks the device; -1 requests software encoding.
            cs.encode_node_index = gid
        # Compositor render node, distinct from the encoder node above: an explicit
        # --render-dri wins; otherwise pixelflux resolves --auto-gpu ("true" or a
        # vendor/driver/DT-prefix/PCI-id token) against the machine itself.
        render_dri = getattr(self.cli_args, 'render_dri', '') or ''
        if render_dri:
            cs.render_node_path = render_dri.encode('utf-8')
        cs.auto_gpu = getattr(self.cli_args, 'auto_gpu', '') or ''
        cs.use_wayland = IS_WAYLAND
        cs.recording_socket = getattr(self.cli_args, 'recording_socket', '') or ''
        # Wayland compositor cursor-theme size (X11 cursor size is set on the X
        # server itself); <=0 keeps the theme default.
        cs.cursor_size = int(getattr(self.cli_args, 'cursor_size', -1))
        # Out-of-band delivery cap: track the input handler's DPI-scaled value
        # so pixelflux's XFixes monitor caps like the python monitor did.
        ih = getattr(self, 'input_handler', None)
        cs.cursor_size_cap = int(getattr(ih, 'cursor_size_cap', 0) or 0) or max(32, cs.cursor_size)

        watermark_path_str = self.cli_args.watermark_path
        if watermark_path_str and os.path.exists(watermark_path_str):
            cs.watermark_path = watermark_path_str.encode('utf-8')
            cs.watermark_location_enum = self.cli_args.watermark_location
        
        return cs
    
    async def run(self):
        """
        Start the DataStreamingServer and all its components.
        """
        self._shutdown_called = False
        self.initialize()

        logger.info("Starting DataStreamingServer...")
        
        self._tasks_to_run = []
        # Start input handler tasks
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

        # Apply the configured desktop DPI at startup so the first session sees
        # it even before any client syncs its own (96 is the X default — skip
        # the xrdb churn when nothing diverges).
        startup_dpi = int(float(getattr(settings, "scaling_dpi", "96") or 96))
        if not IS_WAYLAND and startup_dpi != 96:
            await set_dpi(startup_dpi)

        # Apply an explicit --cursor-size to the X server at startup (the DPI-change
        # handlers re-derive it on later changes); the Wayland compositor gets its
        # cursor size via CaptureSettings instead.
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

    async def shutdown(self):
        """
        Shutdown all DataStreamingServer components and clean up resources.
        This should be called to properly stop the server.
        """
        if self._shutdown_called:
            logger.info("Shutdown already called, skipping")
            return
        self._shutdown_called = True
        logger.info("DataStreamingServer shutdown initiated...")
        
        # Clear websockets connections FIRST so nothing re-enters
        # reconfigure_displays while the tasks below are torn down.
        self.clients.clear()
        self.video_paused_clients.clear()
        self._report_client_presence()
        self.display_clients.clear()

        # Cancel all auxiliary tasks
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

        # Stop input handler components
        if self.input_handler:
            logger.info("Stopping InputHandler components...")
            if hasattr(self.input_handler, "stop_clipboard"):
                self.input_handler.stop_clipboard()
            if hasattr(self.input_handler, "stop_cursor_monitor"):
                self.input_handler.stop_cursor_monitor()
            if hasattr(self.input_handler, "disconnect") and asyncio.iscoroutinefunction(
                self.input_handler.disconnect
            ):
                await self.input_handler.disconnect()

        # Drop the persistent ScreenCapture modules so their encoder sessions and
        # callbacks are released with the server.
        self._persistent_capture_modules.clear()

        self.app = None
        self.input_handler = None
        logger.info("DataStreamingServer shutdown complete.")

    async def start(self):
        self.shutdown_event.clear()
        await self.run()

    async def stop(self):
        self.shutdown_event.set()

    def register_routes(self, api_prefix: str, main_router: web.UrlDispatcher):
        # Data-plane WebSocket under /api so ONE nginx `location /api` (with the
        # WebSocket upgrade) fronts every dynamic path — control endpoints, this
        # data socket, and the WebRTC signaling socket alike. Nothing the browser
        # needs is proxied outside /api anymore.
        main_router.add_get(f'{api_prefix}/api/websockets{{slash:/?}}', self.data_ws_handler)
        main_router.add_post(f'{api_prefix}/api/tokens', self.handle_tokens)

    async def handle_tokens(self, request: web.Request):
        # Provisioning is transport-independent: user_tokens/active_mk_token govern
        # authority for both the websockets and WebRTC gates, so tokens are accepted
        # in any active mode (unlike the data WS endpoint below, which is mode-gated).
        # Read master_token from settings, not self.is_secure_mode, which is only set
        # once the websockets service's initialize() runs (never in WebRTC mode).
        if not settings.master_token:
            return web.json_response({"error": "Server not in secure mode"}, status=404)

        global user_tokens, active_mk_token
        try:
            new_token_data = await request.json()
            if not isinstance(new_token_data, dict): raise ValueError("Payload must be a JSON object")
            # Validate the full payload before mutating global auth state.
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
        asyncio.create_task(reconcile_clients())
        return web.Response(status=200, text="OK")

    async def data_ws_handler(self, request: web.Request):
        if self.supervisor.current_mode != self.mode:
            return web.Response(status=409, text="WebSocket mode is inactive")

        token = ""
        if self.cli_args.master_token:
            token = request.query.get('token') 
            if not token:
                return web.Response(status=401, text="Token missing in secure mode")

        # Frames on this socket are already compressed (H.264/JPEG/Opus), so
        # permessage-deflate only wastes CPU and an extra copy per frame.
        ws = web.WebSocketResponse(compress=False, max_msg_size=WS_MAX_MESSAGE_BYTES)
        await ws.prepare(request)

        peername = request.transport.get_extra_info('peername')
        remote_address = peername[:2] if peername else (request.remote, 0)
        # Extract legacy role/slot from query params
        query_role = request.query.get('role', '')
        query_slot = request.query.get('slot')
        # A view-only basic-auth credential caps the role at viewer no matter what
        # the query string asks for (legacy, non-secure mode); secure mode leaves
        # the ceiling unset and lets the token govern.
        if request.get("auth_role_ceiling") == "viewer":
            query_role = "viewer"
            query_slot = None
        try:
            await self.ws_handler(ws, remote_address, token, query_role=query_role, query_slot=query_slot)
        finally:
            # Covers every exit path that removed the socket from self.clients.
            self._report_client_presence()
        return ws


async def _collect_system_stats_ws(shared_data, interval_seconds=1):
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


async def _collect_gpu_stats_ws(shared_data, gpu_id=0, interval_seconds=1, dri_node=""):
    data_logger.debug(
        f"GPU monitor loop (WS mode) for GPU {gpu_id} (node {dri_node or 'any'}), "
        f"interval: {interval_seconds}s"
    )
    def _pick(gpus):
        # A dri_node match returns exactly the pipeline's GPU; the index only
        # applies to the unfiltered list.
        idx = 0 if (dri_node and len(gpus) == 1) else gpu_id
        return gpus[idx] if 0 <= idx < len(gpus) else None

    try:
        # get_gpus() may spawn/block on vendor tools; keep it off the event loop.
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
                    # Dashboards read gpu_percent (0..100), same field the WebRTC
                    # data channel sends; load stays as the 0..1 fraction for
                    # existing consumers.
                    "gpu_percent": gpu.load * 100,
                    "memory_total": gpu.memoryTotal * 1024 * 1024,
                    "memory_used": gpu.memoryUsed * 1024 * 1024,
                }
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

async def _collect_network_stats_ws(shared_data, server_instance, interval_seconds=2):
    """Periodically calculates bandwidth and collects latency."""
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

async def _send_stats_periodically_ws(websocket, shared_data, server_instance, interval_seconds=5):
    try:
        while True:
            await asyncio.sleep(interval_seconds)
            # Read (don't pop): popping would let one sender starve the others, since
            # many per-connection senders share these singleton collectors.
            system_stats = shared_data.get("system")
            gpu_stats = shared_data.get("gpu")
            network_stats = server_instance._shared_network_stats.get("network")
            try:
                if not websocket:  # Check if websocket is still valid
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

async def on_resize_handler(res_str, current_app_instance, data_server_instance=None, display_id='primary'):
    """
    Handles client resize request. Updates the state for a specific display and triggers a full reconfiguration.
    """
    logger_app_resize.info(f"on_resize_handler for display '{display_id}' with resolution: {res_str}")
    if (display_id == 'primary'
            and not getattr(current_app_instance, 'server_enable_resize', True)):
        # enable_resize gates only the PRIMARY's dynamic resolution; a secondary's resize
        # is its layout bring-up and must stay allowed (WebRTC parity).
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
                # Extended layout: the resize changes the union arrangement (a
                # primary resize moves the secondary's output offset), so it
                # must run the full layout pass, X11-style.
                logger_app_resize.info(
                    f"Wayland Resize: '{display_id}' to {target_w}x{target_h} via layout reconfiguration."
                )
                await data_server_instance.reconfigure_displays()
            elif IS_WAYLAND:
                logger_app_resize.info(f"Wayland Resize: Updating {display_id} to {target_w}x{target_h}.")
                if display_id in data_server_instance.display_layouts:
                    data_server_instance.display_layouts[display_id]['w'] = target_w
                    data_server_instance.display_layouts[display_id]['h'] = target_h

                # A start on the LIVE capture resizes it in place (pixelflux keeps a
                # compatible encoder session); stop+start only when it isn't running.
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
                    # Serialized against concurrent capture stop/start: a live
                    # in-place restart racing a teardown corrupts the instance.
                    async with data_server_instance._video_capture_lock:
                        await asyncio.to_thread(module.start_capture, inst['callback'], settings)
                    inst['settings'] = settings
                else:
                    await data_server_instance._stop_capture_for_display(display_id)
                    await data_server_instance._start_capture_for_display(display_id, target_w, target_h, 0, 0)
                # The compositor may realize something other than the request
                # (even-masking, GBM failure keeps the old mode): read the live
                # geometry back and broadcast it, matching the X11 path's
                # realized clamp + stream_resolution broadcast.
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

async def reconcile_clients():
    """Iterate through connected clients and disconnect those with invalid/changed permissions."""
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
            # new_perms is None when the token was revoked; nothing below
            # applies to a client being disconnected.
            continue
        has_mk_access = False
        if active_mk_token is not None:
            if token == active_mk_token:
                has_mk_access = True
        elif new_perms.get("role") == "controller":
            has_mk_access = True
        
        mk_msg = "MK_ACCESS,1" if has_mk_access else "MK_ACCESS,0"
        try:
            await ws.send_str(mk_msg)
            if has_mk_access:
                data_server = perms.get("data_server")
                if data_server:
                    await data_server.send_current_cursor(ws, remote_address)
        except (ConnectionResetError, OSError, RuntimeError):
            pass
