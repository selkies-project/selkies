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

"""WebRTC-side media pipeline over pixelflux (video) and pcmflux (audio).

Owns one display's capture: pixelflux encodes H.264 on its own capture
thread, pcmflux encodes Opus on its own audio thread, and both hand
zero-copy buffers back into the asyncio loop via `call_soon_threadsafe` for
the transport's `produce_data` to packetize as RTP. Because RTP senders are
live across capture restarts, the pipeline keeps its own monotonic pts
clocks (video: 90 kHz wall-clock anchor; audio: an epoch offset over
pcmflux's re-zeroing sample clock) so pts never jumps backward.

Tunables split two ways, mirroring the WebSockets path: rate/quality knobs
(bitrate, CRF, framerate, streaming mode, paint-over) apply live through
pixelflux's non-blocking update calls, while structural changes (encoder,
CPU/GPU, full color, rate-control mode) restart the capture on the live
module. Every setter stores its value first so changes made while capture is
paused shape the next start.

The pixelflux/pcmflux imports are guarded: plain WebSocket mode and module
import must survive their absence, so capture starts raise a clear error
instead of the import failing.
"""

import asyncio
import logging
import time
from enum import Enum
from abc import ABCMeta, abstractmethod
from typing import Any, Callable, Optional, Tuple

from .settings import settings as app_settings
from .audio_control import AudioControl
from .display_utils import apply_common_capture_settings, format_pixelflux_cursor

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
    """Video rate-control mode: constant bitrate or constant quality (CRF)."""
    CBR = "cbr"
    CRF = "crf"


class MediaPipelineError(Exception):
    """Raised when a capture pipeline cannot start or is unavailable."""
    pass


class MediaPipeline(metaclass=ABCMeta):
    """Interface a transport drives to run and tune a media pipeline."""

    @abstractmethod
    async def start_media_pipeline(self) -> None:
        pass

    @abstractmethod
    async def stop_media_pipeline(self) -> None:
        pass

    @abstractmethod
    def is_media_pipeline_running(self) -> bool:
        pass

    @abstractmethod
    async def set_pointer_visible(self, visible: bool) -> None:
        pass

    @abstractmethod
    async def set_framerate(self, framerate: int) -> None:
        pass

    @abstractmethod
    async def set_video_bitrate(self, bitrate: int) -> None:
        pass

    @abstractmethod
    async def set_audio_bitrate(self, bitrate: int) -> None:
        pass

    @abstractmethod
    async def dynamic_idr_frame(self) -> None:
        pass

    @abstractmethod
    async def update_rate_control_mode(self, mode: RateControlMode) -> None:
        pass

    @abstractmethod
    async def set_crf(self, crf: int) -> None:
        pass


