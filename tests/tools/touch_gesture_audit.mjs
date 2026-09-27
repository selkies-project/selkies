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

// --- two-finger scroll follows finger travel -------------------------------

/** Scroll notches on the wire per direction, from each rising edge of a scroll bit times its magnitude. */
function notches(sent) {
    const bits = { down: 8, up: 16, left: 64, right: 128 };
    const out = { down: 0, up: 0, left: 0, right: 0 };
    let prev = 0;
    for (const [, msg] of sent) {
        const p = msg.split(',');
        if (p[0] !== 'm' && p[0] !== 'm2') continue;
        const mask = Number(p[3]);
        for (const [name, bit] of Object.entries(bits)) {
            if ((mask & bit) && !(prev & bit)) out[name] += Number(p[4]);
        }
        prev = mask;
    }
    return out;
}

/** Two fingers 100 px apart travel (dx, dy) in `steps` equal touchmoves. */
function swipe(input, dx, dy, steps, x0 = 400, y0 = 500) {
    const a = touch(1, x0, y0);
    const b = touch(2, x0 + 100, y0);
    fire(input, 'touchstart', [a], [a]);
    advance(5);
    fire(input, 'touchstart', [b], [a, b]);
    for (let i = 1; i <= steps; i++) {
        advance(8);
        const pa = touch(1, x0 + dx * i / steps, y0 + dy * i / steps);
        const pb = touch(2, x0 + 100 + dx * i / steps, y0 + dy * i / steps);
        fire(input, 'touchmove', [pa, pb], [pa, pb]);
    }
    advance(8);
    const ea = touch(1, x0 + dx, y0 + dy);
    const eb = touch(2, x0 + 100 + dx, y0 + dy);
    fire(input, 'touchend', [ea, eb], []);
    advance(500);
}

for (const trackpad of [true, false]) {
    const mode = trackpad ? 'trackpad' : 'direct touch';
    const counts = [];
    for (const [steps, label] of [[10, '10 x 20 px'], [40, '40 x 5 px'], [100, '100 x 2 px'], [200, '200 x 1 px']]) {
        const { input, sent } = makeInput(trackpad);
        swipe(input, 0, -200, steps);
        counts.push([label, notches(sent)]);
    }
    const downs = counts.map(([, n]) => n.down);
    check(`${mode}: a 200 px two-finger scroll is as long in any number of events`,
          Math.max(...downs) - Math.min(...downs) <= 1 && Math.min(...downs) >= 3,
          counts.map(([l, n]) => `${l}: ${n.down}`).join('; '));
    check(`${mode}: fingers moving up scroll down, and only down`,
          counts.every(([, n]) => n.up === 0 && n.left === 0 && n.right === 0),
          JSON.stringify(counts[0][1]));
    {
        const { input, sent } = makeInput(trackpad);
        swipe(input, 0, -60, 120);
        const n = notches(sent);
        check(`${mode}: a slow scroll, half a pixel per event, still scrolls`, n.down === 1, JSON.stringify(n));
    }
    {
        const { input, sent } = makeInput(trackpad);
        swipe(input, -200, 0, 40);
        const n = notches(sent);
        check(`${mode}: fingers moving left scroll right, as the content follows them`,
              n.right >= 3 && n.left === 0 && n.up === 0 && n.down === 0, JSON.stringify(n));
    }
    {
        const { input, sent } = makeInput(trackpad);
        swipe(input, 45, -300, 60);
        const n = notches(sent);
        check(`${mode}: the drift of a vertical scroll does not scroll sideways`,
              n.down >= 5 && n.left === 0 && n.right === 0, JSON.stringify(n));
    }
}

// --- the finger left over from a multi-finger gesture ----------------------

/** Net relative motion on the wire, in server pixels. */
function travel(sent) {
    let x = 0;
    let y = 0;
    for (const [, msg] of sent) {
        const p = msg.split(',');
        if (p[0] === 'm2') { x += Number(p[1]); y += Number(p[2]); }
    }
    return [x, y];
}

