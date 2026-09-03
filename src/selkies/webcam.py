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
only stopped with the server. Its format follows the first uplink, so a browser
sending JPEG gets an MJPEG device its frames pass through untouched and a
WebCodecs/WebRTC browser an I420 device. A later uplink of the other kind would
otherwise be converted for the life of the process, so an ``auto`` device is
re-created for it — but only while no sink reports a consumer, since an
application holding the device is exactly what the process-wide lifetime is for
(``VirtualWebcam.ensure``).

The WebSocket carries one encoded frame per ``0x06`` message: opcode, codec
id, flags, payload. Besides the keyframe bit the flags byte carries the
frame's upright transform, which the client's encoder leaves out of the
bitstream: bits 1-2 are the clockwise rotation in quarter turns, bit 3 a
horizontal flip applied after the rotation.
"""

import asyncio
import logging
import os
import time
from typing import Any, Dict, Optional, Tuple

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

WS_OPCODE_WEBCAM = 0x06
WS_HEADER_LEN = 3
WS_FLAG_KEYFRAME = 0x01
WS_FLAG_ROTATION_SHIFT = 1
WS_FLAG_ROTATION_MASK = 0x06
WS_FLAG_HFLIP = 0x08


def orientation_from_flags(flags: int) -> Tuple[int, bool]:
    """Clockwise rotation degrees and horizontal flip carried by a ``0x06`` flags byte."""
    rotation = ((flags & WS_FLAG_ROTATION_MASK) >> WS_FLAG_ROTATION_SHIFT) * 90
    return rotation, bool(flags & WS_FLAG_HFLIP)

# Text control messages the server sends back on the WebSocket.
MSG_WEBCAM_DISABLED = "WEBCAM_DISABLED"
MSG_WEBCAM_KEYFRAME = "WEBCAM_KEYFRAME"


def webcam_available() -> bool:
    """Whether the pixelflux extension carrying the virtual camera is importable."""
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


# Floor between the checks that ask whether anything still reads the camera. The answer only
# changes when an application opens or closes the device, while the uplink asks on every frame.
REFORMAT_RECHECK_SECONDS = 5.0


def _device_has_openers(path: str) -> bool:
    """Whether any other process holds `path` open.

    Only this process's own view of /proc is available, which in a container is every process
    that matters; a reader it cannot see is why re-creating the device asks the sinks first.
    """
    try:
        target = os.path.realpath(path)
        me = str(os.getpid())
    except OSError:
        return False
    for pid in os.listdir("/proc"):
        if not pid.isdigit() or pid == me:
            continue
        fd_dir = f"/proc/{pid}/fd"
        try:
            for fd in os.listdir(fd_dir):
                try:
                    if os.path.realpath(os.path.join(fd_dir, fd)) == target:
                        return True
                except OSError:
                    continue
        except OSError:
            continue
    return False


class VirtualWebcam:
    """Lazy owner of the process-wide ``pixelflux.VirtualCamera``.

    The camera is started on the first frame rather than at server start, so sessions that never
    enable the webcam pay nothing; a start failure is logged once and retried on a later frame.
    """

    def __init__(self) -> None:
        self._cam: Optional[Any] = None
        self._lock = asyncio.Lock()
        self._start_failed_logged = False
        self._device_mjpeg = False
        self._reformat_blocked_logged = False
        self._reformat_next_check = 0.0

    @property
    def camera(self) -> Optional[Any]:
        """The running camera, or None until the first successful start."""
        return self._cam

    def needs_ensure(self, codec: Optional[int]) -> bool:
        """Whether ``ensure`` has work to do for a frame of this codec.

        The per-frame answer for a running camera whose format already suits the uplink is no,
        which is the whole of the hot path; the rest is the rare case where an ``auto`` device
        was shaped by an uplink of the other kind and may be worth re-creating.
        """
        if self._cam is None:
            return True
        # The kind comparison first: it answers every frame of a running camera, and the
        # setting lookup below builds a string the hot path has no use for.
        if codec is None or (codec == CODEC_MJPEG) == self._device_mjpeg:
            return False
        if not self._auto_format():
            return False
        return time.monotonic() >= self._reformat_next_check

    @staticmethod
    def _auto_format() -> bool:
        return str(app_settings.webcam_pixel_format or "auto").strip().lower() == "auto"

    def _consumers(self) -> Optional[str]:
        """What is reading the camera right now, or None when nothing is.

        Each sink answers for its own: the interposer counts the clients on its socket,
        PipeWire reports whether a consumer is linked to its node, and a kernel device's
        openers are found the only way a process can, through /proc. A pixelflux
        that cannot report the node's consumers is taken to have one: a re-created
        device would take the picture away from whoever is watching.
        """
        cam = self._cam
        if cam is None:
            return None
        try:
            stats = cam.stats()
        except Exception:
            return "unknown"
        if int(stats.get("clients", 0) or 0) > 0:
            return "interposer client"
        if "pipewire_streaming" in stats:
            if stats.get("pipewire_streaming"):
                return "PipeWire consumer"
        elif stats.get("pipewire"):
            return "a PipeWire node whose consumers this pixelflux does not report"
        device = str(stats.get("device_path") or "")
        if device and _device_has_openers(device):
            return f"an application holding {device}"
        return None

    def _settings(self, codec: Optional[int]) -> Any:
        s = VirtualCameraSettings()
        s.socket_path = webcam_socket_path()
        s.width = int(app_settings.webcam_width)
        s.height = int(app_settings.webcam_height)
        s.fps_num = 30
        s.fps_den = 1
        s.pixel_format = device_pixel_format(str(app_settings.webcam_pixel_format), codec)
        s.device_path = str(app_settings.webcam_device)
        return s

    async def ensure(self, codec: Optional[int] = None) -> Optional[Any]:
        """Returns the running camera, starting it on first use (off the event loop).

        An ``auto`` device already running in the other format is re-created for this codec
        when nothing is reading it, so a session that follows one of the other kind neither
        transcodes every frame (a video uplink fitted into an MJPEG device) nor decodes one it
        could have passed through. A device with a consumer is left exactly as it is:
        applications hold it open across mode switches, and pulling the format out from under
        one is worse than the transcode.

        Args:
            codec: Codec of the frame that brings the camera up, or that found the running
                device in the other format; an ``auto`` device format follows it (see
                ``device_pixel_format``).
        """
        if self._cam is not None:
            if not self.needs_ensure(codec):
                return self._cam
            async with self._lock:
                if self._cam is None or not self.needs_ensure(codec):
                    return self._cam
                reader = await asyncio.to_thread(self._consumers)
                if reader is not None:
                    self._reformat_next_check = time.monotonic() + REFORMAT_RECHECK_SECONDS
                    if not self._reformat_blocked_logged:
                        self._reformat_blocked_logged = True
                        logger.info(
                            "Virtual webcam stays %s for %s: %s is reading it. Frames are "
                            "converted for the device; pin webcam_pixel_format to avoid it.",
                            "MJPEG" if self._device_mjpeg else "raw",
                            "an MJPEG uplink" if codec == CODEC_MJPEG else "a video uplink", reader)
                    return self._cam
                logger.info("Virtual webcam re-created for the %s uplink now that nothing reads it.",
                            "MJPEG" if codec == CODEC_MJPEG else "video")
                await self._stop_locked()
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
            self._device_mjpeg = str(settings.pixel_format).strip().upper() in ("MJPEG", "MJPG", "JPEG")
            self._reformat_blocked_logged = False
            self._reformat_next_check = 0.0
            stats = cam.stats()
            logger.info("Virtual webcam serving %s (%dx%d %s, kernel device: %s, PipeWire node: %s)",
                        cam.socket_path, settings.width, settings.height, settings.pixel_format,
                        cam.device_path or "none", "yes" if stats.get("pipewire") else "no")
            return cam

    def push(self, data: Any, codec: int, keyframe: bool = False, offset: int = 0,
             rotation: int = 0, flip: bool = False) -> int:
        """Hands one encoded frame to the camera; returns its flags (``KEYFRAME_WANTED`` bit).

        Args:
            rotation: Clockwise degrees (0/90/180/270) that make the decoded frame upright.
            flip: Horizontal mirror, applied after the rotation.

        A camera that is not running yet (``ensure`` pending) drops the frame silently.
        """
        cam = self._cam
        if cam is None:
            return 0
        try:
            return int(cam.push(data, codec, keyframe, offset, rotation, flip))
        except Exception as exc:
            logger.error("Virtual webcam push failed: %s", exc)
            return 0

    def keyframe_wanted(self, flags: int) -> bool:
        return bool(flags & getattr(VirtualCamera, "KEYFRAME_WANTED", 1))

    async def _stop_locked(self) -> None:
        """Stops the running camera; the caller holds the lock."""
        cam = self._cam
        self._cam = None
        if cam is not None:
            try:
                await asyncio.to_thread(cam.stop)
            except Exception:
                pass

    async def stop(self) -> None:
        async with self._lock:
            await self._stop_locked()


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
