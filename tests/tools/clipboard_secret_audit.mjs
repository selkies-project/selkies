/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A secret the session copied, on the page. Its preview reaches the dashboards
// masked, never as its text; written locally, it is taken back 60 seconds
// later, or as soon as the session's clipboard moves on, but only while the
// local clipboard still holds it, and only where the engine lets a page check
// that without a gesture or a prompt (Chromium, with clipboard-read granted
// and the page focused). The engine is modeled by those rules; the long timer
// is fired by hand, everything shorter runs on the real clock, and decoding
// runs in the real worker.
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
    decodeStream() { throw new Error('no multipart here'); },
};

const realSetTimeout = globalThis.setTimeout;
const longTimers = [];
globalThis.setTimeout = (fn, ms, ...args) => {
    if (ms >= 60000) {
        const timer = { fn, ms, fired: false, cleared: false };
        longTimers.push(timer);
        return timer;
    }
    return realSetTimeout(fn, ms, ...args);
};
const realClearTimeout = globalThis.clearTimeout;
globalThis.clearTimeout = (t) => {
    if (t && typeof t === 'object' && 'cleared' in t) t.cleared = true;
    else realClearTimeout(t);
};
function fireDeadlines() {
    for (const t of longTimers.splice(0)) if (!t.cleared) { t.fired = true; t.fn(); }
}

const listeners = {};
globalThis.window = {
    isSecureContext: true,
    addEventListener: (type, fn) => { (listeners[type] ||= []).push(fn); },
};
const engine = { focused: true, permission: 'granted' };
globalThis.document = { hidden: false, addEventListener: () => {}, hasFocus: () => engine.focused };
function dispatch(type) {
    for (const fn of listeners[type] || []) fn({ type });
}

const local = { text: 'what the user had' };
const calls = [];
globalThis.ClipboardItem = class {
    constructor(items) { this.items = items; this.types = Object.keys(items); }
};
Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    value: {
        permissions: { query: async () => ({ state: engine.permission }) },
        clipboard: {
            async write(items) {
                calls.push('write');
                if (!engine.focused) throw new DOMException('Document is not focused.', 'NotAllowedError');
                const item = items[0];
                const blob = await item.items['text/plain'];
                local.text = await blob.text();
            },
            async writeText(text) {
                calls.push(`writeText:${text}`);
                if (!engine.focused) throw new DOMException('Document is not focused.', 'NotAllowedError');
                local.text = text;
            },
            async readText() {
                calls.push('readText');
                if (!engine.focused) throw new DOMException('Document is not focused.', 'NotAllowedError');
                return local.text;
            },
        },
    },
});

const {
    clipboardPreviewMessage, createClipboardSync, createDeferredClipboardWriter, createIncomingClipboard,
} = await import('../../addons/selkies-web-core/lib/clipboard-sync.js');

