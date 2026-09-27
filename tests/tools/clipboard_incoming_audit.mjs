/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// When a session copy is written to the local clipboard. Engines decide
// whether a clipboard write may happen at the moment it is asked for --
// Chromium wants the page focused, Firefox and WebKit a recent user
// activation -- and a multi-megabyte image is still crossing the link by the
// time the user has gone to paste it, so the write is asked for at the
// payload's first frame with its data still to come. A payload that repeats
// what the client holds, or that a newer one supersedes, withdraws its write
// and leaves the local clipboard as it was, and a write the engine refused
// waits for the next gesture only while nothing newer was asked for since.
//
// The engine is modeled by its rule alone: a write asked for while the page is
// not allowed to is refused; one asked for while it is lands whenever its data
// settle, allowed or not by then. Decoding and digests run in the real worker.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

const pending = new Map();
let nextId = 0;
globalThis.self = {
    postMessage: (msg) => {
        const waiter = pending.get(msg.id);
        if (!waiter) return;
        pending.delete(msg.id);
        if (msg.success) waiter.resolve(msg);
        else waiter.reject(new Error(msg.error));
    },
};
await import('../../addons/selkies-web-core/clipboard-worker.js');
const workerScope = globalThis.self;

/** The worker bridge's surface, answered in-process by the real worker. */
function ask(action, payload, extra = {}) {
    const id = ++nextId;
    return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        workerScope.onmessage({ data: { id, action, payload, ...extra } });
    });
}
const worker = {
    decode: (b64, mimeType) => ask('DECODE_FROM_B64', b64, { mimeType }),
    hashBytes: (buf) => ask('HASH_BYTES', buf),
    decodeStream(mimeType) {
        const id = ++nextId;
        workerScope.onmessage({ data: { id, action: 'DECODE_BEGIN', mimeType } });
        return {
            push: (b64) => workerScope.onmessage({ data: { id, action: 'DECODE_CHUNK', payload: b64 } }),
            finish: () => new Promise((resolve, reject) => {
                pending.set(id, { resolve, reject });
                workerScope.onmessage({ data: { id, action: 'DECODE_END' } });
            }),
            abort: () => workerScope.onmessage({ data: { id, action: 'DECODE_ABORT' } }),
        };
    },
};

const listeners = {};
globalThis.window = {
    isSecureContext: true,
    addEventListener: (type, fn) => { (listeners[type] ||= []).push(fn); },
};
globalThis.document = { hidden: false, addEventListener: () => {} };
function gesture(type = 'keydown') {
    for (const fn of listeners[type] || []) fn({ type });
}

const engine = { allowed: true };
const local = { value: 'what the user had' };
const writes = [];
globalThis.ClipboardItem = class {
    constructor(items) { this.items = items; this.types = Object.keys(items); }
};
Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    value: {
        clipboard: {
            async write(items) {
                const entry = { outcome: 'pending' };
                writes.push(entry);
                if (!engine.allowed) {
                    entry.outcome = 'refused';
                    throw new DOMException('Document is not focused.', 'NotAllowedError');
                }
                const item = items[0];
                const values = {};
                try {
                    for (const type of item.types) values[type] = await item.items[type];
                } catch (_) {
                    entry.outcome = 'rejected';
                    // Chromium reports data that never came as an activation error.
                    throw new DOMException('Promises were rejected.', 'NotAllowedError');
                }
                entry.outcome = 'landed';
                local.value = values;
            },
        },
    },
});

