/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the client releases when its page stops getting input. A press whose
// release goes to another tab or app (a tab switched mid-press, a touch the
// system took for a gesture) leaves the remote button down: every motion after
// it drags, and the next press goes to a button already down and does nothing.
// Hidden, blurred or leaving, the page lets go of every key, button and touch it
// holds, and a touch whose end went elsewhere is forgotten when the next lands.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

import { Input } from '../../addons/selkies-web-core/lib/input.ts';

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [held-input] ${label}  ${detail}`);
}

function makeInput({ locked = false } = {}) {
    globalThis.window = { devicePixelRatio: 1, requestAnimationFrame: () => {} };
    const element = {};
    globalThis.document = { pointerLockElement: locked ? element : null, visibilityState: 'visible' };
    const input = Object.create(Input.prototype);
    input.element = element;
    input.buttonMask = 0;
    input.x = 0;
    input.y = 0;
    input._pendingMove = null;
    input._moveFlushScheduled = false;
    input._pointerSeq = 0;
    input._trackpadReleaseTimer = null;
    input._activeTouches = new Map();
    input._trackpadTouches = new Map();
    input._activeTouchIdentifier = null;
    input._isTwoFingerGesture = false;
    input._twoFinger = null;
    input._longPressTimer = null;
    input._trackpadGestureMode = null;
    input._keyDownList = {};
    input._altKeysymByCode = new Map();
    input._altGrTimeout = null;
    input.keysReleased = 0;
    input._stopKeyHeartbeat = () => {};
    input._sendKeyEvent = () => { input.keysReleased += 1; };
    input.sent = [];
    input.send = (msg) => input.sent.push(msg);
    return input;
}

{
    const input = makeInput();
    input.x = 640; input.y = 360; input.buttonMask = 1;
    input._keyDownList = { KeyA: 0x61 };
    globalThis.document.visibilityState = 'hidden';
    input._onVisibilityChange();
    check('a page hidden mid-press releases the button where the pointer is',
          input.buttonMask === 0 && input.sent.length === 1 && input.sent[0].startsWith('m,640,360,0,0,'),
          JSON.stringify(input.sent));
    check('and the keys it holds, as before', input.keysReleased === 1, input.keysReleased);
}

{
    const input = makeInput();
    input.x = 10; input.y = 20; input.buttonMask = 0;
    globalThis.document.visibilityState = 'hidden';
    input._onVisibilityChange();
    check('one holding no button sends no pointer message', input.sent.length === 0, JSON.stringify(input.sent));
}

{
    const input = makeInput();
    input.buttonMask = 1;
    globalThis.document.visibilityState = 'visible';
    input._onVisibilityChange();
    check('a page shown again releases nothing', input.buttonMask === 1 && input.sent.length === 0,
          JSON.stringify(input.sent));
}

{
    const input = makeInput({ locked: true });
    input.x = 7; input.y = -3; input.buttonMask = 5;
    input.releaseInput();
    check('under pointer lock the release is a zero delta, not the motion the coordinates hold',
          input.buttonMask === 0 && input.sent.length === 1 && input.sent[0].startsWith('m2,0,0,0,0,'),
          JSON.stringify(input.sent));
}

{
    const input = makeInput();
    input.buttonMask = 1;
    let fired = false;
    input._trackpadReleaseTimer = setTimeout(() => { fired = true; }, 10);
    input.releaseInput();
    await new Promise((resolve) => setTimeout(resolve, 30));
    check("a trackpad tap's pending release is cancelled with it", !fired && input._trackpadReleaseTimer === null
          && input.sent.length === 1, JSON.stringify(input.sent));
}

{
    const input = makeInput();
    input._activeTouches.set(7, { identifier: 7 });
    input._isTwoFingerGesture = true;
    input._trackpadTouches.set(3, { id: 3 });
    globalThis.document.visibilityState = 'hidden';
    input._onVisibilityChange();
    check('a page hidden mid-touch forgets the touches and the gesture in progress',
          input._activeTouches.size === 0 && input._trackpadTouches.size === 0 && !input._isTwoFingerGesture);
}

// A touch whose end went elsewhere stays tracked: Edge numbers each touch anew, so the next tap is a second
// finger, a two-finger gesture that scrolls and never clicks.
const touchstart = (ids, changed) => ({
    touches: ids.map((identifier) => ({ identifier })),
    changedTouches: changed.map((identifier) => ({ identifier })),
});
{
    const input = makeInput();
    input._activeTouches.set(11, { identifier: 11 });
    input._activeTouchIdentifier = 11;
    input.buttonMask = 1;
    input.x = 300; input.y = 200;
    input._forgetLiftedTouches(input._activeTouches, touchstart([12], [12]));
    check('a touch no longer on the surface is forgotten when the next lands, with the button it held',
          input._activeTouches.size === 0 && input._activeTouchIdentifier === null && input.buttonMask === 0
          && input.sent.length === 1 && input.sent[0].startsWith('m,300,200,0,0,'), JSON.stringify(input.sent));
}
{
    const input = makeInput();
    input._activeTouches.set(21, { identifier: 21 });
    input._isTwoFingerGesture = false;
    input._forgetLiftedTouches(input._activeTouches, touchstart([21, 22], [22]));
    check('a finger still down is kept: the second makes the two-finger gesture it should',
          input._activeTouches.size === 1 && input._activeTouches.has(21) && input.sent.length === 0);
}
{
    const input = makeInput();
    input._trackpadTouches.set(5, { id: 5 });
    input._trackpadGestureMode = 'scroll';
    input._forgetLiftedTouches(input._trackpadTouches, touchstart([6], [6]));
    check("the trackpad's touches heal the same way", input._trackpadTouches.size === 0
          && input._trackpadGestureMode === null);
}

process.exit(failed ? 1 : 0);
