#!/usr/bin/env python3
"""The capture demand watch and its routing.

One page is asked for a device while something in the session reads it, the release waits out
the hold-off and a run of idle readings, and a page that may no longer capture is told to let
go before another is asked. Both transports' candidate lists and sends run on the objects the
routing reads, with fakes standing in for pages; the watches themselves are exercised through
a subclass whose reader the test flips, so no device and no sound server is touched.
"""
import asyncio
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "src"))

from selkies import audit  # noqa: E402
from selkies import capture_demand as cd  # noqa: E402
from selkies.settings import settings  # noqa: E402

failures = 0


def check(name: str, ok: bool, detail: str = "") -> None:
    global failures
    print(("PASS  " if ok else "FAIL  ") + name + (f"  {detail}" if detail and not ok else ""))
    if not ok:
        failures += 1


class Port:
    """A transport as the watch sees it: an ordered candidate list and a send that records, or
    fails for a page that has stopped reading."""

    def __init__(self, *pages: str) -> None:
        self.pages = list(pages)
        self.dead: set = set()
        self.sent: list = []

    def capture_candidates(self) -> list:
        return list(self.pages)

    async def tell_capture(self, page: str, subject: str, wanted: bool) -> bool:
        if page in self.dead:
            return False
        self.sent.append((page, wanted))
        return True


class Watch(cd.CaptureDemand):
    """A watch over a device the test reads and releases by hand."""

    subject = "webcam"
    release_seconds = 0.2

    def __init__(self) -> None:
        super().__init__()
        self.reading = None
        self.prepared = 0

    async def prepare(self) -> bool:
        self.prepared += 1
        return True

    async def reader(self):
        return self.reading


async def run_watch_checks() -> None:
    cd.POLL_SECONDS = 0.02
    watch, port = Watch(), Port("a")
    watch.attach(port)
    await asyncio.sleep(0.1)
    check("the device is brought up when the first transport attaches", watch.prepared == 1)
    check("an unread device asks for nothing", port.sent == [] and not watch.wanted, str(port.sent))
    watch.reading = "an application"
    await asyncio.sleep(0.1)
    check("a reader is passed on at the poll that finds it", port.sent == [("a", True)], str(port.sent))
    watch.reading = None
    await asyncio.sleep(0.1)
    check("its absence is not believed at once", port.sent == [("a", True)], str(port.sent))
    await asyncio.sleep(0.3)
    check("and is once it has lasted the hold-off", port.sent == [("a", True), ("a", False)],
          str(port.sent))
    for reading in ("an application", None, "an application"):
        watch.reading = reading
        await asyncio.sleep(0.1)
    check("a reader that returns inside the hold-off changes nothing",
          port.sent == [("a", True), ("a", False), ("a", True)], str(port.sent))
    watch.attach(port)
    check("attaching the same transport twice keeps one entry and one watch",
          watch._ports == [port] and watch._task is not None and not watch._task.done())
    task = watch._task
    watch.detach(port)
    await asyncio.sleep(0.1)
    check("the watch ends with the last transport", task.done())
    check("and forgets the answer, so the next page is not told a stale one", not watch.wanted)
    watch.attach(port)
    await asyncio.sleep(0.1)
    check("a later transport restarts the watch and is told afresh",
          port.sent[-1] == ("a", True) and len(port.sent) == 4, str(port.sent))
    watch.detach(port)
    await asyncio.sleep(0.1)


async def run_hold_off_checks() -> None:
    """Elapsed time alone must not release: a long poll cycle would satisfy it on one reading."""
    class Quick(Watch):
        release_seconds = 0.0

    watch, port = Quick(), Port("a")
    watch._ports, watch._wanted, watch._told = [port], True, (port, "a")
    await watch._poll()
    await watch._poll()
    check("two idle readings do not release even with the hold-off elapsed",
          watch.wanted and port.sent == [], str(port.sent))
    await watch._poll()
    check("a third does", not watch.wanted and port.sent == [("a", False)], str(port.sent))


