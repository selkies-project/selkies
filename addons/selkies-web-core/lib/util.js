/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

export class Queue {
    /**
     * @constructor
     * @param {Array}
     *    Video element to attach events to
     */
    constructor(...elements) {
        /**
         * @type {Array}
         */
        this.items = [];

        this.enqueue(...elements);
    }

    enqueue(...elements) {
        elements.forEach(element => this.items.push(element));
    }

    dequeue(count=1) {
        return this.items.splice(0, count)[0];
    }

    size() {
        return this.items.length;
    }

    isEmpty() {
        return this.items.length===0;
    }

    toArray() {
        return [...this.items]
    }

    remove(element) {
        var index = this.items.indexOf(element)
        this.items.splice(index, 1)
    }

    find(element) {
        return this.items.indexOf(element) == -1 ? false: true;
    }

    clear(){
        this.items.length = 0;
    }
}

// Human-readable names for the wire values surfaced in UIs (transport modes,
// encoders, rate-control modes). The raw values are what the server APIs speak
// and must stay untouched; unknown values fall through unchanged so new wire
// values render as-is instead of breaking. Locale-invariant technical terms,
// so they live here once rather than in every dashboard's translation dicts.
export const DISPLAY_LABELS = {
    websockets: "WebSockets",
    webrtc: "WebRTC",
    h264enc: "H.264 (Full Frame)",
    "h264enc-striped": "H.264 (Striped Frame)",
    openh264enc: "H.264 (OpenH264)",
    jpeg: "JPEG (Striped Frame)",
    cbr: "CBR (Constant Bitrate)",
    crf: "CRF (Constant Quality)",
};

/** @param {string} value @returns {string} */
export const displayLabel = (value) => DISPLAY_LABELS[value] ?? value;
