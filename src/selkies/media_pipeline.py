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
import time
from enum import Enum
from abc import ABCMeta, abstractmethod
from typing import Callable, Awaitable

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
    def start_media_pipeline(self):
        pass

    @abstractmethod
    def stop_media_pipeline(self):
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
    async def set_video_bitrate(self, bitrate: int):
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
        video_bitrate: int = 8,
        audio_bitrate: int = 128000,
        width: int = 1920,
        height: int = 1080,
        audio_channels: int = 2,
        audio_enabled: bool = True,
        audio_device_name = 'output.monitor',
        crf: int = 23,
        rc_mode: RateControlMode = RateControlMode.CBR
    ):
        self.async_event_loop = async_event_loop
        self.audio_channels = audio_channels
        self.encoder_rtc = encoder_rtc
        self.framerate = framerate
        self.video_bitrate = video_bitrate
        self.rc_mode = rc_mode
        # FIXME: h264_crf variable name could be encoder agnostic
        self.h264_crf = crf
        self.audio_bitrate = audio_bitrate
        self.last_resize_success = True
        self.width = width
        self.height = height
        self.audio_enabled = audio_enabled
        self.audio_device_name = audio_device_name
        self.capture_cursor = False
        self.produce_data: Callable[[bytes, int, str], Awaitable[None]] = lambda buf, pts, kind: logger.warning('unhandled produce_data')
        self.send_data_channel_message = lambda msg: logger.warning('unhandled send_data_channel_message')

        self.capture_module = None
        self.pcmflux_module = None
        self._is_screen_capturing = False
        self._is_pcmflux_capturing = False
        self._running = False
        self.async_lock = asyncio.Lock()
        self._video_pts_anchor = None
        self._last_video_pts = -1

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
        except AttributeError:
            logger.error("Video capture module does not support rate control mode updation")
        except Exception as e:
            logger.info(f"Error updating rate control mode {e}", exc_info=True)

    async def set_crf(self, new_crf: int):
        """Set video encoder target CRF.

        :new_crf: CRF value
        """
        if not self._is_screen_capturing or self.capture_module is None:
            return

        if self.rc_mode != RateControlMode.CRF or self.h264_crf == new_crf:
            return

        old_crf = self.h264_crf
        self.h264_crf = new_crf
        try:
            await self.restart_screen_capture()
            logger.info(f"Updated CRF: {old_crf} -> {new_crf}")
        except AttributeError:
            logger.error("Video capture module does not support CRF updation")
        except Exception as e:
            logger.info(f"Error updating CRF {e}", exc_info=True)

    async def set_video_bitrate(self, new_bitrate: int):
        """Set video encoder target bitrate.

        :new_bitrate: bitrate in mbps
        """
        if not self._is_screen_capturing or self.capture_module is None:
            return

        if self.rc_mode == RateControlMode.CRF or new_bitrate <= 0 or self.video_bitrate == new_bitrate:
            return

        try:
            await self.async_event_loop.run_in_executor(None, self.capture_module.update_video_bitrate, new_bitrate * 1000)
            logger.info(f"Updated video bitrate: {self.video_bitrate}Mbps -> {new_bitrate}Mbps")
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
        cs.auto_adjust_screen_capture_size = True
        cs.omit_stripe_headers = True

        if self.encoder_rtc in ["h264enc", "openh264enc", "nvh264enc", "x264enc"]:
            cs.video_streaming_mode = True
            cs.video_fullframe = True
            cs.video_crf = self.h264_crf
            cs.video_cbr_mode = self.rc_mode == RateControlMode.CBR
            cs.video_bitrate_kbps = self.video_bitrate * 1000
            if self.encoder_rtc == "openh264enc":
                cs.use_cpu = True
                cs.use_openh264 = True
            elif self.use_cpu:
                cs.use_cpu = True
        return cs

    def _screen_capture_callback(self, frame):
        try:
            if len(frame) > 0:
                data_bytes = memoryview(frame)
                now = time.monotonic()
                if self._video_pts_anchor is None:
                    self._video_pts_anchor = now
                pts = int((now - self._video_pts_anchor) * 90000)
                if pts <= self._last_video_pts:
                    pts = self._last_video_pts + 1
                self._last_video_pts = pts
                asyncio.run_coroutine_threadsafe(
                    self.produce_data(data_bytes, pts, "video"),
                    self.async_event_loop,
                )
        except Exception as e:
            logger.error(f"Error in capture callback: {e}", exc_info=False)

    async def start_screen_capture(self):
        if self._is_screen_capturing:
            return

        if ScreenCapture is None or CaptureSettings is None:
            raise MediaPipelineError(
                "pixelflux is unavailable (missing or ABI/version-skewed wheel); "
                "cannot start screen capture"
            )

        settings = self.generate_capture_settings()

        try:
            self.capture_module = ScreenCapture()
            await self.async_event_loop.run_in_executor(None, self.capture_module.start_capture, settings, self._screen_capture_callback)
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
            if not self._is_screen_capturing:
                return
            try:
                settings = self.generate_capture_settings()
                await self.async_event_loop.run_in_executor(
                    None, self.capture_module.start_capture, settings, self._screen_capture_callback
                )
                logger.info("Screen capture reconfigured")
            except Exception as e:
                logger.error(f"Error restarting screen capture: {e}")

    async def _start_audio_pipeline(self):
        if self._is_pcmflux_capturing:
            return

        if AudioCapture is None or AudioCaptureSettings is None:
            logger.error(
                "pcmflux is unavailable (missing or ABI/version-skewed wheel); "
                "skipping audio capture"
            )
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
            capture_settings.use_vbr = True
            capture_settings.use_silence_gate = False
            capture_settings.latency_ms = 10
            capture_settings.debug_logging = False
            pcmflux_settings = capture_settings

            logger.info(f"pcmflux settings: device='{self.audio_device_name}', "
                        f"bitrate={capture_settings.opus_bitrate}, channels={capture_settings.channels}")

            def audio_capture_callback(frame):
                try:
                    if len(frame) > 0:
                        data_bytes = memoryview(frame)
                        asyncio.run_coroutine_threadsafe(
                            self.produce_data(data_bytes, frame.pts, "audio"),
                            self.async_event_loop,
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
                    await self._start_audio_pipeline()
                self._running = True
            except Exception as e:
                logger.error(f"Error starting media pipelines: {e}", exc_info=True)
                await self.stop_media_pipeline()

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
