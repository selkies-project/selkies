/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * This file incorporates work covered by the following copyright and
 * permission notice:
 *
 *   Copyright 2019 Google LLC
 *
 *   Licensed under the Apache License, Version 2.0 (the "License");
 *   you may not use this file except in compliance with the License.
 *   You may obtain a copy of the License at
 *
 *        http://www.apache.org/licenses/LICENSE-2.0
 *
 *   Unless required by applicable law or agreed to in writing, software
 *   distributed under the License is distributed on an "AS IS" BASIS,
 *   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *   See the License for the specific language governing permissions and
 *   limitations under the License.
 */

/*eslint no-unused-vars: ["error", { "vars": "local" }]*/


/**
* @typedef {Object} WebRTCSignaling
* @property {function} ondebug - Callback fired when a new debug message is set.
* @property {function} onstatus - Callback fired when a new status message is set.
* @property {function} onerror - Callback fired when an error occurs.
* @property {function} onice - Callback fired when a new ICE candidate is received.
* @property {function} onsdp - Callback fired when SDP is received.
* @property {function} connect - initiate connection to server.
* @property {function} disconnect - close connection to server.
*/
export class WebRTCSignaling {
    /**
     * Interface to the WebRTC signaling server.
     * Protocol reference:
     *   https://github.com/GStreamer/gstreamer/blob/main/subprojects/gst-examples/webrtc/signaling/Protocol.md
     *
     * @constructor
     * @param {URL} [server]
     *    The URL object of the signaling server to connect to, created with `new URL()`.
     *    Reference implementation:
     *      https://github.com/GStreamer/gstreamer/tree/main/subprojects/gst-examples/webrtc/signaling
     */
    constructor(server, client_type, client_slot, client_strict_viewer, client_token, display_id, display_position) {
        /**
         * @private
         * @type {URL}
         */
        this._server = server;

        /**
         * @private
         * @type {number}
         */
        this.peer_id = 1;

        /**
         * @private
         * @type {WebSocket}
         */
        this._ws_conn = null;

        /**
         * @event
         * @type {function}
         */
        this.onstatus = null;

        /**
         * Fired instead of the built-in page reload after repeated connect
         * failures; the app may inspect the endpoint before reloading.
         * @event
         * @type {function}
         */
        this.onfatalretry = null;

        /**
         * @event
         * @type {function}
         */
        this.onerror = null;

        /**
         * @type {function}
         */
        this.ondebug = null;

        /**
         * @event
         * @type {function}
         */
        this.onice = null;

        /**
         * @event
         * @type {function}
         */
        this.onsdp = null;

        /**
         * @event
         * @type {function}
         */
        this.ondisconnect = null;

        /**
         * @type {string}
         */
        this.state = 'disconnected';

        /**
         * @type {number}
         */
        this.retry_count = 0;

        /**
         * Pending retry timer; a failed handshake fires both 'error' and 'close',
         * and both funnel into one scheduled retry.
         * @private
         */
        this._retry_timer = null;

        /**
         * Set by disconnect() so a locally requested close is not treated as a
         * server-side drop needing recovery.
         * @private
         */
        this._intentional_close = false;

        /**
         * @type {Array<number>}
         */
        this.currRes = null;

        /**
         * @type {string}
         */
        this.peer_type = "client";

        /**
         * @type {string}
         * possile values: 'viewer', 'controller'
         */
        this.client_type = client_type;

        /**
         * @type {string}
         */
        this.server_peer_id = null;

        /**
         * @type {number}
         */
        this.client_slot = client_slot;

        /**
         * @type {boolean}
         */
        this.client_strict_viewer = client_strict_viewer;
        // Secure-mode session token; the server matches it against the active
        // mk (mouse+keyboard) token to grant a viewer read-write collaboration.
        this.client_token = client_token;

        /**
         * @private
         * @type {string}
         */
        // Which display this client drives; the server scopes controller/slot
        // uniqueness per display so display2 never evicts the primary.
        this.display_id = display_id || 'primary';

        /**
         * @private
         * @type {string}
         */
        // Where a secondary display sits relative to the primary in the
        // extended desktop layout.
        this.display_position = display_position || 'right';

        /**
         * @type {function}
         */
        this.onshowalert = null;
    }

