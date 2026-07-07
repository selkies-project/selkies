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

try:
    import pcmflux
except (ImportError, RuntimeError):
    pcmflux = None

from .settings import settings as app_settings
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
import av
from fractions import Fraction
from typing import List, Any, Dict, Optional, Union
from .webrtc.contrib.media import MediaRelay
from enum import Enum
from .media_pipeline import MediaPipeline

# leave some room for metadata in the data channel message
CLIPBOARD_CHUNK_SIZE = 65535 - 150

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


# Raw bytes per data-channel message when no limit was negotiated: a 64 KiB
# message (RFC 8841's conservative default) less envelope margin, times 3/4
# for the base64 expansion. Deliberately NOT derived from CLIPBOARD_CHUNK_SIZE:
# that constant tracks the 8 MiB WebSocket frame ceiling and would produce
# multi-megabyte messages no browser's SCTP stack accepts.
DATA_CHANNEL_FALLBACK_CHUNK_SIZE = ((65536 - 512) * 3) // 4


def get_adjusted_chunk_size(peers: Optional[dict] = None) -> int:
    """Raw-byte chunk size for base64 payloads over the data channel.

    Sized from the smallest max-message-size the connected peers negotiated
    (RFC 8841 a=max-message-size), less an envelope margin, times 3/4 for the
    base64 expansion; a 1 MiB message ceiling bounds per-message buffering.
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
    return max(DATA_CHANNEL_FALLBACK_CHUNK_SIZE, (usable * 3) // 4)

class ClientType(str, Enum):
    CONTROLLER = "controller"
    VIEWER = "viewer"

# Server-side input authority: a viewer peer (shared/#player co-op) may only send
# these; anything else (keyboard, mouse, clipboard, cmd) is dropped. Mirrors the WS
# viewer gate so a modified client can't inject input a controller didn't grant.
VIEWER_ALLOWED_PREFIXES = ("SETTINGS,", "START_VIDEO", "REQUEST_KEYFRAME", "js,")

class RTCAppError(Exception):
    pass

class PipelineBridge:
    """A bridge to asynchronously pass data between Media and the RTC pipeline.

    maxsize selects the buffering policy: depth 1 is latest-wins (video wants
    the freshest frame), a deeper bound acts as a short drop-oldest FIFO (audio
    wants continuity so a brief consumer stall doesn't silently drop samples).
    """
    def __init__(self, maxsize: int = 1):
        self._lock = asyncio.Lock()
        self._queue = asyncio.Queue(maxsize=maxsize)

    async def set_data(self, data: Any):
        # If the queue is already full, the consumer is lagging so drop the
        # oldest queued item to make space for the new one.
        async with self._lock:
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
        self.aux_data_channel = None
        self.async_event_loop = async_event_loop
        self.stun_servers = stun_servers
        self.turn_servers = turn_servers
        self.encoder = encoder
        self.last_cursor_sent = None

        self.audio_pipeline_bridge = None
        self.video_pipeline_bridge = None
        self.media_relay = None
        self.media_pipeline: Optional[MediaPipeline] = None
        # Active WebRTC mic decoders (pcmflux AudioPlayback), stopped on teardown.
        self._mic_states = []

        # Data channel events
        self.on_data_open = lambda: logger.warning('unhandled on_data_open')
        self.on_data_close = lambda: logger.warning('unhandled on_data_close')
        self.on_data_error = lambda: logger.warning('unhandled on_data_error')
        self.on_data_message = lambda msg: logger.warning('unhandled on_data_message')
        # Peer advertised gzip support on the input channel (re-negotiated per peer).
        self._gz_tx = False
        self.on_data_msg_bytes = lambda data: logger.warning('unhandled on_data_msg_bytes')

        # WebRTC ICE and SDP events
        self.on_ice = lambda ice, client_peer_id: logger.warning('unhandled ice event')
        self.on_sdp = lambda sdp_type, sdp, client_peer_id: logger.warning('unhandled sdp event')

        self.request_idr_frame = lambda: logger.warning('unhandled request_idr_frame')

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
        """Sends a ping request over the data channel to measure latency"""
        self.__send_data_channel_message(
            "ping", {"start_time": float("%.3f" % t)})

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

    def __send_data_channel_message(self, msg_type: str, data: Any):
        """Sends message to the peer through the data channel.
        Message is dropped if the channel is not open.
        """
        if not self.peer_connections:
            return

        state, data_channel = self.get_data_channel()
        if not state:
            logger.info("skipping message because data channel is not ready: %s" % msg_type)
            return

        msg = {"type": msg_type, "data": data}
        payload = json.dumps(msg)
        try:
            # Large payloads (cursor PNGs, settings, clipboard, stats) compress well;
            # small ones aren't worth the CPU or the risk to input latency.
            if self._gz_tx and len(payload) >= 512:
                data_channel.send(gzip.compress(payload.encode("utf-8"), 6))
            else:
                data_channel.send(payload)
        except ValueError as e:
            # Oversized for the peer's negotiated max-message-size: dropping one
            # message and logging beats the peer hard-closing the channel.
            logger.error("dropping oversized data channel message '%s': %s", msg_type, e)

    def send_media_data_over_channel(self, msg_type, data):
        self.__send_data_channel_message(msg_type, data)

    def get_controller_instance(self):
        """Returns the peer connection object for the controller client, if it exists."""
        return next((obj for obj in self.peer_connections.values() if obj.get("client_type") == ClientType.CONTROLLER), None)

    def munge_sdp(self, sdp: str):
        sdp_text = sdp
        # rtx-time needs to be set to 125 milliseconds for optimal performance
        if 'rtx-time' not in sdp_text:
            logger.warning("injecting rtx-time to SDP")
            sdp_text = re.sub(r'(apt=\d+)', r'\1;rtx-time=125', sdp_text)
        elif 'rtx-time=125' not in sdp_text:
            logger.warning("injecting modified rtx-time to SDP")
            sdp_text = re.sub(r'rtx-time=\d+', r'rtx-time=125', sdp_text)
        # Enable sps-pps-idr-in-keyframe=1 in H.264 and H.265
        if "h264" in self.encoder or "x264" in self.encoder or "h265" in self.encoder or "x265" in self.encoder:
            if 'sps-pps-idr-in-keyframe' not in sdp_text:
                logger.warning("injecting sps-pps-idr-in-keyframe to SDP")
                sdp_text = sdp_text.replace('packetization-mode=', 'sps-pps-idr-in-keyframe=1;packetization-mode=')
            elif 'sps-pps-idr-in-keyframe=1' not in sdp_text:
                logger.warning("injecting modified sps-pps-idr-in-keyframe to SDP")
                sdp_text = re.sub(r'sps-pps-idr-in-keyframe=\d+', r'sps-pps-idr-in-keyframe=1', sdp_text)
            if ("h264" in self.encoder or "x264" in self.encoder) and app_settings.video_fullcolor[0]:
                # Full-colour is a 4:4:4 bitstream: advertise the High 4:4:4 profile so a
                # decoder isn't handed a 4:2:0 baseline profile-level-id that can't match
                # what it receives. 4:2:0 keeps 42e01f (the Firefox negotiation trick).
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

    async def consume_data(self, buf, pts, kind):
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
                    if self.video_pipeline_bridge is not None:
                        await self.video_pipeline_bridge.set_data(packet)
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
                    if self.audio_pipeline_bridge is not None:
                        await self.audio_pipeline_bridge.set_data(packet)
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

    def _on_input_channel_message(self, msg, channel=None, client_type=None):
        """Decompress gzip'd payloads and intercept the compression handshake before
        the input dispatcher (the late-bound on_data_message) sees the message."""
        if isinstance(msg, (bytes, bytearray)) and bytes(msg[:2]) == b"\x1f\x8b":
            try:
                msg = gzip.decompress(msg).decode("utf-8")
            except Exception:
                logger.warning("Dropping undecodable compressed data channel message")
                return
        if msg == "_gz,1":
            # The peer can gunzip: echo the capability so it compresses its own large
            # sends, and compress outbound payloads when this is the controller's
            # channel (the only one __send_data_channel_message targets).
            if channel is not None:
                _, ctrl_channel = self.get_data_channel()
                if channel is ctrl_channel:
                    self._gz_tx = True
                try:
                    channel.send("_gz,1")
                except Exception as e:
                    logger.warning("Failed to ack compression handshake: %s", e)
            return
        if client_type == ClientType.VIEWER and isinstance(msg, str):
            if not msg.startswith(VIEWER_ALLOWED_PREFIXES):
                logger.warning("Dropping unauthorized viewer input: %s", msg[:32])
                return
        return self.on_data_message(msg)

    def on_datachannel(self, channel: RTCDataChannel, client_peer_id: Optional[str] = None):
        """Handles incoming auxiliary data channel.

        Arguments:
            channel        -- the RTCDataChannel object provided by the event
            client_peer_id -- optional id of the client peer associated with this channel
        """
        logger.info(f"Auxiliary data channel opened: {channel.label}", extra={'client_peer_id': client_peer_id})
        self.aux_data_channel = channel
        self.aux_data_channel.on("close", lambda: logger.info("Auxiliary data channel closed"))
        self.aux_data_channel.on("error", lambda e: logger.error("Auxiliary data channel error: %s", e))
        consumer = self._serialize_channel(self.aux_data_channel, lambda data: self.on_data_msg_bytes(data))
        # Track per-peer so connection teardown can stop the consumer even if
        # the channel never emits 'close'.
        peer_obj = self.peer_connections.get(client_peer_id) if client_peer_id else None
        if peer_obj is not None:
            peer_obj.setdefault("channel_consumers", []).append(consumer)

    async def on_peer_connection_established(self, client_peer_id: str, client_type: ClientType):
        if client_type == ClientType.CONTROLLER:
            if self.media_pipeline:
                await self.media_pipeline.start_media_pipeline()
                logger.info(f"Media pipeline started for {client_peer_id}")

    async def on_peer_connection_lost(self, client_peer_id: str, client_type: ClientType):
        """Called when peer connection is lost or closed."""
        if client_type == ClientType.CONTROLLER:
            if self.media_pipeline:
                await self.media_pipeline.stop_media_pipeline()
                logger.info(f"Media pipeline stopped for {client_peer_id}")

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
        if state == "failed":
            await peer_conn.close()
        elif state == "disconnected":
            logger.warning("Peer connection disconnected", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        elif state == "connected":
            await self.on_peer_connection_established(client_peer_id, client_type)
            logger.info("Peer connection established", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        elif state == "closed":
            await self.on_peer_connection_lost(client_peer_id, client_type)
            # Pop by identity, not key: a reconnect may have re-registered the same
            # client_peer_id during the await above, and we must not orphan it.
            removed = None
            if self.peer_connections.get(client_peer_id) is peer_obj:
                removed = self.peer_connections.pop(client_peer_id, None)
            # This peer is done either way; its never-established channels emit
            # no 'close', so stop their consumers here.
            await self._cancel_channel_consumers(peer_obj)
            await self._stop_mic_playbacks()
            if removed is not None and removed.get('client_type') == ClientType.CONTROLLER:
                self.media_relay = None
                self.aux_data_channel = None
                self.video_pipeline_bridge = None
                self.audio_pipeline_bridge = None
            logger.info("Peer connection closed", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        elif state == "connecting":
            logger.info("Peer connection is connecting", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        else:
            logger.debug(f"Unhandled peer connection state: {state}", extra={'client_peer_id': client_peer_id, 'client_type': client_type})

    def on_pli(self, client_peer_id: str, client_type: str):
        logger.info("PLI occurred, triggering IDR frame request", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
        asyncio.run_coroutine_threadsafe(self.request_idr_frame(), self.async_event_loop)

    async def _start_rtc_pipeline(self, client_peer_id: str, c_type: str):
        """Starts the WebRTC pipeline and creates the peer connection."""
        # Normalize client_type to ClientType enum
        client_type = ClientType(c_type)

        # Create media relay if client is of Controller type
        if client_type is ClientType.CONTROLLER:
            self.media_relay = MediaRelay()

            # create data bridge instances for video and audio
            self.video_pipeline_bridge = PipelineBridge()
            self.video_media = VideoMedia(self.video_pipeline_bridge)

            # Audio uses a small drop-oldest FIFO so a brief sender stall keeps
            # continuity instead of dropping a packet on every overtake.
            self.audio_pipeline_bridge = PipelineBridge(maxsize=8)
            self.audio_media = AudioMedia(self.audio_pipeline_bridge)
            logger.info("Media relay and pipeline bridges created for controller client")

        peer_connection =  RTCPeerConnection(self.get_rtc_config())

        if self.media_relay is None:
            raise RTCAppError("Cannot create peer connection: no media relay available. Controller may be disconnected.")

        # add audio and video encoded streams
        rtp_video_sender = peer_connection.addTrack(self.media_relay.subscribe(self.video_media))
        rtp_video_sender.on("pli", lambda cid=client_peer_id, ct=client_type: self.on_pli(cid, ct))
        peer_connection.addTrack(self.media_relay.subscribe(self.audio_media))

        # Microphone: one recvonly audio transceiver inside the SAME bundled SDP (no second
        # negotiation) so the browser can send its mic on demand. Received audio is decoded
        # and written into the virtual mic sink. Gated on the audio + microphone settings;
        # the m-line sits inactive until the client attaches a mic track.
        if bool(app_settings.audio_enabled[0]) and bool(app_settings.microphone_enabled[0]):
            self._setup_mic_receiver(peer_connection)

        # Primary data channel
        data_channel = peer_connection.createDataChannel("input", ordered=True, maxRetransmits=0)
        # New controller channel: compression support is re-negotiated per peer.
        self._gz_tx = False

        # Assign event handlers for the input data channel. Messages are
        # serialized through a single per-channel consumer so input events
        # (e.g. key down/up) are dispatched strictly in arrival order.
        data_channel.on("open", self.on_data_open)
        input_consumer = self._serialize_channel(
            data_channel,
            lambda msg, ch=data_channel, ct=client_type: self._on_input_channel_message(msg, ch, ct),
        )

        # A dynamic secondary data channel intended for file data transmission
        peer_connection.on("datachannel", lambda ch, cid=client_peer_id: self.on_datachannel(ch, cid))
        peer_connection.on("connectionstatechange", lambda cid=client_peer_id: asyncio.run_coroutine_threadsafe(self.on_connectionstatechange(cid), loop=self.async_event_loop))

        try:
            preferred_codec = self.get_mime_by_encoder(self.encoder)
            if preferred_codec is None:
                raise RTCAppError(f"Encoder {self.encoder} is not supported")
            self.force_codec(peer_connection, rtp_video_sender, preferred_codec)

            await peer_connection.setLocalDescription(await peer_connection.createOffer())
            offer = peer_connection.localDescription

            sdp = offer.sdp
            sdp = self.munge_sdp(sdp)
            await self.on_sdp('offer', sdp, client_peer_id)
        except BaseException:
            # Failure before registration: no teardown path could ever reach
            # this consumer, so it must be stopped here.
            input_consumer.cancel()
            raise

        self.peer_connections[client_peer_id] = {
            "peer_conn": peer_connection,
            "data_channel": data_channel,
            "client_type": client_type,
            # A channel that never reaches SCTP-established never emits 'close',
            # so its consumer must also be cancellable from teardown paths.
            "channel_consumers": [input_consumer],
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
        state = {"pb": None, "starting": False}
        self._mic_states.append(state)

        def sink(codec, frame):
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

    async def _stop_mic_playbacks(self):
        states, self._mic_states = self._mic_states, []
        for st in states:
            pb = st.get("pb")
            st["pb"] = None
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
            try:
                del self.peer_connections[client_peer_id]
            except KeyError:
                pass

            if peer_obj.get('client_type') == ClientType.CONTROLLER:
                logger.info("Controller peer disconnected, cleaning up media relay and bridges")
                self.media_relay = None
                self.aux_data_channel = None
                self.video_pipeline_bridge = None
                self.audio_pipeline_bridge = None
        except Exception as e:
            raise RTCAppError(f"Error stopping pipeline: {e}")

    async def start_rtc_connection(self, client_peer_id: str, client_type: str):
        try:
            logger.info("Starting RTC pipeline", extra={'client_peer_id': client_peer_id, 'client_type': client_type})
            await self._start_rtc_pipeline(client_peer_id, client_type)
        except Exception as e:
            logger.error(f"Error starting RTC pipeline: {e}", extra={'client_peer_id': client_peer_id, 'client_type': client_type}, exc_info=True)
        else:
            logger.info("RTC pipeline started successfully", extra={'client_peer_id': client_peer_id, 'client_type': client_type})

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

            self.media_relay = None
            self.aux_data_channel = None
            self.video_pipeline_bridge = None
            self.audio_pipeline_bridge = None
            logger.info("All RTC connections stopped, cleaned up media relay and bridges")
        except Exception as e:
            raise RTCAppError(f"Error stopping all RTC connections: {e}")
