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

import ssl
import json
import base64
import aiohttp
import asyncio
import logging
from typing import Any, Callable, Dict, Optional, Awaitable
from aiohttp import ClientWebSocketResponse, WSMsgType

logger = logging.getLogger("signaling_client")
logger.setLevel(logging.INFO)
logging.getLogger("aiohttp").setLevel(logging.WARNING)


class WebRTCSignalingError(Exception):
    """Base exception for signaling errors."""

    pass


class WebRTCSignalingErrorNoPeer(WebRTCSignalingError):
    """Exception raised when no peer is found."""

    pass


class WebRTCSignalingClient:
    """WebSocket signaling client for WebRTC peer connection establishment.

    Uses aiohttp for WebSocket communication with the signaling server.
    Supports automatic reconnection on connection failures.
    """

    def __init__(
        self,
        server: str,
        enable_https: bool = False,
        enable_basic_auth: bool = False,
        basic_auth_user: Optional[str] = None,
        basic_auth_password: Optional[str] = None,
    ) -> None:
        """Initialize the signaling client.

        Args:
            server: WebSocket server URL (e.g., 'ws://localhost:8080/ws').
            enable_https: Whether to use WSS (secure WebSocket).
            enable_basic_auth: Whether to use HTTP Basic Authentication.
            basic_auth_user: Username for basic auth.
            basic_auth_password: Password for basic auth.
        """
        self.server = server
        self.peer_type = "server"
        self.enable_https = enable_https
        self.enable_basic_auth = enable_basic_auth
        self.basic_auth_user = basic_auth_user
        self.basic_auth_password = basic_auth_password

        self._session: Optional[aiohttp.ClientSession] = None
        self._ws: Optional[ClientWebSocketResponse] = None
        self._stop_event = asyncio.Event()
        self._task: Optional[asyncio.Task] = None

        # Callbacks - to be overridden by the consumer
        self.on_ice: Callable[[Dict[str, Any], str], Awaitable[None]] = (
            lambda ice, client_peer_id: logger.warning("unhandled ice event")
        )
        self.on_sdp: Callable[[str, str, str], Awaitable[None]] = (
            lambda sdp_type, sdp, client_peer_id: logger.warning("unhandled sdp event")
        )
        self.on_disconnect: Callable[[], Awaitable[None]] = lambda: logger.warning(
            "unhandled on_disconnect callback"
        )
        self.on_session_start: Callable[[str, str], Awaitable[None]] = (
            lambda client_peer_id, client_type: logger.warning(
                "unhandled on_session_start callback"
            )
        )
        self.on_session_end: Callable[[str, str], Awaitable[None]] = (
            lambda client_peer_id, client_type: logger.warning(
                "unhandled on_session_end callback"
            )
        )
        self.on_error: Callable[[Exception], Awaitable[None]] = lambda v: logger.warning(
            "unhandled on_error callback: %s", v
        )

    def start(self) -> None:
        """Start the signaling client connection task."""
        self._stop_event.clear()
        self._task = asyncio.create_task(self.connect_and_listen())

    async def connect_and_listen(self) -> None:
        """Connect to the signaling server and listen for messages.

        Automatically reconnects on connection failures.
        """
        ssl_ctx: Optional[ssl.SSLContext] = None
        if self.enable_https:
            ssl_ctx = ssl.create_default_context(purpose=ssl.Purpose.SERVER_AUTH)
            ssl_ctx.check_hostname = False
            ssl_ctx.verify_mode = ssl.CERT_NONE

        headers: Optional[Dict[str, str]] = None
        if self.enable_basic_auth and self.basic_auth_user and self.basic_auth_password:
            auth64 = base64.b64encode(
                f"{self.basic_auth_user}:{self.basic_auth_password}".encode("ascii")
            ).decode("ascii")
            headers = {"Authorization": f"Basic {auth64}"}

        while not self._stop_event.is_set():
            try:
                logger.info("Connecting to signaling server")
                self._session = aiohttp.ClientSession()
                self._ws = await self._session.ws_connect(
                    self.server,
                    headers=headers,
                    ssl=ssl_ctx,
                    heartbeat=30,
                )
                await self._ws.send_str(f"HELLO {self.peer_type}")
                await self._listen()
            except asyncio.CancelledError:
                pass
            except (
                aiohttp.WSServerHandshakeError,
                aiohttp.ClientConnectionError,
                OSError,
            ) as err:
                logger.warning(f"Connection failed, retrying... {err}")
                await asyncio.sleep(2)
            except aiohttp.ClientError as e:
                logger.warning(f"Client error, attempting to reconnect... {e}")
                await asyncio.sleep(2)
            except Exception as e:
                logger.exception(f"Unexpected error: {e}")
                await asyncio.sleep(2)
            finally:
                await self._cleanup_connection()
                if not self._stop_event.is_set():
                    await asyncio.sleep(0.1)

    async def _cleanup_connection(self) -> None:
        """Clean up WebSocket connection and session resources."""
        if self._ws is not None and not self._ws.closed:
            await self._ws.close()
        self._ws = None

        if self._session is not None and not self._session.closed:
            await self._session.close()
        self._session = None

    async def send_ice(
        self, mlineindex: int, candidate: str, client_peer_id: str
    ) -> None:
        """Send ICE candidate to peer via signaling server.

        Args:
            mlineindex: The media line index for the ICE candidate.
            candidate: The ICE candidate string.
            client_peer_id: The peer ID to send the candidate to.
        """
        if self._ws is None or self._ws.closed:
            raise WebRTCSignalingError("WebSocket connection not available")

        msg = json.dumps({"ice": {"candidate": candidate, "sdpMLineIndex": mlineindex}})
        await self._ws.send_str(f"{client_peer_id} {msg}")

    async def send_sdp(self, sdp_type: str, sdp: str, client_peer_id: str) -> None:
        """Send SDP to peer via signaling server.

        Args:
            sdp_type: SDP type ('offer' or 'answer').
            sdp: The SDP content.
            client_peer_id: The peer ID to send the SDP to.
        """
        if self._ws is None or self._ws.closed:
            raise WebRTCSignalingError("WebSocket connection not available")

        logger.info(f"sending sdp type: {sdp_type} to client_peer_id: {client_peer_id}")
        logger.debug("SDP:\n%s" % sdp)

        msg = json.dumps({"sdp": {"type": sdp_type, "sdp": sdp}})
        await self._ws.send_str(f"{client_peer_id} {msg}")

    async def stop(self) -> None:
        """Stop the signaling client and clean up resources."""
        logger.info("Stopping signaling client...")
        self._stop_event.set()

        # Cancel the connection task if running
        if self._task is not None and not self._task.done():
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

        await self._cleanup_connection()
        logger.info("Signaling client stopped")

    async def _listen(self) -> None:
        """Listen for and process incoming signaling messages.

        Message types handled:
            HELLO: Server acknowledgment of registration.
            SESSION_START: New session started with a client.
            SESSION_END: Session with a client ended.
            ERROR: Error messages from server.
            JSON SDP/ICE: WebRTC negotiation messages.
        """
        if self._ws is None:
            raise WebRTCSignalingError("WebSocket connection not available")

        try:
            async for msg in self._ws:
                if msg.type == WSMsgType.TEXT:
                    await self._process_message(msg.data)
                elif msg.type == WSMsgType.CLOSED:
                    logger.warning("WebSocket connection closed by server")
                    break
                elif msg.type == WSMsgType.ERROR:
                    logger.error(f"WebSocket error: {self._ws.exception()}")
                    break
        except asyncio.CancelledError:
            pass
        except aiohttp.ClientError as e:
            logger.warning(
                f"Signaling server closed the connection: {e}", exc_info=True
            )
        except Exception as e:
            logger.error(f"Error processing signaling message: {e}", exc_info=True)
        finally:
            await self.on_disconnect()

    async def _process_message(self, message: str) -> None:
        """Process a single signaling message.

        Args:
            message: The raw message string from the WebSocket.
        """
        if message == "HELLO":
            logger.info("WebSocket connection established with signaling server")

        elif message.startswith("SESSION_START"):
            toks = message.strip().split(" ")
            if len(toks) == 3:
                # peer_id is of format client-<UUID>
                _, client_peer_id, client_type = toks
                await self.on_session_start(client_peer_id, client_type)
            else:
                logger.error(f"invalid SESSION_START message: {message}")

        elif message.startswith("SESSION_END"):
            toks = message.strip().split(" ")
            if len(toks) == 3:
                _, client_peer_id, client_type = toks
                await self.on_session_end(client_peer_id, client_type)
            else:
                logger.error(f"invalid SESSION_END message: {message}")

        elif message.startswith("ERROR"):
            await self.on_error(
                WebRTCSignalingError(f"unhandled signaling message: {message}")
            )

        else:
            # Attempt to parse JSON SDP or ICE message
            client_peer_id: Optional[str] = None
            data: Optional[Dict[str, Any]] = None

            try:
                client_peer_id, message = message.split(" ", maxsplit=1)
                data = json.loads(message)
            except json.JSONDecodeError:
                await self.on_error(
                    WebRTCSignalingError(f"error parsing message as JSON: {message}")
                )
                return
            except ValueError:
                await self.on_error(
                    WebRTCSignalingError(f"failed to parse message: {message}")
                )
                return

            if data is None:
                return

            if data.get("sdp"):
                logger.info(f"received SDP from client_peer_id: {client_peer_id}")
                logger.debug(f"SDP:\n{data['sdp']}")
                await self.on_sdp(
                    data["sdp"].get("type", ""),
                    data["sdp"].get("sdp", ""),
                    client_peer_id,
                )
            elif data.get("ice"):
                logger.info(f"received ICE from client_peer_id: {client_peer_id}")
                logger.debug(f"ICE:\n{data.get('ice')}")
                await self.on_ice(data["ice"], client_peer_id)
            else:
                await self.on_error(
                    WebRTCSignalingError(f"unhandled JSON message: {json.dumps(data)}")
                )
