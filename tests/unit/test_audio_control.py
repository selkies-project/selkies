#!/usr/bin/env python3
"""The shared sound-server control plane (selkies.audio_control) against a stub.

A scripted stand-in for pulsectl_asyncio's PulseAsync plays the server: the
provisioning operations both transports share (capture sink, capture-source
resolution, pcmflux routing, SelkiesVirtualMic) are checked for what they ask
the server to do, and the never-cancel discipline is proven by the stub
recording whether a pending operation was ever cancelled. The pactl fallback
runs against scripted command output. No sound server, no pulsectl.
"""
import asyncio
import logging
import os
import sys
from types import SimpleNamespace
from typing import Any, Dict, List, Optional

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies import audio_control as AC


class PulseDisconnected(Exception):
    pass


class StubServer:
    """Sink/source state shared by every stub client of one scenario."""

    def __init__(self) -> None:
        self.sinks: List[Dict[str, Any]] = []
        self.sources: List[Dict[str, Any]] = []
        self.source_outputs: List[Dict[str, Any]] = []
        self.default_sink: Optional[str] = None
        self.default_source: Optional[str] = None
        self.modules: Dict[int, str] = {}
        self.next_index = 100
        self.refuse_connect = False
        self.hang = False
        self.pending: set = set()
        self.cancelled_ops = 0
        self.log: List[str] = []
        self.virtual_source_appears = True

    def add_sink(self, name: str, module: Optional[int] = None) -> Dict[str, Any]:
        idx = self.next_index
        self.next_index += 1
        sink = {"index": idx, "name": name, "owner_module": module, "proplist": {}}
        self.sinks.append(sink)
        self.add_source(f"{name}.monitor", module)
        return sink

    def add_source(self, name: str, module: Optional[int] = None,
                   proplist: Optional[Dict[str, str]] = None) -> Dict[str, Any]:
        idx = self.next_index
        self.next_index += 1
        src = {"index": idx, "name": name, "owner_module": module, "proplist": proplist or {}}
        self.sources.append(src)
        return src

    def add_source_output(self, app: str, source_index: int) -> Dict[str, Any]:
        idx = self.next_index
        self.next_index += 1
        so = {"index": idx, "name": f"{app} stream", "owner_module": None,
              "proplist": {"application.name": app}, "source": source_index}
        self.source_outputs.append(so)
        return so

    def release(self) -> None:
        """Let every hung operation answer."""
        self.hang = False
        for fut in list(self.pending):
            if not fut.done():
                fut.set_result(None)


