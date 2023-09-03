#!/usr/bin/env python3

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Example 1-1 call signalling server
#
#   Copyright (C) 2017 Centricular Ltd.
#
#   Author: Nirbheek Chauhan <nirbheek@centricular.com>

import os
import sys
import ssl
import logging
import asyncio
import websockets
import basicauth
import time
import argparse
import http
import concurrent
import functools
import json

from hashlib import sha1
import hmac
import base64

from pathlib import Path
from http import HTTPStatus

logger = logging.getLogger("signaling")
web_logger = logging.getLogger("web")

MIME_TYPES = {
    "html": "text/html",
    "js": "text/javascript",
    "css": "text/css",
    "ico": "image/x-icon"
}

def generate_rtc_config(turn_host, turn_port, shared_secret, user, protocol='udp', turn_tls=False):
    # use shared secret to generate hmac credential.

    # Sanitize user for credential compatibility
    user = user.replace(":", "-")

    # credential expires in 24hrs
    exp = int(time.time()) + 24*3600
    username = "{}-{}".format(exp, user)

    # Generate HMAC credential.
    key = bytes(shared_secret, "ascii")
    raw = bytes(username, "ascii")
    hashed = hmac.new(key, raw, sha1)
    password = base64.b64encode(hashed.digest()).decode()

    rtc_config = {}
    rtc_config["lifetimeDuration"] = "{}s".format(24 * 3600)
    rtc_config["blockStatus"] = "NOT_BLOCKED"
    rtc_config["iceTransportPolicy"] = "all"
    rtc_config["iceServers"] = []
    rtc_config["iceServers"].append({
        "urls": [
            "stun:{}:{}".format(turn_host, turn_port)
        ]
    })
    rtc_config["iceServers"].append({
        "urls": [
            "{}:{}:{}?transport={}".format('turns' if turn_tls else 'turn', turn_host, turn_port, protocol)
        ],
        "username": username,
        "credential": password
    })

    return json.dumps(rtc_config, indent=2)

