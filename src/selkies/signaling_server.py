#!/usr/bin/env python3

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Example 1-1 call signaling server
#
#   Copyright (C) 2017 Centricular Ltd.
#
#   Author: Nirbheek Chauhan <nirbheek@centricular.com>

import os
import base64
import sys
import ssl
import logging
import asyncio
import time
import argparse
import http
import concurrent
import functools
import json
import pathlib

import hashlib
import hmac
import base64
import uuid
from dataclasses import dataclass

import websockets
import websockets.asyncio.server

logger = logging.getLogger("signaling")
web_logger = logging.getLogger("web")

# websockets logs an error if a connection is opened and closed before any data is sent.
# The client seems to do same thing, causing an inital handshake error.
logging.getLogger("websockets").setLevel(logging.CRITICAL)

MIME_TYPES = {
    "html": "text/html",
    "js": "text/javascript",
    "css": "text/css",
    "ico": "image/x-icon"
}

def generate_rtc_config(turn_host, turn_port, shared_secret, user, protocol='udp', turn_tls=False, stun_host=None, stun_port=None):
    # Use shared secret to generate HMAC credential

    # Sanitize user for credential compatibility
    user = user.replace(":", "-")

    # Credential expires in 24 hours
    expiry_hour = 24

    exp = int(time.time()) + expiry_hour * 3600
    username = "{}:{}".format(exp, user)

    # Generate HMAC credential
    hashed = hmac.new(bytes(shared_secret, "utf-8"), bytes(username, "utf-8"), hashlib.sha1).digest()
    password = base64.b64encode(hashed).decode()

    def format_ice_host(host):
        if host and ":" in host and not (host.startswith("[") and host.endswith("]")):
            return f"[{host}]"
        return host

    def append_stun_url(stun_urls, seen_stun, host, port):
        if not host:
            return
        try:
            port_num = int(port)
        except (TypeError, ValueError):
            port_num = 3478

        key = (host.lower(), port_num)
        if key in seen_stun:
            return

        seen_stun.add(key)
        stun_urls.append(f"stun:{format_ice_host(host)}:{port_num}")

    # Configure STUN servers
    stun_list = []
    seen_stun = set()
    if stun_host is not None and stun_port is not None:
        append_stun_url(stun_list, seen_stun, str(stun_host), stun_port)
    append_stun_url(stun_list, seen_stun, str(turn_host), turn_port)
    append_stun_url(stun_list, seen_stun, "stun.l.google.com", 19302)
    append_stun_url(stun_list, seen_stun, "stun.cloudflare.com", 3478)

    rtc_config = {}
    rtc_config["lifetimeDuration"] = "{}s".format(expiry_hour * 3600)
    rtc_config["blockStatus"] = "NOT_BLOCKED"
    rtc_config["iceTransportPolicy"] = "all"
    rtc_config["iceServers"] = []
    rtc_config["iceServers"].append({
        "urls": stun_list
    })
    rtc_config["iceServers"].append({
        "urls": [
            "{}:{}:{}?transport={}".format('turns' if turn_tls else 'turn', format_ice_host(str(turn_host)), turn_port, protocol)
        ],
        "username": username,
        "credential": password
    })

    return json.dumps(rtc_config, indent=2)

@dataclass
class Peer:
    uid: str
    ws: websockets.WebSocketServerProtocol
    raddr: str
    peer_type: str
    client_type: str
    client_slot: int
    client_strict_viewer: bool
    peer_status: str = None