async def run_routing_checks() -> None:
    watch, port = Watch(), Port("first", "second")
    watch._ports, watch._wanted = [port], True
    await watch.route()
    check("only the first candidate is asked", port.sent == [("first", True)], str(port.sent))
    await watch.route()
    check("and not again while it stays the candidate", port.sent == [("first", True)], str(port.sent))
    port.pages = ["second", "first"]
    await watch.route()
    check("a page that stops being the candidate is released before the next is asked",
          port.sent[1:] == [("first", False), ("second", True)], str(port.sent))
    port.pages = []
    await watch.route()
    check("the last candidate leaving is still released", port.sent[-1] == ("second", False),
          str(port.sent))
    port.pages, port.dead = ["wedged", "third"], {"wedged"}
    await watch.route()
    check("a send that fails hands the device to the next page", port.sent[-1] == ("third", True),
          str(port.sent))
    other = Port("elsewhere")
    watch._ports = [port, other]
    await watch.route()
    check("a second transport's pages come after the first's", other.sent == [], str(other.sent))
    port.pages = []
    await watch.route()
    check("and take over when the first has none",
          port.sent[-1] == ("third", False) and other.sent == [("elsewhere", True)],
          f"{port.sent} {other.sent}")
    watch.detach(other)
    check("detaching the transport that was asked forgets it without a send",
          watch._told is None and other.sent == [("elsewhere", True)], str(other.sent))
    watch._wanted = False
    port.pages = ["fourth"]
    await watch.route()
    check("nothing is asked while the device is unread", port.sent[-1] == ("third", False),
          str(port.sent))

    # Nobody to ask: the answer is dropped rather than kept for a page that arrives later.
    watch, port = Watch(), Port("a")
    watch._ports, watch._wanted, watch._told = [port], True, (port, "a")
    port.pages = []
    await watch._poll()
    check("a poll with no candidate releases the page asked before and forgets the answer",
          port.sent == [("a", False)] and not watch.wanted, str(port.sent))


async def run_concurrency_checks() -> None:
    """Two routes in flight must not interleave their sends."""
    class Slow(Port):
        in_flight = 0
        overlapped = False

        async def tell_capture(self, page, subject, wanted):
            Slow.in_flight += 1
            Slow.overlapped = Slow.overlapped or Slow.in_flight > 1
            await asyncio.sleep(0.02)
            Slow.in_flight -= 1
            return await super().tell_capture(page, subject, wanted)

    watch, port = Watch(), Slow("a")
    watch._ports, watch._wanted = [port], True
    await asyncio.gather(watch.route(), watch.route(), watch.route())
    check("overlapping routes send one at a time", not Slow.overlapped and port.sent == [("a", True)],
          str(port.sent))


async def run_audit_checks() -> None:
    events: list = []
    real_emit, real_url = audit.emit, settings.audit_webhook_url
    try:
        audit.emit = lambda name, **kw: events.append((name, kw))
        settings.audit_webhook_url = "https://example.invalid/audit"
        watch, port = Watch(), Port("a")
        watch._ports, watch.reading = [port], "ffmpeg"
        await watch._poll()
        check("an ask is recorded with what reads the device",
              events == [("capture.demand", {"subject": "webcam", "action": "ask", "reader": "ffmpeg"})],
              str(events))
        watch.reading = None
        for _ in range(cd.MIN_IDLE_POLLS):
            await watch._poll()
            await asyncio.sleep(0.1)
        check("and the release, without one",
              events[-1] == ("capture.demand", {"subject": "webcam", "action": "release", "reader": None}),
              str(events))
        await watch._poll()
        check("an unchanged answer records nothing", len(events) == 2, str(events))
    finally:
        audit.emit, settings.audit_webhook_url = real_emit, real_url


class Socket:
    """A page's websocket that records what it was sent, or refuses it."""

    def __init__(self, wedged: bool = False) -> None:
        self.wedged = wedged
        self.sent: list = []

    async def send_str(self, text: str) -> None:
        if self.wedged:
            raise ConnectionResetError("this page has stopped reading")
        self.sent.append(text)


class Channel:
    def __init__(self, state: str = "open") -> None:
        self.readyState = state
        self.sent: list = []


