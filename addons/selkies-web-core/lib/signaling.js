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

/**
 * Signaling client for the WebRTC transport.
 *
 * Speaks the line protocol of the server's signaling WebSocket. `HELLO
 * <peer type> <json>` registers the client, the JSON carrying its type, slot,
 * strict-viewer flag, secure-mode token, display id, display position, tab,
 * and the start rate congestion control last held its display at;
 * `SESSION server` asks for the server peer and is answered with
 * `SESSION_OK <peer id>`; from then on the SDP and the ICE candidates travel
 * as `<peer id> {"sdp": ...}` and `<peer id> {"ice": ...}` lines, and
 * `ERROR ...` lines report server-side failures. The server always offers
 * and the client answers.
 *
 * A failed or dropped connection retries RETRY_FIRST_MS later, each further
 * failure twice as far out up to RETRY_MAX_MS; the fourth consecutive failure
 * hands over to `onfatalretry` (or reloads the page) so the browser re-runs
 * HTTP authentication.
 * @module
 */

import { pageTabId, recalledCcStart } from './util.js';

/** First signaling retry after a failure, doubled per failure up to RETRY_MAX_MS. */
const RETRY_FIRST_MS = 400;
const RETRY_MAX_MS = 1000;

/**
 * Connection to the signaling server, delivering SDP and ICE to callbacks.
 *
 * Callbacks are assigned as properties: `onstatus`, `ondebug`, and `onerror`
 * receive messages, `onsdp` an `RTCSessionDescription`, `onice` an
 * `RTCIceCandidate`, `ondisconnect` whether the app should reconnect,
 * `onshowalert` a reason to show the user, and `onfatalretry` replaces the
 * page reload after repeated failures.
 */
export class WebRTCSignaling {
    /**
     * @param {URL} server Signaling WebSocket URL.
     * @param {string} client_type `'controller'` or `'viewer'`.
     * @param {number} client_slot Client slot the server assigns per display.
     * @param {boolean} client_strict_viewer Whether a viewer may never gain control.
     * @param {string} client_token Secure-mode session token; the server matches
     *     it against the active mouse+keyboard token to grant a viewer
     *     read-write collaboration.
     * @param {string=} display_id Display this client drives, `'primary'` by
     *     default; the server scopes controller and slot uniqueness per display
     *     so a secondary display never evicts the primary.
     * @param {string=} display_position Where a secondary display sits relative
     *     to the primary in the extended desktop, `'right'` by default.
     */
    constructor(server, client_type, client_slot, client_strict_viewer, client_token, display_id, display_position) {
        /** @type {URL} */
        this._server = server;

        /**
         * Answers, before the hello, which codec names this engine decodes at
         * 4:4:4, so the server offers 4:2:0 to a client without them rather
         * than a profile it paints nothing of; unset, the hello says nothing
         * and the server takes the client at its word.
         * @type {?function(): Promise<string[]>}
         */
        this.capabilities = null;

        /**
         * Answers, before the hello, which formats this engine decodes at 10
         * bits, a codec name for 4:2:0 and the name with `444` after it for
         * 4:4:4, so the server offers 8 bits to a client without them; unset,
         * the hello says nothing.
         * @type {?function(): Promise<string[]>}
         */
        this.tenBitCapabilities = null;
        /**
         * Returns the [width, height] the page's first resize will ask for,
         * which the server starts the primary's capture at, or null where the
         * page sends none; unset, the hello says nothing.
         * @type {?function(): ?number[]}
         */
        this.pageSize = null;

        /** Local peer id, set by the WebRTC client before `connect`. @type {number} */
        this.peer_id = 1;

        /** @type {WebSocket} */
        this._ws_conn = null;

        /** @type {?function(string): void} */
        this.onstatus = null;

        /**
         * Called instead of the page reload after repeated connect failures,
         * so the app may inspect the endpoint before reloading.
         * @type {?function(): void}
         */
        this.onfatalretry = null;

        /** @type {?function(string): void} */
        this.onerror = null;

        /** @type {?function(string): void} */
        this.ondebug = null;

        /** @type {?function(RTCIceCandidate): void} */
        this.onice = null;

        /** @type {?function(RTCSessionDescription): void} */
        this.onsdp = null;

        /** Called with whether the app should reconnect. @type {?function(boolean): void} */
        this.ondisconnect = null;

        /** `'disconnected'`, `'connecting'`, or `'connected'`. @type {string} */
        this.state = 'disconnected';

        /** @type {number} */
        this.retry_count = 0;

        /**
         * Pending retry timer; a failed handshake fires both `error` and
         * `close`, and both funnel into this one scheduled retry.
         */
        this._retry_timer = null;

        /** Failures since the socket last opened, which set the next retry's delay. */
        this._backoff = 0;

        /** Stops the current socket's events reaching this client (`reconnect`). */
        this._forget_socket = () => {};

        // The network coming back retries at once rather than at the next rung.
        window.addEventListener('online', () => {
            if (!this._retry_timer) return;
            clearTimeout(this._retry_timer);
            this._retry_timer = null;
            this._backoff = 0;
            this.retry_count--;
            this._scheduleRetry();
        });

        /**
         * Set by `disconnect` so a locally requested close is not treated as
         * a server-side drop needing recovery.
         */
        this._intentional_close = false;

        /** @type {Array<number>} */
        this.currRes = null;

        /** @type {string} */
        this.peer_type = "client";

        /** `'viewer'` or `'controller'`. @type {string} */
        this.client_type = client_type;

        /** @type {string} */
        this.server_peer_id = null;

        /** @type {number} */
        this.client_slot = client_slot;

        /** @type {boolean} */
        this.client_strict_viewer = client_strict_viewer;

        /** @type {string} */
        this.client_token = client_token;

        /** @type {string} */
        this.display_id = display_id || 'primary';

        /** @type {string} */
        this.display_position = display_position || 'right';

        /** @type {?function(string): void} */
        this.onshowalert = null;
    }

