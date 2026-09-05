#!/usr/bin/env python3
"""The TCP mux at the ICE layer: ICE-TCP passive candidates on one port.

A hand-rolled active peer plays the browser: it opens the TCP stream to the
passive candidate, frames STUN and data per RFC 4571, checks with
USE-CANDIDATE and answers the server's own check. Against that peer a full
server nominates over the stream and exchanges data both ways, an ICE-lite
server completes on the peer's nomination without a check of its own, the
TCP candidate ranks below the UDP one, a stream whose first frame is not a
STUN check or names no session is closed, a silent stream is closed at the
first-frame deadline, and losing the nominated stream ends the connection at
once. Runs against the vendored `selkies.ice` alone.
"""
import asyncio
import os
import socket
import struct
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "src",
))

from selkies.ice import Connection, TcpMux  # noqa: E402
from selkies.ice import mux as ice_mux  # noqa: E402
from selkies.ice import stun  # noqa: E402
from selkies.ice.candidate import Candidate, candidate_priority  # noqa: E402

passed = failed = 0
LOOPBACK = "127.0.0.1"
FRAME = struct.Struct("!H")


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [ice-tcp-mux] {label}  {detail}", flush=True)


def free_tcp_port() -> int:
    with socket.socket() as probe:
        probe.bind((LOOPBACK, 0))
        return probe.getsockname()[1]


async def gather_loopback(conn: Connection) -> list:
    candidates = await conn.get_component_candidates(1, [LOOPBACK])
    conn._local_candidates += candidates
    conn._local_candidates_end = True
    return candidates


def active_candidate() -> Candidate:
    """The active TCP candidate a browser offers: port 9, never connected to."""
    return Candidate(foundation="1", component=1, transport="tcp",
                     priority=candidate_priority(1, "host", 24575), host=LOOPBACK,
                     port=9, type="host", tcptype="active")


class ActivePeer:
    """The browser's side of ICE-TCP: one stream to the passive candidate."""

    def __init__(self, server: Connection) -> None:
        self.ufrag = "clientfrag"
        self.password = "clientpasswordclientpassword"
        self.server = server
        self.reader = None
        self.writer = None
        self.remote_addr = None
        self.requests_seen = 0
        self.data = asyncio.Queue()
        self.responses = {}
        self.reader_task = None
        self.closed = asyncio.Event()

    async def open(self, port: int) -> None:
        self.reader, self.writer = await asyncio.open_connection(LOOPBACK, port)
        self.remote_addr = self.writer.get_extra_info("peername")[:2]
        self.reader_task = asyncio.create_task(self._read_frames())

    async def send_frame(self, data: bytes) -> None:
        self.writer.write(FRAME.pack(len(data)) + data)
        await self.writer.drain()

    async def _read_frames(self) -> None:
        try:
            while True:
                header = await self.reader.readexactly(2)
                (length,) = FRAME.unpack(header)
                frame = await self.reader.readexactly(length)
                if ice_mux.is_stun(frame):
                    message = stun.parse_message(frame)
                    if message.message_class == stun.Class.REQUEST:
                        self.requests_seen += 1
                        await self.send_frame(bytes(self._answer(message)))
                    else:
                        waiter = self.responses.get(message.transaction_id)
                        if waiter is not None and not waiter.done():
                            waiter.set_result(message)
                else:
                    self.data.put_nowait(frame)
        except (asyncio.IncompleteReadError, ConnectionError, asyncio.CancelledError):
            pass
        finally:
            self.closed.set()

    def _answer(self, request: stun.Message) -> stun.Message:
        response = stun.Message(message_method=stun.Method.BINDING,
                                message_class=stun.Class.RESPONSE,
                                transaction_id=request.transaction_id)
        response.attributes["XOR-MAPPED-ADDRESS"] = self.remote_addr
        response.add_message_integrity(self.password.encode())
        return response

    async def binding_check(self, nominate: bool = True, timeout: float = 5,
                            controlling: bool = True) -> stun.Message:
        request = stun.Message(message_method=stun.Method.BINDING, message_class=stun.Class.REQUEST)
        request.attributes["USERNAME"] = f"{self.server.local_username}:{self.ufrag}"
        request.attributes["PRIORITY"] = candidate_priority(1, "prflx")
        request.attributes["ICE-CONTROLLING" if controlling else "ICE-CONTROLLED"] = 12345
        if nominate:
            request.attributes["USE-CANDIDATE"] = None
        request.add_message_integrity(self.server.local_password.encode())
        waiter = asyncio.get_running_loop().create_future()
        self.responses[request.transaction_id] = waiter
        await self.send_frame(bytes(request))
        return await asyncio.wait_for(waiter, timeout)

    async def close(self) -> None:
        if self.writer is not None:
            self.writer.close()
        if self.reader_task is not None:
            self.reader_task.cancel()
            try:
                await self.reader_task
            except asyncio.CancelledError:
                pass