class StubPulse:
    """Stands in for pulsectl_asyncio.PulseAsync: async ops over StubServer state."""

    instances: List["StubPulse"] = []
    server: StubServer = StubServer()

    def __init__(self, client_name: str) -> None:
        self.name = client_name
        self.connected = False
        self.closed = False
        self.calls: List[str] = []
        self.pending: set = set()
        StubPulse.instances.append(self)

    async def connect(self, timeout: Optional[float] = None) -> None:
        if self.server.refuse_connect:
            raise RuntimeError("Failed to connect to pulseaudio server")
        self.connected = True

    def disconnect(self) -> None:
        self.connected = False
        for fut in list(self.pending):
            if not fut.done():
                fut.set_exception(PulseDisconnected())

    def close(self) -> None:
        self.disconnect()
        self.closed = True

    async def _op(self, what: str) -> None:
        self.calls.append(what)
        self.server.log.append(f"{self.name}:{what}")
        if self.server.hang:
            fut = asyncio.get_running_loop().create_future()
            self.pending.add(fut)
            self.server.pending.add(fut)
            try:
                await fut
            except asyncio.CancelledError:
                self.server.cancelled_ops += 1
                raise
            finally:
                self.pending.discard(fut)
                self.server.pending.discard(fut)

    @staticmethod
    def _obj(d: Dict[str, Any]) -> Any:
        return SimpleNamespace(**d)

    async def sink_list(self) -> list:
        await self._op("sink_list")
        return [self._obj(s) for s in self.server.sinks]

    async def source_list(self) -> list:
        await self._op("source_list")
        return [self._obj(s) for s in self.server.sources]

    async def source_output_list(self) -> list:
        await self._op("source_output_list")
        return [self._obj(s) for s in self.server.source_outputs]

    async def server_info(self) -> Any:
        await self._op("server_info")
        return SimpleNamespace(default_sink_name=self.server.default_sink,
                               default_source_name=self.server.default_source)

    async def module_load(self, name: str, args: str = "") -> int:
        await self._op(f"module_load {name} {args}")
        idx = self.server.next_index
        self.server.next_index += 1
        self.server.modules[idx] = f"{name} {args}"
        params = dict(kv.split("=", 1) for kv in args.split())
        if name == "module-null-sink":
            self.server.add_sink(params["sink_name"], idx)
        elif name == "module-virtual-source" and self.server.virtual_source_appears:
            self.server.add_source(f"output.{params['source_name']}", idx,
                                   {"device.master_device": params.get("master", "")})
        return idx

    async def module_unload(self, index: int) -> None:
        await self._op(f"module_unload {index}")
        self.server.modules.pop(index, None)
        self.server.sinks = [s for s in self.server.sinks if s["owner_module"] != index]
        self.server.sources = [s for s in self.server.sources if s["owner_module"] != index]

    async def sink_default_set(self, name: str) -> None:
        await self._op(f"sink_default_set {name}")
        self.server.default_sink = name

    async def source_default_set(self, name: str) -> None:
        await self._op(f"source_default_set {name}")
        self.server.default_source = name

    async def source_output_move(self, output_index: int, source_index: int) -> None:
        await self._op(f"source_output_move {output_index} {source_index}")
        for so in self.server.source_outputs:
            if so["index"] == output_index:
                so["source"] = source_index


class LogCapture(logging.Handler):
    def __init__(self) -> None:
        super().__init__(level=logging.DEBUG)
        self.records: List[logging.LogRecord] = []

    def emit(self, record: logging.LogRecord) -> None:
        self.records.append(record)

    def lines(self, level: int = logging.DEBUG) -> List[str]:
        return [r.getMessage() for r in self.records if r.levelno >= level]


def fresh_server(**kw: Any) -> StubServer:
    server = StubServer()
    for k, v in kw.items():
        setattr(server, k, v)
    StubPulse.server = server
    StubPulse.instances = []
    AC._fallback_announced = False
    return server


