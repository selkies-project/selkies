# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""PulseAudio/PipeWire control plane shared by the WebSocket and WebRTC transports.

Both transports need the same few one-shot control operations around the
pcmflux data planes: make sure the null sink whose monitor the server captures
exists, resolve the capture source, bring up the SelkiesVirtualMic (the
``input`` null sink plus module-virtual-source) for microphone forwarding, set
the system defaults, and move a strayed pcmflux source-output back onto its
source. `AudioControl` does all of it in-process over pulsectl_asyncio; the
``pactl`` subprocess backend is engaged only when the bindings are missing or
the server connection cannot be made, and says so in one log line.

Safety contract with the ctypes bindings: libpulse keeps raw function pointers
to per-operation callback trampolines that live exactly as long as the
awaiting coroutine frame, so cancelling a pending operation (a timeout, a peer
teardown) lets the server's late reply jump into freed memory. Every operation
therefore runs in a task of its own that is never cancelled: a timeout
abandons the connection instead (libpulse drops the pending operations'
callbacks on disconnect) and the task is left to finish by itself, and every
client is closed through `aclose` rather than garbage-collected with its state
callback still registered. A client binds to the event loop it first runs on
and refuses use from any other loop or thread.
"""

import asyncio
import contextlib
import logging
import os
import re
import weakref
from typing import (Any, Awaitable, Callable, Dict, List, NamedTuple, Optional,
                    Sequence, Tuple, TypeVar)

try:
    import pulsectl_asyncio

    PULSE_AVAILABLE = True
except ImportError:
    pulsectl_asyncio = None
    PULSE_AVAILABLE = False

logger = logging.getLogger("audio_control")

T = TypeVar("T")

DEFAULT_CAPTURE_SINK = "output"
VIRTUAL_MIC_SOURCE = "SelkiesVirtualMic"
VIRTUAL_MIC_SINK = "input"
VIRTUAL_MIC_MASTER = f"{VIRTUAL_MIC_SINK}.monitor"
# PipeWire's default sink when no card exists; its monitor is the capture of
# last resort.
PIPEWIRE_NULL_MONITOR = "auto_null.monitor"
# PipeWire prepends "output." to the name of a virtual source.
VIRTUAL_MIC_SOURCE_NAMES: Tuple[str, ...] = (VIRTUAL_MIC_SOURCE, f"output.{VIRTUAL_MIC_SOURCE}")
PCMFLUX_APP_NAME = "pcmflux"

_fallback_announced = False


class AudioControlError(Exception):
    """A control operation failed or no control backend is reachable."""


class PulseNode(NamedTuple):
    """One sink, source, or source-output as either backend reports it.

    Attributes:
        index: Server object index.
        name: Object name (a source-output's media name).
        owner_module: Index of the module that created it, when known.
        proplist: Property list; ``application.name`` identifies a stream's
            client, ``device.master_device`` a virtual source's master.
        source: For a source-output, the index of the source it records from.
    """
    index: int
    name: str
    owner_module: Optional[int]
    proplist: Dict[str, str]
    source: Optional[int] = None


def capture_sink_name(audio_device_name: Optional[str]) -> str:
    """The sink whose monitor `audio_device_name` names; ``output`` when unset."""
    name = (audio_device_name or "").strip().split(".monitor")[0]
    return name or DEFAULT_CAPTURE_SINK


def _retrieve(task: "asyncio.Future[Any]") -> None:
    """Mark a finished background task's exception as observed."""
    if not task.cancelled():
        task.exception()


def _close_quietly(pulse: Any) -> None:
    with contextlib.suppress(Exception):
        pulse.close()


class _PulsectlBackend:
    """One pulsectl_asyncio connection driven under the never-cancel discipline."""

    def __init__(self, client_name: str, connect_timeout: float, op_timeout: float) -> None:
        self._name = client_name
        self._connect_timeout = connect_timeout
        self._op_timeout = op_timeout
        self._pulse: Any = None
        # Closes a connected client an owner dropped without aclose(), so
        # libpulse never keeps the state callback of a collected object.
        self._finalizer: Optional[weakref.finalize] = None
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._inflight: set = set()

    @property
    def connected(self) -> bool:
        return self._pulse is not None and bool(self._pulse.connected)

    def _bind_loop(self) -> asyncio.AbstractEventLoop:
        loop = asyncio.get_running_loop()
        if self._loop is None:
            self._loop = loop
        elif self._loop is not loop:
            raise RuntimeError("AudioControl client used from a foreign event loop")
        return loop

    async def connect(self) -> None:
        """Open the connection; raises when the server cannot be reached.

        The connect runs in its own task: a caller cancelled mid-handshake
        leaves the task to finish and the client is closed the moment it does,
        so libpulse never holds the state callback of a collected object.
        """
        loop = self._bind_loop()
        self._discard()
        pulse = pulsectl_asyncio.PulseAsync(self._name)
        task = loop.create_task(pulse.connect(timeout=self._connect_timeout))
        try:
            await asyncio.shield(task)
        except asyncio.CancelledError:
            task.add_done_callback(lambda t: (_retrieve(t), _close_quietly(pulse)))
            raise
        except BaseException:
            _close_quietly(pulse)
            raise
        self._pulse = pulse
        self._finalizer = weakref.finalize(self, _close_quietly, pulse)
        self._finalizer.atexit = False

    async def call(self, op: Callable[[Any], Awaitable[T]]) -> T:
        """Run `op(pulse)` in an uncancellable task, bounded by the op timeout.

        A timeout abandons the connection: libpulse cancels the pending
        operation's callbacks on disconnect and the task then ends on its own.
        A cancelled caller leaves the task running; the operation completes
        normally and the client stays usable.

        Raises:
            AudioControlError: Not connected, or the operation failed.
            asyncio.TimeoutError: The server did not answer in time.
        """
        loop = self._bind_loop()
        pulse = self._pulse
        if pulse is None or not pulse.connected:
            raise AudioControlError("not connected to the sound server")

        async def body() -> T:
            if self._pulse is not pulse or not pulse.connected:
                raise AudioControlError("sound server connection closed")
            try:
                return await op(pulse)
            except AudioControlError:
                raise
            except Exception as e:
                raise AudioControlError(str(e) or type(e).__name__) from e

        task = loop.create_task(body())
        self._inflight.add(task)
        task.add_done_callback(self._inflight.discard)
        try:
            return await asyncio.wait_for(asyncio.shield(task), self._op_timeout)
        except asyncio.TimeoutError:
            task.add_done_callback(_retrieve)
            self._discard()
            raise
        except asyncio.CancelledError:
            task.add_done_callback(_retrieve)
            raise

    def _take(self) -> Any:
        """Detach the current client (and its finalizer) for closing."""
        pulse, self._pulse = self._pulse, None
        if self._finalizer is not None:
            self._finalizer.detach()
            self._finalizer = None
        return pulse

    def _discard(self) -> None:
        pulse = self._take()
        if pulse is not None:
            _close_quietly(pulse)

    async def aclose(self) -> None:
        """Let in-flight operations finish, then close the connection."""
        pulse = self._take()
        if pulse is None:
            return
        try:
            if self._inflight:
                await asyncio.wait(set(self._inflight), timeout=self._op_timeout)
        finally:
            _close_quietly(pulse)
            if not self._inflight:
                self._loop = None

    @staticmethod
    def _node(obj: Any) -> PulseNode:
        return PulseNode(
            index=int(obj.index),
            name=str(obj.name),
            owner_module=getattr(obj, "owner_module", None),
            proplist=dict(getattr(obj, "proplist", None) or {}),
            source=getattr(obj, "source", None),
        )

    async def sink_list(self) -> List[PulseNode]:
        return [self._node(s) for s in await self.call(lambda p: p.sink_list())]

    async def source_list(self) -> List[PulseNode]:
        return [self._node(s) for s in await self.call(lambda p: p.source_list())]

    async def source_output_list(self) -> List[PulseNode]:
        return [self._node(s) for s in await self.call(lambda p: p.source_output_list())]

    async def server_defaults(self) -> Tuple[Optional[str], Optional[str]]:
        info = await self.call(lambda p: p.server_info())
        return (getattr(info, "default_sink_name", None) or None,
                getattr(info, "default_source_name", None) or None)

    async def module_load(self, name: str, args: str) -> int:
        return int(await self.call(lambda p: p.module_load(name, args)))

    async def module_unload(self, index: int) -> None:
        await self.call(lambda p: p.module_unload(index))

    async def sink_default_set(self, name: str) -> None:
        await self.call(lambda p: p.sink_default_set(name))

    async def source_default_set(self, name: str) -> None:
        await self.call(lambda p: p.source_default_set(name))

    async def source_output_move(self, output_index: int, source_index: int) -> None:
        await self.call(lambda p: p.source_output_move(output_index, source_index))


_PACTL_BLOCK_RE = re.compile(r"^(?:Sink|Source|Source Output|Sink Input|Module|Client) #(\d+)\s*$", re.M)


def _parse_pactl_list(text: str) -> List[PulseNode]:
    """Parse ``pactl list <kind>`` output (C locale) into PulseNode records."""
    nodes: List[PulseNode] = []
    matches = list(_PACTL_BLOCK_RE.finditer(text))
    for i, m in enumerate(matches):
        end = matches[i + 1].start() if i + 1 < len(matches) else len(text)
        block = text[m.end():end]
        fields: Dict[str, str] = {}
        props: Dict[str, str] = {}
        in_props = False
        for line in block.splitlines():
            if line.startswith("\t\t") and in_props:
                kv = line.strip().split(" = ", 1)
                if len(kv) == 2:
                    props[kv[0]] = kv[1].strip().strip('"')
                continue
            if line.startswith("\t") and not line.startswith("\t\t"):
                key, sep, value = line.strip().partition(":")
                in_props = key == "Properties"
                if sep and not in_props:
                    fields[key] = value.strip()
        owner = fields.get("Owner Module")
        source = fields.get("Source")
        nodes.append(PulseNode(
            index=int(m.group(1)),
            name=fields.get("Name", ""),
            owner_module=int(owner) if owner and owner.isdigit() else None,
            proplist=props,
            source=int(source) if source and source.isdigit() else None,
        ))
    return nodes


class _PactlBackend:
    """``pactl`` subprocesses with the same operation set as the bindings."""

    def __init__(self, timeout: float) -> None:
        self._timeout = timeout

    async def run(self, *args: str) -> str:
        """Run one pactl command and return its stdout.

        Raises:
            AudioControlError: pactl is missing, failed, or timed out.
        """
        logger.debug("pactl fallback: pactl %s", " ".join(args))
        return await self._exec(*args)

    async def _exec(self, *args: str) -> str:
        env = dict(os.environ, LC_ALL="C", LANGUAGE="C")
        try:
            proc = await asyncio.create_subprocess_exec(
                "pactl", *args, env=env,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        except Exception as e:
            raise AudioControlError(f"pactl unavailable: {e}") from e
        try:
            out, err = await asyncio.wait_for(proc.communicate(), timeout=self._timeout)
        except asyncio.TimeoutError:
            # Reap it rather than leaking one blocked pactl per call on a
            # stuck server.
            with contextlib.suppress(ProcessLookupError):
                proc.kill()
            await proc.wait()
            raise AudioControlError(f"pactl {' '.join(args)} timed out") from None
        if proc.returncode != 0:
            raise AudioControlError(
                f"pactl {' '.join(args)} failed: {err.decode(errors='replace').strip()}")
        return out.decode(errors="replace")

    async def sink_list(self) -> List[PulseNode]:
        return _parse_pactl_list(await self.run("list", "sinks"))

    async def source_list(self) -> List[PulseNode]:
        return _parse_pactl_list(await self.run("list", "sources"))

    async def source_output_list(self) -> List[PulseNode]:
        return _parse_pactl_list(await self.run("list", "source-outputs"))

    async def server_defaults(self) -> Tuple[Optional[str], Optional[str]]:
        sink = source = None
        for line in (await self.run("info")).splitlines():
            key, _, value = line.partition(":")
            if key.strip() == "Default Sink":
                sink = value.strip() or None
            elif key.strip() == "Default Source":
                source = value.strip() or None
        return sink, source

    async def module_load(self, name: str, args: str) -> int:
        out = (await self.run("load-module", name, *args.split())).strip()
        if not out.isdigit():
            raise AudioControlError(f"load-module {name} returned no index: {out!r}")
        return int(out)

    async def module_unload(self, index: int) -> None:
        await self.run("unload-module", str(index))

    async def sink_default_set(self, name: str) -> None:
        await self.run("set-default-sink", name)

    async def source_default_set(self, name: str) -> None:
        await self.run("set-default-source", name)

    async def source_output_move(self, output_index: int, source_index: int) -> None:
        await self.run("move-source-output", str(output_index), str(source_index))


class AudioControl:
    """Sound-server control operations over one connection, with a pactl fallback.

    Open lazily on first use (or explicitly with `open`), bound to the event
    loop of that first call, and released with `aclose`; usable as an async
    context manager for one-shot provisioning. Every public operation reports
    failure through its return value and a log line rather than raising, so a
    missing or wedged sound server degrades audio instead of aborting the
    caller. A lost connection is reopened on the next operation.

    Args:
        client_name: Name shown in the server's client list.
        connect_timeout: Seconds to wait for the server handshake.
        op_timeout: Seconds an operation may take before the connection is
            abandoned (or a pactl process is killed).
    """

    def __init__(self, client_name: str, connect_timeout: float = 2.0,
                 op_timeout: float = 5.0) -> None:
        self.client_name = client_name
        self._pulse = (_PulsectlBackend(client_name, connect_timeout, op_timeout)
                       if PULSE_AVAILABLE else None)
        self._pactl = _PactlBackend(op_timeout)
        self._backend: Any = None
        self._fallback_logged = False

    @property
    def backend(self) -> Optional[str]:
        """``"pulsectl"`` or ``"pactl"`` once a backend is settled, else None."""
        if self._backend is None:
            return None
        return "pulsectl" if self._backend is self._pulse else "pactl"

    async def __aenter__(self) -> "AudioControl":
        await self.open()
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.aclose()

    def _announce_fallback(self, reason: str) -> None:
        global _fallback_announced
        if self._fallback_logged:
            return
        self._fallback_logged = True
        level = logging.DEBUG if _fallback_announced else logging.WARNING
        _fallback_announced = True
        logger.log(level, f"Sound server control: {reason}; using pactl subprocesses.")

    async def open(self) -> bool:
        """Settle the backend: connect the bindings, else engage the pactl fallback.

        Returns:
            True when a backend is ready; False when neither the bindings nor
            pactl can reach a server. The fallback, once engaged, stays until
            `aclose`.
        """
        if self._backend is self._pactl:
            return True
        if self._backend is self._pulse and self._pulse is not None and self._pulse.connected:
            return True
        if self._pulse is not None:
            try:
                await self._pulse.connect()
                self._backend = self._pulse
                return True
            except Exception as e:
                self._announce_fallback(f"connection failed ({e})")
        else:
            self._announce_fallback("pulsectl_asyncio is unavailable")
        self._backend = self._pactl
        try:
            await self._pactl.server_defaults()
        except AudioControlError as e:
            logger.warning(f"Sound server control unavailable: {e}")
            return False
        return True

    async def aclose(self) -> None:
        """Release the connection; the next operation reopens it."""
        self._backend = None
        if self._pulse is not None:
            await self._pulse.aclose()

    async def _op(self, what: str, fn: Callable[[Any], Awaitable[T]], default: T) -> T:
        """Run `fn(backend)`, reconnecting once after a lost connection.

        Returns `default` (after one warning) when the operation fails, times
        out, or no backend is usable, so callers degrade rather than raise.
        """
        for attempt in (0, 1):
            if self._backend is None or (
                    self._backend is self._pulse and not self._pulse.connected):
                if not await self.open():
                    return default
            try:
                return await fn(self._backend)
            except asyncio.TimeoutError:
                logger.warning(f"Sound server did not answer ({what}); connection abandoned.")
                return default
            except AudioControlError as e:
                lost = (self._backend is self._pulse and self._pulse is not None
                        and not self._pulse.connected)
                if lost and attempt == 0:
                    logger.info(f"Sound server connection lost during {what}; reconnecting.")
                    continue
                logger.warning(f"Sound server control failed ({what}): {e}")
                return default
        return default

    async def sinks(self) -> List[PulseNode]:
        return await self._op("sink list", lambda b: b.sink_list(), [])

    async def sources(self) -> List[PulseNode]:
        return await self._op("source list", lambda b: b.source_list(), [])

    async def default_devices(self) -> Tuple[Optional[str], Optional[str]]:
        """``(default sink name, default source name)``; None when unknown."""
        return await self._op("server info", lambda b: b.server_defaults(), (None, None))

    async def load_module(self, name: str, args: str) -> Optional[int]:
        """Load a server module; its index, or None when the load failed."""
        return await self._op(f"load {name}", lambda b: b.module_load(name, args), None)

    async def unload_module(self, index: int) -> bool:
        async def run(b: Any) -> bool:
            await b.module_unload(index)
            return True
        return await self._op(f"unload module {index}", run, False)

    async def set_default_sink(self, name: str) -> bool:
        async def run(b: Any) -> bool:
            await b.sink_default_set(name)
            return True
        return await self._op(f"default sink {name}", run, False)

    async def set_default_source(self, name: str) -> bool:
        async def run(b: Any) -> bool:
            await b.source_default_set(name)
            return True
        return await self._op(f"default source {name}", run, False)

    async def _wait_for(self, list_fn: Callable[[], Awaitable[List[PulseNode]]],
                        names: Sequence[str], timeout: float = 2.0) -> Optional[PulseNode]:
        """Poll a listing until an object named in `names` appears.

        PipeWire creates sinks and sources asynchronously, so a freshly loaded
        module's object may not be listed immediately.
        """
        deadline = asyncio.get_running_loop().time() + timeout
        while True:
            for node in await list_fn():
                if node.name in names:
                    return node
            if asyncio.get_running_loop().time() >= deadline:
                return None
            await asyncio.sleep(0.1)

    async def ensure_null_sink(self, name: str) -> bool:
        """Make sure a null sink called `name` exists, loading it if needed."""
        if any(s.name == name for s in await self.sinks()):
            return True
        logger.info(f"Sink '{name}' not found. Creating it...")
        if await self.load_module("module-null-sink", f"sink_name={name}") is None:
            return False
        if await self._wait_for(self.sinks, [name]) is not None:
            logger.info(f"Created sink '{name}'.")
            return True
        logger.error(f"Loaded module-null-sink for '{name}' but it never appeared.")
        return False

    async def ensure_capture_sink(self, audio_device_name: Optional[str]) -> bool:
        """Make sure the sink whose monitor the server captures exists.

        A container's PipeWire or PulseAudio comes up with no sink at all when
        the host exposes no sound card, so the monitor source named by the
        configured audio device is missing and pcmflux gives up after its retry
        budget. The microphone control plane creates the same sink, but only
        once a client sends mic data, which server-to-client audio must not
        wait for.

        Args:
            audio_device_name: The configured capture device (a ``.monitor``
                suffix is stripped to derive the sink name); ``output`` when
                unset.

        Returns:
            True when the sink is present afterwards. Best effort: False only
            means the capture will fail for the usual reasons, so callers
            proceed and let pcmflux report.
        """
        return await self.ensure_null_sink(capture_sink_name(audio_device_name))

    async def resolve_capture_source(self, audio_device_name: Optional[str]) -> Optional[str]:
        """The source to capture: the configured one if it exists, else a monitor.

        Falls back to the default sink's monitor, then to PipeWire's
        ``auto_null.monitor``.

        Returns:
            The source name to capture from, or None when no usable source
            exists (or the server could not be queried).
        """
        default_sink, _ = await self.default_devices()
        default_monitor = f"{default_sink}.monitor" if default_sink else None
        if default_sink:
            logger.info(f"Default sink: '{default_sink}'")
        else:
            logger.warning("Could not determine the default sink.")
        available = {s.name for s in await self.sources()}
        if not available:
            logger.error("Failed to enumerate audio sources.")
            return None
        if audio_device_name and audio_device_name in available:
            logger.info(f"Configured audio device '{audio_device_name}' is valid.")
            return audio_device_name
        if audio_device_name:
            logger.warning(
                f"Configured audio device '{audio_device_name}' not found in available sources.")
        if default_monitor and default_monitor in available:
            logger.info(f"Falling back to the default sink's monitor: '{default_monitor}'")
            return default_monitor
        if PIPEWIRE_NULL_MONITOR in available:
            logger.info(
                f"Default sink monitor not available; falling back to '{PIPEWIRE_NULL_MONITOR}'")
            return PIPEWIRE_NULL_MONITOR
        logger.error(
            "No valid audio source found. Audio capture will likely fail. "
            f"Available sources: {sorted(available)}")
        return None

    async def route_pcmflux(self, targets: Sequence[str]) -> Optional[str]:
        """Put the pcmflux record stream on one of `targets` if it strayed.

        PipeWire often ignores a recording app's requested device and attaches
        it to the default source, particularly across streaming-mode switches.

        Returns:
            The name of the source pcmflux records from afterwards, or None
            when no pcmflux stream (or no target source) exists.
        """
        wanted = [t for t in targets if t]

        async def run(b: Any) -> Optional[str]:
            sources = await b.source_list()
            outputs = await b.source_output_list()
            stream = next((o for o in outputs
                           if o.proplist.get("application.name") == PCMFLUX_APP_NAME), None)
            if stream is None:
                logger.debug("pcmflux has no record stream to route.")
                return None
            by_index = {s.index: s for s in sources}
            current = by_index.get(stream.source)
            if current is not None and current.name in wanted:
                logger.info(f"pcmflux correctly connected to '{current.name}'")
                return current.name
            target = next((s for s in sources if s.name in wanted), None)
            if target is None:
                logger.warning(f"Routing enforcement: no target source among {wanted} exists.")
                return current.name if current else None
            logger.warning(
                f"pcmflux connected to '{current.name if current else stream.source}', "
                f"moving it to '{target.name}'")
            await b.source_output_move(stream.index, target.index)
            return target.name

        return await self._op("pcmflux routing", run, None)

    async def ensure_virtual_microphone(
        self, audio_device_name: Optional[str], is_pcmflux_capturing: bool,
    ) -> Tuple[Optional[int], bool]:
        """Provision the SelkiesVirtualMic control plane shared by both transports.

        Creates the ``input`` and capture null sinks, loads module-virtual-source
        bridging ``input.monitor`` to a recordable source, and makes them the
        system default sink/source so an app recording the default source hears
        the client's forwarded mic. The PCM data plane (pcmflux AudioPlayback
        into the ``input`` sink) belongs to the caller.

        Idempotent: an existing SelkiesVirtualMic is reused, so the websockets
        mic path and the WebRTC mic playback never double-load the module when
        both are live.

        Args:
            audio_device_name: The capture device name whose sink half becomes
                the default output sink.
            is_pcmflux_capturing: When True, verify pcmflux's record stream is
                attached to a valid capture target and move it if not.

        Returns:
            ``(module_index, owns_module)``. `owns_module` is True only when
            THIS call loaded the module, so a caller that merely reused an
            existing source never unloads it out from under the other
            transport on teardown. ``(None, False)`` when the module load could
            not be verified.
        """
        output_sink = capture_sink_name(audio_device_name)
        for sink_name in (VIRTUAL_MIC_SINK, output_sink):
            await self.ensure_null_sink(sink_name)
        if await self.set_default_sink(output_sink):
            logger.info(f"Set system default sink to '{output_sink}'.")

        existing = next((s for s in await self.sources() if s.name in VIRTUAL_MIC_SOURCE_NAMES), None)
        if existing is not None:
            logger.info(f"Virtual source '{existing.name}' (index {existing.index}) already exists.")
            master = existing.proplist.get("device.master_device")
            if master is not None and master != VIRTUAL_MIC_MASTER:
                logger.warning(
                    f"Existing source '{existing.name}' is linked to '{master}', "
                    f"not '{VIRTUAL_MIC_MASTER}'.")
            module_index: Optional[int] = existing.owner_module
            owns_module = False
            await self.set_default_source(existing.name)
        else:
            logger.info(f"Virtual source '{VIRTUAL_MIC_SOURCE}' not found. Loading module...")
            module_index = await self.load_module(
                "module-virtual-source",
                f"source_name={VIRTUAL_MIC_SOURCE} master={VIRTUAL_MIC_MASTER}")
            if module_index is None:
                return None, False
            owns_module = True
            created = await self._wait_for(self.sources, VIRTUAL_MIC_SOURCE_NAMES)
            if created is None:
                logger.error(
                    f"Loaded module {module_index} but source '{VIRTUAL_MIC_SOURCE}' never appeared.")
                await self.unload_module(module_index)
                return None, False
            logger.info(f"Created source '{created.name}' (index {created.index}).")
            if await self.set_default_source(created.name):
                logger.info(f"Set system default source to '{created.name}'.")

        if is_pcmflux_capturing:
            targets = [audio_device_name or "", PIPEWIRE_NULL_MONITOR]
            await self.route_pcmflux(targets)

        logger.info(f"Virtual microphone '{VIRTUAL_MIC_SOURCE}' is ready for microphone forwarding.")
        return module_index, owns_module


async def ensure_capture_sink(audio_device_name: Optional[str],
                              client_name: str = "selkies-sink-provision") -> bool:
    """One-shot `AudioControl.ensure_capture_sink` over a short-lived connection."""
    async with AudioControl(client_name) as control:
        return await control.ensure_capture_sink(audio_device_name)
