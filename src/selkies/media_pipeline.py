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
import ctypes
import pulsectl_asyncio
from enum import Enum
from abc import ABCMeta, abstractmethod
from typing import Callable, Awaitable

from pixelflux import CaptureSettings, ScreenCapture
from pcmflux import AudioCapture, AudioCaptureSettings, AudioChunkCallback

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
        audio_device_name="output.monitor",
        crf: int = 23,
        rc_mode: RateControlMode = RateControlMode.CBR,
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
        self.produce_data: Callable[[bytes, int, str], Awaitable[None]] = lambda buf, pts, kind: logger.warning(
            "unhandled produce_data"
        )
        self.send_data_channel_message: Callable[[str], None] = lambda msg: logger.warning(
            "unhandled send_data_channel_message"
        )

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
            logger.error(
                "Video capture module does not support rate control mode updation"
            )
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

        if (
            self.rc_mode == RateControlMode.CRF
            or new_bitrate <= 0
            or self.video_bitrate == new_bitrate
        ):
            return

        try:
            await self.async_event_loop.run_in_executor(
                None, self.capture_module.update_video_bitrate, new_bitrate * 1000
            )
            logger.info(
                f"Updated video bitrate: {self.video_bitrate}Mbps -> {new_bitrate}Mbps"
            )
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

        if new_bitrate <= 0 or self.audio_bitrate == new_bitrate:
            return

        try:
            await self.async_event_loop.run_in_executor(
                None, self.pcmflux_module.update_audio_bitrate, new_bitrate
            )
            logger.info(
                f"Updated audio bitrate: {self.audio_bitrate // 1000} -> {new_bitrate // 1000} kbps"
            )
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
            await self.async_event_loop.run_in_executor(
                None, self.capture_module.update_framerate, float(self.framerate)
            )
            logger.info(f"Updated framerate to: {self.framerate}")

    async def dynamic_idr_frame(self):
        """Requests an IDR frame from pixelflux"""
        if not self._is_screen_capturing or self.capture_module is None:
            return
        try:
            await self.async_event_loop.run_in_executor(
                None, self.capture_module.request_idr_frame
            )
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

        if self.encoder_rtc in ["nvh264enc", "x264enc"]:
            cs.h264_streaming_mode = True
            cs.h264_fullframe = True
            cs.h264_crf = self.h264_crf
            # Setting h264_cbr_mode to True will make the encoder ignore the crf value
            cs.h264_cbr_mode = self.rc_mode == RateControlMode.CBR
            cs.h264_bitrate_kbps = self.video_bitrate * 1000  # Convert Mbps to kbps
            cs.vaapi_render_node_index = -1
            if self.encoder_rtc == "x264enc":
                cs.use_cpu = True
        return cs

    async def start_screen_capture(self):
        if self._is_screen_capturing:
            return

        settings = self.generate_capture_settings()

        def screen_capture_callback(result_ptr, _):
            if not result_ptr:
                return
            try:
                result = result_ptr.contents
                if result.size > 0:
                    data_bytes = bytes(result.data[10 : result.size])
                    if not hasattr(result, "frame_id"):
                        logger.error(
                            f"Missing frame_id from screen capture result, skipping frame"
                        )
                    else:
                        # Generate pts from frame_id
                        pts_step = 90000 // self.framerate
                        pts = result.frame_id * pts_step
                        asyncio.run_coroutine_threadsafe(
                            self.produce_data(data_bytes, pts, "video"),
                            self.async_event_loop,
                        )

            except Exception as e:
                logger.error(f"Error in capture callback: {e}", exc_info=False)

        try:
            self.capture_module = ScreenCapture()
            await self.async_event_loop.run_in_executor(
                None,
                self.capture_module.start_capture,
                settings,
                screen_capture_callback,
            )
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
            await self.async_event_loop.run_in_executor(
                None, self.capture_module.stop_capture
            )
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
            device_name_bytes = (
                self.audio_device_name.encode("utf-8")
                if self.audio_device_name
                else None
            )
            capture_settings.device_name = device_name_bytes
            capture_settings.sample_rate = 48000
            capture_settings.channels = self.audio_channels
            capture_settings.opus_bitrate = int(self.audio_bitrate)
            capture_settings.frame_duration_ms = 20
            capture_settings.use_vbr = False
            capture_settings.use_silence_gate = False
            capture_settings.latency_ms = 10
            capture_settings.debug_logging = False
            pcmflux_settings = capture_settings

            logger.info(
                f"pcmflux settings: device='{self.audio_device_name}', "
                f"bitrate={capture_settings.opus_bitrate}, channels={capture_settings.channels}"
            )

            def audio_capture_callback(result_ptr, user_data):
                if not result_ptr:
                    return
                try:
                    result = result_ptr.contents
                    if result.data and result.size > 0:
                        data_bytes = bytes(
                            ctypes.cast(
                                result.data,
                                ctypes.POINTER(ctypes.c_ubyte * result.size),
                            ).contents
                        )

                        asyncio.run_coroutine_threadsafe(
                            self.produce_data(data_bytes, result.pts, "audio"),
                            self.async_event_loop,
                        )
                except Exception as e:
                    logger.info(f"Error audio capture callback: {e}")

            pcmflux_callback = AudioChunkCallback(audio_capture_callback)
            self.pcmflux_module = AudioCapture()
            await self.async_event_loop.run_in_executor(
                None,
                self.pcmflux_module.start_capture,
                pcmflux_settings,
                pcmflux_callback,
            )
            self._is_pcmflux_capturing = True
            asyncio.create_task(self._enforce_audio_routing())
            logger.info("pcmflux audio capture started successfully.")
        except Exception as e:
            logger.error(f"Failed to start pcmflux audio pipeline: {e}", exc_info=True)
            await self._stop_audio_pipeline()
            return

    async def _enforce_audio_routing(self):
        """
        PipeWire often ignores requested audio device and connects recording apps
        to the default source. This could happen when switching between
        streaming modes. So route the pcmflux stream to correct source.
        """
        # Give pcmflux a fraction of a second to initialize its PA stream
        await asyncio.sleep(0.5)
        pulse = None
        try:
            pulse = pulsectl_asyncio.PulseAsync("selkies-webrtc-router")
            await pulse.connect()
        except Exception as e:
            logger.error(
                f"Failed to connect to PulseAudio for routing enforcement: {e}"
            )
            return

        try:
            current_source_list = await pulse.source_list()
            correct_source = None
            for s in current_source_list:
                if s.name == self.audio_device_name:
                    correct_source = s
                    break

            if not correct_source:
                logger.warning(
                    f"Routing enforcement: Target source '{self.audio_device_name}' not found."
                )
                return

            source_outputs = await pulse.source_output_list()
            for output in source_outputs:
                app_name = output.proplist.get("application.name", "")
                if app_name == "pcmflux":
                    if output.source != correct_source.index:
                        connected_source_name = "Unknown"
                        for s in current_source_list:
                            if s.index == output.source:
                                connected_source_name = s.name
                                break
                        logger.warning(
                            f"WebRTC pcmflux connected to wrong source "
                            f"'{connected_source_name}', moving to '{correct_source.name}'"
                        )
                        try:
                            await pulse.source_output_move(output.index, correct_source.index)
                            logger.info(
                                f"Successfully moved WebRTC pcmflux to '{correct_source.name}'"
                            )
                        except Exception as move_e:
                            logger.error(f"Failed to move WebRTC pcmflux: {move_e}")
                    else:
                        logger.info(
                            f"WebRTC pcmflux correctly connected to '{correct_source.name}'"
                        )
                    break
        except Exception as e:
            logger.error(f"Error enforcing WebRTC audio routing: {e}")
        finally:
            if pulse is not None:
                pulse.close()

    async def _ensure_audio_device(self):
        """
        Verify the configured audio_device_name is a valid source.
        If not, attempt to fallback to the default sink's monitor
        """
        pulse = None
        try:
            pulse = pulsectl_asyncio.PulseAsync("selkies-media-pipeline")
            await pulse.connect()
        except Exception as e:
            logger.error(f"Failed to connect to PulseAudio/PipeWire: {e}")
            return

        try:
            default_sink_name = None
            default_monitor_name = None
            try:
                server_info = await pulse.server_info()
                default_sink_name = server_info.default_sink_name
                logger.info(
                    f"Default sink from PulseAudio/PipeWire: '{default_sink_name}'"
                )
                if default_sink_name:
                    default_monitor_name = f"{default_sink_name}.monitor"
            except Exception as e:
                logger.warning(f"Could not determine default sink: {e}")

            available_sources = set()
            try:
                sources = await pulse.source_list()
                for src in sources:
                    available_sources.add(src.name)
            except Exception as e:
                logger.error(f"Failed to enumerate audio sources: {e}")
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
            logger.error(f"Error enforcing WebRTC audio routing: {e}")
        finally:
            if pulse is not None:
                pulse.close()

    async def _stop_audio_pipeline(self):
        if not self._is_pcmflux_capturing or not self.pcmflux_module:
            return

        logger.info("Stopping pcmflux audio pipeline...")
        self._is_pcmflux_capturing = False
        if self.pcmflux_module:
            try:
                await self.async_event_loop.run_in_executor(
                    None, self.pcmflux_module.stop_capture
                )
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
