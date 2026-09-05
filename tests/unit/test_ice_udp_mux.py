#!/usr/bin/env python3
"""The UDP mux at the ICE layer: every session on one port.

Two connections attached to one mux socket connect to peers of their own at
the same time and each receives only its own peer's data; the pair that
remains keeps working after the other closes; a STUN server's answer reaches
the connection that asked without the server being learned as anyone's peer;
a port in use fails `open` at once and leaves nothing bound; an address the
mux was not opened on is bound on first use; NAT1TO1 rewrites a muxed
candidate like any other; a configured port range yields to the mux; and a
datagram nobody attached for is dropped without disturbing a session. Runs
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

from selkies.ice import Connection, UdpMux  # noqa: E402
from selkies.ice import stun  # noqa: E402

passed = failed = 0
LOOPBACK = "127.0.0.1"


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [ice-udp-mux] {label}  {detail}", flush=True)


def free_udp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        probe.bind((LOOPBACK, 0))
        return probe.getsockname()[1]


async def gather_loopback(conn: Connection, addresses=(LOOPBACK,)) -> list:
    """Gather host candidates on loopback, which the interface scan skips."""
    candidates = await conn.get_component_candidates(1, list(addresses))
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


async def nothing_arrives(conn: Connection, seconds: float = 0.4) -> bool:
    try:
        await asyncio.wait_for(conn.recv(), seconds)
    except asyncio.TimeoutError:
        return True
    return False


class StunResponder(asyncio.DatagramProtocol):
    """A loopback STUN server answering binding requests with the source address."""

    def __init__(self) -> None:
        self.transport = None
        self.requests = 0

    def connection_made(self, transport) -> None:
        self.transport = transport

    def datagram_received(self, data, addr) -> None:
        try:
            message = stun.parse_message(data)
        except ValueError:
            return
        if message.message_class != stun.Class.REQUEST:
            return
        self.requests += 1
        response = stun.Message(
            message_method=message.message_method,
            message_class=stun.Class.RESPONSE,
            transaction_id=message.transaction_id,
        )
        response.attributes["XOR-MAPPED-ADDRESS"] = (addr[0], addr[1])
        self.transport.sendto(bytes(response), addr)


async def two_sessions_one_port() -> None:
    port = free_udp_port()
    mux = UdpMux(port)
    await mux.open([LOOPBACK])
    check("open binds the port on the address", mux.listen_addresses == [(LOOPBACK, port)],
          mux.listen_addresses)
    servers = [Connection(ice_controlling=False, use_ipv6=False, udp_mux=mux) for _ in range(2)]
    clients = [Connection(ice_controlling=True, use_ipv6=False) for _ in range(2)]
    try:
        for conn in servers + clients:
            await gather_loopback(conn)
        hosts = [[c for c in s.local_candidates if c.type == "host"] for s in servers]
        check("each session offers one host candidate on the mux port",
              all(len(h) == 1 and h[0].port == port and h[0].transport == "udp" for h in hosts),
              [[c.to_sdp() for c in h] for h in hosts])
        check("both sessions are attached on the address", mux.attached(LOOPBACK) == 2, mux.attached())
        check("username fragments are long enough to key a shared socket",
              all(len(s.local_username) >= 8 for s in servers), [s.local_username for s in servers])
        for server, client in zip(servers, clients):
            await pair_up(server, client)
        await asyncio.wait_for(asyncio.gather(*(c.connect() for c in servers + clients)), 15)
        check("both sessions connect through the one socket at once", True)
        for i, (server, client) in enumerate(zip(servers, clients)):
            await server.send(b"server-%d" % i)
            await client.send(b"client-%d" % i)
        got = [await asyncio.wait_for(c.recv(), 3) for c in clients]
        check("each client receives its own server's datagram",
              got == [b"server-0", b"server-1"], got)
        got = [await asyncio.wait_for(s.recv(), 3) for s in servers]
        check("each server receives its own client's datagram",
              got == [b"client-0", b"client-1"], got)
        check("nothing crosses between the sessions",
              await nothing_arrives(servers[0]) and await nothing_arrives(clients[1]))

        # A stranger's datagrams: garbage and a check for a fragment nobody holds.
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as stranger:
            stranger.sendto(b"\x16\x00garbage", (LOOPBACK, port))
            request = stun.Message(message_method=stun.Method.BINDING, message_class=stun.Class.REQUEST)
            request.attributes["USERNAME"] = "nobodyhere:" + clients[0].local_username
            request.attributes["PRIORITY"] = 1
            request.add_message_integrity(b"wrongpassword")
            stranger.sendto(bytes(request), (LOOPBACK, port))
            stranger.settimeout(0.5)
            try:
                stranger.recvfrom(1500)
                answered = True
            except socket.timeout:
                answered = False
        check("a stranger's datagrams are dropped unanswered", not answered)
        await servers[0].send(b"still-there")
        check("and the sessions are undisturbed",
              await asyncio.wait_for(clients[0].recv(), 3) == b"still-there"
              and await nothing_arrives(servers[0]))

        await servers[0].close()
        await clients[0].close()
        check("closing one session detaches it alone", mux.attached(LOOPBACK) == 1, mux.attached())
        await servers[1].send(b"after-close")
        await clients[1].send(b"after-close-back")
        check("the remaining session keeps its socket",
              await asyncio.wait_for(clients[1].recv(), 3) == b"after-close"
              and await asyncio.wait_for(servers[1].recv(), 3) == b"after-close-back")
    finally:
        for conn in servers + clients:
            await conn.close()
        await mux.close()
    check("closing the mux detaches everything", mux.attached() == 0 and mux.listen_addresses == [])


async def srflx_through_the_mux() -> None:
    loop = asyncio.get_running_loop()
    transport, responder = await loop.create_datagram_endpoint(StunResponder, local_addr=(LOOPBACK, 0))
    stun_port = transport.get_extra_info("sockname")[1]
    port = free_udp_port()
    mux = UdpMux(port)
    await mux.open([LOOPBACK])
    conn = Connection(ice_controlling=False, use_ipv6=False, udp_mux=mux,
                      stun_server=(LOOPBACK, stun_port))
    try:
        candidates = await gather_loopback(conn)
        srflx = [c for c in candidates if c.type == "srflx"]
        check("the server-reflexive query rides the mux socket",
              responder.requests == 1 and len(srflx) == 1 and srflx[0].port == port
              and srflx[0].related_port == port,
              [c.to_sdp() for c in candidates])
        learned = list(mux._sockets[LOOPBACK].point.by_addr)
        check("the STUN server is not learned as a peer", (LOOPBACK, stun_port) not in learned, learned)
    finally:
        await conn.close()
        await mux.close()
        transport.close()


async def port_in_use_fails_open() -> None:
    blocker = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    blocker.bind((LOOPBACK, 0))
    port = blocker.getsockname()[1]
    mux = UdpMux(port)
    try:
        await mux.open([LOOPBACK])
        check("a port in use fails open", False, "no OSError")
    except OSError as exc:
        check("a port in use fails open", True, exc)
    finally:
        blocker.close()
    check("and leaves nothing bound", mux.listen_addresses == [], mux.listen_addresses)


async def late_address_nat1to1_and_port_range() -> None:
    port = free_udp_port()
    mux = UdpMux(port)
    await mux.open([LOOPBACK])
    conn = Connection(ice_controlling=False, use_ipv6=False, udp_mux=mux,
                      nat1to1_ips=["203.0.113.5"], port_range=(port + 1, port + 1))
    try:
        candidates = await gather_loopback(conn, (LOOPBACK, "127.0.0.2"))
        check("an address the mux was not opened on is bound on first use",
              sorted(mux.listen_addresses) == [(LOOPBACK, port), ("127.0.0.2", port)],
              mux.listen_addresses)
        check("every host candidate sits on the mux port, the port range yielding to it",
              [c.port for c in candidates] == [port, port], [c.to_sdp() for c in candidates])
        check("NAT1TO1 rewrites the muxed candidates' address",
              all(c.host == "203.0.113.5" for c in candidates), [c.host for c in candidates])
        check("local preferences are unique per address",
              len({c.priority for c in candidates}) == 2, [c.priority for c in candidates])
    finally:
        await conn.close()
        await mux.close()


async def main_async() -> None:
    await two_sessions_one_port()
    await srflx_through_the_mux()
    await port_in_use_fails_open()
    await late_address_nat1to1_and_port_range()


def main() -> int:
    asyncio.run(main_async())
    print(f"[ice-udp-mux] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
