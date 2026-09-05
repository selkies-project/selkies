# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Copyright (c) 2018-2019 Jeremy Lainé.
#   All rights reserved.
#
#   Redistribution and use in source and binary forms, with or without
#   modification, are permitted provided that the following conditions are met:
#
#       * Redistributions of source code must retain the above copyright notice,
#       this list of conditions and the following disclaimer.
#       * Redistributions in binary form must reproduce the above copyright notice,
#       this list of conditions and the following disclaimer in the documentation
#       and/or other materials provided with the distribution.
#       * Neither the name of aioice nor the names of its contributors may
#       be used to endorse or promote products derived from this software without
#       specific prior written permission.
#
#   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
#   ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
#   WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
#   DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
#   FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
#   DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
#   SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
#   CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
#   OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
#   OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

import asyncio
import copy
import enum
import errno
import ipaddress
import itertools
import logging
import random
import re
import secrets
import socket
import threading
from collections.abc import Callable
from typing import Optional, Union, cast

import ifaddr

from . import mdns, mux, stun, turn
from .candidate import Candidate, candidate_foundation, candidate_priority
from .utils import random_string

logger = logging.getLogger(__name__)

ICE_COMPLETED = 1
ICE_FAILED = 2

CONSENT_FAILURES = 6
CONSENT_INTERVAL = 5
# Seconds a connection waits for a path only the peer can open (its nomination
# of an ICE-lite agent, its connection to a passive TCP candidate).
INBOUND_CHECK_TIMEOUT = 30

connection_id = itertools.count()
protocol_id = itertools.count()

_mdns = threading.local()


class TransportPolicy(enum.Enum):
    ALL = 0
    """
    All ICE candidates will be considered.
    """

    RELAY = 1
    """
    Only ICE candidates whose IP addresses are being relayed,
    such as those being passed through a STUN or TURN server,
    will be considered.
    """


async def get_or_create_mdns_protocol(subscriber: object) -> mdns.MDnsProtocol:
    if not hasattr(_mdns, "lock"):
        _mdns.lock = asyncio.Lock()
        _mdns.protocol = None
        _mdns.subscribers = set()
    async with _mdns.lock:
        if _mdns.protocol is None:
            _mdns.protocol = await mdns.create_mdns_protocol()
        _mdns.subscribers.add(subscriber)
    return _mdns.protocol


async def unref_mdns_protocol(subscriber: object) -> None:
    if hasattr(_mdns, "lock"):
        async with _mdns.lock:
            _mdns.subscribers.discard(subscriber)
            if _mdns.protocol and not _mdns.subscribers:
                await _mdns.protocol.close()
                _mdns.protocol = None


def candidate_pair_priority(
    local: Candidate, remote: Candidate, ice_controlling: bool
) -> int:
    """
    See RFC 5245 - 5.7.2. Computing Pair Priority and Ordering Pairs
    """
    G = ice_controlling and local.priority or remote.priority
    D = ice_controlling and remote.priority or local.priority
    return (1 << 32) * min(G, D) + 2 * max(G, D) + (G > D and 1 or 0)


def get_host_addresses(use_ipv4: bool, use_ipv6: bool) -> list[str]:
    """
    Get local IP addresses.
    """
    addresses = []
    for adapter in ifaddr.get_adapters():
        for ip in adapter.ips:
            if isinstance(ip.ip, str) and use_ipv4 and ip.ip != "127.0.0.1":
                addresses.append(ip.ip)
            elif use_ipv6 and ip.ip[0] != "::1" and ip.ip[2] == 0:
                addresses.append(ip.ip[0])
    return addresses


async def relayed_candidate(
    component: int,
    protocol_factory: Callable[[], "StunProtocol"],
    turn_server: tuple[str, int],
    turn_username: Optional[str],
    turn_password: Optional[str],
    turn_ssl: bool,
    turn_transport: str,
) -> tuple[Candidate, "StunProtocol"]:
    """
    Connect to a TURN server to obtain a relayed candidate.
    """
    # Connect to TURN server.
    _, protocol = await turn.create_turn_endpoint(
        protocol_factory,
        server_addr=turn_server,
        username=turn_username,
        password=turn_password,
        ssl=turn_ssl,
        transport=turn_transport,
    )

    # Build relayed candidate.
    candidate_address = protocol.transport.get_extra_info("sockname")
    related_address = protocol.transport.get_extra_info("related_address")
    protocol.local_candidate = Candidate(
        foundation=candidate_foundation("relay", "udp", candidate_address[0]),
        component=component,
        transport="udp",
        priority=candidate_priority(component, "relay"),
        host=candidate_address[0],
        port=candidate_address[1],
        type="relay",
        related_address=related_address[0],
        related_port=related_address[1],
    )
    return protocol.local_candidate, protocol


async def server_reflexive_candidate(
    protocol: "StunProtocol", stun_server: tuple[str, int]
) -> tuple[Candidate, None]:
    """
    Query STUN server to obtain a server-reflexive candidate.
    """
    # lookup address
    loop = asyncio.get_running_loop()
    stun_server = (
        await loop.run_in_executor(None, socket.gethostbyname, stun_server[0]),
        stun_server[1],
    )

    # perform STUN query
    request = stun.Message(
        message_method=stun.Method.BINDING, message_class=stun.Class.REQUEST
    )
    response, _ = await protocol.request(request, stun_server)

    local_candidate = protocol.local_candidate
    return Candidate(
        foundation=candidate_foundation("srflx", "udp", local_candidate.host),
        component=local_candidate.component,
        transport=local_candidate.transport,
        priority=candidate_priority(local_candidate.component, "srflx"),
        host=response.attributes["XOR-MAPPED-ADDRESS"][0],
        port=response.attributes["XOR-MAPPED-ADDRESS"][1],
        type="srflx",
        related_address=local_candidate.host,
        related_port=local_candidate.port,
    ), None


