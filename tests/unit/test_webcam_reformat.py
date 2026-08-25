#!/usr/bin/env python3
"""Re-creating the virtual webcam when the uplink changes kind.

The device format follows the first uplink, and the camera outlives the session that brought it
up: applications hold /dev/videoN open across mode switches. A later uplink of the other kind
therefore finds a device it does not fit -- a video uplink fitted into an MJPEG device costs a
decode and a re-encode of every frame, and an MJPEG uplink into a raw device a decode where the
bytes could have passed through. The camera is re-created for it, but only while nothing is
reading it: each sink answers for its own consumers, and a sink that cannot say leaves the
device alone.
"""
import asyncio
import os
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "src"))

failures = 0


def check(name: str, ok: bool, detail: str = "") -> None:
    global failures
    print(("PASS  " if ok else "FAIL  ") + name + (f"  {detail}" if detail and not ok else ""))
    if not ok:
        failures += 1


class FakeCamera:
    """A started pixelflux.VirtualCamera, as much of one as the decision reads."""

    def __init__(self, stats):
        self._stats = stats
        self.stopped = False
        self.socket_path = "/tmp/selkies-webcam.sock"
        self.device_path = ""

    def start(self, settings):
        self.settings = settings

    def stats(self):
        return dict(self._stats)

    def stop(self):
        self.stopped = True


def make(wc, pixel_format="auto", stats=None):
    """A VirtualWebcam holding a started fake camera of the given device format."""
    cam = wc.VirtualWebcam()
    started = FakeCamera(stats if stats is not None else {"clients": 0, "pipewire_streaming": False})
    cam._cam = started
    cam._device_mjpeg = pixel_format == "MJPEG"
    wc.app_settings.webcam_pixel_format = "auto"
    return cam, started


async def run_checks() -> None:
    """Every check, inside a running loop: on Python 3.9 ``asyncio.Lock()`` binds to the
    current event loop when it is constructed, and VirtualWebcam builds one."""
    from selkies import webcam as wc

    original = wc.app_settings.webcam_pixel_format
    try:
        cam, _ = make(wc, "MJPEG")
        check("an MJPEG uplink into an MJPEG device asks nothing", cam.needs_ensure(wc.CODEC_MJPEG) is False)
        check("a video uplink into an MJPEG device asks", cam.needs_ensure(wc.CODEC_H264) is True)

        cam, _ = make(wc, "I420")
        check("a video uplink into a raw device asks nothing", cam.needs_ensure(wc.CODEC_VP8) is False)
        check("an MJPEG uplink into a raw device asks", cam.needs_ensure(wc.CODEC_MJPEG) is True)
        check("no camera always asks", wc.VirtualWebcam().needs_ensure(wc.CODEC_H264) is True)

        wc.app_settings.webcam_pixel_format = "MJPEG"
        cam, _ = make(wc, "MJPEG")
        wc.app_settings.webcam_pixel_format = "MJPEG"
        check("a pinned format is never second-guessed", cam.needs_ensure(wc.CODEC_H264) is False)
        wc.app_settings.webcam_pixel_format = "auto"

        cam, _ = make(wc, "MJPEG", {"clients": 2, "pipewire_streaming": False})
        check("an interposer client holds the device", cam._consumers() == "interposer client")

        cam, _ = make(wc, "MJPEG", {"clients": 0, "pipewire_streaming": True})
        check("a linked PipeWire consumer holds the device", cam._consumers() == "PipeWire consumer")

        cam, _ = make(wc, "MJPEG", {"clients": 0, "pipewire": True})
        held = cam._consumers()
        check("a pixelflux that cannot report PipeWire consumers holds the device",
              held is not None and "PipeWire" in held, str(held))

        cam, _ = make(wc, "MJPEG", {"clients": 0, "pipewire_streaming": False, "pipewire": True})
        check("a PipeWire node with nothing linked does not hold the device", cam._consumers() is None)

        # A real opener in another process, which is the only thing /proc can show.
        with tempfile.NamedTemporaryFile(prefix="selkies-webcam-", suffix=".dev") as node:
            cam, _ = make(wc, "MJPEG", {"clients": 0, "pipewire_streaming": False,
                                        "device_path": node.name})
            check("a device nothing has open does not hold it", cam._consumers() is None)
            holder = subprocess.Popen(
                [sys.executable, "-c",
                 "import sys, time; f = open(sys.argv[1]); sys.stdout.write('x'); "
                 "sys.stdout.flush(); time.sleep(30)", node.name],
                stdout=subprocess.PIPE)
            try:
                holder.stdout.read(1)
                held = cam._consumers()
                check("an application holding the kernel device holds it",
                      held is not None and "holding" in held, str(held))
            finally:
                holder.terminate()
                holder.wait(timeout=10)

        # The re-create itself: with nothing reading, the camera is stopped and started again in
        # the format the new uplink wants; with a reader, the same camera is kept.
        started = []

        class Recording(FakeCamera):
            def __init__(self):
                super().__init__({"clients": 0, "pipewire_streaming": False})

            def start(self, settings):
                super().start(settings)
                started.append(settings.pixel_format)

        wc.VirtualCamera = Recording
        wc.VirtualCameraSettings = lambda: type("S", (), {})()
        wc.webcam_available = lambda: True

        cam, old = make(wc, "MJPEG")
        got = await cam.ensure(wc.CODEC_H264)
        check("an unread device is re-created for the video uplink",
              old.stopped and started[-1:] == ["I420"] and got is not old, f"{started} stopped={old.stopped}")

        cam, old = make(wc, "MJPEG", {"clients": 1, "pipewire_streaming": False})
        before = list(started)
        got = await cam.ensure(wc.CODEC_H264)
        check("a device with a reader is kept as it is",
              got is old and not old.stopped and started == before, f"{started} stopped={old.stopped}")
        check("and is not asked again until the floor passes", cam.needs_ensure(wc.CODEC_H264) is False)
    finally:
        wc.app_settings.webcam_pixel_format = original


def main() -> int:
    asyncio.run(run_checks())
    print(f"\n{failures} failure(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
