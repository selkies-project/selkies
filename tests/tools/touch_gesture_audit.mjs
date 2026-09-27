/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the client puts on the wire for touch gestures, on a virtual clock.
// Trackpad mode turns a touchscreen into a laptop touchpad: a tap has to click
// as the finger lifts, since holding the press back until the double-tap window
// closes is latency every click pays, and only the release may wait on what the
// next touch turns out to be -- a drag, or the second click of a double click.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { Input } from '../../addons/selkies-web-core/lib/input.js';

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [touch-gestures] ${label}  ${detail}`);
}

// --- a virtual clock --------------------------------------------------------
let now = 1000;
let timers = [];
let timerSeq = 0;
Object.defineProperty(globalThis, 'performance', { value: { now: () => now }, configurable: true });
globalThis.setTimeout = (fn, ms = 0) => {
    const id = ++timerSeq;
    timers.push({ id, at: now + Math.max(0, ms), fn });
    return id;
};
globalThis.clearTimeout = (id) => { timers = timers.filter((t) => t.id !== id); };
globalThis.setInterval = () => 0;
globalThis.clearInterval = () => {};
Date.now = () => now;

/** Runs every timer due within `ms`, in order, then leaves the clock there. */
function advance(ms) {
    const end = now + ms;
    for (;;) {
        timers.sort((a, b) => a.at - b.at || a.id - b.id);
        const next = timers[0];
        if (!next || next.at > end) break;
        timers.shift();
        now = next.at;
        next.fn();
    }
    now = end;
}

// --- a page with nothing but an overlay element -----------------------------
const element = {
    style: { setProperty() {} },
    classList: { contains: () => false },
    contains: (node) => node === element,
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 1280, height: 720 }),
    offsetWidth: 1280,
    offsetHeight: 720,
    focus() {},
};
globalThis.window = {
    devicePixelRatio: 1,
    requestAnimationFrame: () => 0,
    addEventListener() {},
    removeEventListener() {},
    location: { origin: 'http://localhost' },
};
globalThis.document = {
    createElement: () => ({ style: {}, getContext: () => ({}) }),
    body: { appendChild() {} },
    getElementById: () => null,
    pointerLockElement: null,
    activeElement: null,
};

/** A fresh Input in trackpad mode (or direct touch), recording what it sends. */
function makeInput(trackpad = true) {
    const sent = [];
    const input = new Input(element, (msg) => sent.push([now, msg]));
    input.sendMotion = (msg) => sent.push([now, msg]);
    input.inputAttached = true;
    input.setTrackpadMode(trackpad);
    sent.length = 0;
    return { input, sent };
}

const touch = (id, x, y) => ({ identifier: id, clientX: x, clientY: y, screenX: x, screenY: y });

/** Dispatches one touch event of `type` whose changed touches are `changed` and whose live touches are `live`. */
function fire(input, type, changed, live) {
    const event = {
        type, changedTouches: changed, touches: live, target: element,
        preventDefault() {}, stopPropagation() {},
    };
    input._handleTouchEvent(event);
}

/** The left-button transitions on the wire, as [time, 'down'|'up'], from the mask each message carries. */
function leftTransitions(sent, bit = 1) {
    const out = [];
    let held = false;
    for (const [t, msg] of sent) {
        const p = msg.split(',');
        if (p[0] !== 'm' && p[0] !== 'm2') continue;
        const down = (Number(p[3]) & bit) !== 0;
        if (down !== held) {
            out.push([t, down ? 'down' : 'up']);
            held = down;
        }
    }
    return out;
}

// --- a tap clicks as the finger lifts -------------------------------------
{
    const { input, sent } = makeInput();
    const t = touch(1, 400, 300);
    fire(input, 'touchstart', [t], [t]);
    advance(80);
    const lift = now;
    fire(input, 'touchend', [t], []);
    const atLift = leftTransitions(sent);
    check('a trackpad tap presses the button as the finger lifts',
          atLift.length === 1 && atLift[0][1] === 'down' && atLift[0][0] === lift,
          JSON.stringify(atLift));
    advance(1000);
    const all = leftTransitions(sent);
    check('and releases it within the tap window',
          all.length === 2 && all[1][1] === 'up' && all[1][0] - lift <= 250,
          JSON.stringify(all.map(([ts, s]) => [ts - lift, s])));
}

// --- a quick double tap is a double click ---------------------------------
{
    const { input, sent } = makeInput();
    const a = touch(1, 400, 300);
    fire(input, 'touchstart', [a], [a]);
    advance(70);
    fire(input, 'touchend', [a], []);
    advance(90);
    const b = touch(2, 402, 301);
    fire(input, 'touchstart', [b], [b]);
    advance(70);
    fire(input, 'touchend', [b], []);
    advance(1000);
    const all = leftTransitions(sent).map(([, s]) => s);
    check('a double tap sends two clicks', all.join(',') === 'down,up,down,up', all.join(','));
}

// --- a tap then a touch that moves drags ----------------------------------
{
    const { input, sent } = makeInput();
    const a = touch(1, 400, 300);
    fire(input, 'touchstart', [a], [a]);
    advance(70);
    fire(input, 'touchend', [a], []);
    advance(100);
    let b = touch(2, 400, 300);
    fire(input, 'touchstart', [b], [b]);
    for (let i = 1; i <= 10; i++) {
        advance(16);
        b = touch(2, 400 + 6 * i, 300);
        fire(input, 'touchmove', [b], [b]);
    }
    fire(input, 'touchend', [b], []);
    advance(1000);
    const all = leftTransitions(sent).map(([, s]) => s);
    const draggedMoves = sent.filter(([, m]) => m.startsWith('m2,') && Number(m.split(',')[1]) > 0 &&
                                               (Number(m.split(',')[3]) & 1)).length;
    check('a tap then a touch that moves is one press held through the drag',
          all.join(',') === 'down,up' && draggedMoves >= 5, `${all.join(',')} moves=${draggedMoves}`);
}

// --- a second finger never scrolls with the button held -------------------
{
    const { input, sent } = makeInput();
    const a = touch(1, 400, 300);
    fire(input, 'touchstart', [a], [a]);
    advance(70);
    fire(input, 'touchend', [a], []);
    advance(60);
    const b = touch(2, 400, 300);
    fire(input, 'touchstart', [b], [b]);
    advance(30);
    const c = touch(3, 500, 300);
    fire(input, 'touchstart', [c], [b, c]);
    for (let i = 1; i <= 10; i++) {
        advance(16);
        fire(input, 'touchmove', [touch(2, 400, 300 - 10 * i), touch(3, 500, 300 - 10 * i)],
             [touch(2, 400, 300 - 10 * i), touch(3, 500, 300 - 10 * i)]);
    }
    fire(input, 'touchend', [touch(2, 400, 200), touch(3, 500, 200)], []);
    advance(1000);
    const held = sent.filter(([, m]) => m.startsWith('m2,') && Number(m.split(',')[4]) > 0 &&
                                        (Number(m.split(',')[3]) & 1));
    const all = leftTransitions(sent).map(([, s]) => s);
    check('a two-finger scroll after a tap scrolls with the button up, and leaves it up',
          held.length === 0 && all[all.length - 1] === 'up', `${all.join(',')} held-scrolls=${held.length}`);
}

process.exit(failed === 0 ? 0 : 1);
