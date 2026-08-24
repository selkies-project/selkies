/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Secure-mode session token, shared by both transports and both dashboards.
 *
 * The token arrives in the page URL (`?token=`). The WebSocket handshakes
 * carry it themselves; in secure mode every other `/api/` route wants it too,
 * so every caller presents it the same way:
 *  - scripts put `Authorization: Bearer <token>` on their fetch/XHR calls
 *    (`sessionAuthHeaders`); the server accepts it beside Basic auth too,
 *    since a script's header replaces the browser's cached Basic credentials;
 *  - URLs the browser navigates to rather than fetches, such as the
 *    file-manager listing the dashboards open in an iframe, carry it as
 *    `?token=` (`withSessionToken`); the listing keeps it on its own links;
 *  - a same-site cookie scoped to the API prefix (`installSessionCookie`)
 *    covers anything the browser requests on its own, such as a download link
 *    or a listing opened by hand. It is a session cookie, so closing the
 *    browser clears it, and the next page load with a token overwrites it.
 * A server outside secure mode ignores all three.
 * @module
 */
import { getRoutePrefix } from './util.js';

/** Name of the API-scoped session cookie. */
export const SESSION_TOKEN_COOKIE = 'selkies_token';

/**
 * Reads the session token from the page URL.
 * @returns {string} The token, or `''` when the page has none (legacy mode,
 *     or a context without a location).
 */
export function getSessionToken() {
    if (typeof window === 'undefined' || !window.location) return '';
    try {
        return new URLSearchParams(window.location.search).get('token') || '';
    } catch (_) {
        return '';
    }
}

/**
 * Request headers with the Bearer token added when the page holds one.
 * @param {object} [headers] Headers to extend; copied untouched without a token.
 * @returns {object} A plain header object.
 */
export function sessionAuthHeaders(headers) {
    const base = Object.assign({}, headers || {});
    const token = getSessionToken();
    if (token && !('Authorization' in base)) {
        base.Authorization = `Bearer ${token}`;
    }
    return base;
}

/**
 * A same-origin URL with the page's token appended as `?token=`, for URLs the
 * browser navigates to (an iframe src, a link) rather than fetches.
 * @param {string} url Absolute or page-relative URL.
 * @returns {string} The URL as given without a token, else resolved and tokened.
 */
export function withSessionToken(url) {
    const token = getSessionToken();
    if (!token) return url;
    try {
        const resolved = new URL(url, window.location.href);
        resolved.searchParams.set('token', token);
        return resolved.href;
    } catch (_) {
        return url;
    }
}

/**
 * Mirrors the page's token into the API-scoped session cookie.
 *
 * Called once by each core at load. A page without a token leaves any
 * existing cookie alone, since another tab may still be using it; a cookie
 * write the browser blocks leaves the header and query carriers, which is
 * why this is best-effort.
 */
export function installSessionCookie() {
    const token = getSessionToken();
    if (!token || typeof document === 'undefined') return;
    const attributes = [`path=${getRoutePrefix()}/api/`, 'SameSite=Strict'];
    if (window.location.protocol === 'https:') attributes.push('Secure');
    try {
        document.cookie = `${SESSION_TOKEN_COOKIE}=${encodeURIComponent(token)}; ${attributes.join('; ')}`;
    } catch (_) { /* cookies unavailable */ }
}