async def connected_over_tcp(server: Connection, port: int, label: str) -> ActivePeer:
    """Bring `server` and a fresh active peer to a nominated pair over the mux port."""
    await server.add_remote_candidate(active_candidate())
    await server.add_remote_candidate(None)
    peer = ActivePeer(server)
    server.remote_username, server.remote_password = peer.ufrag, peer.password
    connect = asyncio.create_task(server.connect())
    await asyncio.sleep(0.2)
    check(f"{label}: the passive side waits, it never dials the active candidate",
          not connect.done())
    await peer.open(port)
    response = await peer.binding_check(nominate=True)
    check(f"{label}: the check over the stream is answered with the peer's address",
          response.message_class == stun.Class.RESPONSE
          and response.attributes.get("XOR-MAPPED-ADDRESS") == peer.writer.get_extra_info("sockname")[:2],
          response)
    await asyncio.wait_for(connect, 10)
    check(f"{label}: ICE completes on the stream", True)
    return peer


async def full_server_over_tcp(port: int, mux: TcpMux) -> None:
    server = Connection(ice_controlling=False, use_ipv6=False, tcp_mux=mux)
    peer = None
    try:
        candidates = await gather_loopback(server)
        tcp = [c for c in candidates if c.transport == "tcp"]
        udp = [c for c in candidates if c.transport == "udp"]
        check("a passive TCP host candidate is offered on the mux port",
              len(tcp) == 1 and tcp[0].port == port and tcp[0].tcptype == "passive" and tcp[0].type == "host",
              [c.to_sdp() for c in candidates])
        check("it ranks below the UDP host candidate", udp and tcp[0].priority < udp[0].priority,
              [c.priority for c in candidates])
        check("the SDP form names the direction", "tcptype passive" in tcp[0].to_sdp(), tcp[0].to_sdp())
        peer = await connected_over_tcp(server, port, "full agent")
        check("the full agent checked back over the stream", peer.requests_seen >= 1, peer.requests_seen)
        await server.send(b"from-server")
        check("data reaches the peer as one frame",
              await asyncio.wait_for(peer.data.get(), 3) == b"from-server")
        await peer.send_frame(b"from-peer")
        check("and a frame from the peer is received", await asyncio.wait_for(server.recv(), 3) == b"from-peer")
        big = bytes(range(256)) * 5
        await server.send(big)
        check("a frame larger than one TCP segment arrives whole",
              await asyncio.wait_for(peer.data.get(), 3) == big)
        # The peer drops the stream: the connection ends with it.
        peer.writer.close()
        try:
            await asyncio.wait_for(server.recv(), 3)
            check("losing the nominated stream ends the connection at once", False, "recv returned")
        except ConnectionError:
            check("losing the nominated stream ends the connection at once", True)
        except asyncio.TimeoutError:
            check("losing the nominated stream ends the connection at once", False, "still open after 3s")
    finally:
        if peer is not None:
            await peer.close()
        await server.close()


async def lite_server_over_tcp(port: int, mux: TcpMux) -> None:
    server = Connection(ice_controlling=False, use_ipv6=False, tcp_mux=mux, ice_lite=True)
    peer = None
    try:
        await gather_loopback(server)
        peer = await connected_over_tcp(server, port, "ICE-lite")
        await asyncio.sleep(0.5)
        check("the ICE-lite agent sent no check of its own", peer.requests_seen == 0, peer.requests_seen)
        await server.send(b"lite-data")
        check("data flows over the peer-nominated stream",
              await asyncio.wait_for(peer.data.get(), 3) == b"lite-data")
    finally:
        if peer is not None:
            await peer.close()
        await server.close()


