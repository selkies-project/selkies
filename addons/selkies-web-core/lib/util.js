/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Helpers shared by the streaming cores and both dashboards: a small FIFO
 * queue, the human-readable labels for wire values, the WebSocket decodability
 * check, and the route prefix and localStorage namespace every caller derives
 * the same way.
 * @module
 */

/** A FIFO queue over an array. */
export class Queue {
    /** @param {...*} elements Initial items, enqueued in order. */
    constructor(...elements) {
        /** @type {Array} */
        this.items = [];

        this.enqueue(...elements);
    }

    /** @param {...*} elements Items appended in order. */
    enqueue(...elements) {
        elements.forEach(element => this.items.push(element));
    }

    /**
     * Removes the oldest `count` items.
     * @param {number} [count=1] How many items to drop.
     * @returns {*} The oldest of the removed items.
     */
    dequeue(count=1) {
        return this.items.splice(0, count)[0];
    }

    /** @returns {number} */
    size() {
        return this.items.length;
    }

    /** @returns {boolean} */
    isEmpty() {
        return this.items.length===0;
    }

    /** @returns {Array} A copy of the items, oldest first. */
    toArray() {
        return [...this.items]
    }

    /** Removes the first occurrence of `element`. */
    remove(element) {
        var index = this.items.indexOf(element)
        this.items.splice(index, 1)
    }

    /** @returns {boolean} Whether `element` is queued. */
    find(element) {
        return this.items.indexOf(element) == -1 ? false: true;
    }

    /** Drops every item. */
    clear(){
        this.items.length = 0;
    }
}

/**
 * Human-readable names for the wire values surfaced in UIs (transport modes,
 * encoders, rate-control modes). The raw values are what the server APIs
 * speak and stay untouched; unknown values fall through unchanged so new wire
 * values render as-is. Locale-invariant technical terms, so they live here
 * once rather than in every dashboard's translation dictionaries.
 */
export const DISPLAY_LABELS = {
    websockets: "WebSockets",
    webrtc: "WebRTC",
    h264enc: "H.264 (Full Frame)",
    "h264enc-striped": "H.264 (Striped Frame)",
    jpeg: "JPEG (Striped Frame)",
    cbr: "CBR (Constant Bitrate)",
    crf: "CRF (Constant Quality)",
    auto: "Auto",
    h264: "H.264",
    vp8: "VP8",
    mjpeg: "MJPEG",
};

/**
 * @param {string} value A wire value.
 * @returns {string} Its display label, or the value itself when it has none.
 */
export const displayLabel = (value) => DISPLAY_LABELS[value] ?? value;

/**
 * Whether this engine can play an encoder on the WebSocket transport: every
 * H.264 mode decodes through WebCodecs' `VideoDecoder`, while `jpeg` (striped
 * JPEG painted through `createImageBitmap`) needs nothing. An engine without
 * WebCodecs therefore still streams: the core's pre-flight pins `jpeg` instead
 * of failing, and the settings offer nothing it cannot play. The WebRTC
 * transport decodes in the browser's media stack and is not subject to this.
 * @param {string} encoder An encoder wire value.
 * @returns {boolean}
 */
export const canDecodeEncoder = (encoder) => encoder === "jpeg" || typeof VideoDecoder !== "undefined";
/**
 * @param {string[]} encoders Encoder wire values.
 * @returns {string[]} Those `canDecodeEncoder` accepts.
 */
export const decodableEncoders = (encoders) => encoders.filter(canDecodeEncoder);

/**
 * Directory this document is served from, without a trailing slash (`''` at
 * the server root). Every request the client builds hangs off it, so a
 * deployment reverse-proxied under a subfolder reaches its own routes, and an
 * iframed client reads its own path instead of the frame's.
 * @returns {string} The path prefix, e.g. `/desk`.
 */
export function getRoutePrefix() {
    const pathname = window.location.pathname;
    const dirPath = pathname.substring(0, pathname.lastIndexOf('/') + 1);
    return dirPath.replace(/\/$/, '');
}

/**
 * The localStorage namespace every stored key is prefixed with.
 *
 * Origin and pathname only, not the full URL: a per-session `?token=` must
 * not mint a new namespace on each connect. Cores and dashboards share one
 * prefix, so this derivation is the single one they all call.
 * @returns {string} Sanitized namespace, empty outside a browser.
 */
export function getStorageAppName() {
    if (typeof window === 'undefined') return '';
    const urlForKey = window.location.origin + window.location.pathname;
    return urlForKey.replace(/[^a-zA-Z0-9._-]/g, '_');
}