/** Two fingers scroll 100 px up, then one lifts and the other stays down. */
function scrollThenLiftOne(input) {
    const a = touch(1, 400, 500);
    const b = touch(2, 500, 500);
    fire(input, 'touchstart', [a], [a]);
    advance(5);
    fire(input, 'touchstart', [b], [a, b]);
    for (let i = 1; i <= 10; i++) {
        advance(16);
        fire(input, 'touchmove', [touch(1, 400, 500 - 10 * i), touch(2, 500, 500 - 10 * i)],
             [touch(1, 400, 500 - 10 * i), touch(2, 500, 500 - 10 * i)]);
    }
    advance(16);
    fire(input, 'touchend', [touch(2, 500, 400)], [touch(1, 400, 400)]);
}
{
    const { input, sent } = makeInput();
    scrollThenLiftOne(input);
    advance(30);
    fire(input, 'touchmove', [touch(1, 404, 401)], [touch(1, 404, 401)]);
    const rolled = travel(sent);
    advance(300);
    const before = travel(sent);
    for (let i = 1; i <= 10; i++) {
        advance(16);
        fire(input, 'touchmove', [touch(1, 404 + 10 * i, 401)], [touch(1, 404 + 10 * i, 401)]);
    }
    const moved = travel(sent);
    check('the finger left after a two-finger scroll moves the pointer again',
          moved[0] - before[0] === 100 && moved[1] === before[1], `${moved[0] - before[0]},${moved[1] - before[1]}`);
    check('but not while the other one is still leaving', rolled[0] === 0 && rolled[1] === 0, `${rolled}`);
    fire(input, 'touchend', [touch(1, 504, 401)], []);
    advance(500);
    const clicks = leftTransitions(sent);
    check('and lifting it after moving it clicks nothing', clicks.length === 0, JSON.stringify(clicks));
}
{
    // A two-finger tap whose fingers lift one after the other: one right click,
    // and the last finger up is no left click.
    const { input, sent } = makeInput();
    const a = touch(1, 400, 500);
    const b = touch(2, 500, 500);
    fire(input, 'touchstart', [a], [a]);
    advance(5);
    fire(input, 'touchstart', [b], [a, b]);
    advance(60);
    fire(input, 'touchend', [b], [a]);
    advance(40);
    fire(input, 'touchend', [a], []);
    advance(500);
    check('a two-finger tap lifted one finger at a time is one right click',
          leftTransitions(sent, 4).map(([, x]) => x).join(',') === 'down,up' && leftTransitions(sent).length === 0,
          `right=${JSON.stringify(leftTransitions(sent, 4))} left=${JSON.stringify(leftTransitions(sent))}`);
}
{
    const { input, sent } = makeInput();
    const a = touch(1, 400, 500);
    const b = touch(2, 460, 500);
    const c = touch(3, 520, 500);
    fire(input, 'touchstart', [a], [a]);
    advance(5);
    fire(input, 'touchstart', [b], [a, b]);
    advance(5);
    fire(input, 'touchstart', [c], [a, b, c]);
    advance(60);
    fire(input, 'touchend', [a, b, c], []);
    advance(500);
    const middle = leftTransitions(sent, 2).map(([, x]) => x).join(',');
    check('a three-finger tap is a middle click, and nothing else',
          middle === 'down,up' && leftTransitions(sent).length === 0 && leftTransitions(sent, 4).length === 0,
          `middle=${middle} left=${leftTransitions(sent).length} right=${leftTransitions(sent, 4).length}`);
}

// --- trackpad motion goes out as the digitizer reports it -------------------
{
    // pointerrawupdate carries each sample as it arrives; the frame-aligned
    // touchmove that reports the same travel afterwards adds nothing.
    const { input, sent } = makeInput();
    let t = touch(1, 300, 300);
    fire(input, 'touchstart', [t], [t]);
    const immediate = [];
    for (let frame = 1; frame <= 5; frame++) {
        for (let i = 1; i <= 4; i++) {
            advance(4);
            const x = 300 + (frame - 1) * 20 + 5 * i;
            const before = sent.length;
            input._handleRawPointerUpdate({ type: 'pointerrawupdate', pointerType: 'touch', isPrimary: true,
                                            clientX: x, clientY: 300, target: element });
            immediate.push(sent.length > before);
        }
        t = touch(1, 300 + frame * 20, 300);
        fire(input, 'touchmove', [t], [t]);
    }
    fire(input, 'touchend', [t], []);
    advance(500);
    const [x] = travel(sent);
    check('trackpad motion from raw pointer updates goes out as each arrives',
          immediate.every(Boolean), `${immediate.filter(Boolean).length}/${immediate.length}`);
    check('and the touchmoves reporting the same travel add none', x === 100, `${x} of 100`);
}

// --- a pinch is Ctrl+wheel ---------------------------------------------------

const CONTROL_L = 0xffe3;

/**
 * Zoom notches on the wire and whether every one went out under a Control
 * that went down just before it and up right after.
 */
