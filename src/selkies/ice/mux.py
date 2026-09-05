# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Shared ICE sockets: one UDP port and one TCP port for every session.

A deployment that can expose only a port or two forwards one UDP port and one
TCP port and has every session's host candidates share them, instead of an
ephemeral port (or a `webrtc_port_range` window) per session. A `UdpMux`
binds the port on each host address, a `TcpMux` listens on it, and each ICE
`Connection` attaches one protocol per address under its local username
fragment; the protocol sees a datagram transport and never learns that the
socket is shared.

Demultiplexing follows the ICE handshake itself. A datagram from a remote
address already known belongs to the connection it was learned for. One from
an unknown address is only ever STUN: a request names its connection in the
USERNAME (`local:remote` fragments); a response answers a transaction that an
attached protocol registered when it sent the request, a connectivity check or
a server-reflexive query. A peer's address is learned from its request or from
its answer to a check, never from a STUN server's answer, so a STUN server
shared by every session maps to none of them. Anything else is dropped.

ICE-TCP (RFC 6544) rides the TCP port. Each accepted stream carries RFC 4571
frames, a 16-bit length ahead of every packet, and its first frame must be the
peer's STUN binding request, whose USERNAME hands the stream to the connection
it belongs to; from then on the stream is that connection's datagram path to
the peer's address, and losing it while it carries the nominated pair ends the
connection at once rather than after consent expires. A stream that sends
anything else first, or nothing within `FIRST_FRAME_TIMEOUT`, is closed.
Writes are bounded: once the socket's send buffer backs up past asyncio's
high-water mark, packets are dropped the way a full UDP socket drops them, so
a slow link costs frames rather than memory, and the media layers recover from
loss as they do on UDP.
"""
import asyncio
import logging
import socket
import struct
from typing import Any, Callable, Optional

from . import stun
from .turn import UDP_SOCKET_BUFFER_SIZE

logger = logging.getLogger(__name__)

# Seconds an accepted TCP stream has to send its STUN binding request.
FIRST_FRAME_TIMEOUT = 30.0
# Send backlog, per stream, past which packets are dropped instead of queued.
TCP_WRITE_HIGH_WATER = 1 << 20
_FRAME_HEADER = struct.Struct("!H")
_MAX_FRAME = 0xFFFF
_STUN_COOKIE = struct.pack("!I", stun.COOKIE)


def is_stun(data: bytes) -> bool:
    """Whether `data` starts like a STUN message: zero top bits and the magic cookie."""
    return (
        len(data) >= stun.HEADER_LENGTH
        and data[0] & 0xC0 == 0
        and data[4:8] == _STUN_COOKIE
    )


def username_fragment(message: stun.Message) -> Optional[str]:
    """The local username fragment an ICE check's USERNAME (`local:remote`) names."""
    username = message.attributes.get("USERNAME")
    if not isinstance(username, str) or not username:
        return None
    return username.split(":", 1)[0]


class _MuxPoint:
    """Bookkeeping of one shared socket or listener on one local address.

    Attached protocols are keyed by their local username fragment, learned
    peers by remote address, and STUN transactions in flight by transaction id
    together with whether their answer identifies a peer.
    """

    def __init__(self, address: str, port: int) -> None:
        self.address = address
        self.port = port
        self.by_ufrag: dict[str, Any] = {}
        self.by_addr: dict[tuple, Any] = {}
        self.by_tid: dict[bytes, tuple] = {}

    @property
    def sockname(self) -> tuple:
        return (self.address, self.port)

    def attach(self, ufrag: str, protocol: Any) -> None:
        if ufrag in self.by_ufrag:
            raise ValueError(
                f"username fragment {ufrag!r} is already attached on {self.sockname}"
            )
        self.by_ufrag[ufrag] = protocol

    def detach(self, ufrag: str, protocol: Any) -> None:
        if self.by_ufrag.get(ufrag) is protocol:
            del self.by_ufrag[ufrag]
        for addr in [a for a, p in self.by_addr.items() if p is protocol]:
            del self.by_addr[addr]
        for tid in [t for t, (p, _) in self.by_tid.items() if p is protocol]:
            del self.by_tid[tid]

    def learn(self, addr: tuple, protocol: Any) -> None:
        if self.by_addr.get(addr) is not protocol:
            self.by_addr[addr] = protocol
            logger.debug("%s learned peer %s for ufrag of %r", self.sockname, addr, protocol)

    def route(self, data: bytes, addr: tuple) -> Any:
        """The attached protocol a datagram from `addr` belongs to, or None.

        A learned peer's datagrams are routed without a look inside; only STUN
        from an unknown address is parsed, to find the connection it names.
        """
        protocol = self.by_addr.get(addr)
        if protocol is not None or not is_stun(data):
            return protocol
        try:
            message = stun.parse_message(data)
        except ValueError:
            return None
        if message.message_class in (stun.Class.RESPONSE, stun.Class.ERROR):
            pending = self.by_tid.get(message.transaction_id)
            if pending is not None:
                protocol, names_peer = pending
                if names_peer:
                    self.learn(addr, protocol)
        elif message.message_class == stun.Class.REQUEST:
            owner = self.by_ufrag.get(username_fragment(message) or "")
            if owner is not None:
                protocol = owner
                self.learn(addr, owner)
        return protocol


