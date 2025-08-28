# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Copyright 2019 Google LLC
#
#   Licensed under the Apache License, Version 2.0 (the "License");
#   you may not use this file except in compliance with the License.
#   You may obtain a copy of the License at
#
#        http://www.apache.org/licenses/LICENSE-2.0
#
#   Unless required by applicable law or agreed to in writing, software
#   distributed under the License is distributed on an "AS IS" BASIS,
#   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
#   See the License for the specific language governing permissions and
#   limitations under the License.

import asyncio
import base64
import json
import logging
import ssl
import websockets
import websockets.asyncio.client

logger = logging.getLogger("signaling_client")
logger.setLevel(logging.INFO)
# websockets logs an error if a connection is opened and closed before any data is sent.
# The client seems to do same thing, causing an inital handshake error.
logging.getLogger("websockets").setLevel(logging.CRITICAL)

class WebRTCSignalingError(Exception):
    pass

class WebRTCSignalingErrorNoPeer(Exception):
    pass

class WebRTCSignaling:
    def __init__(self, server, id, peer_id, enable_https=False, enable_basic_auth=False,
                 basic_auth_user=None, basic_auth_password=None):
        """Initialize the signaling instance"""

        self.server = server
        self.id = id
        self.peer_id = peer_id
        self.enable_https = enable_https
        self.enable_basic_auth = enable_basic_auth
        self.basic_auth_user = basic_auth_user
        self.basic_auth_password = basic_auth_password
        self.conn = None
        self.stop_event = asyncio.Event()
        self.task = None

        self.on_ice = lambda mlineindex, candidate: logger.warning('unhandled ice event')
        self.on_sdp = lambda sdp_type, sdp: logger.warning('unhandled sdp event')
        self.on_connect = lambda res, scale: logger.warning('unhandled on_connect callback')
        self.on_disconnect = lambda: logger.warning('unhandled on_disconnect callback')
        self.on_session = lambda peer_id: logger.warning('unhandled on_session callback')
        self.on_error = lambda v: logger.warning('unhandled on_error callback: %s', v)

    def start(self) -> None:
        self.stop_event.clear()
        self.task = asyncio.create_task(self.connect_and_listen())

    async def connect_and_listen(self):
        """Connects to and registers id with signaling server.
        Listens for incoming messages continuously. Reconnects on failure.
        """
        sslctx = None
        if self.enable_https:
            sslctx = ssl.create_default_context(purpose=ssl.Purpose.SERVER_AUTH)
            sslctx.check_hostname = False
            sslctx.verify_mode = ssl.CERT_NONE
        headers = None
        if self.enable_basic_auth:
            auth64 = base64.b64encode(bytes("{}:{}".format(self.basic_auth_user, self.basic_auth_password), "ascii")).decode("ascii")
            headers = [("Authorization", "Basic {}".format(auth64))]

        while not self.stop_event.is_set():
            try:
                logger.info(f"Connecting to signaling server")
                self.conn = await websockets.asyncio.client.connect(self.server, additional_headers=headers, ssl=sslctx)

                await self.conn.send('HELLO %d' % self.id)
                await self.listen()
            except asyncio.CancelledError:
                pass
            except (ConnectionRefusedError, OSError) as err:
                logger.warning(f"Connection refused, retrying... {err}")
                await asyncio.sleep(2)
            except websockets.exceptions.ConnectionClosed:
                logger.warning("Connection closed, attempting to reconnect...")
                await asyncio.sleep(2)
            except Exception as e:
                logger.exception(f"Unexpected error: {e}")
                await asyncio.sleep(2)
            
            await asyncio.sleep(0.5)

    async def send_ice(self, mlineindex, candidate):
        """Sends the ice candidate to peer

        Arguments:
            mlineindex {integer} -- the mlineindex
            candidate {string} -- the candidate
        """

        msg = json.dumps(
            {'ice': {'candidate': candidate, 'sdpMLineIndex': mlineindex}})
        await self.conn.send(msg)

    async def send_sdp(self, sdp_type, sdp):
        """Sends the SDP to peer

        Arguments:
            sdp_type {string} -- SDP type, answer or offer.
            sdp {string} -- the SDP
        """

        logger.info("sending sdp type: %s" % sdp_type)
        logger.debug("SDP:\n%s" % sdp)

        msg = json.dumps({'sdp': {'type': sdp_type, 'sdp': sdp}})
        await self.conn.send(msg)

    async def stop(self):
        self.stop_event.set()
        if self.conn is not None:
            await self.conn.close()
            self.conn = None
            if self.task:
                await self.task
        logger.info("Signaling stopped")

    async def listen(self):
        """Handles messages from the signaling server websocket.

        Message types:
          HELLO: response from server indicating peer is registered.
          ERROR*: error messages from server.
          {"sdp": ...}: JSON SDP message
          {"ice": ...}: JSON ICE message

        Callbacks:

        on_connect: fired when HELLO is received.
        on_session: fired after setup_call() succeeds and SESSION_OK is received.
        on_error(WebRTCSignalingErrorNoPeer): fired when setup_call() fails and peer not found message is received.
        on_error(WebRTCSignalingError): fired when message parsing fails or unexpected message is received.

        """
        try:
            async for message in self.conn:
                if message == 'HELLO':
                    logger.info("ws connection established with signaling server")
                elif message.startswith('SESSION'):
                    toks = message.strip()
                    toks = toks.split(' ')
                    if len(toks) >= 2:
                        _, peer_id = toks[0], toks[1]
                    logger.info("starting session with peer: %s", peer_id)
                    await self.on_session(peer_id)
                elif message.startswith('ERROR'):
                    await self.on_error(WebRTCSignalingError("unhandled signaling message: %s" % message))
                else:
                    # Attempt to parse JSON SDP or ICE message
                    data = None
                    try:
                        data = json.loads(message)
                    except Exception as e:
                        if isinstance(e, json.decoder.JSONDecodeError):
                            await self.on_error(WebRTCSignalingError("error parsing message as JSON: %s" % message))
                        else:
                            await self.on_error(WebRTCSignalingError("failed to prase message: %s" % message))
                        continue
                    if data.get("sdp", None):
                        logger.info("received SDP")
                        logger.debug(f"SDP:\n{data['sdp']}")
                        await self.on_sdp(data['sdp'].get('type'), data['sdp'].get('sdp'))
                    elif data.get("ice", None):
                        logger.info("received ICE")
                        logger.debug(f"ICE:\n{data.get('ice')}")
                        await self.on_ice(data['ice'])
                    else:
                        await self.on_error(WebRTCSignalingError("unhandled JSON message: %s", json.dumps(data)))
        except asyncio.CancelledError:
            pass
        except websockets.exceptions.ConnectionClosed as e:
            logger.warning(f"Signaling server closed the connection {e}", exc_info=True)
        except Exception as e:
            logger.error(f"Error signaling client: {e}", exc_info=True)
        finally:
            if self.conn:
                await self.conn.close()
            self.conn = None
            await self.on_disconnect()
