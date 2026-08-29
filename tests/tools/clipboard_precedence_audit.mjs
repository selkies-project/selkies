/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Which local-to-server clipboard send wins. Choosing a file for the image
// upload blurs the page and refocuses it, and the focus read that refocus
// fires reads the clipboard the user had before, so without precedence the
// upload lands on the session clipboard and the old value lands on top of it.
// A push the user asked for therefore outranks a read while it runs and
// briefly after; a copy made later still reaches the session.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

// Set before the module loads: it reads neither at import, but the sender
// closes over them per call and a missing global would read as "no clipboard".
globalThis.window = { isSecureContext: true };
const clipboard = { text: '', read: null };
Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    value: { clipboard: { readText: async () => clipboard.text } },
});

const { createLocalClipboardSender } = await import(
    '../../addons/selkies-web-core/lib/clipboard-sync.js');

// The precedence window is a local in the module; pinned here so the audit
// fails when it drifts, and bound to the behaviour by the checks below.
const EXPLICIT_PRECEDENCE_MS = 1000;

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [clip-precedence] ${label}  ${detail}`);
}

const realNow = Date.now;
let clock = 1_000_000;
Date.now = () => clock;

/** A sender whose transport records what it was asked to send. */
function sender({ hold = null } = {}) {
    const sent = [];
    const s = createLocalClipboardSender({
        isChromium: true,
        isSharedMode: () => false,
        canSync: () => true,
        canRead: () => true,
        binaryEnabled: () => false,
        sendClipboardData: async (data, mime) => {
            sent.push({ data, mime });
            if (hold) await hold.promise;
        },
    });
    return { sender: s, sent };
}

function gate() {
    let release;
    const promise = new Promise((r) => { release = r; });
    return { promise, release };
}

clipboard.text = 'what the user copied before';

// Control: nothing explicit in flight, so the focus read is the sync it exists to be.
{
    const { sender: s, sent } = sender();
    await s.readAndSend();
    check('a focus read sends the local clipboard when nothing else has',
          sent.length === 1 && sent[0].data === clipboard.text, JSON.stringify(sent));
}

// The upload is still on the wire when the refocus fires.
{
    const held = gate();
    const { sender: s, sent } = sender({ hold: held });
    const push = s.sendExplicit('the uploaded image', 'image/png');
    const read = s.readAndSend();
    held.release();
    await Promise.all([push, read]);
    check('a read racing a push in flight sends nothing',
          sent.length === 1 && sent[0].mime === 'image/png', JSON.stringify(sent));
}

// The upload has landed; the refocus arrives a moment later.
{
    const { sender: s, sent } = sender();
    await s.sendExplicit('the uploaded image', 'image/png');
    clock += EXPLICIT_PRECEDENCE_MS - 1;
    await s.readAndSend();
    check('a read just after a push sends nothing',
          sent.length === 1 && sent[0].mime === 'image/png', JSON.stringify(sent));
}

// Long enough after, a local copy is a local copy again.
{
    const { sender: s, sent } = sender();
    await s.sendExplicit('the uploaded image', 'image/png');
    clock += EXPLICIT_PRECEDENCE_MS + 1;
    await s.readAndSend();
    check('a read past the window syncs the local clipboard again',
          sent.length === 2 && sent[1].data === clipboard.text, JSON.stringify(sent));
}

// The picker hands over a File, and the bytes arrive a task later; the push
// has to be on record from the call or the refocus overtakes it.
{
    const held = gate();
    const { sender: s, sent } = sender();
    const blob = { arrayBuffer: async () => { await held.promise; return 'image bytes'; } };
    const push = s.sendExplicit(blob, 'image/png');
    const read = s.readAndSend();
    held.release();
    await Promise.all([push, read]);
    check('a push whose bytes are still being read outranks a read',
          sent.length === 1 && sent[0].data === 'image bytes', JSON.stringify(sent));
}

Date.now = realNow;
console.log(`[clip-precedence] ${failed === 0 ? 'all checks passed' : failed + ' failed'}`);
process.exit(failed === 0 ? 0 : 1);