class _MuxTransport:
    """The datagram-transport face one attached protocol sends and closes through."""

    def __init__(self, point: _MuxPoint, ufrag: str) -> None:
        self._point = point
        self._ufrag = ufrag
        self.protocol: Any = None
        self._closed = False

    def get_extra_info(self, name: str, default: Any = None) -> Any:
        if name == "sockname":
            return self._point.sockname
        return default

    def expect_response(self, transaction_id: bytes, names_peer: bool) -> None:
        """Route the answer to a STUN request this protocol sends back to it.

        `names_peer` says the request is an ICE check, so the answer's source
        is the peer and is learned; a STUN server's answer is not.
        """
        self._point.by_tid[transaction_id] = (self.protocol, names_peer)

    def forget_response(self, transaction_id: bytes) -> None:
        self._point.by_tid.pop(transaction_id, None)

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        self._point.detach(self._ufrag, self.protocol)
        self._release()
        self.protocol.connection_lost(None)

    def _release(self) -> None:
        pass


class _UdpMuxTransport(_MuxTransport):
    def __init__(self, point: _MuxPoint, ufrag: str, socket_transport: Any) -> None:
        super().__init__(point, ufrag)
        self._socket_transport = socket_transport

    def get_extra_info(self, name: str, default: Any = None) -> Any:
        if name == "sockname":
            return self._point.sockname
        return self._socket_transport.get_extra_info(name, default)

    def sendto(self, data: bytes, addr: tuple) -> None:
        self._socket_transport.sendto(data, addr)


class _UdpMuxSocket(asyncio.DatagramProtocol):
    """One shared UDP socket, bound to one local address on the mux port."""

    def __init__(self, point: _MuxPoint) -> None:
        self.point = point
        self.transport: Any = None
        self.closed: Optional[asyncio.Future] = None

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        self.transport = transport
        self.closed = asyncio.get_running_loop().create_future()

    def datagram_received(self, data: bytes, addr: tuple) -> None:
        if not data:
            return
        addr = (addr[0], addr[1])
        protocol = self.point.route(data, addr)
        if protocol is None:
            logger.debug("%s dropped %d bytes from unknown peer %s", self.point.sockname, len(data), addr)
            return
        protocol.datagram_received(data, addr)

    def error_received(self, exc: Exception) -> None:
        logger.debug("%s error_received(%s)", self.point.sockname, exc)

    def connection_lost(self, exc: Optional[Exception]) -> None:
        for protocol in list(self.point.by_ufrag.values()):
            protocol.transport.close()
        if self.closed is not None and not self.closed.done():
            self.closed.set_result(None)


