#!/usr/bin/env python3
"""What a peer's remote candidates may cost the loop every client shares.

A connection takes a candidate once, however often the peer repeats it, and
MAX_REMOTE_CANDIDATES in all, so neither the pairing nor the check list sort
each one costs grows without bound. An mDNS candidate is resolved off the
caller's await, since the signaling that hands candidates over serves every
peer, and a name that did not resolve is not looked up again; end-of-candidates
waits for the lookups still out without holding the caller either. Runs a real
connection on loopback with a stand-in mDNS resolver; no browser.
"""
import asyncio
import os
import sys
import time

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "src",
))

from selkies.ice import Candidate, Connection  # noqa: E402
from selkies.ice import ice as ice_module  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [ice-remote] {label}  {detail}", flush=True)


def candidate(host: str, port: int) -> Candidate:
    return Candidate.from_sdp(f"1 1 udp 2113664127 {host} {port} typ host")


class SlowResolver:
    """An mDNS resolver that answers after a while, and only for `known`."""

    def __init__(self, known: dict) -> None:
        self.known = known
        self.asked: list = []

    async def resolve(self, hostname: str, timeout: float = 1.0):
        self.asked.append(hostname)
        await asyncio.sleep(0.3)
        return self.known.get(hostname)


async def main() -> None:
    conn = Connection(ice_controlling=True, use_ipv6=False)
    conn._local_candidates += await conn.get_component_candidates(1, ["127.0.0.1"])
    conn._local_candidates_end = True
    try:
        start = time.monotonic()
        for _ in range(2000):
            await conn.add_remote_candidate(candidate("127.0.0.1", 30000))
        check("a candidate repeated 2000 times is taken once",
              len(conn.remote_candidates) == 1 and len(conn._check_list) == 1,
              (len(conn.remote_candidates), len(conn._check_list)))
        for port in range(31000, 31500):
            await conn.add_remote_candidate(candidate("127.0.0.1", port))
        took = time.monotonic() - start
        check(f"and {ice_module.MAX_REMOTE_CANDIDATES} distinct ones in all",
              len(conn.remote_candidates) == ice_module.MAX_REMOTE_CANDIDATES, len(conn.remote_candidates))
        check("2500 offers cost the loop well under a second", took < 1.0, f"{took:.3f} s")
    finally:
        await conn.close()

    resolver = SlowResolver({"b.local": "127.0.0.1"})

    async def stand_in(subscriber):
        return resolver

    saved = ice_module.get_or_create_mdns_protocol
    ice_module.get_or_create_mdns_protocol = stand_in
    conn = Connection(ice_controlling=True, use_ipv6=False)
    conn._local_candidates += await conn.get_component_candidates(1, ["127.0.0.1"])
    conn._local_candidates_end = True
    try:
        start = time.monotonic()
        await conn.add_remote_candidate(candidate("a.local", 40000))
        await conn.add_remote_candidate(candidate("b.local", 40001))
        await conn.add_remote_candidate(None)
        held = time.monotonic() - start
        check("mDNS candidates and end-of-candidates return without the lookups", held < 0.1, f"{held:.3f} s")
        check("and end-of-candidates waits for them", not conn._remote_candidates_end)
        await asyncio.sleep(0.5)
        check("the name that resolves is taken", [c.host for c in conn.remote_candidates] == ["127.0.0.1"],
              [c.host for c in conn.remote_candidates])
        check("end-of-candidates lands once the lookups settle", conn._remote_candidates_end)
    finally:
        await conn.close()

    conn = Connection(ice_controlling=True, use_ipv6=False)
    try:
        await conn.add_remote_candidate(candidate("a.local", 40000))
        await asyncio.sleep(0.5)
        resolver.asked.clear()
        await conn.add_remote_candidate(candidate("a.local", 40002))
        await asyncio.sleep(0.5)
        check("a name that did not resolve is not looked up again", resolver.asked == [], resolver.asked)
    finally:
        await conn.close()
        ice_module.get_or_create_mdns_protocol = saved

    print(f"[ice-remote] {passed}/{passed + failed} passed", flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    asyncio.run(main())
