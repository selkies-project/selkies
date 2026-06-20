/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import webrtc from "./selkies-wr-core";
import websockets from "./selkies-ws-core";

const STREAM_MODE_WEBRTC = "webrtc";
const STREAM_MODE_WEBSOCKETS = "websockets";

// Storage key namespace: origin + pathname only, NOT the full URL — a per-session
// ?token=... in the query string must not mint a new localStorage namespace on every
// connect (that leak eventually exhausts the origin quota and blanks the iframe).
// Must match selkies-ws-core.js / selkies-wr-core.js / the dashboard Sidebar.
const urlForKey = window.location.origin + window.location.pathname;
const storageAppName = urlForKey.replace(/[^a-zA-Z0-9._-]/g, '_');
const getPrefixedKey = (key) => {return `${storageAppName}_${key}`}
// Guarded write: a full or unavailable store degrades to a warning instead of
// throwing QuotaExceededError into startup.
const safeSetItem = (key, value) => {
    try {
        localStorage.setItem(key, value);
    } catch (e) {
        console.warn(`Selkies: could not persist '${key}' to localStorage:`, e);
    }
};

// One-time migration/cleanup covering both legacy key schemes:
//   (a) keys from the old sanitizer (a `.-_` char range bug kept ?/=/: literal) AND the
//       old full-href derivation — query-less ones migrate to the new prefix, while
//       token-scoped ones (stable base + literal '?') are PRUNED, which also recovers
//       browsers whose store the token leak already filled (removeItem never hits quota);
//   (b) fixed-regex keys derived from a query-less href already equal the new prefix.
(function migrateStorageKeys() {
    try {
        if (typeof localStorage === 'undefined') return;
        // Legacy prefix: old sanitizer kept a-z and 0x2E-0x5F; char codes avoid a regex range.
        let oldAppName = '';
        for (let i = 0; i < urlForKey.length; i++) {
            const c = urlForKey.charCodeAt(i);
            oldAppName += ((c >= 0x2E && c <= 0x5F) || (c >= 0x61 && c <= 0x7A)) ? urlForKey[i] : '_';
        }
        // Prune token-scoped legacy keys every load (cheap, idempotent, frees quota
        // before any writes below).
        const tokenPrefix = oldAppName + '?';
        const staleKeys = [];
        for (let i = 0; i < localStorage.length; i++) {
            const k = localStorage.key(i);
            if (k && k.startsWith(tokenPrefix)) staleKeys.push(k);
        }
        staleKeys.forEach((k) => localStorage.removeItem(k));
        if (staleKeys.length) {
            console.log(`Selkies: removed ${staleKeys.length} stale token-scoped localStorage keys.`);
        }
        // No-op when the buggy regex produced the same prefix (nothing to migrate).
        if (oldAppName === storageAppName) return;
        const migratedFlagKey = `${storageAppName}_storage_key_migrated`;
        if (localStorage.getItem(migratedFlagKey) !== null) return; // already migrated

        const oldPrefix = `${oldAppName}_`;
        const newPrefix = `${storageAppName}_`;

        // Snapshot keys first; we mutate localStorage inside the loop.
        const allKeys = [];
        for (let i = 0; i < localStorage.length; i++) {
            const k = localStorage.key(i);
            if (k !== null) allKeys.push(k);
        }
        // Only migrate if NEW-prefixed keys are absent but OLD-prefixed keys exist,
        // so we never clobber settings the user already saved under the new prefix.
        const hasNew = allKeys.some((k) => k.startsWith(newPrefix));
        const oldKeys = allKeys.filter((k) => k.startsWith(oldPrefix));
        if (!hasNew && oldKeys.length > 0) {
            for (const oldKey of oldKeys) {
                const suffix = oldKey.slice(oldPrefix.length);
                const newKey = newPrefix + suffix;
                if (localStorage.getItem(newKey) === null) {
                    const val = localStorage.getItem(oldKey);
                    if (val !== null) safeSetItem(newKey, val);
                }
            }
            console.log(`Migrated ${oldKeys.length} setting(s) from old storage prefix "${oldPrefix}" to "${newPrefix}".`);
        }
        // Guard so this runs at most once, regardless of whether anything was copied.
        safeSetItem(migratedFlagKey, '1');
    } catch (e) {
        console.warn('Storage key migration skipped due to error:', e);
    }
})();

let mode = null;

function determineStreamingMode() {
    // Check for runtime injected mode
    const runtimeMode = (typeof window !== 'undefined' && window.__SELKIES_STREAMING_MODE__) ? window.__SELKIES_STREAMING_MODE__ : undefined;
    let lastSessionMode = localStorage.getItem(getPrefixedKey('stream_mode'));
    // Precedence: runtime mode > last session mode > default mode
    const finalMode = runtimeMode ? runtimeMode : (lastSessionMode ? lastSessionMode : STREAM_MODE_WEBSOCKETS);
    console.log(`Streaming mode determined to be: ${finalMode}`);
    return finalMode;
}

function handleMessage(event) {
    let message = event.data;
    if (message.mode !== undefined && message.type === "mode") {
        console.log(`Switching streaming mode to: ${message.mode}`);
        safeSetItem(getPrefixedKey('stream_mode'), message.mode);

        // wait for a few seconds to let the server switch modes
        setTimeout(() => {
            // A full reload swaps the transport stacks; the cores are not built
            // for an in-place mode hand-off.
            window.location.reload();
        }, 2000)
    }
}

function switchStreamingMode(newMode) {
    safeSetItem(getPrefixedKey('stream_mode'), newMode);
    switch (newMode) {
        case STREAM_MODE_WEBRTC:
            mode = webrtc();
            mode.initialize();
            break;
        case STREAM_MODE_WEBSOCKETS:
            mode = websockets();
            break;
        default:
            throw new Error(`Invalid client mode: ${newMode} received, aborting`);
    }
}

if (typeof window !== 'undefined') {
    window.addEventListener("message", handleMessage)
    window.selkiesCoreInitialize = function() {
        const streamingMode = determineStreamingMode();
        switchStreamingMode(streamingMode);
    };
}

// Auto-initialize for backward compatibility when script is loaded directly
// This preserves existing behavior for non-dashboard usage
if (typeof window !== 'undefined' && !window.__SELKIES_DEFER_INITIALIZATION) {
    window.selkiesCoreInitialize();
}
