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
import base64
import json
import logging
import os
import re
import sys
import time

logger = logging.getLogger("media_pipeline")
logger.setLevel(logging.INFO)

try:
    import gi
    gi.require_version('Gst', "1.0")
    gi.require_version('GstRtp', "1.0")
    gi.require_version('GstVideo', "1.0")
    gi.require_version("GstApp", "1.0")
    from gi.repository import Gst, GstRtp, GstVideo, GstApp
    fract = Gst.Fraction(60, 1)
    del fract
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
    sys.exit(1)
logger.info("GStreamer-Python install looks OK")

class MediaPipelineError(Exception):
    pass

class MediaPipeline:
    def __init__(self, async_event_loop, audio_channels=2, framerate=30, encoder=None, gpu_id=0, video_bitrate=2000, audio_bitrate=96000, keyframe_distance=-1.0, congestion_control=False, video_packetloss_percent=0.0, audio_packetloss_percent=0.0):
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
        self.rtpgccbwe = None
        self.congestion_control = congestion_control
        self.encoder = encoder
        self.gpu_id = gpu_id

        self.framerate = framerate
        self.video_bitrate = video_bitrate
        self.audio_bitrate = audio_bitrate

        # Keyframe distance in seconds
        self.keyframe_distance = keyframe_distance
        # Enforce minimum keyframe interval to 60 frames
        self.min_keyframe_frame_distance = 60
        self.keyframe_frame_distance = -1 if self.keyframe_distance == -1.0 else max(self.min_keyframe_frame_distance, int(self.framerate * self.keyframe_distance))
        # Set VBV/HRD buffer multiplier to frame time, set 1.5x when optimal (no keyframes/GOP) to prevent quality degradation in encoders, relax 2x when keyframe/GOP is periodic
        self.vbv_multiplier_nv = 1.5 if self.keyframe_distance == -1.0 else 3
        self.vbv_multiplier_va = 1.5 if self.keyframe_distance == -1.0 else 3
        self.vbv_multiplier_vp = 1.5 if self.keyframe_distance == -1.0 else 3
        self.vbv_multiplier_sw = 1.5 if self.keyframe_distance == -1.0 else 3
        # Packet loss base percentage
        self.video_packetloss_percent = video_packetloss_percent
        self.audio_packetloss_percent = audio_packetloss_percent
        # Prevent bitrate from overshooting because of FEC
        self.fec_video_bitrate = int(self.video_bitrate / (1.0 + (self.video_packetloss_percent / 100.0)))
        # Keep audio bitrate to exact value and increase effective bitrate after FEC to prevent audio quality degradation
        self.fec_audio_bitrate = int(self.audio_bitrate * (1.0 + (self.audio_packetloss_percent / 100.0)))

        Gst.init(None)

        self.check_plugins()

        self.ximagesrc = None
        self.ximagesrc_caps = None
        self.last_cursor_sent = None
        #self.ximagesrc_capsfilter = None
        # self.frame_count = 1
        # self.last_fps_time = time.monotonic()

        self.stream_available = lambda: logger.warning('unhandled stream_available')
        self.send_data_channel_message = lambda msg: logger.warning('unhandled send_data_channel_message')

    def build_app_sink(self, kind="video"):
        """
        Build an appsink element for the specified media kind
        which is used to receive media data from the pipeline.
        """
        appsink = Gst.ElementFactory.make("appsink", f"appsink_{kind}")
        appsink.set_property("emit-signals", True)
        appsink.set_property("max-buffers", 5)

        # Closures for passing on additional arguments to the handlers
        def on_new_sample(sink):
            return self.on_new_sample(sink, kind)
        def on_preroll(sink):
            return self.on_preroll(sink, kind)
        
        appsink.connect("new-sample", on_new_sample)
        appsink.connect("new-preroll", on_preroll)
        self.pipeline.add(appsink)

    def on_new_sample(self, sink, kind=None):
        sample = sink.emit("pull-sample")
        if sample:
            asyncio.run_coroutine_threadsafe(self.stream_available(sample, kind), self.async_event_loop)
        else:
            logger.warning(f"failed to pull {kind} sample")
        return Gst.FlowReturn.OK

    def on_preroll(self, sink, kind=None):
        sample = sink.emit("pull-preroll")
        if sample:
            buf = sample.get_buffer()
            caps = sample.get_caps()
            print("Got sample caps for preroll:", caps.to_string(), "buffer size:", buf.get_size())
            # Send the data to the consumer
            asyncio.run_coroutine_threadsafe(self.stream_available(sample, kind), self.async_event_loop)
        else:
            logger.warning(f"failed to pull {kind} preroll sample")
        return Gst.FlowReturn.OK
    
    def dynamic_idr_frame(self):
        """Send a dynamic IDR frame request to the encoder"""
        if self.pipeline:
            appsink_video = self.pipeline.get_by_name("appsink_video")
            if appsink_video:
                sink_pad = appsink_video.get_static_pad("sink")
                peer_pad = sink_pad.get_peer()
                if not peer_pad:
                    logger.error("appsink_video pad has no peer, malformed pipeline?")
                    return

                if peer_pad:
                    # Create the force key unit event
                    event = GstVideo.video_event_new_upstream_force_key_unit(
                        Gst.CLOCK_TIME_NONE,  # running_time
                        True,                 # all_headers
                        0                     # count; 0 means just the next upcoming frame.
                    )
                    peer_pad.send_event(event)
                    logger.info("Sent force key-frame event to x264enc")
                else:
                    logger.warning("Could not get sink pad of appsink_video element")
                    return

    # [START build_video_pipeline]
    def build_video_pipeline(self):
        """Adds the RTP video stream to the pipeline.
        """

        # Create ximagesrc element named x11
        # Note that when using the ximagesrc plugin, ensure that the X11 server was
        # started with shared memory support: '+extension MIT-SHM' to achieve
        # full frame rates.
        # You can check if XSHM is in use with the following command:
        #   GST_DEBUG=default:5 gst-launch-1.0 ximagesrc ! fakesink num-buffers=1 2>&1 |grep -i xshm
        self.ximagesrc = Gst.ElementFactory.make("ximagesrc", "x11")
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
        self.ximagesrc_caps = Gst.caps_from_string("video/x-raw")

        # Setting the framerate=60/1 capability instructs the ximagesrc element
        # to generate buffers at 60 frames per second (FPS).
        # The higher the FPS, the lower the latency so this parameter is one
        # way to set the overall target latency of the pipeline though keep in
        # mind that the pipeline may not always perform at the full 60 FPS.
        self.ximagesrc_caps.set_value("framerate", Gst.Fraction(self.framerate, 1))

        # Create a capability filter for the ximagesrc_caps
        self.ximagesrc_capsfilter = Gst.ElementFactory.make("capsfilter")
        self.ximagesrc_capsfilter.set_property("caps", self.ximagesrc_caps)

        # ADD_ENCODER: Add new encoder to this list and modify all locations with "ADD_ENCODER:"
        # Reference configuration for fixing when something is broken in web browsers:
        #   https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/blob/main/net/webrtc/src/webrtcsink/imp.rs
        if self.encoder in ["nvh264enc"]:
            # Upload buffers from ximagesrc directly to CUDA memory where
            # the colorspace conversion will be performed.
            cudaupload = Gst.ElementFactory.make("cudaupload")
            if self.gpu_id >= 0:
                cudaupload.set_property("cuda-device-id", self.gpu_id)

            # Convert the colorspace from BGRx to NVENC compatible format.
            # This is performed with CUDA which reduces the overall CPU load
            # compared to using the software videoconvert element.
            cudaconvert = Gst.ElementFactory.make("cudaconvert")
            if self.gpu_id >= 0:
                cudaconvert.set_property("cuda-device-id", self.gpu_id)

            # Instructs cudaconvert to handle Quality of Service (QOS) events
            # from the rest of the pipeline. Setting this value increases
            # encoder stability.
            cudaconvert.set_property("qos", True)

            # Convert ximagesrc BGRx format to NV12 using cudaconvert.
            # This is a more compatible format for client-side software decoders.
            cudaconvert_caps = Gst.caps_from_string("video/x-raw(memory:CUDAMemory)")
            cudaconvert_caps.set_value("format", "NV12")
            cudaconvert_capsfilter = Gst.ElementFactory.make("capsfilter")
            cudaconvert_capsfilter.set_property("caps", cudaconvert_caps)

            # Create the nvh264enc element named nvenc.
            # This is the heart of the video pipeline that converts the raw
            # frame buffers to an H.264 encoded byte-stream on the GPU.
            if self.gpu_id > 0:
                if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
                    nvh264enc = Gst.ElementFactory.make("nvcudah264device{}enc".format(self.gpu_id), "nvenc")
                else:
                    nvh264enc = Gst.ElementFactory.make("nvh264device{}enc".format(self.gpu_id), "nvenc")
            else:
                if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
                    nvh264enc = Gst.ElementFactory.make("nvcudah264enc", "nvenc")
                else:
                    nvh264enc = Gst.ElementFactory.make("nvh264enc", "nvenc")

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
            if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
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
            nvh264enc.set_property("vbv-buffer-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multiplier_nv))
            if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
                nvh264enc.set_property("b-frames", 0)
                # Zero-latency operation mode (no reordering delay)
                nvh264enc.set_property("zero-reorder-delay", True)
            else:
                nvh264enc.set_property("bframes", 0)
                # Zero-latency operation mode (no reordering delay)
                nvh264enc.set_property("zerolatency", True)
            if Gst.version().major == 1 and Gst.version().minor > 20:
                # CABAC is more bandwidth-efficient compared to CAVLC at a tradeoff of slight increase (<= 1 ms) in decoding time
                nvh264enc.set_property("cabac", True)
                # Insert sequence headers (SPS/PPS) per IDR
                nvh264enc.set_property("repeat-sequence-header", True)
            if Gst.version().major == 1 and Gst.version().minor > 22:
                nvh264enc.set_property("preset", "p4")
                nvh264enc.set_property("tune", "ultra-low-latency")
                # Two-pass mode allows to detect more motion vectors,
                # better distribute bitrate across the frame
                # and more strictly adhere to bitrate limits.
                nvh264enc.set_property("multi-pass", "two-pass-quarter")
            else:
                nvh264enc.set_property("preset", "low-latency-hq")

        elif self.encoder in ["nvh265enc"]:
            cudaupload = Gst.ElementFactory.make("cudaupload")
            if self.gpu_id >= 0:
                cudaupload.set_property("cuda-device-id", self.gpu_id)
            cudaconvert = Gst.ElementFactory.make("cudaconvert")
            if self.gpu_id >= 0:
                cudaconvert.set_property("cuda-device-id", self.gpu_id)
            cudaconvert.set_property("qos", True)
            cudaconvert_caps = Gst.caps_from_string("video/x-raw(memory:CUDAMemory)")
            cudaconvert_caps.set_value("format", "NV12")
            cudaconvert_capsfilter = Gst.ElementFactory.make("capsfilter")
            cudaconvert_capsfilter.set_property("caps", cudaconvert_caps)

            if self.gpu_id > 0:
                if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
                    nvh265enc = Gst.ElementFactory.make("nvcudah265device{}enc".format(self.gpu_id), "nvenc")
                else:
                    nvh265enc = Gst.ElementFactory.make("nvh265device{}enc".format(self.gpu_id), "nvenc")
            else:
                if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
                    nvh265enc = Gst.ElementFactory.make("nvcudah265enc", "nvenc")
                else:
                    nvh265enc = Gst.ElementFactory.make("nvh265enc", "nvenc")

            nvh265enc.set_property("bitrate", self.fec_video_bitrate)

            if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
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
            nvh265enc.set_property("vbv-buffer-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multiplier_nv))
            if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
                if "b-frames" in nvenc_properties:
                    nvh265enc.set_property("b-frames", 0)
                nvh265enc.set_property("zero-reorder-delay", True)
            else:
                if "bframes" in nvenc_properties:
                    nvh265enc.set_property("bframes", 0)
                nvh265enc.set_property("zerolatency", True)
            if Gst.version().major == 1 and Gst.version().minor > 20:
                nvh265enc.set_property("repeat-sequence-header", True)
            if Gst.version().major == 1 and Gst.version().minor > 22:
                nvh265enc.set_property("preset", "p4")
                nvh265enc.set_property("tune", "ultra-low-latency")
                nvh265enc.set_property("multi-pass", "two-pass-quarter")
            else:
                nvh265enc.set_property("preset", "low-latency-hq")

        elif self.encoder in ["nvav1enc"]:
            cudaupload = Gst.ElementFactory.make("cudaupload")
            if self.gpu_id >= 0:
                cudaupload.set_property("cuda-device-id", self.gpu_id)
            cudaconvert = Gst.ElementFactory.make("cudaconvert")
            if self.gpu_id >= 0:
                cudaconvert.set_property("cuda-device-id", self.gpu_id)
            cudaconvert.set_property("qos", True)
            cudaconvert_caps = Gst.caps_from_string("video/x-raw(memory:CUDAMemory)")
            cudaconvert_caps.set_value("format", "NV12")
            cudaconvert_capsfilter = Gst.ElementFactory.make("capsfilter")
            cudaconvert_capsfilter.set_property("caps", cudaconvert_caps)

            if self.gpu_id > 0:
                if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
                    nvav1enc = Gst.ElementFactory.make("nvcudaav1device{}enc".format(self.gpu_id), "nvenc")
                else:
                    nvav1enc = Gst.ElementFactory.make("nvav1device{}enc".format(self.gpu_id), "nvenc")
            else:
                if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
                    nvav1enc = Gst.ElementFactory.make("nvcudaav1enc", "nvenc")
                else:
                    nvav1enc = Gst.ElementFactory.make("nvav1enc", "nvenc")

            nvav1enc.set_property("bitrate", self.fec_video_bitrate)

            if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
                nvav1enc.set_property("rate-control", "cbr")
            else:
                nvav1enc.set_property("rc-mode", "cbr")

            nvav1enc.set_property("gop-size", -1 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            nvav1enc.set_property("strict-gop", True)
            nvav1enc.set_property("b-adapt", False)
            nvav1enc.set_property("rc-lookahead", 0)
            nvav1enc.set_property("vbv-buffer-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multiplier_nv))
            if Gst.version().major == 1 and 20 < Gst.version().minor <= 24:
                nvav1enc.set_property("b-frames", 0)
                nvav1enc.set_property("zero-reorder-delay", True)
            else:
                nvav1enc.set_property("bframes", 0)
                nvav1enc.set_property("zerolatency", True)
            if Gst.version().major == 1 and Gst.version().minor > 22:
                nvav1enc.set_property("preset", "p4")
                nvav1enc.set_property("tune", "ultra-low-latency")
                nvav1enc.set_property("multi-pass", "two-pass-quarter")
            else:
                nvav1enc.set_property("preset", "low-latency-hq")

        elif self.encoder in ["vah264enc"]:
            # colorspace conversion
            if self.gpu_id > 0:
                vapostproc = Gst.ElementFactory.make("varenderD{}postproc".format(128 + self.gpu_id), "vapostproc")
            else:
                vapostproc = Gst.ElementFactory.make("vapostproc")
            vapostproc.set_property("scale-method", "fast")
            vapostproc.set_property("qos", True)
            vapostproc_caps = Gst.caps_from_string("video/x-raw(memory:VAMemory)")
            vapostproc_caps.set_value("format", "NV12")
            vapostproc_capsfilter = Gst.ElementFactory.make("capsfilter")
            vapostproc_capsfilter.set_property("caps", vapostproc_caps)

            # encoder
            if self.gpu_id > 0:
                vah264enc = Gst.ElementFactory.make("varenderD{}h264enc".format(128 + self.gpu_id), "vaenc")
                if vah264enc is None:
                    vah264enc = Gst.ElementFactory.make("varenderD{}h264lpenc".format(128 + self.gpu_id), "vaenc")
            else:
                vah264enc = Gst.ElementFactory.make("vah264enc", "vaenc")
                if vah264enc is None:
                    vah264enc = Gst.ElementFactory.make("vah264lpenc", "vaenc")
            vah264enc.set_property("aud", False)
            vah264enc.set_property("b-frames", 0)
            # Set VBV/HRD buffer size (kbits) to optimize for live streaming
            vah264enc.set_property("cpb-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multiplier_va))
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
                vapostproc = Gst.ElementFactory.make("varenderD{}postproc".format(128 + self.gpu_id), "vapostproc")
            else:
                vapostproc = Gst.ElementFactory.make("vapostproc")
            vapostproc.set_property("scale-method", "fast")
            vapostproc.set_property("qos", True)
            vapostproc_caps = Gst.caps_from_string("video/x-raw(memory:VAMemory)")
            vapostproc_caps.set_value("format", "NV12")
            vapostproc_capsfilter = Gst.ElementFactory.make("capsfilter")
            vapostproc_capsfilter.set_property("caps", vapostproc_caps)

            # encoder
            if self.gpu_id > 0:
                vah265enc = Gst.ElementFactory.make("varenderD{}h265enc".format(128 + self.gpu_id), "vaenc")
                if vah265enc is None:
                    vah265enc = Gst.ElementFactory.make("varenderD{}h265lpenc".format(128 + self.gpu_id), "vaenc")
            else:
                vah265enc = Gst.ElementFactory.make("vah265enc", "vaenc")
                if vah265enc is None:
                    vah265enc = Gst.ElementFactory.make("vah265lpenc", "vaenc")
            vah265enc.set_property("aud", False)
            vah265enc.set_property("b-frames", 0)
            # Set VBV/HRD buffer size (kbits) to optimize for live streaming
            vah265enc.set_property("cpb-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multiplier_va))
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
                vapostproc = Gst.ElementFactory.make("varenderD{}postproc".format(128 + self.gpu_id), "vapostproc")
            else:
                vapostproc = Gst.ElementFactory.make("vapostproc")
            vapostproc.set_property("scale-method", "fast")
            vapostproc.set_property("qos", True)
            vapostproc_caps = Gst.caps_from_string("video/x-raw(memory:VAMemory)")
            vapostproc_caps.set_value("format", "NV12")
            vapostproc_capsfilter = Gst.ElementFactory.make("capsfilter")
            vapostproc_capsfilter.set_property("caps", vapostproc_caps)

            # encoder
            if self.gpu_id > 0:
                vavp9enc = Gst.ElementFactory.make("varenderD{}vp9enc".format(128 + self.gpu_id), "vaenc")
                if vavp9enc is None:
                    vavp9enc = Gst.ElementFactory.make("varenderD{}vp9lpenc".format(128 + self.gpu_id), "vaenc")
            else:
                vavp9enc = Gst.ElementFactory.make("vavp9enc", "vaenc")
                if vavp9enc is None:
                    vavp9enc = Gst.ElementFactory.make("vavp9lpenc", "vaenc")
            # Set VBV/HRD buffer size (kbits) to optimize for live streaming
            vavp9enc.set_property("cpb-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multiplier_va))
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
                vapostproc = Gst.ElementFactory.make("varenderD{}postproc".format(128 + self.gpu_id), "vapostproc")
            else:
                vapostproc = Gst.ElementFactory.make("vapostproc")
            vapostproc.set_property("scale-method", "fast")
            vapostproc.set_property("qos", True)
            vapostproc_caps = Gst.caps_from_string("video/x-raw(memory:VAMemory)")
            vapostproc_caps.set_value("format", "NV12")
            vapostproc_capsfilter = Gst.ElementFactory.make("capsfilter")
            vapostproc_capsfilter.set_property("caps", vapostproc_caps)

            # encoder
            if self.gpu_id > 0:
                vaav1enc = Gst.ElementFactory.make("varenderD{}av1enc".format(128 + self.gpu_id), "vaenc")
                if vaav1enc is None:
                    vaav1enc = Gst.ElementFactory.make("varenderD{}av1lpenc".format(128 + self.gpu_id), "vaenc")
            else:
                vaav1enc = Gst.ElementFactory.make("vaav1enc", "vaenc")
                if vaav1enc is None:
                    vaav1enc = Gst.ElementFactory.make("vaav1lpenc", "vaenc")
            # Set VBV/HRD buffer size (kbits) to optimize for live streaming
            vaav1enc.set_property("cpb-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multiplier_va))
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
            videoconvert = Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "NV12")
            videoconvert_capsfilter = Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            # encoder
            x264enc = Gst.ElementFactory.make("x264enc", "x264enc")
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
            x264enc.set_property("vbv-buf-capacity", int((1000 + self.framerate - 1) // self.framerate * self.vbv_multiplier_sw))
            x264enc.set_property("sliced-threads", True)
            x264enc.set_property("byte-stream", True)
            x264enc.set_property("pass", "cbr")
            x264enc.set_property("speed-preset", "ultrafast")
            x264enc.set_property("tune", "zerolatency")
            x264enc.set_property("bitrate", self.fec_video_bitrate)

        elif self.encoder in ["openh264enc"]:
            # Videoconvert for colorspace conversion
            videoconvert = Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            # encoder
            openh264enc = Gst.ElementFactory.make("openh264enc", "openh264enc")
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
            videoconvert = Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            # encoder
            x265enc = Gst.ElementFactory.make("x265enc", "x265enc")
            x265enc.set_property("option-string", "b-adapt=0:bframes=0:rc-lookahead=0:repeat-headers:pmode:wpp")
            x265enc.set_property("key-int-max", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            x265enc.set_property("speed-preset", "ultrafast")
            x265enc.set_property("tune", "zerolatency")
            x265enc.set_property("bitrate", self.fec_video_bitrate)

        elif self.encoder in ["vp8enc", "vp9enc"]:
            videoconvert = Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            if self.encoder == "vp8enc":
                vpenc = Gst.ElementFactory.make("vp8enc", "vpenc")

            elif self.encoder == "vp9enc":
                vpenc = Gst.ElementFactory.make("vp9enc", "vpenc")
                vpenc.set_property("frame-parallel-decoding", True)
                vpenc.set_property("row-mt", True)

            # VPX Parameters
            vpenc.set_property("threads", min(16, max(1, len(os.sched_getaffinity(0)) - 1)))
            # Set VBV/HRD buffer size (milliseconds) to optimize for live streaming
            vbv_buffer_size = int((1000 + self.framerate - 1) // self.framerate * self.vbv_multiplier_vp)
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
            videoconvert = Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            svtav1enc = Gst.ElementFactory.make("svtav1enc", "svtav1enc")
            svtav1enc.set_property("intra-period-length", -1 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            # svtav1enc.set_property("maximum-buffer-size", 150)
            svtav1enc.set_property("preset", 10)
            svtav1enc.set_property("logical-processors", min(24, max(1, len(os.sched_getaffinity(0)) - 1)))
            svtav1enc.set_property("parameters-string", "rc=2:fast-decode=1:buf-initial-sz=100:buf-optimal-sz=120:maxsection-pct=250:lookahead=0:pred-struct=1")
            svtav1enc.set_property("target-bitrate", self.fec_video_bitrate)

        elif self.encoder in ["av1enc"]:
            videoconvert = Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            av1enc = Gst.ElementFactory.make("av1enc", "av1enc")
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
            videoconvert = Gst.ElementFactory.make("videoconvert")
            videoconvert.set_property("n-threads", min(4, max(1, len(os.sched_getaffinity(0)) - 1)))
            videoconvert.set_property("qos", True)
            videoconvert_caps = Gst.caps_from_string("video/x-raw")
            videoconvert_caps.set_value("format", "I420")
            videoconvert_capsfilter = Gst.ElementFactory.make("capsfilter")
            videoconvert_capsfilter.set_property("caps", videoconvert_caps)

            rav1enc = Gst.ElementFactory.make("rav1enc", "rav1enc")
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
            h264enc_caps = Gst.caps_from_string("video/x-h264")

            # Sets the H.264 encoding profile to one compatible with WebRTC.
            # Main profile includes CABAC and is compatible with Chrome.
            # In low-latency encoding, High profile features are not utilized.
            # Browsers only support specific H.264 profiles and they are
            # coded in the RTP payload type set by the rtph264pay_caps below.
            h264enc_caps.set_value("profile", "main")

            # Stream-oriented H.264 codec
            h264enc_caps.set_value("stream-format", "byte-stream")

            # Create a capability filter for the h264enc_caps.
            h264enc_capsfilter = Gst.ElementFactory.make("capsfilter")
            h264enc_capsfilter.set_property("caps", h264enc_caps)

        elif "h265" in self.encoder or "x265" in self.encoder:
            h265enc_caps = Gst.caps_from_string("video/x-h265")
            h265enc_caps.set_value("profile", "main")
            h265enc_caps.set_value("stream-format", "byte-stream")
            h265enc_capsfilter = Gst.ElementFactory.make("capsfilter")
            h265enc_capsfilter.set_property("caps", h265enc_caps)

            rtph265pay = Gst.ElementFactory.make("rtph265pay")
            rtph265pay.set_property("mtu", 1200)
            rtph265pay.set_property("aggregate-mode", "zero-latency")
            rtph265pay.set_property("config-interval", -1)
            extensions_return = self.rtp_add_extensions(rtph265pay)
            if not extensions_return:
                logger.warning("WebRTC RTP extension configuration failed with video, this may lead to suboptimal performance")
            rtph265pay_caps = Gst.caps_from_string("application/x-rtp")
            rtph265pay_caps.set_value("media", "video")
            rtph265pay_caps.set_value("clock-rate", 90000)
            rtph265pay_caps.set_value("encoding-name", "H265")
            rtph265pay_caps.set_value("payload", 100)
            rtph265pay_caps.set_value("rtcp-fb-nack-pli", True)
            rtph265pay_caps.set_value("rtcp-fb-ccm-fir", True)
            rtph265pay_caps.set_value("rtcp-fb-x-gstreamer-fir-as-repair", True)
            rtph265pay_capsfilter = Gst.ElementFactory.make("capsfilter")
            rtph265pay_capsfilter.set_property("caps", rtph265pay_caps)

        elif "vp8" in self.encoder:
            vpenc_caps = Gst.caps_from_string("video/x-vp8")
            vpenc_capsfilter = Gst.ElementFactory.make("capsfilter")
            vpenc_capsfilter.set_property("caps", vpenc_caps)

            rtpvppay = Gst.ElementFactory.make("rtpvp8pay", "rtpvppay")
            rtpvppay.set_property("mtu", 1200)
            rtpvppay.set_property("picture-id-mode", "15-bit")
            extensions_return = self.rtp_add_extensions(rtpvppay)
            if not extensions_return:
                logger.warning("WebRTC RTP extension configuration failed with video, this may lead to suboptimal performance")
            rtpvppay_caps = Gst.caps_from_string("application/x-rtp")
            rtpvppay_caps.set_value("media", "video")
            rtpvppay_caps.set_value("clock-rate", 90000)
            rtpvppay_caps.set_value("encoding-name", "VP8")
            rtpvppay_caps.set_value("payload", 96)
            rtpvppay_caps.set_value("rtcp-fb-nack-pli", True)
            rtpvppay_caps.set_value("rtcp-fb-ccm-fir", True)
            rtpvppay_caps.set_value("rtcp-fb-x-gstreamer-fir-as-repair", True)
            rtpvppay_capsfilter = Gst.ElementFactory.make("capsfilter")
            rtpvppay_capsfilter.set_property("caps", rtpvppay_caps)

        elif "vp9" in self.encoder:
            vpenc_caps = Gst.caps_from_string("video/x-vp9")
            vpenc_capsfilter = Gst.ElementFactory.make("capsfilter")
            vpenc_capsfilter.set_property("caps", vpenc_caps)

            rtpvppay = Gst.ElementFactory.make("rtpvp9pay", "rtpvppay")
            rtpvppay.set_property("mtu", 1200)
            rtpvppay.set_property("picture-id-mode", "15-bit")
            extensions_return = self.rtp_add_extensions(rtpvppay)
            if not extensions_return:
                logger.warning("WebRTC RTP extension configuration failed with video, this may lead to suboptimal performance")
            rtpvppay_caps = Gst.caps_from_string("application/x-rtp")
            rtpvppay_caps.set_value("media", "video")
            rtpvppay_caps.set_value("clock-rate", 90000)
            rtpvppay_caps.set_value("encoding-name", "VP9")
            rtpvppay_caps.set_value("payload", 98)
            rtpvppay_caps.set_value("rtcp-fb-nack-pli", True)
            rtpvppay_caps.set_value("rtcp-fb-ccm-fir", True)
            rtpvppay_caps.set_value("rtcp-fb-x-gstreamer-fir-as-repair", True)
            rtpvppay_capsfilter = Gst.ElementFactory.make("capsfilter")
            rtpvppay_capsfilter.set_property("caps", rtpvppay_caps)

        elif "av1" in self.encoder:
            av1enc_caps = Gst.caps_from_string("video/x-av1")
            av1enc_caps.set_value("parsed", True)
            av1enc_caps.set_value("stream-format", "obu-stream")
            av1enc_capsfilter = Gst.ElementFactory.make("capsfilter")
            av1enc_capsfilter.set_property("caps", av1enc_caps)

            rtpav1pay = Gst.ElementFactory.make("rtpav1pay")
            rtpav1pay.set_property("mtu", 1200)
            extensions_return = self.rtp_add_extensions(rtpav1pay)
            if not extensions_return:
                logger.warning("WebRTC RTP extension configuration failed with video, this may lead to suboptimal performance")
            rtpav1pay_caps = Gst.caps_from_string("application/x-rtp")
            rtpav1pay_caps.set_value("media", "video")
            rtpav1pay_caps.set_value("clock-rate", 90000)
            rtpav1pay_caps.set_value("encoding-name", "AV1")
            rtpav1pay_caps.set_value("payload", 96)
            rtpav1pay_caps.set_value("rtcp-fb-nack-pli", True)
            rtpav1pay_caps.set_value("rtcp-fb-ccm-fir", True)
            rtpav1pay_caps.set_value("rtcp-fb-x-gstreamer-fir-as-repair", True)
            rtpav1pay_capsfilter = Gst.ElementFactory.make("capsfilter")
            rtpav1pay_capsfilter.set_property("caps", rtpav1pay_caps)

        # Add all elements to the pipeline.
        pipeline_elements = [self.ximagesrc, self.ximagesrc_capsfilter]

        # ADD_ENCODER: add new encoder elements to this list
        if self.encoder in ["nvh264enc"]:
            pipeline_elements += [cudaupload, cudaconvert, cudaconvert_capsfilter, nvh264enc, h264enc_capsfilter]

        elif self.encoder in ["nvh265enc"]:
            pipeline_elements += [cudaupload, cudaconvert, cudaconvert_capsfilter, nvh265enc, h265enc_capsfilter, rtph265pay, rtph265pay_capsfilter]

        elif self.encoder in ["nvav1enc"]:
            pipeline_elements += [cudaupload, cudaconvert, cudaconvert_capsfilter, nvav1enc, av1enc_capsfilter, rtpav1pay, rtpav1pay_capsfilter]

        elif self.encoder in ["vah264enc"]:
            pipeline_elements += [vapostproc, vapostproc_capsfilter, vah264enc, h264enc_capsfilter]

        elif self.encoder in ["vah265enc"]:
            pipeline_elements += [vapostproc, vapostproc_capsfilter, vah265enc, h265enc_capsfilter, rtph265pay, rtph265pay_capsfilter]

        elif self.encoder in ["vavp9enc"]:
            pipeline_elements += [vapostproc, vapostproc_capsfilter, vavp9enc, vpenc_capsfilter, rtpvppay, rtpvppay_capsfilter]

        elif self.encoder in ["vaav1enc"]:
            pipeline_elements += [vapostproc, vapostproc_capsfilter, vaav1enc, av1enc_capsfilter, rtpav1pay, rtpav1pay_capsfilter]

        elif self.encoder in ["x264enc"]:
            # TODO: Configure other pipeline elements to link encoder element with appsink
            pipeline_elements += [videoconvert, videoconvert_capsfilter, x264enc, h264enc_capsfilter]

        elif self.encoder in ["openh264enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, openh264enc, h264enc_capsfilter]

        elif self.encoder in ["x265enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, x265enc, h265enc_capsfilter, rtph265pay, rtph265pay_capsfilter]

        elif self.encoder in ["vp8enc", "vp9enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, vpenc, vpenc_capsfilter, rtpvppay, rtpvppay_capsfilter]

        elif self.encoder in ["svtav1enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, svtav1enc, av1enc_capsfilter, rtpav1pay, rtpav1pay_capsfilter]

        elif self.encoder in ["av1enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, av1enc, av1enc_capsfilter, rtpav1pay, rtpav1pay_capsfilter]

        elif self.encoder in ["rav1enc"]:
            pipeline_elements += [videoconvert, videoconvert_capsfilter, rav1enc, av1enc_capsfilter, rtpav1pay, rtpav1pay_capsfilter]

        for pipeline_element in pipeline_elements:
            self.pipeline.add(pipeline_element)

        # Link the pipeline elements and raise exception of linking failures
        # due to incompatible element pad capabilities.
        appsink_video = self.pipeline.get_by_name("appsink_video")
        if not appsink_video:
            raise MediaPipelineError("Failed to find appsink_video element in the pipeline")
        pipeline_elements += [appsink_video]
        for i in range(len(pipeline_elements) - 1):
            if not Gst.Element.link(pipeline_elements[i], pipeline_elements[i + 1]):
                raise MediaPipelineError("Failed to link {} -> {}".format(pipeline_elements[i].get_name(), pipeline_elements[i + 1].get_name()))
    # [END build_video_pipeline]

    # [START build_audio_pipeline]
    def build_audio_pipeline(self):
        """Adds the RTP audio stream to the pipeline.
        """

        # Create element for receiving audio from pulseaudio.
        pulsesrc = Gst.ElementFactory.make("pulsesrc", "pulsesrc")

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
        pulsesrc.set_property("latency-time", 1000)

        # Create capabilities for pulsesrc and set channels
        pulsesrc_caps = Gst.caps_from_string("audio/x-raw")
        pulsesrc_caps.set_value("channels", self.audio_channels)

        # Create a capability filter for the pulsesrc_caps
        pulsesrc_capsfilter = Gst.ElementFactory.make("capsfilter")
        pulsesrc_capsfilter.set_property("caps", pulsesrc_caps)

        # Encode the raw PulseAudio stream to the Opus format which is
        # the default packetized streaming format for the web
        opusenc = Gst.ElementFactory.make("opusenc", "opusenc")

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
            if not Gst.Element.link(pipeline_elements[i], pipeline_elements[i + 1]):
                raise MediaPipelineError("Failed to link {} -> {}".format(pipeline_elements[i].get_name(), pipeline_elements[i + 1].get_name()))
    # [END build_audio_pipeline]

    def check_plugins(self):
        """Check for required gstreamer plugins.

        Raises:
            MediaPipelineError -- thrown if any plugins are missing.
        """

        required = ["opus", "app", "ximagesrc"]

        # ADD_ENCODER: add new encoder to this list
        supported = ["nvh264enc", "nvh265enc", "nvav1enc", "vah264enc", "vah265enc", "vavp9enc", "vaav1enc", "x264enc", "openh264enc", "x265enc", "vp8enc", "vp9enc", "svtav1enc", "av1enc", "rav1enc"]
        if self.encoder not in supported:
            raise MediaPipelineError('Unsupported encoder, must be one of: ' + ','.join(supported))

        # ADD_ENCODER: add new encoder to this list with required GStreamer plugin
        if "av1" in self.encoder or self.congestion_control:
            # rtpav1pay and rtpgccbwe are in gst-plugins-rs
            required.append("rsrtp")

        if self.encoder.startswith("nv"):
            required.append("nvcodec")

        elif self.encoder.startswith("va"):
            required.append("va")

        elif self.encoder in ["x264enc"]:
            required.append("x264")

        elif self.encoder in ["openh264enc"]:
            required.append("openh264")

        elif self.encoder in ["x265enc"]:
            required.append("x265")

        elif self.encoder in ["vp8enc", "vp9enc"]:
            required.append("vpx")

        elif self.encoder in ["svtav1enc"]:
            required.append("svtav1")

        elif self.encoder in ["av1enc"]:
            required.append("aom")

        elif self.encoder in ["rav1enc"]:
            required.append("rav1e")

        missing = list(
            filter(lambda p: Gst.Registry.get().find_plugin(p) is None, required))
        if missing:
            raise MediaPipelineError('Missing gstreamer plugins:', missing)

    def set_framerate(self, framerate):
        """Set pipeline framerate in fps

        Arguments:
            framerate {integer} -- framerate in frames per second, for example, 15, 30, 60.
        """
        if self.pipeline:
            self.framerate = framerate
            # ADD_ENCODER: GOP/IDR Keyframe distance to keep the stream from freezing (in keyframe_dist seconds) and set vbv-buffer-size
            self.keyframe_frame_distance = -1 if self.keyframe_distance == -1.0 else max(self.min_keyframe_frame_distance, int(self.framerate * self.keyframe_distance))
            if self.encoder.startswith("nv"):
                element = Gst.Bin.get_by_name(self.pipeline, "nvenc")
                element.set_property("gop-size", -1 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
                element.set_property("vbv-buffer-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multiplier_nv))
            elif self.encoder.startswith("va"):
                element = Gst.Bin.get_by_name(self.pipeline, "vaenc")
                element.set_property("key-int-max", 1024 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
                element.set_property("cpb-size", int((self.fec_video_bitrate + self.framerate - 1) // self.framerate * self.vbv_multiplier_va))
            elif self.encoder in ["x264enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "x264enc")
                element.set_property("key-int-max", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
                element.set_property("vbv-buf-capacity", int((1000 + self.framerate - 1) // self.framerate * self.vbv_multiplier_sw))
            elif self.encoder in ["openh264enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "openh264enc")
                element.set_property("gop-size", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            elif self.encoder in ["x265enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "x265enc")
                element.set_property("key-int-max", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            elif self.encoder.startswith("vp"):
                element = Gst.Bin.get_by_name(self.pipeline, "vpenc")
                element.set_property("keyframe-max-dist", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
                vbv_buffer_size = int((1000 + self.framerate - 1) // self.framerate * self.vbv_multiplier_vp)
                element.set_property("buffer-initial-size", vbv_buffer_size)
                element.set_property("buffer-optimal-size", vbv_buffer_size)
                element.set_property("buffer-size", vbv_buffer_size)
            elif self.encoder in ["svtav1enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "svtav1enc")
                element.set_property("intra-period-length", -1 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            elif self.encoder in ["av1enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "av1enc")
                element.set_property("keyframe-max-dist", 2147483647 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            elif self.encoder in ["rav1enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "rav1enc")
                element.set_property("max-key-frame-interval", 715827882 if self.keyframe_distance == -1.0 else self.keyframe_frame_distance)
            else:
                logger.warning("setting keyframe interval (GOP size) not supported with encoder: %s" % self.encoder)

            self.ximagesrc_caps = Gst.caps_from_string("video/x-raw")
            self.ximagesrc_caps.set_value("framerate", Gst.Fraction(self.framerate, 1))
            self.ximagesrc_capsfilter.set_property("caps", self.ximagesrc_caps)
            logger.info("framerate set to: %d" % framerate)

    def set_video_bitrate(self, bitrate, cc=False):
        """Set video encoder target bitrate in bps

        Arguments:
            bitrate {integer} -- bitrate in bits per second, for example, 2000 for 2kbits/s or 10000 for 1mbit/sec.
            cc {boolean} -- whether the congestion control element triggered the bitrate change.
        """

        if self.pipeline:
            # Prevent bitrate from overshooting because of FEC
            fec_bitrate = int(bitrate / (1.0 + (self.video_packetloss_percent / 100.0)))
            # Change bitrate range of congestion control element
            if (not cc) and self.congestion_control and self.rtpgccbwe is not None:
                # Prevent encoder freeze because of low bitrate with min-bitrate
                self.rtpgccbwe.set_property("min-bitrate", max(100000 + self.fec_audio_bitrate, int(bitrate * 1000 * 0.1 + self.fec_audio_bitrate)))
                self.rtpgccbwe.set_property("max-bitrate", int(bitrate * 1000 + self.fec_audio_bitrate))
                self.rtpgccbwe.set_property("estimated-bitrate", int(bitrate * 1000 + self.fec_audio_bitrate))
            # ADD_ENCODER: add new encoder to this list and set vbv-buffer-size if unit is bytes instead of milliseconds
            if self.encoder.startswith("nv"):
                element = Gst.Bin.get_by_name(self.pipeline, "nvenc")
                if not cc:
                    element.set_property("vbv-buffer-size", int((fec_bitrate + self.framerate - 1) // self.framerate * self.vbv_multiplier_nv))
                element.set_property("bitrate", fec_bitrate)
            elif self.encoder.startswith("va"):
                element = Gst.Bin.get_by_name(self.pipeline, "vaenc")
                if not cc:
                    element.set_property("cpb-size", int((fec_bitrate + self.framerate - 1) // self.framerate * self.vbv_multiplier_va))
                element.set_property("bitrate", fec_bitrate)
            elif self.encoder in ["x264enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "x264enc")
                element.set_property("bitrate", fec_bitrate)
            elif self.encoder in ["openh264enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "openh264enc")
                element.set_property("bitrate", fec_bitrate * 1000)
            elif self.encoder in ["x265enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "x265enc")
                element.set_property("bitrate", fec_bitrate)
            elif self.encoder in ["vp8enc", "vp9enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "vpenc")
                element.set_property("target-bitrate", fec_bitrate * 1000)
            elif self.encoder in ["svtav1enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "svtav1enc")
                element.set_property("target-bitrate", fec_bitrate)
            elif self.encoder in ["av1enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "av1enc")
                element.set_property("target-bitrate", fec_bitrate)
            elif self.encoder in ["rav1enc"]:
                element = Gst.Bin.get_by_name(self.pipeline, "rav1enc")
                element.set_property("bitrate", fec_bitrate * 1000)
            else:
                logger.warning("set_video_bitrate not supported with encoder: %s" % self.encoder)

            if not cc:
                logger.info("video bitrate set to: %d" % bitrate)
            else:
                logger.debug("video bitrate set with congestion control to: %d" % bitrate)

            self.video_bitrate = bitrate
            self.fec_video_bitrate = fec_bitrate

            if not cc:
                self.send_data_channel_message("pipeline", {"status": "Video bitrate set to: %d" % bitrate})

    def set_audio_bitrate(self, bitrate):
        """Set Opus encoder target bitrate in bps

        Arguments:
            bitrate {integer} -- bitrate in bits per second, for example, 96000 for 96 kbits/s.
        """

        if self.pipeline:
            # Keep audio bitrate to exact value and increase effective bitrate after FEC to prevent audio quality degradation
            fec_bitrate = int(bitrate * (1.0 + (self.audio_packetloss_percent / 100.0)))
            # Change bitrate range of congestion control element
            if self.congestion_control and self.rtpgccbwe is not None:
                # Prevent encoder freeze because of low bitrate with min-bitrate
                self.rtpgccbwe.set_property("min-bitrate", max(100000 + fec_bitrate, int(self.video_bitrate * 1000 * 0.1 + fec_bitrate)))
                self.rtpgccbwe.set_property("max-bitrate", int(self.video_bitrate * 1000 + fec_bitrate))
                self.rtpgccbwe.set_property("estimated-bitrate", int(self.video_bitrate * 1000 + fec_bitrate))
            element = Gst.Bin.get_by_name(self.pipeline, "opusenc")
            element.set_property("bitrate", bitrate)

            logger.info("audio bitrate set to: %d" % bitrate)
            self.audio_bitrate = bitrate
            self.fec_audio_bitrate = fec_bitrate

            self.__send_data_channel_message(
                "pipeline", {"status": "Audio bitrate set to: %d" % bitrate})

    def set_pointer_visible(self, visible):
        """Set pointer visibiltiy on the ximagesrc element

        Arguments:
            visible {bool} -- True to enable pointer visibility
        """
       # logger.info("skipping pinter visibility for now...")
        element = Gst.Bin.get_by_name(self.pipeline, "x11")
        element.set_property("show-pointer", visible)
        self.send_data_channel_message(
            "pipeline", {"status": "Set pointer visibility to: %d" % visible})

    # TODO: Remove this method once all media encoder pipelines are configured properly
    # removing rtp payload and filter elements
    def rtp_add_extensions(self, payloader, audio=False):
        """Adds WebRTC RTP extensions to the payloader

        Arguments:
            payloader {GstRTPBasePayload gobject} -- payloader gobject
            audio {boolean} -- Whether the RTP payloader is for audio
        """
        rtp_id_iteration = 0
        return_result = True
        custom_ext = {"http://www.webrtc.org/experiments/rtp-hdrext/playout-delay": self.PlayoutDelayExtension()}

        rtp_uri_list = []
        if self.congestion_control:
            rtp_uri_list += ["http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"]
        if not audio:
            rtp_uri_list += ["http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"]
        for rtp_uri in rtp_uri_list:
            try:
                rtp_id = self.__pick_rtp_extension_id(payloader, rtp_uri, previous_rtp_id=rtp_id_iteration)
                if rtp_id is not None:
                    if rtp_uri in custom_ext.keys():
                        rtp_extension = custom_ext[rtp_uri]
                    else:
                        rtp_extension = GstRtp.RTPHeaderExtension.create_from_uri(rtp_uri)
                    if not rtp_extension:
                        raise MediaPipelineError("GstRtp.RTPHeaderExtension for {} is None".format(rtp_uri))
                    rtp_extension.set_id(rtp_id)
                    payloader.emit("add-extension", rtp_extension)
                    rtp_id_iteration = rtp_id
            except Exception as e:
                logger.warning("RTP extension {} not added because of error {}".format(rtp_uri, e))
                return_result = False
        return return_result

    def __pick_rtp_extension_id(self, payloader, uri, previous_rtp_id=0):
        """Finds extension ID for RTP extensions with the payloader

        Arguments:
            payloader {GstRTPBasePayload gobject} -- payloader gobject
            uri {str} -- URI for the RTP extension
        """
        payloader_properties = payloader.list_properties()
        enabled_extensions = payloader.get_property("extensions") if "extensions" in [payloader_property.name for payloader_property in payloader_properties] else None
        if not enabled_extensions:
            logger.debug("'extensions' property in {} does not exist in payloader, application code must ensure to select non-conflicting IDs for any additionally configured extensions".format(payloader.get_name()))
            return max(1, previous_rtp_id + 1)
        extension = next((ext for ext in enabled_extensions if ext.get_uri() == uri), None)
        # When extension is already mapped
        if extension:
            return None
        used_numbers = set(ext.get_id() for ext in enabled_extensions)
        # Find first extension ID that does not overlap
        num = 1
        while True:
            if num not in used_numbers:
                return num
            num += 1

    def bus_call(self, message):
        if message is None:
            return True
        t = message.type
        if t == Gst.MessageType.EOS:
            logger.error("End-of-stream\n")
            return False
        elif t == Gst.MessageType.ERROR:
            err, debug = message.parse_error()
            logger.error("Error: %s: %s\n" % (err, debug))
            return False
        elif t == Gst.MessageType.STATE_CHANGED:
            if isinstance(message.src, Gst.Pipeline):
                old_state, new_state, pending_state = message.parse_state_changed()
                logger.info(("Pipeline state changed from %s to %s." %
                    (old_state.value_nick, new_state.value_nick)))
                if (old_state.value_nick == "paused" and new_state.value_nick == "ready"):
                    logger.info("stopping bus message task")
                    return False
        elif t == Gst.MessageType.LATENCY:
            self.pipeline.set_latency(0)
        return True

    async def start_media_pipeline(self):
        """Starts the Media pipeline
        """
        logger.info("starting media pipeline")

        self.pipeline = Gst.Pipeline.new()
        if self.pipeline is None:
            raise MediaPipelineError("Failed to create media pipeline")

        self.build_app_sink(kind="video")
        self.build_app_sink(kind="audio")
        self.build_video_pipeline()
        self.build_audio_pipeline()

        # Advance the state of the pipeline to PLAYING.
        res = self.pipeline.set_state(Gst.State.PLAYING)
        if res == Gst.StateChangeReturn.ASYNC:
            logger.info("pipeline state is Asynchronous")
            while res != Gst.StateChangeReturn.SUCCESS:
                res, _, _ = self.pipeline.get_state(Gst.CLOCK_TIME_NONE)
                await asyncio.sleep(0.1)

        if res != Gst.StateChangeReturn.SUCCESS:
            raise MediaPipelineError(f"Failed to transition media pipeline to PLAYING: {res}")
        logger.info("pipeline started")

    async def handle_bus_calls(self):
        # Start bus call task
        running = True
        bus = None
        while running:
            if self.pipeline is not None:
                bus = await asyncio.to_thread(self.pipeline.get_bus)
            if bus is not None:
                while await asyncio.to_thread(bus.have_pending):
                    msg = bus.pop()
                    if not await asyncio.to_thread(self.bus_call, msg):
                        running = False
            await asyncio.sleep(0.1)

    async def stop_pipeline(self):
        logger.info("stopping pipeline")
        if self.pipeline:
            logger.info("setting pipeline state to NULL")
            await asyncio.to_thread(self.pipeline.set_state, Gst.State.NULL)
            self.pipeline = None
            logger.info("pipeline set to state NULL")
        logger.info("pipeline stopped")

    # TODO: Remove this class once all media encoder pipelines are configured properly
    class PlayoutDelayExtension(GstRtp.RTPHeaderExtension):
        # PlayoutDelayExtension is a extension payload format in
        # http://www.webrtc.org/experiments/rtp-hdrext/playout-delay
        # 0                   1                   2                   3
        # 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
        # +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        # |  ID   | len=2 |       MIN delay       |       MAX delay       |
        # +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        #
        # For zero WebRTC jitterbuffer latency, send 0 for both MIN and MAX delay
        def __init__(self):
            super().__init__()
            self.min_delay = 0
            self.max_delay = 0
            self.set_uri("http://www.webrtc.org/experiments/rtp-hdrext/playout-delay")

        def do_get_supported_flags(self):
            return GstRtp.RTPHeaderExtensionFlags.ONE_BYTE | GstRtp.RTPHeaderExtensionFlags.TWO_BYTE

        def do_get_max_size(self, input_meta):
            return 3  # 3 bytes for MIN delay and MAX delay

        def do_write(self, input_meta, write_flags, output, data, size):
            # Write MIN delay and MIN delay to the RTP header
            # min_delay_high = (self.min_delay >> 4) & 0xFF
            # min_delay_low_max_delay_high = ((self.min_delay & 0x0F) << 4) | ((self.max_delay >> 8) & 0x0F)
            # max_delay_low = self.max_delay & 0xFF
            # delay_concat = bytearray([min_delay_high, min_delay_low_max_delay_high, max_delay_low])

            # As default value is 0 for both MIN delay and MAX delay, no need to do anything
            return 3

        def do_read(self, read_flags, data, size, buffer):
            # Read MIN delay and MAX delay from the RTP header
            self.min_delay = (data[0] << 4) | (data[1] >> 4)
            self.max_delay = ((data[1] & 0x0F) << 8) | data[2]
            return True
