/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// How the client reads gamepads. The Gamepad API has no input event, so pads
// are polled, and every millisecond between polls is latency every press pays:
// the poller runs at the 4 ms Chromium samples pads at, and the on-screen
// touch gamepad, which knows when it changes, is read the moment it says so. A
// stick is one point, so its rest noise is cut by its distance from center:
// cutting each axis on its own pins the minor axis of a push near a cardinal
// direction to zero and then jumps it. Rumble the server relays plays on every
// pad that has a motor to play it with, and stops when the manager goes.
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.

let failed = 0;

function check(label, ok, detail = '') {
    if (!ok) failed++;
    console.log(`${ok ? 'PASS' : 'FAIL'}  [gamepad-poll] ${label}  ${detail}`);
}

const listeners = {};
globalThis.window = {
    addEventListener: (type, fn) => { (listeners[type] || (listeners[type] = [])).push(fn); },
    removeEventListener: (type, fn) => {
        listeners[type] = (listeners[type] || []).filter((f) => f !== fn);
    },
};
const intervals = [];
globalThis.setInterval = (fn, ms) => { intervals.push({ fn, ms }); return intervals.length; };
globalThis.clearInterval = () => {};

const pad = (mapping, axes, buttons = 17) => ({
    index: 0, id: 'Audit Pad (STANDARD GAMEPAD Vendor: 045e Product: 028e)', mapping, connected: true,
    axes, buttons: Array.from({ length: buttons }, () => ({ pressed: false, touched: false, value: 0 })),
});
let pads = [null, null, null, null];
navigator.getGamepads = () => pads;

const { GamepadManager } = await import('../../addons/selkies-web-core/lib/gamepad.js');

function makeManager() {
    const sent = [];
    const manager = new GamepadManager(null,
        (i, b, v) => sent.push(['b', b, v]),
        (i, a, v) => sent.push(['a', a, v]));
    return { manager, sent };
}

// --- the poll runs at the browser's own sampling ------------------------
{
    intervals.length = 0;
    makeManager();
    check('pads are polled every 4 ms', intervals.length === 1 && intervals[0].ms === 4,
          JSON.stringify(intervals.map((t) => t.ms)));
}

// --- the touch gamepad is read as it changes -----------------------------
{
    pads = [pad('standard', [0, 0, 0, 0]), null, null, null];
    const { manager, sent } = makeManager();
    manager._poll();
    sent.length = 0;
    pads[0].buttons[0] = { pressed: true, touched: true, value: 1 };
    for (const fn of listeners.touchgamepadinput || []) fn({ type: 'touchgamepadinput' });
    check('a touch gamepad change is sent as it is announced, before any tick',
          sent.some(([k, b, v]) => k === 'b' && b === 0 && v === 1), JSON.stringify(sent));
    const before = (listeners.touchgamepadinput || []).length;
    manager.destroy();
    const after = (listeners.touchgamepadinput || []).length;
    check('and a destroyed poller stops listening', after === before - 1, `${before} -> ${after} listeners`);
}

// --- a stick's rest is cut by its distance from center -------------------
{
    pads = [pad('standard', [0, 0, 0, 0]), null, null, null];
    const { manager, sent } = makeManager();
    manager._poll();
    const seq = [[0.04, 0.9], [0.049, 0.95], [0.06, 0.95]];
    const xs = [];
    for (const [x, y] of seq) {
        sent.length = 0;
        pads[0].axes = [x, y, 0, 0];
        manager._poll();
        const ax = sent.find(([k, a]) => k === 'a' && a === 0);
        xs.push(ax ? ax[2] : 'unchanged');
    }
    check('a push near a cardinal direction keeps its minor axis', xs[0] === 0.04 && xs[1] === 0.049 && xs[2] === 0.06,
          JSON.stringify(xs));
    sent.length = 0;
    pads[0].axes = [0.03, 0.03, 0, 0];
    manager._poll();
    const rest = sent.filter(([k]) => k === 'a');
    check('a stick within the deadzone of center reads as centered',
          rest.every(([, , v]) => v === 0) && rest.length === 2, JSON.stringify(rest));
}

// --- axes nobody pairs are cut one by one ------------------------------
{
    pads = [pad('', [0, 0, 0, 0, 0, 0]), null, null, null];
    const { manager, sent } = makeManager();
    manager._poll();
    sent.length = 0;
    pads[0].axes = [0.04, 0.9, 0, 0, 0, 0];
    manager._poll();
    const ax = sent.find(([k, a]) => k === 'a' && a === 0);
    check('an unmapped pad keeps the per-axis cut', ax === undefined, JSON.stringify(sent));
}

// --- rumble plays on every pad that can -------------------------------------
{
    const calls = [];
    const actuator = (name) => ({
        playEffect: (type, p) => { calls.push([name, 'play', type, p.strongMagnitude, p.weakMagnitude, p.duration]); return Promise.resolve('complete'); },
        reset: () => { calls.push([name, 'reset']); return Promise.resolve('complete'); },
    });
    const chromePad = pad('standard', [0, 0, 0, 0]);
    chromePad.vibrationActuator = actuator('chrome');
    const geckoPad = pad('standard', [0, 0, 0, 0]);
    geckoPad.hapticActuators = [{ pulse: (v, ms) => { calls.push(['gecko', 'pulse', v, ms]); return Promise.resolve(true); } }];
    const plainPad = pad('standard', [0, 0, 0, 0]);
    pads = [chromePad, geckoPad, plainPad, null];
    const { manager } = makeManager();
    manager.rumble(0.5, 0.25, 300);
    check('a rumble plays dual-rumble on a pad with a vibration actuator, and a pulse of the stronger level on Gecko\'s',
          JSON.stringify(calls) === JSON.stringify([['chrome', 'play', 'dual-rumble', 0.5, 0.25, 300], ['gecko', 'pulse', 0.5, 300]]),
          JSON.stringify(calls));
    calls.length = 0;
    manager.rumble(0, 0, 0);
    check('and a rumble of nothing stops both', JSON.stringify(calls) === JSON.stringify([['chrome', 'reset'], ['gecko', 'pulse', 0, 0]]),
          JSON.stringify(calls));
    calls.length = 0;
    manager.rumble(0, 0, 0);
    check('a stop with nothing playing is not sent', calls.length === 0, JSON.stringify(calls));
    manager.rumble(1, 1, 60000);
    check('an effect is held at most the 5 s the Gamepad API plays at once',
          calls.length === 2 && calls[0][5] === 5000 && calls[1][3] === 5000, JSON.stringify(calls));
    calls.length = 0;
    manager.rumble(0.5, 0.5, 300);
    manager.destroy();
    check('tearing the manager down stops what plays',
          calls.length === 4 && calls[2][1] === 'reset' && calls[3][2] === 0, JSON.stringify(calls));
}

process.exit(failed === 0 ? 0 : 1);
