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
import re
import json
import base64
import urllib.parse

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


def get_adjusted_chunk_size() -> int:
    """Returns adjusted chunk size.

    Base64 encoded data is higher in size compared to its input
    as it uses 4 chars per 3 bytes.
    """
    return (CLIPBOARD_CHUNK_SIZE * 3) // 4

class ClientType(str, Enum):
    CONTROLLER = "controller"
    VIEWER = "viewer"

class RTCAppError(Exception):
    pass

class PipelineBridge:
    """A bridge to asynchronously pass data between Media and the RTC pipeline"""
    def __init__(self):
        self._lock = asyncio.Lock()
        self._queue = asyncio.Queue(maxsize=1)

    async def set_data(self, data: Any):
        # If the queue is already full, it means the consumer is lagging so
        # remove the old item to make space for the new one.
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
        stun_servers: List[str] = None,
        turn_servers: List[str] = None
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

        # Data channel events
        self.on_data_open = lambda: logger.warning('unhandled on_data_open')
        self.on_data_close = lambda: logger.warning('unhandled on_data_close')
        self.on_data_error = lambda: logger.warning('unhandled on_data_error')
        self.on_data_message = lambda msg: logger.warning('unhandled on_data_message')
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

        sdp = RTCSessionDescription(sdp=sdp, type=sdp_type)
        if isinstance(sdp, RTCSessionDescription):
            await peer_conn.setRemoteDescription(sdp)

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
        data_bytes: bytes = data.encode() if is_text and isinstance(data, str) else data
        clipboard_chunk_size = get_adjusted_chunk_size()
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

    def send_video_bitrate(self, bitrate: int):
        """Sends the current video bitrate to the data channel"""
        logger.info("sending video bitrate")
        self.__send_data_channel_message(
            "system", {"action": "video_bitrate,%d" % bitrate})

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
        data_channel.send(json.dumps(msg))

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
        if "opus/" in sdp_text.lower():
            # OPUS_FRAME: Add ptime explicitly to SDP offer
            sdp_text = re.sub(r'([^-]sprop-[^\r\n]+)', r'\1\r\na=ptime:10', sdp_text)

        return sdp_text

    async def consume_data(self, buf, pts, kind):
        if kind == "video":
            if buf:
                try:
                    packet = av.Packet(bytes(buf))
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
                    packet = av.Packet(bytes(buf))
                    packet.time_base = Fraction(1, 48000)
                    if pts is not None:
                        packet.pts = pts
                    if self.audio_pipeline_bridge is not None:
                        await self.audio_pipeline_bridge.set_data(packet)
                except Exception as e:
                    logger.error(f"error processing audio sample: {e}")

    def update_rtc_config(self, stun_servers: List[str], turn_servers: List[str]):
        """Updates the RTC configuration with new STUN and TURN servers."""

        # TODO: Changing ICE servers on an existing peer connection is not supported by aiortc.
        # A new peer connection would need to be created for the changes to take effect, or
        # renegotiation logic would need to be implemented in aiortc.
        self.stun_servers = stun_servers
        self.turn_servers = turn_servers
        logger.warning("aiortc doesn't support ICE servers updation yet")

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

        transceiver = next(t for t in pc.getTransceivers() if t.sender == sender)
        logger.debug(f"Forcing codec preferences to: {[*chosen_codec, rtx_codec]}")
        transceiver.setCodecPreferences([*chosen_codec, rtx_codec])

    def on_datachannel(self, channel: RTCDataChannel, client_peer_id: str = None):
        """Handles incoming auxiliary data channel.

        Arguments:
            channel        -- the RTCDataChannel object provided by the event
            client_peer_id -- optional id of the client peer associated with this channel
        """
        logger.info(f"Auxiliary data channel opened: {channel.label}", extra={'client_peer_id': client_peer_id})
        self.aux_data_channel = channel
        self.aux_data_channel.on("close", lambda: logger.info("Auxiliary data channel closed"))
        self.aux_data_channel.on("error", lambda e: logger.error("Auxiliary data channel error: %s", e))
        self.aux_data_channel.on("message", lambda data: asyncio.run_coroutine_threadsafe(self.on_data_msg_bytes(data), loop=self.async_event_loop))

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

            self.audio_pipeline_bridge = PipelineBridge()
            self.audio_media = AudioMedia(self.audio_pipeline_bridge)
            logger.info("Media relay and pipeline bridges created for controller client")

        peer_connection =  RTCPeerConnection(self.get_rtc_config())

        if self.media_relay is None:
            raise RTCAppError("Cannot create peer connection: no media relay available. Controller may be disconnected.")

        # add audio and video encoded streams
        rtp_video_sender = peer_connection.addTrack(self.media_relay.subscribe(self.video_media))
        rtp_video_sender.on("pli", lambda cid=client_peer_id, ct=client_type: self.on_pli(cid, ct))
        peer_connection.addTrack(self.media_relay.subscribe(self.audio_media))

        # Primary data channel
        data_channel = peer_connection.createDataChannel("input", ordered=True, maxRetransmits=0)

        # Assign event handlers for the input data channel
        data_channel.on("open", self.on_data_open)
        data_channel.on("message", lambda msg: asyncio.run_coroutine_threadsafe(self.on_data_message(msg), loop=self.async_event_loop))

        # A dynamic secondary data channel intended for file data transmission
        peer_connection.on("datachannel", lambda ch, cid=client_peer_id: self.on_datachannel(ch, cid))
        peer_connection.on("connectionstatechange", lambda cid=client_peer_id: asyncio.run_coroutine_threadsafe(self.on_connectionstatechange(cid), loop=self.async_event_loop))

        preferred_codec = self.get_mime_by_encoder(self.encoder)
        if preferred_codec is None:
            raise RTCAppError(f"Encoder {self.encoder} is not supported")
        self.force_codec(peer_connection, rtp_video_sender, preferred_codec)

        await peer_connection.setLocalDescription(await peer_connection.createOffer())
        offer = peer_connection.localDescription

        sdp = offer.sdp
        sdp = self.munge_sdp(sdp)
        await self.on_sdp('offer', sdp, client_peer_id)

        self.peer_connections[client_peer_id] = {
            "peer_conn": peer_connection,
            "data_channel": data_channel,
            "client_type": client_type
        }

    def get_mime_by_encoder(self, encoder: str) -> Optional[str]:
        """Returns respective mime type by encoder name"""

        # TODO: aiortc only supports a limited set of codecs for now
        encoder_mime_map = {
            "x264enc"  : "video/H264",
            "nvh264enc": "video/H264",
            "vp8enc"   : "video/VP8",
            # "av1enc"   : "video/AV1"
        }
        return encoder_mime_map.get(encoder)

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
