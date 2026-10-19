/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What a Chromium Ctrl+V sends ahead of its V. The focus read covers a copy
// made in another window, but the clipboard also changes while the window
// keeps focus (a copy in another tab, a clipboard tool, a desktop shortcut
// that does not move focus), and the chord would then paste the value the
// last focus read sent. So the chord's keydown starts a read of its own and
// its V waits for it.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

const listeners = [];
const replayed = [];
globalThis.window = {
    addEventListener: (type, fn) => listeners.push({ type, fn }),
    removeEventListener: (type, fn) => {
        const i = listeners.findIndex((l) => l.type === type && l.fn === fn);
        if (i >= 0) listeners.splice(i, 1);
    },
    dispatchEvent: (ev) => { replayed.push(ev); },
};
globalThis.document = { activeElement: null };
globalThis.KeyboardEvent = class {
    constructor(type, init) { Object.assign(this, init); this.type = type; }
};

const { createClipboardGestures } = await import(
    '../../addons/selkies-web-core/lib/clipboard-sync.js');

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [clip-paste-chord] ${label}  ${detail}`);
}

function gate() {
    let release;
    const promise = new Promise((r) => { release = r; });
    return { promise, release };
}

function keyEvent(type, code, init = {}) {
    return {
        type, code, key: code === 'KeyV' ? 'v' : '', ctrlKey: true, metaKey: false, altKey: false,
        repeat: false, isComposing: false, prevented: false, stopped: false,
        preventDefault() { this.prevented = true; },
        stopImmediatePropagation() { this.stopped = true; },
        ...init,
    };
}

function press(ev) {
    for (const l of listeners.filter((x) => x.type === ev.type)) l.fn(ev);
}

/** Gestures wired on a fresh window, with a read that waits on `hold`. */
function wire({ isChromium = true, hold = null } = {}) {
    listeners.length = 0;
    replayed.length = 0;
    const reads = [];
    const g = createClipboardGestures({
        isChromium,
        clipboardSync: {},
        sendClipboardData: async () => {},
        canSync: () => true,
        canRead: () => true,
        canWrite: () => true,
        binaryEnabled: () => false,
        getSendInFlight: () => null,
        readLocalClipboard: () => {
            reads.push(Date.now());
            return hold ? hold.promise : Promise.resolve();
        },
    });
    g.wire();
    return { reads, unwire: () => g.unwire() };
}

const settle = () => new Promise((r) => setTimeout(r, 20));

// The keydown reads and holds its V until the read settles, then replays it.
{
    const hold = gate();
    const { reads, unwire } = wire({ hold });
    const down = keyEvent('keydown', 'KeyV');
    press(down);
    check('a Chromium Ctrl+V keydown reads the local clipboard', reads.length === 1, `reads=${reads.length}`);
    check('its V is held while that read is in flight', down.prevented && down.stopped,
          `prevented=${down.prevented} stopped=${down.stopped}`);
    await settle();
    check('nothing is replayed before the read settles', replayed.length === 0, `replayed=${replayed.length}`);
    hold.release();
    await settle();
    check('the V is replayed once the read settles',
          replayed.length === 1 && replayed[0].code === 'KeyV' && replayed[0].__selkiesClipReplay === true,
          `replayed=${replayed.length}`);
    unwire();
}

// The replayed V and autorepeat do not start another read.
{
    const { reads, unwire } = wire();
    press(keyEvent('keydown', 'KeyV'));
    await settle();
    for (const ev of replayed.splice(0)) press(ev);
    press(keyEvent('keydown', 'KeyV', { repeat: true }));
    await settle();
    check('a replayed or autorepeated V reads nothing more', reads.length === 1, `reads=${reads.length}`);
    unwire();
}

// A paste into the page's own form fields stays the page's.
{
    const { reads, unwire } = wire();
    document.activeElement = { id: 'settingsField', tagName: 'INPUT' };
    const down = keyEvent('keydown', 'KeyV');
    press(down);
    document.activeElement = null;
    check('a Ctrl+V in a page form field reads nothing and is not held',
          reads.length === 0 && !down.stopped, `reads=${reads.length} stopped=${down.stopped}`);
    unwire();
}

// Outside Chromium the paste event carries the clipboard; the hook stays unused.
{
    const { reads, unwire } = wire({ isChromium: false });
    press(keyEvent('keydown', 'KeyV'));
    await settle();
    check('outside Chromium the chord does not call the read', reads.length === 0, `reads=${reads.length}`);
    unwire();
}

process.exit(failed ? 1 : 0);
