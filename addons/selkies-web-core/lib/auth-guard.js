/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0 (the "License"); you may not use this file except in
 * compliance with the License. A copy of the License is at
 * https://mozilla.org/MPL/2.0/.
 */

// Auth moving under a live page (basic auth enabled on the server/proxy, or a
// session expiring): the stream either dead-stalls or keeps streaming while
// every API call 401s. Either way only a fresh document can re-present the
// login, so any observed 401 reloads the page once. installAuthGuard wraps
// window.fetch for the whole page (cores and dashboards share it); close
// handlers probe the origin once so a WS dropped BY the auth wall triggers
// the same reload instead of silently retrying.

let installed = false;

export function installAuthGuard() {
    if (installed || typeof window === 'undefined') return;
    installed = true;
    const realFetch = window.fetch.bind(window);
    // Loop breaker: a deployment that serves the page without auth but keeps
    // API routes permanently 401 (expired app cookie, split forward-auth)
    // would reload forever — the counter is per-document otherwise. Two
    // reloads inside the window still re-present an auth wall when the
    // document itself is challenged (that path self-terminates), while a
    // permanent API-only 401 stops reloading instead of spinning.
    const STAMP_KEY = '__selkiesAuthReloadChain';
    const WINDOW_MS = 30000;
    const MAX_CHAIN = 2;
    // The chain stamp must survive the reloads it counts even where storage
    // is blocked (private modes, cookie-blocked embeds): history.state rides
    // the session-history entry through location.reload(), so it is the
    // fallback — without one, every fresh document restarts at 0 and a
    // permanent API-only 401 reloads forever.
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
    // Only a same-origin 401 means this server's auth wall moved; a
    // third-party API's 401 must not reload the page.
    const sameOrigin = (input) => {
        try {
            const url = (typeof input === 'string' || input instanceof URL)
                ? String(input) : input.url;
            return new URL(url, window.location.href).origin === window.location.origin;
        } catch (_) {
            return true;
        }
    };
    window.__selkiesAuthReload = reloadOnce;
    window.fetch = async (...args) => {
        const res = await realFetch(...args);
        if (res.status === 401 && sameOrigin(args[0])) reloadOnce();
        return res;
    };
    // A close-driven probe: the next fetch going through the guard above
    // either proves a 401 (and reloads) or quietly does nothing.
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
