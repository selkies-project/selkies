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
from ..webrtc import (
    MediaStreamTrack,
    RTCPeerConnection,
    RTCIceCandidate,
    RTCRtpSender,
    RTCSessionDescription,
    VideoStreamTrack,
    RTCConfiguration,
    RTCIceServer,
    AudioStreamTrack
)
from ..webrtc.rtcicetransport import (
    Candidate,
    candidate_from_aioice
)
import av
from fractions import Fraction
from abc import ABCMeta, abstractmethod
import gi
gi.require_version('Gst', "1.0")
from gi.repository import Gst

logger = logging.getLogger("rtc")
logger.setLevel(logging.INFO)

class RTCAppError(Exception):
    pass

class AudioMedia(AudioStreamTrack):
    def __init__(self, data_pipeline):
        super().__init__()
        self.data_pipeline = data_pipeline

    async def recv(self):
        # Grab the next audio packet
        packet = await self.data_pipeline.get_data()
        return packet

class VideoMedia(VideoStreamTrack):
    def __init__(self, data_pipeline):
        super().__init__()
        self.data_pipeline = data_pipeline

    async def recv(self):
        # Grab the next video packet
        packet = await self.data_pipeline.get_data()
        return packet

class PipelineBridge:
    """A bridge to asynchronously pass data between Media and the RTC pipeline"""
    def __init__(self):
        self._lock = asyncio.Lock()
        self._queue = asyncio.Queue(maxsize=1)

    async def set_data(self, data):
        # If the queue is already full, it means the consumer is lagging so
        # remove the old item to make space for the new one.
        async with self._lock:
            if self._queue.full():
                self._queue.get_nowait()
            self._queue.put_nowait(data)

    async def get_data(self):
        # asynchronously wait until an item is available in the queue
        return await self._queue.get()

