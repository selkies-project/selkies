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
 *
 * A multipart download arrives as `DECODE_BEGIN`, one `DECODE_CHUNK` per
 * message and `DECODE_END`, which alone answers; `DECODE_ABORT` drops an
 * unfinished one. Only that keeps the payload off the main thread entirely:
 * joining the chunks there first builds the whole base64 string, and then
 * copies it again into the message that carries it here.
 *
 * Every reply carrying bytes also carries `hash`, the change-detection digest
 * of those bytes, computed here because this is where they are already being
 * walked; the main thread would otherwise run the same per-byte loop over a
 * payload of any size. `HASH_BYTES` covers the one caller holding bytes that
 * did not come through a codec, and `REENCODE_PNG` normalizes an image to PNG
 * on an OffscreenCanvas rather than on the page's own.
 * @module
 */

/** Bytes per `String.fromCharCode.apply` call, keeping its argument list bounded. */
const CHUNK_SIZE = 0x8000;

/** Seed of the djb2 digest the clipboard compares payloads by. */
const HASH_SEED = 5381;

/**
 * djb2 over `bytes`, continuing from `h` so a digest can span several chunks.
 * @param {number} h
 * @param {Uint8Array} bytes
 * @returns {number}
 */
function hashBytes(h, bytes) {
    for (let i = 0; i < bytes.length; i++) h = ((h << 5) + h + bytes[i]) | 0;
    return h;
}

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

/**
 * Decodes as much of `text` as forms whole base64 quartets, appending the
 * bytes to `stream` and keeping the rest for the next chunk. A sender is free
 * to split its base64 anywhere, so a chunk boundary need not fall on one.
 * @param {{parts: Array<Uint8Array>, remainder: string, bytes: number}} stream
 * @param {string} text
 */
function feedStream(stream, text) {
    const pending = stream.remainder + text;
    const whole = pending.length - (pending.length % 4);
    stream.remainder = pending.slice(whole);
    if (!whole) return;
    const bytes = base64ToBytes(pending.slice(0, whole));
    stream.parts.push(bytes);
    stream.bytes += bytes.length;
    stream.hash = hashBytes(stream.hash, bytes);
}

/** The stream's bytes as one buffer, remainder included. */
function drainStream(stream) {
    if (stream.remainder) {
        const tail = base64ToBytes(stream.remainder);
        stream.parts.push(tail);
        stream.bytes += tail.length;
        stream.hash = hashBytes(stream.hash, tail);
        stream.remainder = '';
    }
    const all = new Uint8Array(stream.bytes);
    let at = 0;
    for (const part of stream.parts) {
        all.set(part, at);
        at += part.length;
    }
    stream.parts = [];
    return all;
}

/** Multipart downloads in progress, by request id. */
const streams = new Map();

/**
 * Normalizes an image to PNG on an OffscreenCanvas, which is the same decode
 * and encode the page would otherwise run on its own canvas and block on for
 * as long as the image is large.
 * @param {number} id Request id to answer.
 * @param {Blob} blob The image.
 */
async function reencodeToPng(id, blob) {
    let bmp = null;
    try {
        bmp = await createImageBitmap(blob);
        const canvas = new OffscreenCanvas(bmp.width, bmp.height);
        canvas.getContext('2d').drawImage(bmp, 0, 0);
        const png = await canvas.convertToBlob({ type: 'image/png' });
        self.postMessage({ id, success: true, result: png, mimeType: 'image/png',
                           byteLength: png.size });
    } catch (err) {
        self.postMessage({ id, success: false, error: err.message });
    } finally {
        if (bmp) bmp.close();
    }
}

/** Dispatches one bridge request and posts its reply. */
self.onmessage = function(e) {
    const { id, action, payload, mimeType } = e.data;

    try {
        if (action === 'DECODE_BEGIN') {
            streams.set(id, { mimeType, parts: [], remainder: '', bytes: 0, hash: HASH_SEED });
            return;
        }
        if (action === 'DECODE_CHUNK') {
            const stream = streams.get(id);
            if (stream) feedStream(stream, payload);
            return;
        }
        if (action === 'DECODE_ABORT') {
            streams.delete(id);
            return;
        }
        if (action === 'DECODE_END') {
            const stream = streams.get(id);
            streams.delete(id);
            if (!stream) {
                self.postMessage({ id, success: false, error: 'no transfer in progress' });
                return;
            }
            const bytes = drainStream(stream);
            if (stream.mimeType === 'text/plain') {
                self.postMessage({ id, success: true, mimeType: stream.mimeType,
                                   result: new TextDecoder().decode(bytes),
                                   byteLength: bytes.byteLength, hash: stream.hash });
            } else {
                self.postMessage(
                    { id, success: true, mimeType: stream.mimeType,
                      result: bytes.buffer, byteLength: bytes.byteLength,
                      hash: stream.hash },
                    [bytes.buffer]);
            }
            return;
        }
        if (action === 'HASH_BYTES') {
            const bytes = new Uint8Array(payload);
            self.postMessage({ id, success: true, byteLength: bytes.byteLength,
                               hash: hashBytes(HASH_SEED, bytes) });
            return;
        }
        if (action === 'REENCODE_PNG') {
            reencodeToPng(id, payload);
            return;
        }
        if (action === 'ENCODE_BINARY_TO_B64') {
            const bytes = new Uint8Array(payload);
            const base64 = bytesToBase64(bytes);
            self.postMessage({ id, success: true, result: base64,
                               byteLength: bytes.byteLength,
                               hash: hashBytes(e.data.hash === undefined ? HASH_SEED : e.data.hash, bytes) });
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
                    { id, success: true, result: bytes.buffer, mimeType,
                      byteLength: bytes.byteLength, hash: hashBytes(HASH_SEED, bytes) },
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