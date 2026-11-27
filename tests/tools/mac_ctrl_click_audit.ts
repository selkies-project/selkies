/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What a macOS Control-click puts on the wire. macOS takes a Control-click for
 * the right click, and the client sends it, by default, as what the hand did:
 * Control held and the left button. Under `mac_ctrl_click_as_right_click` it
 * goes out as a plain right click instead, on both reports the engines make of
 * it (the primary button with Control, from Blink and WebKit; the secondary
 * with `buttons` holding the primary, from Gecko): the Control the session
 * holds is released first, then the press, a drag while it is held, and its
 * release all carry the secondary button, and nothing is left held. A real
 * right click, a pen, a click with another modifier, and a page on another
 * platform keep what they send today.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

import { Input } from '../../web/selkies-web-core/lib/input.ts';
import { MAC_CTRL_CLICK_AS_RIGHT_CLICK_SPEC, resolveSpec } from '../../web/selkies-web-core/lib/conditional-settings.ts';
import type { ServerSettingEntry } from '../../web/selkies-web-core/lib/conditional-settings.ts';
import { type StandIn, standIn } from './stand_in.ts';

let failed = 0;

/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [mac-ctrl-click] ${label}  ${String(detail)}`);
}

/** The keysym the session holds Control as, and its keyup and keydown on the wire. */
const CONTROL_L = 0xffe3;
const CONTROL_DOWN = `kd,${CONTROL_L}`;
const CONTROL_UP = `ku,${CONTROL_L}`;
/** A press and release of one button at the audit's pointer position, in the wire's numbering. */
const LEFT_CLICK = ['m,10,20,1,0', 'm,10,20,0,0'];
const RIGHT_CLICK = ['m,10,20,4,0', 'm,10,20,0,0'];

// Motion is sent at once when the last went out long enough ago; every event
// here comes 10 ms after the one before it, so none is held back.
let now = 1000;
Object.defineProperty(globalThis, 'performance', { value: { now: () => now }, configurable: true });
Object.assign(globalThis, { setInterval: () => 0, clearInterval: () => {} });

const element: StandIn<HTMLElement> = {
    style: { setProperty() {} },
    classList: { contains: () => false },
    contains: (node) => node === element,
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 1280, height: 720 }),
    offsetWidth: 1280,
    offsetHeight: 720,
    focus() {},
};
Object.assign(globalThis, {
    window: {
        devicePixelRatio: 1,
        requestAnimationFrame: () => 0,
        addEventListener() {},
        removeEventListener() {},
        location: { origin: 'http://localhost' },
    } satisfies StandIn<Window>,
    document: {
        createElement: () => ({ style: {}, getContext: () => ({}) }),
        body: { appendChild() {} },
        getElementById: () => null,
        pointerLockElement: null,
        activeElement: null,
    } satisfies StandIn<Document>,
});

/** Stands the page on a platform, as `navigator.platform` names it. */
function setPlatform(platform: string): void {
    Object.defineProperty(globalThis, 'navigator', {
        value: { platform, userAgent: 'Mozilla/5.0', maxTouchPoints: 0 } satisfies StandIn<Navigator>,
        configurable: true, writable: true,
    });
}

/** What one press, move, or release reports. */
interface Report {
    button?: number;
    buttons?: number;
    x?: number;
    ctrl?: boolean;
    shift?: boolean;
    alt?: boolean;
    meta?: boolean;
    pen?: boolean;
}

/** A mouse event, or a pen's pointer event, over the stream. */
function pointerEvent(type: string, r: Report): PointerEvent {
    const held: Record<string, boolean> = { Control: !!r.ctrl, Shift: !!r.shift, Alt: !!r.alt, Meta: !!r.meta };
    const x = r.x ?? 10;
    return standIn<PointerEvent>({
        type, target: element, clientX: x, clientY: 20, screenX: x, screenY: 20,
        button: r.button ?? 0, buttons: r.buttons ?? 0, movementX: 0, movementY: 0, timeStamp: now,
        ctrlKey: !!r.ctrl, shiftKey: !!r.shift, altKey: !!r.alt, metaKey: !!r.meta,
        getModifierState: (name: string) => held[name] === true,
        preventDefault() {},
        ...(r.pen ? { pointerType: 'pen' } : {}),
    });
}

/** One thing the hand does, in order. */
type Step = ['ctrl-down' | 'ctrl-up'] | ['down' | 'move' | 'up', Report];

/** An input on a platform, the setting as given, recording what it sends. */
function makeInput(platform: string, on: boolean): { input: Input; sent: string[] } {
    setPlatform(platform);
    Input.macCtrlClickAsRightClick = on;
    const sent: string[] = [];
    const input = new Input(standIn<HTMLElement>(element), (msg) => sent.push(msg));
    input.inputAttached = false;
    return { input, sent };
}

/** Runs the steps through the handlers a page's listeners call. */
function play(input: Input, steps: Step[]): void {
    for (const step of steps) {
        now += 10;
        if (step.length === 1) {
            input._sendKeyEvent(CONTROL_L, 'ControlLeft', step[0] === 'ctrl-down');
            continue;
        }
        const [kind, r] = step;
        const type = r.pen ? { down: 'pointerdown', move: 'pointermove', up: 'pointerup' }[kind]
            : { down: 'mousedown', move: 'mousemove', up: 'mouseup' }[kind];
        const event = pointerEvent(type, r);
        if (!r.pen) input._mouseButtonMovement(event);
        else if (kind === 'down') input._handlePointerDown(event);
        else if (kind === 'move') input._handlePointerMove(event);
        else input._handlePointerUp(event);
    }
}

/** The wire, each pointer message without the sequence number that ends it. */
function wire(sent: string[]): string[] {
    return sent.map((m) => (m.startsWith('m,') ? m.split(',').slice(0, 5).join(',') : m));
}

/** Runs the steps on a fresh input and returns its wire, its button mask after, and the keys it still holds. */
function run(steps: Step[], { platform = 'MacIntel', on = true } = {}): { out: string[]; mask: number; keys: string[] } {
    const { input, sent } = makeInput(platform, on);
    play(input, steps);
    return { out: wire(sent), mask: input.buttonMask, keys: Object.keys(input._keyDownList) };
}

const same = (a: string[], b: string[]): boolean => JSON.stringify(a) === JSON.stringify(b);

/** A Control-click as Blink and WebKit report it: the primary button, Control held. */
const BLINK: Step[] = [
    ['ctrl-down'],
    ['down', { button: 0, buttons: 1, ctrl: true }],
    ['up', { button: 0, buttons: 0, ctrl: true }],
    ['ctrl-up'],
];
/** The same Control-click as Gecko reports it: the secondary button while `buttons` holds the primary. */
const GECKO: Step[] = [
    ['ctrl-down'],
    ['down', { button: 2, buttons: 1, ctrl: true }],
    ['up', { button: 2, buttons: 0, ctrl: true }],
    ['ctrl-up'],
];

// --- the default: what the hand did ----------------------------------------
for (const [engine, steps] of [['Blink and WebKit', BLINK], ['Gecko', GECKO]] as const) {
    const got = run(steps, { on: false });
    check(`with the setting off, ${engine}'s Control-click is Control held and the left button`,
          same(got.out, [CONTROL_DOWN, ...LEFT_CLICK, CONTROL_UP]) && got.mask === 0, got.out.join(' '));
}

