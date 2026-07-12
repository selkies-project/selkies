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

import logging
import asyncio
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

from .settings import settings as app_settings, inflate_gz_bounded
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
import av
from fractions import Fraction
from typing import List, Any, Dict, Optional, Union
from .webrtc.contrib.media import MediaRelay
from enum import Enum
from .media_pipeline import MediaPipeline
# The viewer/collaborator input-authority lists live with the input protocol
# (input_handler) and are shared verbatim with the websockets gate.
from .input_handler import (
    VIEWER_ALLOWED_PREFIXES,
    VIEWER_COLLAB_EXTRA_PREFIXES,
    VIEWER_SILENT_DROP_PREFIXES,
)

logger = logging.getLogger("rtc")
logger.setLevel(logging.INFO)

class ConditionalExtraFormatter(logging.Formatter):
    def __init__(self, fmt=None, datefmt=None, style='%', extra_fields=None):
        super().__init__(fmt, datefmt, style)
        self.extra_fields = extra_fields or ['client_peer_id', 'client_type']

    def format(self, record):
        result = super().format(record)
        # Add extra fields only if they exist
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


# Raw bytes per data-channel message when no limit was negotiated: the 256 KiB
# standard message size less envelope margin, times 3/4 for the base64 expansion.
# Deliberately NOT the WebSocket chunk size (8 MiB frames), which no browser's
# SCTP stack accepts.
DATA_CHANNEL_FALLBACK_CHUNK_SIZE = ((262144 - 512) * 3) // 4


def get_adjusted_chunk_size(peers: Optional[dict] = None) -> int:
    """Raw-byte chunk size for base64 payloads over the data channel.

    Sized from the smallest max-message-size the connected peers negotiated
    (RFC 8841 a=max-message-size), less an envelope margin, times 3/4 for the
    base64 expansion; a 1 MiB message ceiling bounds per-message buffering. The
    result never exceeds a peer's negotiated limit, so a peer advertising a
    smaller size is honored rather than overrun.
    """
    limit = None
    for peer in (peers or {}).values():
        channel = peer.get("data_channel")
        sctp = getattr(channel, "transport", None)
        nego = getattr(sctp, "remote_max_message_size", 0)
        if nego:
            limit = nego if limit is None else min(limit, nego)
    if not limit:
        return DATA_CHANNEL_FALLBACK_CHUNK_SIZE
    usable = min(limit, 1024 * 1024) - 512
    return (usable * 3) // 4

class ClientType(str, Enum):
    CONTROLLER = "controller"
    VIEWER = "viewer"

class RTCAppError(Exception):
    pass

class PipelineBridge:
    """A bridge to asynchronously pass data between Media and the RTC pipeline.

    maxsize selects the buffering policy: depth 1 is latest-wins (video wants
    the freshest frame), a deeper bound acts as a short drop-oldest FIFO (audio
    wants continuity so a brief consumer stall doesn't silently drop samples).
    """
    def __init__(self, maxsize: int = 1):
        self._queue = asyncio.Queue(maxsize=maxsize)

    def set_data(self, data: Any):
        # Synchronous, no lock: the drop-oldest check and the put have no await, so
        # the single-threaded loop runs them without interleaving (all access is on
        # the loop thread). If the queue is full the consumer is lagging, so drop the
        # oldest queued item to make space for the new one.
        if self._queue.full():
            self._queue.get_nowait()
        self._queue.put_nowait(data)

    async def get_data(self):
        # asynchronously wait until an item is available in the queue
        return await self._queue.get()

class AudioMedia(AudioStreamTrack):
    def __init__(self, data_pipeline: PipelineBridge):
        super().__init__()
        self.data_pipeline = data_pipeline

    async def recv(self):
        # Grab the next audio packet
        packet = await self.data_pipeline.get_data()
        return packet

class VideoMedia(VideoStreamTrack):
    def __init__(self, data_pipeline: PipelineBridge):
        super().__init__()
        self.data_pipeline = data_pipeline

    async def recv(self):
        # Grab the next video packet
        packet = await self.data_pipeline.get_data()
        return packet