class RTCApp:
    def __init__(self, async_event_loop, stun_servers=None, turn_servers=None, turn_username="", turn_password=""):
        self.peer_connection = None
        self.data_channel = None
        self.aux_data_channel = None
        self.async_event_loop = async_event_loop
        self.stun_servers = stun_servers
        self.turn_servers = turn_servers
        self.last_cursor_sent = None

        self.turn_username = turn_username
        self.turn_password = turn_password

        self.audio_pipeline_bridge = None
        self.video_pipeline_bridge = None

        # Data channel events
        self.on_data_open = lambda: logger.warning('unhandled on_data_open')
        self.on_data_close = lambda: logger.warning('unhandled on_data_close')
        self.on_data_error = lambda: logger.warning('unhandled on_data_error')
        self.on_data_message = lambda msg: logger.warning('unhandled on_data_message')
        self.on_data_msg_bytes = lambda data: logger.warnring('unhandled on_data_msg_bytes')

        # WebRTC ICE and SDP events
        self.on_ice = lambda mlineindex, candidate: logger.warning('unhandled ice event')
        self.on_sdp = lambda sdp_type, sdp: logger.warning('unhandled sdp event')

        self.request_idr_frame = lambda: logger.warning('unhandled request_idr_frame')

    async def set_sdp(self, sdp_type, sdp):
        """Sets remote SDP received by peer.

        Arguments:
            sdp_type {string} -- type of sdp, offer or answer
            sdp {object} -- SDP object

        Raises:
            RTCAppError -- thrown if SDP is received before session has been started.
            RTCAppError -- thrown if SDP type is not 'answer', this script initiates the call, not the peer.
        """

        if sdp_type != 'answer':
            raise RTCAppError('ERROR: sdp type was not "answer"')
        if sdp is None:
            raise RTCAppError("ERROR: sdp can't be None")

        sdp = RTCSessionDescription(sdp=sdp, type=sdp_type)

        if isinstance(sdp, RTCSessionDescription):
            await self.peer_connection.setRemoteDescription(sdp)


    async def set_ice(self, ice):
        """Adds ice candidate received from signalling server

        Arguments:
            mlineindex {integer} -- the mlineindex
            candidate {string} -- the candidate

        Raises:
            RTCAppError -- thrown if called before session is started.
        """

        # Generating RTCIceCandidate from ice
        obj = Candidate.from_sdp(ice.get('candidate'))
        icecandidate = candidate_from_aioice(obj)
        icecandidate.sdpMid = ice.get('sdpMid')
        if isinstance(icecandidate, RTCIceCandidate):
            await self.peer_connection.addIceCandidate(icecandidate)

    def send_clipboard_data(self, data):
        # TODO: WebRTC DataChannel accepts a maximum length of 65489 (= 65535 - 46 for '{"type": "clipboard", "data": {"content": ""}}'), remove this restriction after implementing DataChannel chunking
        CLIPBOARD_RESTRICTION = 65400
        clipboard_message = base64.b64encode(data.encode()).decode("utf-8")
        clipboard_length = len(clipboard_message)
        if clipboard_length <= CLIPBOARD_RESTRICTION:
            self.__send_data_channel_message(
                "clipboard", {"content": clipboard_message})
        else:
            logger.warning("clipboard may not be sent to the client because the base64 message length {} is above the maximum length of {}".format(clipboard_length, CLIPBOARD_RESTRICTION))

    def send_cursor_data(self, data):
        self.last_cursor_sent = data
        self.__send_data_channel_message(
            "cursor", data)

    def send_gpu_stats(self, load, memory_total, memory_used):
        """Sends GPU stats to the data channel

        Arguments:
            load {float} -- utilization of GPU between 0 and 1
            memory_total {float} -- total memory on GPU in MB
            memory_used {float} -- memory used on GPU in MB
        """

        self.__send_data_channel_message("gpu_stats", {
            "load": load,
            "memory_total": memory_total,
            "memory_used": memory_used,
        })

    def send_reload_window(self):
        """Sends reload window command to the data channel
        """
        logger.info("sending window reload")
        self.__send_data_channel_message(
            "system", {"action": "reload"})

    def send_framerate(self, framerate):
        """Sends the current framerate to the data channel
        """
        logger.info("sending framerate")
        self.__send_data_channel_message(
            "system", {"action": "videoFramerate,"+str(framerate)})

    def send_video_bitrate(self, bitrate):
        """Sends the current video bitrate to the data channel
        """
        logger.info("sending video bitrate")
        self.__send_data_channel_message(
            "system", {"action": "video_bitrate,%d" % bitrate})

    def send_audio_bitrate(self, bitrate):
        """Sends the current audio bitrate to the data channel
        """
        logger.info("sending audio bitrate")
        self.__send_data_channel_message(
            "system", {"action": "audio_bitrate,%d" % bitrate})

    def send_encoder(self, encoder):
        """Sends the encoder name to the data channel
        """
        logger.info("sending encoder: " + encoder)
        self.__send_data_channel_message(
            "system", {"action": "encoder,%s" % encoder})

    def send_resize_enabled(self, resize_enabled):
        """Sends the current resize enabled state
        """
        logger.info("sending resize enabled state")
        self.__send_data_channel_message(
            "system", {"action": "resize,"+str(resize_enabled)})

    def send_remote_resolution(self, res):
        """sends the current remote resolution to the client
        """
        logger.info("sending remote resolution of: " + res)
        self.__send_data_channel_message(
            "system", {"action": "resolution," + res})

    def send_ping(self, t):
        """Sends a ping request over the data channel to measure latency
        """
        self.__send_data_channel_message(
            "ping", {"start_time": float("%.3f" % t)})

    def send_latency_time(self, latency):
        """Sends measured latency response time in ms
        """
        self.__send_data_channel_message(
            "latency_measurement", {"latency_ms": latency})

    def send_system_stats(self, cpu_percent, mem_total, mem_used):
        """Sends system stats
        """
        self.__send_data_channel_message(
            "system_stats", {
                "cpu_percent": cpu_percent,
                "mem_total": mem_total,
                "mem_used": mem_used,
            })

    def is_data_channel_ready(self):
        """Checks to see if the data channel is open.

        Returns:
            [bool] -- true if data channel is open
        """
        return self.data_channel and self.peer_connection.connectionState == "connected"

    def __send_data_channel_message(self, msg_type, data):
        """Sends message to the peer through the data channel

        Message is dropped if the channel is not open.

        Arguments:
            msg_type {string} -- the type of message being sent
            data {dict} -- data to send, this is JSON serialized.
        """
        if not self.is_data_channel_ready():
            logger.debug("skipping message because data channel is not ready: %s" % msg_type)
            return

        msg = {"type": msg_type, "data": data}
        self.data_channel.send(json.dumps(msg))

    def send_media_data_over_channel(self, msg_type, data):
        self.__send_data_channel_message(msg_type, data)

    async def receive_data(self, sample, kind):
        if sample:
            buf = sample.get_buffer()
            caps = sample.get_caps()
            if not buf or not caps:
                logger.warning("receive_data: buffer or caps is None")
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

    def get_rtc_config(self):
        # TODO: Handle multiple TURN servers
        turn_server = self.turn_servers[0].split('@')[1]
        turn_server = self.turn_servers[0].split(':', 1)[0] + '://' + turn_server

        ice_servers = []
        ice_servers.append(RTCIceServer(urls=self.stun_servers))
        ice_servers.append(RTCIceServer(urls=turn_server, username=self.turn_username, credential=self.turn_password))
        config = RTCConfiguration(iceServers=ice_servers)
        return config

    def force_codec(self, pc: RTCPeerConnection, sender: RTCRtpSender, forced_codec_mime: str):
        """
        Forces a codec by MIME type and its associated RTX codec
        """
        kind = sender.track.kind  # "video"
        capabilities = RTCRtpSender.getCapabilities(kind)

        chosen_codec = None
        for codec in capabilities.codecs:
            if codec.mimeType == forced_codec_mime:
                chosen_codec = codec
                break

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
        logger.info(f"Forcing codec preferences to: {[chosen_codec, rtx_codec]}")
        transceiver.setCodecPreferences([chosen_codec, rtx_codec])

    async def start_rtc_pipeline(self):
        self.peer_connection =  RTCPeerConnection(self.get_rtc_config())

        # Primary data channel
        self.data_channel = self.peer_connection.createDataChannel("input", ordered=True, maxRetransmits=0)

        # Assign event handlers for the input data channel
        self.data_channel.on("open", self.on_data_open)
        self.data_channel.on("message", self.on_data_message)
        
        # A dynamic secondary data channel intended for file data transmission
        @self.peer_connection.on("datachannel")
        def on_datachannel(channel):
            """Handles incoming auxiliary data channel"""
            logger.info("Auxiliary data channel opened: %s", channel.label)
            self.aux_data_channel = channel
            self.aux_data_channel.on("close", lambda: logger.info("Auxiliary data channel closed"))
            self.aux_data_channel.on("error", lambda e: logger.error("Auxiliary data channel error: %s", e))
            self.aux_data_channel.on("message", lambda data: asyncio.run_coroutine_threadsafe(self.on_data_msg_bytes(data), loop=self.async_event_loop))

        @self.peer_connection.on("connectionstatechange")
        async def on_connectionstatechange():
            logger.info("Peer Connection state is %s", self.peer_connection.connectionState)
            if self.peer_connection.connectionState == "failed":
                await self.peer_connection.close()

        # create data bridge instances for video and audio
        self.video_pipeline_bridge = PipelineBridge()
        video_media = VideoMedia(self.video_pipeline_bridge)

        self.audio_pipeline_bridge = PipelineBridge()
        audio_media = AudioMedia(self.audio_pipeline_bridge)

        # add audio and video encoded streams
        sender = self.peer_connection.addTrack(video_media)
        @sender.on("pli")
        def on_pli():
            logger.info("PLI occurred, triggering IDR frame request")
            self.request_idr_frame()
        self.peer_connection.addTrack(audio_media)

        # FIXME: forcing h264 codec for video, this should be configurable
        self.force_codec(self.peer_connection, sender, "video/H264")
        await self.peer_connection.setLocalDescription(await self.peer_connection.createOffer())
        offer = self.peer_connection.localDescription

        # TODO: sdp munging is required
        sdp = offer.sdp
        await self.on_sdp('offer', sdp)

    async def stop_pipeline(self):
        logger.info("stopping pipeline")
        await self.peer_connection.close()
        self.peer_connection = None
        self.data_channel = None
        self.aux_data_channel = None
        self.video_pipeline_bridge = None
        self.audio_pipeline_bridge = None
        logger.info("pipeline stopped")