const {
    createClipboardSync, createDeferredClipboardWriter, createIncomingClipboard, digestedPayload,
} = await import('../../addons/selkies-web-core/lib/clipboard-sync.js');

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [clip-incoming] ${label}  ${detail}`);
}

const tick = () => new Promise((resolve) => setTimeout(resolve, 20));

function base64(bytes) {
    let s = '';
    for (const b of bytes) s += String.fromCharCode(b);
    return btoa(s);
}

function bytesOf(seed, length) {
    const out = new Uint8Array(length);
    for (let i = 0; i < length; i++) out[i] = (seed * 31 + i * 7) & 0xff;
    return out;
}

async function localBytes() {
    const blob = local.value && local.value['image/png'];
    return blob ? new Uint8Array(await blob.arrayBuffer()) : null;
}

function same(a, b) {
    return !!a && !!b && a.length === b.length && a.every((v, i) => v === b[i]);
}

/** A client: the shared clipboard state, the writer, and the receive path. */
function client({ toPng = async (blob) => blob } = {}) {
    const clipboardSync = createClipboardSync({
        sendRequest: () => {},
        digestBytes: async (buf) => {
            const { byteLength, hash } = await worker.hashBytes(buf);
            return digestedPayload(byteLength, hash);
        },
    });
    const failures = [];
    const incoming = createIncomingClipboard({
        worker,
        clipboardSync,
        writer: createDeferredClipboardWriter(),
        toPng,
        canWriteLocal: () => true,
        binaryEnabled: () => true,
        onPreview: () => {},
        onImageWritten: () => {},
        onImageWriteFailed: (err) => failures.push(err),
    });
    return { incoming, clipboardSync, failures };
}

/**
 * One payload over the multipart frames, `between` running before its end.
 * Frames are separate messages, so what runs in the first frame's microtasks
 * has run before anything else can happen.
 */
async function multipart(incoming, mime, bytes, { cacheOnly = false, between = null } = {}) {
    incoming.begin(mime, bytes.length, cacheOnly);
    await new Promise((resolve) => setTimeout(resolve, 0));
    const size = 3 * 1024;
    for (let at = 0; at < bytes.length; at += size) incoming.push(base64(bytes.subarray(at, at + size)));
    if (between) between();
    incoming.finish();
}

// The write is asked for at the first frame, while the page may still write.
{
    const { incoming } = client();
    writes.length = 0;
    const image = bytesOf(1, 20000);
    incoming.begin('image/png', image.length, false);
    await Promise.resolve();
    await Promise.resolve();
    check('a payload\'s local write is asked for at its first frame', writes.length === 1,
          `${writes.length} writes before any data`);
    incoming.reset();
    await tick();
}

// The user leaves before the bytes are in: the write asked for earlier lands.
{
    const { incoming } = client();
    writes.length = 0;
    engine.allowed = true;
    const image = bytesOf(2, 20000);
    await multipart(incoming, 'image/png', image, { between: () => { engine.allowed = false; } });
    await tick();
    check('an image lands although the page lost focus while it was arriving',
          same(await localBytes(), image) && writes.length === 1, JSON.stringify(writes));
    engine.allowed = true;

    // The same content again: the client holds it, and nothing is written.
    const before = local.value;
    writes.length = 0;
    await multipart(incoming, 'image/png', image);
    await tick();
    check('a payload the client already holds withdraws its write and leaves the local clipboard as it was',
          local.value === before && writes.length === 1 && writes[0].outcome === 'rejected',
          JSON.stringify(writes));
}

// A newer copy made while an older one is still arriving is the one that lands.
{
    const { incoming, failures } = client();
    writes.length = 0;
    const older = bytesOf(3, 30000);
    const newer = bytesOf(4, 9000);
    incoming.begin('image/png', older.length, false);
    incoming.push(base64(older.subarray(0, 3072)));
    await multipart(incoming, 'image/png', newer);
    await tick();
    check('a newer payload withdraws an older one still arriving, and lands',
          same(await localBytes(), newer) && writes.length === 2
          && writes[0].outcome === 'rejected' && writes[1].outcome === 'landed' && failures.length === 0,
          JSON.stringify(writes));
}

// Refused outright (the page had lost focus already): kept for the next gesture.
{
    const { incoming } = client();
    writes.length = 0;
    engine.allowed = false;
    const image = bytesOf(5, 12000);
    await multipart(incoming, 'image/png', image);
    await tick();
    const refused = writes.length === 1 && writes[0].outcome === 'refused';
    engine.allowed = true;
    gesture('keydown');
    await tick();
    check('a write refused at the time is written on the next gesture',
          refused && same(await localBytes(), image), JSON.stringify(writes));
}

// A refusal that arrives after a newer write was asked for is not kept: a
// retry would land the older value over the newer one.
{
    const writer = createDeferredClipboardWriter();
    let refuse = null;
    // Refused the first time, slowly; written if it is ever tried again.
    const older = writer.write(() => (refuse === null
        ? new Promise((_, reject) => { refuse = reject; })
        : Promise.resolve().then(() => { local.value = 'older'; })));
    await writer.write(async () => { local.value = 'newer'; });
    refuse(new DOMException('Document is not focused.', 'NotAllowedError'));
    await older;
    gesture('keydown');
    await tick();
    check('a write refused after a newer one was asked for is dropped, not retried over it',
          local.value === 'newer', JSON.stringify(local.value));
}

// The connect-time reply only fills the cache.
{
    const { incoming, clipboardSync } = client();
    writes.length = 0;
    const image = bytesOf(6, 7000);
    await multipart(incoming, 'image/png', image, { cacheOnly: true });
    await tick();
    check('the connect-time reply is cached and never written locally',
          writes.length === 0 && clipboardSync.lastBlob !== null, `${writes.length} writes`);
}

// Text is written as it came.
{
    const { incoming } = client();
    writes.length = 0;
    incoming.single('text/plain', base64(new TextEncoder().encode('from the session')), false);
    await tick();
    const blob = local.value && local.value['text/plain'];
    const text = blob ? await blob.text() : null;
    check('session text lands', text === 'from the session', text);
}

// An image the browser cannot convert is reported, never retried as refused.
{
    const { incoming, failures } = client({ toPng: async () => { throw new Error('undecodable'); } });
    writes.length = 0;
    await multipart(incoming, 'image/bmp', bytesOf(7, 6000));
    await tick();
    gesture('keydown');
    await tick();
    check('an image that cannot be converted is reported once and not retried on a gesture',
          failures.length === 1 && writes.length === 1, `${failures.length} reports, ${writes.length} writes`);
}

console.log(`[clip-incoming] ${failed === 0 ? 'all checks passed' : failed + ' failed'}`);
process.exit(failed === 0 ? 0 : 1);
