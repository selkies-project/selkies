/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What a touchpad's scroll puts on the wire, on a virtual clock. Where the
// session takes a finger's scroll (Wayland), a wheel classified as a touchpad
// sends its travel in stream pixels and an end once it pauses; a wheel's
// notches, line deltas, and a session that does not take one keep the notch
// pulses. A macOS wheel is told from a touchpad there by what each engine was
// measured to report for one, since the system's acceleration leaves its
// deltas no notch size.
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
/** Stands the page on a platform and engine; the wheel's reading depends on both. */
function setPlatform(platform, userAgent) {
    Object.defineProperty(globalThis, 'navigator', {
        value: { platform, userAgent, maxTouchPoints: 0 }, configurable: true, writable: true });
}
const LINUX = ['Linux x86_64', 'Mozilla/5.0 (X11; Linux x86_64) Chrome/154'];
const MAC_BLINK = ['MacIntel', 'Mozilla/5.0 (Macintosh) Chrome/154 Safari/537.36'];
const MAC_WEBKIT = ['MacIntel', 'Mozilla/5.0 (Macintosh) Version/26.6 Safari/605.1.15'];
const MAC_GECKO = ['MacIntel', 'Mozilla/5.0 (Macintosh; rv:157.0) Gecko/20100101 Firefox/157.0'];
setPlatform(...LINUX);

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
function wheel(input, deltaY, { deltaX = 0, deltaMode = 0, ms = 16, shiftKey = false } = {}) {
    advance(ms);
    input._mouseWheelWrapper({ type: 'wheel', deltaY, deltaX, deltaMode, ctrlKey: false, shiftKey,
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

// macOS, as measured from a wheel's HID reports through the system's own
// acceleration: a detent after a pause is 0.1 of a line, later ones whatever
// the spin's speed made them.
const MAC_SLOW = [4.000244140625, 4.000244140625, 4.000244140625];
const MAC_SPIN = [4.000244140625, 21.70654296875, 67.843017578125, 132.9052734375, 211.6064453125, 361.2994384765625];
/** Notches sent on the buttons of `mask`: each pulse raises its bit once, carrying its count. */
const notches = (sent, mask) => sent.map((m) => m.split(','))
    .filter((f) => f[0] === 'm2' && (Number(f[3]) & mask))
    .reduce((sum, f) => sum + Number(f[4]), 0);
const downs = (sent) => notches(sent, 8);
const sideways = (sent) => notches(sent, 192);

for (const [engine, platform] of [['Blink', MAC_BLINK], ['WebKit', MAC_WEBKIT]]) {
    for (const finger of [true, false]) {
        const session = finger ? 'a session taking finger scroll' : 'a session taking notches';
        setPlatform(...platform);
        {
            const { input, sent } = makeInput(finger);
            for (const d of MAC_SLOW) wheel(input, d, { ms: 400 });
            advance(300);
            check(`macOS ${engine}, ${session}: a wheel's slow detents are a notch each`,
                  downs(sent) === MAC_SLOW.length && fingers(sent).length === 0, JSON.stringify(sent));
        }
        {
            const { input, sent } = makeInput(finger);
            for (const d of MAC_SPIN) wheel(input, d);
            advance(300);
            check(`macOS ${engine}, ${session}: a spin's accelerated detents are still a notch each`,
                  downs(sent) === MAC_SPIN.length && fingers(sent).length === 0, JSON.stringify(sent));
        }
        {
            const { input, sent } = makeInput(finger);
            for (let i = 0; i < 3; i++) wheel(input, 0, { deltaX: 4.000244140625, ms: 300 });
            check(`macOS ${engine}, ${session}: a sideways detent is a sideways notch`,
                  sideways(sent) === 3 && fingers(sent).length === 0, JSON.stringify(sent));
        }
    }
    {
        // Under Shift the system turns the wheel sideways, in whole lines of 40 px.
        const { input, sent } = makeInput(true);
        const turned = [-40, -40, -120, -320];
        for (const d of turned) wheel(input, 0, { deltaX: d, shiftKey: true, ms: 30 });
        check(`macOS ${engine}: a wheel turned under Shift is a sideways notch a detent`,
              sideways(sent) === turned.length && fingers(sent).length === 0, JSON.stringify(sent));
    }
    {
        const { input, sent } = makeInput(true);
        for (const d of [7, 40, 80]) wheel(input, d, { shiftKey: true });
        check(`macOS ${engine}: a touchpad's stroke under Shift stays a finger's`,
              fingers(sent).length === 3 && pulses(sent).length === 0, JSON.stringify(sent));
    }
    {
        const { input, sent } = makeInput(true);
        const stroke = [1, 3, 7, 11, 25, 13, 5];
        for (const d of stroke) wheel(input, d);
        check(`macOS ${engine}: a touchpad's whole pixels stay a finger's`,
              fingers(sent).length === stroke.length && pulses(sent).length === 0, JSON.stringify(sent));
    }
}

setPlatform(...MAC_GECKO);
{
    // Gecko reports the wheel in lines, the count being the system's acceleration.
    const { input, sent } = makeInput(true);
    const spin = [1, 1, 2, 6, 10, 10];
    for (const d of spin) wheel(input, d, { deltaMode: 1 });
    check('macOS Gecko: a wheel\'s accelerated line counts are a notch each',
          downs(sent) === spin.length && fingers(sent).length === 0, JSON.stringify(sent));
}
{
    // At a page zoom of 110% Gecko divides a touchpad's pixels by it.
    const { input, sent } = makeInput(true);
    const stroke = [0.9, 2.7, 6.3, 9.9, 22.5];
    for (const d of stroke) wheel(input, d);
    check('macOS Gecko: a zoomed page\'s fractional touchpad pixels stay a finger\'s',
          fingers(sent).length === stroke.length && pulses(sent).length === 0, JSON.stringify(sent));
}

setPlatform(...LINUX);
{
    const { input, sent } = makeInput(true);
    for (const d of MAC_SLOW) wheel(input, d, { ms: 400 });
    check('off macOS a fractional pixel delta is still a touchpad\'s',
          fingers(sent).filter((m) => m !== 'sfe').length === MAC_SLOW.length && pulses(sent).length === 0,
          JSON.stringify(sent));
}

process.exit(failed ? 1 : 0);
