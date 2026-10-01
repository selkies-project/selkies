/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What a touchpad's scroll puts on the wire, on a virtual clock. Where the
// session takes a finger's scroll (Wayland), a wheel classified as a touchpad
// sends its travel in stream pixels and an end once it pauses; a wheel's
// notches, line deltas, and a session that does not take one keep the notch
// pulses.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { Input } from '../../addons/selkies-web-core/lib/input.js';

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [finger-scroll] ${label}  ${detail}`);
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

// --- a page whose stream fills a 1280x720 element, one stream pixel per CSS pixel
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


/** A page recording what it sends, with the session taking a finger's scroll or not. */
function makeInput(finger) {
    const sent = [];
    const input = new Input(element, (msg) => sent.push(msg));
    input.sendMotion = (msg) => sent.push(msg);
    input.inputAttached = true;
    input.setFingerScroll(finger);
    return { input, sent };
}

/** One wheel event through the page's own handler, `ms` after the previous. */
function wheel(input, deltaY, { deltaX = 0, deltaMode = 0, ms = 16 } = {}) {
    advance(ms);
    input._mouseWheelWrapper({ type: 'wheel', deltaY, deltaX, deltaMode, ctrlKey: false,
                               preventDefault() {} });
}

const pulses = (sent) => sent.filter((m) => m.startsWith('m,') || m.startsWith('m2,'));
const fingers = (sent) => sent.filter((m) => m.startsWith('sf,') || m === 'sfe');

{
    const { input, sent } = makeInput(true);
    for (const d of [3.5, 6.25, 9, 4.75]) wheel(input, d);
    check('a touchpad stroke goes out as the finger travels, in stream pixels',
          JSON.stringify(fingers(sent)) === JSON.stringify(['sf,0.00,3.50', 'sf,0.00,6.25', 'sf,0.00,9.00', 'sf,0.00,4.75']),
          JSON.stringify(sent));
    check('with no notch pulses', pulses(sent).length === 0, JSON.stringify(pulses(sent)));
    advance(199);
    check('and no end while it may still be moving', !sent.includes('sfe'), JSON.stringify(sent));
    advance(2);
    check('an end once it pauses', sent.filter((m) => m === 'sfe').length === 1, JSON.stringify(sent));
    advance(500);
    check('and only one', sent.filter((m) => m === 'sfe').length === 1, JSON.stringify(sent));
}

{
    const { input, sent } = makeInput(true);
    wheel(input, 0, { deltaX: -12.5 });
    check('a sideways stroke goes out on its own axis', fingers(sent)[0] === 'sf,-12.50,0.00', JSON.stringify(sent));
}

{
    const { input, sent } = makeInput(true);
    input._streamDensity = 2;
    input._pointerScaleFrame = null;
    input._windowMath();
    wheel(input, 10);
    check('a stroke scales by the stream pixels per CSS pixel', fingers(sent)[0] === 'sf,0.00,20.00',
          JSON.stringify(sent));
}

{
    // Chrome's notch is 120 px on Linux and 100 on Windows; the detector has
    // its samples only at the fourth.
    const { input, sent } = makeInput(true);
    for (let i = 0; i < 6; i++) wheel(input, 120, { ms: 40 });
    advance(200);
    check('a wheel\'s notches stay notch pulses, its first ones too',
          pulses(sent).length >= 6 && fingers(sent).length === 0, JSON.stringify(sent));
}

{
    const { input, sent } = makeInput(true);
    for (let i = 0; i < 4; i++) wheel(input, 0, { deltaX: 120, ms: 40 });
    advance(200);
    check('and so do a tilt wheel\'s sideways notches, which the detector never samples',
          pulses(sent).length >= 4 && fingers(sent).length === 0, JSON.stringify(sent));
}

{
    const { input, sent } = makeInput(true);
    const stroke = [3, 12, 40, 95, 130, 160, 120, 85, 40, 10];
    for (const d of stroke) wheel(input, d);
    advance(300);
    check('a stroke whose momentum passes a notch\'s size stays a finger\'s',
          fingers(sent).length === stroke.length + 1 && pulses(sent).length === 0, JSON.stringify(sent));
}

{
    const { input, sent } = makeInput(true);
    wheel(input, 3, { deltaMode: 1 });
    check('a line-mode wheel keeps its notches', pulses(sent).length >= 1 && fingers(sent).length === 0,
          JSON.stringify(sent));
}

{
    const { input, sent } = makeInput(false);
    for (const d of [30, 40, 50, 60]) wheel(input, d);
    check('a session that takes no finger scroll gets the touchpad as notch pulses',
          pulses(sent).length >= 1 && fingers(sent).length === 0, JSON.stringify(sent));
}

{
    const { input, sent } = makeInput(true);
    wheel(input, 8);
    input.setFingerScroll(false);
    check('turning finger scroll off mid-stroke ends the stroke', sent[sent.length - 1] === 'sfe',
          JSON.stringify(sent));
    const second = makeInput(true);
    wheel(second.input, 8);
    second.input.detach_context();
    check('and so does detaching the page', second.sent.includes('sfe'), JSON.stringify(second.sent));
}

process.exit(failed ? 1 : 0);