class MediaPipelinePixel(MediaPipeline):
    """pixelflux/pcmflux implementation of MediaPipeline for one display.

    Capture threads deliver encoded frames to `_screen_capture_callback` /
    the audio callback, which hop onto the asyncio loop and hand zero-copy
    memoryviews to `produce_data`. The transport wires `produce_data`,
    `on_pipeline_started`, `on_cursor_data`, and `get_cursor_size_cap`
    before starting the pipeline.
    """

    def __init__(
        self,
        async_event_loop: asyncio.AbstractEventLoop,
        encoder: str,
        framerate: int = 30,
        video_bitrate: int = 8000,
        audio_bitrate: int = 128000,
        width: int = 1920,
        height: int = 1080,
        audio_channels: int = 2,
        audio_enabled: bool = True,
        audio_device_name: str = "output.monitor",
        crf: int = 23,
        rc_mode: RateControlMode = RateControlMode.CBR,
        video_fullcolor: bool = False,
        use_cpu: bool = False,
        video_streaming_mode: bool = True,
        use_paint_over_quality: bool = True,
        video_paintover_crf: int = 18,
        video_paintover_burst_frames: int = 5,
        display_id: str = "primary",
        capture_region: Optional[Tuple[int, int]] = None,
    ) -> None:
        self.async_event_loop = async_event_loop
        # Which display this pipeline feeds and, for a secondary display, the
        # region of the extended framebuffer it captures ((x, y) origin; the
        # dimensions ride self.width/self.height). None captures from (0, 0)
        # with auto size — the single-display behavior.
        self.display_id = display_id or "primary"
        self.capture_region: Optional[Tuple[int, int]] = capture_region
        self.audio_channels = audio_channels
        self.encoder = encoder
        self.framerate = framerate
        self.video_bitrate = video_bitrate
        self.rc_mode = rc_mode
        self.video_crf = crf
        self.video_fullcolor = video_fullcolor
        # Force the software H.264 encoder (x264, or OpenH264 in a GPL-free
        # pixelflux build). WS honors this too; a change is structural, so it
        # restarts capture.
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

        # pixelflux ScreenCapture / pcmflux AudioCapture instances; typed Any
        # because both imports are optional.
        self.capture_module: Any = None
        self.pcmflux_module: Any = None
        self._is_screen_capturing = False
        self._is_pcmflux_capturing = False
        self._running = False
        self.async_lock = asyncio.Lock()
        # Video pts clock; pipeline-scoped (not capture-scoped) so capture
        # restarts and fps changes can never rewind pts on a live RTP sender.
        self._video_pts_anchor: Optional[float] = None
        self._last_video_pts = -1
        # Audio pts continuity: pcmflux's sample clock re-zeros on every capture
        # restart, which is a backward RTP jump on a live sender. Anchor each new
        # capture epoch one frame step past the last emitted pts instead. Only
        # one capture thread exists at a time (stop joins before a new start).
        self._audio_capture_epoch = 0
        self._audio_cb_epoch = -1
        self._audio_pts_offset = 0
        self._audio_last_pts = -1
        self._audio_frame_samples = 480
        self._audio_routing_task: Optional[asyncio.Task] = None
        # Sound-server control connection (sink provisioning, device
        # resolution, pcmflux routing); opened on the first audio start and
        # closed with the capture.
        self._audio_control: Optional[AudioControl] = None
        # Monotonic time of the last audio-recovery restart; floors the retry
        # rate when a device keeps failing (recover_audio_if_failed).
        self._audio_recover_last_attempt = 0.0

    async def set_pointer_visible(self, visible: bool) -> None:
        """Toggle pixelflux cursor capture.

        Args:
            visible: True to capture (and deliver) the pointer cursor.
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

    async def update_rate_control_mode(self, mode: RateControlMode) -> None:
        """Set rate control mode for the video encoder.

        Args:
            mode: Either RateControlMode.CBR or RateControlMode.CRF. A mode
                switch is structural, so it restarts the capture.
        """
        if mode not in [RateControlMode.CBR, RateControlMode.CRF]:
            logger.error(f"Invalid rate control mode: {mode}")
            return

        if mode == self.rc_mode:
            return

        # Store first: a change sent while capture is paused must still shape the
        # next start_screen_capture() (WS parity), not vanish.
        self.rc_mode = mode
        if not self._is_screen_capturing or self.capture_module is None:
            return

        try:
            await self.restart_screen_capture()
            logger.info(f"Updated rate control mode to: {self.rc_mode}")
        except Exception as e:
            logger.info(f"Error updating rate control mode {e}", exc_info=True)

    async def set_crf(self, crf: int) -> None:
        """Set the video encoder's target CRF (applied live in CRF mode).

        Args:
            crf: Constant-rate-factor value; lower is higher quality.
        """
        if self.video_crf == crf:
            return

        # Store first, whatever the active mode: generate_capture_settings() always
        # carries a CRF, so a value chosen while CBR is active (or while capture is
        # paused) is the one a later switch to CRF encodes at instead of a stale one.
        old_crf = self.video_crf
        self.video_crf = crf
        if self.rc_mode != RateControlMode.CRF:
            return
        if not self._is_screen_capturing or self.capture_module is None:
            return
        try:
            # Live retarget: every encoder re-reads the CRF per frame; no restart.
            self.capture_module.update_tunables(self.generate_capture_settings())
            logger.info(f"Updated CRF live: {old_crf} -> {crf}")
        except Exception as e:
            # Keep the stored CRF equal to what the encoder is still using, so the
            # equality guard above lets a retry of this same value through.
            self.video_crf = old_crf
            logger.info(f"Error updating CRF {e}", exc_info=True)

    async def set_use_cpu(self, use_cpu: bool) -> None:
        """Switch h264enc between software (the pixelflux build's encoder) and
        hardware encoding.

        Unlike CRF/bitrate this is structural (a different encoder instance), so
        it restarts the capture instead of going through the live update path.
        """
        if self.use_cpu == use_cpu:
            return
        self.use_cpu = use_cpu
        # With no capture running the value applies at the next start_screen_capture().
        if not self._is_screen_capturing or self.capture_module is None:
            return
        logger.info(f"use_cpu -> {use_cpu}; restarting screen capture")
        await self.restart_screen_capture()

    async def _apply_tunables_live(self, what: str) -> None:
        """Push current capture settings to the running module with no restart."""
        if not self._is_screen_capturing or self.capture_module is None:
            return
        try:
            self.capture_module.update_tunables(self.generate_capture_settings())
            logger.info(f"Updated {what} live")
        except Exception as e:
            logger.info(f"Error updating {what}: {e}", exc_info=True)

    async def set_video_fullcolor(self, fullcolor: bool) -> None:
        """Toggle 4:4:4 color. Structural (pixel format), so restart capture (WS parity)."""
        if self.video_fullcolor == fullcolor:
            return
        self.video_fullcolor = fullcolor
        if not self._is_screen_capturing or self.capture_module is None:
            return
        logger.info(f"video_fullcolor -> {fullcolor}; restarting screen capture")
        await self.restart_screen_capture()

    async def set_encoder(self, encoder: str) -> None:
        """Switch the WebRTC video encoder (h264enc is the only one it can
        stream). Structural (a different encoder instance), so restart capture —
        same as use_cpu (WS parity)."""
        if self.encoder == encoder:
            return
        self.encoder = encoder
        if not self._is_screen_capturing or self.capture_module is None:
            return
        logger.info(f"encoder -> {encoder}; restarting screen capture")
        await self.restart_screen_capture()

    async def set_video_streaming_mode(self, enabled: bool) -> None:
        """Toggle Turbo (stream every frame vs damage-gated). Live tunable."""
        if self.video_streaming_mode == enabled:
            return
        self.video_streaming_mode = enabled
        await self._apply_tunables_live(f"streaming mode -> {enabled}")

    async def set_use_paint_over_quality(self, enabled: bool) -> None:
        if self.use_paint_over_quality == enabled:
            return
        self.use_paint_over_quality = enabled
        await self._apply_tunables_live(f"paint-over -> {enabled}")

    async def set_video_paintover_crf(self, crf: int) -> None:
        if self.video_paintover_crf == crf:
            return
        self.video_paintover_crf = crf
        await self._apply_tunables_live(f"paint-over CRF -> {crf}")

    async def set_video_paintover_burst_frames(self, frames: int) -> None:
        if self.video_paintover_burst_frames == frames:
            return
        self.video_paintover_burst_frames = frames
        await self._apply_tunables_live(f"paint-over burst -> {frames}")

    async def set_video_bitrate(self, bitrate: int) -> None:
        """Set the video encoder's target bitrate (applied live in CBR mode).

        Args:
            bitrate: Target bitrate in kbps.
        """
        if bitrate <= 0 or self.video_bitrate == bitrate:
            return

        # Store first, whatever the active mode: generate_capture_settings() always
        # carries a bitrate, so a value chosen while CRF is active (or while capture
        # is paused) is the one a later switch to CBR encodes at.
        old_bitrate = self.video_bitrate
        self.video_bitrate = bitrate
        if self.rc_mode == RateControlMode.CRF:
            return
        if not self._is_screen_capturing or self.capture_module is None:
            return
        try:
            # Non-blocking in pixelflux (atomic store / channel send).
            self.capture_module.update_video_bitrate(int(bitrate))
            logger.info(
                f"Updated video bitrate: {old_bitrate} -> {bitrate} kbps"
            )
        except Exception as e:
            # Congestion control reads self.video_bitrate as the rate currently
            # being encoded and steers from it: leave it at the value the encoder
            # kept, which also lets the equality guard retry this one.
            self.video_bitrate = old_bitrate
            logger.info(f"Error updating video bitrate {e}", exc_info=True)

    async def set_audio_bitrate(self, bitrate: int) -> None:
        """Set the Opus encoder's target bitrate.

        Args:
            bitrate: Target bitrate in bps.
        """
        if bitrate <= 0 or self.audio_bitrate == bitrate:
            return

        # Store before the live call so a stopped pcmflux applies it on restart.
        old_bitrate = self.audio_bitrate
        self.audio_bitrate = bitrate
        if not self._is_pcmflux_capturing or self.pcmflux_module is None:
            return
        try:
            # Non-blocking in pcmflux (atomic store).
            self.pcmflux_module.update_audio_bitrate(bitrate)
            logger.info(
                f"Updated audio bitrate: {old_bitrate // 1000} -> {bitrate // 1000} kbps"
            )
        except Exception as e:
            # Restart fallback (websockets parity): the stored value is what the
            # fresh capture starts at, so the encoder never keeps running at a
            # rate the pipeline no longer reports.
            logger.warning(
                f"Live audio bitrate update failed ({e}); restarting audio pipeline."
            )
            await self._stop_audio_pipeline()
            await self._start_audio_pipeline()

    async def set_framerate(self, framerate: int) -> None:
        """Set the pixelflux capture rate (applied live).

        Args:
            framerate: Frames per second, for example 15, 30, 60.
        """
        async with self.async_lock:
            if framerate <= 0 or self.framerate == framerate:
                return

            # Store first: a paused capture applies the value at its next start.
            self.framerate = framerate
            if not self._is_screen_capturing or self.capture_module is None:
                return
            # Non-blocking in pixelflux (atomic store / channel send).
            self.capture_module.update_framerate(float(self.framerate))
            logger.info(f"Updated framerate to: {self.framerate}")

    async def dynamic_idr_frame(self) -> None:
        """Request an IDR frame from pixelflux."""
        if not self._is_screen_capturing or self.capture_module is None:
            return
        try:
            # Non-blocking in pixelflux (atomic flag / channel send).
            self.capture_module.request_idr_frame()
            logger.debug("IDR frame requested successfully")
        except Exception as e:
            logger.error(f"Error requesting IDR frame: {e}", exc_info=True)

    def generate_capture_settings(self) -> Any:
        """Build the pixelflux CaptureSettings snapshot for the current state.

        Returns:
            A populated `pixelflux.CaptureSettings` (annotated as Any because
            the import is optional).
        """
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
        cs.output_mode = 1
        # WebRTC has its own RTP framing; both backends omit pixelflux's per-stripe
        # header, so there is nothing for Python to strip. frame_id comes from the
        # frame attribute, not the header.
        self._omit_stripe_headers = True
        cs.omit_stripe_headers = self._omit_stripe_headers
        apply_common_capture_settings(
            cs, app_settings,
            is_wayland=bool(app_settings.wayland[0]),
            display_name=self.display_id,
            scale=self.scale,
            framerate=self.framerate,
            encoder=self.encoder,
            use_cpu=self.use_cpu,
            cbr=self.rc_mode == RateControlMode.CBR,
            bitrate_kbps=self.video_bitrate,
            crf=self.video_crf,
            paintover_crf=self.video_paintover_crf,
            paintover_burst=self.video_paintover_burst_frames,
            fullcolor=self.video_fullcolor,
            streaming=self.video_streaming_mode,
            use_paint_over_quality=self.use_paint_over_quality,
            capture_cursor=self.capture_cursor,
            cursor_size_cap_hint=int(self.get_cursor_size_cap() or 0),
        )
        return cs

    def _screen_capture_callback(self, frame: Any) -> None:
        """Deliver one encoded video frame; runs on the pixelflux capture thread."""
        try:
            hdr = 0 if self._omit_stripe_headers else 10
            if len(frame) > hdr:
                # frame owns its native buffer; pass a zero-copy memoryview (sliced
                # past the header) downstream. consume_data wraps it in EncodedPacket
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
                # consume_data is synchronous, so the lighter call_soon_threadsafe
                # is enough -- no per-frame Future/Task allocation -- matching the
                # websockets path.
                self.async_event_loop.call_soon_threadsafe(
                    self.produce_data, data_bytes, pts, "video"
                )

        except Exception as e:
            logger.error(f"Error in capture callback: {e}", exc_info=False)

    def _pixelflux_cursor_handler(
        self, msg_type: str, data_bytes: Optional[bytes], hot_x: int, hot_y: int
    ) -> None:
        """Translate pixelflux cursor events (either backend) into client cursor
        messages (websockets parity). Runs on the capture-side thread; delivery
        hops to the asyncio loop."""
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

    async def start_screen_capture(self) -> None:
        """Start pixelflux screen capture at the current settings snapshot.

        Raises:
            MediaPipelineError: When pixelflux is unavailable or the capture
                fails to start, so the caller never reports a live stream
                while nothing is captured.
        """
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
                self._screen_capture_callback,
                settings,
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

    async def update_capture_region(self, x: int, y: int, w: int, h: int) -> None:
        """Re-target the live capture to a new region of the extended framebuffer
        (no restart); falls back to a restart when no live capture exists."""
        self.capture_region = (int(x), int(y))
        self.width, self.height = int(w), int(h)
        if (self._is_screen_capturing and self.capture_module is not None
                and not bool(app_settings.wayland[0])):
            # X11-only live re-target; on Wayland the output IS the capture
            # region and a start on the live module reconfigures it in place.
            try:
                await asyncio.to_thread(
                    self.capture_module.update_capture_region, int(x), int(y), int(w), int(h)
                )
                return
            except Exception as e:
                logger.warning(f"Live capture re-target failed ({e}); restarting capture.")
        await self.restart_screen_capture()

    async def stop_screen_capture(self) -> None:
        """Stop the pixelflux capture, dropping the module even on error."""
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

    async def pause_screen_capture(self) -> bool:
        """Consumer-aware pause: stop only the screen capture (audio and the
        pipeline's running state survive) once every consumer of this display
        is hidden; resume_screen_capture restarts it.

        Returns:
            Whether a live capture was actually stopped.
        """
        async with self.async_lock:
            if not self._running or not self._is_screen_capturing:
                return False
            await self.stop_screen_capture()
            return True

    async def resume_screen_capture(self) -> bool:
        """Restart a capture stopped by pause_screen_capture, at the CURRENT
        settings (a resize/scale received while paused applies here).

        Returns:
            Whether a stopped capture was actually started.
        """
        async with self.async_lock:
            if not self._running or self._is_screen_capturing:
                return False
            await self.start_screen_capture()
            return True

    async def restart_screen_capture(self) -> None:
        """Reapply the current settings snapshot to the live capture module."""
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
                    self._screen_capture_callback,
                    settings,
                )
                logger.info("Screen capture reconfigured")
            except Exception as e:
                logger.error(f"Error restarting screen capture: {e}")

    async def _start_audio_pipeline(self) -> None:
        """Start pcmflux Opus capture; best-effort (video survives its absence)."""
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

        # The capture sink first, the device check second (websockets order):
        # a PipeWire that comes up with no sink has no output.monitor to
        # validate against yet, and the check would rewrite the device name to
        # auto_null.monitor for good — a source the first mic enable retires.
        control = self._get_audio_control()
        await control.ensure_capture_sink(self.audio_device_name)
        await self._ensure_audio_device()
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
            self._audio_frame_samples = max(1, int(48000 * frame_ms / 1000))
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

            def audio_capture_callback(frame: Any) -> None:
                """Deliver one Opus frame; runs on the pcmflux capture thread."""
                try:
                    if len(frame) > 0:
                        # zero-copy view; consume_data wraps it in EncodedPacket (zero-copy)
                        # and keeps a reference so `frame` stays alive.
                        data_bytes = memoryview(frame)
                        # Map the per-capture sample clock onto a continuous one:
                        # pcmflux re-zeros pts on every start, and a backward RTP
                        # jump on a live sender plays as an audio glitch/loss.
                        raw_pts = int(frame.pts)
                        if self._audio_cb_epoch != self._audio_capture_epoch:
                            self._audio_cb_epoch = self._audio_capture_epoch
                            self._audio_pts_offset = (
                                self._audio_last_pts + self._audio_frame_samples - raw_pts
                                if self._audio_last_pts >= 0 else 0
                            )
                        pts = self._audio_pts_offset + raw_pts
                        self._audio_last_pts = pts

                        self.async_event_loop.call_soon_threadsafe(
                            self.produce_data, data_bytes, pts, "audio"
                        )
                except Exception as e:
                    logger.info(f"Error audio capture callback: {e}")

            self.pcmflux_module = AudioCapture()
            # Bump the epoch before the capture thread can deliver: the callback
            # re-anchors pts past the last one this pipeline emitted only when it
            # observes a new epoch, so a frame that lands first would otherwise
            # carry the previous epoch's offset over a re-zeroed sample clock
            # (see _audio_cb_epoch).
            self._audio_capture_epoch += 1
            await asyncio.to_thread(
                self.pcmflux_module.start_capture,
                pcmflux_settings,
                audio_capture_callback,
            )
            self._is_pcmflux_capturing = True
            self._audio_recover_last_attempt = 0.0
            # Keep a reference: an unreferenced task can be garbage-collected
            # mid-flight, and a routing failure should be visible in the log.
            self._audio_routing_task = asyncio.create_task(self._enforce_audio_routing())
            self._audio_routing_task.add_done_callback(
                lambda t: (not t.cancelled() and t.exception() is not None)
                and logger.error(f"Audio routing task failed: {t.exception()}")
            )
            # start_capture returns Ok while the worker is still bringing the
            # device up ("starting"), so this reports what pcmflux actually
            # reached; a device that dies during bring-up surfaces through
            # last_error on the next health poll (recover_audio_if_failed).
            state = getattr(self.pcmflux_module, "state", "running")
            logger.info(f"pcmflux audio capture started (state: {state}).")
        except Exception as e:
            logger.error(f"Failed to start pcmflux audio pipeline: {e}", exc_info=True)
            await self._stop_audio_pipeline()
            return

    def _get_audio_control(self) -> AudioControl:
        """The sound-server control client, created on the pipeline's loop."""
        if self._audio_control is None:
            self._audio_control = AudioControl("selkies-webrtc-audio")
        return self._audio_control

    async def _enforce_audio_routing(self) -> None:
        """Move the pcmflux stream onto the configured source if PipeWire strayed.

        PipeWire often ignores the requested audio device and connects
        recording apps to the default source, particularly when switching
        between streaming modes.
        """
        # Give pcmflux a fraction of a second to initialize its PA stream.
        await asyncio.sleep(0.5)
        try:
            await self._get_audio_control().route_pcmflux([self.audio_device_name])
        except Exception as e:
            logger.error(f"Error enforcing WebRTC audio routing: {e}")

    async def _ensure_audio_device(self) -> None:
        """Verify audio_device_name is a valid source, else fall back to the
        default sink's monitor (or PipeWire's `auto_null.monitor`)."""
        try:
            resolved = await self._get_audio_control().resolve_capture_source(
                self.audio_device_name
            )
            if resolved:
                self.audio_device_name = resolved
        except Exception as e:
            logger.error(f"Error validating the audio device: {e}")

    async def _stop_audio_pipeline(self) -> None:
        """Cancel routing enforcement, release the sound-server control
        connection, and stop the pcmflux capture.

        Routing enforcement belongs to the capture session that armed it: a
        stop inside its start-up delay must not move a stream that is gone.
        """
        task = self._audio_routing_task
        self._audio_routing_task = None
        if task is not None:
            task.cancel()
            # return_exceptions: the task's own cancellation is the expected
            # outcome; only a cancel of this coroutine propagates.
            await asyncio.gather(task, return_exceptions=True)
        if self._audio_control is not None:
            await self._audio_control.aclose()
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

    async def recover_audio_if_failed(self) -> bool:
        """Restart the audio capture if its worker has died, else no-op.

        pcmflux reports the start handshake as Ok while the worker is still
        "starting", so a device that fails right after start surfaces only
        later, as ``last_error`` on the capture object. This is polled where
        pipeline health is already evaluated and cycles the audio capture
        through the normal stop/start path; the video pipeline is untouched.
        A short floor between attempts keeps a permanently broken device from
        restarting every tick.

        Returns:
            True when a restart was attempted.
        """
        if not self.audio_enabled or not self._is_pcmflux_capturing:
            return False
        module = self.pcmflux_module
        if module is None:
            return False
        err = getattr(module, "last_error", None)
        if not err:
            return False
        now = time.monotonic()
        if now - self._audio_recover_last_attempt < 5.0:
            return False
        self._audio_recover_last_attempt = now
        async with self.async_lock:
            # Re-check under the lock: a concurrent restart may already have
            # replaced the module, or the pipeline may have stopped.
            if self.pcmflux_module is not module or not self._running:
                return False
            logger.warning(f"pcmflux audio worker failed ({err}); restarting audio capture.")
            await self._stop_audio_pipeline()
            await self._start_audio_pipeline()
        return True

    async def start_media_pipeline(self) -> None:
        """Start video (and, when enabled, audio) capture and mark the pipeline
        running; a start failure tears both back down and stays stopped."""
        async with self.async_lock:
            if self._running:
                return

            logger.info("Starting media pipeline...")
            try:
                await self.start_screen_capture()

                if self.audio_enabled:
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

    async def stop_media_pipeline(self) -> None:
        """Stop both captures and mark the pipeline stopped."""
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

    def is_media_pipeline_running(self) -> bool:
        return self._running

    def is_screen_capturing(self) -> bool:
        """True once the screen capture runs, which precedes `_running` (the
        audio half of the pipeline may still be starting); settings that must
        reach a live capture key on this, not on the whole pipeline."""
        return self._is_screen_capturing
