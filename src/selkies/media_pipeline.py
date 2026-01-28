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

import asyncio
import logging
import os
import re
import sys
import time
import ctypes
from abc import ABCMeta, abstractmethod

from pixelflux import CaptureSettings, ScreenCapture, StripeCallback
from pcmflux import AudioCapture, AudioCaptureSettings, AudioChunkCallback

logger = logging.getLogger("media_pipeline")
logger.setLevel(logging.INFO)

_gst_imported = False
_Gst = None
_GstVideo = None

def _ensure_gst_imported():
    """Lazy initialization of GStreamer dependencies"""
    global _gst_imported, _Gst, _GstVideo
    if not _gst_imported:
        try:
            import gi
            gi.require_version('Gst', "1.0")
            gi.require_version('GstVideo', "1.0")
            from gi.repository import Gst, GstVideo
            Gst.init(None)
            _Gst = Gst
            _GstVideo = GstVideo
            _gst_imported = True
            logger.info("GStreamer-Python install looks OK")
        except Exception as e:
            msg = """ERROR: could not find working GStreamer-Python installation.

    If GStreamer is installed at a certain location, set the path to the environment variable GSTREAMER_PATH, then make sure your environment is set correctly using the below commands (for Debian-like distributions):

    export GSTREAMER_PATH="${GSTREAMER_PATH:-$(pwd)}"
    export PATH="${GSTREAMER_PATH}/bin${PATH:+:${PATH}}"
    export LD_LIBRARY_PATH="${GSTREAMER_PATH}/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}"
    export GST_PLUGIN_PATH="${GSTREAMER_PATH}/lib/x86_64-linux-gnu/gstreamer-1.0${GST_PLUGIN_PATH:+:${GST_PLUGIN_PATH}}"
    export GST_PLUGIN_SYSTEM_PATH="${XDG_DATA_HOME:-${HOME:-~}/.local/share}/gstreamer-1.0/plugins:/usr/lib/x86_64-linux-gnu/gstreamer-1.0${GST_PLUGIN_SYSTEM_PATH:+:${GST_PLUGIN_SYSTEM_PATH}}"
    export GI_TYPELIB_PATH="${GSTREAMER_PATH}/lib/x86_64-linux-gnu/girepository-1.0:/usr/lib/x86_64-linux-gnu/girepository-1.0${GI_TYPELIB_PATH:+:${GI_TYPELIB_PATH}}"
    export PYTHONPATH="${GSTREAMER_PATH}/lib/python3/dist-packages${PYTHONPATH:+:${PYTHONPATH}}"

    Replace "x86_64-linux-gnu" in other architectures manually or use "$(gcc -print-multiarch)" in place.
    """
            logger.error(msg)
            logger.error(e)
            raise Exception("Unable to import gstreamer packages, exiting...")

def _cleanup_gstreamer():
    """Cleanup GStreamer resources and reset module-level variables"""
    global _gst_imported, _Gst, _GstVideo
    if _gst_imported and _Gst:
        try:
            if hasattr(_Gst, 'deinit'):
                _Gst.deinit()
        except Exception as e:
            logger.warning(f"Error during GStreamer cleanup: {e}")
        finally:
            _gst_imported = False
            _Gst = None
            _GstVideo = None

class MediaPipelineError(Exception):
    pass

class MediaPipeline(metaclass=ABCMeta):
    @abstractmethod
    def start_media_pipeline(self):
        pass

    @abstractmethod
    def stop_media_pipeline(self):
        pass

    @abstractmethod
    async def set_pointer_visible(self, visible: bool):
        pass

    @abstractmethod
    async def set_framerate(self, framerate: int):
        pass

    @abstractmethod
    async def set_video_bitrate(self, bitrate: int):
        pass

    @abstractmethod
    async def set_audio_bitrate(self, bitrate: int):
        pass

