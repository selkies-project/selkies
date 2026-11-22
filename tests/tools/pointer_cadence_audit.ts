/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * When the client puts pointer motion on the wire, on a virtual clock. A sample
 * has to leave as it arrives -- from pointerrawupdate where the engine fires
 * it, since mousemove waits for the next frame -- and never be held longer
 * than the send interval, while a 1000 Hz mouse is still thinned to one
 * message per interval and a transport that is not draining gets one per
 * frame. The frame-aligned mousemove that follows a raw update reports the
 * same motion, so it must not send it again: under pointer lock that would be
 * a doubled delta.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

import { Input } from '../../web/selkies-web-core/lib/input.ts';
import { type StandIn, type VirtualTimer, standIn } from './stand_in.ts';

let failed = 0;

/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [pointer-cadence] ${label}  ${String(detail)}`);
}

// --- a virtual clock, with animation frames every 16 ms ---------------------
let now = 1000;
let timers: VirtualTimer[] = [];
let timerSeq = 0;
Object.defineProperty(globalThis, 'performance', { value: { now: () => now }, configurable: true });
Object.assign(globalThis, {
    setTimeout: (fn: () => void, ms = 0) => {
        const id = ++timerSeq;
        timers.push({ id, at: now + Math.max(0, ms), fn });
        return id;
    },
    clearTimeout: (id: number) => { timers = timers.filter((t) => t.id !== id); },
    setInterval: () => 0,
    clearInterval: () => {},
});
let frames: FrameRequestCallback[] = [];
const nextFrameAt = (): number => Math.ceil((now + 0.001) / 16) * 16;

/** Runs every timer and animation frame due within `ms`, in order. */
function advance(ms: number): void {
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

const element: StandIn<HTMLElement> = {
    style: { setProperty() {} },
    classList: { contains: () => false },
    contains: (node) => node === element,
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 1280, height: 720 }),
    offsetWidth: 1280,
    offsetHeight: 720,
    focus() {},
};
/** The stand-in window, a top-level page's unless a check gives it a parent. */
const win: StandIn<Window> = {
    devicePixelRatio: 1,
    requestAnimationFrame: (cb: FrameRequestCallback) => { frames.push(cb); return frames.length; },
    addEventListener() {},
    removeEventListener() {},
    location: { origin: 'http://localhost' },
};
Object.assign(globalThis, { window: win });
/** The stand-in document, whose pointer lock a check takes and gives up. */
const page = {
    createElement: () => ({ style: {}, getContext: () => ({}) }) satisfies StandIn<HTMLCanvasElement>,
    body: { appendChild() {} },
    getElementById: () => null,
    pointerLockElement: null as object | null,
    activeElement: null,
};
Object.assign(globalThis, { document: page satisfies StandIn<Document> });

/** What an input sent, with when on the audit's clock. */
type Sent = Array<[at: number, message: string]>;

/** An input recording what it sends, and when. */
function makeInput(): { input: Input; sent: Sent } {
    const sent: Sent = [];
    const input = new Input(standIn<HTMLElement>(element), (msg) => sent.push([now, msg]));
    input.sendMotion = (msg) => sent.push([now, msg]);
    input.inputAttached = false;
    return { input, sent };
}

const motion = (sent: Sent): Sent => sent.filter(([, m]) => m.startsWith('m,') || m.startsWith('m2,'));
const move = (type: string, x: number, extra: StandIn<PointerEvent> = {}): PointerEvent => standIn<PointerEvent>({
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
    page.pointerLockElement = element;
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
    page.pointerLockElement = null;
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
    const samples: PointerEvent[] = [];
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

// --- a pen stroke where no pointerrawupdate carries it ----------------------
// Safari has no pointerrawupdate: an Apple Pencil reports at 240 Hz and the
// page gets one pointermove per 60 Hz frame, with the three positions before
// its own coalesced into it. Half a second of handwriting, loops of 15 px at
// 1 px/ms: how many of the pen's positions reach the wire, how far the
// polyline they draw strays from the one the pen drew, and whether each
// event's own position, the newest, leaves as the event is handled rather
// than behind the ones it coalesced.
function penStroke(input: Input, sent: Sent): { perSecond: number; err: number; newestAtOnce: number } {
    const t0 = now;
    const pen = (i: number): PointerEvent => {
        const a = (i * 4.1667) / 15;
        const x = 640 + i * 0.4 + 15 * Math.cos(a);
        const y = 360 + 15 * Math.sin(a);
        return move('pointermove', x, { clientY: y, screenY: y, pointerType: 'pen', buttons: 1,
                                        timeStamp: t0 + i * 4.1667 });
    };
    input.buttonMask = 1;
    const truth = [];
    let newestAtOnce = 0;
    for (let frame = 0; frame < 30; frame++) {
        const samples = [0, 1, 2, 3].map((k) => pen(frame * 4 + k));
        truth.push(...samples);
        advance(16.6667);
        const ev = samples[3];
        ev.getCoalescedEvents = () => samples;
        input._handlePointerMove(ev);
        const last = motion(sent).at(-1);
        if (last && last[1].startsWith(`m,${Math.round(ev.clientX)},${Math.round(ev.clientY)},`)) newestAtOnce++;
    }
    advance(50);
    const pts = motion(sent).map(([, m]) => m.split(',').slice(1, 3).map(Number));
    const seg = (p: number[], a: number[], b: number[]): number => {
        const [dx, dy] = [b[0] - a[0], b[1] - a[1]];
        const len = dx * dx + dy * dy;
        const t = len ? Math.max(0, Math.min(1, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len)) : 0;
        return Math.hypot(p[0] - a[0] - t * dx, p[1] - a[1] - t * dy);
    };
    let err = 0;
    for (const s of truth.slice(4)) {
        const p = [s.clientX, s.clientY];
        let best = Infinity;
        for (let i = 1; i < pts.length; i++) best = Math.min(best, seg(p, pts[i - 1], pts[i]));
        err = Math.max(err, best);
    }
    return { perSecond: Math.round(pts.length / 0.5), err, newestAtOnce };
}
{
    const { input, sent } = makeInput();
    advance(100);
    const got = penStroke(input, sent);
    check('a pen stroke without pointerrawupdate sends every position the pen reported',
          got.perSecond >= 230, `${got.perSecond} positions/s of 240`);
    check('and draws the line the pen drew, within a pixel', got.err < 1, `strays ${got.err.toFixed(2)} px`);
    check('and each event\'s own position leaves as the event is handled', got.newestAtOnce === 30,
          `${got.newestAtOnce} of 30 events`);
}

// --- a drag in a frame follows the frame's own coordinates ------------------
// Chromium gives pointerrawupdate in a frame the top-level page's client
// coordinates and the right screen ones, while pointermove has the frame's
// own: here the frame sits 400 px into its parent. Each frame of motion is a
// raw update, then the pointermove and the mousemove of the same position.
function frameDrag(offset: number | ((x: number) => number)): { xs: number[]; atOnce: number } {
    const shift = typeof offset === 'number' ? (): number => offset : offset;
    Object.defineProperty(win, 'top', { value: {}, configurable: true });
    const { input, sent } = makeInput();
    Reflect.deleteProperty(win, 'top');
    input.buttonMask = 1;
    advance(100);
    let atOnce = 0;
    for (let x = 100; x < 400; x += 10) {
        advance(16);
        const before = motion(sent).length;
        input._handleRawPointerUpdate(move('pointerrawupdate', x + shift(x), { screenX: x + 410, buttons: 1 }));
        if (motion(sent).length > before) atOnce++;
        input._checkRawPositions(move('pointermove', x, { screenX: x + 410, buttons: 1 }));
        input._mouseButtonMovement(move('mousemove', x, { screenX: x + 410, buttons: 1 }));
    }
    advance(50);
    return { xs: motion(sent).map(([, m]) => Number(m.split(',')[1])), atOnce };
}
{
    const shifted = frameDrag(400);
    check('a drag in a frame whose raw updates misplace it stays where the frame shows it',
          shifted.xs.length === 30 && shifted.xs.every((x, i) => x === 100 + 10 * i),
          shifted.xs.join(','));
    check('and goes out from the frame-aligned events', shifted.atOnce === 0, `${shifted.atOnce} raw sends`);
    Object.defineProperty(win, 'top', { value: {}, configurable: true });
    const { input, sent } = makeInput();
    Reflect.deleteProperty(win, 'top');
    page.pointerLockElement = element;
    advance(100);
    let locked = 0;
    for (let i = 0; i < 10; i++) {
        advance(16);
        const before = motion(sent).length;
        input._handleRawPointerUpdate(move('pointerrawupdate', 900, { screenX: 510, movementX: 4 }));
        if (motion(sent).length > before) locked++;
        input._checkRawPositions(move('pointermove', 100, { screenX: 510, movementX: 4 }));
        input._mouseButtonMovement(move('mousemove', 100, { screenX: 510, movementX: 4 }));
    }
    advance(50);
    page.pointerLockElement = null;
    const travel = motion(sent).reduce((sum, [, m]) => sum + Number(m.split(',')[1]), 0);
    check('under pointer lock a frame takes raw movement at once, each delta once',
          locked === 10 && travel === 40, `${locked} of 10 raw sends, ${travel} of 40 px`);
    const moved = frameDrag((x) => (x < 250 ? 0 : 400));
    const last = moved.xs.at(-1);
    check('a frame that moves mid-drag corrects the one misplaced raw update in the same frame',
          moved.xs.filter((x) => x > 400).length === 1 && last === 390, moved.xs.join(','));
    {
        Object.defineProperty(win, 'top', { value: {}, configurable: true });
        const { input, sent } = makeInput();
        Reflect.deleteProperty(win, 'top');
        input.buttonMask = 1;
        advance(100);
        for (let frame = 0; frame < 10; frame++) {
            const t0 = now;
            const samples = [0, 1, 2, 3].map((k) => move('pointermove', 100 + frame * 40 + k * 10,
                { screenX: 510 + frame * 40 + k * 10, buttons: 1, timeStamp: t0 + k * 4 }));
            const last = samples[3];
            last.getCoalescedEvents = () => samples;
            advance(16);
            input._handleRawPointerUpdate(move('pointerrawupdate', last.clientX + 400,
                { screenX: last.screenX, buttons: 1 }));
            input._checkRawPositions(last);
            input._mouseButtonMovement(move('mousemove', last.clientX, { screenX: last.screenX, buttons: 1 }));
        }
        advance(50);
        const xs = motion(sent).map(([, m]) => Number(m.split(',')[1]));
        check('a drag in a frame whose raw updates are misplaced keeps the positions its moves coalesced',
              xs.length >= 38 && xs.filter((x) => x > 500).length <= 1, xs.join(','));
    }
    const placed = frameDrag(0);
    check('a frame whose raw updates agree with it takes them after the first move',
          placed.atOnce === 29 && placed.xs.every((x, i) => i === 0 || x > placed.xs[i - 1]),
          `${placed.atOnce} of 29 raw sends, ${placed.xs.join(',')}`);
}

// --- a trackpad scroll's notch goes out with the event that completes it --
{
    const { input, sent } = makeInput();
    advance(2000);
    const deltas = [7, 9, 12, 10, 8, 11, 9, 13, 10, 8, 12, 9, 11, 10, 7, 12, 9, 10, 11, 8,
                    9, 12, 10, 8, 11, 9, 13, 10, 8, 12];
    let total = 0;
    const due = [];
    for (const d of deltas) {
        advance(16);
        const before = Math.floor(total / 100);
        total += d;
        if (Math.floor(total / 100) > before) due.push(now);
        input._mouseWheelWrapper(standIn<WheelEvent>({ type: 'wheel', deltaY: d, deltaX: 0, deltaMode: 0, ctrlKey: false,
                                   target: element, preventDefault() {} }));
    }
    advance(500);
    const rises: number[] = [];
    let prev = 0;
    for (const [t, m] of sent) {
        const p = m.split(',');
        const mask = Number(p[3]);
        if ((mask & 8) && !(prev & 8)) rises.push(t);
        prev = mask;
    }
    const lags = due.map((t, i) => (rises[i] === undefined ? Infinity : rises[i] - t));
    check('a trackpad scroll notch goes out with the event that completes it',
          rises.length === due.length && lags.every((l) => l === 0), `lags ${lags.join(',')} ms`);
}

process.exit(failed === 0 ? 0 : 1);
