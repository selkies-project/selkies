# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""WebRTC signaling: peer registry, session/room relay, and secure-mode gates.

Implements the WebSocket signaling plane the WebRTC streaming mode rides on.
One in-process "server" peer owns the media graph; browser "client" peers
(controllers and viewers, one set per display) pair with it through SESSION
messages, or exchange ROOM messages for multi-peer rooms. Newest-connection-wins
eviction mirrors websockets mode so a page refresh supersedes its own stale
socket, with a takeover-storm breaker so two live auto-reconnecting pages
cannot trade the session forever.

Wire protocol (text frames, space-separated): a peer opens with
`HELLO <server|client> [<json-metadata>]` (`client_type`, `client_slot`,
`client_strict_viewer`, `client_token`, `server_token`, `display_id`,
`display_position`) and is answered `HELLO`. `SESSION <peer-id|server>` pairs
the caller with the callee: the caller gets `SESSION_OK <callee-id>`, the callee
`SESSION_START <caller-id> <client_type> <display_id> <display_position>
[<client_token>]`, and a disconnect sends the partner `SESSION_END <peer-id>
<client_type>`. In a session every message is addressed `<peer-id> <message>`
and relayed as `<sender-id> <message>`, only between session partners. `ROOM
<room-id>` joins or creates a room (`ROOM_OK <member-ids>`, members get
`ROOM_PEER_JOINED` / `ROOM_PEER_LEFT <peer-id>`), where `ROOM_PEER_MSG
<peer-id> <message>` is relayed as `ROOM_PEER_MSG <sender-id> <message>`.
Errors are `ERROR <text>`.

Concurrency model: all registry mutation happens under a single asyncio.Lock,
and every socket send/close triggered while holding it is deferred and flushed
after release, so one slow peer socket can never serialize other handshakes.