async def udp_keeps_the_session(port: int, mux: TcpMux) -> None:
    """A controlling server whose UDP pair is up stays on it when the same
    peer's TCP stream succeeds later."""
    server = Connection(ice_controlling=True, use_ipv6=False, tcp_mux=mux)
    client = Connection(ice_controlling=False, use_ipv6=False)
    peer = None
    try:
        await gather_loopback(server)
        await gather_loopback(client)
        for candidate in client.local_candidates:
            await server.add_remote_candidate(candidate)
        await server.add_remote_candidate(active_candidate())
        await server.add_remote_candidate(None)
        for candidate in server.local_candidates:
            if candidate.transport == "udp":
                await client.add_remote_candidate(candidate)
        await client.add_remote_candidate(None)
        server.remote_username, server.remote_password = client.local_username, client.local_password
        client.remote_username, client.remote_password = server.local_username, server.local_password
        await asyncio.wait_for(asyncio.gather(server.connect(), client.connect()), 10)
        check("the UDP pair is selected first", server._nominated[1].local_candidate.transport == "udp")
        # The same peer now completes its TCP stream; the server nominates it
        # too, as it nominates every success, but keeps the better pair.
        peer = ActivePeer(server)
        peer.ufrag, peer.password = client.local_username, client.local_password
        await peer.open(port)
        await peer.binding_check(nominate=False, controlling=False)
        await asyncio.sleep(1.0)
        tcp_pairs = [p for p in server._check_list if p.local_candidate.transport == "tcp"]
        check("the TCP pair succeeded and was nominated as well",
              tcp_pairs and tcp_pairs[0].state.name == "SUCCEEDED" and tcp_pairs[0].nominated,
              [(p.state.name, p.nominated) for p in tcp_pairs])
        check("the session stays on UDP", server._nominated[1].local_candidate.transport == "udp",
              server._nominated[1])
        await server.send(b"still-udp")
        check("data still takes the UDP pair", await asyncio.wait_for(client.recv(), 3) == b"still-udp")
    finally:
        if peer is not None:
            await peer.close()
        await server.close()
        await client.close()


async def strangers_are_closed(port: int, mux: TcpMux) -> None:
    server = Connection(ice_controlling=False, use_ipv6=False, tcp_mux=mux)
    try:
        await gather_loopback(server)
        async def first_frame_closes(payload: bytes) -> bool:
            reader, writer = await asyncio.open_connection(LOOPBACK, port)
            writer.write(FRAME.pack(len(payload)) + payload)
            await writer.drain()
            try:
                rest = await asyncio.wait_for(reader.read(1), 3)
            except asyncio.TimeoutError:
                rest = None
            writer.close()
            return rest == b""
        check("a first frame that is not STUN gets the stream closed",
              await first_frame_closes(b"\x16\x03\x01hello"))
        request = stun.Message(message_method=stun.Method.BINDING, message_class=stun.Class.REQUEST)
        request.attributes["USERNAME"] = "nobody:clientfrag"
        request.attributes["PRIORITY"] = 1
        request.add_message_integrity(b"whatever")
        check("a check naming no session gets the stream closed",
              await first_frame_closes(bytes(request)))
        ice_mux.FIRST_FRAME_TIMEOUT = 0.5
        reader, writer = await asyncio.open_connection(LOOPBACK, port)
        try:
            rest = await asyncio.wait_for(reader.read(1), 3)
        except asyncio.TimeoutError:
            rest = None
        writer.close()
        check("a silent stream is closed at the first-frame deadline", rest == b"", rest)
    finally:
        await server.close()


async def main_async() -> None:
    port = free_tcp_port()
    mux = TcpMux(port)
    await mux.open([LOOPBACK])
    check("open listens on the address", mux.listen_addresses == [(LOOPBACK, port)], mux.listen_addresses)
    try:
        await full_server_over_tcp(port, mux)
        check("closing the session detaches it", mux.attached() == 0, mux.attached())
        await lite_server_over_tcp(port, mux)
        await udp_keeps_the_session(port, mux)
        await strangers_are_closed(port, mux)
    finally:
        await mux.close()
    blocker = socket.socket()
    blocker.bind((LOOPBACK, 0))
    blocker.listen(1)
    taken = TcpMux(blocker.getsockname()[1])
    try:
        await taken.open([LOOPBACK])
        check("a port in use fails open", False, "no OSError")
    except OSError as exc:
        check("a port in use fails open", True, exc)
    finally:
        blocker.close()


def main() -> int:
    asyncio.run(main_async())
    print(f"[ice-tcp-mux] {passed} passed, {failed} failed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
