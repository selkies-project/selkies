# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Copyright 2019 Google LLC
#
#   Licensed under the Apache License, Version 2.0 (the "License");
#   you may not use this file except in compliance with the License.
#   You may obtain a copy of the License at
#
#        http://www.apache.org/licenses/LICENSE-2.0
#
#   Unless required by applicable law or agreed to in writing, software
#   distributed under the License is distributed on an "AS IS" BASIS,
#   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
#   See the License for the specific language governing permissions and
#   limitations under the License.

"""WebRTC transport engine for Selkies.

Owns the server side of every WebRTC session: peer-connection lifecycle
(offer building, SDP/ICE plumbing, teardown), per-display media graphs (a
`MediaRelay` fanning one encoded video/audio source out to every peer of that
display), and the ordered "input" data channel that carries input, clipboard,
cursor, stats, and control messages.

Structural notes:

- Everything runs on one asyncio loop. Capture threads never touch the loop
  directly; encoded frames arrive via `loop.call_soon_threadsafe` into
  `PipelineBridge` queues that the media tracks drain.
- Data-channel dispatch is serialized per channel through a bounded queue with
  a single consumer task so input events keep strict arrival order.
- Behavior deliberately mirrors the websockets transport (broadcast semantics,
  viewer/collaborator input gates, MK_ACCESS/AUTH_SUCCESS verdicts); parity
  between the two transports is a project invariant. The input gates share
  their prefix allow-lists with the websockets gate (`input_handler`), and
  secure-mode checks read the live `/api/tokens` table
  (`current_session_tokens`) per message so re-provisioning is honored.
- Data-channel compression is negotiated per channel by the `_gz,1`
  handshake: a peer that sends it can gunzip, is echoed the capability, and
  may from then on receive gzip'd payloads; every display page and viewer
  handshakes independently.
"""

import logging
import asyncio
import time
import gzip
import inspect
import re
import json
import base64
import urllib.parse
import aiohttp

try:
    import pcmflux
except (ImportError, RuntimeError):
    pcmflux = None

from .settings import settings as app_settings, inflate_gz_bounded, software_h264_encoder, software_h264_path
from .webcam import CODEC_BY_NAME, get_shared_webcam, webcam_locked_off, webcam_uplink_allowed
from .webrtc import (
    RTCPeerConnection,
    RTCIceCandidate,
    RTCRtpSender,
    RTCSessionDescription,
    VideoStreamTrack,
    RTCConfiguration,
    RTCIceServer,
    AudioStreamTrack,
    RTCDataChannel,
    RTCBundlePolicy
)
from .webrtc.rtcicetransport import (
    Candidate,
    candidate_from_aioice
)
from .webrtc.exceptions import InvalidStateError
from fractions import Fraction

from .webrtc.codecs.base import EncodedPacket
from typing import Any, Callable, Dict, Iterator, List, Optional, Tuple, Union
from .webrtc.contrib.relay import MediaRelay
from enum import Enum
from .media_pipeline import MediaPipeline
from .input_handler import (
    BULK_DRAIN_TIMEOUT_S,
    gamepad_slot_denied,
    VIEWER_ALLOWED_PREFIXES,
    VIEWER_COLLAB_EXTRA_PREFIXES,
    VIEWER_SILENT_DROP_PREFIXES,
)
from .selkies import current_session_tokens

logger = logging.getLogger("rtc")
logger.setLevel(logging.INFO)

class ConditionalExtraFormatter(logging.Formatter):
    """Log formatter that appends selected `extra` fields when present.

    Records logged with `extra={'client_peer_id': ..., 'client_type': ...}`
    get those fields appended as `key=value` pairs; records without them
    format normally, so one formatter serves both peer-scoped and global
    log lines.
    """

    def __init__(self, fmt: Optional[str] = None, datefmt: Optional[str] = None,
                 style: str = '%', extra_fields: Optional[List[str]] = None) -> None:
        super().__init__(fmt, datefmt, style)
        self.extra_fields: List[str] = extra_fields or ['client_peer_id', 'client_type']

    def format(self, record: logging.LogRecord) -> str:
        """Format the record, appending any configured extra fields it carries."""
        result = super().format(record)
        extra_parts = []
        for field in self.extra_fields:
            value = getattr(record, field, None)
            if value is not None:
                extra_parts.append(f"{field}={value}")
        if extra_parts:
            result = f"{result} | {' '.join(extra_parts)}"
        return result

handler = logging.StreamHandler()
formatter = ConditionalExtraFormatter(
    fmt='%(levelname)s:%(name)s:%(message)s',
    datefmt='%Y-%m-%d %H:%M:%S',
    extra_fields=['client_peer_id', 'client_type']
)
logger.handlers.clear()
logger.propagate = False
handler.setFormatter(formatter)
logger.addHandler(handler)


# Raw bytes per bulk (clipboard) chunk before base64 expansion, and the SCTP
# queue cap a transfer waits below. Both sized for latency: a chunk is the unit
# at which input and the media streams can overtake a transfer, and the cap
# bounds what a transfer holds away from them. The chunk is a multiple of 3, so
# chunks concatenate as base64.
DATA_CHANNEL_BULK_CHUNK_SIZE = 16 * 1024 // 3 * 3
DATA_CHANNEL_BULK_HIGH_WATER = 64 * 1024


async def drain_data_channel(channel: RTCDataChannel,
                             high: int = DATA_CHANNEL_BULK_HIGH_WATER,
                             timeout: float = BULK_DRAIN_TIMEOUT_S) -> bool:
    """Wait until `channel` has at most `high` queued bytes.

    Uses the channel's bufferedamountlow event with a temporarily lowered
    threshold; the timeout keeps a stalled/closing channel from wedging the
    sender (the send path already drops on closed channels).

    Args:
        channel: Data channel whose SCTP send queue is being paced.
        high: Maximum queued bytes to allow before returning.
        timeout: Seconds to wait before giving up on a stalled channel.

    Returns:
        True once the queue is below `high`; False when the channel did not
        drain within `timeout` (still open, still stalled).
    """
    if channel.bufferedAmount <= high:
        return True
    prev = channel.bufferedAmountLowThreshold
    loop = asyncio.get_running_loop()
    low = loop.create_future()

    def _on_low() -> None:
        if not low.done():
            low.set_result(None)

    channel.bufferedAmountLowThreshold = high
    channel.on("bufferedamountlow", _on_low)
    try:
        if channel.bufferedAmount > high:
            await asyncio.wait_for(low, timeout)
    except asyncio.TimeoutError:
        return False
    finally:
        channel.remove_listener("bufferedamountlow", _on_low)
        channel.bufferedAmountLowThreshold = prev
    return True