function zooms(sent) {
    let control = false;
    let wrapped = true;
    let prev = 0;
    const out = { in: 0, out: 0, wrapped: true, bare: 0 };
    for (const [, msg] of sent) {
        const p = msg.split(',');
        if (p[0] === 'kd' && Number(p[1]) === CONTROL_L) control = true;
        if (p[0] === 'ku' && Number(p[1]) === CONTROL_L) control = false;
        if (p[0] !== 'm' && p[0] !== 'm2') continue;
        const mask = Number(p[3]);
        const rise = (bit) => (mask & bit) && !(prev & bit);
        if (rise(16) || rise(8)) {
            if (!control) { wrapped = false; out.bare += Number(p[4]); }
            if (rise(16)) out.in += Number(p[4]); else out.out += Number(p[4]);
        }
        prev = mask;
    }
    out.wrapped = wrapped && !control;
    return out;
}

/** Two fingers, midpoint fixed, from `from` to `to` px apart in `steps` touchmoves. */
function pinch(input, from, to, steps) {
    const cx = 600;
    const cy = 400;
    const at = (d) => [touch(1, cx - d / 2, cy), touch(2, cx + d / 2, cy)];
    const [a0, b0] = at(from);
    fire(input, 'touchstart', [a0], [a0]);
    advance(5);
    fire(input, 'touchstart', [b0], [a0, b0]);
    for (let i = 1; i <= steps; i++) {
        advance(16);
        const pts = at(from + (to - from) * i / steps);
        fire(input, 'touchmove', pts, pts);
    }
    advance(16);
    const end = at(to);
    fire(input, 'touchend', end, []);
    advance(300);
}

for (const trackpad of [true, false]) {
    const mode = trackpad ? 'trackpad' : 'direct touch';
    {
        const { input, sent } = makeInput(trackpad);
        pinch(input, 100, 200, 30);
        const z = zooms(sent);
        check(`${mode}: spreading two fingers to twice apart zooms in by about five Ctrl+wheel notches`,
              z.in >= 4 && z.in <= 5 && z.out === 0 && z.wrapped, JSON.stringify(z));
    }
    {
        const { input, sent } = makeInput(trackpad);
        pinch(input, 300, 150, 30);
        const z = zooms(sent);
        check(`${mode}: pinching them to half apart zooms out as far`,
              z.out >= 4 && z.out <= 5 && z.in === 0 && z.wrapped, JSON.stringify(z));
    }
    {
        const { input, sent } = makeInput(trackpad);
        swipe(input, 0, -200, 40);
        const keys = sent.filter(([, m]) => m.startsWith('kd,') || m.startsWith('ku,')).length;
        check(`${mode}: a two-finger scroll is no pinch`, keys === 0, `${keys} key messages`);
    }
}

/** A wheel event as the page receives it. */
const wheel = (deltaY, ctrlKey, deltaMode = 0) => ({
    type: 'wheel', deltaY, deltaX: 0, deltaMode, ctrlKey, target: element, preventDefault() {},
});
{
    // Chromium's touchpad pinch: Ctrl+wheel steps of -100 ln(scale).
    const { input, sent } = makeInput(false);
    for (let i = 0; i < 20; i++) {
        advance(16);
        input._mouseWheelWrapper(wheel(-100 * Math.log(1.035), true));
    }
    advance(300);
    const z = zooms(sent);
    check('a touchpad pinch to twice the scale zooms in by about five Ctrl+wheel notches',
          z.in >= 4 && z.in <= 5 && z.out === 0 && z.wrapped, JSON.stringify(z));
}
{
    // A wheel turned under a Control the page never saw go down.
    const { input, sent } = makeInput(false);
    advance(2000);
    input._mouseWheelWrapper(wheel(100, true));
    advance(300);
    const z = zooms(sent);
    check('a wheel notch under an unseen Control is one Ctrl+wheel notch',
          z.out === 1 && z.in === 0 && z.wrapped, JSON.stringify(z));
}
{
    // Control held through the page: the wheel is the user's own Ctrl+wheel.
    const { input, sent } = makeInput(false);
    input._keyDownList.ControlLeft = CONTROL_L;
    advance(2000);
    input._mouseWheelWrapper(wheel(100, true));
    advance(300);
    const keys = sent.filter(([, m]) => m.startsWith('kd,') || m.startsWith('ku,')).length;
    const n = notches(sent);
    check('a wheel under a Control the page holds adds no Control of its own',
          keys === 0 && n.down >= 1, `keys=${keys} ${JSON.stringify(n)}`);
}

process.exit(failed === 0 ? 0 : 1);