def host_local_preference(index: int, tcptype: Optional[str] = None) -> int:
    """Local preference of the host candidate on the `index`-th address.

    Unique per address as RFC 8445 5.1.2.1 requires, counting down from the
    single-address value; a TCP candidate takes the RFC 6544 4.2 form, its
    direction preference (passive 4, active 6) below every UDP candidate.
    """
    if tcptype is None:
        return 65535 - index
    direction = {"active": 6, "passive": 4, "so": 2}[tcptype]
    return (1 << 13) * direction + (8191 - index)


def sort_candidate_pairs(pairs: list["CandidatePair"], ice_controlling: bool) -> None:
    """
    Sort a list of candidate pairs.
    """

    def pair_priority(pair: CandidatePair) -> int:
        return -candidate_pair_priority(
            pair.local_candidate, pair.remote_candidate, ice_controlling
        )

    pairs.sort(key=pair_priority)


def validate_password(value: str) -> None:
    """
    Check the password is well-formed.

    See RFC 5245 - 15.4. "ice-ufrag" and "ice-pwd" Attributes
    """
    if not re.match("^[a-zA-Z0-9+/]{22,256}$", value):
        raise ValueError("Password must satisfy 22*256ice-char")


def validate_remote_candidate(candidate: Candidate) -> Candidate:
    """
    Check the remote candidate is supported.
    """
    if candidate.type not in ["host", "relay", "srflx"]:
        raise ValueError('Unexpected candidate type "%s"' % candidate.type)
    ipaddress.ip_address(candidate.host)
    return candidate


def validate_username(value: str) -> None:
    """
    Check the username is well-formed.

    See RFC 5245 - 15.4. "ice-ufrag" and "ice-pwd" Attributes
    """
    if not re.match("^[a-zA-Z0-9+/]{4,256}$", value):
        raise ValueError("Username must satisfy 4*256ice-char")


class CandidatePair:
    def __init__(self, protocol: "StunProtocol", remote_candidate: Candidate) -> None:
        self.task: Optional[asyncio.Task] = None
        self.nominated = False
        self.protocol = protocol
        self.remote_candidate = remote_candidate
        self.remote_nominated = False
        self.state = CandidatePair.State.FROZEN

    def __repr__(self) -> str:
        return "CandidatePair(%s -> %s)" % (self.local_addr, self.remote_addr)

    @property
    def component(self) -> int:
        return self.local_candidate.component

    @property
    def local_addr(self) -> tuple[str, int]:
        return (self.local_candidate.host, self.local_candidate.port)

    @property
    def local_candidate(self) -> Candidate:
        return self.protocol.local_candidate

    @property
    def remote_addr(self) -> tuple[str, int]:
        return (self.remote_candidate.host, self.remote_candidate.port)

    class State(enum.Enum):
        FROZEN = 0
        WAITING = 1
        IN_PROGRESS = 2
        SUCCEEDED = 3
        FAILED = 4


class StunProtocol(asyncio.DatagramProtocol):
    def __init__(self, receiver: "Connection") -> None:
        self.__closed: asyncio.Future[bool] = asyncio.Future()
        self.id = next(protocol_id)
        self.local_candidate: Optional[Candidate] = None
        self.receiver = receiver
        self.transport: Optional[asyncio.DatagramTransport] = None
        self.transactions: dict[bytes, stun.Transaction] = {}

    def connection_lost(self, exc: Exception) -> None:  # type: ignore
        self.__log_debug("connection_lost(%s)", exc)
        if not self.__closed.done():
            self.receiver.data_received(None, None)
            self.__closed.set_result(True)

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        self.__log_debug("connection_made(%s)", transport)
        self.transport = cast(asyncio.DatagramTransport, transport)

    def datagram_received(self, data: Union[bytes, str], addr: tuple) -> None:
        # force IPv6 four-tuple to a two-tuple
        addr = (addr[0], addr[1])
        data = cast(bytes, data)
        if not data:
            return

        try:
            message = stun.parse_message(data)
            self.__log_debug("< %s %s", addr, message)
        except ValueError:
            self.receiver.data_received(data, self.local_candidate.component)
            return

        if (
            message.message_class == stun.Class.RESPONSE
            or message.message_class == stun.Class.ERROR
        ) and message.transaction_id in self.transactions:
            transaction = self.transactions[message.transaction_id]
            transaction.response_received(message, addr)
        elif message.message_class == stun.Class.REQUEST:
            self.receiver.request_received(message, addr, self, data)

    def error_received(self, exc: Exception) -> None:
        self.__log_debug("error_received(%s)", exc)

    # custom

    async def close(self) -> None:
        self.transport.close()
        await self.__closed

    async def request(
        self,
        request: stun.Message,
        addr: tuple[str, int],
        integrity_key: Optional[bytes] = None,
        retransmissions: Optional[int] = None,
    ) -> tuple[stun.Message, tuple[str, int]]:
        """
        Execute a STUN transaction and return the response.
        """
        assert request.transaction_id not in self.transactions

        if integrity_key is not None:
            request.add_message_integrity(integrity_key)

        transaction = stun.Transaction(
            request, addr, self, retransmissions=retransmissions
        )
        self.transactions[request.transaction_id] = transaction
        try:
            return await transaction.run()
        finally:
            del self.transactions[request.transaction_id]

    async def send_data(self, data: bytes, addr: tuple[str, int]) -> None:
        self.transport.sendto(data, addr)

    def send_stun(self, message: stun.Message, addr: tuple[str, int]) -> None:
        """
        Send a STUN message.
        """
        self.__log_debug("> %s %s", addr, message)
        self.transport.sendto(bytes(message), addr)

    def __log_debug(self, msg: str, *args: object) -> None:
        logger.debug("%s %s " + msg, self.receiver, self, *args)

    def __repr__(self) -> str:
        return "protocol(%s)" % self.id


