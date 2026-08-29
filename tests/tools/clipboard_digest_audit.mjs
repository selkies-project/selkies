/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The digest the clipboard worker takes while it codes a payload, which stands
// in for the payload's bytes wherever a signature is taken. Change detection
// rests on it matching what the page computes for the same bytes, over a
// payload delivered whole, split at arbitrary boundaries, or sent as chunks.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

const replies = [];
globalThis.self = { postMessage: (msg) => replies.push(msg) };
await import('../../addons/selkies-web-core/clipboard-worker.js');
const worker = globalThis.self;
const { digestedPayload } = await import('../../addons/selkies-web-core/lib/clipboard-sync.js');

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [clip-digest] ${label}  ${detail}`);
}

function send(data) {
    worker.onmessage({ data });
}

function base64(bytes) {
    let s = '';
    for (const b of bytes) s += String.fromCharCode(b);
    return btoa(s);
}

/** The page's own digest, which the worker's has to reproduce exactly. */
function pageHash(bytes) {
    let h = 5381;
    for (let i = 0; i < bytes.length; i++) h = ((h << 5) + h + bytes[i]) | 0;
    return h;
}

function reply(id) {
    return replies.find((m) => m.id === id);
}

const payload = new Uint8Array(300000);
for (let i = 0; i < payload.length; i++) payload[i] = (i * 31 + (i >> 8)) & 255;
const want = pageHash(payload);
const b64 = base64(payload);

send({ id: 1, action: 'DECODE_FROM_B64', payload: b64, mimeType: 'image/png' });
check('a single-message decode digests what the page would',
      reply(1) && reply(1).hash === want, `${reply(1) && reply(1).hash} vs ${want}`);

// Chunk boundaries fall wherever the sender put them, including inside a
// base64 quartet, so the digest cannot depend on where the payload was cut.
for (const size of [4, 999, 65536]) {
    const id = 100 + size;
    send({ id, action: 'DECODE_BEGIN', mimeType: 'image/png' });
    for (let at = 0; at < b64.length; at += size) {
        send({ id, action: 'DECODE_CHUNK', payload: b64.slice(at, at + size) });
    }
    send({ id, action: 'DECODE_END' });
    check(`a multipart decode split at ${size} digests the same`,
          reply(id) && reply(id).hash === want, `${reply(id) && reply(id).hash}`);
}

// The send path encodes a payload one chunk at a time and threads the digest
// through them, so it has to end where a single pass over the whole would.
let h;
for (let at = 0; at < payload.length; at += 16383) {
    const id = 900 + at;
    const chunk = payload.slice(at, at + 16383);
    send({ id, action: 'ENCODE_BINARY_TO_B64', payload: chunk.buffer, hash: at === 0 ? 5381 : h });
    h = reply(id).hash;
}
check('a chunked encode digests the whole payload', h === want, `${h} vs ${want}`);

send({ id: 2, action: 'HASH_BYTES', payload: payload.slice().buffer });
check('a standalone digest matches the codec ones',
      reply(2) && reply(2).hash === want && reply(2).byteLength === payload.length,
      `${reply(2) && reply(2).hash}`);

// A signature built from the digest has to be the string the byte path builds,
// or the two sides of the change-only gate never agree.
const viaBytes = `b:image/png:${payload.length}:${want}`;
const d = digestedPayload(payload.length, want);
check('the digest carries the fields a signature is built from',
      d.byteLength === payload.length && d.hash === want && d.__clipDigest === true,
      viaBytes);

const other = new Uint8Array(payload);
other[other.length - 1] ^= 0xff;
send({ id: 3, action: 'HASH_BYTES', payload: other.buffer });
check('a payload differing in one byte digests differently',
      reply(3) && reply(3).hash !== want, `${reply(3) && reply(3).hash}`);

console.log(`[clip-digest] ${failed ? `${failed} failed` : 'all passed'}`);
process.exit(failed ? 1 : 0);
