# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Asks a page for its camera or microphone only while the session reads the virtual device.

A device requested at connect is held until the session ends, so the indicator beside the
user's camera reports the session's length rather than the desktop's use. With
``webcam_on_start`` or ``microphone_on_start`` set to ``demand``, a `CaptureDemand` per device polls its
readers instead: the interposer's clients, PipeWire's consumers and a kernel device's openers
for the camera (`webcam.VirtualWebcam.consumers`), the sound server's recording streams for the
microphone (`audio_control.AudioControl.recorders`). A reader is passed on at the poll that
finds it; its absence only once it has lasted the device's hold-off, so an application's
probe-then-open never blinks the light.

The answer goes to one page, since every page that captures pushes into the same device: the
first candidate a transport offers, told to release before another is asked or once it may no
longer capture. A transport takes part with ``capture_candidates()``, the pages that may
capture with the preferred one first, and ``async tell_capture(page, subject, wanted) -> bool``;
it calls `sync` wherever that list changes and `detach` at shutdown. The wire verb is
``CAPTURE_DEMAND <subject> <0|1>`` over WebSockets and the ``capture_demand,<subject>,<0|1>``
system action over WebRTC, obeyed by the page unless the user toggled the device themselves.
Neither policy defaults to it, because a process on the desktop can then cause a permission
prompt; every transition is logged and audited as ``capture.demand`` with the reader's name.
"""
import asyncio
import logging
import time
from typing import Any, Dict, List, Optional, Tuple

from . import audit
from .audio_control import VIRTUAL_MIC_SOURCE_NAMES, AudioControl
from .settings import settings as app_settings
from .webcam import get_shared_webcam

logger = logging.getLogger("capture_demand")

MSG_CAPTURE_DEMAND = "CAPTURE_DEMAND"
POLL_SECONDS = 1.0
# Idle polls a release needs besides the hold-off, so one long poll cycle cannot release on
# two readings taken far apart.
MIN_IDLE_POLLS = 3
# Floor between /proc walks for a kernel device's openers; the interposer and PipeWire answer
# on every poll.
WEBCAM_DEVICE_RECHECK_SECONDS = 4.0


def on_demand(subject: str) -> bool:
    """Whether `subject` (``webcam`` or ``microphone``) is asked for on demand; a lock-off wins."""
    enabled, locked = getattr(app_settings, f"{subject}_enabled")
    return getattr(app_settings, f"{subject}_on_start") == "demand" and (enabled or not locked)


class CaptureDemand:
    """Polls one device's readers and keeps the page that may capture told of the answer.

    ``release_seconds`` is per device: a camera re-taken a moment later shows one late frame,
    a microphone re-taken has lost the start of a sentence and, on an engine that forgot the
    grant, raised a second prompt.
    """

    subject = ""
    release_seconds = 2.0

    def __init__(self) -> None:
        self._ports: List[Any] = []
        self._told: Optional[Tuple[Any, Any]] = None
        self._wanted = False
        self._reader: Optional[str] = None
        self._idle_since: Optional[float] = None
        self._idle_polls = 0
        self._task: Optional[Any] = None
        self._lock: Optional[asyncio.Lock] = None

    async def prepare(self) -> bool:
        """Brings the device up, since nothing can read a device that is not there."""
        raise NotImplementedError

    async def reader(self) -> Optional[str]:
        """What reads the device, or None when nothing does or nothing can say."""
        raise NotImplementedError

    @property
    def wanted(self) -> bool:
        return self._wanted

    def attach(self, port: Any) -> None:
        """Adds a transport; the first one starts the watch."""
        if port not in self._ports:
            self._ports.append(port)
        if self._task is None or self._task.done():
            self._task = asyncio.ensure_future(self._run())

    def detach(self, port: Any) -> None:
        """Drops a transport; the watch ends with the last one and the device outlives it."""
        if port in self._ports:
            self._ports.remove(port)
        if self._told is not None and self._told[0] is port:
            self._told = None

    async def route(self) -> None:
        """Re-addresses the answer after a transport's candidates changed.

        The page asked last is released when it may no longer capture, and the page that may
        is asked if it was not; a send that fails hands the device to the next candidate.
        """
        if self._lock is None:
            self._lock = asyncio.Lock()
        async with self._lock:
            candidates = [(p, page) for p in self._ports for page in p.capture_candidates()]
            target = candidates[0] if candidates else None
            if self._told is not None and (not self._wanted or self._told != target):
                port, page = self._told
                self._told = None
                await port.tell_capture(page, self.subject, False)
            if not self._wanted or self._told is not None:
                return
            for port, page in candidates:
                if await port.tell_capture(page, self.subject, True):
                    self._told = (port, page)
                    return

    async def _run(self) -> None:
        if not await self.prepare():
            logger.error("The %s cannot be watched for readers: its device did not start.",
                         self.subject)
            return
        while self._ports:
            await asyncio.sleep(POLL_SECONDS)
            try:
                await self._poll()
            except Exception:
                logger.exception("The %s demand poll failed.", self.subject)
        self._wanted, self._idle_since, self._idle_polls = False, None, 0

    async def _poll(self) -> None:
        if not any(port.capture_candidates() for port in self._ports):
            # Nobody to ask, so no reading is taken: it would be stale by the time a page arrives.
            self._idle_since, self._idle_polls = None, 0
            await self._publish(False)
            return
        name = await self.reader()
        if name is not None:
            self._idle_since, self._idle_polls = None, 0
            self._reader = name
            await self._publish(True)
            return
        self._idle_polls += 1
        if self._idle_since is None:
            self._idle_since = time.monotonic()
        elif (self._idle_polls >= MIN_IDLE_POLLS
              and time.monotonic() - self._idle_since >= self.release_seconds):
            await self._publish(False)

    async def _publish(self, wanted: bool) -> None:
        if wanted != self._wanted:
            self._wanted = wanted
            logger.info("The %s is %s.", self.subject,
                        f"read by {self._reader}; the page is asked for it" if wanted else "released")
            audit.emit("capture.demand", subject=self.subject,
                       action="ask" if wanted else "release",
                       reader=self._reader if wanted else None)
        await self.route()


class WebcamDemand(CaptureDemand):
    """The virtual camera's readers, as its sinks report an open device.

    The camera is started before any uplink, since an application can only open a device that
    exists, so an ``auto`` format resolves to I420 (`webcam.device_pixel_format`).
    """

    subject = "webcam"

    async def prepare(self) -> bool:
        return await get_shared_webcam().ensure() is not None

    async def reader(self) -> Optional[str]:
        return await asyncio.to_thread(
            get_shared_webcam().consumers, False, WEBCAM_DEVICE_RECHECK_SECONDS)


class MicrophoneDemand(CaptureDemand):
    """The virtual microphone's recorders, as the sound server lists them.

    A conferencing application holds an uncorked stream for as long as it runs and mutes in
    software, so this decides when the browser is asked, not whether the indicator is honest.
    """

    subject = "microphone"
    release_seconds = 10.0

    def __init__(self) -> None:
        super().__init__()
        self._control: Optional[AudioControl] = None

    async def prepare(self) -> bool:
        if self._control is None:
            control = AudioControl("selkies-mic-demand")
            if not await control.open():
                return False
            self._control = control
        module, _owned = await self._control.ensure_virtual_microphone(
            app_settings.audio_device_name, is_pcmflux_capturing=False)
        return module is not None

    async def reader(self) -> Optional[str]:
        if self._control is None:
            return None
        names = await self._control.recorders(VIRTUAL_MIC_SOURCE_NAMES)
        return names[0] if names else None


_demands: Dict[str, CaptureDemand] = {}


def demands() -> List[CaptureDemand]:
    """The process-wide watchers the settings turn on, created on first use."""
    out = []
    for cls in (WebcamDemand, MicrophoneDemand):
        if on_demand(cls.subject):
            if cls.subject not in _demands:
                _demands[cls.subject] = cls()
            out.append(_demands[cls.subject])
    return out


async def sync(port: Any) -> None:
    """Attaches `port` to every watch its settings turn on and re-routes the answer."""
    for demand in demands():
        demand.attach(port)
        await demand.route()


def detach(port: Any) -> None:
    """Takes `port` out of every watch, at the transport's shutdown."""
    for demand in _demands.values():
        demand.detach(port)
