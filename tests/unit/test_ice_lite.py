#!/usr/bin/env python3
"""ICE-lite at the ICE layer: the server answers checks, it sends none.

An ICE-lite connection is the controlled agent whatever it was asked to be,
takes no STUN or TURN server, completes on the full peer's nomination without
a check of its own (the peer's checks may even arrive before it starts),
answers a peer that claims the controlled role with 487 rather than switching,
and gives up within the inbound-check bound when nobody ever checks. Runs
against the vendored `selkies.ice` alone.
"""
import asyncio
import os
import socket
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "src",
))

from selkies.ice import Connection  # noqa: E402
from selkies.ice import ice as ice_module  # noqa: E402
from selkies.ice import stun  # noqa: E402
from selkies.ice.candidate import candidate_priority  # noqa: E402

passed = failed = 0
LOOPBACK = "127.0.0.1"


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [ice-lite] {label}  {detail}", flush=True)


async def gather_loopback(conn: Connection) -> list:
    candidates = await conn.get_component_candidates(1, [LOOPBACK])
    conn._local_candidates += candidates
    conn._local_candidates_end = True
    return candidates


async def pair_up(a: Connection, b: Connection) -> None:
    for candidate in b.local_candidates:
        await a.add_remote_candidate(candidate)
    await a.add_remote_candidate(None)
    for candidate in a.local_candidates:
        await b.add_remote_candidate(candidate)
    await b.add_remote_candidate(None)
    a.remote_username, a.remote_password = b.local_username, b.local_password
    b.remote_username, b.remote_password = a.local_username, a.local_password


def count_requests(conn: Connection) -> list:
    """Every STUN request `conn` sends from now on, by wrapping its protocols."""
    sent: list = []
    for protocol in conn._protocols:
        original = protocol.send_stun

        def send_stun(message, addr, original=original):
            if message.message_class == stun.Class.REQUEST:
                sent.append(message)
            original(message, addr)

        protocol.send_stun = send_stun
    return sent


async def lite_against_full(peer_first: bool) -> None:
    label = "peer checks first" if peer_first else "server waits first"
    server = Connection(ice_controlling=True, use_ipv6=False, ice_lite=True)
    client = Connection(ice_controlling=True, use_ipv6=False)
    client.remote_is_lite = True
    try:
        await gather_loopback(server)
        await gather_loopback(client)
        await pair_up(server, client)
        sent = count_requests(server)
        if peer_first:
            client_task = asyncio.create_task(client.connect())
            await asyncio.sleep(0.3)
            server_task = asyncio.create_task(server.connect())
        else:
            server_task = asyncio.create_task(server.connect())
            await asyncio.sleep(0.3)
            client_task = asyncio.create_task(client.connect())
        await asyncio.wait_for(asyncio.gather(server_task, client_task), 10)
        check(f"{label}: both complete", True)
        check(f"{label}: the ICE-lite agent sent no check", not sent, len(sent))
        check(f"{label}: roles are controlled and controlling",
              server.ice_controlling is False and client.ice_controlling is True)
        await server.send(b"lite-to-full")
        await client.send(b"full-to-lite")
        check(f"{label}: data flows both ways",
              await asyncio.wait_for(client.recv(), 3) == b"lite-to-full"
              and await asyncio.wait_for(server.recv(), 3) == b"full-to-lite")
        server.switch_role(ice_controlling=True)
        check(f"{label}: a role switch to controlling is refused", server.ice_controlling is False)
    finally:
        await server.close()
        await client.close()


async def role_conflict_and_timeout() -> None:
    server = Connection(ice_controlling=False, use_ipv6=False, ice_lite=True)
    try:
        candidates = await gather_loopback(server)
        server.remote_username, server.remote_password = "peerfrag", "peerpasswordpeerpassword"
        request = stun.Message(message_method=stun.Method.BINDING, message_class=stun.Class.REQUEST)
        request.attributes["USERNAME"] = f"{server.local_username}:peerfrag"
        request.attributes["PRIORITY"] = candidate_priority(1, "prflx")
        request.attributes["ICE-CONTROLLED"] = 7
        request.add_message_integrity(server.local_password.encode())
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as peer:
            peer.settimeout(3)
            peer.sendto(bytes(request), (LOOPBACK, candidates[0].port))
            loop = asyncio.get_running_loop()
            data, _ = await loop.run_in_executor(None, peer.recvfrom, 1500)
        response = stun.parse_message(data)
        check("a peer claiming the controlled role is answered 487",
              response.message_class == stun.Class.ERROR
              and response.attributes.get("ERROR-CODE", (0,))[0] == 487, response)

        ice_module.INBOUND_CHECK_TIMEOUT = 1
        from selkies.ice.candidate import Candidate
        await server.add_remote_candidate(Candidate(
            foundation="1", component=1, transport="udp", priority=1,
            host=LOOPBACK, port=candidates[0].port + 1, type="host"))
        await server.add_remote_candidate(None)
        started = asyncio.get_running_loop().time()
        try:
            await asyncio.wait_for(server.connect(), 5)
            check("with no peer check the wait ends in failure", False, "connected")
        except ConnectionError:
            elapsed = asyncio.get_running_loop().time() - started
            check("with no peer check the wait ends in failure within the bound",
                  0.8 <= elapsed <= 3, f"{elapsed:.1f}s")
    finally:
        await server.close()


def construction() -> None:
    check("an ICE-lite agent is controlled however it was asked",
          Connection(ice_controlling=True, ice_lite=True).ice_controlling is False)
    for kwargs in ({"stun_server": (LOOPBACK, 3478)}, {"turn_server": (LOOPBACK, 3478)}):
        try:
            Connection(ice_controlling=False, ice_lite=True, **kwargs)
            check(f"ICE-lite refuses {list(kwargs)[0]}", False, "no ValueError")
        except ValueError:
            check(f"ICE-lite refuses {list(kwargs)[0]}", True)


async def main_async() -> None:
    await lite_against_full(peer_first=False)
    await lite_against_full(peer_first=True)
    await role_conflict_and_timeout()


def main() -> int:
    construction()
    asyncio.run(main_async())
    print(f"[ice-lite] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