async def run_transport_checks() -> None:
    """Each transport's candidate list and send, on the tables the routing reads."""
    from selkies import websockets_mode as wm
    from selkies.webrtc_engine import ClientType, RTCApp

    server = object.__new__(wm.DataStreamingServer)
    first, second, viewer, wedged = Socket(), Socket(), Socket(), Socket(wedged=True)
    server.clients = {first, second, viewer, wedged}
    server.display_clients = {}
    wm.client_permissions.clear()
    for ws, role in ((first, "controller"), (second, "controller"), (viewer, "viewer"), (wedged, "controller")):
        wm.client_permissions[ws] = {"role": role}
    check("websockets: controllers on the primary display are candidates in connection order",
          server.capture_candidates() == [first, second, wedged])
    server.display_clients = {"display2": {"ws": first}}
    check("websockets: a page showing a secondary display is not", server.capture_candidates() == [second, wedged])
    server.clients.discard(second)
    check("websockets: nor a page whose socket is gone", server.capture_candidates() == [wedged])
    check("websockets: the verb goes out as CAPTURE_DEMAND <subject> <0|1>",
          await server.tell_capture(first, "webcam", True) and first.sent == ["CAPTURE_DEMAND webcam 1"],
          str(first.sent))
    check("websockets: a page that has stopped reading is reported, not raised",
          await server.tell_capture(wedged, "webcam", True) is False)
    wm.client_permissions.clear()

    app = RTCApp(async_event_loop=asyncio.get_event_loop(), encoder="h264enc", stun_servers=[], turn_servers=[])
    one, two, closed = Channel(), Channel(), Channel("closed")
    app.peer_connections = {
        "p1": {"data_channel": one, "display_id": "primary", "client_type": ClientType.CONTROLLER},
        "p2": {"data_channel": two, "display_id": "primary", "client_type": ClientType.CONTROLLER},
        "p3": {"data_channel": Channel(), "display_id": "primary", "client_type": ClientType.VIEWER},
        "p4": {"data_channel": Channel(), "display_id": "display2", "client_type": ClientType.CONTROLLER},
        "p5": {"data_channel": closed, "display_id": "primary", "client_type": ClientType.CONTROLLER},
        "p6": {"data_channel": None, "display_id": "primary", "client_type": ClientType.CONTROLLER},
    }
    app.send_message_to_channel = lambda ch, kind, payload: ch.sent.append((kind, payload["action"]))
    check("webrtc: open channels of primary controllers are candidates in connection order",
          app.capture_candidates() == [one, two])
    check("webrtc: the action goes out as a system message",
          await app.tell_capture(one, "microphone", False) and one.sent == [("system", "capture_demand,microphone,0")],
          str(one.sent))
    check("webrtc: a closed channel is reported, not written to",
          await app.tell_capture(closed, "webcam", True) is False and closed.sent == [])


async def run_sync_checks() -> None:
    """`sync` attaches a transport to the watches the settings turn on and routes at once."""
    originals = (settings.webcam_enabled, settings.webcam_on_start, settings.microphone_on_start)
    try:
        settings.webcam_enabled, settings.webcam_on_start = (True, False), "demand"
        settings.microphone_on_start = "false"
        cd._demands.clear()
        watch = cd._demands["webcam"] = Watch()
        port, other = Port("a"), Port("b")
        await cd.sync(port)
        check("the transport is attached to the watch the settings turn on", watch._ports == [port])
        check("and none is created for a device left on its start state", list(cd._demands) == ["webcam"])
        watch.reading = "an application"
        await asyncio.sleep(0.1)
        await cd.sync(other)
        check("a second transport joining while the first holds the answer is not asked",
              port.sent == [("a", True)] and other.sent == [], f"{port.sent} {other.sent}")
        cd.detach(port)
        await cd.sync(other)
        check("the first transport leaving hands the device to the second",
              other.sent == [("b", True)], str(other.sent))
        cd.detach(other)
        await asyncio.sleep(0.1)
        check("detaching the last transport ends the watch", watch._task.done())
        settings.webcam_on_start = "false"
        await cd.sync(port)
        check("a policy other than demand attaches nothing", watch._ports == [])
    finally:
        settings.webcam_enabled, settings.webcam_on_start, settings.microphone_on_start = originals
        cd._demands.clear()


def main() -> int:
    for block in (run_watch_checks, run_hold_off_checks, run_routing_checks, run_concurrency_checks,
                  run_audit_checks, run_transport_checks, run_sync_checks):
        asyncio.run(block())
    print(f"\n{failures} failure(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
