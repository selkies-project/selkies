/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The transport switch both dashboards run, from their mode menus and from
 * the notice the WebRTC core raises when its media path keeps failing.
 * @module
 */
import { getRoutePrefix } from './util.js';
import { sessionAuthHeaders } from './session-token.js';

const MASTER_TOKEN_KEY = 'selkies_master_token';

/**
 * Asks the server to swap transports through `/api/switch`, then posts
 * `mode` so the core reloads into it.
 *
 * `window.__selkiesModeSwitching` is set before the request, because the
 * server tears the old peer down before it answers and the core would
 * otherwise start recovering the connection the switch replaces. The request
 * carries this client's own session token, which a controller's is enough
 * for; a stored master token overrides it, and where neither is accepted a
 * 401 prompts for the master token once, keeps it in sessionStorage and
 * retries, dropping one the server rejects so the next attempt re-prompts. A
 * viewer is refused 403 and is asked nothing. A failed switch clears the
 * flag again, since no reload follows and a kept flag would hide a real
 * disconnect.
 * @param {string} mode `webrtc` or `websockets`.
 * @returns {Promise<boolean>} Whether the server switched.
 */
export async function switchStreamMode(mode) {
    window.__selkiesModeSwitching = true;
    try {
        const request = () => {
            const headers = sessionAuthHeaders({ 'Content-Type': 'application/json' });
            let stored = null;
            try { stored = sessionStorage.getItem(MASTER_TOKEN_KEY); } catch (e) { /* sessionStorage unavailable */ }
            if (stored) headers.Authorization = `Bearer ${stored}`;
            return fetch(`${getRoutePrefix()}/api/switch`, {
                method: 'POST',
                headers,
                credentials: 'same-origin',
                body: JSON.stringify({ mode }),
            });
        };
        let response = await request();
        if (response.status === 401) {
            const entered = window.prompt
                ? window.prompt('Switching the stream mode requires the Selkies master token:')
                : null;
            if (entered && entered.trim()) {
                try { sessionStorage.setItem(MASTER_TOKEN_KEY, entered.trim()); } catch (e) { /* sessionStorage unavailable */ }
                response = await request();
            }
        }
        if (!response.ok) {
            if (response.status === 401) {
                try { sessionStorage.removeItem(MASTER_TOKEN_KEY); } catch (e) { /* sessionStorage unavailable */ }
            }
            throw new Error(`Request failed with status ${response.status}`);
        }
        window.postMessage({ type: 'mode', mode }, window.location.origin);
        return true;
    } catch (error) {
        window.__selkiesModeSwitching = false;
        console.error('Error switching stream mode:', error);
        return false;
    }
}
