/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Secure-mode session token, shared by both transports and both dashboards.
 *
 * The token arrives in the page URL, in one of two places:
 *  - the fragment, as a `token=` item beside the keyword that picks the
 *    display or the sharing role (`#token=<t>`, `#display2-right&token=<t>`).
 *    A browser never sends a fragment, so neither the page load nor a proxy
 *    in front of it records the token, and a token that arrives here is kept
 *    out of every URL the client requests afterwards as well;
 *  - the query (`?token=<t>`), which is in the page's request line already,
 *    so the requests that follow it carry it the same way.
 * A page with both uses the fragment's. In secure mode the WebSocket
 * handshakes and every other `/api/` route want the token, so every caller
 * presents it through one of these:
 *  - scripts put `Authorization: Bearer <token>` on their fetch/XHR calls
 *    (`sessionAuthHeaders`); the server accepts it beside Basic auth too,
 *    since a script's header replaces the browser's cached Basic credentials;
 *  - the data socket offers a fragment token as a WebSocket subprotocol
 *    (`sessionTokenProtocols`) and a query token as `?token=` on its URL, and
 *    the WebRTC signaling socket sends either in its hello message;
 *  - URLs the browser navigates to rather than fetches, such as the
 *    file-manager listing the dashboards open in an iframe, carry a query
 *    token as `?token=` (`withSessionToken`), and the listing keeps it on its
 *    own links, which is what serves a page embedded cross-site, where the
 *    cookie cannot follow; a fragment token leaves them to the cookie;
 *  - a same-site cookie scoped to the API prefix (`installSessionCookie`)
 *    covers anything the browser requests on its own, such as a download link
 *    or a listing opened by hand. It is a session cookie, so closing the
 *    browser clears it, and the next page load with a token overwrites it.
 * The fragment's keyword and the links other pages take are `page-url.js`'s.
 * A server outside secure mode ignores every carrier.
 * @module
 */
import { readUrlFragment } from './page-url.js';
import { getRoutePrefix } from './util.js';

/** Name of the API-scoped session cookie. */
export const SESSION_TOKEN_COOKIE = 'selkies_token';

/**
 * Subprotocol a handshake presenting its token offers first, and the one the
 * server selects: the token rides a second subprotocol, never echoed back.
 */
export const SESSION_TOKEN_PROTOCOL = 'selkies';

/** Prefix of the subprotocol that carries the token, base64url without padding. */
export const SESSION_TOKEN_PROTOCOL_PREFIX = 'selkies.token.';

/**
 * Whether the page's session token came in the fragment, which keeps it out of
 * every URL the client requests.
 * @returns {boolean}
 */
export function sessionTokenInFragment() {
    return readUrlFragment().token !== '';
}

/**
 * Reads the session token from the page URL: the fragment's `token=` item, or
 * else the `?token=` query parameter.
 * @returns {string} The token, or `''` when the page has none (legacy mode,
 *     or a context without a location).
 */
export function getSessionToken() {
    const fromFragment = readUrlFragment().token;
    if (fromFragment) return fromFragment;
    if (typeof window === 'undefined' || !window.location) return '';
    try {
        return new URLSearchParams(window.location.search).get('token') || '';
    } catch (_) {
        return '';
    }
}

/**
 * Request headers with the Bearer token added when the page holds one.
 * @param {Record<string, string>} [headers] Headers to extend; copied untouched
 *   without a token, and an Authorization already set is left alone.
 * @returns {Record<string, string>} A plain header object.
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
 * A same-origin URL with a query token appended as `?token=`, for URLs the
 * browser navigates to (an iframe src, a link) rather than fetches. A token
 * that came in the fragment is left to the cookie, so no request line ever
 * carries it.
 * @param {string} url Absolute or page-relative URL.
 * @returns {string} The URL as given without a query token, else resolved and
 *     tokened.
 */
export function withSessionToken(url) {
    if (sessionTokenInFragment()) return url;
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
 * base64url of a string's UTF-8 bytes, without padding, which keeps any token
 * inside the characters a subprotocol name allows.
 * @param {string} text
 * @returns {string}
 */
function base64url(text) {
    let binary = '';
    for (const byte of new TextEncoder().encode(text)) binary += String.fromCharCode(byte);
    return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/**
 * The subprotocols the data socket offers so that its handshake carries a
 * fragment token without a URL: `selkies`, which the server selects, then the
 * token behind `selkies.token.`, which the server reads and never echoes. A
 * query token rides the socket URL instead, so a page opened with one depends
 * on nothing new from a proxy in front.
 * @returns {string[]} The two subprotocols, or none without a fragment token.
 */
export function sessionTokenProtocols() {
    const { token } = readUrlFragment();
    if (!token) return [];
    return [SESSION_TOKEN_PROTOCOL, SESSION_TOKEN_PROTOCOL_PREFIX + base64url(token)];
}

/**
 * Mirrors the page's token into the API-scoped session cookie.
 *
 * Called once by each core at load. A page without a token leaves any
 * existing cookie alone, since another tab may still be using it; a cookie
 * write the browser blocks leaves the other carriers, which is why this is
 * best-effort.
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