def get_adjusted_chunk_size(peers: Optional[dict] = None) -> int:
    """Raw-byte chunk size for base64 payloads over the data channel.

    DATA_CHANNEL_BULK_CHUNK_SIZE, lowered to whatever the connected peers
    negotiated (RFC 8841 a=max-message-size) where one of them advertises less,
    so a peer is never overrun.

    Args:
        peers: Mapping of peer id to peer entry (each may carry a
            `data_channel`); None or empty falls back to the standard size.

    Returns:
        Raw byte count to read per chunk before base64 encoding.
    """
    limit: Optional[int] = None
    for peer in (peers or {}).values():
        channel = peer.get("data_channel")
        sctp = getattr(channel, "transport", None)
        nego = getattr(sctp, "remote_max_message_size", 0)
        if nego:
            limit = nego if limit is None else min(limit, nego)
    if not limit:
        return DATA_CHANNEL_BULK_CHUNK_SIZE
    usable = limit - 512
    return min((usable * 3) // 4, DATA_CHANNEL_BULK_CHUNK_SIZE)

class ClientType(str, Enum):
    """Role a peer connected with: a controller drives a display's media
    pipeline and full input; a viewer receives media with gated input."""
    CONTROLLER = "controller"
    VIEWER = "viewer"

class RTCAppError(Exception):
    """Raised for unrecoverable errors in the RTC signaling/pipeline layer."""
    pass

class PipelineBridge:
    """A bridge to asynchronously pass data between Media and the RTC pipeline.

    maxsize selects the buffering policy: depth 1 is latest-wins (video wants
    the freshest frame), a deeper bound acts as a short drop-oldest FIFO (audio
    wants continuity so a brief consumer stall doesn't silently drop samples).
    """
    def __init__(self, maxsize: int = 1,
                 on_drop: Optional[Callable[[], None]] = None) -> None:
        """Initializes the bridge.

        Args:
            maxsize: Queue depth; 1 means latest-wins, larger is drop-oldest.
            on_drop: Fired (on the loop thread) whenever a queued item is
                dropped. The video bridge uses it to force a recovery keyframe:
                a dropped ENCODED frame breaks the wire reference chain with no
                RTP gap, so the browser never requests a PLI and the smear
                would persist under infinite GOP.
        """
        self._queue: asyncio.Queue = asyncio.Queue(maxsize=maxsize)
        self._on_drop = on_drop

    def set_data(self, data: Any) -> None:
        """Enqueue an item, dropping the oldest one when the queue is full.

        Synchronous, no lock: the drop-oldest check and the put have no await,
        so the single-threaded loop runs them without interleaving (all access
        is on the loop thread). A full queue means the consumer is lagging, so
        the oldest queued item is dropped to make space for the new one.
        """
        if self._queue.full():
            self._queue.get_nowait()
            if self._on_drop is not None:
                self._on_drop()
        self._queue.put_nowait(data)

    async def get_data(self) -> Any:
        """Wait until an item is available in the queue and return it."""
        return await self._queue.get()

class AudioMedia(AudioStreamTrack):
    """Audio track that serves pre-encoded packets from a `PipelineBridge`."""

    def __init__(self, data_pipeline: PipelineBridge) -> None:
        super().__init__()
        self.data_pipeline = data_pipeline

    async def recv(self) -> EncodedPacket:
        """Return the next encoded audio packet from the bridge."""
        packet = await self.data_pipeline.get_data()
        return packet

class VideoMedia(VideoStreamTrack):
    """Video track that serves pre-encoded packets from a `PipelineBridge`."""

    def __init__(self, data_pipeline: PipelineBridge) -> None:
        super().__init__()
        self.data_pipeline = data_pipeline

    async def recv(self) -> EncodedPacket:
        """Return the next encoded video packet from the bridge."""
        packet = await self.data_pipeline.get_data()
        return packet

class RTCApp:
    """Server-side WebRTC engine: peers, per-display media graphs, channels.

    The owning service late-binds most behavior (the `on_*` and `send_*`
    hooks assigned in `__init__`), so this class stays transport-only: it
    builds offers, routes data-channel traffic through the shared input
    gates, and tears peers down symmetrically with the websockets engine.

    Attributes:
        peer_connections: Peer id to peer entry, one per connected browser
            page: `peer_conn`, `data_channel`, `client_type`, `display_id`;
            `client_token` and `client_slot` as held at connect — the token's
            slot in secure mode, else the one the peer claimed at HELLO — so a
            revocation or handoff on `/api/tokens` can find the affected peers
            (per-message checks read the live store); `channel_consumers`,
            cancellable from teardown because a channel that never reached
            SCTP-established emits no close; `mic_state` and `webcam_state`,
            this peer's own uplink sinks (None without the m-line), retired
            per peer; `video_sender` and `video_paused` for the per-peer tab
            pause — the sender's enabled flag drops frames after recv, so the
            relay keeps draining while the peer is hidden.
        displays: Display id to media graph (`relay`, `video_bridge`,
            `video_media`, and on the primary display `audio_bridge` /
            `audio_media`). Built by the display's first peer (a secondary's
            must be its controller) and released with its last consumer, or
            for a secondary with its controller.
        media_pipeline: The primary display's pipeline, driven by the default
            start/stop hooks.
        start_display_media: Per-display capture start, overridable by the
            owning service so a secondary display drives its own pipeline; the
            default starts only the primary.
        stop_display_media: Counterpart of `start_display_media`.
        get_encoder_for_display: Encoder a display runs, resolved at offer
            time; the default is the single global encoder.
        get_fullcolor_for_display: Whether a display emits 4:4:4, resolved at
            offer time because the advertised profile must describe the
            bitstream the display produces now, not the startup setting.
        get_use_cpu_for_display: Whether a display forces software encoding,
            resolved at offer time like the encoder; with it decides whether a
            4:4:4 profile may be advertised.
        on_data_open: Receives the channel that opened so per-connection
            greetings (settings, current cursor) reach the joining peer.
        on_data_close: Data channel closed.
        on_data_error: Data channel error.
        on_data_message: Input dispatcher, called with the message, display id
            and the peer id as `conn_id`.
        on_peer_gone: Async hook called as `(peer_id, peer_entry)` when a peer
            reaches closed: the id releases per-connection input state
            (gamepad associations) after an ungraceful disconnect, the entry
            says what input authority left with it.
        on_ice: ICE candidate to send over signaling.
        on_sdp: SDP offer to send over signaling.
        request_idr_frame: Async keyframe request for a display.
        on_video_consumer_active: Per-peer video pause (tab-hide STOP_VIDEO /
            START_VIDEO), display-scoped; left None the verbs fall through to
            the input dispatcher, which ignores them.
        on_consumers_changed: A display's consumer set changed (join, close).
        provision_virtual_mic: Brings up the shared SelkiesVirtualMic (null
            sinks, module-virtual-source, default source) before a mic
            playback opens its `input` stream, so an app recording the default
            source hears the client's mic (websockets 0x02 parity); None
            leaves the mic playing into `input` without the recordable source.
        last_cursor_sent: Last cursor payload, replayed to late joiners.
    """

    def __init__(
        self,
        async_event_loop: asyncio.AbstractEventLoop,
        encoder: str,
        stun_servers: Optional[List[str]] = None,
        turn_servers: Optional[List[str]] = None
    ) -> None:
        self.peer_connections: Dict[str, Any] = {}
        self.async_event_loop = async_event_loop
        self.stun_servers = stun_servers
        self.turn_servers = turn_servers
        self.encoder = encoder
        self.last_cursor_sent = None

        self.displays: Dict[str, Dict[str, Any]] = {}
        self.media_pipeline: Optional[MediaPipeline] = None
        self.start_display_media = self._default_start_display_media
        self.stop_display_media = self._default_stop_display_media
        self.get_encoder_for_display = lambda display_id: self.encoder
        self.get_fullcolor_for_display = lambda display_id: bool(app_settings.video_fullcolor[0])
        self.get_use_cpu_for_display = lambda display_id: bool(app_settings.use_cpu[0])

        self.on_data_open = lambda channel=None: logger.warning('unhandled on_data_open')
        self.on_data_close = lambda: logger.warning('unhandled on_data_close')
        self.on_data_error = lambda e=None: logger.warning('unhandled on_data_error')
        self.on_data_message = lambda msg, display_id='primary', conn_id=None: logger.warning('unhandled on_data_message')
        self.on_peer_gone = None

        self.on_ice = lambda ice, client_peer_id: logger.warning('unhandled ice event')
        self.on_sdp = lambda sdp_type, sdp, client_peer_id: logger.warning('unhandled sdp event')

        self.request_idr_frame = lambda display_id='primary': logger.warning('unhandled request_idr_frame')

        self.on_video_consumer_active = None
        self.on_consumers_changed = None

        self.provision_virtual_mic = None

    async def set_sdp(self, sdp_type: str, sdp: str, client_peer_id: str) -> None:
        """Apply the remote SDP answer received from a peer over signaling.

        Answers for peers already in a closed/failed state are ignored rather
        than raised: teardown routinely races late signaling messages.

        Args:
            sdp_type: Must be "answer"; the server always offers.
            sdp: The remote session description text.
            client_peer_id: Registered peer the answer belongs to.

        Raises:
            RTCAppError: On a non-answer type, missing sdp/peer id, or an
                unknown peer.
        """
        if sdp_type != 'answer':
            raise RTCAppError('ERROR: sdp type is not "answer"')
        if sdp is None:
            raise RTCAppError("ERROR: sdp can't be None")
        if not client_peer_id:
            raise RTCAppError("ERROR: client_peer_id is required to set sdp")

        peer_obj = self.peer_connections.get(client_peer_id, None)
        if peer_obj is None:
            raise RTCAppError(f"ERROR: peer connection for client_peer_id: {client_peer_id} not found")

        peer_conn = peer_obj["peer_conn"]
        if peer_conn.connectionState in ["closed", "failed"]:
            logger.warning(
                f"Ignoring remote SDP: peer connection in {peer_conn.connectionState} state",
                extra={'client_peer_id': client_peer_id, 'client_type': peer_obj.get('client_type')}
            )
            return

        desc = RTCSessionDescription(sdp=sdp, type=sdp_type)
        await peer_conn.setRemoteDescription(desc)

    async def set_ice(self, ice: Dict, client_peer_id: str) -> None:
        """Add an ICE candidate received from the signaling server.

        An empty candidate string is the end-of-candidates marker and is
        forwarded as None.

        Args:
            ice: Candidate dict with `candidate` and `sdpMid` or
                `sdpMLineIndex` keys, as sent by the browser.
            client_peer_id: Registered peer the candidate belongs to.

        Raises:
            RTCAppError: On a missing peer id, an unknown peer, or a
                candidate that fails to parse.
        """
        if not client_peer_id:
            raise RTCAppError("ERROR: client_peer_id is required to set sdp")

        peer_obj = self.peer_connections.get(client_peer_id, None)
        if peer_obj is None:
            raise RTCAppError(f"ERROR: peer connection for client_peer_id: {client_peer_id} not found")

        peer_conn = peer_obj["peer_conn"]
        if peer_conn.connectionState in ["closed", "failed"]:
            logger.warning(
                f"Ignoring adding ICE candidate: peer connection in {peer_conn.connectionState} state",
                extra={'client_peer_id': client_peer_id, 'client_type': peer_obj.get('client_type')}
            )
            return

        if ice.get('candidate') == "":
            await peer_conn.addIceCandidate(None)
            return

        obj = Candidate.from_sdp(ice.get('candidate'))
        icecandidate = candidate_from_aioice(obj)

        sdp_mid = ice.get('sdpMid')
        if sdp_mid is not None:
            icecandidate.sdpMid = sdp_mid
        else:
            icecandidate.sdpMLineIndex = ice.get('sdpMLineIndex')

        if isinstance(icecandidate, RTCIceCandidate):
            await peer_conn.addIceCandidate(icecandidate)
        else:
            raise RTCAppError("ERROR: ice candidate is not an instance of RTCIceCandidate")

    async def send_clipboard_data(self, data: Union[str, bytes], mime_type: str = "text/plain",
                                  reply_to: Optional[str] = None,
                                  peer_id: Optional[str] = None) -> None:
        """Send clipboard data over the data channel, chunked when large.

        Chunk sends are paced against each channel's SCTP queue (see
        `drain_data_channel`) so a multi-MB clipboard neither buffers
        unboundedly in memory nor starves input/cursor/stats behind it on the
        one ordered stream, and the per-chunk gzip runs off the event loop.
        Each channel runs its own pipeline a chunk at a time, so a slow peer
        paces only its own transfer; one that cannot drain a chunk within the
        bulk timeout is left out of the rest of this payload (it keeps its
        stream, and the client discards the partial transfer at the next
        start) instead of wedging the others. Chunk payloads are built once
        and shared by every channel; an entry is dropped once the slowest
        channel has passed it, so the cache holds what is in flight, not the
        whole payload. An empty payload is sent only as a tagged reply,
        settling a client fetch against an empty server clipboard.

        Args:
            data: Clipboard payload; str is UTF-8 encoded before sending.
            mime_type: Payload MIME type; "text/plain" marks it as text.
            reply_to: Set to the requesting verb (e.g. "cr") when this send
                answers a client fetch rather than announcing a server-side
                clipboard change — the websockets `clipboard_reply,<verb>`
                contract, carried here as an extra field on the clipboard-msg
                / clipboard-msg-start payload so the client can treat the
                payload cache-only without time heuristics (old clients ignore
                the unknown field).
            peer_id: Peer that asked for this payload, addressed alone the way
                `send_system_action` addresses a requester: the other peers
                already hold the content, and a reply they did not ask for is
                read as their own fetch and cached rather than pasted. A
                requester whose channel has closed receives nothing.
        """
        if not data and not reply_to:
            return

        is_text = mime_type == "text/plain"
        data_bytes: bytes = data.encode() if isinstance(data, str) else data
        clipboard_chunk_size = get_adjusted_chunk_size(self.peer_connections)
        requester = None
        if peer_id is not None:
            peer_obj = self.peer_connections.get(peer_id)
            channel = peer_obj.get("data_channel") if peer_obj else None
            if channel is None or channel.readyState != "open":
                return
            requester = channel

        def send_typed(msg_type: str, payload: Any) -> None:
            if requester is not None:
                self.send_message_to_channel(requester, msg_type, payload)
            else:
                self.__send_data_channel_message(msg_type, payload)

        if len(data_bytes) <= clipboard_chunk_size:
            b64data = base64.b64encode(data_bytes).decode('utf-8')
            payload = {
                "content": b64data,
                "mime_type": mime_type,
                "is_binary_data": not is_text,
                "total_size": len(data_bytes)
            }
            if reply_to:
                payload["reply_to"] = reply_to
            send_typed("clipboard-msg", payload)
        else:
            start_payload = {
                "mime_type": mime_type,
                "is_binary_data": not is_text,
                "total_size": len(data_bytes),
            }
            if reply_to:
                start_payload["reply_to"] = reply_to
            channels = ([requester] if requester is not None
                        else list(self._iter_open_data_channels()))
            # One payload at a time per channel: start/data/end carry no
            # transfer id, so a send racing another would interleave two
            # payloads' chunks into one assembly.
            locks = self.__dict__.setdefault("_clipboard_send_locks", {})
            live = {id(c) for c in self._iter_open_data_channels()}
            live.update(id(c) for c in channels)
            for gone in [k for k in locks if k not in live]:
                del locks[gone]
            offsets = list(range(0, len(data_bytes), clipboard_chunk_size))
            prepared: dict = {}
            prepare_lock = asyncio.Lock()
            progress = {id(c): 0 for c in channels}

            async def chunk_for(offset: int, want_gz: bool) -> tuple:
                async with prepare_lock:
                    entry = prepared.get(offset)
                    if entry is None:
                        chunk = data_bytes[offset:offset + clipboard_chunk_size]
                        entry = (json.dumps({
                            "type": "clipboard-msg-data",
                            "data": {"content": base64.b64encode(chunk).decode("utf-8")},
                        }), None)
                    if want_gz and entry[1] is None:
                        entry = (entry[0], await asyncio.to_thread(
                            gzip.compress, entry[0].encode("utf-8"), 6))
                    prepared[offset] = entry
                    return entry

            def passed(channel: Any, offset: int) -> None:
                progress[id(channel)] = offset + clipboard_chunk_size
                floor = min(progress.values())
                for done in [o for o in prepared if o + clipboard_chunk_size <= floor]:
                    prepared.pop(done, None)

            async def deliver(channel: Any) -> None:
                want_gz = bool(getattr(channel, "_selkies_gz_tx", False))
                async with locks.setdefault(id(channel), asyncio.Lock()):
                    self.send_message_to_channel(channel, "clipboard-msg-start", start_payload)
                    for offset in offsets:
                        if channel.readyState != "open":
                            progress.pop(id(channel), None)
                            return
                        payload, gz_payload = await chunk_for(offset, want_gz)
                        self._send_prepared_to_channel(
                            channel, "clipboard-msg-data", payload, gz_payload)
                        if not await drain_data_channel(channel):
                            logger.warning(
                                "Data channel did not drain a clipboard chunk within "
                                f"{BULK_DRAIN_TIMEOUT_S:.0f}s; leaving it out of this payload.")
                            progress.pop(id(channel), None)
                            return
                        passed(channel, offset)
                    self.send_message_to_channel(channel, "clipboard-msg-end", {})

            await asyncio.gather(*(deliver(c) for c in channels), return_exceptions=True)

        logger.info(f"Sent clipboard data of length {len(data_bytes)} with mime type {mime_type}")

    def send_cursor_data(self, data: Any) -> None:
        """Broadcast a cursor update, remembering it for late-joining peers."""
        self.last_cursor_sent = data
        self.__send_data_channel_message(
            "cursor", data)

    def send_gpu_stats(self, load: float, memory_total: int, memory_used: int) -> None:
        """Broadcast GPU stats (load fraction, memory in MiB) to all peers."""

        self.__send_data_channel_message("gpu_stats", {
            "gpu_percent": load * 100,
            "mem_total": memory_total * 1024 * 1024,
            "mem_used": memory_used * 1024 * 1024,
        })

    def send_system_action(self, action: str, peer_id: Optional[str] = None) -> None:
        """Send a system action (e.g. ``command_error,<text>``) to clients.

        With a `peer_id` whose channel is still open, only that peer is
        addressed (requester-scoped feedback); otherwise — including a
        requester that reconnected under a new peer id — the action is
        broadcast, and shared-mode viewers suppress it client-side.
        """
        if peer_id is not None:
            peer_obj = self.peer_connections.get(peer_id)
            channel = peer_obj.get("data_channel") if peer_obj else None
            if channel is not None and channel.readyState == "open":
                self.send_message_to_channel(channel, "system", {"action": action})
                return
        self.__send_data_channel_message("system", {"action": action})

    def send_framerate(self, framerate: int) -> None:
        """Broadcast the current framerate to all peers."""
        logger.info("sending framerate")
        self.__send_data_channel_message(
            "system", {"action": "videoFramerate," + str(framerate)})

    def send_video_bitrate(self, bitrate: int) -> None:
        """Broadcast the current video bitrate to all peers."""
        logger.info("sending video bitrate")
        self.__send_data_channel_message(
            "system", {"action": "video_bitrate," + str(bitrate)})

    def send_audio_bitrate(self, bitrate: int) -> None:
        """Broadcast the current audio bitrate to all peers."""
        logger.info("sending audio bitrate")
        self.__send_data_channel_message(
            "system", {"action": "audio_bitrate,%d" % bitrate})

    def send_encoder(self, encoder: str) -> None:
        """Broadcast the active encoder name to all peers."""
        logger.info("sending encoder: " + encoder)
        self.__send_data_channel_message(
            "system", {"action": "encoder,%s" % encoder})

    def send_resize_enabled(self, resize_enabled: bool) -> None:
        """Broadcast the current resize-enabled state to all peers."""
        logger.info("sending resize enabled state")
        self.__send_data_channel_message(
            "system", {"action": "resize," + str(resize_enabled)})

    def send_remote_resolution(self, res: str, display_id: str = "primary") -> None:
        """Send the realized remote resolution to the clients of `display_id`.

        Display-scoped: the websockets transport tags its stream_resolution
        with the display id and addresses only that page, so a secondary's
        realized size must never rescale the primary page.
        """
        logger.info("sending remote resolution of: " + res)
        sent = False
        for peer_obj in self.peer_connections.values():
            if (peer_obj.get("display_id") or "primary") != display_id:
                continue
            peer_conn = peer_obj.get("peer_conn")
            channel = peer_obj.get("data_channel")
            if (
                peer_conn is not None
                and channel is not None
                and peer_conn.connectionState == "connected"
                and channel.readyState == "open"
            ):
                self.send_message_to_channel(
                    channel, "system", {"action": "resolution," + res})
                sent = True
        if not sent:
            logger.info("skipping remote resolution because no data channel is ready")

    def send_ping(self, t: float) -> None:
        """Send a ping request to the PRIMARY controller only.

        Latency is measured against one shared ping_start, and the websockets
        transport likewise derives its reported latency from the primary
        client.
        """
        state, data_channel = self.get_data_channel()
        if not state:
            return
        self.send_message_to_channel(
            data_channel, "ping", {"start_time": float("%.3f" % t)})

    def send_latency_time(self, latency: float) -> None:
        """Broadcast the measured latency response time in milliseconds."""
        self.__send_data_channel_message(
            "latency_measurement", {"latency_ms": latency})

    def send_system_stats(self, cpu_percent: float, mem_total: int, mem_used: int) -> None:
        """Broadcast CPU and memory stats to all peers."""
        self.__send_data_channel_message(
            "system_stats", {
                "cpu_percent": cpu_percent,
                "mem_total": mem_total,
                "mem_used": mem_used,
            })

    def get_data_channel(self) -> Tuple[bool, Optional[RTCDataChannel]]:
        """Return the controller's data channel and whether it is usable.

        Returns:
            A `(ready, channel)` pair: ready is True only when the controller's
            connection is connected and its channel open; channel is None when
            no controller exists.
        """
        state = False
        peer_obj = self.get_controller_instance()
        if not peer_obj:
            return state, None

        conn_state = peer_obj.get("peer_conn").connectionState
        data_channel_state = peer_obj.get("data_channel").readyState
        return conn_state == "connected" and data_channel_state == "open", peer_obj.get("data_channel")

    def _iter_open_data_channels(self) -> Iterator[RTCDataChannel]:
        """Yield every connected peer's open data channel — controllers and
        viewers, all displays."""
        for peer_obj in self.peer_connections.values():
            peer_conn = peer_obj.get("peer_conn")
            channel = peer_obj.get("data_channel")
            if (
                peer_conn is not None
                and channel is not None
                and peer_conn.connectionState == "connected"
                and channel.readyState == "open"
            ):
                yield channel

    def send_message_to_channel(self, channel: RTCDataChannel, msg_type: str,
                                data: Any) -> None:
        """Send one typed message to one specific peer's data channel.

        Payloads of 512 bytes or more (cursor PNGs, settings, clipboard, stats)
        are gzipped for a channel that completed the `_gz` handshake; smaller
        ones are not worth the CPU or the risk to input latency.
        """
        payload = json.dumps({"type": msg_type, "data": data})
        gz_payload = None
        if getattr(channel, "_selkies_gz_tx", False) and len(payload) >= 512:
            gz_payload = gzip.compress(payload.encode("utf-8"), 6)
        self._send_prepared_to_channel(channel, msg_type, payload, gz_payload)

    def _send_prepared_to_channel(self, channel: RTCDataChannel, msg_type: str,
                                  payload: str,
                                  gz_payload: Optional[bytes]) -> None:
        """Guarded raw send of an already-serialized (and possibly
        pre-compressed) message; bulk senders reuse one compression across
        channels instead of re-gzipping per peer.

        A message oversized for the peer's negotiated max-message-size
        (`ValueError`) is dropped and logged, which beats the peer
        hard-closing the channel; one that finds the channel no longer open
        (`InvalidStateError`, a close racing the sender) is dropped like any
        not-ready channel's.
        """
        try:
            if gz_payload is not None and getattr(channel, "_selkies_gz_tx", False):
                channel.send(gz_payload)
            else:
                channel.send(payload)
        except ValueError as e:
            logger.error("dropping oversized data channel message '%s': %s", msg_type, e)
        except InvalidStateError:
            logger.info("skipping message because data channel closed mid-send: %s" % msg_type)

    def __send_data_channel_message(self, msg_type: str, data: Any) -> None:
        """Broadcast a typed message to every connected peer.

        All display controllers and viewers receive it — the websockets
        transport broadcasts cursor, stats, and clipboard to all of its
        clients, so the channel path must too. Channels that are not open are
        skipped.
        """
        if not self.peer_connections:
            return
        sent = False
        for channel in self._iter_open_data_channels():
            self.send_message_to_channel(channel, msg_type, data)
            sent = True
        if not sent:
            logger.info("skipping message because no data channel is ready: %s" % msg_type)

    def send_media_data_over_channel(self, msg_type: str, data: Any) -> None:
        """Broadcast a media-related message to all peers."""
        self.__send_data_channel_message(msg_type, data)

    async def close_display_peers(self, display_id: str) -> None:
        """Close every peer attached to `display_id`.

        Each close is finished by the connection-state handler (channel
        consumers, media graph, display registration), which runs as its own
        task.
        """
        for peer_obj in [obj for obj in self.peer_connections.values()
                         if (obj.get("display_id") or "primary") == display_id]:
            peer_conn = peer_obj.get("peer_conn")
            if peer_conn is not None:
                try:
                    await peer_conn.close()
                except Exception as e:
                    logger.warning(f"Error closing a '{display_id}' peer: {e}")

    def get_controller_instance(self) -> Optional[Dict[str, Any]]:
        """Return the peer entry for the controller client, if one exists.

        With multiple display controllers connected, the PRIMARY display's
        controller is the authoritative one (latency pings and
        controller-directed replies).
        """
        controllers = [
            obj for obj in self.peer_connections.values()
            if obj.get("client_type") == ClientType.CONTROLLER
        ]
        if not controllers:
            return None
        return next(
            (obj for obj in controllers if obj.get("display_id", "primary") == "primary"),
            controllers[0],
        )

    def munge_sdp(self, sdp: str, encoder: Optional[str] = None,
                  fullcolor: Optional[bool] = None,
                  use_cpu: Optional[bool] = None) -> str:
        """Rewrite the local offer SDP for optimal streaming behavior.

        Injects a 125 ms rtx-time, `sps-pps-idr-in-keyframe=1` for H.264/H.265,
        the Opus ptime, and generous video bandwidth ceilings
        (`_munge_video_bandwidth`). Displays can run different encoders,
        chroma formats and software-encoding flags; the caller passes the ones
        this offer's display is using (defaults: the primary/global encoder and
        the configured full-colour and software-encoding settings).

        Full colour is a 4:4:4 bitstream, so the H.264 profile-level-id is
        rewritten to High 4:4:4 (`f4001f`) rather than handing the decoder a
        4:2:0 baseline profile that cannot match what it receives; 4:2:0 keeps
        `42e01f`, the profile Firefox negotiates. A session known to encode on
        OpenH264 (the software encoder of a GPL-free pixelflux build, forced
        onto the CPU) is excluded: it always emits limited-range 4:2:0, and a
        4:4:4 profile makes decoders misread its color range (visibly darker
        output).

        The Opus ptime advertises the real frame duration pcmflux emits
        (`audio_frame_duration_ms`) so the client keys its minptime munge off
        it, rounded to whole milliseconds for browser SDP parsers.

        Args:
            sdp: The local offer SDP text.
            encoder: Encoder the display is running; None means the global one.
            fullcolor: Whether the display emits a 4:4:4 bitstream; None reads
                the configured setting.
            use_cpu: Whether the display forces software encoding; None reads
                the configured setting.

        Returns:
            The munged SDP text.
        """
        encoder = encoder or self.encoder
        if fullcolor is None:
            fullcolor = bool(app_settings.video_fullcolor[0])
        if use_cpu is None:
            use_cpu = bool(app_settings.use_cpu[0])
        software_path = software_h264_path(
            encoder, use_cpu or str(getattr(app_settings, "gpu_id", "")).strip() == "-1")
        sdp_text = sdp
        if 'rtx-time' not in sdp_text:
            logger.warning("injecting rtx-time to SDP")
            sdp_text = re.sub(r'(apt=\d+)', r'\1;rtx-time=125', sdp_text)
        elif 'rtx-time=125' not in sdp_text:
            logger.warning("injecting modified rtx-time to SDP")
            sdp_text = re.sub(r'rtx-time=\d+', r'rtx-time=125', sdp_text)
        if "h264" in encoder or "x264" in encoder or "h265" in encoder or "x265" in encoder:
            if 'sps-pps-idr-in-keyframe' not in sdp_text:
                logger.warning("injecting sps-pps-idr-in-keyframe to SDP")
                sdp_text = sdp_text.replace('packetization-mode=', 'sps-pps-idr-in-keyframe=1;packetization-mode=')
            elif 'sps-pps-idr-in-keyframe=1' not in sdp_text:
                logger.warning("injecting modified sps-pps-idr-in-keyframe to SDP")
                sdp_text = re.sub(r'sps-pps-idr-in-keyframe=\d+', r'sps-pps-idr-in-keyframe=1', sdp_text)
            if ("h264" in encoder or "x264" in encoder) and fullcolor \
                    and not (software_path and software_h264_encoder() == "openh264"):
                sdp_text = re.sub(r'profile-level-id=[0-9A-Fa-f]{6}',
                                  'profile-level-id=f4001f', sdp_text)
        if "opus/" in sdp_text.lower():
            frame_ms = float(getattr(app_settings, 'audio_frame_duration_ms', '10') or 10)
            # A 2.5 ms frame advertises 3; pcmflux keeps the real frame.
            ptime = int(frame_ms + 0.5)
            # a=ptime is media-level: it must sit in the audio m-section, not after
            # the video sprop lines, and audio-less offers must skip it.
            if f"a=ptime:{ptime}" not in sdp_text:
                sections = re.split(r'(?m)(?=^m=)', sdp_text)
                for i, section in enumerate(sections):
                    if section.startswith('m=audio'):
                        lines = section.split('\r\n')
                        lines.insert(1, f'a=ptime:{ptime}')
                        sections[i] = '\r\n'.join(lines)
                        break
                sdp_text = ''.join(sections)

        sdp_text = self._munge_video_bandwidth(sdp_text)

        return sdp_text

    def _munge_video_bandwidth(self, sdp_text: str) -> str:
        """Raise the bandwidth ceiling of every video m-section in the SDP.

        A generous `b=AS` keeps the browser's REMB from throttling a
        high-bitrate desktop stream (it is a cap hint, not a target), and
        `x-google-max-bitrate` mirrors it on the Chrome receive side. Both are
        scoped to each video m-section: the `b=AS` presence check looks only
        inside that section (a `b=AS` elsewhere says nothing about this
        section's ceiling), and the line goes after the section's own `c=`,
        or right after the `m=video` line when the section inherits the
        session-level `c=` (RFC 4566). The x-google hints go on the fmtp of
        every video rtpmap payload type except RTX, so VP8/VP9 get them too;
        a codec without an fmtp gets one carrying just the hints.
        """
        XGOOGLE = "x-google-max-bitrate=300000;x-google-min-bitrate=0"
        lines = sdp_text.split("\r\n")
        out: List[str] = []
        i = 0
        n = len(lines)
        while i < n:
            line = lines[i]
            if not line.startswith("m=video"):
                out.append(line)
                i += 1
                continue

            section = [line]
            i += 1
            while i < n and not lines[i].startswith("m="):
                section.append(lines[i])
                i += 1

            if not any(s.startswith("b=AS:") for s in section):
                c_idx = next(
                    (idx for idx, s in enumerate(section) if s.startswith("c=")),
                    None,
                )
                insert_at = (c_idx + 1) if c_idx is not None else 1
                section.insert(insert_at, "b=AS:300000")

            video_pts = []
            for s in section:
                m = re.match(r'a=rtpmap:(\d+)\s+(\S+)', s)
                if m and not m.group(2).lower().startswith("rtx/"):
                    video_pts.append(m.group(1))

            for pt in video_pts:
                fmtp_idx = next(
                    (idx for idx, s in enumerate(section)
                     if s.startswith("a=fmtp:{} ".format(pt))),
                    None,
                )
                if fmtp_idx is not None:
                    if "x-google-max-bitrate" not in section[fmtp_idx]:
                        section[fmtp_idx] = re.sub(
                            r'^(a=fmtp:{} )'.format(pt),
                            r'\g<1>' + XGOOGLE + ';',
                            section[fmtp_idx],
                        )
                else:
                    rtpmap_idx = next(
                        (idx for idx, s in enumerate(section)
                         if s.startswith("a=rtpmap:{} ".format(pt))),
                        None,
                    )
                    if rtpmap_idx is not None:
                        section.insert(rtpmap_idx + 1, "a=fmtp:{} {}".format(pt, XGOOGLE))

            out.extend(section)

        return "\r\n".join(out)

    def consume_data(self, buf: Any, pts: Optional[int], kind: str,
                     display_id: str = "primary") -> None:
        """Feed one encoded frame from the capture side into a display's bridge.

        Synchronous: scheduled via `loop.call_soon_threadsafe` from the capture
        thread, since `set_data` does not await — no per-frame Future/Task.
        `EncodedPacket` references the encoder buffer without copying and the
        packers walk it as a memoryview, so no per-frame FFmpeg object is
        allocated and no whole-frame copy is taken.

        Args:
            buf: Buffer-protocol object holding the encoded sample.
            pts: Presentation timestamp in the stream's clock, or None.
            kind: "video" or "audio".
            display_id: Display whose media graph receives the sample.
        """
        graph = self.displays.get(display_id or "primary")
        if graph is None:
            return
        if kind == "video":
            if buf:
                try:
                    RTP_VIDEO_CLOCK_RATE = 90000
                    packet = EncodedPacket(buf, pts, Fraction(1, RTP_VIDEO_CLOCK_RATE))
                    bridge = graph.get("video_bridge")
                    if bridge is not None:
                        bridge.set_data(packet)
                except Exception as e:
                    logger.error(f"error processing video sample: {e}")
        elif kind == "audio":
            if buf:
                try:
                    packet = EncodedPacket(buf, pts, Fraction(1, 48000))
                    bridge = graph.get("audio_bridge")
                    if bridge is not None:
                        bridge.set_data(packet)
                except Exception as e:
                    logger.error(f"error processing audio sample: {e}")

    def update_rtc_config(self, stun_servers: List[str], turn_servers: List[str]) -> None:
        """Update the STUN/TURN servers used for every NEW peer connection.

        get_rtc_config() reads these at peer-creation time, so a refresh (typically
        rotated TURN REST credentials) takes effect for every subsequent connection.
        Live sessions deliberately keep their established ICE: their TURN allocations
        stay valid, and forcing an ICE restart on refresh would drop working streams.
        """
        changed = (stun_servers, turn_servers) != (self.stun_servers, self.turn_servers)
        self.stun_servers = stun_servers
        self.turn_servers = turn_servers
        if changed:
            logger.debug(
                "RTC ICE servers updated; applies to new connections "
                "(established sessions keep their current ICE)."
            )

    def format_turn_servers(self, turn_servers: List[str]) -> List[Dict[str, Optional[str]]]:
        """Parse turn:// or turns:// URL strings into ICE server dicts.

        Non-TURN or unparsable entries are skipped; missing ports fall back to
        the scheme default, bare IPv6 hosts are bracketed, and URL-encoded
        credentials are decoded.

        Returns:
            One dict per valid server with `urls` and, when the URL carried
            them, `username` / `credential` keys.
        """
        formatted_servers: List[Dict[str, Optional[str]]] = []
        for server in turn_servers or []:
            if not isinstance(server, str):
                continue

            lower_server = server.lower()
            if not (lower_server.startswith("turn://") or lower_server.startswith("turns://")):
                continue

            parsed = urllib.parse.urlparse(server)
            if not parsed.hostname:
                continue

            scheme = 'turns' if parsed.scheme.lower() == 'turns' else 'turn'
            try:
                port = parsed.port or (443 if scheme == 'turns' else 3478)
            except ValueError:
                port = 443 if scheme == 'turns' else 3478

            host = parsed.hostname
            if host and ":" in host and not (host.startswith("[") and host.endswith("]")):
                host = f"[{host}]"

            query = f"?{parsed.query}" if parsed.query else ""
            turn_entry: Dict[str, Optional[str]] = {
                'urls': f'{scheme}:{host}:{port}{query}'
            }

            if parsed.username is not None and parsed.password is not None:
                turn_entry['username'] = urllib.parse.unquote(parsed.username)
                turn_entry['credential'] = urllib.parse.unquote(parsed.password)

            formatted_servers.append(turn_entry)
        return formatted_servers

    def format_stun_servers(self, stun_servers: List[str]) -> List[str]:
        """Strip the URL scheme separator from each STUN server string."""
        formatted_servers: List[str] = []
        for stun in stun_servers:
            server = stun.split("//")
            formatted_servers.append("".join(server))
        return formatted_servers

    def get_rtc_config(self) -> RTCConfiguration:
        """Build the RTCConfiguration for a new peer from the current servers.

        Operator-configured public addresses (`webrtc_public_ip`, comma- or
        space-separated IPv4/IPv6) are advertised in host ICE candidates for
        hosts behind static 1:1 NAT; each family maps to its own host
        candidates.
        """
        formatted_turn_servers = self.format_turn_servers(self.turn_servers)
        formatted_stun_servers = self.format_stun_servers(self.stun_servers)
        logger.debug(f"stun servers: {formatted_stun_servers}")
        logger.debug(f"turn servers: {formatted_turn_servers}")

        ice_servers = []
        if self.stun_servers:
            ice_servers.append(RTCIceServer(urls=formatted_stun_servers))
        for turn in formatted_turn_servers:
            turn_kwargs: Dict[str, Any] = {
                'urls': turn.get('urls', [])
            }
            if turn.get('username') is not None:
                turn_kwargs['username'] = turn.get('username')
            if turn.get('credential') is not None:
                turn_kwargs['credential'] = turn.get('credential')
            ice_servers.append(RTCIceServer(**turn_kwargs))
        public_ips = (
            getattr(app_settings, "webrtc_public_ip", "") or ""
        ).replace(",", " ").split()
        config = RTCConfiguration(
            iceServers=ice_servers,
            bundlePolicy=RTCBundlePolicy.MAX_BUNDLE,
            iceHostPublicIps=public_ips or None,
        )
        return config

    def force_codec(self, pc: RTCPeerConnection, sender: RTCRtpSender,
                    forced_codec_mime: str) -> None:
        """Restrict a sender's codec preferences to one MIME type plus RTX.

        Every codec matching the MIME type stays eligible — H.264 appears once
        per advertised profile. FlexFEC rides along when the receiver supports
        it (Chrome family); a receiver without it answers without the codec and
        the sender emits no repair stream.

        Args:
            pc: Peer connection owning the sender's transceiver.
            sender: RTP sender whose transceiver is being restricted.
            forced_codec_mime: MIME type (e.g. "video/H264") to force.

        Raises:
            ValueError: When the codec or its RTX companion is not in the
                sender capabilities.
        """
        kind = sender.track.kind
        capabilities = RTCRtpSender.getCapabilities(kind)
        logger.debug(f"Current capabilities for {kind}: {capabilities}")

        chosen_codec = []
        for codec in capabilities.codecs:
            if codec.mimeType == forced_codec_mime:
                chosen_codec.append(codec)

        if not chosen_codec:
            raise ValueError(f"Codec {forced_codec_mime} not found in capabilities")

        rtx_codec = None
        for codec in capabilities.codecs:
            if codec.mimeType.lower() == f"{kind}/rtx":
                rtx_codec = codec
                break

        if not rtx_codec:
            raise ValueError(f"RTX codec for {forced_codec_mime} not found")

        flexfec_codec = next(
            (
                codec
                for codec in capabilities.codecs
                if codec.mimeType.lower() == f"{kind}/flexfec-03"
            ),
            None,
        )
        preferences = [*chosen_codec, rtx_codec]
        if flexfec_codec is not None:
            preferences.append(flexfec_codec)

        transceiver = next(t for t in pc.getTransceivers() if t.sender == sender)
        logger.debug(f"Forcing codec preferences to: {preferences}")
        transceiver.setCodecPreferences(preferences)

    async def _drain_channel_queue(self, queue: asyncio.Queue,
                                   handler: Callable[[Any], Any],
                                   label: str) -> None:
        """Single consumer that dispatches queued messages strictly in order.

        Running one awaited handler at a time is what guarantees ordering: if
        each message spawned its own task, handlers that await mid-dispatch
        could complete out of order (e.g. a key-up finishing before its
        key-down, sticking the key).
        """
        while True:
            msg = await queue.get()
            try:
                result = handler(msg)
                if inspect.isawaitable(result):
                    await result
            except asyncio.CancelledError:
                raise
            except Exception as e:
                logger.error("Error handling message on channel %s: %s", label, e)

    def _serialize_channel(self, channel: RTCDataChannel,
                           handler: Callable[[Any], Any],
                           max_queue: int = 512) -> "asyncio.Task":
        """Wire a channel's messages through a bounded per-channel queue drained
        by a single consumer task, so dispatch stays in arrival order.

        The message handler only enqueues (drop+log on overflow); the consumer
        is cancelled when the channel closes. handler is called late-bound so
        reassigning the target callback still takes effect.

        Returns:
            The consumer task, so teardown paths can cancel it for channels
            that never emit a close event.
        """
        queue: asyncio.Queue = asyncio.Queue(maxsize=max_queue)

        def _enqueue(msg: Any) -> None:
            try:
                queue.put_nowait(msg)
            except asyncio.QueueFull:
                logger.warning("Data channel %s input queue full, dropping message", channel.label)

        consumer = self.async_event_loop.create_task(
            self._drain_channel_queue(queue, handler, channel.label)
        )
        channel.on("message", _enqueue)
        channel.on("close", lambda: consumer.cancel())
        return consumer

    def _send_collab_state(self, channel: RTCDataChannel,
                           client_type: ClientType,
                           client_token: Optional[str]) -> None:
        """Send a peer its mk-token input verdict over the data channel.

        Delivered as a `system` action (the channel is JSON-typed):
        `mk_access,1` attaches the client's input context, `mk_access,0`
        detaches it. Sent to controllers as well as viewers (websockets
        MK_ACCESS parity) — an mk handoff strips a controller's input
        authority too, and without the verdict its page keeps a live input UI
        whose messages the server drops. No-op outside secure mode. Gated on
        secure mode itself, NOT on an mk token existing — a handoff that
        clears the token must still push the 0 that detaches the previous
        holder.
        """
        if not app_settings.master_token:
            return
        granted = (
            self._viewer_is_collaborator(client_token)
            if client_type == ClientType.VIEWER
            else self._mk_input_authorized(client_token)
        )
        try:
            channel.send(json.dumps(
                {"type": "system", "data": {"action": f"mk_access,{1 if granted else 0}"}}))
        except Exception:
            logger.debug("collab-state send failed (channel closing)", exc_info=True)

    def _send_auth_success(self, channel: RTCDataChannel,
                           client_type: ClientType,
                           client_token: Optional[str]) -> None:
        """Tell the client its effective role/slot (websockets AUTH_SUCCESS
        parity): role coercion is decided server-side, so the page must learn
        the verdict to degrade its own UI instead of driving a controller UI
        whose input is all dropped."""
        try:
            role = "controller" if client_type == ClientType.CONTROLLER else "viewer"
            slot = None
            if client_token:
                perms = current_session_tokens()[0].get(client_token)
                if perms:
                    slot = perms.get("slot")
            verdict = json.dumps({"role": role, "slot": slot})
            channel.send(json.dumps(
                {"type": "system", "data": {"action": f"auth_success,{verdict}"}}))
        except Exception:
            logger.debug("auth_success send failed (channel closing)", exc_info=True)

    def _viewer_is_collaborator(self, client_token: Optional[str]) -> bool:
        """A viewer holding the active mk (mouse+keyboard) token is a read-write
        collaborator — mirrors the WS mk-token path — but only while enable_collab
        is on. Fail-safe: any missing piece means not a collaborator (stays
        read-only)."""
        if not client_token:
            return False
        if not bool(app_settings.enable_collab[0]):
            return False
        _, mk = current_session_tokens()
        return mk is not None and client_token == mk

    def _mk_input_authorized(self, client_token: Optional[str]) -> bool:
        """Token-level input authority in secure mode: the active mk-token holder,
        or a controller-role token while no mk token is provisioned."""
        tokens, mk = current_session_tokens()
        if mk is not None:
            return bool(client_token) and client_token == mk
        perms = tokens.get(client_token) if client_token else None
        return bool(perms) and perms.get("role") == "controller"

    def peer_holds_input_authority(self, peer: Optional[Dict[str, Any]]) -> bool:
        """Whether a peer entry may drive keyboard/mouse input, composing the two
        gates on_data_message applies: a viewer needs to be a read-write
        collaborator, and in secure mode every peer is additionally held to the
        token check. Used for session-wide input cleanup (held keys and pointer
        buttons are one global desktop state), so it is deliberately fail-safe:
        an unknown peer holds nothing."""
        if not peer:
            return False
        client_token = peer.get("client_token")
        if peer.get("client_type") == ClientType.VIEWER and not self._viewer_is_collaborator(client_token):
            return False
        if not app_settings.master_token:
            return True
        return self._mk_input_authorized(client_token)

    def _secure_input_denied(self, msg: str, client_token: Optional[str]) -> bool:
        """Secure-mode (master token configured) input authority, mirroring the WS
        gate: cmd and the keyboard/mouse/clipboard set are admitted only from the
        active mk-token holder, or from a controller-role token when no mk-token is
        provisioned. client_type is self-asserted over signaling, so a peer that
        merely claims 'controller' is still held to the token here. `co`
        (composed-text typing, `co,end,<text>`) is keyboard input like kd/ku.
        The bare `cr` clipboard read-back is exempt like every clipboard read
        on the websockets transport: the handler itself direction-gates it
        (enable_clipboard "out") and it is sent at connect, before the peer
        can hold input authority.

        Returns:
            True when the message must be dropped.
        """
        if not app_settings.master_token:
            return False
        if msg.split(",", 1)[0] in ("cr",):
            return False
        if msg.split(",", 1)[0] not in ("cmd", "co") and not msg.startswith(VIEWER_COLLAB_EXTRA_PREFIXES):
            return False
        authorized = self._mk_input_authorized(client_token)
        if not authorized:
            logger.warning("Dropping unauthorized secure-mode input: %s", msg[:32])
        return not authorized

    def _gamepad_denied(self, msg: str, client_type: Optional[ClientType],
                        client_token: Optional[str],
                        client_slot: Optional[int]) -> bool:
        """Gamepad slot authority, mirroring the WS gate: a client may only drive
        the slot it holds, so a viewer or collaborator can't spoof another
        player's controller.

        Under a master token the slot comes from the live token store, so a
        revocation or re-slot lands on the next message; otherwise from the
        peer's HELLO claim, which the signaling server validated (`-1` is its
        unassigned sentinel). A legacy controller's claim of slot 1 is registry
        identity rather than a gamepad restriction — the websockets handshake
        gives it no slot at all — and is dropped, so the same client in the
        same role is governed the same on both transports.

        Returns:
            True when the gamepad message must be dropped.
        """
        if not msg.startswith("js,"):
            return False
        role = "controller" if client_type is ClientType.CONTROLLER else "viewer"
        if app_settings.master_token:
            tokens, _ = current_session_tokens()
            perms = tokens.get(client_token) if client_token else None
            slot = perms.get("slot") if perms else None
        elif role == "viewer":
            slot = client_slot if (client_slot or 0) > 0 else None
        else:
            slot = None
        return gamepad_slot_denied(msg, role, slot, bool(app_settings.master_token))

    def _on_input_channel_message(self, msg: Any,
                                  channel: Optional[RTCDataChannel] = None,
                                  client_type: Optional[ClientType] = None,
                                  client_token: Optional[str] = None,
                                  display_id: str = "primary",
                                  peer_id: Optional[str] = None,
                                  client_slot: Optional[int] = None) -> Any:
        """Pre-filter one data-channel message before the input dispatcher.

        In order: a gzip'd payload is inflated with a bound (the channel's
        negotiated max-message-size caps only the compressed size, websockets
        0x05 parity); the `_gz,1` handshake marks this channel for gzip'd
        sends and is echoed; a viewer's SETTINGS snapshot is connection sync
        only, never applied (the websockets transport likewise ignores viewer
        payloads); a viewer may send only the allow-listed messages, and a
        read-write collaborator (mk token plus enable_collab) additionally the
        keyboard/mouse/clipboard set — the same two tiers as the websockets
        gate, so a collaborator still cannot send cmd — with blur/visibility
        noise dropped silently and the collaborator check reached only for
        otherwise-disallowed input so normal viewer traffic pays nothing; the
        secure-mode input and gamepad gates apply; STOP_VIDEO / START_VIDEO
        pause this peer only (viewer-allowed, so a hidden viewer pauses its
        own feed); STOP_AUDIO / START_AUDIO are ignored because audio is
        negotiated per peer over SDP here, so the websockets global toggle
        would be a no-op for late joiners or cut audio for peers that never
        asked — each page mutes its `<video>` locally, which stops Opus
        playback without stopping RTP. Everything else reaches the late-bound
        `on_data_message` with the peer id as the connection id, so
        per-connection input state (gamepad associations) traces to the peer.

        Returns:
            Whatever the dispatched handler returns (possibly an awaitable,
            which the channel's queue consumer awaits), or None when the
            message was consumed or dropped here.
        """
        if isinstance(msg, (bytes, bytearray)) and bytes(msg[:2]) == b"\x1f\x8b":
            try:
                msg = inflate_gz_bounded(msg)
            except Exception:
                logger.warning("Dropping undecodable compressed data channel message")
                return
        if msg == "_gz,1":
            if channel is not None:
                channel._selkies_gz_tx = True
                try:
                    channel.send("_gz,1")
                except Exception as e:
                    logger.warning("Failed to ack compression handshake: %s", e)
            return
        if client_type == ClientType.VIEWER and isinstance(msg, str) and msg.startswith("SETTINGS,"):
            logger.debug("Ignoring SETTINGS payload from a viewer (display '%s')", display_id)
            return
        if client_type == ClientType.VIEWER and isinstance(msg, str):
            if not msg.startswith(VIEWER_ALLOWED_PREFIXES) and not (
                msg.startswith(VIEWER_COLLAB_EXTRA_PREFIXES)
                and self._viewer_is_collaborator(client_token)
            ):
                if not msg.startswith(VIEWER_SILENT_DROP_PREFIXES):
                    logger.warning("Dropping unauthorized viewer input: %s", msg[:32])
                return
        if isinstance(msg, str) and self._secure_input_denied(msg, client_token):
            return
        if isinstance(msg, str) and self._gamepad_denied(msg, client_type, client_token, client_slot):
            logger.warning("Dropping gamepad input for a slot this peer does not hold: %s", msg[:32])
            return
        if msg in ("STOP_VIDEO", "START_VIDEO") and self.on_video_consumer_active is not None:
            return self.on_video_consumer_active(
                peer_id, display_id or "primary", msg == "START_VIDEO")
        if msg in ("STOP_AUDIO", "START_AUDIO"):
            logger.debug("Ignoring %s over WebRTC: audio is per-peer (SDP), not global.", msg)
            return
        return self.on_data_message(msg, display_id or "primary", conn_id=peer_id)

    async def on_peer_connection_established(self, client_peer_id: str, client_type: ClientType, display_id: str = "primary") -> None:
        """Start the display's capture when a peer finishes connecting.

        Every consumer asks, not just the controller: a lone viewer must get
        the desktop it joined for (websockets parity), and the start is
        idempotent on a display whose capture is already running.
        """
        await self.start_display_media(display_id)
        logger.info(f"Media pipeline start requested for {client_peer_id} (display '{display_id}')")

    async def _default_start_display_media(self, display_id: str) -> None:
        """Single-display default: only the primary pipeline is started."""
        if display_id == "primary" and self.media_pipeline:
            await self.media_pipeline.start_media_pipeline()

    async def _default_stop_display_media(self, display_id: str) -> None:
        """Single-display default: only the primary pipeline is stopped."""
        if display_id == "primary" and self.media_pipeline:
            await self.media_pipeline.stop_media_pipeline()

    async def on_connectionstatechange(self, client_peer_id: str) -> None:
        """React to a peer connection's state changes.

        The "closed" branch is the teardown point for a peer whose client
        vanished without a session end (ICE failure, the browser going away):
        it deregisters the peer and reaps what the peer owned (consumer tasks,
        mic playback, the display's media while nothing else consumes it, the
        on_peer_gone hook). An explicit stop deregisters the peer before
        closing it, so for that peer this handler finds no entry and the stop
        reaps it itself.
        """
        peer_conn = None
        peer_obj = None
        if client_peer_id:
            peer_obj = self.peer_connections.get(client_peer_id, None)
            if peer_obj:
                peer_conn = peer_obj.get("peer_conn")

        if peer_conn is None:
            logger.debug("No peer connection found for connectionstatechange")
            return

        state = peer_conn.connectionState
        client_type = peer_obj.get('client_type') if peer_obj else ''
        display_id = (peer_obj.get('display_id') if peer_obj else None) or 'primary'
        if state == "failed":
            await peer_conn.close()
        elif state == "disconnected":
            logger.warning("Peer connection disconnected", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        elif state == "connected":
            await self.on_peer_connection_established(client_peer_id, client_type, display_id)
            logger.info("Peer connection established", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        elif state == "closed":
            self.peer_connections.pop(client_peer_id, None)
            await self._reap_peer(client_peer_id, peer_obj)
            logger.info("Peer connection closed", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        elif state == "connecting":
            logger.info("Peer connection is connecting", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        else:
            logger.debug(f"Unhandled peer connection state: {state}", extra={'client_peer_id': client_peer_id, 'client_type': client_type})

    def on_pli(self, client_peer_id: str, client_type: str) -> None:
        """Translate a peer's RTP PLI into an IDR request for its display."""
        logger.debug("PLI occurred, triggering IDR frame request", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        peer_obj = self.peer_connections.get(client_peer_id) or {}
        display_id = peer_obj.get("display_id") or "primary"
        asyncio.run_coroutine_threadsafe(self.request_idr_frame(display_id), self.async_event_loop)

    def _idr_on_video_drop(self, display_id: str,
                           min_interval: float = 0.5) -> Callable[[], None]:
        """Build the drop hook for a display's video bridge.

        A dropped encoded frame leaves the wire referencing a picture no
        client received; the drop is upstream of RTP so no loss is signalled
        and the browser never asks for a keyframe, leaving a persistent smear
        under infinite GOP. The hook forces a recovery IDR, debounced so a
        sustained consumer lag doesn't turn every dropped frame into a large
        keyframe and deepen the congestion.
        """
        state: Dict[str, Optional[float]] = {"last": None}

        def on_drop() -> None:
            loop = self.async_event_loop
            if loop is None:
                return
            now = loop.time()
            if state["last"] is not None and now - state["last"] < min_interval:
                return
            state["last"] = now
            req = getattr(self, "request_idr_frame", None)
            if req is None:
                return
            try:
                asyncio.run_coroutine_threadsafe(req(display_id), loop)
            except Exception:
                pass

        return on_drop

    async def _start_rtc_pipeline(
        self,
        client_peer_id: str,
        c_type: str,
        client_token: Optional[str] = None,
        display_id: str = "primary",
        client_slot: Optional[int] = None,
    ) -> None:
        """Create a peer connection and send its offer over signaling.

        Builds the display's media graph when none exists yet, attaches the
        media tracks, the mic and webcam receivers, and the serialized input
        channel, and registers the peer entry only after the offer was sent —
        a failure before registration tears the half-built connection down
        here (consumer task and connection) because no other teardown path
        could ever find it. Registration ends with a consumers-changed
        notification so a capture stopped by the all-consumers-paused rule
        restarts for the joiner (websockets parity: a joining shared viewer
        always restarts a stopped capture).

        A display's media graph is shared by every peer of the display and
        lives while any of them consumes it. Its controller creates it; on the
        primary display a lone viewer does too, since the desktop exists
        whether or not anyone controls it (websockets parity), while a
        secondary display exists only through its controller's layout, so a
        viewer cannot bring one up. Audio and the mic and webcam return paths
        exist only on the primary display: a secondary display page renders
        video and carries input, matching the websockets model.

        The mic is one recvonly audio transceiver inside the same bundled SDP
        (no second negotiation), inactive until the client attaches a track,
        so it is negotiated whenever audio is on: `microphone_enabled` only
        picks the client-side default, and a runtime enable must not need a
        renegotiation the stack does not do. A locked-off microphone withholds
        the m-line entirely. The webcam has the same shape as one recvonly
        video transceiver. The input data channel is reliable and ordered:
        input, clipboard and upload control all ride it and none tolerates
        loss.

        Args:
            client_peer_id: Signaling id of the connecting peer.
            c_type: Client role string, coerced to `ClientType`.
            client_token: Session token the peer authenticated with, if any.
            display_id: Display this peer attaches to.
            client_slot: One-based player slot the peer claimed at HELLO, which
                the gamepad gate holds it to outside secure mode.

        Raises:
            RTCAppError: When a viewer joins a secondary display that has no
                media graph (its controller defines it) or the encoder is
                unsupported.
        """
        client_type = ClientType(c_type)
        display_id = display_id or "primary"

        graph = self.displays.get(display_id)
        if graph is None and (client_type is ClientType.CONTROLLER or display_id == "primary"):
            graph = {"relay": MediaRelay()}
            graph["video_bridge"] = PipelineBridge(on_drop=self._idr_on_video_drop(display_id))
            graph["video_media"] = VideoMedia(graph["video_bridge"])
            if display_id == "primary":
                graph["audio_bridge"] = PipelineBridge(maxsize=8)
                graph["audio_media"] = AudioMedia(graph["audio_bridge"])
            self.displays[display_id] = graph
            logger.info(f"Media relay and pipeline bridges created for display '{display_id}' ({client_type.value} peer)")
        if graph is None:
            raise RTCAppError(
                f"Cannot create peer connection: no media graph for display '{display_id}'. Controller may be disconnected."
            )

        peer_connection =  RTCPeerConnection(self.get_rtc_config())
        media_relay = graph["relay"]

        rtp_video_sender = peer_connection.addTrack(media_relay.subscribe(graph["video_media"]))
        rtp_video_sender.on("pli", lambda cid=client_peer_id, ct=client_type: self.on_pli(cid, ct))
        if graph.get("audio_media") is not None:
            peer_connection.addTrack(media_relay.subscribe(graph["audio_media"]))

        mic_on, mic_locked = app_settings.microphone_enabled
        mic_state = None
        if display_id == "primary" and bool(app_settings.audio_enabled[0]) and (mic_on or not mic_locked):
            mic_state = self._setup_mic_receiver(peer_connection, client_type, client_token)

        webcam_state = None
        if display_id == "primary" and not webcam_locked_off():
            webcam_state = self._setup_webcam_receiver(peer_connection, client_type, client_token)

        data_channel = peer_connection.createDataChannel("input", ordered=True)

        data_channel.on("open", lambda ch=data_channel: self.on_data_open(ch))
        data_channel.on("open", lambda ch=data_channel, ct=client_type, tok=client_token:
                        self._send_collab_state(ch, ct, tok))
        data_channel.on("open", lambda ch=data_channel, ct=client_type, tok=client_token:
                        self._send_auth_success(ch, ct, tok))
        data_channel.on("close", lambda: self.on_data_close())
        data_channel.on("error", lambda e=None: self.on_data_error(e))
        input_consumer = self._serialize_channel(
            data_channel,
            lambda msg, ch=data_channel, ct=client_type, tok=client_token, did=display_id, pid=client_peer_id, slot=client_slot: self._on_input_channel_message(msg, ch, ct, tok, did, pid, slot),
        )

        peer_connection.on("connectionstatechange", lambda cid=client_peer_id: asyncio.run_coroutine_threadsafe(self.on_connectionstatechange(cid), loop=self.async_event_loop))

        try:
            try:
                display_encoder = self.get_encoder_for_display(display_id) or self.encoder
            except Exception:
                display_encoder = self.encoder
            try:
                display_fullcolor = bool(self.get_fullcolor_for_display(display_id))
            except Exception:
                display_fullcolor = bool(app_settings.video_fullcolor[0])
            try:
                display_use_cpu = bool(self.get_use_cpu_for_display(display_id))
            except Exception:
                display_use_cpu = bool(app_settings.use_cpu[0])
            preferred_codec = self.get_mime_by_encoder(display_encoder)
            if preferred_codec is None:
                raise RTCAppError(f"Encoder {display_encoder} is not supported")
            self.force_codec(peer_connection, rtp_video_sender, preferred_codec)

            await peer_connection.setLocalDescription(await peer_connection.createOffer())
            offer = peer_connection.localDescription

            sdp = offer.sdp
            sdp = self.munge_sdp(sdp, display_encoder, display_fullcolor, display_use_cpu)
            await self.on_sdp('offer', sdp, client_peer_id)
        except BaseException:
            input_consumer.cancel()
            try:
                await peer_connection.close()
            except Exception:
                logger.warning("Failed to close peer connection after failed start", exc_info=True)
            raise

        peer_slot = client_slot if (client_slot or 0) > 0 else None
        if client_token:
            _perms = current_session_tokens()[0].get(client_token)
            if _perms:
                peer_slot = _perms.get("slot")

        self.peer_connections[client_peer_id] = {
            "peer_conn": peer_connection,
            "data_channel": data_channel,
            "client_type": client_type,
            "client_slot": peer_slot,
            "display_id": display_id,
            "channel_consumers": [input_consumer],
            "mic_state": mic_state,
            "webcam_state": webcam_state,
            "video_sender": rtp_video_sender,
            "video_paused": False,
            "client_token": client_token,
        }
        await self._notify_consumers_changed(display_id)

    def _setup_mic_receiver(self, peer_connection: RTCPeerConnection,
                            client_type: Optional[ClientType] = None,
                            client_token: Optional[str] = None) -> Dict[str, Any]:
        """Add a recvonly mic transceiver and route its Opus into pcmflux.

        The encoded payload goes straight into pcmflux — no aiortc/Python Opus
        decode. RED (UDP loss resilience) is gated by audio_redundancy: when
        on, the shared caps offer it and pcmflux de-frames + loss-recovers
        each RED payload off the GIL before decoding (the RTP timestamp
        anchors the redundant blocks' offsets); when off, the m-line is
        restricted to plain Opus and packets are decoded directly.

        Only a controller or a live collab (m/k) holder speaks into the
        desktop mixer — the websockets transport's mic gate verdict — and the
        collab state is read per packet so an m/k handoff takes effect without
        renegotiation. The first packet opens the pcmflux playback off the
        loop, dropping packets until it is ready; the shared SelkiesVirtualMic
        is provisioned first (`provision_virtual_mic`, idempotent, shared with
        the websockets path) so apps recording the default source hear this
        mic, and a provisioning failure does not block playback. A start that
        completes after the peer was torn down stops its playback instead of
        publishing it. pcmflux raises once its playback worker dies (e.g.
        PulseAudio restarted mid-run), so a failed write drops the chunk and
        tears the stream down for the next packet to reopen — swallowing it
        would leave this peer's mic silent forever (websockets parity).

        Returns:
            The per-peer mic state dict (`pb`, `starting`, `closed`) that
            `_stop_mic_playback_state` later tears down.
        """
        mic_tx = peer_connection.addTransceiver("audio", direction="recvonly")
        if not bool(app_settings.audio_redundancy[0]):
            try:
                caps = RTCRtpSender.getCapabilities("audio")
                opus_only = [c for c in caps.codecs if c.mimeType.lower() == "audio/opus"]
                if opus_only:
                    mic_tx.setCodecPreferences(opus_only)
            except Exception as e:
                logger.info(f"mic opus-only preference not applied: {e}")

        loop = self.async_event_loop
        state: Dict[str, Any] = {"pb": None, "starting": False, "closed": False}

        def sink(codec: Any, frame: Any) -> None:
            if state["closed"]:
                return
            if client_type is ClientType.VIEWER and not self._viewer_is_collaborator(client_token):
                if not state.get("role_denied_logged"):
                    state["role_denied_logged"] = True
                    logger.info("Dropping microphone audio from a view-only peer (no m/k authority).")
                return
            data = bytes(getattr(frame, "data", b"") or b"")
            if not data:
                return
            pb = state["pb"]
            if pb is None:
                if not state["starting"]:
                    state["starting"] = True

                    async def _start():
                        try:
                            if pcmflux is None:
                                raise RuntimeError("pcmflux is not installed")
                            if self.provision_virtual_mic is not None:
                                try:
                                    await self.provision_virtual_mic()
                                except Exception as e_prov:
                                    logger.error(f"WebRTC virtual mic provisioning failed: {e_prov}")
                            pb2 = pcmflux.AudioPlayback()
                            ps = pcmflux.AudioPlaybackSettings()
                            ps.device_name = b"input"
                            ps.sample_rate = 24000
                            ps.channels = 1
                            ps.latency_ms = 40
                            await asyncio.to_thread(pb2.start, ps)
                            if state["closed"]:
                                await asyncio.to_thread(pb2.stop)
                                return
                            state["pb"] = pb2
                        except Exception as e:
                            logger.error(f"WebRTC mic playback start failed: {e}")
                            state["starting"] = False

                    loop.call_soon_threadsafe(lambda: asyncio.ensure_future(_start()))
                return
            try:
                if getattr(codec, "name", "").lower() == "red":
                    pb.write_red(data, int(getattr(frame, "timestamp", 0) or 0))
                else:
                    pb.write(data)
            except Exception as e:
                logger.error(f"WebRTC mic playback write failed: {e}")
                state["pb"] = None
                state["starting"] = False

                def _teardown(dead=pb):
                    async def _t():
                        try:
                            await asyncio.to_thread(dead.stop)
                        except Exception:
                            pass
                    asyncio.ensure_future(_t())

                loop.call_soon_threadsafe(_teardown)

        mic_tx.receiver._encoded_audio_sink = sink
        return state

    def _setup_webcam_receiver(self, peer_connection: RTCPeerConnection,
                               client_type: Optional[ClientType] = None,
                               client_token: Optional[str] = None) -> Dict[str, Any]:
        """Add a recvonly video transceiver and route its encoded frames into the
        virtual webcam.

        The browser encodes its camera with its own WebRTC encoder (hardware
        where it has one) and the depacketized frames — Annex-B H.264, VP8 or
        VP9 — go straight to pixelflux, which decodes them off the GIL; no
        Python decode and no data-channel chunking. When the decoder asks for a
        keyframe (after a drop or a late start) the request becomes a PLI.

        The first frame brings the camera up off the loop; frames until then
        are dropped and the decoder's first keyframe request becomes a PLI, so
        the stream starts clean. A running camera that an uplink of the other
        kind finds is re-created there too, when nothing is reading it. The
        start latch is released only on success, so a camera that cannot
        start is not retried per frame while one that did leaves the next
        uplink free to ask for its own format.

        Returns:
            The per-peer webcam state dict that `_close_webcam_state` retires.
        """
        cam_tx = peer_connection.addTransceiver("video", direction="recvonly")
        try:
            caps = RTCRtpSender.getCapabilities("video")
            wanted = ("video/h264", "video/vp8", "video/vp9", "video/rtx")
            preferred = [c for c in caps.codecs if c.mimeType.lower() in wanted]
            if preferred:
                cam_tx.setCodecPreferences(preferred)
        except Exception as e:
            logger.info(f"webcam codec preference not applied: {e}")

        loop = self.async_event_loop
        receiver = cam_tx.receiver
        webcam = get_shared_webcam()
        state: Dict[str, Any] = {"closed": False, "starting": False, "last_pli": 0.0}

        def request_keyframe() -> None:
            now = time.monotonic()
            if now - state["last_pli"] < 0.25:
                return
            state["last_pli"] = now
            loop.call_soon_threadsafe(lambda: asyncio.ensure_future(receiver.request_keyframe()))

        def sink(codec: Any, frame: Any) -> None:
            if state["closed"]:
                return
            is_viewer = client_type is ClientType.VIEWER
            if not webcam_uplink_allowed(is_viewer, is_viewer and self._viewer_is_collaborator(client_token)):
                if not state.get("denied_logged"):
                    state["denied_logged"] = True
                    logger.info("Dropping webcam video from a peer without webcam authority.")
                return
            data = getattr(frame, "data", b"") or b""
            codec_id = CODEC_BY_NAME.get(str(getattr(codec, "name", "")).lower())
            if not data or codec_id is None:
                return
            if webcam.needs_ensure(codec_id):
                if not state["starting"]:
                    state["starting"] = True

                    async def start_camera() -> None:
                        # Released only on success: a failed camera is not retried per frame.
                        if await webcam.ensure(codec_id) is not None:
                            state["starting"] = False

                    loop.call_soon_threadsafe(lambda: asyncio.ensure_future(start_camera()))
                if webcam.camera is None:
                    return
            if webcam.keyframe_wanted(webcam.push(data, codec_id)):
                request_keyframe()

        receiver._encoded_video_sink = sink
        return state

    def _close_webcam_state(self, state: Optional[Dict[str, Any]]) -> None:
        """Retire one peer's webcam sink; the shared camera itself outlives peers."""
        if state:
            state["closed"] = True

    async def _stop_mic_playback_state(self, state: Optional[Dict[str, Any]]) -> None:
        """Stop ONE peer's mic playback.

        Per-peer ownership: a closing peer must never silence the mic of the
        other primary peers. Marks the state closed so an in-flight
        first-packet start cannot publish a live playback into a torn-down
        peer (which nothing would ever stop).
        """
        if not state:
            return
        state["closed"] = True
        pb = state.get("pb")
        state["pb"] = None
        if pb is not None:
            try:
                await asyncio.to_thread(pb.stop)
            except Exception:
                pass

    def get_mime_by_encoder(self, encoder: str) -> Optional[str]:
        """Return the RTP MIME type for an encoder name.

        Every pipeline encoder emits H.264; offering another MIME would
        negotiate a codec the stream cannot honor, so a new entry may only be
        added together with a real pixelflux encoder (the vendored webrtc
        stack keeps its VP8 RTP support for that). An unmapped encoder, e.g. a
        stale persisted client setting, must never take the transport down.

        Returns:
            The MIME type; unmapped encoders fall back to "video/H264".
        """

        encoder_mime_map = {
            "h264enc": "video/H264",
        }
        mime = encoder_mime_map.get(encoder)
        if mime is None:
            logger.error(
                f"No MIME mapping for encoder {encoder}; falling back to video/H264"
            )
            mime = "video/H264"
        return mime

    async def _cancel_channel_consumers(self, peer_obj: Dict[str, Any]) -> None:
        """Cancel and await a peer's data channel queue consumers.

        A channel that never reached SCTP-established never emits 'close', so
        its consumer is only reachable from here; cancelling one the 'close'
        event already stopped is a no-op.
        """
        consumers = peer_obj.get("channel_consumers") or []
        for consumer in consumers:
            consumer.cancel()
        if consumers:
            await asyncio.gather(*consumers, return_exceptions=True)

    async def _stop_rtc_pipeline(self, client_peer_id: str) -> None:
        """Close a peer connection and release everything the peer owned.

        The explicit-stop counterpart of the "closed" state branch. The peer
        is deregistered BEFORE its connection closes, so the state event that
        close raises finds no entry and every teardown step runs exactly once,
        here — SESSION_END arrives as soon as the client's signaling socket
        drops, long before ICE gives up on the peer, and nothing may wait for
        the state machine.

        Raises:
            RTCAppError: When teardown itself fails.
        """
        try:
            peer_obj = self.peer_connections.pop(client_peer_id, None)
            if not peer_obj:
                logger.debug(f"Peer object not found for client peer_id: {client_peer_id}")
                return
            peer_conn = peer_obj.get("peer_conn")
            if peer_conn is not None:
                await peer_conn.close()
            await self._reap_peer(client_peer_id, peer_obj)
        except Exception as e:
            raise RTCAppError(f"Error stopping pipeline: {e}") from e

    async def _reap_peer(self, client_peer_id: str, peer_obj: Dict[str, Any]) -> None:
        """Release what a deregistered peer owned; shared by the explicit stop
        and the "closed" state branch so both run the same steps: channel
        consumers, this peer's own mic playback and webcam sink, the
        `on_peer_gone` hook with the entry, then the display's media when
        nothing consumes it any more."""
        await self._cancel_channel_consumers(peer_obj)
        await self._stop_mic_playback_state(peer_obj.get("mic_state"))
        self._close_webcam_state(peer_obj.get("webcam_state"))
        if self.on_peer_gone is not None:
            try:
                await self.on_peer_gone(client_peer_id, peer_obj)
            except Exception:
                logger.exception("on_peer_gone hook failed")
        display_id = peer_obj.get("display_id") or "primary"
        await self._release_display_if_unconsumed(
            display_id, peer_obj.get("client_type") is ClientType.CONTROLLER)
        await self._notify_consumers_changed(display_id)

    async def _release_display_if_unconsumed(self, display_id: str,
                                             controller_left: bool) -> None:
        """Stop a display's media and drop its graph once the display is done.

        A secondary display is its controller's: the controller leaving ends
        it, viewers included (the graph teardown closes them). The primary's
        desktop exists on its own, so its media and graph serve the remaining
        viewers across a controller's departure and are released with the
        last consumer (the websockets engine likewise keeps a viewer-started
        capture until nothing decodes it). A display with no graph and no
        departing controller was already released (a viewer closed by the
        graph teardown) or never built (a viewer refused a graph-less
        secondary).
        """
        display_id = display_id or "primary"
        remaining = any(
            (p.get("display_id") or "primary") == display_id
            for p in self.peer_connections.values()
        )
        if remaining and not (controller_left and display_id != "primary"):
            return
        if display_id not in self.displays and not controller_left:
            return
        logger.info(f"Display '{display_id}' has no consumer left; releasing its media")
        try:
            await self.stop_display_media(display_id)
        except Exception:
            logger.exception(f"stop_display_media failed for display '{display_id}'")
        await self._teardown_display_graph(display_id)

    async def _notify_consumers_changed(self, display_id: str) -> None:
        """Tell the owning service this display's consumer set changed.

        The service re-checks the all-paused capture stop so a departing
        unpaused peer cannot leave a capture running for hidden-only
        consumers, and a joining one can restart a stopped capture.
        """
        cb = self.on_consumers_changed
        if cb is None:
            return
        try:
            result = cb(display_id or "primary")
            if inspect.isawaitable(result):
                await result
        except Exception as e:
            logger.debug(f"consumers-changed notification failed: {e}")

    async def _teardown_display_graph(self, display_id: str) -> None:
        """Drop one display's media graph, reaping its relay workers.

        The relay's run-track workers only exit when the SOURCE track errors,
        so dropping the reference alone leaks them pending in recv() ("Task
        was destroyed but it is pending!").
        """
        display_id = display_id or 'primary'
        graph = self.displays.pop(display_id, None)
        if not graph:
            return
        relay = graph.get('relay')
        if relay is not None:
            try:
                await relay.stop()
            except Exception as e_relay:
                logger.warning(f"Media relay teardown error (continuing): {e_relay}")
        await self._close_display_viewers(display_id)

    async def _close_display_viewers(self, display_id: str) -> None:
        """Close every non-controller peer of a display whose graph is gone
        (a secondary released by its controller, or a drop).

        Such a viewer (`#shared` / `#player*`) is bound to a dead source: its
        sender would block in recv() forever, frozen on the last frame, while
        ICE stays connected so the client never self-heals. Closing the
        connection lets the client see the drop and reload.
        """
        victims = [
            (pid, obj) for pid, obj in list(self.peer_connections.items())
            if (obj.get('display_id') or 'primary') == display_id
            and obj.get('client_type') != ClientType.CONTROLLER
        ]
        for pid, obj in victims:
            pc = obj.get('peer_conn')
            if pc is None:
                continue
            try:
                logger.info(f"Closing orphaned viewer '{pid}' of display '{display_id}' (its graph is gone)")
                await pc.close()
            except Exception as e:
                logger.debug(f"Error closing orphaned viewer '{pid}': {e}")

    async def start_rtc_connection(self, client_peer_id: str, client_type: str, client_token: Optional[str] = None, display_id: str = "primary", client_slot: Optional[int] = None) -> None:
        """Start a peer connection, cleaning up the half-built state on failure.

        A signaling socket that dies mid-handshake (refresh/eviction race) is
        routine churn, not a server fault, and is logged without a traceback.
        """
        try:
            logger.info("Starting RTC pipeline", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
            await self._start_rtc_pipeline(client_peer_id, client_type, client_token, display_id, client_slot)
        except (aiohttp.ClientConnectionResetError, ConnectionResetError) as e:
            logger.info(f"Peer went away during RTC setup: {e}", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
            await self._cleanup_failed_start(client_peer_id, client_type, display_id)
        except Exception as e:
            logger.error(f"Error starting RTC pipeline: {e}", extra={'client_peer_id': client_peer_id, 'client_type': client_type}, exc_info=True)
            await self._cleanup_failed_start(client_peer_id, client_type, display_id)
        else:
            logger.info("RTC pipeline started successfully", extra={'client_peer_id': client_peer_id, 'client_type': client_type})

    async def _cleanup_failed_start(self, client_peer_id: str, client_type: str,
                                    display_id: str = "primary") -> None:
        """Release what a failed pipeline start leaves behind, NOW.

        Waiting for the signaling session-end (which may never come if the
        peer's socket was already gone) leaves live ICE gatherers whose STUN
        retries fire into torn-down transports. A peer that failed before it
        was registered closed its own half-built connection; what outlives it
        is the display it claimed — the graph built for it here and, for a
        secondary display, the registration the owning service made before
        the start — which no later event would release.
        """
        if client_peer_id in self.peer_connections:
            try:
                await self._stop_rtc_pipeline(client_peer_id)
            except Exception as e:
                logger.debug(f"Failed-start cleanup for {client_peer_id}: {e}")
            return
        try:
            await self._release_display_if_unconsumed(
                display_id, ClientType(client_type) is ClientType.CONTROLLER)
        except Exception as e:
            logger.debug(f"Failed-start display release for {client_peer_id}: {e}")

    async def stop_rtc_connection(self, client_peer_id: str, client_type: str) -> None:
        """Stop a specific peer connection by ID."""
        try:
            logger.info("Stopping RTC pipeline", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
            await self._stop_rtc_pipeline(client_peer_id)
        except Exception as e:
            logger.error(f"Error stopping RTC pipeline: {e}", extra={'client_peer_id': client_peer_id, 'client_type': client_type}, exc_info=True)
        else:
            logger.info("RTC pipeline stopped successfully", extra={'client_peer_id': client_peer_id, 'client_type': client_type})

    async def stop_all_rtc_connections(self) -> None:
        """Stop all active peer connections and clean up media resources.

        Raises:
            RTCAppError: When teardown fails.
        """
        try:
            logger.info("Stopping all RTC connections")
            for client_peer_id in list(self.peer_connections.keys()):
                await self._stop_rtc_pipeline(client_peer_id)

            for display_id in list(self.displays.keys()):
                await self._teardown_display_graph(display_id)
            logger.info("All RTC connections stopped, cleaned up media relays and bridges")
        except Exception as e:
            raise RTCAppError(f"Error stopping all RTC connections: {e}") from e