class MediaPipelineGst(MediaPipeline):
    def __init__(
        self,
        async_event_loop: asyncio.AbstractEventLoop,
        encoder: str,
        audio_channels: int = 2,
        framerate: int = 30,
        gpu_id: int = 0,
        video_bitrate: int = 2000,
        audio_bitrate: int = 96000,
        keyframe_distance: float = -1.0,
        video_packetloss_percent: float = 0.0,
        audio_packetloss_percent: float = 0.0
    ):
        """Initialize GStreamer WebRTC app.

        Initializes GObjects and checks for required plugins.

        Arguments:
            stun_servers {[list of string]} -- Optional STUN server uris in the form of:
                                    stun:<host>:<port>
            turn_servers {[list of strings]} -- Optional TURN server uris in the form of:
                                    turn://<user>:<password>@<host>:<port>
        """

        self.async_event_loop = async_event_loop
        self.audio_channels = audio_channels
        self.pipeline = None
        self.encoder = encoder
        self.gpu_id = gpu_id
        self.framerate = framerate
        self.video_bitrate = video_bitrate
        self.audio_bitrate = audio_bitrate
        # Keyframe distance in seconds
        self.keyframe_distance = keyframe_distance
        # Packet loss base percentage
        self.video_packetloss_percent = video_packetloss_percent
        self.audio_packetloss_percent = audio_packetloss_percent

        self._calculate_auxiliary_keyframe_properties()

        # Ensure GStreamer is imported before using it
        _ensure_gst_imported()

        self.check_plugins()

        self.ximagesrc = None
        self.ximagesrc_caps = None
        self._bus_task = None
        self.produce_data = lambda s, k: logger.warning('unhandled produce_data')
        self.send_data_channel_message = lambda msg: logger.warning('unhandled send_data_channel_message')
        self.last_resize_success = True
        self.async_lock = asyncio.Lock()

    def _calculate_auxiliary_keyframe_properties(self):
        """Calculate required keyframe properties based on current settings"""
        # Enforce minimum keyframe interval to 60 frames
        self.min_keyframe_frame_distance = 60
        self.keyframe_frame_distance = -1 if self.keyframe_distance == -1.0 else max(self.min_keyframe_frame_distance, int(self.framerate * self.keyframe_distance))

        # Set VBV/HRD buffer multiplier to frame time, set 1.5x when optimal (no keyframes/GOP) to prevent quality degradation in encoders, relax 2x when keyframe/GOP is periodic
        vbv_multiplier = 1.5 if self.keyframe_distance == -1.0 else 3
        self.vbv_multipliers = {
            'nv': vbv_multiplier,
            'va': vbv_multiplier,
            'vp': vbv_multiplier,
            'sw': vbv_multiplier
        }

        # Prevent bitrate from overshooting because of FEC
        self.fec_video_bitrate = int(self.video_bitrate / (1.0 + (self.video_packetloss_percent / 100.0)))
        # Keep audio bitrate to exact value and increase effective bitrate after FEC to prevent audio quality degradation
        self.fec_audio_bitrate = int(self.audio_bitrate * (1.0 + (self.audio_packetloss_percent / 100.0)))

    def _create_app_sinks(self):
        """Create application sinks for video and audio"""
        self._create_app_sink("video")
        self._create_app_sink("audio")

    def _create_app_sink(self, kind: str):
        """Create an appsink for the specified media kind"""
        appsink = _Gst.ElementFactory.make("appsink", f"appsink_{kind}")
        appsink.set_property("emit-signals", True)
        appsink.set_property("max-buffers", 5)
        # Disable synchronization against the pipeline clock
        # This makes the appsink to emit new_sample event as soon as data is avilable
        appsink.set_property("sync", False)

        appsink.connect("new-sample", lambda sink: self._on_new_sample(sink, kind))
        appsink.connect("new-preroll", lambda sink: self._on_preroll(sink, kind))
        self.pipeline.add(appsink)

    def _on_new_sample(self, sink, kind: str):
        sample = sink.emit("pull-sample")
        if sample:
            asyncio.run_coroutine_threadsafe(
                self.produce_data(sample, kind),
                self.async_event_loop
            )
        else:
            logger.warning(f"failed to pull {kind} sample")
        return _Gst.FlowReturn.OK

    def _on_preroll(self, sink, kind: str):
        sample = sink.emit("pull-preroll")
        if sample:
            buf = sample.get_buffer()
            caps = sample.get_caps()
            print("Got sample caps for preroll:", caps.to_string(), "buffer size:", buf.get_size())
            asyncio.run_coroutine_threadsafe(
                self.produce_data(sample, kind),
                self.async_event_loop
            )
        else:
            logger.warning(f"failed to pull {kind} preroll sample")
        return _Gst.FlowReturn.OK

    async def dynamic_idr_frame(self):
        """Send an immediate IDR frame request to the encoder"""
        if not self.pipeline:
            return

        appsink_video = self.pipeline.get_by_name("appsink_video")
        if not appsink_video:
            logger.error("appsink_video element not found in pipeline")
            return

        sink_pad = appsink_video.get_static_pad("sink")
        if not sink_pad:
            logger.error("appsink_video element has no sink pad")
            return

        peer_pad = sink_pad.get_peer()
        if not peer_pad:
            logger.error("appsink_video pad has no peer")
            return

        event = _GstVideo.video_event_new_upstream_force_key_unit(
                    _Gst.CLOCK_TIME_NONE,  # running_time
                    True,                 # all_headers
                    0                     # count; 0 means just the very next frame.
                )
        try:
            sent = await asyncio.to_thread(peer_pad.send_event, event)
            if sent:
                logger.info("Successfully sent force key-frame event upstream.")
            else:
                logger.warning("Failed to send key-frame event upstream.")
        except Exception as e:
            logger.exception(f"An unexpected error occurred while sending key-frame event: {e}")

    def _build_video_pipeline(self):
        """Adds the RTP video stream to the pipeline.
        """

        # Create ximagesrc element named x11
        # Note that when using the ximagesrc plugin, ensure that the X11 server was
        # started with shared memory support: '+extension MIT-SHM' to achieve
        # full frame rates.
        # You can check if XSHM is in use with the following command:
        #   GST_DEBUG=default:5 gst-launch-1.0 ximagesrc ! fakesink num-buffers=1 2>&1 |grep -i xshm
        self.ximagesrc = _Gst.ElementFactory.make("ximagesrc", "x11")
        ximagesrc = self.ximagesrc

        # disables display of the pointer using the XFixes extension,
        # common when building a remote desktop interface as the clients
        # mouse pointer can be used to give the user perceived lower latency.
        # This can be programmatically toggled after the pipeline is started
        # for example if the user is viewing fullscreen in the browser,
        # they may want to revert to seeing the remote cursor when the
        # client side cursor disappears.
        ximagesrc.set_property("show-pointer", 0)

        # Tells GStreamer that you are using an X11 window manager or
        # compositor with off-screen buffer. If you are not using a
        # window manager this can be set to 0. It's also important to
        # make sure that your X11 server is running with the XSHM extension
        # to ensure direct memory access to frames which will reduce latency.
        ximagesrc.set_property("remote", 1)

        # Defines the size in bytes to read per buffer. Increasing this from
        # the default of 4096 bytes helps performance when capturing high
        # resolutions like 1080P, and 2K.
        ximagesrc.set_property("blocksize", 16384)

        # The X11 XDamage extension allows the X server to indicate when a
        # regions of the screen has changed. While this can significantly
        # reduce CPU usage when the screen is idle, it has little effect with
        # constant motion. This can also have a negative consequences with H.264
        # as the video stream can drop out and take several seconds to recover
        # until a valid I-Frame is received.
        # Set this to 0 for most streaming use cases.
        ximagesrc.set_property("use-damage", 0)

        # Create capabilities for ximagesrc
        self.ximagesrc_caps = _Gst.caps_from_string("video/x-raw")

        # Setting the framerate=60/1 capability instructs the ximagesrc element
        # to generate buffers at 60 frames per second (FPS).
        # The higher the FPS, the lower the latency so this parameter is one
        # way to set the overall target latency of the pipeline though keep in
        # mind that the pipeline may not always perform at the full 60 FPS.
        self.ximagesrc_caps.set_value("framerate", _Gst.Fraction(self.framerate, 1))

        # Create a capability filter for the ximagesrc_caps
        self.ximagesrc_capsfilter = _Gst.ElementFactory.make("capsfilter")
        self.ximagesrc_capsfilter.set_property("caps", self.ximagesrc_caps)

        # ADD_ENCODER: Add new encoder to this list and modify all locations with "ADD_ENCODER:"
        # Reference configuration for fixing when something is broken in web browsers:
        #   https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/blob/main/net/webrtc/src/webrtcsink/imp.rs
        if self.encoder in ["nvh264enc"]:
            # Upload buffers from ximagesrc directly to CUDA memory where
            # the colorspace conversion will be performed.
            cudaupload = _Gst.ElementFactory.make("cudaupload")
            if self.gpu_id >= 0:
                cudaupload.set_property("cuda-device-id", self.gpu_id)

            # Convert the colorspace from BGRx to NVENC compatible format.
            # This is performed with CUDA which reduces the overall CPU load
            # compared to using the software videoconvert element.
            cudaconvert = _Gst.ElementFactory.make("cudaconvert")
            if self.gpu_id >= 0:
                cudaconvert.set_property("cuda-device-id", self.gpu_id)

            # Instructs cudaconvert to handle Quality of Service (QOS) events
            # from the rest of the pipeline. Setting this value increases
            # encoder stability.
            cudaconvert.set_property("qos", True)

            # Convert ximagesrc BGRx format to NV12 using cudaconvert.
            # This is a more compatible format for client-side software decoders.
            cudaconvert_caps = _Gst.caps_from_string("video/x-raw(memory:CUDAMemory)")
            cudaconvert_caps.set_value("format", "NV12")
            cudaconvert_capsfilter = _Gst.ElementFactory.make("capsfilter")
            cudaconvert_capsfilter.set_property("caps", cudaconvert_caps)

            # Create the nvh264enc element named nvenc.
            # This is the heart of the video pipeline that converts the raw
            # frame buffers to an H.264 encoded byte-stream on the GPU.
            if self.gpu_id > 0:
                if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                    nvh264enc = _Gst.ElementFactory.make("nvcudah264device{}enc".format(self.gpu_id), "nvenc")
                else:
                    nvh264enc = _Gst.ElementFactory.make("nvh264device{}enc".format(self.gpu_id), "nvenc")
            else:
                if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                    nvh264enc = _Gst.ElementFactory.make("nvcudah264enc", "nvenc")
                else:
                    nvh264enc = _Gst.ElementFactory.make("nvh264enc", "nvenc")

            # The initial bitrate of the encoder in bits per second.
            # Setting this to 0 will use the bitrate from the NVENC preset.
            # This parameter can be set while the pipeline is running using the
            # set_video_bitrate() method. This helps to match the available
            # bandwidth. If set too high, the cliend side jitter buffer will
            # not be unable to lock on to the stream and it will fail to render.
            nvh264enc.set_property("bitrate", self.fec_video_bitrate)

            # Rate control mode tells the encoder how to compress the frames to
            # reach the target bitrate. A Constant Bit Rate (CBR) setting is best
            # for streaming use cases as bitrate is the most important factor.
            # A Variable Bit Rate (VBR) setting tells the encoder to adjust the
            # compression level based on scene complexity, something not needed
            # when streaming in real-time.
            if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                nvh264enc.set_property("rate-control", "cbr")
            else:
                nvh264enc.set_property("rc-mode", "cbr")

            # Group of Pictures (GOP) size is the distance between I-Frames that
            # contain the full frame data needed to render a whole frame.
            # A negative consequence when using infinite GOP size is that
            # when packets are lost, the decoder may never recover.
            # NVENC supports infinite GOP by setting this to -1.
            nvh264enc.set_property("gop-size", -1 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            # Minimize GOP-to-GOP rate fluctuations
            nvh264enc.set_property("strict-gop", True)

            # The NVENC encoder supports a limited number of encoding presets.
            # These presets are different than the open x264 standard.
            # The presets control the picture coding technique, bitrate,
            # and encoding quality.
            #
            # See this link for details on NVENC parameters recommended for
            # low-latency streaming (also a setting reference for other encoders):
            #   https://docs.nvidia.com/video-technologies/video-codec-sdk/12.2/nvenc-video-encoder-api-prog-guide/index.html#recommended-nvenc-settings
            #
            # See this link for details on each preset:
            #   https://docs.nvidia.com/video-technologies/video-codec-sdk/12.2/nvenc-preset-migration-guide/index.html
            nvh264enc.set_property("aud", False)
            # Do not automatically add b-frames
            nvh264enc.set_property("b-adapt", False)
            # Disable lookahead
            nvh264enc.set_property("rc-lookahead", 0)
            # Set VBV/HRD buffer size (kbits) to optimize for live streaming
            nvh264enc.set_property("vbv-buffer-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multipliers['nv']))
            if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                nvh264enc.set_property("b-frames", 0)
                # Zero-latency operation mode (no reordering delay)
                nvh264enc.set_property("zero-reorder-delay", True)
            else:
                nvh264enc.set_property("bframes", 0)
                # Zero-latency operation mode (no reordering delay)
                nvh264enc.set_property("zerolatency", True)
            if _Gst.version().major == 1 and _Gst.version().minor > 20:
                # CABAC is more bandwidth-efficient compared to CAVLC at a tradeoff of slight increase (<= 1 ms) in decoding time
                nvh264enc.set_property("cabac", True)
                # Insert sequence headers (SPS/PPS) per IDR
                nvh264enc.set_property("repeat-sequence-header", True)
            if _Gst.version().major == 1 and _Gst.version().minor > 22:
                nvh264enc.set_property("preset", "p4")
                nvh264enc.set_property("tune", "ultra-low-latency")
                # Two-pass mode allows to detect more motion vectors,
                # better distribute bitrate across the frame
                # and more strictly adhere to bitrate limits.
                nvh264enc.set_property("multi-pass", "two-pass-quarter")
            else:
                nvh264enc.set_property("preset", "low-latency-hq")

        elif self.encoder in ["nvh265enc"]:
            cudaupload = _Gst.ElementFactory.make("cudaupload")
            if self.gpu_id >= 0:
                cudaupload.set_property("cuda-device-id", self.gpu_id)
            cudaconvert = _Gst.ElementFactory.make("cudaconvert")
            if self.gpu_id >= 0:
                cudaconvert.set_property("cuda-device-id", self.gpu_id)
            cudaconvert.set_property("qos", True)
            cudaconvert_caps = _Gst.caps_from_string("video/x-raw(memory:CUDAMemory)")
            cudaconvert_caps.set_value("format", "NV12")
            cudaconvert_capsfilter = _Gst.ElementFactory.make("capsfilter")
            cudaconvert_capsfilter.set_property("caps", cudaconvert_caps)

            if self.gpu_id > 0:
                if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                    nvh265enc = _Gst.ElementFactory.make("nvcudah265device{}enc".format(self.gpu_id), "nvenc")
                else:
                    nvh265enc = _Gst.ElementFactory.make("nvh265device{}enc".format(self.gpu_id), "nvenc")
            else:
                if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                    nvh265enc = _Gst.ElementFactory.make("nvcudah265enc", "nvenc")
                else:
                    nvh265enc = _Gst.ElementFactory.make("nvh265enc", "nvenc")

            nvh265enc.set_property("bitrate", self.fec_video_bitrate)

            if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                nvh265enc.set_property("rate-control", "cbr")
            else:
                nvh265enc.set_property("rc-mode", "cbr")

            nvh265enc.set_property("gop-size", -1 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            nvh265enc.set_property("strict-gop", True)
            nvh265enc.set_property("aud", False)
            # B-frames in H.265 are only provided with newer GPUs
            nvenc_properties = [nvenc_property.name for nvenc_property in nvh265enc.list_properties()]
            if "b-adapt" in nvenc_properties:
                nvh265enc.set_property("b-adapt", False)
            nvh265enc.set_property("rc-lookahead", 0)
            nvh265enc.set_property("vbv-buffer-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multipliers['nv']))
            if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                if "b-frames" in nvenc_properties:
                    nvh265enc.set_property("b-frames", 0)
                nvh265enc.set_property("zero-reorder-delay", True)
            else:
                if "bframes" in nvenc_properties:
                    nvh265enc.set_property("bframes", 0)
                nvh265enc.set_property("zerolatency", True)
            if _Gst.version().major == 1 and _Gst.version().minor > 20:
                nvh265enc.set_property("repeat-sequence-header", True)
            if _Gst.version().major == 1 and _Gst.version().minor > 22:
                nvh265enc.set_property("preset", "p4")
                nvh265enc.set_property("tune", "ultra-low-latency")
                nvh265enc.set_property("multi-pass", "two-pass-quarter")
            else:
                nvh265enc.set_property("preset", "low-latency-hq")

        elif self.encoder in ["nvav1enc"]:
            cudaupload = _Gst.ElementFactory.make("cudaupload")
            if self.gpu_id >= 0:
                cudaupload.set_property("cuda-device-id", self.gpu_id)
            cudaconvert = _Gst.ElementFactory.make("cudaconvert")
            if self.gpu_id >= 0:
                cudaconvert.set_property("cuda-device-id", self.gpu_id)
            cudaconvert.set_property("qos", True)
            cudaconvert_caps = _Gst.caps_from_string("video/x-raw(memory:CUDAMemory)")
            cudaconvert_caps.set_value("format", "NV12")
            cudaconvert_capsfilter = _Gst.ElementFactory.make("capsfilter")
            cudaconvert_capsfilter.set_property("caps", cudaconvert_caps)

            if self.gpu_id > 0:
                if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                    nvav1enc = _Gst.ElementFactory.make("nvcudaav1device{}enc".format(self.gpu_id), "nvenc")
                else:
                    nvav1enc = _Gst.ElementFactory.make("nvav1device{}enc".format(self.gpu_id), "nvenc")
            else:
                if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                    nvav1enc = _Gst.ElementFactory.make("nvcudaav1enc", "nvenc")
                else:
                    nvav1enc = _Gst.ElementFactory.make("nvav1enc", "nvenc")

            nvav1enc.set_property("bitrate", self.fec_video_bitrate)

            if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                nvav1enc.set_property("rate-control", "cbr")
            else:
                nvav1enc.set_property("rc-mode", "cbr")

            nvav1enc.set_property("gop-size", -1 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            nvav1enc.set_property("strict-gop", True)
            nvav1enc.set_property("b-adapt", False)
            nvav1enc.set_property("rc-lookahead", 0)
            nvav1enc.set_property("vbv-buffer-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multipliers['nv']))
            if _Gst.version().major == 1 and 20 < _Gst.version().minor <= 24:
                nvav1enc.set_property("b-frames", 0)
                nvav1enc.set_property("zero-reorder-delay", True)
            else:
                nvav1enc.set_property("bframes", 0)
                nvav1enc.set_property("zerolatency", True)
            if _Gst.version().major == 1 and _Gst.version().minor > 22:
                nvav1enc.set_property("preset", "p4")
                nvav1enc.set_property("tune", "ultra-low-latency")
                nvav1enc.set_property("multi-pass", "two-pass-quarter")
            else:
                nvav1enc.set_property("preset", "low-latency-hq")

        elif self.encoder in ["vah264enc"]:
            # colorspace conversion
            if self.gpu_id > 0:
                vapostproc = _Gst.ElementFactory.make("varenderD{}postproc".format(128 + self.gpu_id), "vapostproc")
            else:
                vapostproc = _Gst.ElementFactory.make("vapostproc")
            vapostproc.set_property("scale-method", "fast")
            vapostproc.set_property("qos", True)
            vapostproc_caps = _Gst.caps_from_string("video/x-raw(memory:VAMemory)")
            vapostproc_caps.set_value("format", "NV12")
            vapostproc_capsfilter = _Gst.ElementFactory.make("capsfilter")
            vapostproc_capsfilter.set_property("caps", vapostproc_caps)

            # encoder
            if self.gpu_id > 0:
                vah264enc = _Gst.ElementFactory.make("varenderD{}h264enc".format(128 + self.gpu_id), "vaenc")
                if vah264enc is None:
                    vah264enc = _Gst.ElementFactory.make("varenderD{}h264lpenc".format(128 + self.gpu_id), "vaenc")
            else:
                vah264enc = _Gst.ElementFactory.make("vah264enc", "vaenc")
                if vah264enc is None:
                    vah264enc = _Gst.ElementFactory.make("vah264lpenc", "vaenc")
            vah264enc.set_property("aud", False)
            vah264enc.set_property("b-frames", 0)
            # Set VBV/HRD buffer size (kbits) to optimize for live streaming
            vah264enc.set_property("cpb-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multipliers['va']))
            vah264enc.set_property("dct8x8", False)
            vah264enc.set_property("key-int-max", 1024 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            vah264enc.set_property("mbbrc", "disabled")
            vah264enc.set_property("num-slices", 4)
            vah264enc.set_property("ref-frames", 1)
            vah264enc.set_property("rate-control", "cbr")
            vah264enc.set_property("target-usage", 6)
            vah264enc.set_property("bitrate", self.fec_video_bitrate)

        elif self.encoder in ["vah265enc"]:
            # colorspace conversion
            if self.gpu_id > 0:
                vapostproc = _Gst.ElementFactory.make("varenderD{}postproc".format(128 + self.gpu_id), "vapostproc")
            else:
                vapostproc = _Gst.ElementFactory.make("vapostproc")
            vapostproc.set_property("scale-method", "fast")
            vapostproc.set_property("qos", True)
            vapostproc_caps = _Gst.caps_from_string("video/x-raw(memory:VAMemory)")
            vapostproc_caps.set_value("format", "NV12")
            vapostproc_capsfilter = _Gst.ElementFactory.make("capsfilter")
            vapostproc_capsfilter.set_property("caps", vapostproc_caps)

            # encoder
            if self.gpu_id > 0:
                vah265enc = _Gst.ElementFactory.make("varenderD{}h265enc".format(128 + self.gpu_id), "vaenc")
                if vah265enc is None:
                    vah265enc = _Gst.ElementFactory.make("varenderD{}h265lpenc".format(128 + self.gpu_id), "vaenc")
            else:
                vah265enc = _Gst.ElementFactory.make("vah265enc", "vaenc")
                if vah265enc is None:
                    vah265enc = _Gst.ElementFactory.make("vah265lpenc", "vaenc")
            vah265enc.set_property("aud", False)
            vah265enc.set_property("b-frames", 0)
            # Set VBV/HRD buffer size (kbits) to optimize for live streaming
            vah265enc.set_property("cpb-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multipliers['va']))
            vah265enc.set_property("key-int-max", 1024 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            vah265enc.set_property("mbbrc", "disabled")
            vah265enc.set_property("num-slices", 4)
            vah265enc.set_property("ref-frames", 1)
            vah265enc.set_property("rate-control", "cbr")
            vah265enc.set_property("target-usage", 6)
            vah265enc.set_property("bitrate", self.fec_video_bitrate)

        elif self.encoder in ["vavp9enc"]:
            # colorspace conversion
            if self.gpu_id > 0:
                vapostproc = _Gst.ElementFactory.make("varenderD{}postproc".format(128 + self.gpu_id), "vapostproc")
            else:
                vapostproc = _Gst.ElementFactory.make("vapostproc")
            vapostproc.set_property("scale-method", "fast")
            vapostproc.set_property("qos", True)
            vapostproc_caps = _Gst.caps_from_string("video/x-raw(memory:VAMemory)")
            vapostproc_caps.set_value("format", "NV12")
            vapostproc_capsfilter = _Gst.ElementFactory.make("capsfilter")
            vapostproc_capsfilter.set_property("caps", vapostproc_caps)

            # encoder
            if self.gpu_id > 0:
                vavp9enc = _Gst.ElementFactory.make("varenderD{}vp9enc".format(128 + self.gpu_id), "vaenc")
                if vavp9enc is None:
                    vavp9enc = _Gst.ElementFactory.make("varenderD{}vp9lpenc".format(128 + self.gpu_id), "vaenc")
            else:
                vavp9enc = _Gst.ElementFactory.make("vavp9enc", "vaenc")
                if vavp9enc is None:
                    vavp9enc = _Gst.ElementFactory.make("vavp9lpenc", "vaenc")
            # Set VBV/HRD buffer size (kbits) to optimize for live streaming
            vavp9enc.set_property("cpb-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multipliers['va']))
            vavp9enc.set_property("hierarchical-level", 1)
            vavp9enc.set_property("key-int-max", 1024 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            vavp9enc.set_property("mbbrc", "disabled")
            vavp9enc.set_property("ref-frames", 1)
            vavp9enc.set_property("rate-control", "cbr")
            vavp9enc.set_property("target-usage", 6)
            vavp9enc.set_property("bitrate", self.fec_video_bitrate)

        elif self.encoder in ["vaav1enc"]:
            # colorspace conversion
            if self.gpu_id > 0:
                vapostproc = _Gst.ElementFactory.make("varenderD{}postproc".format(128 + self.gpu_id), "vapostproc")
            else:
                vapostproc = _Gst.ElementFactory.make("vapostproc")
            vapostproc.set_property("scale-method", "fast")
            vapostproc.set_property("qos", True)
            vapostproc_caps = _Gst.caps_from_string("video/x-raw(memory:VAMemory)")
            vapostproc_caps.set_value("format", "NV12")
            vapostproc_capsfilter = _Gst.ElementFactory.make("capsfilter")
            vapostproc_capsfilter.set_property("caps", vapostproc_caps)

            # encoder
            if self.gpu_id > 0:
                vaav1enc = _Gst.ElementFactory.make("varenderD{}av1enc".format(128 + self.gpu_id), "vaenc")
                if vaav1enc is None:
                    vaav1enc = _Gst.ElementFactory.make("varenderD{}av1lpenc".format(128 + self.gpu_id), "vaenc")
            else:
                vaav1enc = _Gst.ElementFactory.make("vaav1enc", "vaenc")
                if vaav1enc is None:
                    vaav1enc = _Gst.ElementFactory.make("vaav1lpenc", "vaenc")
            # Set VBV/HRD buffer size (kbits) to optimize for live streaming
            vaav1enc.set_property("cpb-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multipliers['va']))
            vaav1enc.set_property("hierarchical-level", 1)
            vaav1enc.set_property("key-int-max", 1024 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            vaav1enc.set_property("mbbrc", "disabled")
            vaav1enc.set_property("ref-frames", 1)
            vaav1enc.set_property("tile-groups", 16)
            vaav1enc.set_property("rate-control", "cbr")
            vaav1enc.set_property("target-usage", 6)
            vaav1enc.set_property("bitrate", self.fec_video_bitrate)

        elif self.encoder in ["x264enc"]:
            # Videoconvert for colorspace conversion
            videoconvert = _Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = _Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "NV12")
            videoconvert_capsfilter = _Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            # encoder
            x264enc = _Gst.ElementFactory.make("x264enc", "x264enc")
            # Chromium has issues with more than four encoding slices
            x264enc.set_property("threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            x264enc.set_property("aud", False)
            x264enc.set_property("b-adapt", False)
            x264enc.set_property("bframes", 0)
            x264enc.set_property("dct8x8", False)
            x264enc.set_property("insert-vui", True)
            x264enc.set_property("key-int-max", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            x264enc.set_property("mb-tree", False)
            x264enc.set_property("rc-lookahead", 0)
            x264enc.set_property("sync-lookahead", 0)
            # Set VBV/HRD buffer size (milliseconds) to optimize for live streaming
            x264enc.set_property("vbv-buf-capacity", int((1000 + self.framerate - 1) // self.framerate * self.vbv_multipliers['sw']))
            x264enc.set_property("sliced-threads", True)
            x264enc.set_property("byte-stream", True)
            x264enc.set_property("pass", "cbr")
            x264enc.set_property("speed-preset", "ultrafast")
            x264enc.set_property("tune", "zerolatency")
            x264enc.set_property("bitrate", self.fec_video_bitrate)

        elif self.encoder in ["openh264enc"]:
            # Videoconvert for colorspace conversion
            videoconvert = _Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = _Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = _Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            # encoder
            openh264enc = _Gst.ElementFactory.make("openh264enc", "openh264enc")
            openh264enc.set_property("adaptive-quantization", False)
            openh264enc.set_property("background-detection", False)
            openh264enc.set_property("enable-frame-skip", False)
            openh264enc.set_property("scene-change-detection", False)
            openh264enc.set_property("usage-type", "screen")
            openh264enc.set_property("complexity", "low")
            openh264enc.set_property("gop-size", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            openh264enc.set_property("multi-thread", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            openh264enc.set_property("slice-mode", "n-slices")
            # Chromium has issues with more than four encoding slices
            openh264enc.set_property("num-slices", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            openh264enc.set_property("rate-control", "bitrate")
            openh264enc.set_property("bitrate", self.fec_video_bitrate * 1000)

        elif self.encoder in ["x265enc"]:
            # Videoconvert for colorspace conversion
            videoconvert = _Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = _Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = _Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            # encoder
            x265enc = _Gst.ElementFactory.make("x265enc", "x265enc")
            x265enc.set_property("option-string", "b-adapt=0:bframes=0:rc-lookahead=0:repeat-headers:pmode:wpp")
            x265enc.set_property("key-int-max", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            x265enc.set_property("speed-preset", "ultrafast")
            x265enc.set_property("tune", "zerolatency")
            x265enc.set_property("bitrate", self.fec_video_bitrate)

        elif self.encoder in ["vp8enc", "vp9enc"]:
            videoconvert = _Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = _Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = _Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            if self.encoder == "vp8enc":
                vpenc = _Gst.ElementFactory.make("vp8enc", "vpenc")

            elif self.encoder == "vp9enc":
                vpenc = _Gst.ElementFactory.make("vp9enc", "vpenc")
                vpenc.set_property("frame-parallel-decoding", True)
                vpenc.set_property("row-mt", True)

            # VPX Parameters
            vpenc.set_property("threads", min(16, max(1, len(os.sched_getaffinity(0)) - 1)))
            # Set VBV/HRD buffer size (milliseconds) to optimize for live streaming
            vbv_buffer_size = int((1000 + self.framerate - 1) // self.framerate * self.vbv_multipliers['vp'])
            vpenc.set_property("buffer-initial-size", vbv_buffer_size)
            vpenc.set_property("buffer-optimal-size", vbv_buffer_size)
            vpenc.set_property("buffer-size", vbv_buffer_size)
            vpenc.set_property("cpu-used", -16)
            vpenc.set_property("deadline", 1)
            vpenc.set_property("end-usage", "cbr")
            vpenc.set_property("error-resilient", "default")
            vpenc.set_property("keyframe-mode", "disabled")
            vpenc.set_property("keyframe-max-dist", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            vpenc.set_property("lag-in-frames", 0)
            vpenc.set_property("max-intra-bitrate", 250)
            vpenc.set_property("multipass-mode", "first-pass")
            vpenc.set_property("overshoot", 10)
            vpenc.set_property("undershoot", 25)
            vpenc.set_property("static-threshold", 0)
            vpenc.set_property("tuning", "psnr")
            vpenc.set_property("target-bitrate", self.fec_video_bitrate * 1000)

        elif self.encoder in ["svtav1enc"]:
            videoconvert = _Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = _Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = _Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            svtav1enc = _Gst.ElementFactory.make("svtav1enc", "svtav1enc")
            svtav1enc.set_property("intra-period-length", -1 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            # svtav1enc.set_property("maximum-buffer-size", 150)
            svtav1enc.set_property("preset", 10)
            svtav1enc.set_property("logical-processors", min(24, max(1, len(os.sched_getaffinity(0)) - 1)))
            svtav1enc.set_property("parameters-string", "rc=2:fast-decode=1:buf-initial-sz=100:buf-optimal-sz=120:maxsection-pct=250:lookahead=0:pred-struct=1")
            svtav1enc.set_property("target-bitrate", self.fec_video_bitrate)

        elif self.encoder in ["av1enc"]:
            videoconvert = _Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = _Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = _Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            av1enc = _Gst.ElementFactory.make("av1enc", "av1enc")
            # av1enc.set_property("buf-initial-sz", 100)
            # av1enc.set_property("buf-optimal-sz", 120)
            # av1enc.set_property("buf-sz", 150)
            av1enc.set_property("cpu-used", 10)
            av1enc.set_property("end-usage", "cbr")
            av1enc.set_property("keyframe-max-dist", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            av1enc.set_property("lag-in-frames", 0)
            av1enc.set_property("overshoot-pct", 10)
            av1enc.set_property("row-mt", True)
            av1enc.set_property("usage-profile", "realtime")
            av1enc.set_property("tile-columns", 2)
            av1enc.set_property("tile-rows", 2)
            av1enc.set_property("threads", min(24, max(1, len(os.sched_getaffinity(0)) - 1)))
            av1enc.set_property("target-bitrate", self.fec_video_bitrate)

        elif self.encoder in ["rav1enc"]:
            videoconvert = _Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = _Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = _Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            rav1enc = _Gst.ElementFactory.make("rav1enc", "rav1enc")
            rav1enc.set_property("low-latency", True)
            rav1enc.set_property("max-key-frame-interval", 715827882 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            rav1enc.set_property("rdo-lookahead-frames", 0)
            rav1enc.set_property("reservoir-frame-delay", 12)
            rav1enc.set_property("speed-preset", 10)
            rav1enc.set_property("tiles", 16)
            rav1enc.set_property("threads", min(24, max(1, len(os.sched_getaffinity(0)) - 1)))
            rav1enc.set_property("bitrate", self.fec_video_bitrate * 1000)

        else:
            raise MediaPipelineError("Unsupported encoder for pipeline: %s" % self.encoder)

        if "h264" in self.encoder or "x264" in self.encoder:
            # Set the capabilities for the H.264 codec.
            h264enc_caps = _Gst.caps_from_string("video/x-h264")

            # Sets the H.264 encoding profile to one compatible with WebRTC.
            # Main profile includes CABAC and is compatible with Chrome.
            # In low-latency encoding, High profile features are not utilized.
            # Browsers only support specific H.264 profiles and they are
            # coded in the RTP payload type set by the rtph264pay_caps below.
            h264enc_caps.set_value("profile", "main")

            # Stream-oriented H.264 codec
            h264enc_caps.set_value("stream-format", "byte-stream")

            # Create a capability filter for the h264enc_caps.
            h264enc_capsfilter = _Gst.ElementFactory.make("capsfilter")
            h264enc_capsfilter.set_property("caps", h264enc_caps)

        elif "h265" in self.encoder or "x265" in self.encoder:
            h265enc_caps = _Gst.caps_from_string("video/x-h265")
            h265enc_caps.set_value("profile", "main")
            h265enc_caps.set_value("stream-format", "byte-stream")
            h265enc_capsfilter = _Gst.ElementFactory.make("capsfilter")
            h265enc_capsfilter.set_property("caps", h265enc_caps)

        elif "vp8" in self.encoder:
            vpenc_caps = _Gst.caps_from_string("video/x-vp8")
            vpenc_capsfilter = _Gst.ElementFactory.make("capsfilter")
            vpenc_capsfilter.set_property("caps", vpenc_caps)

        elif "vp9" in self.encoder:
            vpenc_caps = _Gst.caps_from_string("video/x-vp9")
            vpenc_capsfilter = _Gst.ElementFactory.make("capsfilter")
            vpenc_capsfilter.set_property("caps", vpenc_caps)

        elif "av1" in self.encoder:
            av1enc_caps = _Gst.caps_from_string("video/x-av1")
            av1enc_caps.set_value("parsed", True)
            av1enc_caps.set_value("stream-format", "obu-stream")
            av1enc_capsfilter = _Gst.ElementFactory.make("capsfilter")
            av1enc_capsfilter.set_property("caps", av1enc_caps)

        # Add all elements to the pipeline.
        pipeline_elements = [self.ximagesrc, self.ximagesrc_capsfilter]

        # ADD_ENCODER: add new encoder elements to this list
        if self.encoder in ["nvh264enc"]:
            pipeline_elements += [cudaupload, cudaconvert, cudaconvert_capsfilter, nvh264enc, h264enc_capsfilter]

        elif self.encoder in ["nvh265enc"]:
            pipeline_elements += [cudaupload, cudaconvert, cudaconvert_capsfilter, nvh265enc, h265enc_capsfilter]

        elif self.encoder in ["nvav1enc"]:
            pipeline_elements += [cudaupload, cudaconvert, cudaconvert_capsfilter, nvav1enc, av1enc_capsfilter]

        elif self.encoder in ["vah264enc"]:
            pipeline_elements += [vapostproc, vapostproc_capsfilter, vah264enc, h264enc_capsfilter]

        elif self.encoder in ["vah265enc"]:
            pipeline_elements += [vapostproc, vapostproc_capsfilter, vah265enc, h265enc_capsfilter]

        elif self.encoder in ["vavp9enc"]:
            pipeline_elements += [vapostproc, vapostproc_capsfilter, vavp9enc, vpenc_capsfilter]

        elif self.encoder in ["vaav1enc"]:
            pipeline_elements += [vapostproc, vapostproc_capsfilter, vaav1enc, av1enc_capsfilter]

        elif self.encoder in ["x264enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, x264enc, h264enc_capsfilter]

        elif self.encoder in ["openh264enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, openh264enc, h264enc_capsfilter]

        elif self.encoder in ["x265enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, x265enc, h265enc_capsfilter]

        elif self.encoder in ["vp8enc", "vp9enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, vpenc, vpenc_capsfilter]

        elif self.encoder in ["svtav1enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, svtav1enc, av1enc_capsfilter]

        elif self.encoder in ["av1enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, av1enc, av1enc_capsfilter]

        elif self.encoder in ["rav1enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, rav1enc, av1enc_capsfilter]

        for pipeline_element in pipeline_elements:
            self.pipeline.add(pipeline_element)

        # Link the pipeline elements and raise exception of linking failures
        # due to incompatible element pad capabilities.
        appsink_video = self.pipeline.get_by_name("appsink_video")
        if not appsink_video:
            raise MediaPipelineError("Failed to find appsink_video element in the pipeline")
        pipeline_elements += [appsink_video]
        for i in range(len(pipeline_elements) - 1):
            if not _Gst.Element.link(pipeline_elements[i], pipeline_elements[i + 1]):
                raise MediaPipelineError("Failed to link {} -> {}".format(pipeline_elements[i].get_name(), pipeline_elements[i + 1].get_name()))

    def _build_audio_pipeline(self):
        """Adds the RTP audio stream to the pipeline.
        """

        # Create element for receiving audio from pulseaudio.
        pulsesrc = _Gst.ElementFactory.make("pulsesrc", "pulsesrc")

        # Let the audio source provide the global clock.
        # This is important when trying to keep the audio and video
        # jitter buffers in sync. If there is skew between the video and audio
        # buffers, features like NetEQ will continuously increase the size of the
        # jitter buffer to catch up and will never recover.
        pulsesrc.set_property("provide-clock", True)

        # Apply stream time to buffers, this helps with pipeline synchronization.
        # Disabled by default because pulsesrc should not be re-timestamped with the current stream time when pushed out to the GStreamer pipeline and destroy the original synchronization.
        pulsesrc.set_property("do-timestamp", False)

        # Maximum and minimum amount of data to read in each iteration in microseconds
        pulsesrc.set_property("buffer-time", 100000)
        pulsesrc.set_property("latency-time", 10000)

        # Create capabilities for pulsesrc and set channels
        pulsesrc_caps = _Gst.caps_from_string("audio/x-raw")
        pulsesrc_caps.set_value("channels", self.audio_channels)

        # Create a capability filter for the pulsesrc_caps
        pulsesrc_capsfilter = _Gst.ElementFactory.make("capsfilter")
        pulsesrc_capsfilter.set_property("caps", pulsesrc_caps)

        # Encode the raw PulseAudio stream to the Opus format which is
        # the default packetized streaming format for the web
        opusenc = _Gst.ElementFactory.make("opusenc", "opusenc")

        # Low-latency and high-quality configurations
        opusenc.set_property("audio-type", "restricted-lowdelay")
        opusenc.set_property("bandwidth", "fullband")
        opusenc.set_property("bitrate-type", "cbr")
        # OPUS_FRAME: Modify all locations with "OPUS_FRAME:"
        # Browser-side SDP munging ("minptime=3"/"minptime=5") is required if frame-size < 10
        opusenc.set_property("frame-size", "10")
        opusenc.set_property("perfect-timestamp", True)
        opusenc.set_property("max-payload-size", 4000)
        # In-band FEC in Opus
        opusenc.set_property("inband-fec", self.audio_packetloss_percent > 0)
        opusenc.set_property("packet-loss-percentage", self.audio_packetloss_percent)

        # Set audio bitrate
        # This can be dynamically changed using set_audio_bitrate()
        opusenc.set_property("bitrate", self.audio_bitrate)

        # Add all elements to the pipeline.
        pipeline_elements = [pulsesrc, pulsesrc_capsfilter, opusenc]

        for pipeline_element in pipeline_elements:
            self.pipeline.add(pipeline_element)

        # Link the pipeline elements and raise exception of linking fails
        # due to incompatible element pad capabilities.
        appsink_audio = self.pipeline.get_by_name("appsink_audio")
        if not appsink_audio:
            raise MediaPipelineError("Failed to find appsink_audio element in the pipeline")
        pipeline_elements += [appsink_audio]
        for i in range(len(pipeline_elements) - 1):
            if not _Gst.Element.link(pipeline_elements[i], pipeline_elements[i + 1]):
                raise MediaPipelineError("Failed to link {} -> {}".format(pipeline_elements[i].get_name(), pipeline_elements[i + 1].get_name()))


    def check_plugins(self):
        """Validate required GStreamer plugins are available"""
        required = ["opus", "app", "ximagesrc"]

        # Encoder-specific requirements
        encoder_requirements = {
            "nv": ["nvcodec"],
            "va": ["va"],
            "x264": ["x264"],
            "openh264": ["openh264"],
            "x265": ["x265"],
            "vp": ["vpx"],
            "svtav1": ["svtav1"],
            "aom": ["aom"],
            "rav1e": ["rav1e"],
            "av1": ["rsrtp"]
        }

        # Add encoder-specific requirements
        for prefix, plugins in encoder_requirements.items():
            if self.encoder.startswith(prefix):
                required.extend(plugins)

        # Check for missing plugins
        missing = [p for p in required if not _Gst.Registry.get().find_plugin(p)]
        if missing:
            raise MediaPipelineError(f"Missing required plugins: {', '.join(missing)}")

    async def set_framerate(self, framerate: int):
        """Set pipeline framerate in fps

        Arguments:
            framerate {integer} -- framerate in frames per second, for example, 15, 30, 60.
        """
        if not self.pipeline:
            return

        self.framerate = framerate
        # ADD_ENCODER: GOP/IDR Keyframe distance to keep the stream from freezing (in keyframe_dist seconds) and set vbv-buffer-size
        self.keyframe_frame_distance = -1 if self.keyframe_distance == -1.0 else max(self.min_keyframe_frame_distance, int(self.framerate * self.keyframe_distance))
        if self.encoder.startswith("nv"):
            element = _Gst.Bin.get_by_name(self.pipeline, "nvenc")
            element.set_property("gop-size", -1 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            element.set_property("vbv-buffer-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multipliers['nv']))
        elif self.encoder.startswith("va"):
            element = _Gst.Bin.get_by_name(self.pipeline, "vaenc")
            element.set_property("key-int-max", 1024 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            element.set_property("cpb-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multipliers['va']))
        elif self.encoder in ["x264enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "x264enc")
            element.set_property("key-int-max", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            element.set_property("vbv-buf-capacity", int((1000 + self.framerate - 1) // self.framerate * self.vbv_multipliers['sw']))
        elif self.encoder in ["openh264enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "openh264enc")
            element.set_property("gop-size", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
        elif self.encoder in ["x265enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "x265enc")
            element.set_property("key-int-max", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
        elif self.encoder.startswith("vp"):
            element = _Gst.Bin.get_by_name(self.pipeline, "vpenc")
            element.set_property("keyframe-max-dist", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            vbv_buffer_size = int((1000 + self.framerate - 1) // self.framerate * self.vbv_multipliers['vp'])
            element.set_property("buffer-initial-size", vbv_buffer_size)
            element.set_property("buffer-optimal-size", vbv_buffer_size)
            element.set_property("buffer-size", vbv_buffer_size)
        elif self.encoder in ["svtav1enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "svtav1enc")
            element.set_property("intra-period-length", -1 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
        elif self.encoder in ["av1enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "av1enc")
            element.set_property("keyframe-max-dist", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
        elif self.encoder in ["rav1enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "rav1enc")
            element.set_property("max-key-frame-interval", 715827882 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
        else:
            logger.warning(f"Setting keyframe interval (GOP size) not supported with encoder: {self.encoder}")

        self.ximagesrc_caps = _Gst.caps_from_string("video/x-raw")
        self.ximagesrc_caps.set_value("framerate", _Gst.Fraction(self.framerate, 1))
        self.ximagesrc_capsfilter.set_property("caps", self.ximagesrc_caps)
        logger.info(f"Framerate set to: {self.framerate}")

    async def set_video_bitrate(self, bitrate: int):
        """
        Set video encoder target bitrate.

        Arguments:
            bitrate {integer} -- target bitrate in mbps
        """

        if not self.pipeline:
            return

        bitrate = int(bitrate) * 1000 # Convert to kbps
        # Prevent bitrate from overshooting because of FEC
        fec_bitrate = int(bitrate / (1.0 + (self.video_packetloss_percent / 100.0)))

        # ADD_ENCODER: add new encoder to this list and set vbv-buffer-size if unit is bytes instead of milliseconds
        if self.encoder.startswith("nv"):
            element = _Gst.Bin.get_by_name(self.pipeline, "nvenc")
            element.set_property("vbv-buffer-size", int((fec_bitrate + self.framerate - 1) // self.framerate * self.vbv_multipliers['nv']))
            element.set_property("bitrate", fec_bitrate)
        elif self.encoder.startswith("va"):
            element = _Gst.Bin.get_by_name(self.pipeline, "vaenc")
            element.set_property("cpb-size", int((fec_bitrate + self.framerate - 1) // self.framerate * self.vbv_multipliers['va']))
            element.set_property("bitrate", fec_bitrate)
        elif self.encoder in ["x264enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "x264enc")
            element.set_property("bitrate", fec_bitrate)
        elif self.encoder in ["openh264enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "openh264enc")
            element.set_property("bitrate", fec_bitrate * 1000)
        elif self.encoder in ["x265enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "x265enc")
            element.set_property("bitrate", fec_bitrate)
        elif self.encoder in ["vp8enc", "vp9enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "vpenc")
            element.set_property("target-bitrate", fec_bitrate * 1000)
        elif self.encoder in ["svtav1enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "svtav1enc")
            element.set_property("target-bitrate", fec_bitrate)
        elif self.encoder in ["av1enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "av1enc")
            element.set_property("target-bitrate", fec_bitrate)
        elif self.encoder in ["rav1enc"]:
            element = _Gst.Bin.get_by_name(self.pipeline, "rav1enc")
            element.set_property("bitrate", fec_bitrate * 1000)
        else:
            logger.warning(f"set_video_bitrate not supported with encoder: {self.encoder}")

        logger.info(f"video bitrate set to: {bitrate}")
        self.video_bitrate = bitrate
        self.fec_video_bitrate = fec_bitrate
        self.send_data_channel_message("pipeline", {"status": f"Video bitrate set to: {bitrate}"})

    async def set_audio_bitrate(self, bitrate: int):
        """Set Opus encoder target bitrate in bps"""

        if not self.pipeline:
            return

        # Keep audio bitrate to exact value and increase effective bitrate after FEC to prevent audio quality degradation
        fec_bitrate = int(bitrate * (1.0 + (self.audio_packetloss_percent / 100.0)))
        element = _Gst.Bin.get_by_name(self.pipeline, "opusenc")
        element.set_property("bitrate", bitrate)

        self.audio_bitrate = bitrate
        self.fec_audio_bitrate = fec_bitrate

        logger.info(f"audio bitrate set to: {bitrate}")
        self.send_data_channel_message("pipeline", {"status": f"Audio bitrate set to: {bitrate}"})

    async def set_pointer_visible(self, visible: bool):
        """Set pointer visibility on the ximagesrc element"""

        element = _Gst.Bin.get_by_name(self.pipeline, "x11")
        element.set_property("show-pointer", visible)
        self.send_data_channel_message(
            "pipeline", {"status": "Set pointer visibility to: %d" % visible})

    # --- Core Pipeline Management ---
    async def start_media_pipeline(self):
        """Starts the Media pipeline asynchronously"""
        logger.info("Starting media pipeline")

        try:
            self.pipeline = _Gst.Pipeline.new()
            if not self.pipeline:
                raise MediaPipelineError("Failed to create media pipeline")

            self._create_app_sinks()
            self._build_video_pipeline()
            self._build_audio_pipeline()

            # Start pipeline asynchronously
            await self._start_pipeline_async()

            # Start bus monitoring task
            self._running = True
            self._bus_task = asyncio.create_task(self._monitor_bus())

            logger.info("Pipeline started successfully")
        except Exception as e:
            logger.error(f"Failed to start pipeline: {e}")
            await self.stop_media_pipeline()
            raise

    async def _start_pipeline_async(self):
        """Start pipeline asynchronously with proper state handling"""
        # Transition pipeline to PLAYING state
        res = await asyncio.to_thread(self.pipeline.set_state, _Gst.State.PLAYING)

        if res == _Gst.StateChangeReturn.ASYNC:
            logger.debug(f"Waiting for the media pipeline state change to SUCCESS")
            # Wait for state change to complete
            while res != _Gst.StateChangeReturn.SUCCESS:
                res, _, _ = await asyncio.to_thread(
                    self.pipeline.get_state, _Gst.CLOCK_TIME_NONE
                )
                if res == _Gst.StateChangeReturn.FAILURE:
                    raise MediaPipelineError(f"Pipeline state change result: {res.value_nick}")
                await asyncio.sleep(2)

        if res != _Gst.StateChangeReturn.SUCCESS:
            raise MediaPipelineError(f"Failed to transition pipeline to PLAYING: {res}")

    async def stop_media_pipeline(self):
        logger.info("Stopping pipeline")
        async with self.async_lock:
            self._running = False
            if self._bus_task and not self._bus_task.done():
                self._bus_task.cancel()
                try:
                    await self._bus_task
                except asyncio.CancelledError:
                    pass
                self._bus_task = None

            if self.pipeline:
                logger.info("Setting pipeline state to NULL")
                await asyncio.to_thread(self.pipeline.set_state, _Gst.State.NULL)
                self.pipeline = None
                logger.info("Pipeline stopped")

                # # Clean up GStreamer resources when pipeline is stopped
                # await self._cleanup_gstreamer_resources()

    async def _cleanup_gstreamer_resources(self):
        """Clean up GStreamer resources asynchronously"""
        try:
            await asyncio.to_thread(_cleanup_gstreamer)
            logger.info("GStreamer resources cleaned up")
        except Exception as e:
            logger.warning(f"Error during GStreamer cleanup: {e}")

    async def _monitor_bus(self):
        """Monitor GStreamer bus asynchronously"""
        while self._running and self.pipeline:
            bus = await asyncio.to_thread(self.pipeline.get_bus)

            if bus and await asyncio.to_thread(bus.have_pending):
                msg = await asyncio.to_thread(bus.pop)
                if not await self._handle_bus_message(msg):
                    # Critical error, stop pipeline
                    await self.stop_pipeline()
                    return
            await asyncio.sleep(0.1)

    async def _handle_bus_message(self, message):
        if message is None:
            return True

        t = message.type
        if t == _Gst.MessageType.EOS:
            logger.error("End-of-stream")
            return False
        elif t == _Gst.MessageType.ERROR:
            err, debug = message.parse_error()
            logger.error(f"Pipeline error: {err}: {debug}")
            return False
        elif t == _Gst.MessageType.STATE_CHANGED:
            if isinstance(message.src, _Gst.Pipeline):
                old_state, new_state, pending_state = message.parse_state_changed()
                logger.info(f"Pipeline state changed from {old_state.value_nick} to {new_state.value_nick}")
                if old_state == _Gst.State.PAUSED and new_state == _Gst.State.READY:
                    logger.info("stopping bus message task")
                    return False
        elif t == _Gst.MessageType.LATENCY:
            await asyncio.to_thread(self.pipeline.set_latency, 0)
        return True

class MediaPipelinePixel(MediaPipeline):
    def __init__(
        self,
        async_event_loop: asyncio.AbstractEventLoop,
        encoder: str,
        framerate: int = 30,
        video_bitrate: int = 2000,
        audio_bitrate: int = 128000,
        width: int = 1920,
        height: int = 1080,
        audio_channels: int = 2,
        audio_enabled: bool = True,
        audio_device_name = 'output.monitor'
    ):
        self.async_event_loop = async_event_loop
        self.audio_channels = audio_channels
        self.encoder = encoder
        self.framerate = framerate
        self.video_bitrate = video_bitrate
        self.audio_bitrate = audio_bitrate
        self.last_resize_success = True
        self.width = width
        self.height = height
        self.audio_enabled = audio_enabled
        self.audio_device_name = audio_device_name
        self.capture_cursor = False
        self.produce_data = lambda buf, pts, kind: logger.warning('unhandled produce_data')
        self.send_data_channel_message = lambda msg: logger.warning('unhandled send_data_channel_message')

        self.capture_module = None
        self.pcmflux_module = None
        self._is_screen_capturing = False
        self._is_pcmflux_capturing = False
        self._running = False
        self.async_lock = asyncio.Lock()

    async def set_pointer_visible(self, visible: bool):
        """To enable capturing the cursor from pixeflux.
        
        :visible: set True to enable
        """
        if not self._is_screen_capturing or self.capture_module is None:
            return

        if self.capture_cursor == visible:
            return 

        self.capture_cursor = visible
        await self.restart_screen_capture()
        logger.info(f"Set pointer visibility to: {visible}")

    async def set_video_bitrate(self, new_bitrate: int):
        """Set video encoder target bitrate.

        :new_bitrate: bitrate in mbps
        """
        if not self._is_screen_capturing or self.capture_module is None:
            return

        new_bitrate *= 1000   # convert to kpbs
        if new_bitrate <= 0 or self.video_bitrate == new_bitrate:
            return

        try:
            await self.async_event_loop.run_in_executor(None, self.capture_module.update_video_bitrate, new_bitrate)
            logger.info(f"Updated video bitrate: {self.video_bitrate} -> {new_bitrate}")
            self.video_bitrate = new_bitrate
        except AttributeError:
            logger.error("Video capture module does not support video bitrate updation")
        except Exception as e:
            logger.info(f"Error updating video bitrate {e}", exc_info=True)

    async def set_audio_bitrate(self, new_bitrate: int):
        """Set audio encoder target bitrate.

        :new_bitrate: bitrate in kbps
        """
        if not self._is_pcmflux_capturing or self.pcmflux_module is None:
            return

        if  new_bitrate <= 0 or self.audio_bitrate == new_bitrate:
            return

        try:
            await self.async_event_loop.run_in_executor(None, self.pcmflux_module.update_audio_bitrate, new_bitrate)
            logger.info(f"Updated audio bitrate: {self.audio_bitrate // 1000} -> {new_bitrate // 1000} kbps")
            self.audio_bitrate = new_bitrate
        except AttributeError:
            logger.error("Audio capture module does not support audio bitrate updation")
        except Exception as e:
            logger.info(f"Error updating audio bitrate {e}", exc_info=True)

    async def set_framerate(self, framerate: int):
        """Set pixelflux capture rate in fps .

        :framerate: framerate in frames per second, for example, 15, 30, 60.
        """
        async with self.async_lock:
            if not self._is_screen_capturing:
                return

            if framerate <= 0 or self.framerate == framerate:
                return

            self.framerate = framerate
            await self.async_event_loop.run_in_executor(None, self.capture_module.update_framerate, float(self.framerate))
            logger.info(f"Updated framerate to: {self.framerate}")

    async def dynamic_idr_frame(self):
        """Requests an IDR frame from pixelflux"""
        if not self._is_screen_capturing or self.capture_module is None:
            return
        try:
            await self.async_event_loop.run_in_executor(None, self.capture_module.request_idr_frame)
            logger.info("IDR frame requested successfully")
        except AttributeError:
            logger.error("ScreenCapture module does not support IDR frame request")
        except Exception as e:
            logger.error(f"Error requesting IDR frame: {e}", exc_info=True)

    def generate_capture_settings(self):
        """Generates configuration for pixelflux screen capturing"""
        cs = CaptureSettings()
        cs.capture_width = self.width
        cs.capture_height = self.height
        cs.capture_x = 0
        cs.capture_y = 0
        cs.target_fps = float(self.framerate)
        cs.capture_cursor = self.capture_cursor
        cs.output_mode = 1

        if self.encoder in ["nvh264enc", "x264enc"]:
            cs.h264_streaming_mode = True
            cs.h264_fullframe = True
            cs.h264_crf = 23
            cs.h264_cbr_mode = True
            cs.h264_bitrate_kbps = self.video_bitrate
            cs.vaapi_render_node_index = -1   #
            if self.encoder == "x264enc":
                cs.use_cpu = True

        cs.auto_adjust_screen_capture_size = True
        return cs

    async def start_screen_capture(self):
        if self._is_screen_capturing:
            return

        settings = self.generate_capture_settings()
        def screen_capture_callback(result_ptr, user_data):
            if not result_ptr:
                return
            try:
                result = result_ptr.contents
                if result.size > 0:
                    data_bytes = bytes(result.data[10:result.size])
                    if not hasattr(result, "frame_id"):
                        logger.error(f"frame_id from callback is empty: {result.frame_id}")
                    else:
                        # Generate pts from frame_id
                        pts_step = 90000 // self.framerate
                        pts = result.frame_id * pts_step
                        asyncio.run_coroutine_threadsafe(self.produce_data(data_bytes, pts, "video"), self.async_event_loop)

            except Exception as e:
                logger.error(f"Error in capture callback: {e}", exc_info=False)

        try:
            self.capture_module = ScreenCapture()
            await self.async_event_loop.run_in_executor(None, self.capture_module.start_capture, settings, screen_capture_callback)
            self._is_screen_capturing = True
            logger.info("Started screen capture module")
        except Exception as e:
            logger.error(f"Failed to start screen capture: {e}", exc_info=True)
            self.capture_module = None
            self._is_screen_capturing = False


    async def stop_screen_capture(self):
        if not self._is_screen_capturing or self.capture_module is None:
            return
        try:
            await self.async_event_loop.run_in_executor(None, self.capture_module.stop_capture)
            self.capture_module = None
            self._is_screen_capturing = False
            logger.info("Stopped screen capture module")
        except Exception as e:
            logger.error(f"Error stopping screen capture: {e}", exc_info=True)
            self.capture_module = None
            self._is_screen_capturing = False

    async def restart_screen_capture(self):
        if not self._is_screen_capturing:
            return

        async with self.async_lock:
            try:
                await self.stop_screen_capture()
                await self.start_screen_capture()
                logger.info("Screen capture restarted successfully")
            except Exception as e:
                logger.error(f"Error restarting screen capture: {e}")

    async def _start_audio_pipeline(self):
        if self._is_pcmflux_capturing:
            return

        logger.info("Starting pcmflux audio pipeline...")
        try:
            capture_settings = AudioCaptureSettings()
            device_name_bytes = self.audio_device_name.encode('utf-8') if self.audio_device_name else None
            capture_settings.device_name = device_name_bytes
            capture_settings.sample_rate = 48000
            capture_settings.channels = self.audio_channels
            capture_settings.opus_bitrate = int(self.audio_bitrate)
            capture_settings.frame_duration_ms = 20
            capture_settings.use_vbr = False
            capture_settings.use_silence_gate = False
            capture_settings.debug_logging = False
            pcmflux_settings = capture_settings

            logger.info(f"pcmflux settings: device='{self.audio_device_name}', "
                        f"bitrate={capture_settings.opus_bitrate}, channels={capture_settings.channels}")

            def audio_capture_callback(result_ptr, user_data):
                if not result_ptr:
                    return
                try:
                    result = result_ptr.contents
                    if result.data and result.size > 0:
                        data_bytes = bytes(ctypes.cast(
                            result.data, ctypes.POINTER(ctypes.c_ubyte * result.size)
                        ).contents)

                        asyncio.run_coroutine_threadsafe(self.produce_data(data_bytes, result.pts, "audio"), self.async_event_loop)
                except Exception as e:
                    logger.info(f"Error audio capture callback: {e}")

            pcmflux_callback = AudioChunkCallback(audio_capture_callback)
            self.pcmflux_module = AudioCapture()
            await self.async_event_loop.run_in_executor(None, self.pcmflux_module.start_capture, pcmflux_settings, pcmflux_callback)
            self._is_pcmflux_capturing = True
            logger.info("pcmflux audio capture started successfully.")
        except Exception as e:
            logger.error(f"Failed to start pcmflux audio pipeline: {e}", exc_info=True)
            await self._stop_audio_pipeline()
            return

    async def _stop_audio_pipeline(self):
        if not self._is_pcmflux_capturing or not self.pcmflux_module:
            return

        logger.info("Stopping pcmflux audio pipeline...")
        self._is_pcmflux_capturing = False
        if self.pcmflux_module:
            try:
                await self.async_event_loop.run_in_executor(None, self.pcmflux_module.stop_capture)
            except Exception as e:
                logger.error(f"Error during pcmflux stop_capture: {e}")
            finally:
                self.pcmflux_module = None

            logger.info("pcmflux audio pipeline stopped.")
        return

    async def start_media_pipeline(self):
        async with self.async_lock:
            logger.info("Starting media pipeline...")
            try:
                await self.start_screen_capture()

                if self.audio_enabled:
                    await self._start_audio_pipeline()
                self._running = True
            except Exception as e:
                logger.error(f"Error starting media pipelines: {e}", exc_info=True)
                await self.stop_media_pipeline()

    async def stop_media_pipeline(self):
        async with self.async_lock:
            logger.info("Stopping media pipeline...")
            try:
                await self.stop_screen_capture()

                if self.audio_enabled:
                    await self._stop_audio_pipeline()
                self._running = False
            except Exception as e:
                logger.error(f"Error stopping media pipelines: {e}", exc_info=True)

