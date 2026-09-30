#!/usr/bin/env python3
"""Consent freshness (RFC 7675) on the server's ICE-lite connection.

A consent check answered only after its first retransmission timeout, as
from behind a queue standing half a second deep on a slow link, keeps the
connection; one never answered ends it within the consent timeout and one
check's retransmissions. Runs against the vendored `selkies.ice` alone, with
the interval and timeout scaled down.
"""
import asyncio
import os
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "src",
))

from selkies.ice import Connection  # noqa: E402
from selkies.ice import ice as ice_module  # noqa: E402
from selkies.ice import stun  # noqa: E402

passed = failed = 0
LOOPBACK = "127.0.0.1"
INTERVAL = 0.2
TIMEOUT = 2.0
# One unanswered check: the RTO doubling from 0.5 s over its retransmissions.
CHECK_S = sum(stun.RETRY_RTO * 2 ** i for i in range(ice_module.CONSENT_RETRANSMISSIONS + 1))


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [ice-consent] {label}  {detail}", flush=True)


async def connected_pair() -> tuple:
    server = Connection(ice_controlling=False, use_ipv6=False, ice_lite=True)
    client = Connection(ice_controlling=True, use_ipv6=False)
    client.remote_is_lite = True
    for conn in (server, client):
        conn._local_candidates += await conn.get_component_candidates(1, [LOOPBACK])
        conn._local_candidates_end = True
    for a, b in ((server, client), (client, server)):
        for candidate in b.local_candidates:
            await a.add_remote_candidate(candidate)
        await a.add_remote_candidate(None)
        a.remote_username, a.remote_password = b.local_username, b.local_password
    await asyncio.wait_for(asyncio.gather(server.connect(), client.connect()), 10)
    return server, client


def answer_after(conn: Connection, delay) -> list:
    """Hold every STUN answer `conn` sends for `delay` seconds, or drop it with None;
    returns the held sends, for cancelling at close."""
    loop = asyncio.get_running_loop()
    held: list = []
    for protocol in conn._protocols:
        original = protocol.send_stun

        def send_stun(message, addr, original=original):
            if message.message_class == stun.Class.REQUEST:
                original(message, addr)
            elif delay is not None:
                held.append(loop.call_later(delay, original, message, addr))

        protocol.send_stun = send_stun
    return held


def consented(conn: Connection) -> bool:
    task = conn._query_consent_task
    return task is not None and not task.done()


async def late_answers() -> None:
    server, client = await connected_pair()
    held: list = []
    try:
        held = answer_after(client, 0.8)
        await asyncio.sleep(2.5 * TIMEOUT)
        check("answers past the first RTO keep consent", consented(server))
    finally:
        for handle in held:
            handle.cancel()
        await server.close()
        await client.close()


async def no_answers() -> None:
    server, client = await connected_pair()
    try:
        answer_after(client, None)
        started = asyncio.get_running_loop().time()
        bound = TIMEOUT + CHECK_S + 1.2 * INTERVAL + 1.0
        while consented(server) and asyncio.get_running_loop().time() - started < bound:
            await asyncio.sleep(0.05)
        elapsed = asyncio.get_running_loop().time() - started
        check("an unanswered peer loses consent within the timeout and one check",
              not consented(server) and TIMEOUT <= elapsed <= bound, f"{elapsed:.1f}s")
    finally:
        await server.close()
        await client.close()


async def main_async() -> None:
    ice_module.CONSENT_INTERVAL = INTERVAL
    ice_module.CONSENT_TIMEOUT = TIMEOUT
    await late_answers()
    await no_answers()


def main() -> int:
    asyncio.run(main_async())
    print(f"[ice-consent] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
