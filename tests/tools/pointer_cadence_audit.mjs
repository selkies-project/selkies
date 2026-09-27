/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// When the client puts pointer motion on the wire, on a virtual clock. A sample
// has to leave as it arrives -- from pointerrawupdate where the engine fires
// it, since mousemove waits for the next frame -- and never be held longer
// than the send interval, while a 1000 Hz mouse is still thinned to one
// message per interval and a transport that is not draining gets one per
// frame. The frame-aligned mousemove that follows a raw update reports the
// same motion, so it must not send it again: under pointer lock that would be
// a doubled delta.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { Input } from '../../addons/selkies-web-core/lib/input.js';

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [pointer-cadence] ${label}  ${detail}`);
}

// --- a virtual clock, with animation frames every 16 ms ---------------------
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
let frames = [];
const nextFrameAt = () => Math.ceil((now + 0.001) / 16) * 16;

/** Runs every timer and animation frame due within `ms`, in order. */
function advance(ms) {
    const end = now + ms;
    for (;;) {
        timers.sort((a, b) => a.at - b.at || a.id - b.id);
        const frameAt = frames.length ? nextFrameAt() : Infinity;
        const next = timers[0];
        const at = Math.min(next ? next.at : Infinity, frameAt);
        if (at > end) break;
        now = at;
        if (next && next.at === at) {
            timers.shift();
            next.fn();
        } else {
            const due = frames;
            frames = [];
            due.forEach((cb) => cb(now));
        }
    }
    now = end;
}

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
    requestAnimationFrame: (cb) => { frames.push(cb); return frames.length; },
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

function makeInput() {
    const sent = [];
    const input = new Input(element, (msg) => sent.push([now, msg]));
    input.sendMotion = (msg) => sent.push([now, msg]);
    input.inputAttached = false;
    return { input, sent };
}

const motion = (sent) => sent.filter(([, m]) => m.startsWith('m,') || m.startsWith('m2,'));
const move = (type, x, extra = {}) => ({
    type, target: element, clientX: x, clientY: 300, screenX: x, screenY: 300,
    buttons: 0, pointerType: 'mouse', movementX: 0, movementY: 0, timeStamp: now, ...extra,
});

// --- a sample after a pause goes out as it arrives --------------------------
{
    const { input, sent } = makeInput();
    advance(100);
    input._handleRawPointerUpdate(move('pointerrawupdate', 400));
    const m = motion(sent);
    check('a raw pointer update after a pause is sent at once',
          m.length === 1 && m[0][0] === now && m[0][1].startsWith('m,400,300,'), JSON.stringify(m));
}

// --- a 1000 Hz mouse is thinned, never held past the interval ----------------
{
    const { input, sent } = makeInput();
    advance(100);
    const arrivals = [];
    for (let i = 0; i < 200; i++) {
        input._handleRawPointerUpdate(move('pointerrawupdate', 100 + i));
        arrivals.push([now, 100 + i]);
        advance(1);
    }
    advance(50);
    const m = motion(sent);
    const holds = arrivals.map(([t, x]) => {
        const out = m.find(([ts, msg]) => ts >= t && Number(msg.split(',')[1]) >= x);
        return out ? out[0] - t : Infinity;
    });
    const worst = Math.max(...holds);
    check('1000 Hz motion goes out as one message per 2 ms', m.length >= 95 && m.length <= 105,
          `${m.length} messages for 200 samples`);
    check('and no sample is held longer than 2 ms', worst <= 2, `worst ${worst} ms`);
    check('and the last position is the last sent', m[m.length - 1][1].startsWith('m,299,'),
          m[m.length - 1][1]);
}

// --- a 125 Hz device's every sample goes out at once ------------------------
{
    const { input, sent } = makeInput();
    advance(100);
    let immediate = 0;
    for (let i = 0; i < 50; i++) {
        const before = motion(sent).length;
        input._handleRawPointerUpdate(move('pointerrawupdate', 100 + 8 * i));
        if (motion(sent).length === before + 1) immediate++;
        advance(8);
    }
    check('a 125 Hz device sends every sample as it arrives', immediate === 50, `${immediate}/50`);
}

// --- the frame-aligned mousemove after a raw update sends nothing more ------
{
    const { input, sent } = makeInput();
    globalThis.document.pointerLockElement = element;
    advance(100);
    let total = 0;
    for (let frame = 0; frame < 30; frame++) {
        let frameSum = 0;
        for (let i = 0; i < 8; i++) {
            input._handleRawPointerUpdate(move('pointerrawupdate', 0, { movementX: 3 }));
            frameSum += 3;
            advance(2);
        }
        total += frameSum;
        input._mouseButtonMovement(move('mousemove', 0, { movementX: frameSum }));
    }
    advance(50);
    globalThis.document.pointerLockElement = null;
    const travel = motion(sent).reduce((sum, [, m]) => sum + Number(m.split(',')[1]), 0);
    check('under pointer lock a delta counts once, not again on its mousemove',
          travel === total, `${travel} of ${total}`);
}

// --- a page whose engine fires no raw updates sends from mousemove ----------
{
    const { input, sent } = makeInput();
    advance(100);
    input._mouseButtonMovement(move('mousemove', 321));
    const m = motion(sent);
    check('without raw updates a mousemove is sent at once', m.length === 1 && m[0][1].startsWith('m,321,'),
          JSON.stringify(m));
}

// --- a transport that is not draining gets one message per frame -----------
{
    const { input, sent } = makeInput();
    input.motionBacklog = () => 64 * 1024;
    advance(100);
    for (let i = 0; i < 64; i++) {
        input._handleRawPointerUpdate(move('pointerrawupdate', 100 + i));
        advance(1);
    }
    advance(50);
    const m = motion(sent);
    check('a backed-up transport gets one motion message per frame', m.length >= 4 && m.length <= 6,
          `${m.length} messages in 64 ms`);
}

// --- a stroke keeps the samples the engine coalesced -------------------------
{
    const { input, sent } = makeInput();
    input.buttonMask = 1;
    advance(100);
    const t0 = now;
    const samples = [];
    for (let i = 0; i < 20; i++) {
        samples.push(move('pointerrawupdate', 100 + 5 * i, { buttons: 1, timeStamp: t0 + i }));
    }
    const last = samples[samples.length - 1];
    last.getCoalescedEvents = () => samples;
    advance(20);
    input._handleRawPointerUpdate(last);
    advance(50);
    const xs = motion(sent).map(([, m]) => Number(m.split(',')[1]));
    check('a drag sends what one event coalesced, one sample per 2 ms of it',
          xs.length >= 10 && xs.length <= 11 && xs[xs.length - 1] === 195, xs.join(','));
    check('in the order they were drawn', xs.every((x, i) => i === 0 || x > xs[i - 1]), xs.join(','));
}

process.exit(failed === 0 ? 0 : 1);