    /**
     * Sets status message.
     *
     * @private
     * @param {String} message
     */
    _setStatus(message) {
        if (this.onstatus !== null) {
            this.onstatus(message);
        }
    }

    /**
     * Sets a debug message.
     * @private
     * @param {String} message
     */
    _setDebug(message) {
        if (this.ondebug !== null) {
            this.ondebug(message);
        }
    }

    /**
     * Sets error message.
     *
     * @private
     * @param {String} message
     */
    _setError(message) {
        if (this.onerror !== null) {
            this.onerror(message);
        }
    }

    /**
     * Sets SDP
     *
     * @private
     * @param {String} message
     */
    _setSDP(sdp) {
        if (this.onsdp !== null) {
            this.onsdp(sdp);
        }
    }

    /**
     * Sets ICE
     *
     * @private
     * @param {RTCIceCandidate} icecandidate
     */
    _setICE(icecandidate) {
        if (this.onice !== null) {
            this.onice(icecandidate);
        }
    }

    /**
     * Fired whenever the signaling websocket is opened.
     * Sends the peer id to the signaling server.
     *
     * @private
     * @event
     */
    _onServerOpen() {
        // Send local device resolution and scaling with HELLO message.
        this.state = 'connected';
        const meta = {
            'client_type': this.client_type,
            'client_slot': this.client_slot,
            'client_strict_viewer': this.client_strict_viewer,
            'client_token': this.client_token,
            'display_id': this.display_id,
            'display_position': this.display_position,
        }
        this._ws_conn.send(`HELLO ${this.peer_type} ${JSON.stringify(meta)}`);
        this._setStatus("Registering with server, peer type: " + this.peer_type + ", client type: " + this.client_type);
        this.retry_count = 0;
    }

    /**
     * Fired whenever the signaling websocket emits and error.
     * Reconnects after 3 seconds.
     *
     * @private
     * @event
     */
    _scheduleRetry() {
        if (this._retry_timer) return;
        this.retry_count++;
        this._retry_timer = setTimeout(() => {
            this._retry_timer = null;
            if (this.retry_count > 3) {
                // Repeated connect failures (e.g. credentials expired and the WS
                // upgrade now 401s): reload so the browser re-runs HTTP auth.
                // onfatalretry lets the app probe the endpoint first (e.g. detect
                // a server-side transport mode change) before reloading.
                if (this.onfatalretry !== null) {
                    this.onfatalretry();
                } else {
                    window.location.reload();
                }
            } else {
                this.connect();
            }
        }, 3000);
    }

    _onServerError() {
        this._setStatus("Connection error, retry in 3 seconds.");
        if (this._ws_conn.readyState === this._ws_conn.CLOSED) {
            this._scheduleRetry();
        }
    }

