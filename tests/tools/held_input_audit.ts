/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the client releases when its page stops getting input. A press whose
 * release goes to another tab or app (a tab switched mid-press, a touch the
 * system took for a gesture) leaves the remote button down: every motion after
 * it drags, and the next press goes to a button already down and does nothing.
 * Hidden, blurred or leaving, the page lets go of every key, button and touch it
 * holds, and a touch whose end went elsewhere is forgotten when the next lands.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

import { Input } from '../../web/selkies-web-core/lib/input.ts';
import type { StandIn } from './stand_in.ts';

/** An `Input` that records what it would send and how many keys it released. */
type AuditedInput = Input & { sent: string[]; keysReleased: number };
/** A touch the input tracks on the surface. */
type ActiveTouch = Parameters<Input['_activeTouches']['set']>[1];
/** A touch the input tracks on the trackpad. */
type TrackpadTouch = Parameters<Input['_trackpadTouches']['set']>[1];

/** The stand-in document of the last input made, whose visibility a check turns. */
let page: { pointerLockElement: object | null; visibilityState: DocumentVisibilityState } = {
    pointerLockElement: null, visibilityState: 'visible',
};

let failed = 0;

/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [held-input] ${label}  ${String(detail)}`);
}

/** An `Input` with only what the release paths touch, over a fresh window and document. */
function makeInput({ locked = false } = {}): AuditedInput {
    const element = {};
    page = { pointerLockElement: locked ? element : null, visibilityState: 'visible' };
    Object.assign(globalThis, {
        window: { devicePixelRatio: 1, requestAnimationFrame: () => {} } satisfies StandIn<Window>,
        document: page satisfies StandIn<Document>,
    });
    const input = Object.create(Input.prototype) as AuditedInput;
    input.element = element as HTMLElement;
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
    input.send = (msg: string) => input.sent.push(msg);
    return input;
}

{
    const input = makeInput();
    input.x = 640; input.y = 360; input.buttonMask = 1;
    input._keyDownList = { KeyA: 0x61 };
    page.visibilityState = 'hidden';
    input._onVisibilityChange();
    check('a page hidden mid-press releases the button where the pointer is',
          input.buttonMask === 0 && input.sent.length === 1 && input.sent[0].startsWith('m,640,360,0,0,'),
          JSON.stringify(input.sent));
    check('and the keys it holds, as before', input.keysReleased === 1, input.keysReleased);
}

{
    const input = makeInput();
    input.x = 10; input.y = 20; input.buttonMask = 0;
    page.visibilityState = 'hidden';
    input._onVisibilityChange();
    check('one holding no button sends no pointer message', input.sent.length === 0, JSON.stringify(input.sent));
}

{
    const input = makeInput();
    input.buttonMask = 1;
    page.visibilityState = 'visible';
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
    input._activeTouches.set(7, { identifier: 7 } as ActiveTouch);
    input._isTwoFingerGesture = true;
    input._trackpadTouches.set(3, { id: 3 } as TrackpadTouch);
    page.visibilityState = 'hidden';
    input._onVisibilityChange();
    check('a page hidden mid-touch forgets the touches and the gesture in progress',
          input._activeTouches.size === 0 && input._trackpadTouches.size === 0 && !input._isTwoFingerGesture);
}

// A touch whose end went elsewhere stays tracked: Edge numbers each touch anew, so the next tap is a second
// finger, a two-finger gesture that scrolls and never clicks.
const touchstart = (ids: number[], changed: number[]): TouchEvent => ({
    touches: ids.map((identifier) => ({ identifier })),
    changedTouches: changed.map((identifier) => ({ identifier })),
}) as StandIn<TouchEvent> as TouchEvent;
{
    const input = makeInput();
    input._activeTouches.set(11, { identifier: 11 } as ActiveTouch);
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
    input._activeTouches.set(21, { identifier: 21 } as ActiveTouch);
    input._isTwoFingerGesture = false;
    input._forgetLiftedTouches(input._activeTouches, touchstart([21, 22], [22]));
    check('a finger still down is kept: the second makes the two-finger gesture it should',
          input._activeTouches.size === 1 && input._activeTouches.has(21) && input.sent.length === 0);
}
{
    const input = makeInput();
    input._trackpadTouches.set(5, { id: 5 } as TrackpadTouch);
    input._trackpadGestureMode = 'scroll';
    input._forgetLiftedTouches(input._trackpadTouches, touchstart([6], [6]));
    check("the trackpad's touches heal the same way", input._trackpadTouches.size === 0
          && input._trackpadGestureMode === null);
}

process.exit(failed ? 1 : 0);