In secure mode (a master token is configured) the auth middleware forwards
WebSocket upgrades without Basic auth, so the token checks here are the only
gate: client peers must present a provisioned session token and server peers
the master token itself. Token checks are constant-time lookups in the live
control-plane table (`/api/tokens`), read per handshake.
"""

import os
import hmac
import json
import uuid
import logging
import asyncio
import concurrent.futures
from dataclasses import dataclass
from aiohttp.web_ws import WebSocketResponse
from aiohttp import web, WSMessage, WSMsgType
from typing import Awaitable, Callable, Dict, Set, Optional, Any, Tuple, List

from .webrtc_utils import _is_trusted_config_file
from .settings import settings as app_settings
from .selkies import _lookup_session_token
from .stream_server import note_pong

logger = logging.getLogger("signaling")


@dataclass
class Peer:
    """A connected signaling peer (the in-process server or a browser client).

    Attributes:
        uid: Registry key, "server-" or "client-" prefixed UUID.
        ws: The peer's WebSocket.
        raddr: Remote address, for logging.
        peer_type: Either "server" or "client".
        client_type: For clients, "viewer" or "controller".
        client_slot: Player slot number, or -1 when unassigned.
        client_strict_viewer: Whether the client is a strict shared viewer.
        client_token: Secure-mode session token; matched against the active
            mk token to grant a viewer read-write collaboration (websockets
            mk-token parity).
        peer_status: None until paired, then "session" or the room id.
        display_id: Display this peer drives ("primary", "display2", ...);
            controller and slot uniqueness are scoped per display so a second
            display's client never evicts the primary (websockets parity).
        display_position: Where a secondary display sits relative to the
            primary ("right"/"left"/"up"/"down"), carried to the server side
            so it lays the framebuffer out like websockets mode.
    """

    uid: str
    ws: WebSocketResponse
    raddr: str
    peer_type: str
    client_type: Optional[str]
    client_slot: Optional[int]
    client_strict_viewer: Optional[bool]
    client_token: Optional[str] = None
    peer_status: Optional[str] = None
    display_id: str = "primary"
    display_position: str = "right"


class WebRTCPeerManagement:
    """Manages WebRTC peer registration, session/room relay, and RTC config.

    Attributes:
        peers: Registry of connected peers by uid.
        sessions: Unidirectional caller-to-callee (client-to-server) session map;
            stored one-way so termination is unambiguous on client disconnect.
        rooms: Sets of member peer uids by room id.
        on_client_presence: Called with True/False as browser (client-type)
            peers come and go; the server's own signaling peer does not count.
        rtc_config: Config served from `/api/turn`. Primarily the config the
            server itself resolved via `get_rtc_configuration()` (REST,
            Cloudflare, JSON file, legacy, HMAC or built-in default), passed
            as `options.rtc_config` and kept fresh by the RTC monitors through
            `set_rtc_config`, so the client negotiates with the same ICE
            servers as the server; the local `rtc_config_file` is read only
            when no resolved config was supplied.
        _eviction_times: Recent newest-wins takeover times per identity key,
            the takeover-storm window `_eviction_storm` reads.
    """

    def __init__(self, options: Any) -> None:
        self.peers: Dict[str, Peer] = {}
        self.sessions: Dict[str, str] = {}
        self.rooms: Dict[str, Set[str]] = {}
        self.on_client_presence: Optional[Callable[[bool], None]] = None
        self._eviction_times: Dict[Any, List[float]] = {}

        self.keepalive_timeout: int = options.keepalive_timeout

        self.turn_shared_secret: Optional[str] = options.turn_shared_secret
        self.turn_host: Optional[str] = options.turn_host
        self.turn_port: Optional[int] = options.turn_port
        self.turn_protocol: str = options.turn_protocol.lower()
        if self.turn_protocol != "tcp":
            self.turn_protocol = "udp"
        self.turn_tls: Optional[bool] = options.turn_tls
        self.turn_auth_header_name: str = options.turn_auth_header_name
        self.stun_host: Optional[str] = options.stun_host
        self.stun_port: Optional[int] = options.stun_port

        self.enable_sharing: bool = options.enable_sharing
        self.enable_shared: bool = options.enable_shared
        self.enable_player2: bool = options.enable_player2
        self.enable_player3: bool = options.enable_player3
        self.enable_player4: bool = options.enable_player4
        self.lock: asyncio.Lock = asyncio.Lock()

        self.rtc_config: Optional[Any] = getattr(options, "rtc_config", None)
        if not self.rtc_config and os.path.exists(options.rtc_config_file):
            logger.info("parsing rtc_config_file: {}".format(options.rtc_config_file))
            if _is_trusted_config_file(options.rtc_config_file):
                self.rtc_config = self.read_file(options.rtc_config_file)
            else:
                logger.error(
                    "Refusing to use RTC config file {!r}: unsafe ownership or permissions.".format(
                        options.rtc_config_file
                    )
                )
        if self.turn_shared_secret:
            if not (self.turn_host and self.turn_port):
                raise Exception(
                    "missing turn_host or turn_port options with turn_shared_secret"
                )

    def read_file(self, path: str) -> Optional[str]:
        """Read and return the contents of a file as a string.

        Args:
            path: Path to the file to read.

        Returns:
            File contents as a string, or None if the file cannot be read.
        """
        try:
            with open(path, "r") as f:
                return f.read()
        except OSError as e:
            logger.error("Failed to read rtc_config_file {!r}: {}".format(path, e))
            return None

    def set_rtc_config(self, rtc_config: Any) -> None:
        """Set the RTC configuration served to clients.

        Args:
            rtc_config: RTC configuration as a JSON string or dict.
        """
        self.rtc_config = rtc_config

    async def recv_msg_ping(self, ws: WebSocketResponse, raddr: str) -> WSMessage:
        """Wait for a message, sending periodic pings to prevent connection timeout.

        Args:
            ws: The peer's WebSocket.
            raddr: Remote address, for logging.

        Returns:
            The received message; blocks (with keepalive pings on each
            ``keepalive_timeout``) until one arrives.

        Raises:
            Exception: The socket closed while waiting.
        """
        msg_obj: Optional[Any] = None
        while msg_obj is None:
            try:
                if ws.closed:
                    raise Exception(f"Websocket Connection closed with {ws.close_code} status code")
                msg_obj = await asyncio.wait_for(ws.receive(), self.keepalive_timeout)
            except (asyncio.TimeoutError, concurrent.futures.TimeoutError):
                logger.debug("Sending keepalive ping to {!r} in recv".format(raddr))
                await ws.ping()
            except (ConnectionResetError, ConnectionError, Exception):
                raise
        return msg_obj

    async def cleanup_session(self, uid: str) -> None:
        """Clean up a session when a peer disconnects.

        Every client — a primary controller included — only tells the server
        its session ended: the server socket is shared by every display's
        session and every viewer, so dropping it would reset the whole
        session. The server side reaps just this peer and keeps a reconnect
        grace on the primary capture, so a controller tab reload never kicks
        the viewers or the second display (websockets `_teardown_if_unclaimed`
        parity).

        Args:
            uid: Peer ID to clean up.
        """
        if uid in self.sessions:
            other_id = self.sessions[uid]
            del self.sessions[uid]
            peer = self.peers.get(uid)
            client_type = peer.client_type if peer else "unknown"
            logger.info(
                "Cleaned up {} session, client type {!r}".format(
                    uid, client_type
                )
            )

            if not peer:
                return
            if peer.client_type in ("controller", "viewer"):
                other_peer = self.peers.get(other_id)
                if other_peer:
                    wso = other_peer.ws
                    msg: str = "SESSION_END {} {}".format(uid, peer.client_type)
                    logger.info("{} -> {}: {}".format(uid, other_id, msg))
                    await wso.send_str(msg)

    async def cleanup_room(self, uid: str, room_id: str) -> None:
        """Remove a peer from a room and notify the remaining members.

        Args:
            uid: Peer ID to remove.
            room_id: Room ID to remove from.
        """
        room_peers: Optional[Set[str]] = self.rooms.get(room_id)
        if not room_peers or uid not in room_peers:
            return
        room_peers.remove(uid)
        if not room_peers:
            del self.rooms[room_id]
        # Snapshot: the awaits below can interleave with a concurrent join.
        for pid in list(room_peers):
            peer = self.peers.get(pid)
            if not peer:
                continue
            wsp = peer.ws

            msg = "ROOM_PEER_LEFT {}".format(uid)
            logger.info("room {}: {} -> {}: {}".format(room_id, uid, pid, msg))
            await wsp.send_str(msg)

    def _evict_dead_peer_locked(self, pid: str) -> List[Callable[[], Awaitable[Any]]]:
        """Detach a dead peer from shared state (in-memory only) while holding
        ``self.lock``.

        Mirrors `cleanup_session` and `cleanup_room` without socket I/O. The
        session partner (the server) is told the session ended and keeps its
        socket: it serves every client, and closing it would cascade —
        `remove_peer` on the server closes all clients, including the
        replacement just registered, a self-terminating reconnect.

        Returns:
            Zero-arg coroutine factories for the partner notifications
            (SESSION_END / ROOM_PEER_LEFT / close); the caller MUST run them
            AFTER releasing the lock so a slow partner socket can't stall
            other handshakes.
        """
        deferred: List[Callable[[], Awaitable[Any]]] = []
        peer = self.peers.get(pid)

        if pid in self.sessions:
            other_id = self.sessions[pid]
            del self.sessions[pid]
            if peer is not None:
                other_peer = self.peers.get(other_id)
                if other_peer is not None:
                    wso = other_peer.ws
                    msg = "SESSION_END {} {}".format(pid, peer.client_type)
                    logger.info("{} -> {}: {}".format(pid, other_id, msg))
                    deferred.append(lambda ws=wso, m=msg: ws.send_str(m))

        peer_status = getattr(peer, "peer_status", None) if peer else None
        if peer_status and peer_status != "session":
            room_peers: Optional[Set[str]] = self.rooms.get(peer_status)
            if room_peers and pid in room_peers:
                room_peers.remove(pid)
                if not room_peers:
                    del self.rooms[peer_status]
                msg = "ROOM_PEER_LEFT {}".format(pid)
                for other_pid in list(room_peers):
                    other_peer = self.peers.get(other_pid)
                    if not other_peer:
                        continue
                    logger.info(
                        "room {}: {} -> {}: {}".format(
                            peer_status, pid, other_pid, msg
                        )
                    )
                    deferred.append(
                        lambda ws=other_peer.ws, m=msg: ws.send_str(m)
                    )

        self.peers.pop(pid, None)
        return deferred

    async def remove_peer(self, uid: str) -> None:
        """Remove a peer and clean up its session, room, and socket.

        A server peer leaving closes every client connection, since the server
        owns the media graph they all stream from; a client peer leaving
        closes only its own socket. Closes are collected under the lock and
        awaited after it, bounded per socket and best-effort.

        Args:
            uid: Peer ID to remove.
        """
        deferred_closes: List[Callable[[], Awaitable[Any]]] = []
        async with self.lock:
            await self.cleanup_session(uid)

            peer = self.peers.get(uid)
            if peer:
                ws = peer.ws
                raddr = peer.raddr
                peer_status = peer.peer_status
                peer_type = peer.peer_type
                client_type = peer.client_type
                if peer_status and peer_status != "session":
                    await self.cleanup_room(uid, peer_status)
                if peer_type == "server":
                    del self.peers[uid]
                    for p in list(self.peers.values()):
                        if p.peer_type == "client":
                            deferred_closes.append(
                                lambda cws=p.ws: cws.close(
                                    code=4000,
                                    message=b"Server disconnected, closing connection.",
                                )
                            )
                else:
                    del self.peers[uid]
                    deferred_closes.append(
                        lambda cws=ws: cws.close(
                            code=1000, message=b"Connection closed"
                        )
                    )
                    logger.info(
                        "Disconnected from peer {!r} at {!r} of client_type {!r}".format(
                            uid, raddr, client_type
                        )
                    )
        for make_coro in deferred_closes:
            try:
                await asyncio.wait_for(make_coro(), timeout=5)
            except Exception as exc:
                logger.debug("Deferred peer close failed/timed out: {}".format(exc))

    async def peer_connection_handler(
        self,
        ws: WebSocketResponse,
        raddr: str,
        uid: str,
        peer_type: str,
        client_type: Optional[str] = None,
        client_slot: Optional[int] = None,
        client_strict_viewer: Optional[bool] = None,
    ) -> None:
        """Serve a registered peer's message loop until it disconnects.

        Relays session messages between paired partners, room messages between
        room members, and processes the SESSION/ROOM commands that establish
        those pairings (message formats in the module docstring). The peer
        was already registered atomically in ``hello_peer``.

        A session relay is accepted in either direction between partners
        (`sessions` is stored client-to-server only) and refused toward an
        unrelated peer or one not in a session. SESSION_START carries the
        caller's secure-mode token (space-free) as a trailing field, omitted
        when absent, so the server side can grant a matching viewer
        read-write collaboration. A room id may not be the "session" status
        sentinel, empty, or contain whitespace.

        Args:
            ws: The peer's WebSocket.
            raddr: Remote address, for logging.
            uid: Peer ID.
            peer_type: Either "server" or "client".
            client_type: For clients, either "viewer" or "controller".
            client_slot: Player slot number (1-4), or -1 when unassigned.
            client_strict_viewer: Whether the client is a strict viewer.
        """
        peer_status = None
        logger.info(
            f"Registered peer {uid} at {raddr} with peer_type {peer_type} and client_type {client_type}"
        )

        try:
            while True:
                msg_obj = await self.recv_msg_ping(ws, raddr)

                if msg_obj.type in [WSMsgType.CLOSE, WSMsgType.CLOSING]:
                    logger.info(f"Peer connection closed: {uid!r}")
                    break
                elif msg_obj.type == WSMsgType.ERROR:
                    logger.error("Peer Connection error")
                    raise Exception(f"Peer Connection error: {uid!r}")

                # autoping is off: answer PING here, feed PONG to the uplink gauge.
                if msg_obj.type == WSMsgType.PING:
                    await ws.pong(msg_obj.data)
                    continue
                if msg_obj.type == WSMsgType.PONG:
                    note_pong(ws, msg_obj.data)
                    continue

                if msg_obj.type != WSMsgType.TEXT:
                    logger.warning(f"Ignoring non-text message from peer {uid!r}")
                    continue
                msg = str(msg_obj.data)

                peer = self.peers.get(uid)
                if not peer:
                    logger.error(f"Peer {uid} not found in peers dict")
                    break
                peer_status = peer.peer_status
                if peer_status is not None:
                    if peer_status == "session":
                        parts = msg.split(maxsplit=1)
                        if len(parts) < 2:
                            logger.warning(f"Malformed session message from {uid}")
                            continue
                        other_id, msg_string = parts
                        other_peer = self.peers.get(other_id)
                        if not other_peer:
                            logger.warning(f"Peer {other_id} not found for session message relay")
                            continue
                        if not (
                            self.sessions.get(uid) == other_id
                            or self.sessions.get(other_id) == uid
                        ):
                            logger.warning(
                                f"Rejecting session relay {uid} -> {other_id}: not session partners"
                            )
                            continue
                        if other_peer.peer_status != "session":
                            logger.warning(
                                f"Rejecting session relay {uid} -> {other_id}: target not in a session"
                            )
                            continue
                        wso = other_peer.ws
                        logger.info("{} -> {}: {}".format(uid, other_id, msg))
                        msg_string = "{} {}".format(uid, msg_string)
                        await wso.send_str(msg_string)
                    elif peer_status:
                        if msg.startswith("ROOM_PEER_MSG"):
                            parts = msg.split(maxsplit=2)
                            if len(parts) < 3:
                                await ws.send_str("ERROR invalid ROOM_PEER_MSG format")
                                continue
                            _, other_id, msg = parts
                            other_peer = self.peers.get(other_id)
                            if not other_peer:
                                await ws.send_str(
                                    "ERROR peer {!r} not found".format(other_id)
                                )
                                continue
                            wso = other_peer.ws
                            status = other_peer.peer_status
                            room_id = peer_status
                            if status != room_id:
                                await ws.send_str(
                                    "ERROR peer {!r} is not in the room".format(other_id)
                                )
                                continue
                            msg = "ROOM_PEER_MSG {} {}".format(uid, msg)
                            logger.info(
                                "room {}: {} -> {}: {}".format(room_id, uid, other_id, msg)
                            )
                            await wso.send_str(msg)
                        else:
                            await ws.send_str("ERROR invalid msg, already in room")
                            continue
                    else:
                        raise AssertionError("Unknown peer status {!r}".format(peer_status))
                elif msg.startswith("SESSION"):
                    logger.info("{!r} command {!r}".format(uid, msg))
                    parts = msg.split(maxsplit=1)
                    if len(parts) < 2:
                        logger.warning(f"Malformed session message from {uid}")
                        continue
                    _, callee_id = parts
                    if callee_id == "server":
                        callee_id = next(
                            (
                                uid
                                for uid, pdata in self.peers.items()
                                if hasattr(pdata, "peer_type")
                                and pdata.peer_type == "server"
                            ),
                            callee_id,
                        )
                    if callee_id not in self.peers:
                        await ws.send_str("ERROR peer server not found")
                        continue
                    await ws.send_str("SESSION_OK " + str(callee_id))
                    callee_peer = self.peers.get(callee_id)
                    if not callee_peer:
                        logger.error(f"Callee peer {callee_id} not found after check")
                        continue
                    wsc = callee_peer.ws
                    wsc_raadr = callee_peer.raddr
                    logger.info(
                        "Session from {!r} ({!r}) to {!r} ({!r})".format(
                            uid, raddr, callee_id, wsc_raadr
                        )
                    )
                    session_start = "SESSION_START {} {} {} {}".format(
                        uid, client_type, peer.display_id, peer.display_position
                    )
                    if peer.client_token:
                        session_start += " " + peer.client_token
                    await wsc.send_str(session_start)
                    peer.peer_status = peer_status = "session"
                    callee_peer.peer_status = "session"
                    self.sessions[uid] = callee_id
                elif msg.startswith("ROOM"):
                    logger.info("{!r} command {!r}".format(uid, msg))
                    parts = msg.split(maxsplit=1)
                    if len(parts) < 2:
                        logger.warning(f"Malformed room message from {uid}")
                        continue
                    _, room_id = parts
                    if room_id == "session" or room_id.split() != [room_id]:
                        await ws.send_str("ERROR invalid room id {!r}".format(room_id))
                        continue
                    if room_id in self.rooms:
                        if uid in self.rooms[room_id]:
                            raise AssertionError(
                                "How did we accept a ROOM command "
                                "despite already being in a room?"
                            )
                    else:
                        self.rooms[room_id] = set()
                    room_peers = " ".join([pid for pid in self.rooms[room_id]])
                    await ws.send_str("ROOM_OK {}".format(room_peers))
                    peer.peer_status = peer_status = room_id
                    # setdefault: a concurrent cleanup_room may have deleted the room during the await.
                    room_set = self.rooms.setdefault(room_id, set())
                    room_set.add(uid)
                    # Snapshot: peers can join/leave across the awaits.
                    for pid in list(room_set):
                        if pid == uid:
                            continue
                        peer = self.peers.get(pid)
                        if not peer:
                            continue
                        wsp = peer.ws
                        msg = "ROOM_PEER_JOINED {}".format(uid)
                        logger.info("room {}: {} -> {}: {}".format(room_id, uid, pid, msg))
                        await wsp.send_str(msg)
                else:
                    logger.info("Ignoring unknown message {!r} from {!r}".format(msg, uid))
        except Exception as e:
            logger.error(f"Error at connection handler for peer {uid!r}: {e}")

    def allowed_client_slots(self) -> List[int]:
        """Return the allowed player slot numbers (1 always; 2-4 when enabled)."""
        return [1] + [i for i in range(2, 5) if getattr(self, f"enable_player{i}")]

    _EVICTION_STORM_WINDOW_S: float = 5.0
    _EVICTION_STORM_LIMIT: int = 3

    def _eviction_storm(self, key: Any) -> bool:
        """Return True when the identity ``key`` (slot/controller) has been
        taken over LIMIT+ times inside the window: two live auto-reconnecting
        pages are trading the session (each takeover closes the other
        mid-handshake). Rejecting the newest claimant breaks the loop and keeps
        the current holder stable; a lone refresh (one takeover per reload)
        never trips this."""
        now = asyncio.get_running_loop().time()
        times = [t for t in self._eviction_times.get(key, ()) if now - t < self._EVICTION_STORM_WINDOW_S]
        self._eviction_times[key] = times
        return len(times) >= self._EVICTION_STORM_LIMIT

    def _record_eviction(self, key: Any) -> None:
        self._eviction_times.setdefault(key, []).append(asyncio.get_running_loop().time())

    def _secure_token_rejected(self, client_token: Any) -> bool:
        """Secure mode (master token configured) binds streaming access to a
        server-issued token, mirroring the websockets handshake: a client peer must
        present a client_token that maps to a provisioned token. The auth middleware
        skips Basic on WS upgrades in secure mode expecting this gate, so without it
        a client peer would reach signaling unauthenticated."""
        if not app_settings.master_token:
            return False
        if not isinstance(client_token, str):
            return True
        return _lookup_session_token(client_token) is None

    def _secure_server_token_rejected(self, server_token: Any) -> bool:
        """Secure mode: the server peer owns the media graph for every client, and a
        server peer leaving closes all of them, so registering as one is an
        administrative act. Only the in-process signaling client (and anything else
        holding the master token) may claim it — the auth middleware forwards WS
        upgrades without Basic in secure mode, so this is the gate that stands between
        an unauthenticated socket and impersonating the streaming server."""
        if not app_settings.master_token:
            return False
        if not server_token or not isinstance(server_token, str):
            return True
        return not hmac.compare_digest(
            server_token.encode("utf-8"), str(app_settings.master_token).encode("utf-8")
        )

    def _secure_effective_client_type(
        self, client_type: Optional[str], client_token: Any
    ) -> Optional[str]:
        """Secure mode: the token's provisioned role is authoritative (client_type is
        self-asserted over signaling). Coerce a viewer-role token's 'controller' claim
        to viewer so it can't own a media graph, open a secondary display, or drive
        input — mirroring ws_handler's token-derived role."""
        if not app_settings.master_token or client_type != "controller":
            return client_type
        perms = _lookup_session_token(client_token) if isinstance(client_token, str) else None
        if perms and perms.get("role") == "controller":
            return client_type
        return "viewer"

    async def hello_peer(
        self, ws: WebSocketResponse, raddr: str, auth_role_ceiling: Optional[str] = None
    ) -> Tuple[str, str, Optional[str], Optional[int], Optional[bool]]:
        """Exchange the HELLO handshake, validate, and register the peer.

        Validation, eviction of superseded/dead holders, and registration all
        happen atomically under the lock; the HELLO reply and any deferred
        eviction notifications are sent after release so a slow socket can't
        serialize other handshakes. A HELLO send that fails unregisters the
        peer so a half-open peer never stays in the table.

        Identity collisions resolve newest-wins, as in websockets mode where a
        new display connection supersedes the old one: a page refresh
        reconnects before its old socket is reaped and must not bounce off
        itself, so dead holders are reaped and a live one is closed. The
        identities are the display's sole client in non-sharing mode, a
        player slot in sharing mode (`-1` is the unassigned sentinel, exempt
        from uniqueness), and a display's controller — each scoped per
        display so display2 never supersedes the primary — unless the
        takeover-storm breaker (`_eviction_storm`) rejects the claimant. A
        viewer first reaps a dead, unreaped controller and treats it as
        absent rather than pairing with a stale peer; a viewer of the primary
        display needs no controller, since the desktop exists regardless and
        the server starts its capture for a lone viewer (websockets parity),
        while a secondary display exists only through its controller's layout,
        so its viewer is refused without one. Strict shared viewers (the
        `#shared` URL hash) are refused when `enable_shared` is off.

        Args:
            ws: The connecting WebSocket.
            raddr: Remote address, for logging.
            auth_role_ceiling: Highest role the transport credential grants;
                "viewer" caps a self-asserted controller.

        Returns:
            Tuple of (peer_id, peer_type, client_type, client_slot,
            client_strict_viewer).

        Raises:
            Exception: Protocol validation failed or the peer was rejected;
                the socket is closed with a reason before raising.
        """
        # With autoping off, a PING/PONG racing the handshake is not an invalid HELLO.
        while True:
            msg_obj = await ws.receive()
            if msg_obj.type == WSMsgType.PING:
                await ws.pong(msg_obj.data)
                continue
            if msg_obj.type == WSMsgType.PONG:
                continue
            break
        if msg_obj.type != WSMsgType.TEXT or not isinstance(msg_obj.data, str):
            await ws.close(code=1002, message=b"invalid protocol")
            raise Exception("Invalid hello message type from {!r}".format(raddr))

        hello = msg_obj.data
        toks = hello.split(maxsplit=2)
        client_type = None
        client_slot = None
        client_strict_viewer = None
        client_token = None
        server_token = None
        display_id = "primary"
        display_position = "right"
        dead_peer_notifications: List[Callable[[], Awaitable[Any]]] = []

        def evict_peer_locked(
            pid: str, peer: Peer, reason: bytes, storm_key: Any = None
        ) -> None:
            """Evict a peer regardless of liveness: a live holder is superseded
            (closed) like websockets mode does for a reconnecting display; a dead
            one is just reaped. Socket I/O is deferred until the lock is released.
            Only live supersedes count toward the takeover-storm window — reaping
            an already-dead holder is routine reconnect housekeeping."""
            peer_ws = getattr(peer, "ws", None)
            if peer_ws is not None and not peer_ws.closed:
                if storm_key is not None:
                    self._record_eviction(storm_key)
                dead_peer_notifications.append(
                    lambda ws_old=peer_ws, r=reason: ws_old.close(code=4001, message=r)
                )
            dead_peer_notifications.extend(self._evict_dead_peer_locked(pid))

        result = None
        try:
            async with self.lock:
                if len(toks) > 2:
                    hello, peer_type, json_metadata_str = toks
                    try:
                        json_metadata = json.loads(json_metadata_str)
                        client_type = json_metadata.get("client_type")
                        client_slot = json_metadata.get("client_slot")
                        client_strict_viewer = json_metadata.get("client_strict_viewer")
                        client_token = json_metadata.get("client_token")
                        server_token = json_metadata.get("server_token")
                        display_id = json_metadata.get("display_id") or "primary"
                        pos = json_metadata.get("display_position")
                        display_position = pos if pos in ("right", "left", "up", "down") else "right"
                    except json.JSONDecodeError as e:
                        await ws.close(code=1002, message=b"invalid protocol")
                        raise Exception("Invalid JSON metadata from {!r}".format(raddr)) from e
                    # Coerced to int so collision checks hold (1.0 is slot 1); bool is
                    # an int subclass and is rejected rather than read as slot 1.
                    if client_slot is not None:
                        if isinstance(client_slot, bool):
                            await ws.close(code=1002, message=b"invalid protocol")
                            raise Exception(
                                "Invalid client_slot {!r} from {!r}".format(
                                    client_slot, raddr
                                )
                            )
                        try:
                            client_slot = int(client_slot)
                        except (TypeError, ValueError) as e:
                            await ws.close(code=1002, message=b"invalid protocol")
                            raise Exception(
                                "Invalid client_slot {!r} from {!r}".format(
                                    client_slot, raddr
                                )
                            ) from e
                else:
                    hello, peer_type = toks

                if hello != "HELLO":
                    await ws.close(code=1002, message=b"invalid protocol")
                    raise Exception("Invalid hello from {!r}".format(raddr))
                if peer_type not in ("server", "client"):
                    await ws.close(code=1002, message=b"invalid protocol")
                    raise Exception("Invalid peer type from {!r}".format(raddr))
                if peer_type == "client" and client_type not in ("viewer", "controller"):
                    await ws.close(code=1002, message=b"invalid protocol")
                    raise Exception("Invalid client type from {!r}".format(raddr))
                if peer_type == "client" and self._secure_token_rejected(client_token):
                    await ws.close(code=4001, message=b"Invalid authentication token")
                    raise Exception(
                        "Rejecting client from {!r}: missing or invalid token in secure mode".format(raddr)
                    )
                if peer_type == "server" and self._secure_server_token_rejected(server_token):
                    await ws.close(code=4001, message=b"Invalid authentication token")
                    raise Exception(
                        "Rejecting server peer from {!r}: missing or invalid master token in secure mode".format(raddr)
                    )

                if peer_type == "client":
                    client_type = self._secure_effective_client_type(client_type, client_token)
                    if auth_role_ceiling == "viewer" and client_type == "controller":
                        client_type = "viewer"
                    if not self.enable_sharing:
                        existing_clients = [
                            (pid, peer)
                            for pid, peer in self.peers.items()
                            if hasattr(peer, "peer_type") and peer.peer_type == "client"
                            and getattr(peer, "display_id", "primary") == display_id
                        ]
                        if existing_clients and self._eviction_storm(("client", display_id)):
                            await ws.close(
                                code=4000,
                                message=b"Session takeover loop detected; another page holds this session.",
                            )
                            raise Exception(
                                "Rejecting client from {!r}: takeover storm".format(raddr)
                            )
                        for pid, peer in existing_clients:
                            evict_peer_locked(
                                pid, peer, b"Superseded by a new connection.",
                                storm_key=("client", display_id),
                            )
                            logger.info(
                                "Evicting client {!r} for non-sharing reconnect from {!r}".format(
                                    pid, raddr
                                )
                            )
                    else:
                        allowed_slots = self.allowed_client_slots()
                        if client_slot != -1 and (client_slot not in allowed_slots):
                            await ws.close(
                                code=4000, message=b"Invalid player id provided, check URL."
                            )
                            raise Exception(
                                "Invalid client slot provided {!r}".format(client_slot)
                            )
                        if client_slot != -1:
                            colliding = [
                                (pid, peer)
                                for pid, peer in self.peers.items()
                                if getattr(peer, "client_slot", None) == client_slot
                                and getattr(peer, "display_id", "primary") == display_id
                            ]
                            if colliding and self._eviction_storm(("slot", display_id, client_slot)):
                                await ws.close(
                                    code=4000,
                                    message=b"Player slot takeover loop detected; another page holds this slot.",
                                )
                                raise Exception(
                                    "Rejecting slot {!r} claim from {!r}: takeover storm".format(
                                        client_slot, raddr
                                    )
                                )
                            for pid, peer in colliding:
                                evict_peer_locked(
                                    pid, peer,
                                    b"Superseded by a new connection for this player slot.",
                                    storm_key=("slot", display_id, client_slot),
                                )
                                logger.info(
                                    "Evicting peer {!r} holding slot {!r} for reconnect from {!r}".format(
                                        pid, client_slot, raddr
                                    )
                                )

                    if not self.enable_shared and client_strict_viewer:
                        await ws.close(
                            code=4000, message=b"Strict shared clients are not enabled."
                        )
                        raise Exception(
                            "Strict shared clients are disabled; connection from {!r}".format(
                                raddr
                            )
                        )

                    controller_entry = next(
                        (
                            (pid, peer)
                            for pid, peer in self.peers.items()
                            if hasattr(peer, "client_type")
                            and peer.client_type == "controller"
                            and getattr(peer, "display_id", "primary") == display_id
                        ),
                        None,
                    )
                    peer_controller = controller_entry[1] if controller_entry else None
                    if client_type == "controller":
                        if peer_controller is not None:
                            if self._eviction_storm(("controller", display_id)):
                                await ws.close(
                                    code=4000,
                                    message=b"Session takeover loop detected; another page holds this session.",
                                )
                                raise Exception(
                                    "Rejecting controller from {!r}: takeover storm".format(raddr)
                                )
                            assert controller_entry is not None
                            ctrl_pid, ctrl_peer = controller_entry
                            evict_peer_locked(
                                ctrl_pid, ctrl_peer,
                                b"Superseded by a new controller connection.",
                                storm_key=("controller", display_id),
                            )
                            peer_controller = None
                            logger.info(
                                "Evicting controller {!r} for reconnect from {!r}".format(
                                    ctrl_pid, raddr
                                )
                            )
                    if client_type == "viewer":
                        if peer_controller is not None:
                            assert controller_entry is not None
                            ctrl_pid, ctrl_peer = controller_entry
                            ctrl_ws = getattr(ctrl_peer, "ws", None)
                            if ctrl_ws is None or ctrl_ws.closed:
                                dead_peer_notifications.extend(
                                    self._evict_dead_peer_locked(ctrl_pid)
                                )
                                peer_controller = None
                                logger.info(
                                    "Evicting dead controller {!r} ahead of viewer from {!r}".format(
                                        ctrl_pid, raddr
                                    )
                                )
                        if not peer_controller and display_id != "primary":
                            await ws.close(
                                code=4000,
                                message=b"No controller detected. A secondary display's viewer requires that display's controller.",
                            )
                            raise Exception(
                                "No controller for display {!r}; rejecting viewer from {!r}".format(
                                    display_id, raddr
                                )
                            )

                uid = str(uuid.uuid4())
                puid = "-".join([peer_type, uid])
                self.peers[puid] = Peer(
                    uid=puid,
                    ws=ws,
                    raddr=raddr,
                    peer_type=peer_type,
                    client_type=client_type,
                    client_slot=client_slot,
                    client_strict_viewer=client_strict_viewer,
                    client_token=client_token,
                    display_id=display_id,
                    display_position=display_position,
                )
                result = (puid, peer_type, client_type, client_slot, client_strict_viewer)
        finally:
            for make_coro in dead_peer_notifications:
                try:
                    await asyncio.wait_for(make_coro(), timeout=5)
                except Exception as exc:
                    logger.debug(
                        "Deferred dead-peer notification failed/timed out: {}".format(exc)
                    )
        try:
            await ws.send_str("HELLO")
        except Exception:
            async with self.lock:
                self.peers.pop(result[0], None)
            raise
        return result

    async def signaling_handler(
        self, ws: WebSocketResponse, raddr: str, auth_role_ceiling: Optional[str] = None
    ) -> None:
        """Serve one signaling connection end to end: handshake, message loop,
        and removal on disconnect.

        Args:
            ws: The connecting WebSocket.
            raddr: Remote address, for logging.
            auth_role_ceiling: Highest role the transport-level credential
                grants ("viewer" caps a self-asserted controller in legacy
                basic-auth mode).
        """
        peer_id = None
        try:
            (
                peer_id,
                peer_type,
                client_type,
                client_slot,
                client_strict_viewer,
            ) = await self.hello_peer(ws, raddr, auth_role_ceiling)
        except Exception as e:
            logger.error(f"Error during handshake with peer {raddr}: {e}")
            return
        self._notify_client_presence()

        try:
            await self.peer_connection_handler(
                ws,
                raddr,
                peer_id,
                peer_type,
                client_type,
                client_slot,
                client_strict_viewer,
            )
        except Exception as e:
            logger.error(
                "Error in connection handler for peer {!r}: {}".format(raddr, e),
                exc_info=True,
            )
            await ws.close(code=1002, message=b"internal error")
        finally:
            if peer_id:
                await self.remove_peer(peer_id)
                self._notify_client_presence()

    def _notify_client_presence(self) -> None:
        """Report whether any browser client peer is connected to the presence
        callback; the server's own signaling peer does not count."""
        if self.on_client_presence:
            self.on_client_presence(
                any(p.peer_type == "client" for p in self.peers.values())
            )

    async def handle_turn_req(self, request: web.Request) -> web.Response:
        """GET /api/turn: serve the RTC/TURN configuration to clients.

        Who may ask is the auth middleware's call (Basic credentials, or a
        session token in secure mode); this only serves the config. It is the
        exact config the server resolved (`rtc_config`), the single source of
        truth, with no per-request credential path: the client always
        negotiates with the same ICE servers the server uses. When the HMAC
        method is active, `generate_rtc_config()` mints the username with a
        generic default so it is never a bare `<expiry>:`.

        Returns:
            The RTC configuration as JSON, or 404 when none is resolved.
        """
        path = request.path

        if self.rtc_config:
            data = self.rtc_config
            if isinstance(data, str):
                data = data.encode("utf-8")
            return web.Response(status=200, body=data, content_type="application/json")

        logger.warning("HTTP GET {} 404 NOT FOUND - Missing RTC config".format(path))
        return web.Response(status=404, text="404 NOT FOUND")
