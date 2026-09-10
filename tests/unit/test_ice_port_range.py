#!/usr/bin/env python3
"""The webrtc_port_range slice at the ICE layer.

Host ICE sockets must bind inside the operator's UDP window, out-of-range
configuration is rejected rather than clamped, a port another session already
holds is retried past, and an exhausted window degrades exactly like a failed
bind always has: the address is logged and skipped, nothing raises. Each case
runs on the stock loop and on uvloop, which the service prefers and which
reports a refused datagram bind as a wrapper of its own rather than the
kernel's error. Runs against the vendored `selkies.ice` alone, so a plain
interpreter with ifaddr and dnspython suffices.
"""
import asyncio
import errno
import os
import random
import socket
import sys
from typing import Any, Callable, Optional

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "src",
))

from selkies.ice import ice as ice_module  # noqa: E402
from selkies.ice.ice import Connection  # noqa: E402

ROUNDS = 6

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [ice-port-range] {label}  {detail}",
          flush=True)


def rejects(port_range) -> bool:
    try:
        Connection(ice_controlling=True, port_range=port_range)
    except ValueError:
        return True
    return False


def gatherer(window: tuple) -> Connection:
    return Connection(ice_controlling=True, use_ipv6=False, port_range=window)


def host_ports(candidates: list) -> list:
    return [c.port for c in candidates if c.type == "host"]


def occupied_window(taken: int) -> tuple:
    """A window whose `taken` lowest ports are held, with its highest free.

    Returns the window, the sockets holding it, and the free port. The window
    is placed above a probed-free base and confirmed bindable end to end, so
    a port another process holds is found here rather than mid-check.
    """
    for _ in range(20):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            probe.bind(("127.0.0.1", 0))
            base = probe.getsockname()[1]
        if base + taken > 65535:
            continue
        held: list = []
        try:
            for offset in range(taken + 1):
                sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                sock.bind(("127.0.0.1", base + offset))
                held.append(sock)
        except OSError:
            for sock in held:
                sock.close()
            continue
        held.pop().close()
        return (base, base + taken), held, base + taken
    raise OSError("no window of consecutive free UDP ports on 127.0.0.1")


def refusal(own: Optional[int], cause: Optional[int]) -> OSError:
    """A bind failure carrying `own` itself and `cause` on its cause.

    An `own` of None is the shape uvloop raises for a datagram bind it could
    not place: a wrapper naming the address, with no code of its own.
    """
    exc = (OSError(own, os.strerror(own)) if own is not None
           else OSError("could not bind to local_addr ('127.0.0.1', 0)"))
    if cause is not None:
        exc.__cause__ = OSError(cause, os.strerror(cause))
    return exc


class _RefusingLoop:
    """The running loop with every datagram bind refused, and the ports noted."""

    def __init__(self, loop: Any, raises: Callable[[], OSError]) -> None:
        self._loop = loop
        self._raises = raises
        self.tried: list = []

    def __getattr__(self, name: str) -> Any:
        return getattr(self._loop, name)

    async def create_datagram_endpoint(self, protocol_factory, local_addr=None,
                                       **kwargs) -> Any:
        self.tried.append(local_addr[1])
        raise self._raises()


class _StubbedAsyncio:
    """The asyncio module with `get_running_loop` answering `loop`."""

    def __init__(self, loop: Any) -> None:
        self._loop = loop

    def __getattr__(self, name: str) -> Any:
        return getattr(asyncio, name)

    def get_running_loop(self) -> Any:
        return self._loop


async def gather_on(loop: Any, window: tuple) -> list:
    """Gather component candidates with `loop` in place of the running one."""
    conn = gatherer(window)
    real = ice_module.asyncio
    ice_module.asyncio = _StubbedAsyncio(loop)
    try:
        return await conn.get_component_candidates(1, ["127.0.0.1"])
    finally:
        ice_module.asyncio = real
        await conn.close()