class UdpMux:
    """Every session's UDP host candidates on one port, one socket per local address.

    `open` binds the port on the addresses given, so a port in use fails the
    service at startup rather than the first session; `attach` binds lazily for
    an address that appeared later.
    """

    def __init__(self, port: int) -> None:
        self.port = port
        self._sockets: dict[str, _UdpMuxSocket] = {}
        self._closed = False

    @property
    def listen_addresses(self) -> list:
        return [sock.point.sockname for sock in self._sockets.values()]

    def attached(self, address: Optional[str] = None) -> int:
        """Protocols attached on `address`, or on every address."""
        sockets = self._sockets.values() if address is None else [self._sockets[address]]
        return sum(len(sock.point.by_ufrag) for sock in sockets)

    async def open(self, addresses: list) -> None:
        """Bind the port on each address; on failure nothing stays bound.

        Raises:
            OSError: A bind failed, typically because the port is in use.
        """
        try:
            for address in addresses:
                await self._bind(address)
        except OSError:
            await self.close()
            raise

    async def _bind(self, address: str) -> _UdpMuxSocket:
        if self._closed:
            raise OSError("UDP mux is closed")
        sock = self._sockets.get(address)
        if sock is not None:
            return sock
        loop = asyncio.get_running_loop()
        point = _MuxPoint(address, self.port)
        transport, sock = await loop.create_datagram_endpoint(
            lambda: _UdpMuxSocket(point), local_addr=(address, self.port)
        )
        raw = transport.get_extra_info("socket")
        if raw is not None:
            raw.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, UDP_SOCKET_BUFFER_SIZE)
        self._sockets[address] = sock
        return sock

    async def attach(self, address: str, ufrag: str, protocol_factory: Callable[[], Any]) -> Any:
        """A protocol of a connection with local fragment `ufrag` on `address`.

        Raises:
            OSError: The address could not be bound on the mux port.
            ValueError: The fragment is already attached on that address.
        """
        sock = await self._bind(address)
        transport = _UdpMuxTransport(sock.point, ufrag, sock.transport)
        protocol = protocol_factory()
        transport.protocol = protocol
        sock.point.attach(ufrag, protocol)
        protocol.connection_made(transport)
        return protocol

    async def close(self) -> None:
        """Detach every protocol and release the sockets, which are free to rebind on return."""
        self._closed = True
        sockets = list(self._sockets.values())
        self._sockets.clear()
        for sock in sockets:
            if sock.transport is not None:
                sock.transport.close()
        for sock in sockets:
            if sock.closed is not None:
                await sock.closed


class _TcpMuxTransport(_MuxTransport):
    """A connection's face of the TCP listener: its accepted streams by peer address."""

    def __init__(self, point: _MuxPoint, ufrag: str) -> None:
        super().__init__(point, ufrag)
        self._streams: dict[tuple, "_TcpStream"] = {}

    def sendto(self, data: bytes, addr: tuple) -> None:
        stream = self._streams.get(addr)
        if stream is None:
            logger.debug("%s no stream to %s, dropping %d bytes", self._point.sockname, addr, len(data))
            return
        stream.send(data)

    def add_stream(self, stream: "_TcpStream") -> None:
        previous = self._streams.get(stream.remote_addr)
        self._streams[stream.remote_addr] = stream
        if previous is not None and previous is not stream:
            previous.close()

    def remove_stream(self, stream: "_TcpStream") -> None:
        if self._streams.get(stream.remote_addr) is stream:
            del self._streams[stream.remote_addr]
            if not self._closed:
                self.protocol.receiver.peer_stream_lost(stream.remote_addr)

    def _release(self) -> None:
        for stream in list(self._streams.values()):
            stream.close()
        self._streams.clear()


