/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/** Explicit opt-in, applied state, and per-display holds for both dashboards. @module */
import assert from 'node:assert/strict';
import { LOSSLESS_STATIC_REFINEMENT_SPEC as spec, USE_PAINT_OVER_QUALITY_SPEC as parentSpec,
    resolveSpec } from '../../addons/selkies-web-core/lib/conditional-settings.js';
import { losslessSettingState, readLosslessStatus, subscribeLosslessStatus } from '../../addons/selkies-web-core/lib/lossless-settings.js';
import { holdDisplaySettings, releaseHeldSettings, restoreHeldPicks } from '../../addons/selkies-web-core/lib/held-settings.js';
import { getTranslator } from '../../addons/selkies-dashboard/src/translations.js';

let passed = 0;
/** Run one assertion group and fail the process if its contract is violated. */
function check(name, test) {
    test();
    passed++;
    console.log(`PASS [lossless-settings] ${name}`);
}

/** A storage implementation with the browser's null-for-absent behavior. */
function storage() {
    const entries = new Map();
    return { getItem: (key) => entries.has(key) ? entries.get(key) : null,
        setItem: (key, value) => entries.set(key, String(value)), removeItem: (key) => entries.delete(key) };
}
const local = storage(), session = storage(), listeners = new Set();
globalThis.window = { localStorage: local, sessionStorage: session, location: { origin: 'https://test.invalid' },
    addEventListener: (_name, fn) => listeners.add(fn), removeEventListener: (_name, fn) => listeners.delete(fn) };
const keyFor = (key) => `test_${key}_display2`;
const readExplicit = (key) => local.getItem(`${keyFor(key)}_explicit_choice`) === 'true' ? local.getItem(keyFor(key)) : null;
const resolve = (server = null, context = {}) => resolveSpec(spec, server, context, readExplicit);

check('missing preference and server echoes remain off', () => {
    assert.equal(resolve(), false);
    local.setItem(keyFor(spec.storageKey), 'true');
    assert.equal(resolve({ [spec.serverKey]: { value: true, locked: false, overridden: false } }), false);
    for (const encoder of ['jpeg', 'h264enc', 'h264enc-striped']) {
        for (const videoStreamingMode of [true, false]) assert.equal(resolve(null, { encoder, videoStreamingMode }), false);
    }
});
check('explicit per-display choice, operator override, and lock have precedence', () => {
    const server = { [spec.serverKey]: { value: true, overridden: true } };
    assert.equal(resolve(server), true);
    local.setItem(`${keyFor(spec.storageKey)}_explicit_choice`, 'true');
    local.setItem(keyFor(spec.storageKey), 'false');
    assert.equal(resolve(server), false);
    assert.equal(resolve({ [spec.serverKey]: { value: true, locked: true } }), true);
    local.setItem(`test_${spec.storageKey}`, 'true');
    assert.equal(resolve(), false);
});
check('toggle posts immediately and parent inhibition precedes its setting batch', () => {
    const calls = [];
    const io = { postToCore: (message) => calls.push(message), postSetting: (settings) => calls.push(settings) };
    spec.propagate(false, {}, io);
    parentSpec.propagate(false, {}, io);
    assert.deepEqual(calls, [ { type: 'setLosslessStaticRefinement', enabled: false },
        { type: 'setLosslessParentState', enabled: false }, { use_paint_over_quality: false } ]);
});
check('unsupported and parent-off never display an active toggle', () => {
    const ready = { supported: true, effective: true, reason: 'ready' };
    assert.equal(losslessSettingState(ready, true, false).checked, false);
    assert.equal(losslessSettingState(ready, false, true).checked, false);
    assert.equal(losslessSettingState(ready, true, true).checked, true);
    const rtc = losslessSettingState({ supported: false, effective: true, reason: 'webrtc-unavailable' }, true, true);
    assert.deepEqual(rtc, { checked: false, available: false, reason: 'webrtc-unavailable' });
    assert.equal(losslessSettingState(null, true, true).reason, 'waiting-for-server');
    assert.equal(losslessSettingState({ supported: false, effective: false, reason: 'ready' }, true, true).reason, 'unavailable');
});
check('status subscription accepts only this page and releases its listener', () => {
    assert.equal(readLosslessStatus(), null);
    let notifications = 0;
    const unsubscribe = subscribeLosslessStatus(() => notifications++);
    const callback = [...listeners][0];
    const data = { type: 'losslessRefinementStatus' };
    callback({ source: {}, origin: window.location.origin, data });
    callback({ source: window, origin: 'https://other.invalid', data });
    assert.equal(notifications, 0);
    window.losslessRefinementStatus = { supported: false, effective: false, reason: 'disconnected' };
    callback({ source: window, origin: window.location.origin, data });
    assert.equal(notifications, 1);
    assert.equal(readLosslessStatus().reason, 'disconnected');
    unsubscribe();
    assert.equal(listeners.size, 0);
});
check('held display state preserves and restores the explicit preference', () => {
    local.setItem(keyFor(spec.storageKey), 'false');
    holdDisplaySettings(keyFor, 'test', { [spec.storageKey]: true }, 'shared display');
    assert.equal(local.getItem(`${keyFor(spec.storageKey)}_pick`), 'false');
    assert.equal(resolve(), true);
    session.removeItem('test_display_settings_tab');
    restoreHeldPicks(keyFor, 'test');
    assert.equal(resolve(), false);
    holdDisplaySettings(keyFor, 'test', { [spec.storageKey]: true }, 'shared display');
    releaseHeldSettings(keyFor, [spec.storageKey]);
    assert.equal(local.getItem(`${keyFor(spec.storageKey)}_pick`), null);
});
check('both dashboards resolve labels and precise unavailable reasons', () => {
    for (const locale of ['en', 'es', 'zh_cn', 'zh_tw', 'hi', 'pt', 'fr', 'ru', 'de', 'tr', 'it', 'nl', 'ar', 'ko', 'ja', 'vi', 'th', 'fil', 'da']) {
        const { t } = getTranslator(locale);
        for (const key of ['label', 'help', 'reasons.webrtc-unavailable', 'reasons.capture-unavailable']) {
            assert.notEqual(t(`losslessStatic.${key}`), `losslessStatic.${key}`);
        }
    }
});
console.log(`[lossless-settings] ${passed}/${passed} passed`);