let failed = 0;
function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [clip-secret-page] ${label}  ${detail}`);
}
const settle = () => new Promise((resolve) => realSetTimeout(resolve, 30));
const b64 = (text) => btoa(String.fromCharCode(...new TextEncoder().encode(text)));
const SECRET = 'correct horse battery staple';

/** A page: the clipboard state, the writer, and the receive path of one core. */
function page({ isChromium = true, writeLocal = true } = {}) {
    const previews = [];
    const clipboardSync = createClipboardSync({ sendRequest: () => {} });
    const options = { writeLocal };
    const incoming = createIncomingClipboard({
        worker,
        clipboardSync,
        writer: createDeferredClipboardWriter(),
        toPng: async (blob) => blob,
        canWriteLocal: () => options.writeLocal,
        binaryEnabled: () => true,
        onPreview: (text, secret) => previews.push(clipboardPreviewMessage(text, secret)),
        onImageWritten: () => {},
        onImageWriteFailed: () => {},
        isChromium,
    });
    return { incoming, clipboardSync, previews, options };
}

async function reset() {
    calls.length = 0;
    longTimers.length = 0;
    engine.focused = true;
    engine.permission = 'granted';
    local.text = 'what the user had';
}

// The preview never carries the secret.
{
    await reset();
    const { incoming, previews } = page();
    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    const last = previews[previews.length - 1];
    check('a secret reaches the dashboards as the flag alone',
          last.secret === true && last.text === '' && last.totalLength === 0
          && !JSON.stringify(previews).includes(SECRET), JSON.stringify(last));
    check('and still lands on the local clipboard', local.text === SECRET, local.text);
}

// A transport that marks the payload in a message of its own marks only the next one.
{
    await reset();
    const { incoming, previews } = page();
    incoming.markSecret();
    incoming.single('text/plain', b64(SECRET), false);
    await settle();
    incoming.single('text/plain', b64('ordinary'), false);
    await settle();
    check('a mark ahead of a payload applies to it and to nothing after it',
          previews[0].secret === true && previews[1].secret === false && previews[1].text === 'ordinary',
          JSON.stringify(previews));
    incoming.markSecret();
    incoming.reset();
    incoming.single('text/plain', b64('after a reconnect'), false);
    await settle();
    check('a mark left by a dropped connection marks nothing', previews[2].secret === false,
          JSON.stringify(previews[2]));
}

// Taken back once its time is up, only while the local clipboard still holds it.
{
    await reset();
    const { incoming } = page();
    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    fireDeadlines();
    await settle();
    check('a secret still on the local clipboard is taken back when its time is up',
          local.text === '' && calls.includes('writeText:'), JSON.stringify(calls));

    calls.length = 0;
    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    check('the session copying it again afterwards puts it back', local.text === SECRET,
          JSON.stringify(calls));

    local.text = 'a copy the user made since';
    calls.length = 0;
    fireDeadlines();
    await settle();
    check('anything else on the local clipboard is left alone',
          local.text === 'a copy the user made since' && calls.includes('readText')
          && !calls.includes('writeText:'), JSON.stringify(calls));
}

// Taken back as soon as the session's clipboard lets it go.
{
    await reset();
    const { incoming, previews } = page();
    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    incoming.single('text/plain', '', false, true);
    await settle();
    check('an empty secret takes the secret back at once, before its time is up',
          local.text === '' && longTimers.every((t) => !t.fired), JSON.stringify(calls));
    check('and empties the preview', previews[previews.length - 1].text === ''
          && previews[previews.length - 1].secret === false, JSON.stringify(previews[previews.length - 1]));
}

// A later copy that lands replaces the secret itself; one that does not, takes it back.
{
    await reset();
    const { incoming, options } = page();
    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    calls.length = 0;
    incoming.single('text/plain', b64('the next copy'), false);
    await settle();
    fireDeadlines();
    await settle();
    check('a later copy written locally replaces the secret, with nothing emptied',
          local.text === 'the next copy' && !calls.includes('writeText:'), JSON.stringify(calls));

    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    options.writeLocal = false;
    calls.length = 0;
    incoming.single('text/plain', b64('a copy that stays in the session'), false);
    await settle();
    check('a later copy that is not written locally takes the secret back',
          local.text === '' && calls.includes('writeText:'), JSON.stringify(calls));
}

// The same secret copied again stays for a full term.
{
    await reset();
    const { incoming } = page();
    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    const first = longTimers[0];
    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    check('copying the same secret again restarts its term',
          first.cleared && longTimers.length === 2 && !longTimers[1].cleared && local.text === SECRET,
          `${longTimers.length} timers`);
}

// The engine's own rules.
{
    await reset();
    const { incoming } = page();
    engine.permission = 'prompt';
    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    calls.length = 0;
    fireDeadlines();
    await settle();
    check('without clipboard-read granted nothing is read, so no prompt is raised',
          !calls.includes('readText') && local.text === SECRET, JSON.stringify(calls));

    engine.permission = 'granted';
    incoming.single('text/plain', b64('reset'), false);
    await settle();
    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    engine.focused = false;
    calls.length = 0;
    fireDeadlines();
    await settle();
    const whileAway = local.text;
    engine.focused = true;
    dispatch('focus');
    await settle();
    check('an unfocused page takes it back when focused again',
          whileAway === SECRET && local.text === '', JSON.stringify(calls));
}

{
    await reset();
    const { incoming, previews } = page({ isChromium: false });
    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    incoming.single('text/plain', '', false, true);
    await settle();
    fireDeadlines();
    await settle();
    check('outside Chromium nothing is read or emptied, and no timer runs',
          !calls.includes('readText') && !calls.includes('writeText:') && longTimers.length === 0
          && previews[0].secret === true, JSON.stringify(calls));
}

// The dashboards' copy button.
{
    await reset();
    const { incoming, options } = page();
    options.writeLocal = false;
    incoming.single('text/plain', b64(SECRET), false, true);
    await settle();
    const before = local.text;
    const copied = await incoming.copySecret();
    check('the copy button writes the masked secret, which is then watched like any other',
          before === 'what the user had' && copied === true && local.text === SECRET
          && longTimers.length === 1, JSON.stringify(calls));
    incoming.single('text/plain', b64('ordinary'), false);
    await settle();
    check('with no secret in the session it writes nothing', await incoming.copySecret() === false);
}

console.log(`[clip-secret-page] ${failed === 0 ? 'all checks passed' : failed + ' failed'}`);
process.exit(failed === 0 ? 0 : 1);
