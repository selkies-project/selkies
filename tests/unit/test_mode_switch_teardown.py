#!/usr/bin/env python3
"""A stopped websockets service leaves no capture behind.

The reconfigure pass stops captures only in the branch it takes when no display
client is left, and the reconnect grace holds those entries for seconds after
the sockets close — so on a transport mode switch that branch is not the one
taken and `shutdown()` owns the stop itself. It is asserted here against a
pipeline pass that stops nothing, which is what the real one does while the
grace still holds an entry.

The pixelflux cursor callback goes with the capture: its slot is process-wide
and outlives the capture that set it, so nothing else would let go of what the
closure captured.
"""
import asyncio
import os
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)

for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]
os.environ["SELKIES_FILE_MANAGER_PATH"] = tempfile.mkdtemp(prefix="selkies-teardown-")

import helpers as H  # noqa: E402

import selkies.selkies as S  # noqa: E402


class FakeCapture:
    """A ScreenCapture stand-in recording its stop and cursor withdrawal."""

    def __init__(self, withdrawable: bool = True) -> None:
        self.stopped = 0
        self.withdrawn = 0
        if withdrawable:
            self.clear_cursor_callback = self._clear

    def _clear(self) -> None:
        self.withdrawn += 1

    def stop_capture(self) -> None:
        self.stopped += 1


class FakeSocket:
    """A client socket that closes cleanly and nothing more."""

    def __init__(self) -> None:
        self.closed = False

    async def close(self, code=None, message=None) -> None:
        self.closed = True


def make_server(*display_ids: str, withdrawable: bool = True):
    """A DataStreamingServer carrying only the state `shutdown()` touches.

    Its `shutdown_pipelines` stops nothing, standing in for the reconfigure
    pass that finds the display clients still registered behind the grace.
    """
    srv = S.DataStreamingServer.__new__(S.DataStreamingServer)
    srv._shutdown_called = False
    srv.clients = set()
    srv.video_paused_clients = set()
    srv.display_clients = {}
    srv.capture_instances = {}
    srv.video_relay_groups = {}
    srv._persistent_capture_modules = {}
    srv._tasks_to_run = []
    srv._video_capture_lock = asyncio.Lock()
    srv.input_handler = None
    srv.metrics = None
    srv.app = object()
    srv.pipelines_ran_with = []
    srv._report_client_presence = lambda: None

    async def shutdown_pipelines():
        srv.pipelines_ran_with.append(dict(srv.display_clients))

    srv.shutdown_pipelines = shutdown_pipelines

    modules = {}
    for display_id in display_ids:
        module = FakeCapture(withdrawable)
        modules[display_id] = module
        srv._persistent_capture_modules[display_id] = module
        srv.capture_instances[display_id] = {"module": module, "callback": None,
                                             "settings": None}
        socket = FakeSocket()
        srv.clients.add(socket)
        srv.display_clients[display_id] = {"ws": socket, "width": 1280, "height": 720}
    return srv, modules


async def scenario(res: H.Results) -> None:
    srv, modules = make_server("primary", "display2")
    await srv.shutdown()

    res.check("the pipeline pass ran while the display clients were still held",
              srv.pipelines_ran_with and sorted(srv.pipelines_ran_with[0]) ==
              ["display2", "primary"], srv.pipelines_ran_with)
    res.check("every capture is stopped, on both displays",
              all(module.stopped == 1 for module in modules.values()),
              {name: module.stopped for name, module in modules.items()})
    res.check("no capture instance survives the shutdown",
              srv.capture_instances == {}, srv.capture_instances)
    res.check("the app is cleared only after the captures are down",
              srv.app is None)

    # Shutdown is idempotent, and a second pass must not stop a capture twice:
    # the module belongs to the process, not to this service.
    await srv.shutdown()
    res.check("a second shutdown stops nothing again",
              all(module.stopped == 1 for module in modules.values()),
              {name: module.stopped for name, module in modules.items()})

    res.check("the cursor callback is withdrawn from every module",
              all(module.withdrawn == 1 for module in modules.values()),
              {name: module.withdrawn for name, module in modules.items()})

    # A build with no withdrawal keeps holding what the callback captured; it
    # must not fail the shutdown.
    srv, modules = make_server("primary", withdrawable=False)
    await srv.shutdown()
    res.check("a build with no withdrawal still shuts down",
              modules["primary"].stopped == 1 and srv.app is None)

    # A capture with no display client of its own (a viewer-started primary)
    # is the case the reconfigure pass does stop; it must still be stopped here.
    srv, modules = make_server("primary")
    srv.display_clients.clear()
    srv.clients.clear()
    await srv.shutdown()
    res.check("a capture with no display client left is stopped too",
              modules["primary"].stopped == 1 and srv.capture_instances == {})

    # A module that throws on stop must not strand the ones after it.
    srv, modules = make_server("primary", "display2")

    def explode():
        raise RuntimeError("capture wedged")

    modules["primary"].stop_capture = explode
    await srv.shutdown()
    res.check("a capture that fails to stop does not strand the others",
              modules["display2"].stopped == 1 and srv.capture_instances == {})


def run() -> H.Results:
    res = H.Results("mode-switch-teardown")
    asyncio.run(scenario(res))
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not run().failed() else 1)