    _setupCall() {
        this._setStatus("Initiating session with server.");
        this._ws_conn.send(`SESSION server`);
    }
    /**
     * Fired whenever a message is received from the signaling server.
     * Message types:
     *   HELLO: response from server indicating peer is registered.
     *   ERROR*: error messages from server.
     *   {"sdp": ...}: JSON SDP message
     *   {"ice": ...}: JSON ICE message
     *
     * @private
     * @event
     * @param {Event} event The event: https://developer.mozilla.org/en-US/docs/Web/API/MessageEvent
     */
    _onServerMessage(event) {
        this._setDebug("server message: " + event.data);

        if (event.data === "HELLO") {
            this._setStatus("Registered with server.");
            this._setupCall();
            return;
        }

        if (event.data.startsWith("SESSION_OK")) { 
            this._setStatus("Session established with server.");
            this.server_peer_id = event.data.split(" ")[1];
            return;
        }

        if (event.data.startsWith("ERROR")) {
            if (event.data === "ERROR peer server not found") {
                this._setError("Server not found. Retrying...");
                setTimeout(() => {
                    this._setupCall();
                }, 1000);
            }
            return;
        }

        // Attempt to parse JSON SDP or ICE message
        var msg;
        try {
            // strip off prefix
            msg = event.data.substring(event.data.indexOf(' ') + 1);
            msg = JSON.parse(msg);
        } catch (e) {
            if (e instanceof SyntaxError) {
                this._setError("error parsing message as JSON: " + event.data);
            } else {
                this._setError("failed to parse message: " + event.data);
            }
            return;
        }

        if (msg.sdp != null) {
            this._setSDP(new RTCSessionDescription(msg.sdp));
        } else if (msg.ice != null) {
            var icecandidate = new RTCIceCandidate(msg.ice);
            this._setICE(icecandidate);
        } else {
            this._setError("unhandled JSON message: " + msg);
        }
    }

    /**
     * Fired whenever the signaling websocket is closed.
     * Reconnects after 1 second.
     *
     * @private
     * @event
     */
    _onServerClose(event) {
        if (this.state === 'connecting') {
            // Handshake never completed (e.g. the upgrade was rejected with 401).
            // Recover here: the paired 'error' event is not guaranteed to observe
            // readyState CLOSED, so this close may be the only recovery signal.
            this.state = 'disconnected';
            this._scheduleRetry();
            return;
        }
        this.state = 'disconnected';
        this._setError("Server closed connection.");
        const intentional = this._intentional_close;
        this._intentional_close = false;
        if (this.ondisconnect !== null) {
            if (event.code === 4000) {
                if (this.onshowalert !== null) this.onshowalert(event.reason);
            } else if (event.code === 4001) {
                // Superseded: another live connection took this session over. Auto-
                // reconnecting would evict the new holder and the two pages would
                // take the slot from each other forever — stay down, tell the user.
                if (this.onshowalert !== null) {
                    this.onshowalert(event.reason || 'Session superseded by a new connection. Reload to take over.');
                }
            } else if ((event.code === 1000 || event.code === 1001) && intentional) {
                this.ondisconnect(false);
            } else {
                // Server-initiated close, clean or not: recover like the websockets
                // transport (reconnect; repeated failures reload for re-auth).
                console.log("Reconnecting due to server-side connection closure.");
                this.ondisconnect(true);
            }
        }
    }

    /**
     * Initiates the connection to the signaling server.
     * After this is called, a series of handshakes occurs between the signaling
     * server and the server (peer) to negotiate ICE candidates and media capabilities.
     */
    connect() {
        this.state = 'connecting';
        this._setStatus("Connecting to server.");

        this._ws_conn = new WebSocket(this._server);

        // Bind event handlers.
        this._ws_conn.addEventListener('open', this._onServerOpen.bind(this));
        this._ws_conn.addEventListener('error', this._onServerError.bind(this));
        this._ws_conn.addEventListener('message', this._onServerMessage.bind(this));
        this._ws_conn.addEventListener('close', this._onServerClose.bind(this));
    }

    /**
     * Closes connection to signaling server.
     * Triggers onServerClose event.
     */
    disconnect() {
        this._intentional_close = true;
        this._ws_conn.close();
    }

    /**
     * Send ICE candidate.
     *
     * @param {RTCIceCandidate} ice
     */
    sendICE(ice) {
        this._setDebug("sending ice candidate: " + JSON.stringify(ice));
        this._ws_conn.send(`${this.server_peer_id} ${JSON.stringify({ 'ice': ice })}`);
    }

    /**
     * Send local session description.
     *
     * @param {RTCSessionDescription} sdp
     */
    sendSDP(sdp) {
        this._setDebug("sending local sdp: " + JSON.stringify(sdp));
        this._ws_conn.send(`${this.server_peer_id} ${JSON.stringify({ 'sdp': sdp })}`);
    }
}