    /** Forwards a status message to `onstatus`. */
    _setStatus(message) {
        if (this.onstatus !== null) {
            this.onstatus(message);
        }
    }

    /** Forwards a debug message to `ondebug`. */
    _setDebug(message) {
        if (this.ondebug !== null) {
            this.ondebug(message);
        }
    }

    /** Forwards an error message to `onerror`. */
    _setError(message) {
        if (this.onerror !== null) {
            this.onerror(message);
        }
    }

    /** Forwards a remote description to `onsdp`. */
    _setSDP(sdp) {
        if (this.onsdp !== null) {
            this.onsdp(sdp);
        }
    }

    /** Forwards a remote ICE candidate to `onice`. */
    _setICE(icecandidate) {
        if (this.onice !== null) {
            this.onice(icecandidate);
        }
    }

    /**
     * Registers with the server once the socket opens: sends `HELLO` with
     * the client metadata and resets the retry count.
     */
    async _onServerOpen() {
        this.state = 'connected';
        const meta = {
            'client_type': this.client_type,
            'client_slot': this.client_slot,
            'client_strict_viewer': this.client_strict_viewer,
            'client_token': this.client_token,
            'display_id': this.display_id,
            'display_position': this.display_position,
            'client_tab_id': pageTabId(),
            'cc_start_kbps': recalledCcStart(this.display_id),
        }
        if (this.capabilities) {
            try { meta.fullcolor_codecs = await this.capabilities(); } catch (e) { /* the server takes silence as decodable */ }
        }
        if (this.tenBitCapabilities) {
            try { meta.tenbit_codecs = await this.tenBitCapabilities(); } catch (e) { /* the server takes silence as decodable */ }
        }
        if (this.pageSize) {
            try {
                const size = this.pageSize();
                if (size) meta.page_size = size;
            } catch (e) { /* the server then waits for the first resize */ }
        }
        if (!this._ws_conn || this._ws_conn.readyState !== WebSocket.OPEN) return;
        this._ws_conn.send(`HELLO ${this.peer_type} ${JSON.stringify(meta)}`);
        this._setStatus("Registering with server, peer type: " + this.peer_type + ", client type: " + this.client_type);
        this.retry_count = 0;
        this._backoff = 0;
    }

    /**
     * Schedules one reconnect, RETRY_FIRST_MS after the first failure and
     * twice as far out after each further one up to RETRY_MAX_MS; a timer
     * already pending absorbs the second of the paired `error` and `close`
     * events.
     *
     * After three failed retries the credentials have most likely expired and
     * the upgrade is being rejected, so the page reloads for the browser to
     * re-run HTTP authentication, unless `onfatalretry` is set to let the app
     * probe the endpoint first (for a server-side transport mode change, say).
     */
    _scheduleRetry() {
        if (this._retry_timer) return;
        this.retry_count++;
        const delay = Math.min(RETRY_FIRST_MS * 2 ** this._backoff, RETRY_MAX_MS);
        this._backoff++;
        this._retry_timer = setTimeout(() => {
            this._retry_timer = null;
            if (this.retry_count > 3) {
                if (this.onfatalretry !== null) {
                    this.onfatalretry();
                } else {
                    window.location.reload();
                }
            } else {
                this.connect();
            }
        }, delay);
    }