async def bind_inside_window(on: str) -> None:
    # A window of 24 ports above a probed-free base keeps the check off the
    # single-port race where another process grabs the port between probe
    # and bind.
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        probe.bind(("127.0.0.1", 0))
        base = min(probe.getsockname()[1], 65535 - 24)
    window = (base, base + 24)
    conn = gatherer(window)
    try:
        ports = host_ports(await conn.get_component_candidates(1, ["127.0.0.1"]))
        check(f"host candidate binds inside the window on {on}",
              len(ports) == 1 and window[0] <= ports[0] <= window[1],
              f"window={window} got={ports}")
    finally:
        await conn.close()


async def held_ports_are_retried_past(on: str) -> None:
    """The window tried in order, so reaching the free port takes the retry.

    Left shuffled, a draw that opens on the free port would pass without one.
    """
    window, held, free = occupied_window(3)
    shuffle = random.shuffle
    random.shuffle = lambda seq: None
    conn = gatherer(window)
    try:
        ports = host_ports(await conn.get_component_candidates(1, ["127.0.0.1"]))
        check(f"the held ports of a window are retried past on {on}",
              ports == [free], f"window={window} free={free} got={ports}")
    finally:
        random.shuffle = shuffle
        await conn.close()
        for sock in held:
            sock.close()


async def every_order_finds_the_free_port(on: str) -> None:
    """The shuffled order a session really draws, over enough draws to cover both ends."""
    window, held, free = occupied_window(3)
    try:
        landed = []
        for _ in range(ROUNDS):
            conn = gatherer(window)
            try:
                landed += host_ports(
                    await conn.get_component_candidates(1, ["127.0.0.1"]))
            finally:
                await conn.close()
        check(f"every draw of the window's order finds the free port on {on}",
              landed == [free] * ROUNDS, f"free={free} got={landed}")
    finally:
        for sock in held:
            sock.close()


async def exhausted_window_skips_address(on: str) -> None:
    blocker = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    blocker.bind(("127.0.0.1", 0))
    port = blocker.getsockname()[1]
    conn = gatherer((port, port))
    try:
        candidates = await conn.get_component_candidates(1, ["127.0.0.1"])
        check(f"exhausted window skips the address without raising on {on}",
              candidates == [], f"got={candidates}")
    finally:
        await conn.close()
        blocker.close()


async def what_each_refusal_costs(on: str) -> None:
    """Which bind failures are worth another port, by the ports they cost.

    Only an occupied port can be cured by trying another, and the running loop
    decides where that shows: uvloop's wrapper carries the kernel's code on
    its cause alone. No loop raises the remaining shapes to order, so each is
    injected in place of the bind and counted.
    """
    window = (50000, 50003)
    ports = window[1] - window[0] + 1
    shapes = [
        ("an occupied port", errno.EADDRINUSE, None, ports),
        ("an occupied port behind the loop's wrapper", None, errno.EADDRINUSE, ports),
        ("a wrapper naming no cause", None, None, 1),
        ("a cause another port cannot cure", None, errno.EACCES, 1),
        ("a code of the loop's own over its cause", errno.EACCES, errno.EADDRINUSE, 1),
    ]
    for label, own, cause, expect in shapes:
        loop = _RefusingLoop(asyncio.get_running_loop(),
                             lambda own=own, cause=cause: refusal(own, cause))
        candidates = await gather_on(loop, window)
        check(f"{label} costs {expect} of the window's {ports} ports on {on}",
              len(loop.tried) == expect and candidates == [],
              f"tried={loop.tried} candidates={candidates}")


async def cases(on: str) -> None:
    await bind_inside_window(on)
    await held_ports_are_retried_past(on)
    await every_order_finds_the_free_port(on)
    await exhausted_window_skips_address(on)
    await what_each_refusal_costs(on)


def loops() -> list:
    """Each event loop the service can run on, with the runner that owns it."""
    runners = [("asyncio", asyncio.run)]
    try:
        import uvloop
    except ImportError:
        return runners
    runners.append(("uvloop", uvloop.run))
    return runners


def main() -> int:
    for bad in ((1023, 2000), (2000, 1000), (5000, 70000), (0, 0)):
        check(f"rejects port_range {bad}", rejects(bad))
    check("default stays ephemeral",
          Connection(ice_controlling=True)._port_range is None)
    runners = loops()
    print(f"[ice-port-range] cases run on {', '.join(n for n, _ in runners)}", flush=True)
    for on, run in runners:
        run(cases(on))
    print(f"[ice-port-range] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
