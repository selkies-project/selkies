/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Base64 codec for clipboard payloads, run off the main thread.
 *
 * `lib/clipboard-worker-bridge.js` posts `{id, action, payload, mimeType}`
 * and receives `{id, success, result, mimeType, byteLength}` or
 * `{id, success: false, error}`. `ENCODE_BINARY_TO_B64` takes an
 * ArrayBuffer, `ENCODE_TEXT_TO_B64` a string, both answering with the base64
 * text; `DECODE_FROM_B64` takes base64 and answers with a string for
 * `text/plain`, else with a transferred ArrayBuffer. `byteLength` is the
 * decoded size, which the bridge reports and compares against limits.
 * @module
 */

/** Bytes per `String.fromCharCode.apply` call, keeping its argument list bounded. */
const CHUNK_SIZE = 0x8000;

/**
 * @param {string} text
 * @returns {string} Base64 of the UTF-8 encoding of `text`.
 */
function stringToBase64(text) {
    const bytes = new TextEncoder().encode(text);
    let binString = "";
    for (let i = 0; i < bytes.length; i += CHUNK_SIZE) {
        const chunk = bytes.subarray(i, i + CHUNK_SIZE);
        binString += String.fromCharCode.apply(null, chunk);
    }
    return btoa(binString);
}

/**
 * @param {string} base64
 * @returns {{text: string, byteLength: number}} The decoded UTF-8 text and its byte size.
 */
function base64ToString(base64) {
    const binString = atob(base64);
    const len = binString.length;
    const bytes = new Uint8Array(len);
    for (let i = 0; i < len; i++) {
        bytes[i] = binString.charCodeAt(i);
    }
    return { text: new TextDecoder().decode(bytes), byteLength: len };
}

/**
 * @param {string} base64
 * @returns {Uint8Array}
 */
function base64ToBytes(base64) {
    const binString = atob(base64);
    const len = binString.length;
    const bytes = new Uint8Array(len);
    for (let i = 0; i < len; i++) {
        bytes[i] = binString.charCodeAt(i);
    }
    return bytes;
}

/**
 * @param {Uint8Array} bytes
 * @returns {string}
 */
function bytesToBase64(bytes) {
    let binString = "";
    for (let i = 0; i < bytes.length; i += CHUNK_SIZE) {
        const chunk = bytes.subarray(i, i + CHUNK_SIZE);
        binString += String.fromCharCode.apply(null, chunk);
    }
    return btoa(binString);
}

/** Dispatches one bridge request and posts its reply. */
self.onmessage = function(e) {
    const { id, action, payload, mimeType } = e.data;

    try {
        if (action === 'ENCODE_BINARY_TO_B64') {
            const bytes = new Uint8Array(payload);
            const base64 = bytesToBase64(bytes);
            self.postMessage({ id, success: true, result: base64 });
        } 
        else if (action === 'ENCODE_TEXT_TO_B64') {
            const base64 = stringToBase64(payload);
            self.postMessage({ id, success: true, result: base64 });
        }
        else if (action === 'DECODE_FROM_B64') {
            if (mimeType === 'text/plain') {
                const { text, byteLength } = base64ToString(payload);
                self.postMessage({ id, success: true, result: text, mimeType, byteLength });
            } else {
                const bytes = base64ToBytes(payload);
                self.postMessage(
                    { id, success: true, result: bytes.buffer, mimeType, byteLength: bytes.byteLength }, 
                    [bytes.buffer] 
                );
            }
        } else {
            self.postMessage({ id, success: false, error: `Unknown action: ${action}` });
        }
    } catch (err) {
        self.postMessage({ id, success: false, error: err.message });
    }
};