async def scenario(res: H.Results, log: LogCapture) -> None:
    AC.pulsectl_asyncio = SimpleNamespace(PulseAsync=StubPulse)
    AC.PULSE_AVAILABLE = True

    server = fresh_server()
    ctl = AC.AudioControl("t-sink")
    ok = await ctl.ensure_capture_sink("output.monitor")
    res.check("capture sink: created from the monitor name",
              ok and [s["name"] for s in server.sinks] == ["output"], server.sinks)
    res.check("capture sink: module-null-sink sink_name=output loaded",
              "module-null-sink sink_name=output" in server.modules.values(), server.modules)
    calls_before = len(server.log)
    ok = await ctl.ensure_capture_sink("output.monitor")
    res.check("capture sink: existing sink is left alone (one listing, no load)",
              ok and len(server.log) == calls_before + 1 and len(server.modules) == 1, server.log[calls_before:])
    res.check("capture sink: default name when unset",
              await ctl.ensure_capture_sink(None) and len(server.modules) == 1, server.modules)
    res.check("backend reported as pulsectl", ctl.backend == "pulsectl", ctl.backend)
    await ctl.aclose()
    res.check("aclose closes the client", StubPulse.instances[-1].closed, "")

    server = fresh_server()
    server.add_sink("output")
    server.default_sink = "output"
    server.add_source("auto_null.monitor")
    async with AC.AudioControl("t-resolve") as ctl:
        res.check("resolve: configured source kept when present",
                  await ctl.resolve_capture_source("output.monitor") == "output.monitor", "")
        res.check("resolve: missing source falls back to the default sink monitor",
                  await ctl.resolve_capture_source("gone.monitor") == "output.monitor", "")
        server.default_sink = "vanished"
        res.check("resolve: then to auto_null.monitor",
                  await ctl.resolve_capture_source("gone.monitor") == "auto_null.monitor", "")
        server.sources = []
        res.check("resolve: None when nothing can be enumerated",
                  await ctl.resolve_capture_source("gone.monitor") is None, "")

    server = fresh_server()
    out = server.add_sink("output")
    inp = server.add_sink("input")
    out_mon = next(s for s in server.sources if s["name"] == "output.monitor")
    in_mon = next(s for s in server.sources if s["name"] == "input.monitor")
    stream = server.add_source_output("pcmflux", in_mon["index"])
    async with AC.AudioControl("t-route") as ctl:
        got = await ctl.route_pcmflux(["output.monitor"])
        res.check("route: strayed pcmflux stream moved onto the target",
                  got == "output.monitor" and stream["source"] == out_mon["index"], (got, stream))
        moves = [c for c in server.log if "source_output_move" in c]
        got = await ctl.route_pcmflux(["output.monitor"])
        res.check("route: correctly attached stream is not moved again",
                  got == "output.monitor" and len([c for c in server.log if "source_output_move" in c]) == len(moves), "")
        server.source_outputs = []
        res.check("route: no pcmflux stream -> None", await ctl.route_pcmflux(["output.monitor"]) is None, "")
    del out, inp

    server = fresh_server()
    async with AC.AudioControl("t-mic") as ctl:
        idx, owned = await ctl.ensure_virtual_microphone("output.monitor", False)
        names = sorted(s["name"] for s in server.sinks)
        res.check("mic: input and output sinks created", names == ["input", "output"], names)
        res.check("mic: module-virtual-source loaded and owned",
                  idx is not None and owned and server.modules.get(idx, "").startswith(
                      "module-virtual-source source_name=SelkiesVirtualMic master=input.monitor"),
                  (idx, owned, server.modules))
        res.check("mic: defaults point at the capture sink and the virtual source",
                  (server.default_sink, server.default_source) == ("output", "output.SelkiesVirtualMic"),
                  (server.default_sink, server.default_source))
        idx2, owned2 = await ctl.ensure_virtual_microphone("output.monitor", False)
        res.check("mic: second provisioning reuses the source and does not own it",
                  idx2 == idx and owned2 is False and len(server.modules) == 3, (idx2, owned2, server.modules))
        # With pcmflux capturing on the wrong source the routing pass moves it.
        wrong = server.add_source("SelkiesVirtualMic.monitor")
        stream = server.add_source_output("pcmflux", wrong["index"])
        await ctl.ensure_virtual_microphone("output.monitor", True)
        target = next(s for s in server.sources if s["name"] == "output.monitor")
        res.check("mic: capturing pcmflux is moved onto the capture monitor",
                  stream["source"] == target["index"], stream)
        res.check("mic: unload_module removes the owned module",
                  await ctl.unload_module(idx) and idx not in server.modules, server.modules)
        server.virtual_source_appears = False
        idx3, owned3 = await ctl.ensure_virtual_microphone("output.monitor", False)
        res.check("mic: a source that never appears is reported as (None, False) and its module unloaded",
                  (idx3, owned3) == (None, False)
                  and not any(v.startswith("module-virtual-source") for v in server.modules.values()),
                  (idx3, owned3, server.modules))

    server = fresh_server()
    server.add_sink("output")
    log.records.clear()
    ctl = AC.AudioControl("t-cancel", op_timeout=0.3)
    await ctl.open()
    client = StubPulse.instances[-1]
    server.hang = True
    caller = asyncio.ensure_future(ctl.sources())
    await asyncio.sleep(0.05)
    caller.cancel()
    try:
        await caller
        cancelled_seen = False
    except asyncio.CancelledError:
        cancelled_seen = True
    res.check("cancel: the caller is cancelled", cancelled_seen, "")
    res.check("cancel: the pending server operation is NOT cancelled",
              server.cancelled_ops == 0 and len(client.pending) == 1, (server.cancelled_ops, len(client.pending)))
    server.release()
    await asyncio.sleep(0.05)
    res.check("cancel: operation finished on its own, client kept",
              not client.pending and client.connected and not client.closed, "")
    res.check("cancel: client still usable afterwards",
              [s.name for s in await ctl.sources()] == ["output.monitor"], "")

    server.hang = True
    started = asyncio.get_running_loop().time()
    got = await ctl.sources()
    elapsed = asyncio.get_running_loop().time() - started
    res.check("timeout: a hung server answers with the default after op_timeout",
              got == [] and 0.25 <= elapsed < 2.0, elapsed)
    res.check("timeout: the connection was abandoned (closed), op never cancelled",
              client.closed and server.cancelled_ops == 0, (client.closed, server.cancelled_ops))
    await asyncio.sleep(0.05)
    res.check("timeout: the abandoned operation ended through the disconnect",
              not client.pending, len(client.pending))
    server.hang = False
    got = await ctl.sources()
    res.check("timeout: next operation reconnects with a fresh client",
              [s.name for s in got] == ["output.monitor"] and StubPulse.instances[-1] is not client
              and StubPulse.instances[-1].connected, "")
    server.hang = True
    live = StubPulse.instances[-1]
    op = asyncio.ensure_future(ctl.sources())
    await asyncio.sleep(0.05)
    closer = asyncio.ensure_future(ctl.aclose())
    await asyncio.sleep(0.05)
    res.check("aclose: waits for the in-flight operation", not closer.done() and not live.closed, "")
    server.release()
    await closer
    await op
    res.check("aclose: closes once it finished, op never cancelled",
              live.closed and server.cancelled_ops == 0 and op.result() is not None, "")
    res.check("cancel/timeout: no errors logged by the control", not log.lines(logging.ERROR),
              log.lines(logging.ERROR))

    server = fresh_server()
    ctl = AC.AudioControl("t-loop")
    await ctl.open()

    def other_loop() -> str:
        async def use() -> str:
            try:
                await ctl.sinks()
                return "allowed"
            except RuntimeError as e:
                return str(e)
        return asyncio.run(use())

    verdict = await asyncio.to_thread(other_loop)
    res.check("a client refuses use from another loop", "foreign event loop" in verdict, verdict)
    await ctl.aclose()

    server = fresh_server(refuse_connect=True)
    log.records.clear()
    pactl_calls: List[List[str]] = []

    async def fake_run(self: Any, *args: str) -> str:
        pactl_calls.append(list(args))
        if args[0] == "info":
            return "Default Sink: output\nDefault Source: output.monitor\n"
        if args == ("list", "sinks"):
            return "Sink #7\n\tState: IDLE\n\tName: output\n\tOwner Module: 3\n\tProperties:\n\t\tnode.name = \"output\"\n"
        if args == ("list", "sources"):
            return ("Source #8\n\tName: output.monitor\n\tOwner Module: 3\n\tMonitor of Sink: output\n"
                    "\tProperties:\n\t\tdevice.class = \"monitor\"\n\nSource #9\n\tName: output.SelkiesVirtualMic\n"
                    "\tOwner Module: 5\n\tProperties:\n\t\tdevice.master_device = \"input.monitor\"\n")
        if args == ("list", "source-outputs"):
            return ("Source Output #12\n\tDriver: PipeWire\n\tOwner Module: n/a\n\tSource: 9\n"
                    "\tProperties:\n\t\tapplication.name = \"pcmflux\"\n\t\tmedia.name = \"Record Stream\"\n")
        if args[0] == "load-module":
            return "77\n"
        return ""

    real_exec = AC._PactlBackend._exec
    AC._PactlBackend._exec = fake_run
    try:
        ctl = AC.AudioControl("t-fallback")
        res.check("fallback: usable when the bindings cannot connect", await ctl.open() and ctl.backend == "pactl", ctl.backend)
        warn = [line for line in log.lines(logging.WARNING) if "pactl" in line]
        res.check("fallback: announced in one warning line", len(warn) == 1 and "connection failed" in warn[0], warn)
        res.check("fallback: capture sink resolved over pactl",
                  await ctl.ensure_capture_sink("output.monitor") and ["list", "sinks"] in pactl_calls, pactl_calls)
        res.check("fallback: per-command debug line present",
                  any(line.startswith("pactl fallback: pactl list sinks") for line in log.lines()), "")
        got = await ctl.route_pcmflux(["output.monitor"])
        res.check("fallback: parsed source-outputs and moved pcmflux by index",
                  got == "output.monitor" and ["move-source-output", "12", "8"] in pactl_calls, (got, pactl_calls[-1]))
        idx, owned = await ctl.ensure_virtual_microphone("output.monitor", False)
        res.check("fallback: existing virtual source reused (owner module parsed)", (idx, owned) == (5, False), (idx, owned))
        res.check("fallback: defaults set over pactl",
                  ["set-default-sink", "output"] in pactl_calls and ["set-default-source", "output.SelkiesVirtualMic"] in pactl_calls, "")
        res.check("fallback: load-module index parsed", await ctl.load_module("module-null-sink", "sink_name=x") == 77, "")
        await ctl.aclose()
        log.records.clear()
        ctl2 = AC.AudioControl("t-fallback-2")
        await ctl2.open()
        res.check("fallback: later controls announce at debug only",
                  not [line for line in log.lines(logging.WARNING) if "pactl" in line]
                  and any("pactl" in line for line in log.lines()), log.lines(logging.WARNING))
        await ctl2.aclose()

        AC.PULSE_AVAILABLE = False
        AC._fallback_announced = False
        log.records.clear()
        ctl3 = AC.AudioControl("t-nobindings")
        res.check("fallback: engaged when pulsectl_asyncio is unavailable",
                  await ctl3.open() and ctl3.backend == "pactl"
                  and any("unavailable" in line and "pactl" in line for line in log.lines(logging.WARNING)),
                  log.lines(logging.WARNING))
        await ctl3.aclose()
    finally:
        AC._PactlBackend._exec = real_exec
        AC.PULSE_AVAILABLE = True

    sample = (
        "Source #149\n\tState: IDLE\n\tName: output.monitor\n\tDescription: Monitor of output\n"
        "\tOwner Module: 536870913\n\tMonitor of Sink: output\n\tProperties:\n"
        "\t\tnode.name = \"output\"\n\t\tdevice.class = \"monitor\"\n\tFormats:\n\t\tpcm\n\n"
        "Source #41285\n\tState: SUSPENDED\n\tName: output.SelkiesVirtualMic\n\tOwner Module: 536870917\n"
        "\tProperties:\n\t\tmedia.class = \"Audio/Source\"\n\t\tnode.virtual = \"true\"\n"
    )
    nodes = AC._parse_pactl_list(sample)
    res.check("parser: two sources with index, name, owner module and properties",
              [(n.index, n.name, n.owner_module) for n in nodes]
              == [(149, "output.monitor", 536870913), (41285, "output.SelkiesVirtualMic", 536870917)]
              and nodes[0].proplist.get("device.class") == "monitor"
              and nodes[1].proplist.get("node.virtual") == "true"
              and "pcm" not in nodes[0].proplist, nodes)
    res.check("capture_sink_name derives the sink from a monitor name",
              (AC.capture_sink_name("output.monitor"), AC.capture_sink_name(None),
               AC.capture_sink_name(" speakers "), AC.capture_sink_name(".monitor"))
              == ("output", "output", "speakers", "output"), "")


def main() -> bool:
    res = H.Results("audio-control")
    log = LogCapture()
    AC.logger.addHandler(log)
    AC.logger.setLevel(logging.DEBUG)
    try:
        asyncio.run(scenario(res, log))
    finally:
        AC.logger.removeHandler(log)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
