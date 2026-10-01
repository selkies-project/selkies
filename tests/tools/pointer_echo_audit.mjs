/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Where trackpad mode draws the cursor, on a virtual clock. The page asks the
// server to echo the pointer, draws the cursor at the echo moved on by the
// deltas it sent past the message the echo includes, and has the cursor
// composited into the video instead when no echo comes.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { Input } from '../../addons/selkies-web-core/lib/input.js';

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [pointer-echo-client] ${label}  ${detail}`);
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

const settle = () => new Promise((resolve) => setImmediate(resolve));

/** A page in trackpad mode with a cursor image, recording what it sends. */
async function makeInput() {
    const sent = [];
    const input = new Input(element, (msg) => sent.push(msg));
    input.sendMotion = (msg) => sent.push(msg);
    input.inputAttached = true;
    input._cursorImageBitmap = { width: 16, height: 16 };
    input.setTrackpadMode(true);
    input.resumePointerEcho();
    await settle();
    return { input, sent };
}

/** Where the cursor's hotspot is drawn, or null while it is hidden. */
function cursorAt(input) {
    if (input.cursorDiv.style.display !== 'block') return null;
    const m = /translate\(([-\d.]+)px, ([-\d.]+)px\)/.exec(input.cursorDiv.style.transform || '');
    return m ? [Number(m[1]) + input.cursorHotspot.x, Number(m[2]) + input.cursorHotspot.y] : null;
}

const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

{
    const { input, sent } = await makeInput();
    check('trackpad mode asks for the echo once for a toggle and a reconnect together',
          same(sent, ['_pointer_echo,1']), JSON.stringify(sent));
    check('and draws nothing before the first echo', cursorAt(input) === null);

    input.onPointerEcho('pointer,primary,100,100,1,0');
    check('an echo places the cursor', same(cursorAt(input), [100, 100]), cursorAt(input));

    input._sendPointer(['m2', 10, 5, 0, 0], true);
    input._sendPointer(['m2', 10, 5, 0, 0], true);
    check('each delta sent moves it at once', same(cursorAt(input), [120, 110]), cursorAt(input));

    input.onPointerEcho('pointer,primary,110,105,1,1');
    check('an echo including part of the motion keeps the rest on top of it',
          same(cursorAt(input), [120, 110]), cursorAt(input));

    input.onPointerEcho('pointer,primary,130,108,1,2');
    check('an echo of all of it takes the server position, acceleration and all',
          same(cursorAt(input), [130, 108]), cursorAt(input));

    input._sendPointer(['m2', 0, 0, 1, 0], false);
    check('a button change moves nothing', same(cursorAt(input), [130, 108]), cursorAt(input));

    input.onPointerEcho('pointer,primary,1270,700,2,3');
    input._sendPointer(['m2', 20, 20, 0, 0], true);
    check('a delta counts by the scale the echo names, held inside the display',
          same(cursorAt(input), [1279, 719]), cursorAt(input));

    input._sendPointer(['m2', -30, 0, 0, 0], true);
    check('a delta back from the edge moves from the edge, as the session moves it',
          same(cursorAt(input), [1219, 719]), cursorAt(input));

    input.onPointerEcho('pointer,display2,10,10,1,5');
    check('an echo naming another display hides the cursor, which that page draws', cursorAt(input) === null);
    input._sendPointer(['m2', -30, 0, 0, 0], true);
    check('and moves on its deltas do not bring it back', cursorAt(input) === null);
    input.onPointerEcho('pointer,primary,1279,40,1,7');
    check('until an echo names this display again', same(cursorAt(input), [1279, 40]), cursorAt(input));

    input._mouseButtonMovement({ type: 'mousemove', clientX: 5, clientY: 5, target: element });
    check('a mouse over the page does not move the trackpad cursor',
          same(cursorAt(input), [1279, 40]), cursorAt(input));

    sent.length = 0;
    input.setTrackpadMode(false);
    check('leaving trackpad mode stops the echo without touching compositing never asked for',
          same(sent, ['_pointer_echo,0']), JSON.stringify(sent));
    check('and hides the cursor', cursorAt(input) === null);
}

{
    const { input, sent } = await makeInput();
    sent.length = 0;
    advance(1999);
    check('no echo yet asks for nothing more', sent.length === 0, JSON.stringify(sent));
    advance(2);
    check('a server that never echoes composites the cursor instead',
          same(sent, ['SET_NATIVE_CURSOR_RENDERING,1']), JSON.stringify(sent));
    check('with no cursor drawn on the page', cursorAt(input) === null);

    sent.length = 0;
    input.resumePointerEcho();
    await settle();
    input.onPointerEcho('pointer,primary,50,60,1,0');
    check('a reconnect whose server echoes takes the compositing back',
          same(sent, ['_pointer_echo,1', 'SET_NATIVE_CURSOR_RENDERING,0']), JSON.stringify(sent));
    check('and draws the cursor on the page', same(cursorAt(input), [50, 60]), cursorAt(input));
    advance(5000);
    check('an echo cancels the wait', same(sent, ['_pointer_echo,1', 'SET_NATIVE_CURSOR_RENDERING,0']),
          JSON.stringify(sent));
}

{
    const { input, sent } = await makeInput();
    sent.length = 0;
    input.onPointerEcho('pointer,none');
    check('a session that cannot say where its pointer is composites at once',
          same(sent, ['SET_NATIVE_CURSOR_RENDERING,1']), JSON.stringify(sent));
    sent.length = 0;
    input.setTrackpadMode(false);
    check('and leaving trackpad mode gives the compositing back',
          same(sent, ['_pointer_echo,0', 'SET_NATIVE_CURSOR_RENDERING,0']), JSON.stringify(sent));
}

{
    const { input } = await makeInput();
    input.onPointerEcho('pointer,primary,100,100,1,0');
    for (let i = 0; i < 600; i++) input._sendPointer(['m2', 1, 0, 0, 0], true);
    check('deltas past an echo that does not come stay bounded', input._echoDeltas.length <= 256,
          input._echoDeltas.length);
    check('without the cursor losing any of them', same(cursorAt(input), [700, 100]), cursorAt(input));
}

{
    const { input } = await makeInput();
    for (let i = 0; i < 600; i++) input._sendPointer(['m2', 1, 0, 0, 0], true);
    check('deltas before any echo stay bounded too', input._echoDeltas.length <= 256, input._echoDeltas.length);
    input.onPointerEcho('pointer,none');
    input._sendPointer(['m2', 1, 0, 0, 0], true);
    check('and none are kept while the server composites the cursor', input._echoDeltas.length === 0,
          input._echoDeltas.length);
}

{
    const { input } = await makeInput();
    input.displayId = 'display2';
    input.onPointerEcho('pointer,display2,12,34,1,0');
    check('a secondary page draws the echo that names it', same(cursorAt(input), [12, 34]), cursorAt(input));
    input.onPointerEcho('pointer,display2,x,34,1,0');
    check('a malformed echo is ignored', same(cursorAt(input), [12, 34]), cursorAt(input));
    input.onPointerEcho('pointer,primary,12,34,1,0');
    check('and the one that names the primary hides it', cursorAt(input) === null);
}

process.exit(failed ? 1 : 0);