class RTCApp:
    def __init__(
        self,
        async_event_loop: asyncio.AbstractEventLoop,
        encoder: str,
        stun_servers: Optional[List[str]] = None,
        turn_servers: Optional[List[str]] = None
    ):
        self.peer_connections: Dict[str, Any] = {}
        self.async_event_loop = async_event_loop
        self.stun_servers = stun_servers
        self.turn_servers = turn_servers
        self.encoder = encoder
        self.last_cursor_sent = None

        # Per-display media graphs: display_id -> {relay, video_bridge, video_media,
        # audio_bridge?, audio_media?}. A display's graph is created by its first
        # controller and torn down with it; only the primary display carries audio.
        self.displays: Dict[str, Dict[str, Any]] = {}
        self.media_pipeline: Optional[MediaPipeline] = None
        # Per-display capture start/stop, overridable by the owning service so a
        # secondary display can drive its own pipeline; the defaults preserve the
        # single-display behavior (the primary pipeline follows its controller).
        self.start_display_media = self._default_start_display_media
        self.stop_display_media = self._default_stop_display_media
        # Per-display encoder resolution for offer building (codec preference and
        # SDP munging): the owning service overrides this when displays can run
        # different encoders; the default is the single global encoder.
        self.get_encoder_for_display = lambda display_id: self.encoder

        # Data channel events. on_data_open receives the channel that opened so
        # per-connection greetings (server settings, current cursor) reach the
        # joining peer, not just the controller.
        self.on_data_open = lambda channel=None: logger.warning('unhandled on_data_open')
        self.on_data_close = lambda: logger.warning('unhandled on_data_close')
        self.on_data_error = lambda e=None: logger.warning('unhandled on_data_error')
        self.on_data_message = lambda msg, display_id='primary': logger.warning('unhandled on_data_message')

        # WebRTC ICE and SDP events
        self.on_ice = lambda ice, client_peer_id: logger.warning('unhandled ice event')
        self.on_sdp = lambda sdp_type, sdp, client_peer_id: logger.warning('unhandled sdp event')

        self.request_idr_frame = lambda display_id='primary': logger.warning('unhandled request_idr_frame')

    async def set_sdp(self, sdp_type: str, sdp: str, client_peer_id: str):
        """Sets remote SDP received by peer"""
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

    async def set_ice(self, ice: Dict, client_peer_id: str):
        """Adds ice candidate received from signaling server"""
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

        # Generate RTCIceCandidate from ice
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

    async def send_clipboard_data(self, data: Union[str, bytes], mime_type: str = "text/plain"):
        """Sends clipboard data over the data channel in chunks"""
        if not data:
            return

        is_text = mime_type == "text/plain"
        data_bytes: bytes = data.encode() if isinstance(data, str) else data
        clipboard_chunk_size = get_adjusted_chunk_size(self.peer_connections)
        if len(data_bytes) <= clipboard_chunk_size:
            b64data = base64.b64encode(data_bytes).decode('utf-8')
            self.__send_data_channel_message(
                "clipboard-msg",
                {
                    "content": b64data,
                    "mime_type": mime_type,
                    "is_binary_data": not is_text,
                    "total_size": len(data_bytes)
                }
            )
        else:
            read = 0
            self.__send_data_channel_message(
                "clipboard-msg-start",
                {
                    "mime_type": mime_type,
                    "is_binary_data": not is_text,
                    "total_size": len(data_bytes),
                }
            )
            while read < len(data_bytes):
                chunk = data_bytes[read:read + clipboard_chunk_size]
                b64_encoded_chunk = base64.b64encode(chunk).decode("utf-8")
                self.__send_data_channel_message(
                    "clipboard-msg-data", {"content": b64_encoded_chunk}
                )
                read += len(chunk)
                await asyncio.sleep(0)
            self.__send_data_channel_message("clipboard-msg-end", {})

        logger.info(f"Sent clipboard data of length {len(data_bytes)} with mime type {mime_type}")

    def send_cursor_data(self, data: Any):
        self.last_cursor_sent = data
        self.__send_data_channel_message(
            "cursor", data)

    def send_gpu_stats(self, load: float, memory_total: int, memory_used: int):
        """Sends GPU stats to the data channel"""

        self.__send_data_channel_message("gpu_stats", {
            "gpu_percent": load * 100,
            "mem_total": memory_total * 1024 * 1024,
            "mem_used": memory_used * 1024 * 1024,
        })

    def send_reload_window(self):
        """Sends reload window command to the data channel"""
        logger.info("sending window reload")
        self.__send_data_channel_message(
            "system", {"action": "reload"})

    def send_framerate(self, framerate: int):
        """Sends the current framerate to the data channel."""
        logger.info("sending framerate")
        self.__send_data_channel_message(
            "system", {"action": "videoFramerate," + str(framerate)})

    def send_video_bitrate(self, bitrate: float):
        """Sends the current video bitrate to the data channel"""
        logger.info("sending video bitrate")
        self.__send_data_channel_message(
            "system", {"action": "video_bitrate," + str(bitrate)})

    def send_audio_bitrate(self, bitrate: int):
        """Sends the current audio bitrate to the data channel"""
        logger.info("sending audio bitrate")
        self.__send_data_channel_message(
            "system", {"action": "audio_bitrate,%d" % bitrate})

    def send_encoder(self, encoder: str):
        """Sends the encoder name to the data channel"""
        logger.info("sending encoder: " + encoder)
        self.__send_data_channel_message(
            "system", {"action": "encoder,%s" % encoder})

    def send_resize_enabled(self, resize_enabled: bool):
        """Sends the current resize enabled state
        """
        logger.info("sending resize enabled state")
        self.__send_data_channel_message(
            "system", {"action": "resize," + str(resize_enabled)})

    def send_remote_resolution(self, res: str):
        """sends the current remote resolution to the client"""
        logger.info("sending remote resolution of: " + res)
        self.__send_data_channel_message(
            "system", {"action": "resolution," + res})

    def send_ping(self, t: float):
        """Sends a ping request to the PRIMARY controller only: latency is measured
        against one shared ping_start, and the websockets transport likewise derives
        its reported latency from the primary client."""
        state, data_channel = self.get_data_channel()
        if not state:
            return
        self.send_message_to_channel(
            data_channel, "ping", {"start_time": float("%.3f" % t)})

    def send_latency_time(self, latency: float):
        """Sends measured latency response time in ms"""
        self.__send_data_channel_message(
            "latency_measurement", {"latency_ms": latency})

    def send_system_stats(self, cpu_percent: float, mem_total: int, mem_used: int):
        """Sends system stats"""
        self.__send_data_channel_message(
            "system_stats", {
                "cpu_percent": cpu_percent,
                "mem_total": mem_total,
                "mem_used": mem_used,
            })

    def get_data_channel(self):
        """Checks to see if the data channel is open"""
        state = False
        peer_obj = self.get_controller_instance()
        if not peer_obj:
            return state, None

        conn_state = peer_obj.get("peer_conn").connectionState
        data_channel_state = peer_obj.get("data_channel").readyState
        return conn_state == "connected" and data_channel_state == "open", peer_obj.get("data_channel")

    def _iter_open_data_channels(self):
        """Every connected peer's open data channel — controllers and viewers,
        all displays."""
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

    def send_message_to_channel(self, channel, msg_type: str, data: Any):
        """Sends one typed message to one specific peer's data channel."""
        msg = {"type": msg_type, "data": data}
        payload = json.dumps(msg)
        try:
            # Large payloads (cursor PNGs, settings, clipboard, stats) compress well;
            # small ones aren't worth the CPU or the risk to input latency. Only
            # channels that completed the _gz handshake may receive gzip.
            if getattr(channel, "_selkies_gz_tx", False) and len(payload) >= 512:
                channel.send(gzip.compress(payload.encode("utf-8"), 6))
            else:
                channel.send(payload)
        except ValueError as e:
            # Oversized for the peer's negotiated max-message-size: dropping one
            # message and logging beats the peer hard-closing the channel.
            logger.error("dropping oversized data channel message '%s': %s", msg_type, e)
        except InvalidStateError:
            # The channel left 'open' between the readiness check and the send
            # (close racing a sender): drop the message like any not-ready channel.
            logger.info("skipping message because data channel closed mid-send: %s" % msg_type)

    def __send_data_channel_message(self, msg_type: str, data: Any):
        """Broadcasts a typed message to every connected peer (all display
        controllers and viewers) — the websockets transport broadcasts cursor,
        stats, and clipboard to all of its clients, so the channel path must too.
        Channels that are not open are skipped.
        """
        if not self.peer_connections:
            return
        sent = False
        for channel in self._iter_open_data_channels():
            self.send_message_to_channel(channel, msg_type, data)
            sent = True
        if not sent:
            logger.info("skipping message because no data channel is ready: %s" % msg_type)

    def send_media_data_over_channel(self, msg_type, data):
        self.__send_data_channel_message(msg_type, data)

    def get_controller_instance(self):
        """Returns the peer connection object for the controller client, if it exists.
        With multiple display controllers connected, the PRIMARY display's controller
        is the authoritative one (latency pings and controller-directed replies)."""
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

    def munge_sdp(self, sdp: str, encoder: Optional[str] = None):
        # Displays can run different encoders; the caller passes the one this
        # offer's display uses (default: the primary/global encoder).
        encoder = encoder or self.encoder
        sdp_text = sdp
        # rtx-time needs to be set to 125 milliseconds for optimal performance
        if 'rtx-time' not in sdp_text:
            logger.warning("injecting rtx-time to SDP")
            sdp_text = re.sub(r'(apt=\d+)', r'\1;rtx-time=125', sdp_text)
        elif 'rtx-time=125' not in sdp_text:
            logger.warning("injecting modified rtx-time to SDP")
            sdp_text = re.sub(r'rtx-time=\d+', r'rtx-time=125', sdp_text)
        # Enable sps-pps-idr-in-keyframe=1 in H.264 and H.265
        if "h264" in encoder or "x264" in encoder or "h265" in encoder or "x265" in encoder:
            if 'sps-pps-idr-in-keyframe' not in sdp_text:
                logger.warning("injecting sps-pps-idr-in-keyframe to SDP")
                sdp_text = sdp_text.replace('packetization-mode=', 'sps-pps-idr-in-keyframe=1;packetization-mode=')
            elif 'sps-pps-idr-in-keyframe=1' not in sdp_text:
                logger.warning("injecting modified sps-pps-idr-in-keyframe to SDP")
                sdp_text = re.sub(r'sps-pps-idr-in-keyframe=\d+', r'sps-pps-idr-in-keyframe=1', sdp_text)
            if ("h264" in encoder or "x264" in encoder) \
                    and "openh264" not in encoder and app_settings.video_fullcolor[0]:
                # Full-colour is a 4:4:4 bitstream: advertise the High 4:4:4 profile so a
                # decoder isn't handed a 4:2:0 baseline profile-level-id that can't match
                # what it receives. 4:2:0 keeps 42e01f (the Firefox negotiation trick).
                # OpenH264 is excluded: it always emits limited-range 4:2:0, and a 4:4:4
                # profile makes decoders misread its color range (visibly darker output).
                sdp_text = re.sub(r'profile-level-id=[0-9A-Fa-f]{6}',
                                  'profile-level-id=f4001f', sdp_text)
        if "opus/" in sdp_text.lower():
            # OPUS_FRAME: Advertise the REAL Opus frame duration as ptime in the offer
            # (pcmflux emits audio_frame_duration_ms frames; the client's answer keys
            # its minptime munge off this value).
            frame_ms = float(getattr(app_settings, 'audio_frame_duration_ms', '10') or 10)
            # Advertise ptime in whole milliseconds (a 2.5 ms Opus frame rounds to 3) for
            # browser SDP parsers; the client derives minptime from it and pcmflux keeps
            # the real 2.5 ms frame.
            ptime = int(frame_ms + 0.5)
            sdp_text = re.sub(r'([^-]sprop-[^\r\n]+)', r'\1\r\na=ptime:' + str(ptime), sdp_text)

        # Raise the SDP bandwidth ceiling so the browser's REMB doesn't throttle a
        # high-bitrate desktop stream (b=AS is a cap hint, not a target; generous is
        # safe). x-google-max-bitrate mirrors it on the Chrome receive side. Both are
        # scoped to the m=video section so they survive future codec/aiortc changes.
        sdp_text = self._munge_video_bandwidth(sdp_text)

        return sdp_text

    def _munge_video_bandwidth(self, sdp_text: str) -> str:
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

            # Gather this video m-section (up to the next m= line or EOF).
            section = [line]
            i += 1
            while i < n and not lines[i].startswith("m="):
                section.append(lines[i])
                i += 1

            # Per-section b=AS: only insert when absent within THIS video block
            # (the previous global guard skipped insertion if any b=AS existed
            # anywhere in the offer). RFC4566 allows the media section to omit
            # its own c= (inheriting the session-level c=); fall back to right
            # after the m=video line when no per-media c= is present.
            if not any(s.startswith("b=AS:") for s in section):
                c_idx = next(
                    (idx for idx, s in enumerate(section) if s.startswith("c=")),
                    None,
                )
                insert_at = (c_idx + 1) if c_idx is not None else 1
                section.insert(insert_at, "b=AS:300000")

            # Inject the x-google ceiling per video codec fmtp, keyed on the
            # video rtpmap payload types (not just packetization-mode), so
            # VP8/VP9 get it too. RTX is excluded.
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
                    # No fmtp for this codec (e.g. VP8/VP9 with no parameters):
                    # add one carrying just the x-google hints.
                    rtpmap_idx = next(
                        (idx for idx, s in enumerate(section)
                         if s.startswith("a=rtpmap:{} ".format(pt))),
                        None,
                    )
                    if rtpmap_idx is not None:
                        section.insert(rtpmap_idx + 1, "a=fmtp:{} {}".format(pt, XGOOGLE))

            out.extend(section)

        return "\r\n".join(out)

    def consume_data(self, buf, pts, kind, display_id: str = "primary"):
        # Synchronous: scheduled via loop.call_soon_threadsafe from the capture
        # thread (no per-frame Future/Task), since set_data no longer awaits.
        graph = self.displays.get(display_id or "primary")
        if graph is None:
            return
        if kind == "video":
            if buf:
                try:
                    # av.Packet accepts a buffer-protocol object zero-copy and
                    # keeps it as the owning ref, avoiding a per-frame copy.
                    packet = av.Packet(buf)
                    RTP_VIDEO_CLOCK_RATE = 90000
                    packet.time_base = Fraction(1, RTP_VIDEO_CLOCK_RATE)
                    if pts is not None:
                        packet.pts = pts
                        packet.dts = packet.pts
                    bridge = graph.get("video_bridge")
                    if bridge is not None:
                        bridge.set_data(packet)
                except Exception as e:
                    logger.error(f"error processing video sample: {e}")
        elif kind == "audio":
            if buf:
                try:
                    # Zero-copy: av.Packet takes the buffer directly as owner.
                    packet = av.Packet(buf)
                    packet.time_base = Fraction(1, 48000)
                    if pts is not None:
                        packet.pts = pts
                    bridge = graph.get("audio_bridge")
                    if bridge is not None:
                        bridge.set_data(packet)
                except Exception as e:
                    logger.error(f"error processing audio sample: {e}")

    def update_rtc_config(self, stun_servers: List[str], turn_servers: List[str]):
        """Updates the STUN/TURN servers used for every NEW peer connection.

        get_rtc_config() reads these at peer-creation time, so a refresh (typically
        rotated TURN REST credentials) takes effect for every subsequent connection.
        Live sessions deliberately keep their established ICE: their TURN allocations
        stay valid, and forcing an ICE restart on refresh would drop working streams.
        """
        changed = (stun_servers, turn_servers) != (self.stun_servers, self.turn_servers)
        self.stun_servers = stun_servers
        self.turn_servers = turn_servers
        if changed:
            logger.info(
                "RTC ICE servers updated; applies to new connections "
                "(established sessions keep their current ICE)."
            )

    def format_turn_servers(self, turn_servers: List[str]):
        """
        Restructure each TURN server string to the expected format
        and return a list of formatted TURN server URLs.
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
        """Restructure each STUN server string to expected format"""
        formatted_servers = []
        for stun in stun_servers:
            server = stun.split("//")
            formatted_servers.append("".join(server))
        return formatted_servers

    def get_rtc_config(self):
        # Format TURN servers
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
        config = RTCConfiguration(iceServers=ice_servers, bundlePolicy=RTCBundlePolicy.MAX_BUNDLE)
        return config

    def force_codec(self, pc: RTCPeerConnection, sender: RTCRtpSender, forced_codec_mime: str):
        """
        Forces a codec by MIME type and its associated RTX codec
        """
        kind = sender.track.kind
        capabilities = RTCRtpSender.getCapabilities(kind)
        logger.debug(f"Current capabilities for {kind}: {capabilities}")

        # Collect all codecs matching the given MIME type (e.g., all H264 codecs which may include different profiles)
        chosen_codec = []
        for codec in capabilities.codecs:
            if codec.mimeType == forced_codec_mime:
                chosen_codec.append(codec)

        if not chosen_codec:
            raise ValueError(f"Codec {forced_codec_mime} not found in capabilities")

        # Find the RTX codec associated with the chosen codec's payload type
        rtx_codec = None
        for codec in capabilities.codecs:
            if codec.mimeType.lower() == f"{kind}/rtx":
                rtx_codec = codec
                break

        if not rtx_codec:
            raise ValueError(f"RTX codec for {forced_codec_mime} not found")

        # FlexFEC rides along when the receiver supports it (Chrome family); a
        # receiver without it simply answers without the codec and the sender
        # emits no repair stream.
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

    async def _drain_channel_queue(self, queue: asyncio.Queue, handler, label: str):
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

    def _serialize_channel(self, channel: RTCDataChannel, handler, max_queue: int = 512):
        """Wire a channel's messages through a bounded per-channel queue drained
        by a single consumer task, so dispatch stays in arrival order.

        The message handler only enqueues (drop+log on overflow); the consumer
        is cancelled when the channel closes. handler is called late-bound so
        reassigning the target callback still takes effect.
        """
        queue: asyncio.Queue = asyncio.Queue(maxsize=max_queue)

        def _enqueue(msg):
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

    def _viewer_is_collaborator(self, client_token):
        """A viewer holding the active mk (mouse+keyboard) token is a read-write
        collaborator — mirrors the WS mk-token path — but only while enable_collab
        is on. Fail-safe: any missing piece => not a collaborator (stays read-only)."""
        if not client_token:
            return False
        if not bool(app_settings.enable_collab[0]):
            return False
        try:
            # active_mk_token is a runtime global owned by the WS control plane
            # (set via the secure-mode /api/tokens endpoint, registered in all
            # modes); read it dynamically so a re-provision is honored per message.
            from . import selkies as _sk
            return _sk.active_mk_token is not None and client_token == _sk.active_mk_token
        except Exception:
            return False

    def _on_input_channel_message(self, msg, channel=None, client_type=None, client_token=None, display_id="primary"):
        """Decompress gzip'd payloads and intercept the compression handshake before
        the input dispatcher (the late-bound on_data_message) sees the message."""
        if isinstance(msg, (bytes, bytearray)) and bytes(msg[:2]) == b"\x1f\x8b":
            try:
                # Bounded inflate, mirroring the WebSocket 0x05 path: the channel's
                # negotiated max-message-size caps the compressed size only.
                msg = inflate_gz_bounded(msg)
            except Exception:
                logger.warning("Dropping undecodable compressed data channel message")
                return
        if msg == "_gz,1":
            # The peer can gunzip: echo the capability so it compresses its own large
            # sends, and mark THIS channel so outbound payloads to it may be gzipped.
            # Compression is negotiated per channel — every display page and viewer
            # handshakes (or not) independently.
            if channel is not None:
                channel._selkies_gz_tx = True
                try:
                    channel.send("_gz,1")
                except Exception as e:
                    logger.warning("Failed to ack compression handshake: %s", e)
            return
        if client_type == ClientType.VIEWER and isinstance(msg, str) and msg.startswith("SETTINGS,"):
            # A viewer's settings snapshot is connection sync only, never applied —
            # the websockets transport likewise ignores viewer payloads server-side.
            logger.debug("Ignoring SETTINGS payload from a viewer (display '%s')", display_id)
            return
        if client_type == ClientType.VIEWER and isinstance(msg, str):
            # A viewer may only send the allow-listed messages; an authenticated
            # read-write collaborator (mk token + enable_collab) additionally gets
            # the keyboard/mouse/clipboard set — the same two tiers as the WS gate
            # (a collaborator still can't send cmd or other controller-only
            # messages). The collaborator check is only reached for otherwise-
            # disallowed input, so normal viewer traffic pays nothing.
            if not msg.startswith(VIEWER_ALLOWED_PREFIXES) and not (
                msg.startswith(VIEWER_COLLAB_EXTRA_PREFIXES)
                and self._viewer_is_collaborator(client_token)
            ):
                # Blur/visibility lifecycle noise is dropped silently (WS parity).
                if not msg.startswith(VIEWER_SILENT_DROP_PREFIXES):
                    logger.warning("Dropping unauthorized viewer input: %s", msg[:32])
                return
        return self.on_data_message(msg, display_id or "primary")

    async def on_peer_connection_established(self, client_peer_id: str, client_type: ClientType, display_id: str = "primary"):
        if client_type == ClientType.CONTROLLER:
            await self.start_display_media(display_id)
            logger.info(f"Media pipeline start requested for {client_peer_id} (display '{display_id}')")

    async def on_peer_connection_lost(self, client_peer_id: str, client_type: ClientType, display_id: str = "primary"):
        """Called when peer connection is lost or closed."""
        if client_type == ClientType.CONTROLLER:
            await self.stop_display_media(display_id)
            logger.info(f"Media pipeline stop requested for {client_peer_id} (display '{display_id}')")

    async def _default_start_display_media(self, display_id: str):
        if display_id == "primary" and self.media_pipeline:
            await self.media_pipeline.start_media_pipeline()

    async def _default_stop_display_media(self, display_id: str):
        if display_id == "primary" and self.media_pipeline:
            await self.media_pipeline.stop_media_pipeline()

    async def on_connectionstatechange(self, client_peer_id: str):
        """Handle connection state changes for a peer connection.
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
            await self.on_peer_connection_lost(client_peer_id, client_type, display_id)
            # Pop by identity, not key: a reconnect may have re-registered the same
            # client_peer_id during the await above, and we must not orphan it.
            removed = None
            if self.peer_connections.get(client_peer_id) is peer_obj:
                removed = self.peer_connections.pop(client_peer_id, None)
            # This peer is done either way; its never-established channels emit
            # no 'close', so stop their consumers here.
            await self._cancel_channel_consumers(peer_obj)
            # Per-peer mic teardown: only THIS peer's playback stops; the other
            # primary peers (controller + co-op viewers) keep their mics.
            await self._stop_mic_playback_state(peer_obj.get("mic_state"))
            if removed is not None and removed.get('client_type') == ClientType.CONTROLLER:
                await self._teardown_display_graph(display_id)
            logger.info("Peer connection closed", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        elif state == "connecting":
            logger.info("Peer connection is connecting", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        else:
            logger.debug(f"Unhandled peer connection state: {state}", extra={'client_peer_id': client_peer_id, 'client_type': client_type})

    def on_pli(self, client_peer_id: str, client_type: str):
        logger.debug("PLI occurred, triggering IDR frame request", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        peer_obj = self.peer_connections.get(client_peer_id) or {}
        display_id = peer_obj.get("display_id") or "primary"
        asyncio.run_coroutine_threadsafe(self.request_idr_frame(display_id), self.async_event_loop)

    async def _start_rtc_pipeline(
        self,
        client_peer_id: str,
        c_type: str,
        client_token: Optional[str] = None,
        display_id: str = "primary",
    ):
        """Starts the WebRTC pipeline and creates the peer connection."""
        # Normalize client_type to ClientType enum
        client_type = ClientType(c_type)
        display_id = display_id or "primary"

        # A display's media graph is created by its controller; audio (and the
        # mic return path) only exist on the primary display — a secondary
        # display page renders video and carries input, matching the WS model.
        if client_type is ClientType.CONTROLLER:
            graph: Dict[str, Any] = {"relay": MediaRelay()}
            graph["video_bridge"] = PipelineBridge()
            graph["video_media"] = VideoMedia(graph["video_bridge"])
            if display_id == "primary":
                # Audio uses a small drop-oldest FIFO so a brief sender stall keeps
                # continuity instead of dropping a packet on every overtake.
                graph["audio_bridge"] = PipelineBridge(maxsize=8)
                graph["audio_media"] = AudioMedia(graph["audio_bridge"])
            self.displays[display_id] = graph
            logger.info(f"Media relay and pipeline bridges created for controller of display '{display_id}'")

        peer_connection =  RTCPeerConnection(self.get_rtc_config())

        graph = self.displays.get(display_id)
        if graph is None:
            raise RTCAppError(
                f"Cannot create peer connection: no media graph for display '{display_id}'. Controller may be disconnected."
            )
        media_relay = graph["relay"]

        # add audio and video encoded streams
        rtp_video_sender = peer_connection.addTrack(media_relay.subscribe(graph["video_media"]))
        rtp_video_sender.on("pli", lambda cid=client_peer_id, ct=client_type: self.on_pli(cid, ct))
        if graph.get("audio_media") is not None:
            peer_connection.addTrack(media_relay.subscribe(graph["audio_media"]))

        # Microphone: one recvonly audio transceiver inside the SAME bundled SDP (no second
        # negotiation) so the browser can send its mic on demand. The m-line sits inactive
        # until the client attaches a mic track, so it is negotiated whenever audio is on:
        # microphone_enabled only picks the client-side default (off), and a runtime enable
        # must not require a renegotiation the stack doesn't do. A LOCKED-off microphone
        # setting still withholds the m-line entirely. Only the primary display carries audio.
        mic_on, mic_locked = app_settings.microphone_enabled
        mic_state = None
        if display_id == "primary" and bool(app_settings.audio_enabled[0]) and (mic_on or not mic_locked):
            mic_state = self._setup_mic_receiver(peer_connection)

        # Primary data channel, fully reliable + ordered: input, clipboard, and
        # upload control all ride it, and none of them tolerate loss.
        # (Compression is negotiated per channel via the _gz handshake.)
        data_channel = peer_connection.createDataChannel("input", ordered=True)

        # Assign event handlers for the input data channel. Messages are
        # serialized through a single per-channel consumer so input events
        # (e.g. key down/up) are dispatched strictly in arrival order.
        # close/error are late-bound (setup_callbacks reassigns the handlers).
        # open passes the channel so the greeting goes to the peer that joined.
        data_channel.on("open", lambda ch=data_channel: self.on_data_open(ch))
        data_channel.on("close", lambda: self.on_data_close())
        data_channel.on("error", lambda e=None: self.on_data_error(e))
        input_consumer = self._serialize_channel(
            data_channel,
            lambda msg, ch=data_channel, ct=client_type, tok=client_token, did=display_id: self._on_input_channel_message(msg, ch, ct, tok, did),
        )

        peer_connection.on("connectionstatechange", lambda cid=client_peer_id: asyncio.run_coroutine_threadsafe(self.on_connectionstatechange(cid), loop=self.async_event_loop))

        try:
            try:
                display_encoder = self.get_encoder_for_display(display_id) or self.encoder
            except Exception:
                display_encoder = self.encoder
            preferred_codec = self.get_mime_by_encoder(display_encoder)
            if preferred_codec is None:
                raise RTCAppError(f"Encoder {display_encoder} is not supported")
            self.force_codec(peer_connection, rtp_video_sender, preferred_codec)

            await peer_connection.setLocalDescription(await peer_connection.createOffer())
            offer = peer_connection.localDescription

            sdp = offer.sdp
            sdp = self.munge_sdp(sdp, display_encoder)
            await self.on_sdp('offer', sdp, client_peer_id)
        except BaseException:
            # Failure before registration: no teardown path could ever reach this
            # consumer or this connection (_stop_rtc_pipeline only finds registered
            # peers), so both must be torn down here — otherwise the fully-built
            # RTCPeerConnection (ICE gatherers, channel, tracks, mic receiver) is
            # orphaned alive on every offer/SDP-send failure.
            input_consumer.cancel()
            try:
                await peer_connection.close()
            except Exception:
                logger.warning("Failed to close peer connection after failed start", exc_info=True)
            raise

        self.peer_connections[client_peer_id] = {
            "peer_conn": peer_connection,
            "data_channel": data_channel,
            "client_type": client_type,
            "display_id": display_id,
            # A channel that never reaches SCTP-established never emits 'close',
            # so its consumer must also be cancellable from teardown paths.
            "channel_consumers": [input_consumer],
            # This peer's OWN mic playback state (None when no mic m-line): mic
            # teardown is per-peer, so one peer closing never silences the others.
            "mic_state": mic_state,
        }

    def _setup_mic_receiver(self, peer_connection):
        """Add a recvonly mic transceiver in the bundled session and route its encoded
        Opus straight into pcmflux -- no aiortc/Python Opus decode. RED (UDP loss
        resilience) is gated by audio_redundancy: when on, the shared caps offer it and
        pcmflux de-frames + loss-recovers each RED payload off the GIL before decoding;
        when off, the m-line is restricted to plain Opus."""
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
        state = {"pb": None, "starting": False, "closed": False}

        def sink(codec, frame):
            if state["closed"]:
                return
            data = bytes(getattr(frame, "data", b"") or b"")
            if not data:
                return
            pb = state["pb"]
            if pb is None:
                # First packet: open the pcmflux playback off the loop, dropping until ready.
                if not state["starting"]:
                    state["starting"] = True

                    async def _start():
                        try:
                            if pcmflux is None:
                                raise RuntimeError("pcmflux is not installed")
                            pb2 = pcmflux.AudioPlayback()
                            ps = pcmflux.AudioPlaybackSettings()
                            ps.device_name = b"input"
                            ps.sample_rate = 24000
                            ps.channels = 1
                            ps.latency_ms = 40
                            await asyncio.to_thread(pb2.start, ps)
                            if state["closed"]:
                                # Peer torn down while the start was in flight:
                                # discard rather than publish into a dead state.
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
                    # RED (audio_redundancy on): pcmflux de-frames + loss-recovers + decodes,
                    # all off the GIL. The RTP timestamp anchors the redundant blocks' offsets.
                    pb.write_red(data, int(getattr(frame, "timestamp", 0) or 0))
                else:
                    # Plain Opus (RED off): decode directly -- no de-framing, dedup, or alloc.
                    pb.write(data)
            except Exception:
                pass

        mic_tx.receiver._encoded_audio_sink = sink
        return state

    async def _stop_mic_playback_state(self, state):
        """Stop ONE peer's mic playback (per-peer ownership: a closing peer must
        never silence the mic of the other primary peers). Marks the state closed
        so an in-flight first-packet start cannot publish a live playback into a
        torn-down peer (which nothing would ever stop)."""
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
        """Returns respective mime type by encoder name"""

        # Every pipeline encoder emits H.264. Offering another MIME here would
        # negotiate a codec the stream cannot honor, so new entries may only be
        # added together with a real pixelflux encoder; the vendored webrtc stack
        # retains its VP8 RTP support for that future.
        encoder_mime_map = {
            "h264enc"     : "video/H264",
            "openh264enc" : "video/H264",
        }
        mime = encoder_mime_map.get(encoder)
        if mime is None:
            # An unmapped encoder (e.g. a stale persisted client setting) must never
            # take the transport down; fall back to the always-vendored H.264 path
            # and flag it.
            logger.error(
                f"No MIME mapping for encoder {encoder}; falling back to video/H264"
            )
            mime = "video/H264"
        return mime

    async def _cancel_channel_consumers(self, peer_obj: Dict[str, Any]):
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

    async def _stop_rtc_pipeline(self, client_peer_id: str):
        """Stops the WebRTC pipeline and closes the peer connection."""
        try:
            if not self.peer_connections:
                return

            peer_obj = self.peer_connections.get(client_peer_id, None)
            if not peer_obj:
                logger.warning(f"Peer object not found for client peer_id: {client_peer_id}")
                return

            peer_conn = peer_obj.get("peer_conn")
            if peer_conn is not None:
                await peer_conn.close()
            await self._cancel_channel_consumers(peer_obj)
            # Explicit stop deletes the registration before the 'closed' state
            # event can see it, so this peer's mic must be stopped here too.
            await self._stop_mic_playback_state(peer_obj.get("mic_state"))
            try:
                del self.peer_connections[client_peer_id]
            except KeyError:
                pass

            if peer_obj.get('client_type') == ClientType.CONTROLLER:
                display_id = peer_obj.get('display_id') or 'primary'
                logger.info(f"Controller peer disconnected, cleaning up media graph of display '{display_id}'")
                await self._teardown_display_graph(display_id)
        except Exception as e:
            raise RTCAppError(f"Error stopping pipeline: {e}")

    async def _teardown_display_graph(self, display_id: str):
        """Drop one display's media graph, reaping its relay workers.

        The relay's __run_track workers only exit when the SOURCE track errors,
        so dropping the reference alone leaks them pending in recv() ("Task was
        destroyed but it is pending!")."""
        graph = self.displays.pop(display_id or 'primary', None)
        if not graph:
            return
        relay = graph.get('relay')
        if relay is not None:
            try:
                await relay.stop()
            except Exception as e_relay:
                logger.warning(f"Media relay teardown error (continuing): {e_relay}")

    async def start_rtc_connection(self, client_peer_id: str, client_type: str, client_token: Optional[str] = None, display_id: str = "primary"):
        try:
            logger.info("Starting RTC pipeline", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
            await self._start_rtc_pipeline(client_peer_id, client_type, client_token, display_id)
        except (aiohttp.ClientConnectionResetError, ConnectionResetError) as e:
            # The peer's signaling socket died mid-handshake (refresh/eviction race):
            # routine churn, not a server fault — one line, no traceback.
            logger.info(f"Peer went away during RTC setup: {e}", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
            await self._cleanup_failed_start(client_peer_id)
        except Exception as e:
            logger.error(f"Error starting RTC pipeline: {e}", extra={'client_peer_id': client_peer_id, 'client_type': client_type}, exc_info=True)
            await self._cleanup_failed_start(client_peer_id)
        else:
            logger.info("RTC pipeline started successfully", extra={'client_peer_id': client_peer_id, 'client_type': client_type})

    async def _cleanup_failed_start(self, client_peer_id: str):
        """Close the half-built peer connection of a failed pipeline start NOW.
        Waiting for the signaling session-end (which may never come if the peer's
        socket was already gone) leaves live ICE gatherers whose STUN retries
        fire into torn-down transports."""
        try:
            await self._stop_rtc_pipeline(client_peer_id)
        except Exception as e:
            logger.debug(f"Failed-start cleanup for {client_peer_id}: {e}")

    async def stop_rtc_connection(self, client_peer_id: str, client_type: str):
        """Stop a specific peer connection by ID."""
        try:
            logger.info("Stopping RTC pipeline", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
            await self._stop_rtc_pipeline(client_peer_id)
        except Exception as e:
            logger.error(f"Error stopping RTC pipeline: {e}", extra={'client_peer_id': client_peer_id, 'client_type': client_type}, exc_info=True)
        else:
            logger.info("RTC pipeline stopped successfully", extra={'client_peer_id': client_peer_id, 'client_type': client_type})

    async def stop_all_rtc_connections(self):
        """Stop all active peer connections and cleanup media resources."""
        try:
            logger.info("Stopping all RTC connections")
            for client_peer_id in list(self.peer_connections.keys()):
                await self._stop_rtc_pipeline(client_peer_id)

            for display_id in list(self.displays.keys()):
                await self._teardown_display_graph(display_id)
            logger.info("All RTC connections stopped, cleaned up media relays and bridges")
        except Exception as e:
            raise RTCAppError(f"Error stopping all RTC connections: {e}")
