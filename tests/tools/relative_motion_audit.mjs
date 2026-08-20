/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the client does with relative motion, which pointer lock and the
// trackpad both produce. Engines report movementX/Y in whole CSS pixels,
// carrying their own sub-pixel remainder, so a client whose scale is not a
// whole number sees a stream of deltas that only add up over several events --
// rounding each one on its own loses or invents a fraction of a pixel every
// time. The size and the sign of that error depend on how fast the pointer is
// moving, which is indistinguishable from an acceleration curve, and motion
// that should cancel out drifts instead.
//
// `engineCss` is the measured behaviour of Chrome and Firefox: at a device
// pixel ratio of 1.25 a stream of one-device-pixel motions arrives as
// 1,1,0,1,1, and at 1.5 as 1,0,1 -- exactly a carried division by the ratio.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { Input } from '../../addons/selkies-web-core/lib/input.js';

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [relative-motion] ${label}  ${detail}`);
}

/** The CSS-pixel deltas an engine reports for a device-pixel motion. */
function engineCss(devicePx, dpr) {
    let carry = 0;
    return devicePx.map((d) => {
        carry += d / dpr;
        const out = Math.round(carry);
        carry -= out;
        return out;
    });
}

const steps = (n, delta) => new Array(n).fill(delta);

/**
 * An Input with only what the relative-motion path touches. `sink` gives the
 * stream box a manual-resolution or shared session maps through.
 */
function makeInput({ dpr = 1, sink = null } = {}) {
    globalThis.window = {
        devicePixelRatio: dpr,
        is_manual_resolution_mode: !!sink,
        isManualResolutionMode: !!sink,
        requestAnimationFrame: (cb) => frames.push(cb),
    };
    globalThis.document = {
        pointerLockElement: null,
        getElementById: (id) => (sink && id === 'videoCanvas' ? sink : null),
    };
    const input = Object.create(Input.prototype);
    input.element = {};
    input.isSharedMode = false;
    input.useCssScaling = false;
    input.buttonMask = 0;
    input.x = 0;
    input.y = 0;
    input._relCarryX = 0;
    input._relCarryY = 0;
    input._pendingMove = null;
    input._moveFlushScheduled = false;
    input.m = { mouseMultiX: 1, mouseMultiY: 1, mouseOffsetX: 0, mouseOffsetY: 0,
                elementClientX: 0, elementClientY: 0, frameW: 1920, frameH: 1080 };
    input.sent = [];
    input.send = (msg) => input.sent.push(msg);
    return input;
}

let frames = [];

/** A canvas presenting `w`x`h` stream pixels in a `boxW`-wide CSS box. */
function makeSink(w, h, boxW, boxH) {
    return {
        tagName: 'CANVAS', width: w, height: h,
        getBoundingClientRect: () => ({ left: 0, top: 0, width: boxW, height: boxH }),
    };
}

/** Push CSS deltas through the coalescer, flushing every `perFrame` events. */
function drive(input, deltas, perFrame = 4) {
    frames = [];
    deltas.forEach((d, i) => {
        input._queueCoalescedMouseMove('m2', d, 0, input.buttonMask);
        if ((i + 1) % perFrame === 0) {
            input._moveFlushScheduled = false;
            input._flushCoalescedMouseMove();
        }
    });
    input._moveFlushScheduled = false;
    input._flushCoalescedMouseMove();
}

/** Net server-pixel travel the client put on the wire. */
function travel(input) {
    return input.sent.reduce((sum, msg) => {
        const parts = msg.split(',');
        return parts[0] === 'm2' ? sum + Number(parts[1]) : sum;
    }, 0);
}

// --- what a fractional scale does to a stream of small deltas -------------
for (const dpr of [1, 1.25, 1.5, 2]) {
    for (const [name, devicePx] of [['1px steps', steps(40, 1)],
                                    ['2px steps', steps(40, 2)],
                                    ['8px flick', steps(20, 8)]]) {
        const input = makeInput({ dpr });
        const want = devicePx.reduce((a, b) => a + b, 0);
        drive(input, engineCss(devicePx, dpr));
        const got = travel(input);
        check(`dpr ${dpr}: ${name} travels ${want} server px`,
              Math.abs(got - want) <= 1, `${got}`);
    }
}

// Every delta lands on a half pixel, where rounding each event on its own
// inflates the travel by a fifth. One event per frame, so each half pixel
// reaches the quantizer on its own rather than being summed away first.
{
    const input = makeInput({ dpr: 1.25 });
    drive(input, steps(40, 2), 1);
    check('dpr 1.25: half-pixel deltas do not inflate the travel',
          Math.abs(travel(input) - 100) <= 1, `${travel(input)}`);
}

// --- motion that cancels out ----------------------------------------------
{
    const input = makeInput({ dpr: 1.5 });
    const there = engineCss(steps(60, 1), 1.5);
    drive(input, there.concat(there.map((d) => -d)));
    check('dpr 1.5: there and back leaves the pointer where it started',
          travel(input) === 0, `${travel(input)}`);
}

// --- sub-pixel motion is not silently dropped -----------------------------
{
    // A 0.4 server px per event stream: every event on its own rounds to zero.
    const input = makeInput({ dpr: 0.4 });
    drive(input, steps(50, 1), 1);
    check('a stream of sub-pixel deltas still moves the pointer',
          Math.abs(travel(input) - 20) <= 1, `${travel(input)}`);
}

// --- the scale absolute coordinates use -----------------------------------
{
    // Manual resolution: a 1920-wide stream presented in a 960 CSS px box, so a
    // CSS pixel of motion covers two stream pixels.
    const input = makeInput({ dpr: 1, sink: makeSink(1920, 1080, 960, 540) });
    drive(input, steps(30, 4));
    check('a stream box scales relative motion like it scales clicks',
          travel(input) === 240, `${travel(input)}`);
}
{
    const input = makeInput({ dpr: 1, sink: makeSink(960, 540, 1920, 1080) });
    drive(input, steps(30, 4));
    check('an upscaled stream box scales motion down the same way',
          travel(input) === 60, `${travel(input)}`);
}

// --- absolute and relative read the same box ------------------------------
{
    const sink = makeSink(1920, 1080, 960, 540);
    const input = makeInput({ dpr: 1, sink });
    input._applySinkCoordinates(100, 50, sink, null);
    check('a click maps through the stream box', input.x === 200 && input.y === 100,
          `${input.x},${input.y}`);
    const [dx, dy] = input._relativeToServer(100, 50);
    check('a delta of the same size covers the same ground',
          dx === 200 && dy === 100, `${dx},${dy}`);
    input._applySinkCoordinates(5000, -20, sink, null);
    check('a click outside the box is clamped to the stream',
          input.x === 1920 && input.y === 0, `${input.x},${input.y}`);
}

// --- the remainder does not outlive the lock ------------------------------
{
    const input = makeInput({ dpr: 1.5 });
    input.cursorDiv = { style: {} };
    input.resetKeyboard = () => {};
    drive(input, [1], 1);
    check('a partial pixel is left over to carry', input._relCarryX !== 0,
          `${input._relCarryX}`);
    input._pointerLock();
    check('the remainder is dropped when the lock changes',
          input._relCarryX === 0 && input._relCarryY === 0,
          `${input._relCarryX},${input._relCarryY}`);
}

// --- a button event carries its own motion --------------------------------
{
    const input = makeInput({ dpr: 1.25 });
    const [x, y] = input._relativeToServer(4, 0);
    check('a delta sent outside the coalescer is scaled the same way',
          x === 5 && y === 0, `${x},${y}`);
}

// --- a button change is not a motion --------------------------------------
{
    // Under lock this.x/this.y hold movement deltas, so a button-state send
    // must not replay them as a payload.
    const input = makeInput({ dpr: 1 });
    input.x = 999; input.y = 777;
    input.buttonMask = 1;
    globalThis.document.pointerLockElement = input.element;
    input._sendMouseState();
    check('a locked button change sends no motion',
          input.sent[input.sent.length - 1] === 'm2,0,0,1,0',
          `${input.sent[input.sent.length - 1]}`);
    globalThis.document.pointerLockElement = null;
    input.buttonMask = 0;
    input._sendMouseState();
    check('an unlocked button change sends the position',
          input.sent[input.sent.length - 1] === 'm,999,777,0,0',
          `${input.sent[input.sent.length - 1]}`);
}
{
    const sink = makeSink(1920, 1080, 1920, 1080);
    const input = makeInput({ dpr: 1, sink });
    globalThis.document.pointerLockElement = sink;
    input.x = 400; input.y = 300;
    input._sendMouseState();
    check('a lock held on the canvas counts as a stream lock',
          input.sent[input.sent.length - 1] === 'm2,0,0,0,0',
          `${input.sent[input.sent.length - 1]}`);
}

// --- absolute motion is rounded once too ----------------------------------
{
    // Rounding the frame-space coordinate before the device scale would make
    // odd server pixels unreachable at a device pixel ratio of 2.
    const input = makeInput({ dpr: 2 });
    input._mouseButtonMovement({ type: 'mousemove', target: input.element,
                                 clientX: 100.3, clientY: 0 });
    const queued = input._pendingMove;
    check('absolute motion reaches every server pixel at dpr 2',
          queued !== null && queued.mtype === 'm' && queued.x === 201,
          `${queued && `${queued.mtype},${queued.x}`}`);
}

// --- a button event maps its own position ---------------------------------
{
    // The lock leaves movement deltas behind, so a release that arrives after
    // it ends has no position of its own unless it maps one.
    const input = makeInput({ dpr: 1 });
    globalThis.document.pointerLockElement = input.element;
    input._mouseButtonMovement({ type: 'mousemove', target: input.element,
                                 movementX: 37, movementY: -21 });
    globalThis.document.pointerLockElement = null;
    input.buttonMask = 1;
    input._mouseButtonMovement({ type: 'mouseup', target: input.element,
                                 button: 0, clientX: 640, clientY: 360 });
    check('a button event after the lock ends carries its own position',
          input.sent[input.sent.length - 1] === 'm,640,360,0,0',
          `${input.sent[input.sent.length - 1]}`);
    check('the locked motion before it went out as a delta',
          input.sent.includes('m2,37,-21,0,0'), `${input.sent}`);
}

// --- a non-finite delta cannot latch the accumulator ----------------------
{
    const input = makeInput({ dpr: 1 });
    input._quantizeRelative(NaN, 0);
    const [x, y] = input._relativeToServer(10, 4);
    check('motion after a non-finite delta still lands', x === 10 && y === 4,
          `${x},${y}`);
}

process.exit(failed === 0 ? 0 : 1);