// --- the setting on, on a Mac ----------------------------------------------
for (const [engine, steps] of [['Blink and WebKit', BLINK], ['Gecko', GECKO]] as const) {
    const got = run(steps);
    check(`with the setting on, ${engine}'s Control-click releases Control and is a right click`,
          same(got.out, [CONTROL_DOWN, CONTROL_UP, ...RIGHT_CLICK]), got.out.join(' '));
    check(`and leaves no button or key held after ${engine}'s`, got.mask === 0 && got.keys.length === 0,
          `mask ${got.mask}, keys ${got.keys.join(',')}`);
}

{
    // The hand holds the primary through the drag, and motion over the stream says so.
    const got = run([
        ['ctrl-down'],
        ['move', { x: 9 }],
        ['down', { button: 0, buttons: 1, ctrl: true }],
        ['move', { x: 11, buttons: 1, ctrl: true }],
        ['move', { x: 12, buttons: 1, ctrl: true }],
        ['up', { x: 12, button: 0, buttons: 0, ctrl: true }],
        ['move', { x: 13, ctrl: true }],
    ]);
    check('a drag holds the right button throughout and never the left',
          same(got.out, [CONTROL_DOWN, 'm,9,20,0,0', CONTROL_UP, 'm,10,20,4,0', 'm,11,20,4,0',
                         'm,12,20,4,0', 'm,12,20,0,0', 'm,13,20,0,0']), got.out.join(' '));
}

{
    const got = run([
        ['down', { button: 2, buttons: 1, ctrl: true }],
        ['move', { x: 11, buttons: 0 }],
        ['down', { x: 11, button: 0, buttons: 1 }],
        ['move', { x: 12, buttons: 1 }],
        ['up', { x: 12, button: 0, buttons: 0 }],
    ]);
    check('a Control-click whose release went elsewhere ends with the next motion, and a left drag after it is left',
          same(got.out, ['m,10,20,4,0', 'm,11,20,0,0', 'm,11,20,1,0', 'm,12,20,1,0', 'm,12,20,0,0']),
          got.out.join(' '));
}

{
    const { input, sent } = makeInput('MacIntel', true);
    play(input, [['down', { button: 0, buttons: 1, ctrl: true }]]);
    input.releaseInput();
    play(input, [['down', { button: 0, buttons: 1 }], ['move', { x: 11, buttons: 1 }],
                 ['up', { x: 11, button: 0, buttons: 0 }]]);
    const out = wire(sent);
    check('a page that lets go mid-press ends the Control-click with it',
          same(out, ['m,10,20,4,0', 'm,10,20,0,0', 'm,10,20,1,0', 'm,11,20,1,0', 'm,11,20,0,0']), out.join(' '));
}

