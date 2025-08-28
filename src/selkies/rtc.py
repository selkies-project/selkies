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
import sys
import asyncio
import re
import json
import sys
import base64

from .webrtc import (
    MediaStreamTrack,
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
from typing import List, Any, Dict, Optional
import gi
gi.require_version('Gst', "1.0")
from gi.repository import Gst

logger = logging.getLogger("rtc")
logger.setLevel(logging.INFO)

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
        self.peer_connection = None
        self.data_channel = None
        self.aux_data_channel = None
        self.async_event_loop = async_event_loop
        self.stun_servers = stun_servers
        self.turn_servers = turn_servers
        self.encoder = encoder
        self.last_cursor_sent = None

        self.audio_pipeline_bridge = None
        self.video_pipeline_bridge = None

        # Data channel events
        self.on_data_open = lambda: logger.warning('unhandled on_data_open')
        self.on_data_close = lambda: logger.warning('unhandled on_data_close')
        self.on_data_error = lambda: logger.warning('unhandled on_data_error')
        self.on_data_message = lambda msg: logger.warning('unhandled on_data_message')
        self.on_data_msg_bytes = lambda data: logger.warning('unhandled on_data_msg_bytes')

        # WebRTC ICE and SDP events
        self.on_ice = lambda mlineindex, candidate: logger.warning('unhandled ice event')
        self.on_sdp = lambda sdp_type, sdp: logger.warning('unhandled sdp event')

        self.request_idr_frame = lambda: logger.warning('unhandled request_idr_frame')

    async def set_sdp(self, sdp_type: str, sdp: str):
        """Sets remote SDP received by peer"""

        if sdp_type != 'answer':
            raise RTCAppError('ERROR: sdp type was not "answer"')
        if sdp is None:
            raise RTCAppError("ERROR: sdp can't be None")

        sdp = RTCSessionDescription(sdp=sdp, type=sdp_type)

        if isinstance(sdp, RTCSessionDescription):
            await self.peer_connection.setRemoteDescription(sdp)

    async def set_ice(self, ice: Dict):
        """Adds ice candidate received from signalling server"""
        if ice.get('candidate') == "":
            await self.peer_connection.addIceCandidate(None)
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
            await self.peer_connection.addIceCandidate(icecandidate)
        else:
            raise RTCAppError("ERROR: ice candidate is not an instance of RTCIceCandidate")

    async def send_clipboard_data(self, data: str, mime_type: str = "text/plain"):
        """Sends clipboard data over the data channel in chunks"""
        CLIPBOARD_CHUNK_SIZE = 65400
        if not data:
            return
        
        # TODO: add support for binary clipboard data
        clipboard_message = base64.b64encode(data.encode()).decode("utf-8")
        read = 0
        while read < len(clipboard_message):
            if read + CLIPBOARD_CHUNK_SIZE < len(clipboard_message):
                chunk = clipboard_message[read:read + CLIPBOARD_CHUNK_SIZE]
                self.__send_data_channel_message("clipboard-msg", {"content": chunk})
            else:
                chunk = clipboard_message[read:]
                self.__send_data_channel_message("clipboard-msg-end", {"content": chunk})
            read += len(chunk)
        logger.debug(f"Sent clipboard data of length {len(data)} with mime type {mime_type}")

    def send_cursor_data(self, data: Any):
        self.last_cursor_sent = data
        self.__send_data_channel_message(
            "cursor", data)

    def send_gpu_stats(self, load: Any, memory_total: Any, memory_used: Any):
        """Sends GPU stats to the data channel"""

        self.__send_data_channel_message("gpu_stats", {
            "load": load,
            "memory_total": memory_total,
            "memory_used": memory_used,
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
            "system", {"action": "videoFramerate,"+str(framerate)})

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
            "system", {"action": "resize,"+str(resize_enabled)})

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

    def is_data_channel_ready(self):
        """Checks to see if the data channel is open"""
        return self.data_channel and self.peer_connection.connectionState == "connected"

    def __send_data_channel_message(self, msg_type: str, data: Any):
        """Sends message to the peer through the data channel.
        Message is dropped if the channel is not open.
        """
        if not self.peer_connection:
            return

        if not self.is_data_channel_ready():
            logger.debug("skipping message because data channel is not ready: %s" % msg_type)
            return

        msg = {"type": msg_type, "data": data}
        self.data_channel.send(json.dumps(msg))

    def send_media_data_over_channel(self, msg_type, data):
        self.__send_data_channel_message(msg_type, data)

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

    async def consume_data(self, sample, kind):
        if sample:
            buf = sample.get_buffer()
            caps = sample.get_caps()
            if not buf or not caps:
                logger.warning("consume_data: buffer or caps is None")
                return Gst.FlowReturn.OK

            # map the buffer to get a memoryview
            result, map_info = buf.map(Gst.MapFlags.READ)
            if not result:
                return Gst.FlowReturn.ERROR

            if kind == "video":
                try:
                    packet = av.Packet(bytes(map_info.data))

                    RTP_VIDEO_CLOCK_RATE = 90000
                    packet.time_base = Fraction(1, RTP_VIDEO_CLOCK_RATE)
                     # Convert GStreamer's nanosecond timestamps to the 90kHz video clock rate
                    if buf.pts is not None and buf.pts != Gst.CLOCK_TIME_NONE:
                        packet.pts = (buf.pts * RTP_VIDEO_CLOCK_RATE) // 1000000000
                        packet.dts = packet.pts  # Since there are no B-frames, PTS and DTS are the same
                        delta = buf.has_flags(Gst.BufferFlags.DELTA_UNIT)
                        packet.is_keyframe = not delta
                    if self.video_pipeline_bridge != None:
                        await self.video_pipeline_bridge.set_data(packet)
                except Exception as e:
                    logger.error(f"error processing video sample: {e}")
            else:
                try:
                    packet = av.Packet(bytes(map_info.data))

                    # For audio, dynamically get the clock rate from the GStreamer caps
                    audio_info = caps.get_structure(0)
                    _, clock_rate = audio_info.get_int("rate")
                    if not clock_rate:
                        logger.warning("Could not get clock-rate from caps, falling back to 48000")
                        clock_rate = 48000
                    packet.time_base = Fraction(1, clock_rate)

                    if buf.pts is not None and buf.pts != Gst.CLOCK_TIME_NONE:
                        packet.pts = (buf.pts * clock_rate) // 1000000000

                    if self.audio_pipeline_bridge != None:
                        await self.audio_pipeline_bridge.set_data(packet)
                except Exception as e:
                        logger.error(f"error processing audio sample: {e}")

            buf.unmap(map_info)
        else:
            logger.warning("sample received is empty")

    def format_turn_servers(self, turn_servers: List[str]):
        """
        Restructure each TURN server string to the expected format
        and return a list of formatted TURN server URLs.
        """
        formatted_servers = []
        for server in turn_servers or []:
            # Expecting format: username:password@host:port
            if '@' in server:
                parts = server.split('@')
                if len(parts) == 2:
                    credentials, host = parts
                    if ':' in credentials:
                        scheme, username, password = credentials.split(':', 2)
                        username = username.strip("/")
                    else:
                        scheme, username, password = 'turn:', '', ''
                    host = scheme + ':' + host
                    formatted_servers.append({
                        'urls': host,
                        'username': username,
                        'credential': password
                    })
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
            ice_servers.append(RTCIceServer(
                urls=turn.get('urls', []),
                username=turn.get('username', ''),
                credential=turn.get('credential', '')
            ))
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

    def on_datachannel(self, channel: RTCDataChannel):
        """Handles incoming auxiliary data channel"""
        logger.info("Auxiliary data channel opened: %s", channel.label)
        self.aux_data_channel = channel
        self.aux_data_channel.on("close", lambda: logger.info("Auxiliary data channel closed"))
        self.aux_data_channel.on("error", lambda e: logger.error("Auxiliary data channel error: %s", e))
        self.aux_data_channel.on("message", lambda data: asyncio.run_coroutine_threadsafe(self.on_data_msg_bytes(data), loop=self.async_event_loop))

    async def on_connectionstatechange(self):
        if self.peer_connection:
            state = self.peer_connection.connectionState
            logger.info("Peer Connection state is %s", state)
            if state == "failed":
                await self.peer_connection.close()
            elif state == "disconnected":
                logger.warning("Peer connection disconnected.")
            elif state == "connected":
                logger.info("Peer connection established.")
            elif state == "closed":
                logger.info("Peer connection closed.")
            elif state == "connecting":
                logger.info("Peer connection is connecting.")
            else:
                logger.debug(f"Unhandled peer connection state: {state}")

    def on_pli(self):
        logger.info("PLI occurred, triggering IDR frame request")
        asyncio.run_coroutine_threadsafe(self.request_idr_frame(), self.async_event_loop)

    async def start_rtc_connection(self):
        # TODO: The logic for hanlding multi user session could be handled by this func
        try:
            logger.info("Starting rtc pipeline")
            await self._start_rtc_pipeline()
        except Exception as e:
            logger.error(f"Error starting rtc pipeline: {e}", exc_info=True)
        else:
            logger.info("Pipeline started successfully")

    async def stop_rtc_connection(self):
        try:
            logger.info("Stopping rtc pipeline")
            await self._stop_rtc_pipeline()
        except Exception as e:
            logger.error(f"Error stopping rtc pipeline: {e}", exc_info=True)
        else:
            logger.info("Pipeline stopped successfully")

    async def _start_rtc_pipeline(self):
        self.peer_connection =  RTCPeerConnection(self.get_rtc_config())

        # create data bridge instances for video and audio
        self.video_pipeline_bridge = PipelineBridge()
        video_media = VideoMedia(self.video_pipeline_bridge)

        self.audio_pipeline_bridge = PipelineBridge()
        audio_media = AudioMedia(self.audio_pipeline_bridge)

        # add audio and video encoded streams
        rtp_video_sender = self.peer_connection.addTrack(video_media)
        rtp_video_sender.on("pli", self.on_pli)
        self.peer_connection.addTrack(audio_media)

        # Primary data channel
        self.data_channel = self.peer_connection.createDataChannel("input", ordered=True, maxRetransmits=0)

        # Assign event handlers for the input data channel
        self.data_channel.on("open", self.on_data_open)
        self.data_channel.on("message", lambda msg: asyncio.run_coroutine_threadsafe(self.on_data_message(msg), loop=self.async_event_loop))

        # A dynamic secondary data channel intended for file data transmission
        self.peer_connection.on("datachannel", self.on_datachannel)
        self.peer_connection.on("connectionstatechange", self.on_connectionstatechange)

        preferred_codec = self.get_mime_by_encoder(self.encoder)
        if preferred_codec is None:
            raise RTCAppError(f"Encoder {self.encoder} is not supported")
        self.force_codec(self.peer_connection, rtp_video_sender, preferred_codec)

        await self.peer_connection.setLocalDescription(await self.peer_connection.createOffer())
        offer = self.peer_connection.localDescription

        sdp = offer.sdp
        sdp = self.munge_sdp(sdp)
        await self.on_sdp('offer', sdp)

    def get_mime_by_encoder(self, encoder: str) -> Optional[str]:
        """Returns respective mime type by encoder name"""

        # TODO: aiortc only supports a limited set of codecs for now
        encoder_mime_map = {
            "x264enc"  : "video/H264",
            "vp8enc"   : "video/VP8",
            # "av1enc"   : "video/AV1"
        }
        return encoder_mime_map.get(encoder)

    async def _stop_rtc_pipeline(self):
        """Stops the WebRTC pipeline and closes the peer connection."""
        try:
            # FIXME: maybe checking for None is not appropriate
            if self.peer_connection is not None:
                await self.peer_connection.close()
            self.peer_connection = None
            self.data_channel = None
            self.aux_data_channel = None
            self.video_pipeline_bridge = None
            self.audio_pipeline_bridge = None
        except Exception as e:
            raise RTCAppError(f"Error stopping pipeline: {e}")
