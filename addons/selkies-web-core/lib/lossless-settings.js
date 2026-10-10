/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Dashboard state for optional lossless stills. The preference remains separate
 * from the capture and presentation capability reported by the running core.
 * @module
 */

/**
 * @typedef {object} LosslessStatus
 * @property {boolean} supported
 * @property {boolean} effective
 * @property {string} reason
 */

/** Read the running core's last status without synthesizing capability. */
export function readLosslessStatus() {
    const host = /** @type {Window & {losslessRefinementStatus?: LosslessStatus}} */ (window);
    return host.losslessRefinementStatus || null;
}

/**
 * Subscribe to status changes from this page's core.
 * @param {() => void} listener
 * @returns {() => void}
 */
export function subscribeLosslessStatus(listener) {
    const receive = (event) => {
        if (event.source === window && event.origin === window.location.origin
            && event.data?.type === 'losslessRefinementStatus') listener();
    };
    window.addEventListener('message', receive);
    return () => window.removeEventListener('message', receive);
}

const REASONS = new Set(['ready', 'disabled', 'waiting-for-server', 'disconnected',
    'paint-over-disabled', 'capture-unavailable', 'full-frame-required', 'canvas-required',
    'canvas-sink-required', 'png-decoder-unavailable', 'webrtc-unavailable', 'page-hidden',
    'video-stopped']);

/**
 * Render the applied state while retaining a disabled display's saved preference.
 * @param {LosslessStatus|null} status
 * @param {boolean} requested
 * @param {boolean} parent
 * @returns {{checked: boolean, available: boolean, reason: string}}
 */
export function losslessSettingState(status, requested, parent) {
    const available = !!status?.supported;
    let reason = !parent ? 'paint-over-disabled' : !status ? 'waiting-for-server'
        : !available ? status.reason : !requested ? 'disabled'
        : status.effective ? 'ready' : status.reason === 'disabled' ? 'waiting-for-server' : status.reason;
    if (!REASONS.has(reason) || (!available && (reason === 'ready' || reason === 'disabled'))) reason = 'unavailable';
    return { checked: !!(parent && requested && available && status?.effective), available, reason };
}
