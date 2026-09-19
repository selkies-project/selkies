#!/usr/bin/env python3
"""What each device counts as a reader, and the settings that turn the watches on.

The camera answers from its sinks with the opposite default to the format decision: an
answer a build cannot give leaves a device alone there and asks for no camera here. Its
/proc walk is held to one per floor on the demand path and stays fresh for the format path.
The microphone counts uncorked streams recording from the virtual source and not the
module's own bridge from the master. Neither touches a real device: the camera is a fake
pixelflux, the sound server a fake backend.
"""
import asyncio
import os
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "src"))

from selkies import capture_demand as cd  # noqa: E402
from selkies import webcam as wc  # noqa: E402
from selkies.audio_control import VIRTUAL_MIC_SOURCE_NAMES, AudioControl, PulseNode  # noqa: E402
from selkies.settings import pipeline_starts_on, settings  # noqa: E402

failures = 0


def check(name: str, ok: bool, detail: str = "") -> None:
    global failures
    print(("PASS  " if ok else "FAIL  ") + name + (f"  {detail}" if detail and not ok else ""))
    if not ok:
        failures += 1


def run_setting_checks() -> None:
    originals = (settings.webcam_enabled, settings.webcam_on_start,
                 settings.microphone_enabled, settings.microphone_on_start)
    try:
        for subject in ("webcam", "microphone"):
            setattr(settings, f"{subject}_enabled", (True, False))
            for policy in ("false", "true"):
                setattr(settings, f"{subject}_on_start", policy)
                check(f"{subject}: the {policy} policy is not on demand", cd.on_demand(subject) is False)
            check(f"{subject}: the true policy starts the uplink at connect", pipeline_starts_on(subject) is True)
            setattr(settings, f"{subject}_on_start", "demand")
            check(f"{subject}: the demand policy turns the watch on", cd.on_demand(subject) is True)
            check(f"{subject}: and starts nothing at connect", pipeline_starts_on(subject) is False)
            setattr(settings, f"{subject}_enabled", (False, True))
            check(f"{subject}: a device locked off is never asked for", cd.on_demand(subject) is False)
            setattr(settings, f"{subject}_enabled", (False, False))
            check(f"{subject}: merely off is not locked off", cd.on_demand(subject) is True)
    finally:
        (settings.webcam_enabled, settings.webcam_on_start,
         settings.microphone_enabled, settings.microphone_on_start) = originals


class FakeCamera:
    """A started pixelflux.VirtualCamera whose interposer client count the test sets."""

    clients = 0
    formats: list = []

    def __init__(self) -> None:
        self.socket_path = "/tmp/selkies-webcam.sock"
        self.device_path = ""

    def start(self, settings) -> None:
        FakeCamera.formats.append(settings.pixel_format)

    def stats(self) -> dict:
        return {"clients": FakeCamera.clients, "pipewire_streaming": False}

    def stop(self) -> None:
        pass


def with_stats(answer) -> wc.VirtualWebcam:
    """A VirtualWebcam whose camera reports `answer`, or raises it when it is an exception."""
    class Cam:
        def stats(self):
            if isinstance(answer, Exception):
                raise answer
            return dict(answer)
    cam = wc.VirtualWebcam()
    cam._cam = Cam()
    return cam


def run_webcam_checks() -> None:
    unreporting = with_stats({"clients": 0, "pipewire": True})
    check("a build that cannot report its PipeWire consumers asks for no camera",
          unreporting.consumers(False) is None)
    check("while the format decision still assumes one", unreporting.consumers() is not None)
    broken = with_stats(RuntimeError("no stats"))
    check("a stats call that fails asks for no camera", broken.consumers(False) is None)
    check("while the format decision still assumes a reader", broken.consumers() == "unknown")
    check("an interposer client is a reader on both",
          with_stats({"clients": 1}).consumers(False) == "interposer client")
    check("so is a PipeWire consumer",
          with_stats({"clients": 0, "pipewire_streaming": True}).consumers(False) == "PipeWire consumer")

    walks: list = []
    original = wc._device_has_openers
    wc._device_has_openers = lambda path: (walks.append(path), False)[1]
    try:
        cam = with_stats({"clients": 0, "device_path": "/dev/video0"})
        for _ in range(3):
            cam.consumers(False, cd.WEBCAM_DEVICE_RECHECK_SECONDS)
        check("three demand polls inside the floor walk /proc once", len(walks) == 1, str(len(walks)))
        cam._device_checked_at -= cd.WEBCAM_DEVICE_RECHECK_SECONDS
        cam.consumers(False, cd.WEBCAM_DEVICE_RECHECK_SECONDS)
        check("and once the floor has passed it walks again", len(walks) == 2, str(len(walks)))
        cam.consumers()
        cam.consumers()
        check("the format decision walks every time it asks", len(walks) == 4, str(len(walks)))
        wc._device_has_openers = lambda path: True
        cam._device_checked_at = 0.0
        check("the walk's answer names the device",
              cam.consumers(False, cd.WEBCAM_DEVICE_RECHECK_SECONDS) == "an application holding /dev/video0")
        walks.clear()
        with_stats({"clients": 1, "device_path": "/dev/video0"}).consumers(False)
        check("a sink that already named a reader never walks at all", walks == [])
    finally:
        wc._device_has_openers = original

    # The walk itself, against this process's own /proc: a path nobody holds, then one a
    # child process keeps open.
    with tempfile.NamedTemporaryFile() as held:
        check("a path nothing holds has no openers", wc._device_has_openers(held.name) is False)
        child = subprocess.Popen([sys.executable, "-c", "import sys,time; time.sleep(30)"],
                                 stdin=open(held.name, "rb"))
        try:
            deadline = time.time() + 5
            while time.time() < deadline and not wc._device_has_openers(held.name):
                time.sleep(0.1)
            check("a path a child process holds open is found", wc._device_has_openers(held.name) is True)
        finally:
            child.kill()
            child.wait()