class WebRTCSimpleServer(object):
    def __init__(self, options):
        ############### Global data ###############

        # Format: {uid: Peer_object}
        self.peers = dict()
        # Format: {caller_uid: callee_uid}
        # Unidirectional mapping
        self.sessions = dict()
        # Format: {room_id: {peer1_id, peer2_id, peer3_id, ...}}
        # Room dict with a set of peers in each room
        self.rooms = dict()

        # Websocket Server Instance
        self.server = None

        # Signal used to shutdown server
        self.stop_server = None

        # Options
        self.addr = options.addr
        self.port = options.port
        self.keepalive_timeout = options.keepalive_timeout
        self.cert_restart = options.cert_restart
        self.enable_https = options.enable_https
        self.https_cert = options.https_cert
        self.https_key = options.https_key
        self.health_path = options.health
        self.web_root = options.web_root
        self.mode = options.mode

        # Certificate mtime, used to detect when to restart the server
        self.cert_mtime = -1

        self.cache_ttl = 300
        self.http_cache = {}

        # TURN options
        self.turn_shared_secret = options.turn_shared_secret
        self.turn_host = options.turn_host
        self.turn_port = options.turn_port
        self.turn_protocol = options.turn_protocol.lower()
        if self.turn_protocol != 'tcp':
            self.turn_protocol = 'udp'
        self.turn_tls = options.turn_tls
        self.turn_auth_header_name = options.turn_auth_header_name
        self.stun_host = options.stun_host
        self.stun_port = options.stun_port

        # Basic authentication options
        self.enable_basic_auth = options.enable_basic_auth
        self.basic_auth_user = options.basic_auth_user
        self.basic_auth_password = options.basic_auth_password

        # Sharing mode options
        self.enable_sharing = options.enable_sharing
        self.enable_shared = options.enable_shared
        self.enable_player2 = options.enable_player2
        self.enable_player3 = options.enable_player3
        self.enable_player4 = options.enable_player4
        self.lock = asyncio.Lock()

        self.rtc_config = options.rtc_config
        if os.path.exists(options.rtc_config_file):
            logger.info("parsing rtc_config_file: {}".format(options.rtc_config_file))
            self.rtc_config = self.read_file(options.rtc_config_file)

        # Validate TURN arguments
        if self.turn_shared_secret:
            if not (self.turn_host and self.turn_port):
                raise Exception("missing turn_host or turn_port options with turn_shared_secret")

        # Validate basic authentication arguments
        if self.enable_basic_auth:
            if not self.basic_auth_password:
                raise Exception("missing basic_auth_password when using enable_basic_auth option.")

    ############### Helper functions ###############

    def set_rtc_config(self, rtc_config):
        self.rtc_config = rtc_config

    def read_file(self, path):
        with open(path, 'rb') as f:
            return f.read()

    async def cache_file(self, full_path):
        data, cached_at = self.http_cache.get(full_path, (None, None))
        now = time.time()
        if data is None or now - cached_at >= self.cache_ttl:
            data = await asyncio.to_thread(self.read_file, full_path)
            self.http_cache[full_path] = (data, now)
        return data

    def http_response(self, status, response_headers, body):
        # Expecting bytes, but if string, then convert to bytes
        if isinstance(body, str):
            body = body.encode()
        status = http.HTTPStatus(status)
        headers = websockets.datastructures.Headers(
            [
                ("Connection", "close"),
                ("Content-Length", str(len(body))),
                ("Content-Type", "text/plain; charset=utf-8"),
            ]
        )
        # Overriding and appending headers if provided
        for key, value in response_headers.raw_items():
            if headers.get(key) is not None:
                del headers[key]
            headers[key] = value
        return websockets.http11.Response(status.value, status.phrase, headers, body)

    async def process_request(self, server_root, connection, request):
        path = request.path
        request_headers = request.headers
        response_headers = websockets.datastructures.Headers()
        username = ''
        if self.enable_basic_auth:
            if "basic" in request_headers.get("authorization", "").lower():
                decoded_username, decoded_password = websockets.headers.parse_authorization_basic(request_headers.get("authorization"))
                if not (decoded_username == self.basic_auth_user and decoded_password == self.basic_auth_password):
                    return self.http_response(http.HTTPStatus.UNAUTHORIZED, response_headers, b'Unauthorized')
            else:
                response_headers['WWW-Authenticate'] = 'Basic realm="restricted, charset="UTF-8"'
                return self.http_response(http.HTTPStatus.UNAUTHORIZED, response_headers, b'Authorization required')

        if path == "/ws/" or path == "/ws" or path.endswith("/signaling/") or path.endswith("/signaling"):
            return None

        if path == self.health_path + "/" or path == self.health_path:
            return self.http_response(http.HTTPStatus.OK, response_headers, b"OK\n")

        if path == "/turn/" or path == "/turn":
            if self.turn_shared_secret:
                # Get username from auth header.
                if not username:
                    username = request_headers.get(self.turn_auth_header_name, "username")
                    if not username:
                        web_logger.warning("HTTP GET {} 401 Unauthorized - missing auth header: {}".format(path, self.turn_auth_header_name))
                        return self.http_response(http.HTTPStatus.UNAUTHORIZED, response_headers, b'401 Unauthorized - missing auth header')
                web_logger.info("Generating HMAC credential for user: {}".format(username))
                rtc_config = generate_rtc_config(self.turn_host, self.turn_port, self.turn_shared_secret, username, self.turn_protocol, self.turn_tls, self.stun_host, self.stun_port)
                return self.http_response(http.HTTPStatus.OK, response_headers, str.encode(rtc_config))

            elif self.rtc_config:
                data = self.rtc_config
                if type(data) == str:
                    data = str.encode(data)
                response_headers['Content-Type'] = 'application/json'
                return self.http_response(http.HTTPStatus.OK, response_headers, data)
            else:
                web_logger.warning("HTTP GET {} 404 NOT FOUND - Missing RTC config".format(path))
                return self.http_response(http.HTTPStatus.NOT_FOUND, response_headers, b'404 NOT FOUND')

        if path == "/mode" or path == "/mode/":
            data = json.dumps({"mode" : self.mode})
            response_headers['Content-Type'] = 'application/json'
            return self.http_response(http.HTTPStatus.OK, response_headers, data)

        path = path.split("?")[0]
        if path == '/':
            path = '/index.html'

        # Derive full system path
        full_path = os.path.realpath(os.path.join(server_root, path[1:]))

        # Validate the path
        if os.path.commonpath((server_root, full_path)) != server_root or \
                not os.path.exists(full_path) or not os.path.isfile(full_path):
            response_headers['Content-Type'] = 'text/html'
            web_logger.info("HTTP GET {} 404 NOT FOUND".format(path))
            return self.http_response(http.HTTPStatus.NOT_FOUND, response_headers, b'404 NOT FOUND')

        # Guess file content type
        extension = full_path.split(".")[-1]
        mime_type = MIME_TYPES.get(extension, "application/octet-stream")
        response_headers['Content-Type'] = mime_type

        # Read the whole file into memory and send it out
        body = await self.cache_file(full_path)
        response_headers['Content-Length'] = str(len(body))
        web_logger.info("HTTP GET {} 200 OK".format(path))
        return self.http_response(http.HTTPStatus.OK, response_headers, body)

    async def recv_msg_ping(self, ws, raddr):
        '''
        Wait for a message forever, and send a regular ping to prevent bad routers
        from closing the connection.
        '''
        msg = None
        while msg is None:
            try:
                msg = await asyncio.wait_for(ws.recv(), self.keepalive_timeout)
            except (asyncio.TimeoutError, concurrent.futures._base.TimeoutError):
                web_logger.info('Sending keepalive ping to {!r} in recv'.format(raddr))
                await ws.ping()
        return msg

    async def cleanup_session(self, uid):
        if uid in self.sessions:
            other_id = self.sessions[uid]
            del self.sessions[uid]
            logger.info("Cleaned up {} session, client type {!r}".format(uid, self.peers[uid].client_type))

            peer = self.peers[uid]
            # if controller closes the connection also close server side connection
            if peer.client_type == 'controller':
                if other_id in self.peers:
                    logger.info("Closing connection to {}, client type {!r}".format(other_id, self.peers[other_id].client_type))
                    other_peer = self.peers.get(other_id, None)
                    if other_peer:
                        wso = other_peer.ws
                        await wso.close()
            elif peer.client_type == 'viewer':
                # if viewer closes the connection notify the server
                if other_id in self.peers:
                    other_peer = self.peers.get(other_id, None)
                    if other_peer:
                        wso = other_peer.ws
                        msg = 'SESSION_END {} {}'.format(uid, peer.client_type)
                        logger.info("{} -> {}: {}".format(uid, other_id, msg))
                        await wso.send(msg)

    async def cleanup_room(self, uid, room_id):
        room_peers = self.rooms[room_id]
        if uid not in room_peers:
            return
        room_peers.remove(uid)
        for pid in room_peers:
            peer = self.peers[pid]
            wsp = peer.ws

            msg = 'ROOM_PEER_LEFT {}'.format(uid)
            logger.info('room {}: {} -> {}: {}'.format(room_id, uid, pid, msg))
            await wsp.send(msg)

    async def remove_peer(self, uid):
        async with self.lock:
            await self.cleanup_session(uid)

            if uid in self.peers:
                peer = self.peers[uid]
                ws = peer.ws
                raddr = peer.raddr
                peer_status = peer.peer_status
                peer_type = peer.peer_type
                client_type = peer.client_type
                if peer_status and peer_status != 'session':
                    await self.cleanup_room(uid, peer_status)
                # special case for server peer disconnection, close all client connections
                if peer_type == 'server':
                    del self.peers[uid]
                    for p in list(self.peers.values()):
                        if p.peer_type == 'client':
                            ws = p.ws
                            await ws.close(code=4000, reason="Server disconnected, closing connection.")
                else:
                    del self.peers[uid]
                    await ws.close()
                    logger.info("Disconnected from peer {!r} at {!r} of client_type {!r}".format(uid, raddr, client_type))

    ############### Handler functions ###############

    async def connection_handler(self, ws, uid, peer_type, client_type=None, client_slot=None, client_strict_viewer=None):
        raddr = ws.remote_address
        peer_status = None
        self.peers[uid] = Peer(uid=uid, ws=ws, raddr=raddr, peer_type=peer_type, client_type=client_type, client_slot=client_slot, client_strict_viewer=client_strict_viewer)
        logger.info("Registered peer {!r} at {!r} with peer_type {!r} and client_type {!r}".format(uid, raddr, peer_type, client_type))
        while True:
            # Receive command, wait forever if necessary
            msg = await self.recv_msg_ping(ws, raddr)
            # Update current status
            peer = self.peers[uid]
            peer_status = peer.peer_status
            # We are in a session or a room, messages must be relayed
            if peer_status is not None:
                # We're in a session, route message to connected peer
                if peer_status == 'session':
                    # Session message are prefixed with sender peer_id, eg: client-<UUID> <message>
                    other_id, msg_string = msg.split(maxsplit=1)
                    other_peer = self.peers[other_id]
                    wso = other_peer.ws
                    status = other_peer.peer_status
                    assert(status == 'session')
                    logger.info("{} -> {}: {}".format(uid, other_id, msg))
                    msg_string = '{} {}'.format(uid, msg_string)
                    await wso.send(msg_string)
                # We're in a room, accept room-specific commands
                elif peer_status:
                    # ROOM_PEER_MSG peer_id MSG
                    if msg.startswith('ROOM_PEER_MSG'):
                        _, other_id, msg = msg.split(maxsplit=2)
                        if other_id not in self.peers:
                            await ws.send('ERROR peer {!r} not found'
                                          ''.format(other_id))
                            continue
                        other_peer = self.peers[other_id]
                        wso = other_peer.ws
                        status = other_peer.peer_status
                        room_id = peer_status
                        if status != room_id:
                            await ws.send('ERROR peer {!r} is not in the room'
                                          ''.format(other_id))
                            continue
                        msg = 'ROOM_PEER_MSG {} {}'.format(uid, msg)
                        logger.info('room {}: {} -> {}: {}'.format(room_id, uid, other_id, msg))
                        await wso.send(msg)
                    else:
                        await ws.send('ERROR invalid msg, already in room')
                        continue
                else:
                    raise AssertionError('Unknown peer status {!r}'.format(peer_status))
            # Requested a session with a specific peer
            elif msg.startswith('SESSION'):
                logger.info("{!r} command {!r}".format(uid, msg))
                _, callee_id = msg.split(maxsplit=1)
                if callee_id == 'server':
                    callee_id = next((uid for uid, pdata in self.peers.items() if hasattr(pdata, 'peer_type') and pdata.peer_type == 'server'), callee_id)
                if callee_id not in self.peers:
                    await ws.send('ERROR peer server not found')
                    continue
                if peer_status is not None:
                    await ws.send('ERROR peer {!r} busy'.format(callee_id))
                    continue
                await ws.send('SESSION_OK ' + str(callee_id))
                wsc = self.peers[callee_id].ws
                logger.info('Session from {!r} ({!r}) to {!r} ({!r})'
                      ''.format(uid, raddr, callee_id, wsc.remote_address))
                # Notify callee. callee is always server
                await wsc.send('SESSION_START {} {}'.format(uid, client_type))
                # Register session
                self.peers[uid].peer_status = peer_status = 'session'
                self.peers[callee_id].peer_status = 'session'
                # We only store session between caller and callee, where caller is
                # always of peer_type 'client' and callee is 'server', so we can handle
                # termination of sessions clearly if client disconnects.
                self.sessions[uid] = callee_id
            # Requested joining or creation of a room
            elif msg.startswith('ROOM'):
                logger.info('{!r} command {!r}'.format(uid, msg))
                _, room_id = msg.split(maxsplit=1)
                # Room name cannot be 'session', empty, or contain whitespace
                if room_id == 'session' or room_id.split() != [room_id]:
                    await ws.send('ERROR invalid room id {!r}'.format(room_id))
                    continue
                if room_id in self.rooms:
                    if uid in self.rooms[room_id]:
                        raise AssertionError('How did we accept a ROOM command '
                                             'despite already being in a room?')
                else:
                    # Create room if required
                    self.rooms[room_id] = set()
                room_peers = ' '.join([pid for pid in self.rooms[room_id]])
                await ws.send('ROOM_OK {}'.format(room_peers))
                # Enter room
                self.peers[uid].peer_status = peer_status = room_id
                self.rooms[room_id].add(uid)
                for pid in self.rooms[room_id]:
                    if pid == uid:
                        continue
                    peer = self.peers[pid]
                    wsp = peer.ws
                    msg = 'ROOM_PEER_JOINED {}'.format(uid)
                    logger.info('room {}: {} -> {}: {}'.format(room_id, uid, pid, msg))
                    await wsp.send(msg)
            else:
                logger.info('Ignoring unknown message {!r} from {!r}'.format(msg, uid))

    def allowed_client_slots(self):
        return [1] + [i for i in range(2, 5) if getattr(self, f'enable_player{i}')]

    async def hello_peer(self, ws):
        '''
        Exchange hello, register peer
        '''
        raddr = ws.remote_address
        hello = await ws.recv()
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
                    client_type = json_metadata.get('client_type')
                    client_slot = json_metadata.get('client_slot')
                    client_strict_viewer = json_metadata.get('client_strict_viewer')
                except json.JSONDecodeError:
                    await ws.close(code=1002, reason='invalid protocol')
                    raise Exception("Invalid JSON metadata from {!r}".format(raddr))
            else:
                hello, peer_type = toks

            if hello != 'HELLO':
                await ws.close(code=1002, reason='invalid protocol')
                raise Exception("Invalid hello from {!r}".format(raddr))
            if peer_type not in ('server', 'client'):
                await ws.close(code=1002, reason='invalid protocol')
                raise Exception("Invalid peer type from {!r}".format(raddr))
            if peer_type == 'client' and client_type not in ('viewer', 'controller'):
                await ws.close(code=1002, reason='invalid protocol')
                raise Exception("Invalid client type from {!r}".format(raddr))

            if peer_type == 'client':
                if not self.enable_sharing:
                    client_exists = next((peer for peer in self.peers.values() if hasattr(peer, 'peer_type') and peer.peer_type == "client"), None)
                    if client_exists:
                        logger.info("peer: {}".format(self.peers))
                        await ws.close(code=4000, reason="Multiple connections not allowed in non-sharing mode.")
                        raise Exception("Multiple connections not allowed in non-sharing mode; connection from {!r}".format(raddr))
                else:
                    allowed_slots = self.allowed_client_slots()
                    if client_slot != -1 and (client_slot not in allowed_slots):
                        await ws.close(code=4000, reason="Invalid player id provided, check URL.")
                        raise Exception("Invalid client slot provided {!r}".format(client_slot))

                # clients with hash #shared
                if not self.enable_shared and client_strict_viewer:
                    await ws.close(code=4000, reason="Strict shared clients are not enabled.")
                    raise Exception("Strict shared clients are disabled; connection from {!r}".format(raddr))

                peer_controller = next((peer for peer in self.peers.values() if hasattr(peer, 'client_type') and peer.client_type == 'controller'), None)
                if client_type == 'controller':
                    if peer_controller:
                        await ws.close(code=4000, reason="Duplicate controller. A client of type 'controller' already exists.")
                        raise Exception("Duplicate controllers not allowed; connection from {!r}".format(raddr))
                if client_type == 'viewer':
                    if not peer_controller:
                        await ws.close(code=4000, reason="No controller detected. Viewer clients require an existing controller client.")
                        raise Exception("No controller detected for client of type 'viewer'; connection from {!r}".format(raddr))

            # Generate unique peer ID
            uid = str(uuid.uuid4())
            puid = "-".join([peer_type, uid])
            # Send back a HELLO
            await ws.send('HELLO')
            return puid, peer_type, client_type, client_slot, client_strict_viewer

    def get_https_certs(self):
        cert_pem = os.path.abspath(self.https_cert) if os.path.isfile(self.https_cert) else None
        key_pem = os.path.abspath(self.https_key) if os.path.isfile(self.https_key) else None
        return cert_pem, key_pem

    def get_ssl_ctx(self, https_server=True):
        if not self.enable_https:
            return None
        # Create an SSL context to be used by the websocket server
        cert_pem, key_pem = self.get_https_certs()
        logger.info('Using TLS with provided certificate and private key from arguments')
        ssl_purpose = ssl.Purpose.CLIENT_AUTH if https_server else ssl.Purpose.SERVER_AUTH
        sslctx = ssl.create_default_context(purpose=ssl_purpose)
        sslctx.check_hostname = False
        sslctx.verify_mode = ssl.CERT_NONE
        try:
            sslctx.load_cert_chain(cert_pem, keyfile=key_pem)
        except Exception:
            logger.error('Certificate or private key file not found or incorrect. To use a self-signed certificate, install the package \'ssl-cert\' and add the group \'ssl-cert\' to your user in Debian-based distributions or generate a new certificate with root using \'openssl req -x509 -newkey rsa:4096 -keyout /etc/ssl/private/ssl-cert-snakeoil.key -out /etc/ssl/certs/ssl-cert-snakeoil.pem -days 3650 -nodes -subj "/CN=localhost"\'')
            sys.exit(1)
        return sslctx

    async def run(self):
        async def handler(ws):
            '''
            All incoming messages are handled here. @path is unused.
            '''
            raddr = ws.remote_address
            logger.info("Connected to {!r}".format(raddr))
            try:
                # peer_id is either 'server-<UUID>' or 'client-<UUID>'
                peer_id, peer_type, client_type, client_slot, client_strict_viewer = await self.hello_peer(ws)
            except Exception as e:
                logger.error("Error during handshake with peer {!r}: {}".format(raddr, e))
                return
            try:
                await self.connection_handler(ws, peer_id, peer_type, client_type, client_slot, client_strict_viewer)
            except websockets.exceptions.ConnectionClosed:
                logger.info("Connection to peer {!r} closed, exiting handler".format(raddr))
            except Exception as e:
                logger.error("Error in connection handler for peer {!r}: {}".format(raddr, e))
                await ws.close(code=1002, reason='internal error')
            finally:
                await self.remove_peer(peer_id)

        # Initial cache of web_root files
        await asyncio.gather(*[self.cache_file(os.path.realpath(f)) for f in pathlib.Path(self.web_root).rglob('*.*')])

        sslctx = self.get_ssl_ctx(https_server=True)

        # Setup logging
        logger.setLevel(logging.INFO)
        web_logger.setLevel(logging.WARN)

        http_protocol = 'https:' if self.enable_https else 'http:'
        logger.info("Listening on {}//{}:{}".format(http_protocol, self.addr, self.port))
        # Websocket and HTTP server
        http_handler = functools.partial(self.process_request, self.web_root)
        self.stop_server = asyncio.Future()
        async with websockets.asyncio.server.serve(handler, self.addr, self.port, ssl=sslctx, process_request=http_handler,
                               # Maximum number of messages that websockets will pop
                               # off the asyncio and OS buffers per connection. See:
                               # https://websockets.readthedocs.io/en/stable/api.html#websockets.protocol.WebSocketCommonProtocol
                               max_queue=16) as self.server:
            await self.stop_server

        if self.enable_https:
            asyncio.create_task(self.check_server_needs_restart())

    async def stop(self):
        logger.info('Stopping server... ')
        if self.stop_server and not self.stop_server.done():
            self.stop_server.set_result(True)
        if self.server is not None:
            self.server.close()
            await self.server.wait_closed()
        logger.info('Stopped.')

    def check_cert_changed(self):
        cert_pem, key_pem = self.get_https_certs()
        mtime = max(os.stat(key_pem).st_mtime, os.stat(cert_pem).st_mtime)
        if self.cert_mtime < 0:
            self.cert_mtime = mtime
            return False
        if mtime > self.cert_mtime:
            self.cert_mtime = mtime
            return True
        return False

    async def check_server_needs_restart(self):
        '''
        When the certificate changes, we need to restart the server
        '''
        while self.cert_restart:
            await asyncio.sleep(1.0)
            if self.check_cert_changed():
                logger.info('Certificate changed, stopping server...')
                await self.stop()
                return