    /**
     * Starts the retry cycle over, for an app whose `onfatalretry` found the
     * server not answering: the next connect on the delay the failures so far
     * set, and the next cycle ending in `onfatalretry` again.
     */
    retry() {
        this.retry_count = 0;
        this._scheduleRetry();
    }

    /** Socket error: retries when the socket is already closed, else the close event does. */
    _onServerError() {
        this._setStatus("Connection error, retrying.");
        if (this._ws_conn.readyState === this._ws_conn.CLOSED) {
            this._scheduleRetry();
        }
    }

    /** Asks the server for a session with its server peer. */
    _setupCall() {
        this._setStatus("Initiating session with server.");
        this._ws_conn.send(`SESSION server`);
    }
    /**
     * Dispatches a server line: `HELLO` (registered, so request the session),
     * `SESSION_OK <peer id>` (session established), `ERROR ...` (the missing
     * server peer is retried after a second), and otherwise a
     * `<peer id> {"sdp": ...}` or `<peer id> {"ice": ...}` line.
     * @param {MessageEvent} event
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

        var msg;
        try {
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
     * Socket closed. A close during the handshake (the upgrade was rejected,
     * say) schedules the retry itself, since the paired `error` event is not
     * guaranteed to observe `readyState` CLOSED. Afterwards the close code
     * decides: 4000 is a verdict on this client and shows the server's
     * reason; 4001 means another live connection superseded this session,
     * and auto-reconnecting would make the two pages evict each other
     * forever, so it stays down and tells the user; a clean close that
     * `disconnect` requested reports `ondisconnect(false)`; any other close
     * the server made, the 1001 of a server going away (stopping,
     * restarting, or switching transport) among them, reports
     * `ondisconnect(true)` so the app recovers like the WebSocket transport
     * (reconnect, and repeated failures reload for re-authentication).
     * @param {CloseEvent} event
     */
    _onServerClose(event) {
        if (this.state === 'connecting') {
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
                if (this.onshowalert !== null) {
                    this.onshowalert(event.reason || 'Session superseded by a new connection. Reload to take over.');
                }
            } else if ((event.code === 1000 || event.code === 1001) && intentional) {
                this.ondisconnect(false);
            } else {
                console.log("Reconnecting due to server-side connection closure.");
                this.ondisconnect(true);
            }
        }
    }

    /**
     * Opens the signaling socket; registration, the session request, and the
     * SDP and ICE exchange follow from the socket events.
     */
    connect() {
        this.state = 'connecting';
        this._setStatus("Connecting to server.");

        this._ws_conn = new WebSocket(this._server);
        let live = true;
        this._forget_socket = () => { live = false; };
        const own = (handler) => (event) => { if (live) handler.call(this, event); };

        this._ws_conn.addEventListener('open', own(this._onServerOpen));
        this._ws_conn.addEventListener('error', own(this._onServerError));
        this._ws_conn.addEventListener('message', own(this._onServerMessage));
        this._ws_conn.addEventListener('close', own(this._onServerClose));
    }

    /**
     * Starts a new session in place of one whose media path failed while this
     * socket stayed up: the socket is closed without waiting for its close
     * (which a dead path can hold back for a long time), whatever it still
     * reports is ignored, among it the 4001 the server's newest-wins sends a
     * holder it had not reaped yet, and `ondisconnect(true)` has the app
     * reconnect as after a server-side drop.
     */
    reconnect() {
        this._forget_socket();
        if (this._retry_timer) {
            clearTimeout(this._retry_timer);
            this._retry_timer = null;
        }
        try { if (this._ws_conn) this._ws_conn.close(); } catch (e) { /* already closing */ }
        this.state = 'disconnected';
        if (this.ondisconnect !== null) this.ondisconnect(true);
    }

    /** Closes the socket; the close is reported as `ondisconnect(false)`, not as a drop. */
    disconnect() {
        this._intentional_close = true;
        this._ws_conn.close();
    }

    /**
     * Sends a local ICE candidate to the server peer.
     * @param {RTCIceCandidate} ice
     */
    sendICE(ice) {
        this._setDebug("sending ice candidate: " + JSON.stringify(ice));
        this._ws_conn.send(`${this.server_peer_id} ${JSON.stringify({ 'ice': ice })}`);
    }

    /**
     * Sends the local description (the answer) to the server peer.
     * @param {RTCSessionDescription} sdp
     */
    sendSDP(sdp) {
        this._setDebug("sending local sdp: " + JSON.stringify(sdp));
        this._ws_conn.send(`${this.server_peer_id} ${JSON.stringify({ 'sdp': sdp })}`);
    }
}