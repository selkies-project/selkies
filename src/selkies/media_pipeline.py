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
import time
from enum import Enum
from abc import ABCMeta, abstractmethod
from typing import Callable, Awaitable

from .settings import settings as app_settings
from .display_utils import parse_dri_node_to_index, parse_gpu_id, format_pixelflux_cursor

# C-API (non-abi3) wheels: a missing/ABI-skewed build raises ImportError or
# RuntimeError at import. Degrade to None so plain WS mode and module import
# survive; capture-start methods below raise a clear error only if used absent.
try:
    from pixelflux import CaptureSettings, ScreenCapture
except (ImportError, RuntimeError):
    CaptureSettings = ScreenCapture = None

try:
    from pcmflux import AudioCapture, AudioCaptureSettings
except (ImportError, RuntimeError):
    AudioCapture = AudioCaptureSettings = None

logger = logging.getLogger("media_pipeline")
logger.setLevel(logging.INFO)


class RateControlMode(str, Enum):
    CBR = "cbr"
    CRF = "crf"


class MediaPipelineError(Exception):
    pass


class MediaPipeline(metaclass=ABCMeta):
    @abstractmethod
    async def start_media_pipeline(self):
        pass

    @abstractmethod
    async def stop_media_pipeline(self):
        pass

    @abstractmethod
    def is_media_pipeline_running(self) -> bool:
        pass

    @abstractmethod
    async def set_pointer_visible(self, visible: bool):
        pass

    @abstractmethod
    async def set_framerate(self, framerate: int):
        pass

    @abstractmethod
    async def set_video_bitrate(self, bitrate: float):
        pass

    @abstractmethod
    async def set_audio_bitrate(self, bitrate: int):
        pass

    @abstractmethod
    async def dynamic_idr_frame(self):
        pass

    @abstractmethod
    async def update_rate_control_mode(self, mode: RateControlMode):
        pass

    @abstractmethod
    async def set_crf(self, crf: int):
        pass