def entrypoint():
    default_web_root = os.path.join(os.getcwd(), "../../addons/selkies-web-core/src")

    parser = argparse.ArgumentParser(formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    parser.add_argument('--addr', default='', help='Address to listen on (default: all interfaces, both ipv4 and ipv6)')
    parser.add_argument('--port', default=8443, type=int, help='Port to listen on')
    parser.add_argument('--web_root', default=default_web_root, type=str, help='Path to web root')
    parser.add_argument('--rtc_config_file', default="/tmp/rtc.json", type=str, help='Path to JSON RTC config file')
    parser.add_argument('--rtc_config', default="", type=str, help='JSON RTC config data')
    parser.add_argument('--turn_shared_secret', default="", type=str, help='shared secret for generating TURN HMAC credentials')
    parser.add_argument('--turn_host', default="", type=str, help='TURN host when generating RTC config with shared secret')
    parser.add_argument('--turn_port', default="", type=str, help='TURN port when generating RTC config with shared secret')
    parser.add_argument('--turn_protocol', default="udp", type=str, help='TURN protocol to use ("udp" or "tcp"), set to "tcp" without the quotes if "udp" is blocked on the network')
    parser.add_argument('--enable_turn_tls', default=False, dest='turn_tls', action='store_true', help='enable TURN over TLS (for the TCP protocol) or TURN over DTLS (for the UDP protocol), valid TURN server certificate required')
    parser.add_argument('--turn_auth_header_name', default="x-auth-user", type=str, help='auth header for TURN REST username')
    parser.add_argument('--stun_host', default="stun.l.google.com", type=str, help='STUN host for WebRTC hole punching')
    parser.add_argument('--stun_port', default="19302", type=str, help='STUN port for WebRTC hole punching')
    parser.add_argument('--keepalive_timeout', dest='keepalive_timeout', default=30, type=int, help='Timeout for keepalive (in seconds)')
    parser.add_argument('--enable_https', default=False, help='Enable HTTPS connection', action='store_true')
    parser.add_argument('--https_cert', default="/etc/ssl/certs/ssl-cert-snakeoil.pem", type=str, help='HTTPS certificate file path')
    parser.add_argument('--https_key', default="/etc/ssl/private/ssl-cert-snakeoil.key", type=str, help='HTTPS private key file path, set to an empty string if the private key is included in the certificate')
    parser.add_argument('--health', default='/health', help='Health check route')
    parser.add_argument('--restart_on_cert_change', default=False, dest='cert_restart', action='store_true', help='Automatically restart if the HTTPS certificate changes')
    parser.add_argument('--enable_basic_auth', default=False, dest='enable_basic_auth', action='store_true', help="Use basic authentication, must also set basic_auth_user, and basic_auth_password arguments")
    parser.add_argument('--basic_auth_user', default="", help='Username for basic authentication')
    parser.add_argument('--basic_auth_password', default="", help='Password for basic authentication, if not set, no authorization will be enforced')

    options = parser.parse_args(sys.argv[1:])

    r = WebRTCSimpleServer(options)

    print('Starting server...')
    asyncio.run(r.run())

if __name__ == "__main__":
    entrypoint()