// --- what keeps its meaning ------------------------------------------------
{
    const got = run([['ctrl-down'], ['down', { button: 2, buttons: 2, ctrl: true }],
                     ['up', { button: 2, buttons: 0, ctrl: true }]]);
    check('a real right click with Control held keeps Control held', same(got.out, [CONTROL_DOWN, ...RIGHT_CLICK])
          && same(got.keys, ['ControlLeft']), got.out.join(' '));
}
{
    const got = run([['down', { button: 0, buttons: 1 }], ['up', { button: 0, buttons: 0 }]]);
    check('a plain left click is a left click', same(got.out, LEFT_CLICK), got.out.join(' '));
}
for (const [chord, extra] of [['Shift', { shift: true }], ['Option', { alt: true }], ['Command', { meta: true }]] as const) {
    const { input, sent } = makeInput('MacIntel', true);
    input.setShortcutsEnabled(false);
    play(input, [['ctrl-down'], ['down', { button: 0, buttons: 1, ctrl: true, ...extra }],
                 ['up', { button: 0, buttons: 0, ctrl: true, ...extra }]]);
    const out = wire(sent);
    check(`a Control+${chord}+click is still Control and the left button`,
          same(out, [CONTROL_DOWN, ...LEFT_CLICK]), out.join(' '));
}
{
    const got = run([['ctrl-down'], ['down', { button: 0, buttons: 1, ctrl: true, pen: true }],
                     ['up', { button: 0, buttons: 0, ctrl: true, pen: true }]]);
    check("a pen's tap with Control held is still Control and the left button",
          same(got.out, [CONTROL_DOWN, ...LEFT_CLICK]), got.out.join(' '));
}
for (const platform of ['Win32', 'Linux x86_64']) {
    const got = run(BLINK, { platform });
    check(`on ${platform}, where Control-click is no right click, it is still Control and the left button`,
          same(got.out, [CONTROL_DOWN, ...LEFT_CLICK, CONTROL_UP]), got.out.join(' '));
}
{
    const got = run(GECKO, { platform: 'iPad' });
    check('an iPad, an Apple device to the client as it is to the Command remap, sends it as a right click too',
          same(got.out, [CONTROL_DOWN, CONTROL_UP, ...RIGHT_CLICK]), got.out.join(' '));
}

// --- the setting -----------------------------------------------------------
{
    const resolve = (stored: string | null, server: ServerSettingEntry): unknown => resolveSpec(
        MAC_CTRL_CLICK_AS_RIGHT_CLICK_SPEC, { mac_ctrl_click_as_right_click: server }, { macDesktop: true }, () => stored);
    check('the setting resolves off by default', resolve(null, { value: false }) === false);
    check("an operator's value turns it on", resolve(null, { value: true, overridden: true }) === true);
    check('a stored choice beats an operator value that is not locked',
          resolve('false', { value: true, overridden: true }) === false);
    check('a locked value beats a stored choice', resolve('true', { value: false, locked: true }) === false);
    const posted: unknown[] = [];
    MAC_CTRL_CLICK_AS_RIGHT_CLICK_SPEC.propagate!(true, {}, { postToCore: (m) => posted.push(m), postSetting: () => {} });
    check('a change is propagated to the core as setMacCtrlClickAsRightClick',
          JSON.stringify(posted) === '[{"type":"setMacCtrlClickAsRightClick","value":true}]', JSON.stringify(posted));
}
{
    const { input, sent } = makeInput('MacIntel', false);
    input.setMacCtrlClickAsRightClick(true);
    play(input, [['ctrl-down'], ['down', { button: 0, buttons: 1, ctrl: true }]]);
    input.setMacCtrlClickAsRightClick(false);
    play(input, [['move', { x: 11, buttons: 1, ctrl: true }], ['up', { x: 11, button: 0, buttons: 0, ctrl: true }]]);
    const pressed = wire(sent);
    check('turned off mid-press, the press still ends as the right click it began as',
          same(pressed, [CONTROL_DOWN, CONTROL_UP, 'm,10,20,4,0', 'm,11,20,4,0', 'm,11,20,0,0']), pressed.join(' '));
    sent.length = 0;
    play(input, [['ctrl-up'], ['ctrl-down'], ['down', { x: 11, button: 0, buttons: 1, ctrl: true }],
                 ['up', { x: 11, button: 0, buttons: 0, ctrl: true }]]);
    const next = wire(sent);
    check('and the next Control-click is Control and the left button again',
          same(next, [CONTROL_DOWN, 'm,11,20,1,0', 'm,11,20,0,0']), next.join(' '));
}

process.exit(failed === 0 ? 0 : 1);