class MediaPipelinePixel(MediaPipeline):
    def __init__(
        self,
        async_event_loop: asyncio.AbstractEventLoop,
        encoder_rtc: str,
        framerate: int = 30,
        video_bitrate: float = 8,
        audio_bitrate: int = 128000,
        width: int = 1920,
        height: int = 1080,
        audio_channels: int = 2,
        audio_enabled: bool = True,
        audio_device_name="output.monitor",
        crf: int = 23,
        rc_mode: RateControlMode = RateControlMode.CBR,
        video_fullcolor: bool = False,
        use_cpu: bool = False,
        video_streaming_mode: bool = True,
        use_paint_over_quality: bool = True,
        video_paintover_crf: int = 18,
        video_paintover_burst_frames: int = 5,
        display_id: str = "primary",
        capture_region=None,
    ):
        self.async_event_loop = async_event_loop
        # Which display this pipeline feeds and, for a secondary display, the
        # region of the extended framebuffer it captures ((x, y) origin; the
        # dimensions ride self.width/self.height). None captures from (0, 0)
        # with auto size — the single-display behavior.
        self.display_id = display_id or "primary"
        self.capture_region = capture_region
        self.audio_channels = audio_channels
        self.encoder_rtc = encoder_rtc
        self.framerate = framerate
        self.video_bitrate = video_bitrate
        self.rc_mode = rc_mode
        self.video_crf = crf
        self.video_fullcolor = video_fullcolor
        # Force software x264 for h264enc (openh264enc is always software). WS
        # honors this too; a change is structural, so it restarts capture.
        self.use_cpu = use_cpu
        # Turbo (stream every frame vs damage-gated) and the paint-over quality knobs —
        # the same pixelflux tunables the WS path exposes. Streaming mode and paint-over
        # apply live (update_tunables); only fullcolor is structural.
        self.video_streaming_mode = video_streaming_mode
        self.use_paint_over_quality = use_paint_over_quality
        self.video_paintover_crf = video_paintover_crf
        self.video_paintover_burst_frames = video_paintover_burst_frames
        self.audio_bitrate = audio_bitrate
        self.last_resize_success = True
        self.width = width
        self.height = height
        # Wayland compositor capture scale (DPI/96). Applied on Wayland only; a
        # DPI change updates it and restarts capture so pixelflux re-reads it.
        self.scale = 1.0
        self.audio_enabled = audio_enabled
        self.audio_device_name = audio_device_name
        self.capture_cursor = False
        self.produce_data: Callable[[bytes, int, str], None] = lambda buf, pts, kind: logger.warning(
            "unhandled produce_data"
        )
        self.send_data_channel_message: Callable[[str], None] = lambda msg: logger.warning(
            "unhandled send_data_channel_message"
        )
        # Fired after the video stream (re)starts so the transport can resend the
        # cursor (a slept/woken tab clears its cursor canvas). No-op by default.
        self.on_pipeline_started: Callable[[], None] = lambda: None
        # Cursor updates from pixelflux (Wayland compositor / X11 XFixes
        # monitor), already in the client cursor-message shape
        # (curdata/width/height/hotx/hoty/handle).
        self.on_cursor_data: Callable[[dict], None] = lambda data: None
        # Longest cursor edge the X11 monitor delivers (DPI-scaled upstream);
        # <= 0 falls back to the capture-settings default.
        self.get_cursor_size_cap: Callable[[], int] = lambda: 0

        self.capture_module = None
        self.pcmflux_module = None
        self._is_screen_capturing = False
        self._is_pcmflux_capturing = False
        self._running = False
        self.async_lock = asyncio.Lock()
        # Video pts clock; pipeline-scoped (not capture-scoped) so capture
        # restarts and fps changes can never rewind pts on a live RTP sender.
        self._video_pts_anchor = None
        self._last_video_pts = -1
        self._audio_routing_task = None

    async def set_pointer_visible(self, visible: bool):
        """To enable capturing the cursor from pixeflux.

        :visible: set True to enable
        """
        if self.capture_cursor == visible:
            return

        # Record the choice even while capture is down: the next start reads
        # capture_cursor from the settings snapshot, so a toggle sent during a
        # pause (or before a secondary display starts) still takes effect.
        self.capture_cursor = visible
        if not self._is_screen_capturing or self.capture_module is None:
            return
        try:
            # Live toggle: the capture thread re-reads the cursor flag per grab.
            self.capture_module.update_tunables(self.generate_capture_settings())
            logger.info(f"Set pointer visibility to: {visible}")
        except Exception as e:
            logger.error(f"Error setting pointer visibility: {e}", exc_info=True)

    async def update_rate_control_mode(self, mode: RateControlMode):
        """Set rate control mode for video encoder.

        :mode: Rate control mode, either "cbr" or "crf"
        """
        if not self._is_screen_capturing or self.capture_module is None:
            return

        if mode == self.rc_mode:
            return

        if mode not in [RateControlMode.CBR, RateControlMode.CRF]:
            logger.error(f"Invalid rate control mode: {mode}")
            return

        self.rc_mode = mode
        try:
            await self.restart_screen_capture()
            logger.info(f"Updated rate control mode to: {self.rc_mode}")
        except Exception as e:
            logger.info(f"Error updating rate control mode {e}", exc_info=True)

    async def set_crf(self, crf: int):
        """Set video encoder target CRF.

        :crf: CRF value
        """
        if not self._is_screen_capturing or self.capture_module is None:
            return

        if self.rc_mode != RateControlMode.CRF or self.video_crf == crf:
            return

        old_crf = self.video_crf
        self.video_crf = crf
        try:
            # Live retarget: NVENC/x264/VAAPI re-read the CRF per frame; no restart.
            self.capture_module.update_tunables(self.generate_capture_settings())
            logger.info(f"Updated CRF live: {old_crf} -> {crf}")
        except Exception as e:
            logger.info(f"Error updating CRF {e}", exc_info=True)

    async def set_use_cpu(self, use_cpu: bool):
        """Switch h264enc between software x264 and hardware encoding.

        Unlike CRF/bitrate this is structural (a different encoder instance), so
        it restarts the capture instead of going through the live update path.
        """
        if self.use_cpu == use_cpu:
            return
        self.use_cpu = use_cpu
        if not self._is_screen_capturing or self.capture_module is None:
            return  # applied on the next start_screen_capture()
        logger.info(f"use_cpu -> {use_cpu}; restarting screen capture")
        await self.restart_screen_capture()

    async def _apply_tunables_live(self, what: str):
        """Push current capture settings to the running module with no restart."""
        if not self._is_screen_capturing or self.capture_module is None:
            return
        try:
            self.capture_module.update_tunables(self.generate_capture_settings())
            logger.info(f"Updated {what} live")
        except Exception as e:
            logger.info(f"Error updating {what}: {e}", exc_info=True)

    async def set_video_fullcolor(self, fullcolor: bool):
        """Toggle 4:4:4 color. Structural (pixel format), so restart capture (WS parity)."""
        if self.video_fullcolor == fullcolor:
            return
        self.video_fullcolor = fullcolor
        if not self._is_screen_capturing or self.capture_module is None:
            return
        logger.info(f"video_fullcolor -> {fullcolor}; restarting screen capture")
        await self.restart_screen_capture()

    async def set_encoder_rtc(self, encoder_rtc: str):
        """Switch the WebRTC video encoder (h264enc <-> openh264enc). Structural (a
        different encoder instance), so restart capture — same as use_cpu (WS parity)."""
        if self.encoder_rtc == encoder_rtc:
            return
        self.encoder_rtc = encoder_rtc
        if not self._is_screen_capturing or self.capture_module is None:
            return
        logger.info(f"encoder_rtc -> {encoder_rtc}; restarting screen capture")
        await self.restart_screen_capture()

    async def set_video_streaming_mode(self, enabled: bool):
        """Toggle Turbo (stream every frame vs damage-gated). Live tunable."""
        if self.video_streaming_mode == enabled:
            return
        self.video_streaming_mode = enabled
        await self._apply_tunables_live(f"streaming mode -> {enabled}")

    async def set_use_paint_over_quality(self, enabled: bool):
        if self.use_paint_over_quality == enabled:
            return
        self.use_paint_over_quality = enabled
        await self._apply_tunables_live(f"paint-over -> {enabled}")

    async def set_video_paintover_crf(self, crf: int):
        if self.video_paintover_crf == crf:
            return
        self.video_paintover_crf = crf
        await self._apply_tunables_live(f"paint-over CRF -> {crf}")

    async def set_video_paintover_burst_frames(self, frames: int):
        if self.video_paintover_burst_frames == frames:
            return
        self.video_paintover_burst_frames = frames
        await self._apply_tunables_live(f"paint-over burst -> {frames}")

    async def set_video_bitrate(self, bitrate: float):
        """Set video encoder target bitrate.

        :bitrate: bitrate in mbps (fractions allowed for sub-Mbps targets)
        """
        if not self._is_screen_capturing or self.capture_module is None:
            return

        if (
            self.rc_mode == RateControlMode.CRF
            or bitrate <= 0
            or self.video_bitrate == bitrate
        ):
            return

        try:
            # Non-blocking in pixelflux (atomic store / channel send).
            self.capture_module.update_video_bitrate(int(round(bitrate * 1000)))
            logger.info(
                f"Updated video bitrate: {self.video_bitrate}Mbps -> {bitrate}Mbps"
            )
            self.video_bitrate = bitrate
        except Exception as e:
            logger.info(f"Error updating video bitrate {e}", exc_info=True)

    async def set_audio_bitrate(self, bitrate: int):
        """Set audio encoder target bitrate.

        :bitrate: bitrate in kbps
        """
        if not self._is_pcmflux_capturing or self.pcmflux_module is None:
            return

        if bitrate <= 0 or self.audio_bitrate == bitrate:
            return

        try:
            # Non-blocking in pcmflux (atomic store).
            self.pcmflux_module.update_audio_bitrate(bitrate)
            logger.info(
                f"Updated audio bitrate: {self.audio_bitrate // 1000} -> {bitrate // 1000} kbps"
            )
            self.audio_bitrate = bitrate
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
            # Non-blocking in pixelflux (atomic store / channel send).
            self.capture_module.update_framerate(float(self.framerate))
            logger.info(f"Updated framerate to: {self.framerate}")

    async def dynamic_idr_frame(self):
        """Requests an IDR frame from pixelflux"""
        if not self._is_screen_capturing or self.capture_module is None:
            return
        try:
            # Non-blocking in pixelflux (atomic flag / channel send).
            self.capture_module.request_idr_frame()
            logger.debug("IDR frame requested successfully")
        except Exception as e:
            logger.error(f"Error requesting IDR frame: {e}", exc_info=True)

    def generate_capture_settings(self):
        """Generates configuration for pixelflux screen capturing"""
        cs = CaptureSettings()
        cs.capture_width = self.width
        cs.capture_height = self.height
        if self.capture_region is not None:
            # A laid-out region of the extended framebuffer: pin the exact
            # geometry (auto-adjust would balloon the region to the whole root).
            cs.capture_x = int(self.capture_region[0])
            cs.capture_y = int(self.capture_region[1])
            cs.auto_adjust_screen_capture_size = False
        else:
            cs.capture_x = 0
            cs.capture_y = 0
            cs.auto_adjust_screen_capture_size = True
        cs.target_fps = float(self.framerate)
        cs.capture_cursor = self.capture_cursor
        cs.output_mode = 1
        # WebRTC has its own RTP framing; omit pixelflux's per-stripe header on both
        # backends (the Wayland backend now supports omission) so there's no Python
        # strip. frame_id comes from the frame attribute, not the header.
        self._omit_stripe_headers = True
        cs.omit_stripe_headers = self._omit_stripe_headers

        if self.encoder_rtc in ["h264enc", "openh264enc"]:
            cs.video_streaming_mode = self.video_streaming_mode
            cs.video_fullframe = True
            cs.video_crf = self.video_crf
            # 4:4:4 rides the same policy as the WS path: NVENC/x264 honor it,
            # VAAPI falls back to software x264, OpenH264 surfaces 4:2:0-only.
            cs.video_fullcolor = self.video_fullcolor
            # Paint-over quality (static-scene refinement); applied live like CRF.
            cs.use_paint_over_quality = self.use_paint_over_quality
            cs.video_paintover_crf = self.video_paintover_crf
            cs.video_paintover_burst_frames = self.video_paintover_burst_frames
            # Setting video_cbr_mode to True will make the encoder ignore the crf value
            cs.video_cbr_mode = self.rc_mode == RateControlMode.CBR
            cs.video_bitrate_kbps = int(round(float(self.video_bitrate) * 1000))  # Convert Mbps to kbps
            if self.encoder_rtc == "openh264enc":
                cs.use_cpu = True
                cs.use_openh264 = True
            elif self.use_cpu:
                # Honor use_cpu for h264enc too (parity with the WS path): force
                # software x264 instead of selecting a hardware encoder node.
                cs.use_cpu = True
            else:
                # h264enc is hardware-first like the WS path — pixelflux picks NVENC
                # or VA-API when present and falls back to software x264 otherwise.
                # --gpu-id picks the device by index when no --encode-dri path is given.
                dri_node = str(getattr(app_settings, 'encode_dri', '') or '')
                gid = parse_gpu_id(getattr(app_settings, 'gpu_id', ''))
                if dri_node:
                    cs.encode_node_path = dri_node.encode('utf-8')
                    cs.encode_node_index = parse_dri_node_to_index(dri_node)
                elif gid is not None:
                    # >= 0 picks the device; -1 requests software encoding.
                    cs.encode_node_index = gid
            # 0 = infinite GOP; recovery IDRs come on demand (PLI -> dynamic IDR).
            cs.keyframe_interval_s = float(
                getattr(app_settings, 'keyframe_interval', 0) or 0
            )
            # CBR QP clamp (0 = encoder default).
            cs.video_min_qp = int(getattr(app_settings, 'video_min_qp', 0) or 0)
            cs.video_max_qp = int(getattr(app_settings, 'video_max_qp', 0) or 0)
            # Compositor render node, distinct from the encoder node above: an
            # explicit --render-dri wins; otherwise pixelflux resolves --auto-gpu
            # ("true" or a vendor/driver/DT-prefix/PCI-id token) itself.
            render_dri = str(getattr(app_settings, 'render_dri', '') or '')
            if render_dri:
                cs.render_node_path = render_dri.encode('utf-8')
            cs.auto_gpu = str(getattr(app_settings, 'auto_gpu', '') or '')
        # Backend choice, the H.264 recording tap, and the compositor cursor-theme
        # size ride the settings pipeline into pixelflux (it reads no SELKIES_*
        # environment itself).
        cs.use_wayland = bool(app_settings.wayland[0])
        cs.recording_socket = str(getattr(app_settings, 'recording_socket', '') or '')
        cs.cursor_size = int(getattr(app_settings, 'cursor_size', -1))
        cap = int(self.get_cursor_size_cap() or 0)
        cs.cursor_size_cap = cap if cap > 0 else max(32, cs.cursor_size)
        cs.debug_logging = bool(app_settings.debug[0])
        if cs.use_wayland:
            cs.scale = self.scale
        # Server-embedded watermark, burned into the frame by pixelflux on both
        # backends. Kept server-side (never broadcast), so it must be set here too.
        watermark_path = str(getattr(app_settings, 'watermark_path', '') or '')
        if watermark_path and os.path.exists(watermark_path):
            cs.watermark_path = watermark_path.encode('utf-8')
            cs.watermark_location_enum = int(getattr(app_settings, 'watermark_location', -1))
        return cs

    def _screen_capture_callback(self, frame):
        try:
            hdr = 0 if self._omit_stripe_headers else 10
            if len(frame) > hdr:
                # frame owns its native buffer; pass a zero-copy memoryview (sliced
                # past the header) downstream. consume_data wraps it in av.Packet(buf)
                # (zero-copy) and keeps a reference so `frame` stays alive.
                data_bytes = memoryview(frame)[hdr:]
                # pts (90 kHz) comes from a pipeline-scoped monotonic clock,
                # not frame.frame_id: the u16 frame counter wraps at 65536,
                # restarts at 0 on every capture restart, and its implied
                # step changes on live fps raises — all backward RTP jumps
                # on a live sender. Only one capture thread exists at a time
                # (stop_capture joins before a new start), so this state
                # needs no lock. Clock ties bump by one tick so pts is
                # strictly increasing.
                now = time.monotonic()
                if self._video_pts_anchor is None:
                    self._video_pts_anchor = now
                pts = int((now - self._video_pts_anchor) * 90000)
                if pts <= self._last_video_pts:
                    pts = self._last_video_pts + 1
                self._last_video_pts = pts
                # consume_data is synchronous now (its bridge put no longer awaits),
                # so schedule it with the lighter call_soon_threadsafe -- no per-frame
                # Future/Task allocation -- matching the websockets path.
                self.async_event_loop.call_soon_threadsafe(
                    self.produce_data, data_bytes, pts, "video"
                )

        except Exception as e:
            logger.error(f"Error in capture callback: {e}", exc_info=False)

    def _pixelflux_cursor_handler(self, msg_type, data_bytes, hot_x, hot_y):
        """pixelflux cursor events (either backend) -> client cursor messages
        (websockets parity). Runs on the capture-side thread; delivery hops to
        the asyncio loop."""
        try:
            size = int(getattr(app_settings, "cursor_size", -1) or -1)
            if size <= 0:
                size = 24
            payload = format_pixelflux_cursor(msg_type, data_bytes, hot_x, hot_y, size)
            if payload is None:
                return
            self.async_event_loop.call_soon_threadsafe(self.on_cursor_data, payload)
        except Exception as e:
            logger.error(f"Error handling pixelflux cursor: {e}")

    async def start_screen_capture(self):
        if self._is_screen_capturing:
            return

        if ScreenCapture is None or CaptureSettings is None:
            # pixelflux absent/ABI-skewed: fail clearly here, not at import.
            raise MediaPipelineError(
                "pixelflux is unavailable (missing or ABI/version-skewed wheel); "
                "cannot start screen capture"
            )

        settings = self.generate_capture_settings()

        try:
            self.capture_module = ScreenCapture()
            # pixelflux is the cursor source on both backends (compositor on
            # Wayland, XFixes monitor on X11). An older X11-only pixelflux
            # stashes this harmlessly and the input handler's python monitor
            # keeps delivering instead.
            self.capture_module.set_cursor_callback(self._pixelflux_cursor_handler)
            await asyncio.to_thread(
                self.capture_module.start_capture,
                settings,
                self._screen_capture_callback,
            )
            self._is_screen_capturing = True
            logger.info("Started screen capture module")
        except Exception as e:
            logger.error(f"Failed to start screen capture: {e}", exc_info=True)
            self.capture_module = None
            self._is_screen_capturing = False
            # Propagate so start_media_pipeline neither marks itself running nor
            # fires on_pipeline_started: a swallowed failure here reads as a live
            # stream to the transport while nothing is captured.
            raise MediaPipelineError(f"screen capture failed to start: {e}") from e

    async def update_capture_region(self, x: int, y: int, w: int, h: int):
        """Re-target the live capture to a new region of the extended framebuffer
        (no restart); falls back to a restart when no live capture exists."""
        self.capture_region = (int(x), int(y))
        self.width, self.height = int(w), int(h)
        if self._is_screen_capturing and self.capture_module is not None:
            try:
                await asyncio.to_thread(
                    self.capture_module.update_capture_region, int(x), int(y), int(w), int(h)
                )
                return
            except Exception as e:
                logger.warning(f"Live capture re-target failed ({e}); restarting capture.")
        await self.restart_screen_capture()

    async def stop_screen_capture(self):
        if not self._is_screen_capturing or self.capture_module is None:
            return
        try:
            await asyncio.to_thread(self.capture_module.stop_capture)
            self.capture_module = None
            self._is_screen_capturing = False
            logger.info("Stopped screen capture module")
        except Exception as e:
            logger.error(f"Error stopping screen capture: {e}", exc_info=True)
            self.capture_module = None
            self._is_screen_capturing = False

    async def restart_screen_capture(self):
        async with self.async_lock:
            # Checked under the lock: a concurrent stop_media_pipeline may have
            # stopped capture while we waited, and a restart must not resurrect it.
            if not self._is_screen_capturing:
                return
            try:
                # Start on the LIVE module: pixelflux applies the new settings in
                # place (a compatible Wayland encoder session survives; X11 cycles
                # the capture internally). Only structural changes land here --
                # rate/quality knobs go through the live update_* paths.
                settings = self.generate_capture_settings()
                await asyncio.to_thread(
                    self.capture_module.start_capture,
                    settings,
                    self._screen_capture_callback,
                )
                logger.info("Screen capture reconfigured")
            except Exception as e:
                logger.error(f"Error restarting screen capture: {e}")

    async def _start_audio_pipeline(self):
        if self._is_pcmflux_capturing:
            return

        if AudioCapture is None or AudioCaptureSettings is None:
            # pcmflux absent/ABI-skewed: skip audio (best-effort) without aborting
            # the already-running video pipeline.
            logger.error(
                "pcmflux is unavailable (missing or ABI/version-skewed wheel); "
                "skipping audio capture"
            )
            return

        logger.info("Starting pcmflux audio pipeline...")
        try:
            capture_settings = AudioCaptureSettings()
            device_name_bytes = (
                self.audio_device_name.encode("utf-8")
                if self.audio_device_name
                else None
            )
            capture_settings.device_name = device_name_bytes
            capture_settings.sample_rate = 48000
            capture_settings.channels = self.audio_channels
            capture_settings.opus_bitrate = int(self.audio_bitrate)
            # Same latency-floor consideration as the WebSocket path; RTP has no fixed
            # ptime requirement, so shorter Opus frames flow through unchanged.
            frame_ms = float(getattr(app_settings, 'audio_frame_duration_ms', '20') or 20)
            capture_settings.frame_duration_ms = frame_ms
            # VBR (matches the WebSocket path): opus_bitrate is the target average
            # the encoder varies around by content complexity — better quality per
            # bit than CBR. RTP carries variable Opus payloads fine and browsers
            # decode VBR natively (no cbr=1 in the offer fmtp to contradict).
            capture_settings.use_vbr = True
            capture_settings.use_silence_gate = False
            capture_settings.latency_ms = int(min(10, frame_ms))
            capture_settings.debug_logging = False
            # WebRTC repacketizes into RTP, so it needs raw Opus: disable pcmflux's
            # 2-byte header.
            capture_settings.omit_audio_header = True
            pcmflux_settings = capture_settings

            logger.info(
                f"pcmflux settings: device='{self.audio_device_name}', "
                f"bitrate={capture_settings.opus_bitrate}, channels={capture_settings.channels}"
            )

            def audio_capture_callback(frame):
                try:
                    if len(frame) > 0:
                        # zero-copy view; consume_data wraps it in av.Packet(buf) (zero-copy)
                        # and keeps a reference so `frame` stays alive.
                        data_bytes = memoryview(frame)

                        self.async_event_loop.call_soon_threadsafe(
                            self.produce_data, data_bytes, frame.pts, "audio"
                        )
                except Exception as e:
                    logger.info(f"Error audio capture callback: {e}")

            self.pcmflux_module = AudioCapture()
            await asyncio.to_thread(
                self.pcmflux_module.start_capture,
                pcmflux_settings,
                audio_capture_callback,
            )
            self._is_pcmflux_capturing = True
            # Keep a reference: an unreferenced task can be garbage-collected
            # mid-flight, and a routing failure should be visible in the log.
            self._audio_routing_task = asyncio.create_task(self._enforce_audio_routing())
            self._audio_routing_task.add_done_callback(
                lambda t: (not t.cancelled() and t.exception() is not None)
                and logger.error(f"Audio routing task failed: {t.exception()}")
            )
            logger.info("pcmflux audio capture started successfully.")
        except Exception as e:
            logger.error(f"Failed to start pcmflux audio pipeline: {e}", exc_info=True)
            await self._stop_audio_pipeline()
            return

    async def _pactl(self, *args):
        """Run pactl and return its stdout ('' on failure). The PA control plane is
        driven via subprocess on this path: the in-process asyncio PA bindings can
        run a native event callback against freed state under load (observed
        SIGSEGV in the loop during peer churn), and these are rare one-shot ops."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "pactl", *args,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            out, err = await asyncio.wait_for(proc.communicate(), timeout=5)
            if proc.returncode != 0:
                logger.warning(
                    f"pactl {' '.join(args)} failed: {err.decode(errors='replace').strip()}"
                )
                return ""
            return out.decode(errors="replace")
        except Exception as e:
            logger.warning(f"pactl {' '.join(args)} failed: {e}")
            return ""

    async def _list_sources(self):
        """{name: index} of current sources via `pactl list short sources`."""
        sources = {}
        for line in (await self._pactl("list", "short", "sources")).splitlines():
            parts = line.split("\t")
            if len(parts) >= 2:
                sources[parts[1]] = parts[0]
        return sources

    async def _enforce_audio_routing(self):
        """
        PipeWire often ignores requested audio device and connects recording apps
        to the default source. This could happen when switching between
        streaming modes. So route the pcmflux stream to correct source.
        """
        # Give pcmflux a fraction of a second to initialize its PA stream
        await asyncio.sleep(0.5)
        try:
            sources = await self._list_sources()
            correct_index = sources.get(self.audio_device_name)
            if correct_index is None:
                logger.warning(
                    f"Routing enforcement: Target source '{self.audio_device_name}' not found."
                )
                return

            blob = await self._pactl("list", "source-outputs")
            for block in blob.split("Source Output #")[1:]:
                index = block.split("\n", 1)[0].strip()
                app = re.search(r'application\.name = "([^"]*)"', block)
                if not app or app.group(1) != "pcmflux":
                    continue
                current = re.search(r"^\s*Source:\s*(\d+)", block, re.M)
                if current and current.group(1) == correct_index:
                    logger.info(
                        f"WebRTC pcmflux correctly connected to '{self.audio_device_name}'"
                    )
                else:
                    logger.warning(
                        f"WebRTC pcmflux connected to the wrong source, moving to "
                        f"'{self.audio_device_name}'"
                    )
                    await self._pactl(
                        "move-source-output", index, self.audio_device_name
                    )
                    logger.info(
                        f"Requested move of WebRTC pcmflux to '{self.audio_device_name}'"
                    )
                break
        except Exception as e:
            logger.error(f"Error enforcing WebRTC audio routing: {e}")

    async def _ensure_audio_device(self):
        """
        Verify the configured audio_device_name is a valid source.
        If not, attempt to fallback to the default sink's monitor
        """
        try:
            default_sink_name = (await self._pactl("get-default-sink")).strip()
            default_monitor_name = None
            if default_sink_name:
                logger.info(
                    f"Default sink from PulseAudio/PipeWire: '{default_sink_name}'"
                )
                default_monitor_name = f"{default_sink_name}.monitor"
            else:
                logger.warning("Could not determine default sink.")

            available_sources = set(await self._list_sources())
            if not available_sources:
                logger.error("Failed to enumerate audio sources.")
                return

            if self.audio_device_name and self.audio_device_name in available_sources:
                logger.info(
                    f"Configured audio device '{self.audio_device_name}' is valid."
                )
            else:
                if self.audio_device_name:
                    logger.warning(
                        f"Configured audio device '{self.audio_device_name}' not found "
                        f"in available sources."
                    )
                # Fallback to default sink's monitor if available
                if default_monitor_name and default_monitor_name in available_sources:
                    logger.info(
                        f"Falling back to default sink monitor: '{default_monitor_name}'"
                    )
                    self.audio_device_name = default_monitor_name
                elif "auto_null.monitor" in available_sources:
                    logger.info(
                        "Default sink monitor not available; falling back to 'auto_null.monitor'"
                    )
                    # Pipewiere's default sink monitor
                    self.audio_device_name = "auto_null.monitor"
                else:
                    logger.error(
                        "No valid audio source found. Audio capture will likely fail. "
                        f"Available sources: {sorted(available_sources)}"
                    )
        except Exception as e:
            logger.error(f"Error validating the audio device: {e}")

    async def _stop_audio_pipeline(self):
        if not self._is_pcmflux_capturing or not self.pcmflux_module:
            return

        logger.info("Stopping pcmflux audio pipeline...")
        self._is_pcmflux_capturing = False
        if self.pcmflux_module:
            try:
                await asyncio.to_thread(self.pcmflux_module.stop_capture)
            except Exception as e:
                logger.error(f"Error during pcmflux stop_capture: {e}")
            finally:
                self.pcmflux_module = None

            logger.info("pcmflux audio pipeline stopped.")
        return

    async def start_media_pipeline(self):
        async with self.async_lock:
            if self._running:
                return

            logger.info("Starting media pipeline...")
            try:
                await self.start_screen_capture()

                if self.audio_enabled:
                    await self._ensure_audio_device()
                    await self._start_audio_pipeline()
                else:
                    logger.info(
                        "Audio pipeline is disabled, skipping audio capture startup."
                    )
                self._running = True
                # Notify the transport video is live (to restore the cursor); a
                # callback failure must not abort a successfully started pipeline.
                try:
                    self.on_pipeline_started()
                except Exception:
                    logger.warning(
                        "on_pipeline_started callback raised; pipeline remains running",
                        exc_info=True,
                    )
            except Exception as e:
                logger.error(f"Error starting media pipelines: {e}", exc_info=True)
                # Inline teardown: stop_media_pipeline() would re-enter the held lock and deadlock.
                try:
                    await self.stop_screen_capture()
                    if self.audio_enabled:
                        await self._stop_audio_pipeline()
                except Exception:
                    logger.error("Error during start-failure cleanup", exc_info=True)
                self._running = False

    async def stop_media_pipeline(self):
        async with self.async_lock:
            if not self._running:
                return

            logger.info("Stopping media pipeline...")
            try:
                await self.stop_screen_capture()

                if self.audio_enabled:
                    await self._stop_audio_pipeline()
                self._running = False
            except Exception as e:
                logger.error(f"Error stopping media pipelines: {e}", exc_info=True)

    def is_media_pipeline_running(self):
        return self._running
