/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The clipboard worker's multipart accumulation. A download reaches it one
// message at a time so the page never holds the whole base64 payload, which
// means the worker owns what used to be a join on the main thread: chunk
// boundaries fall wherever the sender put them, and two transfers can be open
// at once.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

const replies = [];
globalThis.self = { postMessage: (msg) => replies.push(msg) };
await import('../../addons/selkies-web-core/clipboard-worker.js');
const worker = globalThis.self;

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [clip-stream] ${label}  ${detail}`);
}

function send(data) {
    worker.onmessage({ data });
}

function base64(bytes) {
    let s = '';
    for (const b of bytes) s += String.fromCharCode(b);
    return btoa(s);
}

/** Splits `text` into runs of the given lengths, the remainder last. */
function split(text, size) {
    const out = [];
    for (let at = 0; at < text.length; at += size) out.push(text.slice(at, at + size));
    return out;
}

/** Runs one transfer and returns the worker's reply. */
function transfer(id, mimeType, chunks) {
    replies.length = 0;
    send({ id, action: 'DECODE_BEGIN', mimeType });
    for (const chunk of chunks) send({ id, action: 'DECODE_CHUNK', payload: chunk });
    send({ id, action: 'DECODE_END' });
    return replies[replies.length - 1];
}

const payload = new Uint8Array(3000);
for (let i = 0; i < payload.length; i++) payload[i] = (i * 31 + 7) & 0xff;
const encoded = base64(payload);

function bytesOf(reply) {
    return new Uint8Array(reply.result);
}

function same(a, b) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
    return true;
}

// The sender's own chunking: whole quartets, so each arrives decodable alone.
{
    const reply = transfer(1, 'image/png', split(encoded, 400));
    check('chunks on quartet boundaries decode to the payload',
          reply.success && same(bytesOf(reply), payload) && reply.byteLength === payload.length,
          `${reply.byteLength} bytes`);
}

// A boundary anywhere else: the tail of one chunk completes with the next.
for (const size of [1, 7, 401, 999]) {
    const reply = transfer(2, 'image/png', split(encoded, size));
    check(`chunks of ${size} chars decode to the payload`,
          reply.success && same(bytesOf(reply), payload), `${reply.byteLength} bytes`);
}

// Text comes back decoded, with the byte count the size check compares.
{
    const text = 'clipboard text with a multi-byte character: ééé';
    const bytes = new TextEncoder().encode(text);
    const reply = transfer(3, 'text/plain', split(base64(bytes), 5));
    check('a text transfer comes back as text',
          reply.success && reply.result === text && reply.byteLength === bytes.length,
          `${reply.byteLength} bytes`);
}

// Interleaved transfers: each id keeps its own bytes.
{
    replies.length = 0;
    const other = new Uint8Array([1, 2, 3, 4, 5, 6, 7]);
    send({ id: 10, action: 'DECODE_BEGIN', mimeType: 'image/png' });
    send({ id: 11, action: 'DECODE_BEGIN', mimeType: 'image/png' });
    for (const chunk of split(encoded, 37)) send({ id: 10, action: 'DECODE_CHUNK', payload: chunk });
    send({ id: 11, action: 'DECODE_CHUNK', payload: base64(other) });
    send({ id: 11, action: 'DECODE_END' });
    send({ id: 10, action: 'DECODE_END' });
    const first = replies.find((r) => r.id === 11);
    const second = replies.find((r) => r.id === 10);
    check('two open transfers keep their own bytes',
          same(bytesOf(first), other) && same(bytesOf(second), payload),
          `${first.byteLength} and ${second.byteLength} bytes`);
}

// An aborted transfer leaves nothing behind for a later end to answer with.
{
    replies.length = 0;
    send({ id: 20, action: 'DECODE_BEGIN', mimeType: 'image/png' });
    send({ id: 20, action: 'DECODE_CHUNK', payload: encoded });
    send({ id: 20, action: 'DECODE_ABORT' });
    send({ id: 20, action: 'DECODE_END' });
    const reply = replies[replies.length - 1];
    check('an aborted transfer answers nothing but a failure',
          reply.success === false, JSON.stringify(reply));
}

console.log(`[clip-stream] ${failed === 0 ? 'all checks passed' : failed + ' failed'}`);
process.exit(failed === 0 ? 0 : 1);