class WebRTCSimpleServer(object):

    def __init__(self, loop, options):
        ############### Global data ###############

        # Format: {uid: (Peer WebSocketServerProtocol,
        #                remote_address,
        #                <'session'|room_id|None>)}
        self.peers = dict()
        # Format: {caller_uid: callee_uid,
        #          callee_uid: caller_uid}
        # Bidirectional mapping between the two peers
        self.sessions = dict()
        # Format: {room_id: {peer1_id, peer2_id, peer3_id, ...}}
        # Room dict with a set of peers in each room
        self.rooms = dict()

        # Event loop
        self.loop = loop
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

        # Certificate mtime, used to detect when to restart the server
        self.cert_mtime = -1

        self.cache_ttl = 60
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

        # Basic authentication options
        self.enable_basic_auth = options.enable_basic_auth
        self.basic_auth_user = options.basic_auth_user
        self.basic_auth_password = options.basic_auth_password

        self.rtc_config = options.rtc_config
        if os.path.exists(options.rtc_config_file):
            logger.info("parsing rtc_config_file: {}".format(options.rtc_config_file))
            self.rtc_config = open(options.rtc_config_file, 'rb').read()

        # Perform initial cache of web_root files
        for f in Path(self.web_root).rglob('*.*'):
            self.cache_file(f)

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

    def cache_file(self, full_path):
        data, ttl = self.http_cache.get(full_path, (None, None))
        now = time.time()
        if data is None or now - ttl >= self.cache_ttl:
            # refresh cache
            data = open(full_path, 'rb').read()
            self.http_cache[full_path] = (data, now)
        return data

    async def process_request(self, server_root, path, request_headers):
        response_headers = [
            ('Server', 'asyncio websocket server'),
            ('Connection', 'close'),
        ]

        username = ''
        if self.enable_basic_auth:
            if "basic" in request_headers.get("authorization", "").lower():
                username, passwd = basicauth.decode(request_headers.get("authorization"))
                if not (username == self.basic_auth_user and passwd == self.basic_auth_password):
                    return http.HTTPStatus.UNAUTHORIZED, response_headers, b'Unauthorized'
            else:
                response_headers.append(('WWW-Authenticate', 'Basic realm="restricted, charset="UTF-8"'))
                return http.HTTPStatus.UNAUTHORIZED, response_headers, b'Authorization required'

        if path == "/ws" or path.endswith("/signalling/"):
            return None

        if path == self.health_path:
            return http.HTTPStatus.OK, response_headers, b"OK\n"

        if path == '/turn/':
            if self.turn_shared_secret:
                # Get username from auth header.
                if not username:
                    username = request_headers.get(self.turn_auth_header_name, "")
                    if not username:
                        web_logger.warning("HTTP GET {} 401 Unauthorized - missing auth header: {}".format(path, self.turn_auth_header_name))
                        return HTTPStatus.UNAUTHORIZED, response_headers, b'401 Unauthorized - missing auth header'
                web_logger.info("Generating HMAC credential for user: {}".format(username))
                rtc_config = generate_rtc_config(self.turn_host, self.turn_port, self.turn_shared_secret, username, self.turn_protocol, self.turn_tls)
                return http.HTTPStatus.OK, response_headers, str.encode(rtc_config)

            elif self.rtc_config:
                data = self.rtc_config
                if type(data) == str:
                    data = str.encode(data)
                response_headers.append(('Content-Type', 'application/json'))
                return http.HTTPStatus.OK, response_headers, data
            else:
                web_logger.warning("HTTP GET {} 404 NOT FOUND - Missing RTC config".format(path))
                return HTTPStatus.NOT_FOUND, response_headers, b'404 NOT FOUND'
        
        path = path.split("?")[0]
        if path == '/':
            path = '/index.html'

        # Derive full system path
        full_path = os.path.realpath(os.path.join(server_root, path[1:]))

        # Validate the path
        if os.path.commonpath((server_root, full_path)) != server_root or \
                not os.path.exists(full_path) or not os.path.isfile(full_path):
            response_headers.append(('Content-Type', 'text/html'))
            web_logger.info("HTTP GET {} 404 NOT FOUND".format(path))
            return HTTPStatus.NOT_FOUND, response_headers, b'404 NOT FOUND'

        # Guess file content type
        extension = full_path.split(".")[-1]
        mime_type = MIME_TYPES.get(extension, "application/octet-stream")
        response_headers.append(('Content-Type', mime_type))

        # Read the whole file into memory and send it out
        body = self.cache_file(full_path)
        response_headers.append(('Content-Length', str(len(body))))
        web_logger.info("HTTP GET {} 200 OK".format(path))
        return HTTPStatus.OK, response_headers, body

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
            logger.info("Cleaned up {} session".format(uid))
            if other_id in self.sessions:
                del self.sessions[other_id]
                logger.info("Also cleaned up {} session".format(other_id))
                # If there was a session with this peer, also
                # close the connection to reset its state.
                if other_id in self.peers:
                    logger.info("Closing connection to {}".format(other_id))
                    wso, oaddr, _ = self.peers[other_id]
                    del self.peers[other_id]
                    await wso.close()

    async def cleanup_room(self, uid, room_id):
        room_peers = self.rooms[room_id]
        if uid not in room_peers:
            return
        room_peers.remove(uid)
        for pid in room_peers:
            wsp, paddr, _ = self.peers[pid]
            msg = 'ROOM_PEER_LEFT {}'.format(uid)
            logger.info('room {}: {} -> {}: {}'.format(room_id, uid, pid, msg))
            await wsp.send(msg)

    async def remove_peer(self, uid):
        await self.cleanup_session(uid)
        if uid in self.peers:
            ws, raddr, status = self.peers[uid]
            if status and status != 'session':
                await self.cleanup_room(uid, status)
            del self.peers[uid]
            await ws.close()
            logger.info("Disconnected from peer {!r} at {!r}".format(uid, raddr))

    ############### Handler functions ###############

    
    async def connection_handler(self, ws, uid):
        raddr = ws.remote_address
        peer_status = None
        self.peers[uid] = [ws, raddr, peer_status]
        logger.info("Registered peer {!r} at {!r}".format(uid, raddr))
        while True:
            # Receive command, wait forever if necessary
            msg = await self.recv_msg_ping(ws, raddr)
            # Update current status
            peer_status = self.peers[uid][2]
            # We are in a session or a room, messages must be relayed
            if peer_status is not None:
                # We're in a session, route message to connected peer
                if peer_status == 'session':
                    other_id = self.sessions[uid]
                    wso, oaddr, status = self.peers[other_id]
                    assert(status == 'session')
                    logger.info("{} -> {}: {}".format(uid, other_id, msg))
                    await wso.send(msg)
                # We're in a room, accept room-specific commands
                elif peer_status:
                    # ROOM_PEER_MSG peer_id MSG
                    if msg.startswith('ROOM_PEER_MSG'):
                        _, other_id, msg = msg.split(maxsplit=2)
                        if other_id not in self.peers:
                            await ws.send('ERROR peer {!r} not found'
                                          ''.format(other_id))
                            continue
                        wso, oaddr, status = self.peers[other_id]
                        if status != room_id:
                            await ws.send('ERROR peer {!r} is not in the room'
                                          ''.format(other_id))
                            continue
                        msg = 'ROOM_PEER_MSG {} {}'.format(uid, msg)
                        logger.info('room {}: {} -> {}: {}'.format(room_id, uid, other_id, msg))
                        await wso.send(msg)
                    elif msg == 'ROOM_PEER_LIST':
                        room_id = self.peers[peer_id][2]
                        room_peers = ' '.join([pid for pid in self.rooms[room_id] if pid != peer_id])
                        msg = 'ROOM_PEER_LIST {}'.format(room_peers)
                        logger.info('room {}: -> {}: {}'.format(room_id, uid, msg))
                        await ws.send(msg)
                    else:
                        await ws.send('ERROR invalid msg, already in room')
                        continue
                else:
                    raise AssertionError('Unknown peer status {!r}'.format(peer_status))
            # Requested a session with a specific peer
            elif msg.startswith('SESSION'):
                logger.info("{!r} command {!r}".format(uid, msg))
                _, callee_id = msg.split(maxsplit=1)
                if callee_id not in self.peers:
                    await ws.send('ERROR peer {!r} not found'.format(callee_id))
                    continue
                if peer_status is not None:
                    await ws.send('ERROR peer {!r} busy'.format(callee_id))
                    continue
                await ws.send('SESSION_OK')
                wsc = self.peers[callee_id][0]
                logger.info('Session from {!r} ({!r}) to {!r} ({!r})'
                      ''.format(uid, raddr, callee_id, wsc.remote_address))
                # Register session
                self.peers[uid][2] = peer_status = 'session'
                self.sessions[uid] = callee_id
                self.peers[callee_id][2] = 'session'
                self.sessions[callee_id] = uid
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
                self.peers[uid][2] = peer_status = room_id
                self.rooms[room_id].add(uid)
                for pid in self.rooms[room_id]:
                    if pid == uid:
                        continue
                    wsp, paddr, _ = self.peers[pid]
                    msg = 'ROOM_PEER_JOINED {}'.format(uid)
                    logger.info('room {}: {} -> {}: {}'.format(room_id, uid, pid, msg))
                    await wsp.send(msg)
            else:
                logger.info('Ignoring unknown message {!r} from {!r}'.format(msg, uid))

    async def hello_peer(self, ws):
        '''
        Exchange hello, register peer
        '''
        raddr = ws.remote_address
        hello = await ws.recv()
        hello, uid = hello.split(maxsplit=1)
        if hello != 'HELLO':
            await ws.close(code=1002, reason='invalid protocol')
            raise Exception("Invalid hello from {!r}".format(raddr))
        if not uid or uid in self.peers or uid.split() != [uid]: # no whitespace
            await ws.close(code=1002, reason='invalid peer uid')
            raise Exception("Invalid uid {!r} from {!r}".format(uid, raddr))
        # Send back a HELLO
        await ws.send('HELLO')
        return uid

    def get_https_certs(self):
        cert_pem = os.path.abspath(self.https_cert) if os.path.isfile(self.https_cert) else None
        key_pem = os.path.abspath(self.https_key) if os.path.isfile(self.https_key) else None
        return cert_pem, key_pem

    def get_ssl_ctx(self, https_server=True):
        if not self.enable_https:
            return None
        # Create an SSL context to be used by the websocket server
        cert_pem, key_pem = self.get_https_certs()
        logger.info('Using TLS with certificate in {!r} and private key in {!r}'.format(cert_pem, key_pem))
        ssl_purpose = ssl.Purpose.CLIENT_AUTH if https_server else ssl.Purpose.SERVER_AUTH
        sslctx = ssl.create_default_context(purpose=ssl_purpose)
        sslctx.check_hostname = False
        sslctx.verify_mode = ssl.CERT_NONE
        try:
            sslctx.load_cert_chain(cert_pem, keyfile=key_pem)
        except Exception:
            logger.error('Certificate or private key file not found or incorrect. To use a self-signed certificate, install the package \'ssl-cert\' and add the group \'ssl-cert\' to your user in Debian-based distributions or generate a new certificate with root using \'openssl req -x509 -newkey rsa:4096 -keyout /etc/ssl/private/ssl-cert-snakeoil.key -out /etc/ssl/certs/ssl-cert-snakeoil.pem -days 3650 -nodes -subj \"/CN=localhost\"\'')
            sys.exit(1)
        return sslctx

    async def run(self):
        async def handler(ws, path):
            '''
            All incoming messages are handled here. @path is unused.
            '''
            raddr = ws.remote_address
            logger.info("Connected to {!r}".format(raddr))
            peer_id = await self.hello_peer(ws)
            try:
                await self.connection_handler(ws, peer_id)
            except websockets.ConnectionClosed:
                logger.info("Connection to peer {!r} closed, exiting handler".format(raddr))
            finally:
                await self.remove_peer(peer_id)

        sslctx = self.get_ssl_ctx(https_server=True)

        # Setup logging
        logger.setLevel(logging.INFO)
        web_logger.setLevel(logging.WARN)

        http_protocol = 'https:' if self.enable_https else 'http:'
        logger.info("Listening on {}//{}:{}".format(http_protocol, self.addr, self.port))
        # Websocket and HTTP server
        http_handler = functools.partial(self.process_request, self.web_root)
        self.stop_server = self.loop.create_future()
        async with websockets.serve(handler, self.addr, self.port, ssl=sslctx, process_request=http_handler, loop=self.loop,
                               # Maximum number of messages that websockets will pop
                               # off the asyncio and OS buffers per connection. See:
                               # https://websockets.readthedocs.io/en/stable/api.html#websockets.protocol.WebSocketCommonProtocol
                               max_queue=16) as self.server:
            await self.stop_server

        asyncio.ensure_future(self.check_server_needs_restart(), loop=self.loop)

    async def stop(self):
        logger.info('Stopping server... ')
        self.stop_server.set_result(True)
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
            await asyncio.sleep(1)
            if self.check_cert_changed():
                logger.info('Certificate changed, stopping server...')
                await self.stop()
                return


