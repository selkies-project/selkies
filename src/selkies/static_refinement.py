# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Bound optional still captures to the scene carried by the video sample.

One display owns one capture task and retains at most one bounded PNG. Clients
request the scene their canvas has presented; they never name a root window or
choose a capture object. Scene changes discard the cache and invalidate pending
results. Turning the feature off prevents new work without canceling an executor
thread that might still be compressing an already admitted native snapshot.

The transport must pace the image behind video, attach sample metadata to the
same relay item as the video, and recheck the epoch before every write. A browser
must validate that identity in the renderer that owns its visible canvas.
"""

import asyncio
import itertools
import time
from dataclasses import dataclass
from typing import Any, Callable, Dict, Optional, Tuple


PROTOCOL_VERSION = 1
MAX_PNG_BYTES = 64 * 1024 * 1024
CAPTURE_TIMEOUT_S = 1.0
QUIET_SECONDS = 0.5
MAX_ID = (1 << 64) - 1
_EPOCHS = itertools.count(1)


def _identity(value: Any) -> int:
    """Parse an unsigned native identity without accepting rounded JSON numbers."""
    if not isinstance(value, str) or not value.isascii() or not value.isdecimal():
        raise ValueError("Invalid refinement identity")
    if len(value) > 20:
        raise ValueError("Invalid refinement identity")
    number = int(value)
    if not 0 < number <= MAX_ID or str(number) != value:
        raise ValueError("Invalid refinement identity")
    return number


@dataclass(frozen=True)
class SceneSample:
    """Identity of a complete captured picture, independent of decoder frame IDs."""

    run: int
    source: int
    scene: int
    sample: int
    width: int
    height: int

    @property
    def key(self) -> Tuple[int, int, int, int, int]:
        return self.run, self.source, self.scene, self.width, self.height

    def wire(self) -> Dict[str, Any]:
        return {"run": str(self.run), "source": str(self.source),
                "scene": str(self.scene), "sample": str(self.sample),
                "width": self.width, "height": self.height}

    @classmethod
    def parse(cls, message: Dict[str, Any]) -> "SceneSample":
        """Validate a client request; identities remain decimal strings on the wire."""
        width, height = message.get("width"), message.get("height")
        if (type(width) is not int or type(height) is not int
                or not 0 < width <= 65535 or not 0 < height <= 65535):
            raise ValueError("Invalid refinement geometry")
        return cls(*(_identity(message.get(key)) for key in ("run", "source", "scene", "sample")),
                   width, height)

    @classmethod
    def from_frame(cls, frame: Any, data: memoryview) -> Optional["SceneSample"]:
        """Read provenance and geometry from a supported full-frame payload.

        Capture callbacks survive in-place resizes. Their initial dimensions
        and the display's current settings need not describe a queued sample;
        its own native wire header does.
        """
        if len(data) < 12 or data[0] != 0x04 or data[4] or data[5]:
            return None
        width = int.from_bytes(data[6:8], "big")
        height = int.from_bytes(data[8:10], "big")
        if not width or not height:
            return None
        ids = (frame.sample_run_id, frame.source_id, frame.scene_id, frame.sample_seq)
        if any(type(value) is not int or not 0 < value <= MAX_ID for value in ids):
            return None
        return cls(*ids, width, height)


@dataclass(frozen=True)
class RefinedImage:
    """One immutable PNG with the native identity of its own captured pixels."""

    epoch: int
    stamp: SceneSample
    png: bytes


class RefinementUnavailable(RuntimeError):
    """The requested scene or optional capture route is no longer available."""


class StaticRefinement:
    """Serialize one display's optional scene-bound captures and bounded cache."""

    def __init__(self) -> None:
        self.epoch = 0
        self.requested = False
        self.parent = False
        self.supported = False
        self.effective = False
        self.reason = "disabled"
        self.module: Any = None
        self.latest: Optional[SceneSample] = None
        self.image: Optional[RefinedImage] = None
        self._attempted: Optional[Tuple[int, int, int, int, int]] = None
        self._capture: Optional[asyncio.Task] = None
        self._configuration: Any = None
        self._high_water: Optional[SceneSample] = None
        self._scene_since = 0.0
        self._waiters: Dict[object, Tuple[int, SceneSample, Callable[[], bool]]] = {}

    def configure(self, module: Any, requested: bool, parent: bool,
                  consumers: bool, full_frame: bool) -> bool:
        """Apply settings without changing the capture backend, codec, or video sink."""
        native_supported = bool(module is not None and module.scene_tracking_supported)
        supported = bool(native_supported and full_frame)
        effective = bool(requested and parent and consumers and supported)
        reason = ("capture-unavailable" if not native_supported
                  else "full-frame-required" if not full_frame
                  else "canvas-required" if not consumers
                  else "paint-over-disabled" if not parent
                  else "disabled" if not requested else "ready")
        configuration = (module, bool(requested), bool(parent), supported, effective, reason)
        native_enabled = bool(module is not None and module.scene_tracking_enabled)
        if configuration == self._configuration and (not effective or native_enabled):
            return False
        old_module, old_effective = self.module, self.effective
        self.invalidate()
        self.module = module
        self.requested, self.parent = bool(requested), bool(parent)
        self.supported, self.effective, self.reason = supported, effective, reason
        self._configuration = configuration
        if old_effective and old_module is not None and (old_module is not module or not effective):
            try:
                old_module.set_scene_tracking(False)
            except RuntimeError:
                pass
        if effective and (not old_effective or old_module is not module or not native_enabled):
            try:
                module.set_scene_tracking(True)
            except RuntimeError:
                self.effective = False
                self.reason = "capture-unavailable"
                self._configuration = None
        return True

    def invalidate(self) -> None:
        """Withdraw all cached and in-flight results before a setting transition."""
        self.epoch = next(_EPOCHS)
        self.latest = None
        self.image = None
        self._attempted = None
        self._high_water = None

    def close(self) -> None:
        """Stop admitting captures and release the optional native tracking state."""
        self.configure(None, False, False, False, False)

    def observe(self, stamp: Optional[SceneSample]) -> bool:
        """Follow native scene changes without using arrival time as scene identity."""
        if not self.effective:
            return False
        if stamp is None:
            self.latest = None
            self.image = None
            self._attempted = None
            return False
        previous = self._high_water
        if previous is not None and (stamp.run < previous.run or (
                stamp.run == previous.run and stamp.sample <= previous.sample)):
            return False
        changed_source = previous is not None and (
            stamp.run, stamp.source, stamp.width, stamp.height
        ) != (previous.run, previous.source, previous.width, previous.height)
        if changed_source:
            self.invalidate()
        if self.latest is None or stamp.key != self.latest.key:
            self.image = None
            self._attempted = None
            self._scene_since = time.monotonic()
        self.latest = stamp
        self._high_water = stamp
        return changed_source

    def valid(self, epoch: int, stamp: SceneSample) -> bool:
        return bool(self.effective and epoch == self.epoch and self.latest is not None
                    and stamp.key == self.latest.key and stamp.sample <= self.latest.sample)

    def status(self) -> Dict[str, Any]:
        return {"type": "lossless_status", "version": PROTOCOL_VERSION,
                "epoch": self.epoch, "requested": self.requested,
                "supported": self.supported, "effective": self.effective, "reason": self.reason}

    async def capture(self, epoch: int, stamp: SceneSample,
                      admitted: Optional[Callable[[], bool]] = None) -> RefinedImage:
        """Share one admitted capture across consumers of the same current scene."""
        waiter = object()
        self._waiters[waiter] = (epoch, stamp, admitted or (lambda: True))
        try:
            return await self._capture_shared(epoch, stamp)
        finally:
            self._waiters.pop(waiter, None)

    async def _capture_shared(self, epoch: int, stamp: SceneSample) -> RefinedImage:
        """Wait past obsolete work without duplicating the next scene's capture."""
        while True:
            if not self.valid(epoch, stamp):
                raise RefinementUnavailable("Scene changed")
            if self.image is not None:
                return self.image
            task = self._capture
            if task is not None and not task.done():
                try:
                    await asyncio.shield(task)
                except RefinementUnavailable:
                    pass
                continue
            if self._attempted == stamp.key:
                raise RefinementUnavailable("Scene capture already attempted")
            self._attempted = stamp.key
            task = asyncio.create_task(self._capture_image(epoch, stamp, self.module))
            self._capture = task
            task.add_done_callback(self._capture_finished)
            return await asyncio.shield(task)

    def _capture_finished(self, task: asyncio.Task) -> None:
        """Consume errors even when all waiting clients have already disconnected."""
        if self._capture is task:
            self._capture = None
        if not task.cancelled():
            task.exception()

    async def _capture_image(self, epoch: int, wanted: SceneSample, module: Any) -> RefinedImage:
        """Compress off the event loop and validate the returned scene before caching."""
        remaining = QUIET_SECONDS - (time.monotonic() - self._scene_since)
        if remaining > 0:
            await asyncio.sleep(remaining)
        if not self.valid(epoch, wanted) or not any(
                other_epoch == epoch and other.key == wanted.key and admitted()
                for other_epoch, other, admitted in tuple(self._waiters.values())):
            if self._attempted == wanted.key:
                self._attempted = None
            raise RefinementUnavailable("Scene changed before capture")
        try:
            snapshot = await asyncio.to_thread(module.snapshot_png, wanted.run, CAPTURE_TIMEOUT_S)
            returned = SceneSample.parse({
                "run": str(snapshot["run_id"]), "sample": str(snapshot["sample_seq"]),
                "source": str(snapshot["source_id"]), "scene": str(snapshot["scene_id"]),
                "width": snapshot["width"], "height": snapshot["height"],
            })
            png = snapshot["png"]
            bits = snapshot["preserved_rgb_bits"]
        except (RuntimeError, ValueError, KeyError, TypeError) as exc:
            raise RefinementUnavailable(str(exc)) from exc
        if (not self.valid(epoch, wanted) or returned.key != wanted.key
                or returned.sample < wanted.sample or bits != 8
                or not isinstance(png, bytes) or len(png) > MAX_PNG_BYTES
                or not png.startswith(b"\x89PNG\r\n\x1a\n")):
            raise RefinementUnavailable("Snapshot does not match the current scene")
        image = RefinedImage(epoch, returned, png)
        self.image = image
        return image
