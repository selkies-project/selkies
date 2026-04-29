# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import os
import json
import uuid
import logging
import asyncio
import concurrent.futures
from dataclasses import dataclass
from aiohttp.web_ws import WebSocketResponse
from aiohttp import web, WSMessage, WSMsgType
from typing import Dict, Set, Optional, Any, Tuple, List

from .webrtc_utils import generate_rtc_config

logger = logging.getLogger("signaling")


@dataclass
class Peer:
    """Represents a connected WebSocket peer."""

    uid: str
    ws: WebSocketResponse
    raddr: str
    peer_type: str
    client_type: Optional[str]
    client_slot: Optional[int]
    client_strict_viewer: Optional[bool]
    peer_status: Optional[str] = None


class WebRTCPeerManagement:
    """Manages WebRTC peer connections and signaling."""

    def __init__(self, options: Any):
        # Format: {uid: Peer_object}
        self.peers: Dict[str, Peer] = {}
        # Format: {caller_uid: callee_uid}
        # Unidirectional mapping
        self.sessions: Dict[str, str] = {}
        # Format: {room_id: {peer1_id, peer2_id, peer3_id, ...}}
        # Room dict with a set of peers in each room
        self.rooms: Dict[str, Set[str]] = {}

        # Options
        self.keepalive_timeout: int = options.keepalive_timeout

        # TURN options
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

        # Sharing mode options
        self.enable_sharing: bool = options.enable_sharing
        self.enable_shared: bool = options.enable_shared
        self.enable_player2: bool = options.enable_player2
        self.enable_player3: bool = options.enable_player3
        self.enable_player4: bool = options.enable_player4
        self.lock: asyncio.Lock = asyncio.Lock()

        # RTC config - can be str or dict
        self.rtc_config: Optional[Any] = None
        if os.path.exists(options.rtc_config_file):
            logger.info("parsing rtc_config_file: {}".format(options.rtc_config_file))
            self.rtc_config = self.read_file(options.rtc_config_file)

        # Validate TURN arguments
        if self.turn_shared_secret:
            if not (self.turn_host and self.turn_port):
                raise Exception(
                    "missing turn_host or turn_port options with turn_shared_secret"
                )

    def set_rtc_config(self, rtc_config: Any) -> None:
        """Set the RTC configuration.

        Args:
            rtc_config: RTC configuration as JSON string or dict
        """
        self.rtc_config = rtc_config

    async def recv_msg_ping(
        self, ws: WebSocketResponse, raddr: Tuple[str, int]
    ) -> WSMessage:
        """Wait for a message, sending periodic pings to prevent connection timeout.
        Args:
            ws: aiohttp WebSocketResponse
            raddr: Remote address tuple (host, port)

        Returns:
            Received message or None
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
        Args:
            uid: Peer ID to clean up
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
            # if controller closes the connection also close server side connection
            if peer.client_type == "controller":
                other_peer = self.peers.get(other_id)
                if other_peer:
                    logger.info(
                        "Closing connection to {}, client type {!r}".format(
                            other_id, other_peer.client_type
                        )
                    )
                    wso: WebSocketResponse = other_peer.ws
                    await wso.close(code=1000, message=b"Connection closed")
            elif peer.client_type == "viewer":
                # if viewer closes the connection notify the server
                other_peer = self.peers.get(other_id)
                if other_peer:
                    wso = other_peer.ws
                    msg: str = "SESSION_END {} {}".format(uid, peer.client_type)
                    logger.info("{} -> {}: {}".format(uid, other_id, msg))
                    await wso.send_str(msg)

    async def cleanup_room(self, uid: str, room_id: str) -> None:
        """Clean up a peer from a room.
        Args:
            uid: Peer ID to remove
            room_id: Room ID to remove from
        """
        room_peers: Set[str] = self.rooms[room_id]
        if uid not in room_peers:
            return
        room_peers.remove(uid)
        for pid in room_peers:
            peer = self.peers.get(pid)
            if not peer:
                continue
            wsp = peer.ws

            msg = "ROOM_PEER_LEFT {}".format(uid)
            logger.info("room {}: {} -> {}: {}".format(room_id, uid, pid, msg))
            await wsp.send_str(msg)

    async def remove_peer(self, uid: str) -> None:
        """Remove a peer and clean up associated resources.
        Args:
            uid: Peer ID to remove
        """
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
                # special case for server peer disconnection, close all client connections
                if peer_type == "server":
                    del self.peers[uid]
                    for p in list(self.peers.values()):
                        if p.peer_type == "client":
                            ws = p.ws
                            await ws.close(
                                code=4000,
                                message=b"Server disconnected, closing connection.",
                            )
                else:
                    del self.peers[uid]
                    await ws.close(code=1000, message=b"Connection closed")
                    logger.info(
                        "Disconnected from peer {!r} at {!r} of client_type {!r}".format(
                            uid, raddr, client_type
                        )
                    )

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
        """Handle WebSocket connection for a peer.
        Args:
            ws: aiohttp WebSocketResponse
            raddr: Remote address tuple (host, port)
            uid: Peer ID
            peer_type: Either 'server' or 'client'
            client_type: For clients, either 'viewer' or 'controller'
            client_slot: Player slot number (1-4)
            client_strict_viewer: Whether client is strict viewer
        """
        peer_status = None
        self.peers[uid] = Peer(
            uid=uid,
            ws=ws,
            raddr=raddr,
            peer_type=peer_type,
            client_type=client_type,
            client_slot=client_slot,
            client_strict_viewer=client_strict_viewer,
        )
        logger.info(
            f"Registered peer {uid} at {raddr} with peer_type {peer_type} and client_type {client_type}"
        )

        try:
            while True:
                # Receive command, wait forever if necessary
                msg_obj = await self.recv_msg_ping(ws, raddr)

                if msg_obj.type in [WSMsgType.CLOSE, WSMsgType.CLOSING]:
                    logger.info(f"Peer connection closed: {uid!r}")
                    break
                elif msg_obj.type == WSMsgType.ERROR:
                    logger.error("Peer Connection error")
                    raise Exception(f"Peer Connection error: {uid!r}")
                
                if msg_obj.type != WSMsgType.TEXT:
                    logger.warning(f"Ignoring non-text message from peer {uid!r}")
                    continue
                msg = str(msg_obj.data)

                # Update current status
                peer = self.peers.get(uid)
                if not peer:
                    logger.error(f"Peer {uid} not found in peers dict")
                    break
                peer_status = peer.peer_status
                # We are in a session or a room, messages must be relayed
                if peer_status is not None:
                    # We're in a session, route message to connected peer
                    if peer_status == "session":
                        parts = msg.split(maxsplit=1)
                        if len(parts) < 2:
                            logger.warning(f"Malformed session message from {uid}")
                            continue
                        # Session message are prefixed with sender peer_id
                        # Eg., client-<UUID> <message>
                        other_id, msg_string = parts
                        other_peer = self.peers.get(other_id)
                        if not other_peer:
                            logger.warning(f"Peer {other_id} not found for session message relay")
                            continue
                        wso = other_peer.ws
                        status = other_peer.peer_status
                        assert status == "session"
                        logger.info("{} -> {}: {}".format(uid, other_id, msg))
                        msg_string = "{} {}".format(uid, msg_string)
                        await wso.send_str(msg_string)
                    # We're in a room, accept room-specific commands
                    elif peer_status:
                        # ROOM_PEER_MSG peer_id MSG
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
                # Requested a session with a specific peer
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
                    if peer_status is not None:
                        await ws.send_str("ERROR peer {!r} busy".format(callee_id))
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
                    # Notify callee. callee is always server
                    await wsc.send_str("SESSION_START {} {}".format(uid, client_type))
                    # Register session
                    peer.peer_status = peer_status = "session"
                    callee_peer.peer_status = "session"
                    # We only store session between caller and callee, where caller is
                    # always of peer_type 'client' and callee is 'server', so we can handle
                    # termination of sessions clearly if client disconnects.
                    self.sessions[uid] = callee_id
                # Requested joining or creation of a room
                elif msg.startswith("ROOM"):
                    logger.info("{!r} command {!r}".format(uid, msg))
                    parts = msg.split(maxsplit=1)
                    if len(parts) < 2:
                        logger.warning(f"Malformed room message from {uid}")
                        continue
                    _, room_id = parts
                    # Room name cannot be 'session', empty, or contain whitespace
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
                        # Create room if required
                        self.rooms[room_id] = set()
                    room_peers = " ".join([pid for pid in self.rooms[room_id]])
                    await ws.send_str("ROOM_OK {}".format(room_peers))
                    # Enter room
                    peer.peer_status = peer_status = room_id
                    self.rooms[room_id].add(uid)
                    for pid in self.rooms[room_id]:
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
        """Get list of allowed client slot numbers.
        Returns:
            List of allowed player slot numbers (1-4)
        """
        return [1] + [i for i in range(2, 5) if getattr(self, f"enable_player{i}")]

    async def hello_peer(
        self, ws: WebSocketResponse, raddr: str
    ) -> Tuple[str, str, Optional[str], Optional[int], Optional[bool]]:
        """Exchange hello message and register peer.
        Args:
            ws: aiohttp WebSocketResponse

        Returns:
            Tuple of (peer_id, peer_type, client_type, client_slot, client_strict_viewer)

        Raises:
            Exception: If protocol validation fails
        """
        msg_obj = await ws.receive()
        if msg_obj.type != WSMsgType.TEXT or not isinstance(msg_obj.data, str):
            await ws.close(code=1002, message=b"invalid protocol")
            raise Exception("Invalid hello message type from {!r}".format(raddr))

        hello = msg_obj.data
        toks = hello.split(maxsplit=2)
        client_type = None
        client_slot = None
        client_strict_viewer = None
        async with self.lock:
            if len(toks) > 2:
                # peer_type is either 'server' or 'client'
                # client_type is either 'viewer' or 'controller'
                hello, peer_type, json_metadata_str = toks
                try:
                    json_metadata = json.loads(json_metadata_str)
                    client_type = json_metadata.get("client_type")
                    client_slot = json_metadata.get("client_slot")
                    client_strict_viewer = json_metadata.get("client_strict_viewer")
                except json.JSONDecodeError:
                    await ws.close(code=1002, message=b"invalid protocol")
                    raise Exception("Invalid JSON metadata from {!r}".format(raddr))
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

            if peer_type == "client":
                if not self.enable_sharing:
                    client_exists = next(
                        (
                            peer
                            for peer in self.peers.values()
                            if hasattr(peer, "peer_type") and peer.peer_type == "client"
                        ),
                        None,
                    )
                    if client_exists:
                        logger.info("peer: {}".format(self.peers))
                        await ws.close(
                            code=4000,
                            message=b"Multiple connections not allowed in non-sharing mode.",
                        )
                        raise Exception(
                            "Multiple connections not allowed in non-sharing mode; connection from {!r}".format(
                                raddr
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

                # clients with hash #shared
                if not self.enable_shared and client_strict_viewer:
                    await ws.close(
                        code=4000, message=b"Strict shared clients are not enabled."
                    )
                    raise Exception(
                        "Strict shared clients are disabled; connection from {!r}".format(
                            raddr
                        )
                    )

                peer_controller = next(
                    (
                        peer
                        for peer in self.peers.values()
                        if hasattr(peer, "client_type")
                        and peer.client_type == "controller"
                    ),
                    None,
                )
                if client_type == "controller":
                    if peer_controller:
                        await ws.close(
                            code=4000,
                            message=b"Duplicate controller. A client of type 'controller' already exists.",
                        )
                        raise Exception(
                            "Duplicate controllers not allowed; connection from {!r}".format(
                                raddr
                            )
                        )
                if client_type == "viewer":
                    if not peer_controller:
                        await ws.close(
                            code=4000,
                            message=b"No controller detected. Viewer clients require an existing controller client.",
                        )
                        raise Exception(
                            "No controller detected for client of type 'viewer'; connection from {!r}".format(
                                raddr
                            )
                        )

            # Generate unique peer ID
            uid = str(uuid.uuid4())
            puid = "-".join([peer_type, uid])
            # Send back a HELLO
            await ws.send_str("HELLO")
            return puid, peer_type, client_type, client_slot, client_strict_viewer

    async def signaling_handler(self, ws: WebSocketResponse, raddr: str) -> None:
        """Signaling handler to manage the peers connected for signaling purposes.
        Args:
            ws: aiohttp WebSocketResponse
        """
        peer_id = None
        try:
            (
                peer_id,
                peer_type,
                client_type,
                client_slot,
                client_strict_viewer,
            ) = await self.hello_peer(ws, raddr)
        except Exception as e:
            logger.error(f"Error during handshake with peer {raddr}: {e}")
            return

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

    async def handle_turn_req(self, request: web.Request) -> web.Response:
        """
        Handle TURN/RTC configuration requests.
        Args:
            request: aiohttp web.Request object

        Returns:
            web.Response with RTC/TURN configuration JSON
        """
        path = request.path
        request_headers = request.headers

        if self.turn_shared_secret:
            # Get username from auth header.
            username = request_headers.get(self.turn_auth_header_name, "")
            if not username:
                logger.warning(
                    f"HTTP GET {path} 401 - missing auth header: {self.turn_auth_header_name}. "
                    f"Using a default username"
                )

            logger.info("Generating HMAC credential for user: {}".format(username))
            rtc_config = generate_rtc_config(
                self.turn_host,
                self.turn_port,
                self.turn_shared_secret,
                username,
                self.turn_protocol,
                self.turn_tls,
                self.stun_host,
                self.stun_port,
            )
            return web.Response(
                status=200,
                body=rtc_config.encode("utf-8"),
                content_type="application/json",
            )
        elif self.rtc_config:
            data = self.rtc_config
            if isinstance(data, str):
                data = data.encode("utf-8")
            return web.Response(status=200, body=data, content_type="application/json")
        else:
            logger.warning(
                "HTTP GET {} 404 NOT FOUND - Missing RTC config".format(path)
            )
            return web.Response(status=404, text="404 NOT FOUND")
