#!/usr/bin/env python3
"""The webrtc_port_range slice at the ICE layer.

Host ICE sockets must bind inside the operator's UDP window, out-of-range
configuration is rejected rather than clamped, and an exhausted window
degrades exactly like a failed bind always has: the address is logged and
skipped, nothing raises. Runs against the vendored `selkies.ice` alone, so
a plain interpreter with ifaddr and dnspython suffices.
"""
import asyncio
import os
import socket
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "src",
))

from selkies.ice.ice import Connection  # noqa: E402

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


async def bind_inside_window() -> None:
    # A window of 24 ports above a probed-free base keeps the check off the
    # single-port race where another process grabs the port between probe
    # and bind.
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        probe.bind(("127.0.0.1", 0))
        base = min(probe.getsockname()[1], 65535 - 24)
    window = (base, base + 24)
    conn = Connection(ice_controlling=True, use_ipv6=False, port_range=window)
    try:
        candidates = await conn.get_component_candidates(1, ["127.0.0.1"])
        hosts = [c.port for c in candidates if c.type == "host"]
        check("host candidate binds inside the window",
              len(hosts) == 1 and window[0] <= hosts[0] <= window[1],
              f"window={window} got={hosts}")
    finally:
        await conn.close()


async def exhausted_window_skips_address() -> None:
    blocker = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    blocker.bind(("127.0.0.1", 0))
    port = blocker.getsockname()[1]
    conn = Connection(ice_controlling=True, use_ipv6=False,
                      port_range=(port, port))
    try:
        candidates = await conn.get_component_candidates(1, ["127.0.0.1"])
        check("exhausted window skips the address without raising",
              candidates == [], f"got={candidates}")
    finally:
        await conn.close()
        blocker.close()


def main() -> int:
    for bad in ((1023, 2000), (2000, 1000), (5000, 70000), (0, 0)):
        check(f"rejects port_range {bad}", rejects(bad))
    check("default stays ephemeral",
          Connection(ice_controlling=True)._port_range is None)
    asyncio.run(bind_inside_window())
    asyncio.run(exhausted_window_skips_address())
    print(f"[ice-port-range] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