async def run_webcam_demand_checks() -> None:
    originals = (wc.VirtualCamera, wc.VirtualCameraSettings, wc.webcam_available,
                 wc.app_settings.webcam_pixel_format, wc._shared_webcam)
    try:
        wc.VirtualCamera = FakeCamera
        wc.VirtualCameraSettings = lambda: type("S", (), {})()
        wc.webcam_available = lambda: True
        wc.app_settings.webcam_pixel_format = "auto"
        wc._shared_webcam = None
        FakeCamera.formats, FakeCamera.clients = [], 0
        demand = cd.WebcamDemand()
        check("the camera is brought up before any uplink asks for it",
              await demand.prepare() is True and FakeCamera.formats == ["I420"], str(FakeCamera.formats))
        check("an unread device has no reader", await demand.reader() is None)
        FakeCamera.clients = 1
        check("an interposer client is one", await demand.reader() == "interposer client")
        check("and the camera is never re-created for it", FakeCamera.formats == ["I420"])
    finally:
        (wc.VirtualCamera, wc.VirtualCameraSettings, wc.webcam_available,
         wc.app_settings.webcam_pixel_format, wc._shared_webcam) = originals
        FakeCamera.clients = 0


class FakeBackend:
    """Answers the two list calls `recorders` makes, with the indices a live server used."""

    MIC, MASTER = 164, 67

    def __init__(self, outputs: list) -> None:
        self._outputs = outputs

    async def source_list(self) -> list:
        return [PulseNode(53, "output.monitor", None, {}),
                PulseNode(self.MASTER, "input.monitor", None, {}),
                PulseNode(self.MIC, "output.SelkiesVirtualMic", None, {})]

    async def source_output_list(self) -> list:
        return self._outputs


def node(index: int, source: int, corked: bool, app: str = "") -> PulseNode:
    return PulseNode(index, "stream", None, {"application.name": app} if app else {},
                     source=source, corked=corked)


async def recorders_for(outputs: list) -> list:
    control = AudioControl("test")
    control._backend = FakeBackend(outputs)
    return await control.recorders(VIRTUAL_MIC_SOURCE_NAMES)


async def run_microphone_checks() -> None:
    bridge = node(165, FakeBackend.MASTER, True)
    check("the virtual source's own bridge stream is not a reader", await recorders_for([bridge]) == [])
    check("a corked stream on the microphone is not a reader",
          await recorders_for([bridge, node(170, FakeBackend.MIC, True, "gnome-control-center")]) == [])
    check("an uncorked stream on the microphone is",
          await recorders_for([bridge, node(171, FakeBackend.MIC, False, "parecord")]) == ["parecord"])
    check("the bridge uncorking with it does not double-count",
          await recorders_for([node(165, FakeBackend.MASTER, False),
                               node(171, FakeBackend.MIC, False, "parecord")]) == ["parecord"])
    check("a recorder on some other source is not one of ours",
          await recorders_for([node(172, 53, False, "pcmflux")]) == [])
    check("a stream with no application name still counts",
          await recorders_for([node(173, FakeBackend.MIC, False)]) == ["stream"])

    opened: list = []

    class FakeControl:
        def __init__(self, name: str) -> None:
            opened.append(name)
            self.ensured = 0

        async def open(self) -> bool:
            return True

        async def ensure_virtual_microphone(self, *a, **k):
            self.ensured += 1
            return ("module.1", False)

        async def recorders(self, names):
            return ["parecord", "arecord"]

    original = cd.AudioControl
    cd.AudioControl = FakeControl
    try:
        demand = cd.MicrophoneDemand()
        check("preparing opens one sound-server connection and makes the virtual source",
              await demand.prepare() is True and len(opened) == 1 and demand._control.ensured == 1)
        await demand.prepare()
        check("preparing again reuses the connection and re-ensures the source",
              len(opened) == 1 and demand._control.ensured == 2)
        check("the first recorder is the reader", await demand.reader() == "parecord")
        check("letting go of a microphone is held off longer than a camera",
              cd.MicrophoneDemand.release_seconds > cd.WebcamDemand.release_seconds)
    finally:
        cd.AudioControl = original


def main() -> int:
    run_setting_checks()
    run_webcam_checks()
    asyncio.run(run_webcam_demand_checks())
    asyncio.run(run_microphone_checks())
    print(f"\n{failures} failure(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
