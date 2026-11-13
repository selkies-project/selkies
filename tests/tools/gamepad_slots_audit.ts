/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What a page sends for its pads when the server's verdict names its slots:
 * one slot as the number it always was, or a token's list of them, each pad
 * on its own slot in the browser's order (pad 0 on slot 3, pad 1 on slot 4),
 * a slot that goes released and the pads re-announced where the list moves,
 * and a rumble played on the pad driving the slot it is for.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */


import type { StandIn } from './stand_in.ts';

let failed = 0;

/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [gamepad-slots] ${label}  ${String(detail)}`);
}

Object.assign(globalThis, { setInterval: () => 0, clearInterval: () => {}, setTimeout: () => 0, clearTimeout: () => {} });
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
        postMessage() {},
        location: { origin: 'http://localhost' },
    } satisfies StandIn<Window>,
    document: {
        createElement: () => ({ style: {}, getContext: () => ({}) }) satisfies StandIn<HTMLCanvasElement>,
        body: { appendChild() {} },
        getElementById: () => null,
        pointerLockElement: null,
        activeElement: null,
    } satisfies StandIn<Document>,
});

/** A local pad: the members of a `Gamepad` the manager reads, which a check sets. */
interface FakePad {
    index: number;
    id: string;
    mapping: GamepadMappingType;
    connected: boolean;
    axes: number[];
    buttons: Array<{ pressed: boolean; touched: boolean; value: number }>;
    vibrationActuator: StandIn<GamepadHapticActuator>;
}

let pads: Array<FakePad | null> = [null, null, null, null];
Object.defineProperty(globalThis, 'navigator', {
    value: { platform: 'Linux x86_64', userAgent: 'Mozilla/5.0 (X11; Linux x86_64) Chrome/154', maxTouchPoints: 0,
             getGamepads: () => pads } satisfies StandIn<Navigator>,
    configurable: true, writable: true });

const { Input, slotList, slotPlayerIndex } = await import('../../web/selkies-web-core/lib/input.ts');

const plays: Array<[name: string, level: number | string | undefined]> = [];
const pad = (name: string, index: number): FakePad => ({
    index, id: `${name} (STANDARD GAMEPAD Vendor: 045e Product: 0b13)`, mapping: 'standard', connected: true,
    axes: [0, 0, 0, 0], buttons: Array.from({ length: 17 }, () => ({ pressed: false, touched: false, value: 0 })),
    vibrationActuator: {
        playEffect: (type, e) => { plays.push([name, e!.strongMagnitude]); return Promise.resolve('complete'); },
        reset: () => { plays.push([name, 'reset']); return Promise.resolve('complete'); },
    },
});
const press = (p: FakePad, b: number, v: number): void => { p.buttons[b] = { pressed: v > 0, touched: v > 0, value: v }; };
// A held control's heartbeat (`js,h`) rides every poll that finds one held; the checks of it are their own.
const gamepadSends = (sent: string[], beats = false): string[] => sent.filter((m) => m.startsWith('js,') && (beats || !m.startsWith('js,h')))
    .map((m) => m.split(',').slice(0, 3).join(','));

check('a verdict names one slot as its number, several as their list, and none as null',
      JSON.stringify([slotList(3), slotList([3, 4]), slotList('2'), slotList(null), slotList(0), slotList([])])
      === JSON.stringify([[3], [3, 4], [2], null, null, null]));

check('a verdict of several slots targets the first of them, as one of a single slot targets it',
      JSON.stringify([slotPlayerIndex(3), slotPlayerIndex([3, 4]), slotPlayerIndex([2]), slotPlayerIndex(null) ?? 'none',
                      slotPlayerIndex(undefined) ?? 'none', slotPlayerIndex(0) ?? 'none', slotPlayerIndex([]) ?? 'none'])
      === JSON.stringify([2, 2, 1, 'none', 'none', 'none', 'none']));

// --- a token holding slots 3 and 4 -------------------------------------------
{
    const a = pad('Pad A', 0);
    const b = pad('Pad B', 1);
    pads = [a, b, null, null];
    const sent: string[] = [];
    const input = new Input(element as HTMLElement, (msg) => sent.push(msg), false, 0, false, [3, 4]);
    input._gamepadConnected({ gamepad: a } as StandIn<GamepadEvent> as GamepadEvent);
    input.gamepadManager!._poll();
    check('each pad is announced on its own slot, pad 0 on slot 3 and pad 1 on slot 4',
          JSON.stringify(gamepadSends(sent)) === JSON.stringify(['js,c,2', 'js,c,3']), JSON.stringify(sent));
    sent.length = 0;
    press(b, 0, 1);
    press(a, 1, 1);
    input.gamepadManager!._poll();
    check('and each pad\'s presses go out on its slot',
          JSON.stringify(gamepadSends(sent)) === JSON.stringify(['js,b,2', 'js,b,3']), JSON.stringify(sent));
    sent.length = 0;
    input._gamepadHeartbeat();
    check('a held pad\'s heartbeat goes out on both slots',
          JSON.stringify(gamepadSends(sent, true)) === JSON.stringify(['js,h,2', 'js,h,3']), JSON.stringify(sent));
    sent.length = 0;
    input.rumble(3, 0.5, 0.5, 300);
    check('a rumble for slot 4 plays on pad 1 alone', JSON.stringify(plays) === JSON.stringify([['Pad B', 0.5]]),
          JSON.stringify(plays));
    plays.length = 0;
    input.rumble(0, 0.5, 0.5, 300);
    check('and one for a slot the page does not hold plays nowhere', plays.length === 0, JSON.stringify(plays));
    input.updateControllerSlot(4);
    check('re-slotted to 4 alone: slot 4 is released, and pad 0 moves from slot 3 to it',
          JSON.stringify(gamepadSends(sent)) === JSON.stringify(['js,d,3', 'js,d,2', 'js,c,3']), JSON.stringify(sent));
    sent.length = 0;
    input.updateControllerSlot([4, 1]);
    input.gamepadManager!._poll();
    check('given slot 1 beside it, pad 0 is announced on slot 4 again and pad 1 takes slot 1 at once',
          JSON.stringify(gamepadSends(sent).filter((m) => !m.startsWith('js,b'))) === JSON.stringify(['js,c,3', 'js,c,0']),
          JSON.stringify(sent));
    input.gamepadManager!.destroy();
}

// --- one slot, as before -----------------------------------------------------
{
    const a = pad('Pad A', 0);
    pads = [a, null, null, null];
    const sent: string[] = [];
    const input = new Input(element as HTMLElement, (msg) => sent.push(msg), false, 0, false, 2);
    input._gamepadConnected({ gamepad: a } as StandIn<GamepadEvent> as GamepadEvent);
    input.gamepadManager!._poll();
    press(a, 0, 1);
    input.gamepadManager!._poll();
    check('a page holding slot 2 sends its pad on index 1, as it always did',
          JSON.stringify(gamepadSends(sent)) === JSON.stringify(['js,c,1', 'js,b,1']), JSON.stringify(sent));
    sent.length = 0;
    plays.length = 0;
    input.rumble(1, 0.5, 0.5, 300);
    check('and its rumble plays on every pad it has', JSON.stringify(plays) === JSON.stringify([['Pad A', 0.5]]),
          JSON.stringify(plays));
    input.gamepadManager!.destroy();
}

process.exit(failed === 0 ? 0 : 1);