class _TcpStream(asyncio.BufferedProtocol):
    """One accepted ICE-TCP stream: RFC 4571 frames in, bounded frames out.

    The receive buffer is handed to asyncio directly, so a packet is copied
    once, out of the buffer into the frame the connection receives.
    """

    def __init__(self, listener: "_TcpMuxListener") -> None:
        self._listener = listener
        self._owner: Optional[_TcpMuxTransport] = None
        self._buffer = bytearray(2 * (_FRAME_HEADER.size + _MAX_FRAME))
        self._view = memoryview(self._buffer)
        self._start = 0
        self._end = 0
        self._paused = False
        self._first_timer: Optional[asyncio.TimerHandle] = None
        self.transport: Any = None
        self.remote_addr: tuple = ("", 0)
        self.dropped = 0

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        self.transport = transport
        peer = transport.get_extra_info("peername") or ("", 0)
        self.remote_addr = (peer[0], peer[1])
        transport.set_write_buffer_limits(high=TCP_WRITE_HIGH_WATER)
        self._first_timer = asyncio.get_running_loop().call_later(
            FIRST_FRAME_TIMEOUT, self._first_frame_timeout
        )

    def _first_frame_timeout(self) -> None:
        if self._owner is None:
            logger.info("%s closing %s: no STUN binding request within %.0fs",
                        self._listener.point.sockname, self.remote_addr, FIRST_FRAME_TIMEOUT)
            self.close()

    def get_buffer(self, sizehint: int) -> memoryview:
        if self._end == len(self._buffer):
            self._view[: self._end - self._start] = self._view[self._start : self._end]
            self._end -= self._start
            self._start = 0
        return self._view[self._end :]

    def buffer_updated(self, nbytes: int) -> None:
        self._end += nbytes
        while True:
            pending = self._end - self._start
            if pending < _FRAME_HEADER.size:
                break
            (length,) = _FRAME_HEADER.unpack_from(self._buffer, self._start)
            if pending < _FRAME_HEADER.size + length:
                break
            begin = self._start + _FRAME_HEADER.size
            frame = bytes(self._view[begin : begin + length])
            self._start = begin + length
            if length and not self._frame(frame):
                return
        if self._start == self._end:
            self._start = self._end = 0

    def _frame(self, data: bytes) -> bool:
        """Deliver one frame; the first must be a STUN request naming a connection."""
        if self._owner is None:
            owner = self._claim(data)
            if owner is None:
                self.close()
                return False
            self._owner = owner
            if self._first_timer is not None:
                self._first_timer.cancel()
            owner.add_stream(self)
        self._owner.protocol.datagram_received(data, self.remote_addr)
        return True

    def _claim(self, data: bytes) -> Optional[_TcpMuxTransport]:
        point = self._listener.point
        if not is_stun(data):
            logger.info("%s closing %s: first frame is not STUN", point.sockname, self.remote_addr)
            return None
        try:
            message = stun.parse_message(data)
        except ValueError as exc:
            logger.info("%s closing %s: malformed STUN (%s)", point.sockname, self.remote_addr, exc)
            return None
        ufrag = username_fragment(message)
        owner = point.by_ufrag.get(ufrag or "") if message.message_class == stun.Class.REQUEST else None
        if owner is None:
            logger.info("%s closing %s: no connection for username fragment %r",
                        point.sockname, self.remote_addr, ufrag)
            return None
        return owner.transport

    def pause_writing(self) -> None:
        self._paused = True

    def resume_writing(self) -> None:
        self._paused = False

    def send(self, data: bytes) -> None:
        if self.transport is None or self.transport.is_closing():
            return
        if self._paused:
            self.dropped += 1
            return
        self.transport.writelines((_FRAME_HEADER.pack(len(data)), data))

    def close(self) -> None:
        if self.transport is not None and not self.transport.is_closing():
            self.transport.close()

    def connection_lost(self, exc: Optional[Exception]) -> None:
        if self._first_timer is not None:
            self._first_timer.cancel()
        if self._owner is not None:
            owner, self._owner = self._owner, None
            owner.remove_stream(self)
        if self.dropped:
            logger.info("%s stream from %s dropped %d packets to a backed-up link",
                        self._listener.point.sockname, self.remote_addr, self.dropped)


class _TcpMuxListener:
    """One listening socket on the mux port, bound to one local address."""

    def __init__(self, point: _MuxPoint) -> None:
        self.point = point
        self.server: Any = None


class TcpMux:
    """Every session's passive TCP host candidates on one port, one listener per address."""

    def __init__(self, port: int) -> None:
        self.port = port
        self._listeners: dict[str, _TcpMuxListener] = {}
        self._closed = False

    @property
    def listen_addresses(self) -> list:
        return [listener.point.sockname for listener in self._listeners.values()]

    def attached(self, address: Optional[str] = None) -> int:
        """Protocols attached on `address`, or on every address."""
        listeners = self._listeners.values() if address is None else [self._listeners[address]]
        return sum(len(listener.point.by_ufrag) for listener in listeners)

    async def open(self, addresses: list) -> None:
        """Listen on each address; on failure nothing stays bound.

        Raises:
            OSError: A bind failed, typically because the port is in use.
        """
        try:
            for address in addresses:
                await self._listen(address)
        except OSError:
            await self.close()
            raise

    async def _listen(self, address: str) -> _TcpMuxListener:
        if self._closed:
            raise OSError("TCP mux is closed")
        listener = self._listeners.get(address)
        if listener is not None:
            return listener
        loop = asyncio.get_running_loop()
        listener = _TcpMuxListener(_MuxPoint(address, self.port))
        listener.server = await loop.create_server(
            lambda: _TcpStream(listener), host=address, port=self.port, reuse_address=True
        )
        self._listeners[address] = listener
        return listener

    async def attach(self, address: str, ufrag: str, protocol_factory: Callable[[], Any]) -> Any:
        """A protocol of a connection with local fragment `ufrag` on `address`.

        Raises:
            OSError: The address could not be bound on the mux port.
            ValueError: The fragment is already attached on that address.
        """
        listener = await self._listen(address)
        transport = _TcpMuxTransport(listener.point, ufrag)
        protocol = protocol_factory()
        transport.protocol = protocol
        listener.point.attach(ufrag, protocol)
        protocol.connection_made(transport)
        return protocol

    async def close(self) -> None:
        """Detach every protocol, end their streams and stop listening; the port is free on return."""
        self._closed = True
        listeners = list(self._listeners.values())
        self._listeners.clear()
        for listener in listeners:
            for protocol in list(listener.point.by_ufrag.values()):
                protocol.transport.close()
            if listener.server is not None:
                listener.server.close()