class MuxedStunProtocol(StunProtocol):
    """A connection's protocol on a shared socket or listener (`mux`).

    The mux hands it a transport of its own and routes the answers to the
    STUN requests it sends back by transaction id; an ICE check's answer also
    teaches the mux the peer's address, a STUN server's does not.
    """

    async def request(
        self,
        request: stun.Message,
        addr: tuple[str, int],
        integrity_key: Optional[bytes] = None,
        retransmissions: Optional[int] = None,
    ) -> tuple[stun.Message, tuple[str, int]]:
        self.transport.expect_response(
            request.transaction_id, "USERNAME" in request.attributes
        )
        try:
            return await super().request(
                request, addr, integrity_key=integrity_key, retransmissions=retransmissions
            )
        finally:
            self.transport.forget_response(request.transaction_id)

    def __repr__(self) -> str:
        return "protocol(%s, muxed)" % self.id


class ConnectionEvent:
    pass


class ConnectionClosed(ConnectionEvent):
    pass


class Connection:
    """
    An ICE connection for a single media stream.

    :param ice_controlling: Whether the local peer has the controlling role.
    :param components: The number of components.
    :param stun_server: The address of the STUN server or `None`.
    :param turn_server: The address of the TURN server or `None`.
    :param turn_username: The username for the TURN server.
    :param turn_password: The password for the TURN server.
    :param turn_ssl: Whether to use TLS for the TURN server.
    :param turn_transport: The transport for TURN server, `"udp"` or `"tcp"`.
    :param use_ipv4: Whether to use IPv4 candidates.
    :param use_ipv6: Whether to use IPv6 candidates.
    :param transport_policy: Transport policy.
    :param local_username: An optional local username, otherwise a random one
                           will be generated.
    :param local_password: An optional local password, otherwise a random one
                           will be generated.
    :param nat1to1_ips: Optional public addresses (IPv4 and/or IPv6) to
                        advertise in host candidates of the matching family
                        (static 1:1 NAT), otherwise host candidates use the
                        bound address.
    :param port_range: Optional inclusive `(min, max)` UDP port window the
                       host candidate sockets bind into.
    :param udp_mux: Optional `mux.UdpMux` whose shared sockets carry the UDP
                    host candidates instead of sockets of their own; the port
                    range is then unused.
    :param tcp_mux: Optional `mux.TcpMux` whose shared listeners add a passive
                    ICE-TCP host candidate per address.
    :param ice_lite: Run as an ICE-lite agent (RFC 8445 appendix A): host
                     candidates only, always the controlled role, answering
                     the peer's connectivity checks instead of sending any.
    """

    def __init__(
        self,
        ice_controlling: bool,
        components: int = 1,
        stun_server: Optional[tuple[str, int]] = None,
        turn_server: Optional[tuple[str, int]] = None,
        turn_username: Optional[str] = None,
        turn_password: Optional[str] = None,
        turn_ssl: bool = False,
        turn_transport: str = "udp",
        use_ipv4: bool = True,
        use_ipv6: bool = True,
        transport_policy: TransportPolicy = TransportPolicy.ALL,
        local_username: Optional[str] = None,
        local_password: Optional[str] = None,
        nat1to1_ips: Optional[list[str]] = None,
        port_range: Optional[tuple[int, int]] = None,
        udp_mux: Optional[mux.UdpMux] = None,
        tcp_mux: Optional[mux.TcpMux] = None,
        ice_lite: bool = False,
    ) -> None:
        if ice_lite and (stun_server is not None or turn_server is not None):
            raise ValueError(
                "An ICE-lite agent gathers host candidates only; it takes no STUN or TURN server."
            )
        self.ice_lite = ice_lite
        self.ice_controlling = ice_controlling and not ice_lite
        self._udp_mux = udp_mux
        self._tcp_mux = tcp_mux

        if local_username is None:
            # Shared sockets tell sessions apart by this fragment alone, so it
            # is longer than the four characters ICE requires.
            local_username = random_string(8)
        else:
            validate_username(local_username)

        if local_password is None:
            local_password = random_string(22)
        else:
            validate_password(local_password)

        #: Whether the remote party is an ICE Lite implementation.
        self.remote_is_lite = False
        #: Remote username, which you need to set.
        self.remote_username: Optional[str] = None
        #: Remote password, which you need to set.
        self.remote_password: Optional[str] = None

        self.stun_server = stun_server
        self.turn_server = turn_server
        self.turn_username = turn_username
        self.turn_password = turn_password
        self.turn_ssl = turn_ssl
        self.turn_transport = turn_transport

        # private
        self._closed = False
        self._components = set(range(1, components + 1))
        self._check_list: list[CandidatePair] = []
        self._check_list_done = False
        self._check_list_state: asyncio.Queue = asyncio.Queue()
        self._early_checks: list[
            tuple[stun.Message, tuple[str, int], StunProtocol]
        ] = []
        self._early_checks_done = False
        self._event_waiter: Optional[asyncio.Future[ConnectionEvent]] = None
        self._id = next(connection_id)
        self._local_candidates: list[Candidate] = []
        self._local_candidates_end = False
        self._local_candidates_start = False
        self._local_password = local_password
        self._local_username = local_username
        self._nominated: dict[int, CandidatePair] = {}
        self._nominating: set[int] = set()
        self._protocols: list[StunProtocol] = []
        self._remote_candidates: list[Candidate] = []
        self._remote_candidates_end = False
        self._query_consent_task: Optional[asyncio.Task] = None
        self._queue: asyncio.Queue[tuple[Optional[bytes], Optional[int]]] = (
            asyncio.Queue()
        )
        self._tie_breaker = secrets.randbits(64)
        self._use_ipv4 = use_ipv4
        self._use_ipv6 = use_ipv6

        # Host-candidate UDP confinement: a scheduler-managed deployment hands
        # a session a small firewalled port window, so the sockets behind
        # direct host candidates must bind inside it. Bounds below 1024 are
        # rejected rather than clamped; STUN/TURN/mDNS sockets stay
        # unconfined, and the TURN relay keeps its own separate
        # TURN_MIN_PORT/TURN_MAX_PORT.
        if port_range is not None:
            range_min, range_max = port_range
            if not 1024 <= range_min <= range_max <= 65535:
                raise ValueError(
                    "port_range must satisfy 1024 <= min <= max <= 65535, "
                    f"got {port_range!r}"
                )
        self._port_range = port_range

        # NAT1TO1: validated public addresses keyed by IP version. A host
        # candidate of a configured family advertises the mapped address in
        # place of its bound private one; unconfigured families are unchanged.
        self._nat1to1_ips: dict[int, str] = {}
        for raw_ip in nat1to1_ips or []:
            try:
                addr = ipaddress.ip_address(raw_ip.strip())
            except ValueError:
                continue
            self._nat1to1_ips[addr.version] = str(addr)

        if (
            stun_server is None
            and turn_server is None
            and transport_policy == TransportPolicy.RELAY
        ):
            raise ValueError(
                "Relay transport policy requires a STUN and/or TURN server."
            )

        self._transport_policy = transport_policy

    @property
    def local_candidates(self) -> list[Candidate]:
        """
        Local candidates, automatically set by :meth:`gather_candidates`.
        """
        return self._local_candidates[:]

    @property
    def local_password(self) -> str:
        """
        Local password, set at construction time.
        """
        return self._local_password

    @property
    def local_username(self) -> str:
        """
        Local username, set at construction time.
        """
        return self._local_username

    @property
    def remote_candidates(self) -> list[Candidate]:
        """
        Remote candidates, which you need to populate using
        :meth:`add_remote_candidate`.
        """
        return self._remote_candidates[:]

    async def add_remote_candidate(self, remote_candidate: Optional[Candidate]) -> None:
        """
        Add a remote candidate or signal end-of-candidates.

        To signal end-of-candidates, pass `None`.

        :param remote_candidate: A :class:`Candidate` instance or `None`.
        """
        if self._remote_candidates_end:
            raise ValueError("Cannot add remote candidate after end-of-candidates.")

        # end-of-candidates
        if remote_candidate is None:
            self._prune_components()
            self._remote_candidates_end = True
            return

        # resolve mDNS candidate
        if mdns.is_mdns_hostname(remote_candidate.host):
            mdns_protocol = await get_or_create_mdns_protocol(self)
            remote_addr = await mdns_protocol.resolve(remote_candidate.host)
            if remote_addr is None:
                self.__log_info(
                    f'Remote candidate "{remote_candidate.host}" could not be resolved'
                )
                return
            self.__log_info(
                f'Remote candidate "{remote_candidate.host}" resolved to {remote_addr}'
            )

            copy_candidate = copy.copy(remote_candidate)
            copy_candidate.host = remote_addr
            await self.add_remote_candidate(copy_candidate)
            return

        # validate the remote candidate
        try:
            validate_remote_candidate(remote_candidate)
        except ValueError as e:
            self.__log_info(
                f'Remote candidate "{remote_candidate.host}" is not valid: {e}'
            )
            return
        self._remote_candidates.append(remote_candidate)

        # pair the remote candidate
        for protocol in self._protocols:
            if self._pairs_with(
                protocol, remote_candidate
            ) and not self._find_pair(protocol, remote_candidate):
                pair = CandidatePair(protocol, remote_candidate)
                self._check_list.append(pair)
        self.sort_check_list()

    async def gather_candidates(self) -> None:
        """
        Gather local candidates.

        You **must** call this coroutine before calling :meth:`connect`.
        """
        if not self._local_candidates_start:
            self._local_candidates_start = True
            addresses = get_host_addresses(
                use_ipv4=self._use_ipv4, use_ipv6=self._use_ipv6
            )
            coros = [
                self.get_component_candidates(component=component, addresses=addresses)
                for component in self._components
            ]
            for candidates in await asyncio.gather(*coros):
                self._local_candidates += candidates
            self._local_candidates_end = True

    def get_default_candidate(self, component: int) -> Optional[Candidate]:
        """
        Get the default local candidate for the specified component.

        :param component: The component whose default candidate is requested.
        """
        for candidate in sorted(self._local_candidates, key=lambda x: x.priority):
            if candidate.component == component:
                return candidate
        return None

    async def connect(self) -> None:
        """
        Perform ICE handshake.

        This coroutine returns if a candidate pair was successfully nominated
        and raises an exception otherwise.
        """
        if not self._local_candidates_end:
            raise ConnectionError("Local candidates gathering was not performed")

        if self.remote_username is None or self.remote_password is None:
            raise ConnectionError("Remote username or password is missing")

        # 5.7.1. Forming Candidate Pairs
        for remote_candidate in self._remote_candidates:
            for protocol in self._protocols:
                if self._pairs_with(
                    protocol, remote_candidate
                ) and not self._find_pair(protocol, remote_candidate):
                    pair = CandidatePair(protocol, remote_candidate)
                    self._check_list.append(pair)
        self.sort_check_list()

        if not self.ice_lite:
            self._unfreeze_initial()

        # handle early checks
        for early_check in self._early_checks:
            self.check_incoming(*early_check)
        self._early_checks = []
        self._early_checks_done = True

        # perform checks; an ICE-lite agent only answers the peer's
        if not self.ice_lite:
            while True:
                if not self.check_periodic():
                    break
                await asyncio.sleep(0.02)

        # wait for completion; a path only the peer can open is waited for
        # within a bound, since no local check ever fails it
        if self._check_list or self._awaits_inbound_checks():
            timeout = INBOUND_CHECK_TIMEOUT if self._awaits_inbound_checks() else None
            try:
                res = await asyncio.wait_for(self._check_list_state.get(), timeout)
            except asyncio.TimeoutError:
                self.__log_info(
                    "ICE failed: no check from the peer completed within %ss",
                    INBOUND_CHECK_TIMEOUT,
                )
                self._check_list_done = True
                res = ICE_FAILED
        else:
            res = ICE_FAILED

        # cancel remaining checks
        for check in self._check_list:
            if check.task:
                check.task.cancel()

        if res != ICE_COMPLETED:
            raise ConnectionError("ICE negotiation failed")

        # start consent freshness tests
        self._query_consent_task = asyncio.create_task(self.query_consent())

    async def close(self) -> None:
        """
        Close the connection.
        """
        # stop consent freshness tests
        if self._query_consent_task and not self._query_consent_task.done():
            self._query_consent_task.cancel()
            try:
                await self._query_consent_task
            except asyncio.CancelledError:
                pass

        # stop check list, and a connect() awaiting the peer's checks
        if not self._check_list_done:
            self._check_list_state.put_nowait(ICE_FAILED)
            self._check_list_done = True

        # unreference mDNS
        await unref_mdns_protocol(self)

        self._nominated.clear()
        for protocol in self._protocols:
            await protocol.close()
        self._protocols.clear()
        self._local_candidates.clear()

        # emit event
        if not self._closed:
            self._emit_event(ConnectionClosed())
            self._closed = True

    async def get_event(self) -> Optional[ConnectionEvent]:
        """
        Return the next `ConnectionEvent` or `None` if the connection is
        already closed.

        This method may only be called once at a time.
        """
        assert self._event_waiter is None, "already awaiting event"
        if self._closed:
            return None
        loop = asyncio.get_running_loop()
        self._event_waiter = loop.create_future()
        return await asyncio.shield(self._event_waiter)

    async def recv(self) -> bytes:
        """
        Receive the next datagram.

        The return value is a `bytes` object representing the data received.

        If the connection is not established, a `ConnectionError` is raised.
        """
        data, component = await self.recvfrom()
        return data

    async def recvfrom(self) -> tuple[bytes, int]:
        """
        Receive the next datagram.

        The return value is a `(bytes, component)` tuple where `bytes` is a
        bytes object representing the data received and `component` is the
        component on which the data was received.

        If the connection is not established, a `ConnectionError` is raised.
        """
        if not len(self._nominated):
            raise ConnectionError("Cannot receive data, not connected")

        result = await self._queue.get()
        if result[0] is None:
            raise ConnectionError("Connection lost while receiving data")
        return result

    async def send(self, data: bytes) -> None:
        """
        Send a datagram on the first component.

        If the connection is not established, a `ConnectionError` is raised.

        :param data: The data to be sent.
        """
        await self.sendto(data, 1)

    async def sendto(self, data: bytes, component: int) -> None:
        """
        Send a datagram on the specified component.

        If the connection is not established, a `ConnectionError` is raised.

        :param data: The data to be sent.
        :param component: The component on which to send the data.
        """
        active_pair = self._nominated.get(component)
        if active_pair:
            await active_pair.protocol.send_data(data, active_pair.remote_addr)
        else:
            raise ConnectionError("Cannot send data, not connected")

    def set_selected_pair(
        self, component: int, local_foundation: str, remote_foundation: str
    ) -> None:
        """
        Force the selected candidate pair.

        If the remote party does not support ICE, you should using this
        instead of calling :meth:`connect`.
        """
        # find local candidate
        protocol = None
        for p in self._protocols:
            if (
                p.local_candidate.component == component
                and p.local_candidate.foundation == local_foundation
            ):
                protocol = p
                break

        # find remote candidate
        remote_candidate = None
        for c in self._remote_candidates:
            if c.component == component and c.foundation == remote_foundation:
                remote_candidate = c

        assert protocol and remote_candidate
        self._nominated[component] = CandidatePair(protocol, remote_candidate)

    # private

    def build_request(self, pair: CandidatePair, nominate: bool) -> stun.Message:
        tx_username = "%s:%s" % (self.remote_username, self.local_username)
        request = stun.Message(
            message_method=stun.Method.BINDING, message_class=stun.Class.REQUEST
        )
        request.attributes["USERNAME"] = tx_username
        request.attributes["PRIORITY"] = candidate_priority(pair.component, "prflx")
        if self.ice_controlling:
            request.attributes["ICE-CONTROLLING"] = self._tie_breaker
            if nominate:
                request.attributes["USE-CANDIDATE"] = None
        else:
            request.attributes["ICE-CONTROLLED"] = self._tie_breaker
        return request

    def check_complete(self, pair: CandidatePair) -> None:
        pair.task = None

        if pair.state == CandidatePair.State.SUCCEEDED:
            if pair.nominated and self._selects(pair):
                self._nominated[pair.component] = pair

                # 8.1.2.  Updating States
                #
                # The agent MUST remove all Waiting and Frozen pairs in the check
                # list and triggered check queue for the same component as the
                # nominated pairs for that media stream.
                for p in self._check_list:
                    if p.component == pair.component and p.state in [
                        CandidatePair.State.WAITING,
                        CandidatePair.State.FROZEN,
                    ]:
                        self.check_state(p, CandidatePair.State.FAILED)

            # Once there is at least one nominated pair in the valid list for
            # every component of at least one media stream and the state of the
            # check list is Running:
            if len(self._nominated) == len(self._components):
                if not self._check_list_done:
                    self.__log_info("ICE completed")
                    self._check_list_state.put_nowait(ICE_COMPLETED)
                    self._check_list_done = True
                return

            # 7.1.3.2.3.  Updating Pair States
            for p in self._check_list:
                if (
                    p.local_candidate.foundation == pair.local_candidate.foundation
                    and p.state == CandidatePair.State.FROZEN
                ):
                    self.check_state(p, CandidatePair.State.WAITING)

        for p in self._check_list:
            if p.state not in [
                CandidatePair.State.SUCCEEDED,
                CandidatePair.State.FAILED,
            ]:
                return

        if not self.ice_controlling:
            for p in self._check_list:
                if p.state == CandidatePair.State.SUCCEEDED:
                    return

        # the peer may still open a path of its own; connect() bounds the wait
        if self._awaits_inbound_checks():
            return

        if not self._check_list_done:
            self.__log_info("ICE failed")
            self._check_list_state.put_nowait(ICE_FAILED)
            self._check_list_done = True

    def check_incoming(
        self, message: stun.Message, addr: tuple[str, int], protocol: StunProtocol
    ) -> None:
        """
        Handle a successful incoming check.
        """
        component = protocol.local_candidate.component

        # find remote candidate
        remote_candidate = None
        for c in self._remote_candidates:
            if c.host == addr[0] and c.port == addr[1]:
                remote_candidate = c
                assert remote_candidate.component == component
                break
        if remote_candidate is None:
            # 7.2.1.3. Learning Peer Reflexive Candidates
            local_candidate = protocol.local_candidate
            remote_candidate = Candidate(
                foundation=random_string(10),
                component=component,
                transport=local_candidate.transport,
                priority=message.attributes["PRIORITY"],
                host=addr[0],
                port=addr[1],
                type="prflx",
                tcptype="active" if local_candidate.transport == "tcp" else None,
            )
            self._remote_candidates.append(remote_candidate)
            self.__log_info("Discovered peer reflexive candidate %s", remote_candidate)

        # find pair
        pair = self._find_pair(protocol, remote_candidate)
        if pair is None:
            pair = CandidatePair(protocol, remote_candidate)
            pair.state = CandidatePair.State.WAITING
            self._check_list.append(pair)
            self.sort_check_list()

        # triggered check; an ICE-lite agent sends none
        if not self.ice_lite and pair.state in [
            CandidatePair.State.WAITING,
            CandidatePair.State.FAILED,
        ]:
            self.check_start_task(pair)

        # 7.2.1.5. Updating the Nominated Flag
        if "USE-CANDIDATE" in message.attributes and not self.ice_controlling:
            pair.remote_nominated = True

            if self.ice_lite:
                # 7.3.1.5: the peer's nomination alone puts the pair in the
                # valid list of an ICE-lite agent
                if pair.state != CandidatePair.State.SUCCEEDED:
                    self.check_state(pair, CandidatePair.State.SUCCEEDED)
                if not pair.nominated:
                    pair.nominated = True
                    self.check_complete(pair)
            elif pair.state == CandidatePair.State.SUCCEEDED:
                pair.nominated = True
                self.check_complete(pair)

    def check_periodic(self) -> bool:
        # find the highest-priority pair that is in the waiting state
        for pair in self._check_list:
            if pair.state == CandidatePair.State.WAITING:
                self.check_start_task(pair)
                return True

        # find the highest-priority pair that is in the frozen state
        for pair in self._check_list:
            if pair.state == CandidatePair.State.FROZEN:
                self.check_start_task(pair)
                return True

        # if we expect more candidates, keep going
        if not self._remote_candidates_end:
            return not self._check_list_done

        return False

    async def check_start(self, pair: CandidatePair) -> None:
        """
        Starts a check.
        """
        self.check_state(pair, CandidatePair.State.IN_PROGRESS)

        nominate = self.ice_controlling and not self.remote_is_lite
        request = self.build_request(pair, nominate=nominate)
        try:
            response, addr = await pair.protocol.request(
                request,
                pair.remote_addr,
                integrity_key=self.remote_password.encode("utf8"),
            )
        except stun.TransactionError as exc:
            # 7.1.3.1. Failure Cases
            if (
                exc.response
                and exc.response.attributes.get("ERROR-CODE", (None, None))[0] == 487
            ):
                if "ICE-CONTROLLING" in request.attributes:
                    self.switch_role(ice_controlling=False)
                elif "ICE-CONTROLLED" in request.attributes:
                    self.switch_role(ice_controlling=True)
                return await self.check_start(pair)
            else:
                self.check_state(pair, CandidatePair.State.FAILED)
                self.check_complete(pair)
                return

        # check remote address matches
        if addr != pair.remote_addr:
            self.__log_info("Check %s failed : source address mismatch", pair)
            self.check_state(pair, CandidatePair.State.FAILED)
            self.check_complete(pair)
            return

        # success
        if nominate or pair.remote_nominated:
            # nominated by aggressive nomination or the remote party
            pair.nominated = True
        elif self.ice_controlling and pair.component not in self._nominating:
            # perform regular nomination
            self.__log_info("Check %s nominating pair", pair)
            self._nominating.add(pair.component)
            request = self.build_request(pair, nominate=True)
            try:
                await pair.protocol.request(
                    request,
                    pair.remote_addr,
                    integrity_key=self.remote_password.encode("utf8"),
                )
            except stun.TransactionError:
                self.__log_info("Check %s failed : could not nominate pair", pair)
                self.check_state(pair, CandidatePair.State.FAILED)
                self.check_complete(pair)
                return
            pair.nominated = True
        self.check_state(pair, CandidatePair.State.SUCCEEDED)
        self.check_complete(pair)

    def check_start_task(self, pair: CandidatePair) -> None:
        """
        Starts a check in a task, unless already started.
        """
        if pair.task is None:
            pair.task = asyncio.create_task(self.check_start(pair))

    def check_state(self, pair: CandidatePair, state: CandidatePair.State) -> None:
        """
        Updates the state of a check.
        """
        self.__log_info("Check %s %s -> %s", pair, pair.state, state)
        pair.state = state

    def _emit_event(self, event: ConnectionEvent) -> None:
        if self._event_waiter is not None:
            waiter = self._event_waiter
            self._event_waiter = None
            waiter.set_result(event)

    def _find_pair(
        self, protocol: StunProtocol, remote_candidate: Candidate
    ) -> Optional[CandidatePair]:
        """
        Find a candidate pair in the check list.
        """
        for pair in self._check_list:
            if pair.protocol == protocol and pair.remote_candidate == remote_candidate:
                return pair
        return None

    def _selects(self, pair: CandidatePair) -> bool:
        """
        Whether a nominated `pair` becomes the selected pair of its component.

        The controlled agent follows the peer's latest nomination. The
        controlling agent nominates every pair that succeeds, and keeps the
        highest-priority one: a TCP pair that succeeds after a UDP one must
        not move the session onto TCP.
        """
        current = self._nominated.get(pair.component)
        if current is None or current is pair or not self.ice_controlling:
            return True
        return candidate_pair_priority(
            pair.local_candidate, pair.remote_candidate, self.ice_controlling
        ) > candidate_pair_priority(
            current.local_candidate, current.remote_candidate, self.ice_controlling
        )

    def _pairs_with(self, protocol: StunProtocol, remote_candidate: Candidate) -> bool:
        """
        Whether the local candidate behind `protocol` forms a check list pair
        with `remote_candidate` in advance.

        A passive TCP candidate (RFC 6544) opens no connection of its own, so
        its pairs are learned from the peer's checks in `check_incoming`.
        """
        local_candidate = protocol.local_candidate
        if not local_candidate.can_pair_with(remote_candidate):
            return False
        return local_candidate.tcptype != "passive"

    def _awaits_inbound_checks(self) -> bool:
        """
        Whether a path can still be opened only by the peer: an ICE-lite agent
        awaits its nomination, a passive TCP candidate its connection, as long
        as the peer offered (or may still offer) a TCP candidate.
        """
        if self.ice_lite:
            return True
        if not any(
            p.local_candidate is not None and p.local_candidate.tcptype == "passive"
            for p in self._protocols
        ):
            return False
        return not self._remote_candidates_end or any(
            c.transport.lower() == "tcp" for c in self._remote_candidates
        )

    def peer_stream_lost(self, addr: tuple[str, int]) -> None:
        """
        A stream to `addr` closed; when the nominated pair rode it the
        connection ends with it instead of waiting for consent to expire.
        """
        if any(pair.remote_addr == addr for pair in self._nominated.values()):
            self.__log_info("Stream to %s lost, connection over", addr)
            self.data_received(None, None)

    async def get_component_candidates(
        self, component: int, addresses: list[str], timeout: int = 5
    ) -> list[Candidate]:
        candidates = []
        loop = asyncio.get_running_loop()

        # gather host candidates
        #
        # With a configured port range, binding is the reservation: the ports
        # are tried in random order and the kernel rejects occupied ones, so
        # concurrent sessions sharing one range spread without coordination.
        # On a UDP mux the shared sockets are the reservation instead.
        if self._port_range is not None and self._udp_mux is None:
            bind_ports = list(range(self._port_range[0], self._port_range[1] + 1))
            random.shuffle(bind_ports)
        else:
            bind_ports = [0]
        host_protocols = []
        for index, address in enumerate(addresses):
            # create transport
            protocol = None
            if self._udp_mux is not None:
                try:
                    protocol = await self._udp_mux.attach(
                        address, self.local_username, lambda: MuxedStunProtocol(self)
                    )
                except (OSError, ValueError) as exc:
                    self.__log_info("Could not share the UDP mux on %s - %s", address, exc)
                    continue
            else:
                last_error: Optional[OSError] = None
                for bind_port in bind_ports:
                    try:
                        transport, protocol = await loop.create_datagram_endpoint(
                            lambda: StunProtocol(self), local_addr=(address, bind_port)
                        )
                        break
                    except OSError as exc:
                        last_error = exc
                        # Only an occupied port can be cured by trying another
                        # one; anything else (address gone, permissions) would
                        # fail across the whole window identically.
                        if exc.errno != errno.EADDRINUSE:
                            break
                if protocol is None:
                    self.__log_info("Could not bind to %s - %s", address, last_error)
                    continue
                sock = transport.get_extra_info("socket")
                if sock is not None:
                    sock.setsockopt(
                        socket.SOL_SOCKET, socket.SO_RCVBUF, turn.UDP_SOCKET_BUFFER_SIZE
                    )
            host_protocols.append(protocol)

            # add host candidate
            candidate_address = protocol.transport.get_extra_info("sockname")
            protocol.local_candidate = Candidate(
                foundation=candidate_foundation("host", "udp", candidate_address[0]),
                component=component,
                transport="udp",
                priority=candidate_priority(
                    component, "host", host_local_preference(index)
                ),
                host=self._advertised_host(candidate_address[0]),
                port=candidate_address[1],
                type="host",
            )
            if self._transport_policy == TransportPolicy.ALL:
                candidates.append(protocol.local_candidate)
        self._protocols += host_protocols

        # ICE-TCP: a passive host candidate per address on the shared TCP
        # port; the peer opens the stream, so it takes part in no outbound
        # check and never carries a server-reflexive query
        if self._tcp_mux is not None:
            for index, address in enumerate(addresses):
                try:
                    protocol = await self._tcp_mux.attach(
                        address, self.local_username, lambda: MuxedStunProtocol(self)
                    )
                except (OSError, ValueError) as exc:
                    self.__log_info("Could not share the TCP mux on %s - %s", address, exc)
                    continue
                candidate_address = protocol.transport.get_extra_info("sockname")
                protocol.local_candidate = Candidate(
                    foundation=candidate_foundation("host", "tcp", candidate_address[0]),
                    component=component,
                    transport="tcp",
                    priority=candidate_priority(
                        component, "host", host_local_preference(index, "passive")
                    ),
                    host=self._advertised_host(candidate_address[0]),
                    port=candidate_address[1],
                    type="host",
                    tcptype="passive",
                )
                self._protocols.append(protocol)
                if self._transport_policy == TransportPolicy.ALL:
                    candidates.append(protocol.local_candidate)

        tasks: list[asyncio.Task[tuple[Candidate, Optional[StunProtocol]]]] = []

        # Query STUN server for server-reflexive candidates (IPv4 only).
        if self.stun_server:
            for protocol in host_protocols:
                if ipaddress.ip_address(protocol.local_candidate.host).version == 4:
                    tasks.append(
                        asyncio.create_task(
                            server_reflexive_candidate(protocol, self.stun_server)
                        )
                    )

        # Connect to TURN server.
        if self.turn_server:
            tasks.append(
                asyncio.create_task(
                    relayed_candidate(
                        component=component,
                        protocol_factory=lambda: StunProtocol(self),
                        turn_server=self.turn_server,
                        turn_username=self.turn_username,
                        turn_password=self.turn_password,
                        turn_ssl=self.turn_ssl,
                        turn_transport=self.turn_transport,
                    )
                )
            )

        # Run tasks in parallel and handle exceptions.
        if len(tasks):
            done, pending = await asyncio.wait(tasks, timeout=timeout)
            for task in done:
                if task.exception() is None:
                    candidate, protocol = task.result()
                    candidates.append(candidate)
                    if protocol is not None:
                        self._protocols.append(protocol)
            for task in pending:
                task.cancel()

        return candidates

    def _advertised_host(self, bound_address: str) -> str:
        """
        The address a host candidate bound on `bound_address` advertises.

        NAT1TO1: the configured public address of the candidate's IP family
        replaces the private one while the socket stays bound privately, so a
        peer behind static 1:1 NAT connects to this host directly. Foundations
        stay keyed on the bound address (hashed, never signalled verbatim).
        """
        return self._nat1to1_ips.get(
            ipaddress.ip_address(bound_address).version, bound_address
        )

    def _prune_components(self) -> None:
        """
        Remove components for which the remote party did not provide any candidates.

        This can only be determined after end-of-candidates.
        """
        seen_components = set(map(lambda x: x.component, self._remote_candidates))
        missing_components = self._components - seen_components
        if missing_components:
            self.__log_info(
                "Components %s have no candidate pairs" % missing_components
            )
            self._components = seen_components

    async def query_consent(self) -> None:
        """
        Periodically check consent (RFC 7675).
        """
        failures = 0
        while True:
            # randomize between 0.8 and 1.2 times CONSENT_INTERVAL
            await asyncio.sleep(CONSENT_INTERVAL * (0.8 + 0.4 * random.random()))

            for pair in self._nominated.values():
                request = self.build_request(pair, nominate=False)
                try:
                    await pair.protocol.request(
                        request,
                        pair.remote_addr,
                        integrity_key=self.remote_password.encode("utf8"),
                        retransmissions=0,
                    )
                    failures = 0
                except stun.TransactionError:
                    failures += 1
                if failures >= CONSENT_FAILURES:
                    self.__log_info("Consent to send expired")
                    self._query_consent_task = None
                    return await self.close()

    def data_received(self, data: Optional[bytes], component: Optional[int]) -> None:
        self._queue.put_nowait((data, component))

    def request_received(
        self,
        message: stun.Message,
        addr: tuple[str, int],
        protocol: StunProtocol,
        raw_data: bytes,
    ) -> None:
        if message.message_method != stun.Method.BINDING:
            self.respond_error(message, addr, protocol, (400, "Bad Request"))
            return

        # authenticate request
        try:
            stun.parse_message(
                raw_data, integrity_key=self.local_password.encode("utf8")
            )
            if self.remote_username is not None:
                rx_username = "%s:%s" % (self.local_username, self.remote_username)
                if message.attributes.get("USERNAME") != rx_username:
                    raise ValueError("Wrong username")
        except ValueError:
            self.respond_error(message, addr, protocol, (400, "Bad Request"))
            return

        # 7.2.1.1. Detecting and Repairing Role Conflicts
        if self.ice_controlling and "ICE-CONTROLLING" in message.attributes:
            self.__log_info("Role conflict, expected to be controlling")
            if self._tie_breaker >= message.attributes["ICE-CONTROLLING"]:
                self.respond_error(message, addr, protocol, (487, "Role Conflict"))
                return
            self.switch_role(ice_controlling=False)
        elif not self.ice_controlling and "ICE-CONTROLLED" in message.attributes:
            if self.ice_lite:
                # only the full agent can take the controlling role
                self.respond_error(message, addr, protocol, (487, "Role Conflict"))
                return
            self.__log_info("Role conflict, expected to be controlled")
            if self._tie_breaker < message.attributes["ICE-CONTROLLED"]:
                self.respond_error(message, addr, protocol, (487, "Role Conflict"))
                return
            self.switch_role(ice_controlling=True)

        # send binding response
        response = stun.Message(
            message_method=stun.Method.BINDING,
            message_class=stun.Class.RESPONSE,
            transaction_id=message.transaction_id,
        )
        response.attributes["XOR-MAPPED-ADDRESS"] = addr
        response.add_message_integrity(self.local_password.encode("utf8"))
        protocol.send_stun(response, addr)

        if not self._check_list and not self._early_checks_done:
            self._early_checks.append((message, addr, protocol))
        else:
            self.check_incoming(message, addr, protocol)

    def respond_error(
        self,
        request: stun.Message,
        addr: tuple[str, int],
        protocol: StunProtocol,
        error_code: tuple[int, str],
    ) -> None:
        response = stun.Message(
            message_method=request.message_method,
            message_class=stun.Class.ERROR,
            transaction_id=request.transaction_id,
        )
        response.attributes["ERROR-CODE"] = error_code
        response.add_message_integrity(self.local_password.encode("utf8"))
        protocol.send_stun(response, addr)

    def sort_check_list(self) -> None:
        sort_candidate_pairs(self._check_list, self.ice_controlling)

    def switch_role(self, ice_controlling: bool) -> None:
        if ice_controlling and self.ice_lite:
            self.__log_info("Staying controlled: an ICE-lite agent never controls")
            return
        self.__log_info(
            "Switching to %s role", ice_controlling and "controlling" or "controlled"
        )
        self.ice_controlling = ice_controlling
        self.sort_check_list()

    def _unfreeze_initial(self) -> None:
        # unfreeze first pair for the first component
        first_pair = None
        for pair in self._check_list:
            if pair.component == min(self._components):
                first_pair = pair
                break
        if first_pair is None:
            return
        if first_pair.state == CandidatePair.State.FROZEN:
            self.check_state(first_pair, CandidatePair.State.WAITING)

        # unfreeze pairs with same component but different foundations
        seen_foundations = set(first_pair.local_candidate.foundation)
        for pair in self._check_list:
            if (
                pair.component == first_pair.component
                and pair.local_candidate.foundation not in seen_foundations
                and pair.state == CandidatePair.State.FROZEN
            ):
                self.check_state(pair, CandidatePair.State.WAITING)
                seen_foundations.add(pair.local_candidate.foundation)

    def __log_info(self, msg: str, *args: object) -> None:
        logger.info("%s " + msg, self, *args)

    def __repr__(self) -> str:
        return "Connection(%s)" % self._id
