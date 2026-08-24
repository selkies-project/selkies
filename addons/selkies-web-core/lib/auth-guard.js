/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0 (the "License"); you may not use this file except in
 * compliance with the License. A copy of the License is at
 * https://mozilla.org/MPL/2.0/.
 */

/**
 * Page reload on an authentication wall that moves under a live page.
 *
 * When basic auth is enabled on the server or proxy, or a session expires,
 * the stream either dead-stalls or keeps streaming while every API call
 * returns 401. Either way only a fresh document can re-present the login, so
 * any observed same-origin 401 reloads the page once. The server's own Bearer
 * verdicts are exempt: a reload cannot change them. `installAuthGuard` wraps
 * `window.fetch` for the whole page (cores and dashboards share it) and
 * publishes `window.__selkiesAuthProbe`, which socket close handlers call so
 * a WebSocket dropped by the auth wall triggers the same reload instead of
 * silently retrying.
 *
 * Reloads are chained through a stamp that survives them: a deployment that
 * serves the page without auth but keeps API routes permanently 401 (an
 * expired app cookie, split forward-auth) would otherwise reload forever.
 * Two reloads inside the window still re-present an auth wall when the
 * document itself is challenged, since that path self-terminates, while a
 * permanent API-only 401 stops reloading. The stamp lives in sessionStorage,
 * falling back to `history.state` where storage is blocked (private modes,
 * cookie-blocked embeds) because that rides the session-history entry
 * through `location.reload()`.
 * @module
 */

let installed = false;

/**
 * Installs the fetch wrapper and the probe once per page.
 *
 * The server's token verdicts (secure mode: a route wanting a session or
 * master token) name the Bearer scheme in `WWW-Authenticate`; a reload would
 * re-present the very same token, so those are left to the caller (the
 * file-upload error, the mode-switch master-token prompt).
 */
export function installAuthGuard() {
    if (installed || typeof window === 'undefined') return;
    installed = true;
    const realFetch = window.fetch.bind(window);
    const STAMP_KEY = '__selkiesAuthReloadChain';
    const WINDOW_MS = 30000;
    const MAX_CHAIN = 2;
    const readStamp = () => {
        try {
            const s = JSON.parse(sessionStorage.getItem(STAMP_KEY) || 'null');
            if (s) return s;
        } catch (_) { /* storage blocked */ }
        try {
            const h = window.history.state;
            if (h && typeof h === 'object' && h[STAMP_KEY]) return h[STAMP_KEY];
        } catch (_) { /* history unavailable */ }
        return null;
    };
    const writeStamp = (stamp) => {
        let stored = false;
        try {
            sessionStorage.setItem(STAMP_KEY, JSON.stringify(stamp));
            stored = true;
        } catch (_) { /* storage blocked */ }
        if (stored) return;
        try {
            const h = window.history.state;
            if (h == null || (typeof h === 'object' && !Array.isArray(h))) {
                window.history.replaceState(
                    Object.assign({}, h || {}, { [STAMP_KEY]: stamp }), '');
            }
        } catch (_) { /* history unavailable */ }
    };
    let chain = 0;
    const stamp = readStamp();
    if (stamp && Date.now() - stamp.at < WINDOW_MS) chain = stamp.n | 0;
    let reloading = false;
    let capWarned = false;
    const reloadOnce = () => {
        if (reloading || chain >= MAX_CHAIN) {
            if (!reloading && !capWarned) {
                capWarned = true;
                console.warn('auth-guard: 401 persists after reloads; leaving the page as-is instead of looping.');
            }
            return;
        }
        reloading = true;
        writeStamp({ at: Date.now(), n: chain + 1 });
        window.location.reload();
    };
    // A third-party API's 401 must not reload the page.
    const sameOrigin = (input) => {
        try {
            const url = (typeof input === 'string' || input instanceof URL)
                ? String(input) : input.url;
            return new URL(url, window.location.href).origin === window.location.origin;
        } catch (_) {
            return true;
        }
    };
    const isTokenVerdict = (res) => {
        try {
            return /^Bearer realm="Selkies/i.test(res.headers.get('WWW-Authenticate') || '');
        } catch (_) {
            return false;
        }
    };
    window.__selkiesAuthReload = reloadOnce;
    window.fetch = async (...args) => {
        const res = await realFetch(...args);
        if (res.status === 401 && sameOrigin(args[0]) && !isTokenVerdict(res)) reloadOnce();
        return res;
    };
    // The probe's fetch goes through the guard above: it either proves a 401
    // and reloads, or quietly does nothing.
    window.__selkiesAuthProbe = () => {
        try {
            window.fetch(new Request(window.location.href, {
                method: 'HEAD',
                cache: 'no-store',
                credentials: 'same-origin',
            })).catch(() => {});
        } catch (_) { /* fetch unavailable */ }
    };
}
