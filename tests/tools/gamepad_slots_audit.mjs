/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What a page sends for its pads when the server's verdict names its slots:
// one slot as the number it always was, or a token's list of them, each pad
// on its own slot in the browser's order (pad 0 on slot 3, pad 1 on slot 4),
// a slot that goes released and the pads re-announced where the list moves,
// and a rumble played on the pad driving the slot it is for.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [gamepad-slots] ${label}  ${detail}`);
}

globalThis.setInterval = () => 0;
globalThis.clearInterval = () => {};
globalThis.setTimeout = () => 0;
globalThis.clearTimeout = () => {};
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
    postMessage() {},
    location: { origin: 'http://localhost' },
};
globalThis.document = {
    createElement: () => ({ style: {}, getContext: () => ({}) }),
    body: { appendChild() {} },
    getElementById: () => null,
    pointerLockElement: null,
    activeElement: null,
};
let pads = [null, null, null, null];
Object.defineProperty(globalThis, 'navigator', {
    value: { platform: 'Linux x86_64', userAgent: 'Mozilla/5.0 (X11; Linux x86_64) Chrome/154', maxTouchPoints: 0,
             getGamepads: () => pads },
    configurable: true, writable: true });

const { Input, slotList } = await import('../../addons/selkies-web-core/lib/input.js');

const plays = [];
const pad = (name, index) => ({
    index, id: `${name} (STANDARD GAMEPAD Vendor: 045e Product: 0b13)`, mapping: 'standard', connected: true,
    axes: [0, 0, 0, 0], buttons: Array.from({ length: 17 }, () => ({ pressed: false, touched: false, value: 0 })),
    vibrationActuator: {
        playEffect: (type, e) => { plays.push([name, e.strongMagnitude]); return Promise.resolve('complete'); },
        reset: () => { plays.push([name, 'reset']); return Promise.resolve('complete'); },
    },
});
const press = (p, b, v) => { p.buttons[b] = { pressed: v > 0, touched: v > 0, value: v }; };
// A held control's heartbeat (`js,h`) rides every poll that finds one held; the checks of it are their own.
const gamepadSends = (sent, beats = false) => sent.filter((m) => m.startsWith('js,') && (beats || !m.startsWith('js,h')))
    .map((m) => m.split(',').slice(0, 3).join(','));

check('a verdict names one slot as its number, several as their list, and none as null',
      JSON.stringify([slotList(3), slotList([3, 4]), slotList('2'), slotList(null), slotList(0), slotList([])])
      === JSON.stringify([[3], [3, 4], [2], null, null, null]));

// --- a token holding slots 3 and 4 -------------------------------------------
{
    const a = pad('Pad A', 0);
    const b = pad('Pad B', 1);
    pads = [a, b, null, null];
    const sent = [];
    const input = new Input(element, (msg) => sent.push(msg), false, 0, false, [3, 4]);
    input._gamepadConnected({ gamepad: a });
    input.gamepadManager._poll();
    check('each pad is announced on its own slot, pad 0 on slot 3 and pad 1 on slot 4',
          JSON.stringify(gamepadSends(sent)) === JSON.stringify(['js,c,2', 'js,c,3']), JSON.stringify(sent));
    sent.length = 0;
    press(b, 0, 1);
    press(a, 1, 1);
    input.gamepadManager._poll();
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
    input.gamepadManager._poll();
    check('given slot 1 beside it, pad 0 is announced on slot 4 again and pad 1 takes slot 1 at once',
          JSON.stringify(gamepadSends(sent).filter((m) => !m.startsWith('js,b'))) === JSON.stringify(['js,c,3', 'js,c,0']),
          JSON.stringify(sent));
    input.gamepadManager.destroy();
}

// --- one slot, as before -----------------------------------------------------
{
    const a = pad('Pad A', 0);
    pads = [a, null, null, null];
    const sent = [];
    const input = new Input(element, (msg) => sent.push(msg), false, 0, false, 2);
    input._gamepadConnected({ gamepad: a });
    input.gamepadManager._poll();
    press(a, 0, 1);
    input.gamepadManager._poll();
    check('a page holding slot 2 sends its pad on index 1, as it always did',
          JSON.stringify(gamepadSends(sent)) === JSON.stringify(['js,c,1', 'js,b,1']), JSON.stringify(sent));
    sent.length = 0;
    plays.length = 0;
    input.rumble(1, 0.5, 0.5, 300);
    check('and its rumble plays on every pad it has', JSON.stringify(plays) === JSON.stringify([['Pad A', 0.5]]),
          JSON.stringify(plays));
    input.gamepadManager.destroy();
}

process.exit(failed === 0 ? 0 : 1);