def main():
    parser = argparse.ArgumentParser(formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    # See: host, port in https://docs.python.org/3/library/asyncio-eventloop.html#asyncio.loop.create_server
    parser.add_argument('--addr', default='', help='Address to listen on (default: all interfaces, both ipv4 and ipv6)')
    parser.add_argument('--port', default=8443, type=int, help='Port to listen on')
    parser.add_argument('--web_root', default=os.getcwd(), type=str, help='Path to web root')
    parser.add_argument('--rtc_config_file', default="/tmp/rtc.json", type=str, help='Path to json rtc config file')
    parser.add_argument('--rtc_config', default="", type=str, help='JSON rtc config data')
    parser.add_argument('--turn_shared_secret', default="", type=str, help='shared secret for generating TURN HMAC credentials')
    parser.add_argument('--turn_host', default="", type=str, help='TURN host when generating RTC config with shared secret')
    parser.add_argument('--turn_port', default="", type=str, help='TURN port when generating RTC config with shared secret')
    parser.add_argument('--turn_protocol', default="udp", type=str, help='TURN protocol to use ("udp" or "tcp"), set to "tcp" without the quotes if "udp" is blocked on the network.')
    parser.add_argument('--enable_turn_tls', default=False, dest='turn_tls', action='store_true', help='enable TURN over TLS (for the TCP protocol) or TURN over DTLS (for the UDP protocol), valid TURN server certificate required.')
    parser.add_argument('--turn_auth_header_name', default="x-auth-user", type=str, help='auth header for turn credentials')
    parser.add_argument('--keepalive_timeout', dest='keepalive_timeout', default=30, type=int, help='Timeout for keepalive (in seconds)')
    parser.add_argument('--enable_https', default=False, help='Enable HTTPS connection', action='store_true')
    parser.add_argument('--https_cert', default="/etc/ssl/certs/ssl-cert-snakeoil.pem", type=str, help='HTTPS certificate file path')
    parser.add_argument('--https_key', default="/etc/ssl/private/ssl-cert-snakeoil.key", type=str, help='HTTPS private key file path, set to an empty string if the private key is included in the certificate')
    parser.add_argument('--health', default='/health', help='Health check route')
    parser.add_argument('--restart_on_cert_change', default=False, dest='cert_restart', action='store_true', help='Automatically restart if the HTTPS certificate changes')
    parser.add_argument('--enable_basic_auth', default=False, dest='enable_basic_auth', action='store_true', help="Use basic authentication, must also set basic_auth_user, and basic_auth_password arguments")
    parser.add_argument('--basic_auth_user', default="", help='Username for basic authentication.')
    parser.add_argument('--basic_auth_password', default="", help='Password for basic authentication, if not set, no authorization will be enforced.')

    options = parser.parse_args(sys.argv[1:])

    loop = asyncio.get_event_loop()

    r = WebRTCSimpleServer(loop, options)

    print('Starting server...')
    asyncio.ensure_future(r.run(), loop=loop)
    print("Started server")
    loop.run_forever()

if __name__ == "__main__":
    main()
