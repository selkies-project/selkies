# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Virtual webcam: the client's camera uplink, served to applications as a V4L2 device.

Both transports deliver encoded camera frames here — the WebSocket as ``0x06``
binary messages carrying WebCodecs output, WebRTC as the depacketized frames of
the recvonly video m-line — and this module forwards them to the process-wide
``pixelflux.VirtualCamera``. Decoding, fitting, and publishing to every device
sink (the shared-memory ring behind the ``selkies_v4l2_interposer.so``
``LD_PRELOAD`` library, and a v4l2loopback device where one is configured)
happen on pixelflux's own thread; Python only gates the sender and hands the
bytes over without copying them.

Lifetime is process-wide, like the gamepad interposer servers: applications
open ``/dev/videoN`` once at their own startup and must survive transport-mode
switches and browser reconnects, so one camera is shared by every client and is
only stopped with the server. Its format is fixed when it comes up: by default
it follows the first uplink, so a browser sending JPEG gets an MJPEG device its
frames pass through untouched and a WebCodecs/WebRTC browser an I420 device.
"""

import asyncio
import logging
import os
from typing import Any, Dict, Optional

try:
    from pixelflux import VirtualCamera, VirtualCameraSettings
except (ImportError, RuntimeError):
    VirtualCamera = VirtualCameraSettings = None

from .settings import settings as app_settings

logger = logging.getLogger("webcam")

WEBCAM_SOCKET_NAME = "selkies_webcam0.sock"

# Codec ids shared by the WebSocket frame header, the WebRTC codec names and
# pixelflux's VirtualCamera.CODEC_* constants.
CODEC_MJPEG = 0
CODEC_H264 = 1
CODEC_VP8 = 2
CODEC_VP9 = 3
CODEC_AV1 = 4
CODEC_HEVC = 5
CODEC_BY_NAME: Dict[str, int] = {
    "mjpeg": CODEC_MJPEG, "jpeg": CODEC_MJPEG, "h264": CODEC_H264, "vp8": CODEC_VP8,
    "vp9": CODEC_VP9, "av1": CODEC_AV1, "hevc": CODEC_HEVC, "h265": CODEC_HEVC,
}

# WebSocket framing of one encoded frame: opcode, codec id, flags, payload.
WS_OPCODE_WEBCAM = 0x06
WS_HEADER_LEN = 3
WS_FLAG_KEYFRAME = 0x01

# Text control messages the server sends back on the WebSocket.
MSG_WEBCAM_DISABLED = "WEBCAM_DISABLED"
MSG_WEBCAM_KEYFRAME = "WEBCAM_KEYFRAME"


def webcam_available() -> bool:
    """Whether the installed pixelflux provides the virtual camera."""
    return VirtualCamera is not None


def webcam_locked_off() -> bool:
    """True when the operator locked the webcam off, so no client may feed it."""
    enabled, locked = app_settings.webcam_enabled
    return locked and not enabled


def webcam_uplink_allowed(is_viewer: bool, is_collaborator: bool) -> bool:
    """The shared gate for both transports: controllers and authorized collaborators may feed
    the camera, read-only viewers may not, and nobody may when it is locked off."""
    if webcam_locked_off():
        return False
    return not is_viewer or is_collaborator


def webcam_socket_path() -> str:
    """Full path of the interposer socket, inside the configured socket directory."""
    return os.path.join(app_settings.webcam_socket_path or "/tmp", WEBCAM_SOCKET_NAME)


def device_pixel_format(setting: str, codec: Optional[int]) -> str:
    """The device format for a ``webcam_pixel_format`` value and the codec of the frame that
    brings the camera up.

    ``auto`` follows that first uplink: an MJPEG uplink (a browser without WebCodecs) becomes
    an MJPEG device carrying the browser's JPEG frames as received, with no decode on the
    server, and anything else an I420 device; an explicit format is used as given.
    """
    name = (setting or "auto").strip()
    if name.lower() != "auto":
        return name
    return "MJPEG" if codec == CODEC_MJPEG else "I420"


class VirtualWebcam:
    """Lazy owner of the process-wide ``pixelflux.VirtualCamera``.

    The camera is started on the first frame rather than at server start, so sessions that never
    enable the webcam pay nothing; a start failure is logged once and retried on a later frame.
    """

    def __init__(self) -> None:
        self._cam: Optional[Any] = None
        self._lock = asyncio.Lock()
        self._start_failed_logged = False

    @property
    def camera(self) -> Optional[Any]:
        """The running camera, or None until the first successful start."""
        return self._cam

    def _settings(self, codec: Optional[int]) -> Any:
        s = VirtualCameraSettings()
        s.socket_path = webcam_socket_path()
        s.width = int(app_settings.webcam_width)
        s.height = int(app_settings.webcam_height)
        s.fps_num = 30
        s.fps_den = 1
        s.pixel_format = device_pixel_format(str(app_settings.webcam_pixel_format), codec)
        s.device_path = str(app_settings.webcam_device)
        s.pipewire = bool(app_settings.webcam_pipewire[0])
        return s

    async def ensure(self, codec: Optional[int] = None) -> Optional[Any]:
        """Returns the running camera, starting it on first use (off the event loop).

        Args:
            codec: Codec of the frame that brings the camera up; an ``auto`` device format
                follows it (see ``device_pixel_format``).
        """
        if self._cam is not None:
            return self._cam
        if not webcam_available():
            if not self._start_failed_logged:
                self._start_failed_logged = True
                logger.error("pixelflux VirtualCamera unavailable; webcam forwarding disabled.")
            return None
        async with self._lock:
            if self._cam is not None:
                return self._cam
            cam = VirtualCamera()
            try:
                settings = self._settings(codec)
                await asyncio.to_thread(cam.start, settings)
            except Exception as exc:
                if not self._start_failed_logged:
                    self._start_failed_logged = True
                    logger.error("Virtual webcam start failed: %s", exc)
                return None
            self._cam = cam
            stats = cam.stats()
            logger.info("Virtual webcam serving %s (%dx%d %s, kernel device: %s, PipeWire node: %s)",
                        cam.socket_path, settings.width, settings.height, settings.pixel_format,
                        cam.device_path or "none", "yes" if stats.get("pipewire") else "no")
            return cam

    def push(self, data: Any, codec: int, keyframe: bool = False, offset: int = 0) -> int:
        """Hands one encoded frame to the camera; returns its flags (``KEYFRAME_WANTED`` bit).

        A camera that is not running yet (``ensure`` pending) drops the frame silently.
        """
        cam = self._cam
        if cam is None:
            return 0
        try:
            return int(cam.push(data, codec, keyframe, offset))
        except Exception as exc:
            logger.error("Virtual webcam push failed: %s", exc)
            return 0

    def keyframe_wanted(self, flags: int) -> bool:
        return bool(flags & getattr(VirtualCamera, "KEYFRAME_WANTED", 1))

    async def stop(self) -> None:
        cam = self._cam
        self._cam = None
        if cam is not None:
            try:
                await asyncio.to_thread(cam.stop)
            except Exception:
                pass


_shared_webcam: Optional[VirtualWebcam] = None


def get_shared_webcam() -> VirtualWebcam:
    """The process-wide webcam (created lazily, started on the first frame)."""
    global _shared_webcam
    if _shared_webcam is None:
        _shared_webcam = VirtualWebcam()
    return _shared_webcam


async def stop_shared_webcam() -> None:
    """Stops and clears the process-wide webcam, if any."""
    global _shared_webcam
    cam = _shared_webcam
    _shared_webcam = None
    if cam is not None:
        await cam.stop()
