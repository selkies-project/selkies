/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Kernel gamepad end-to-end: a real browser client with a synthetic Gamepad
// API drives selkies over each transport; the /dev/uinput emulator records
// what the kernel would receive. A #player2 client repeats it on the sharing
// path, where the slot comes from the connection rather than the message. A
// client with two local pads, a flight stick whose pots jitter beside the pad
// in use, drives its one slot with the pad it takes up, with and without a
// token that binds it to a slot. A token holding slots 3 and 4 has the page's
// two pads drive one each.
import { readFileSync } from 'node:fs';

import type { Browser, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { WIRE_TAP_JS } from '../lib/page.ts';
import { realConsole, waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';
import { created, decode, has, shimEnv, STANDARD_XPAD, type InputEvent } from '../lib/uinput.ts';

// Linux input event codes (linux/input-event-codes.h).
const EV_SYN = 0x00;
const EV_KEY = 0x01;
const EV_ABS = 0x03;
const BTN_A = 0x130;
const BTN_B = 0x131;
const ABS_X = 0x00;
const ABS_Z = 0x02;
const ABS_HAT0Y = 0x11;

const PAD_INIT = `
window.__pad = {
  index: 0, id: "Selkies Test Pad (STANDARD GAMEPAD Vendor: 045e Product: 028e)",
  mapping: "standard", connected: true, timestamp: 1,
  buttons: Array.from({length: 17}, () => ({pressed: false, touched: false, value: 0})),
  axes: [0, 0, 0, 0],
};
navigator.getGamepads = () => [window.__pad, null, null, null];
window.__padPress = (i, v) => {
  window.__pad.buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
  window.__pad.timestamp = performance.now();
};
window.__padAxis = (i, v) => {
  window.__pad.axes[i] = v;
  window.__pad.timestamp = performance.now();
};
`;

const TWO_PADS_INIT = `
(() => {
  const blank = (n) => Array.from({length: n}, () => ({pressed: false, touched: false, value: 0}));
  window.__pads = [
    {index: 0, id: "Logitech Extreme 3D (Vendor: 046d Product: c215)", mapping: "", connected: true,
     timestamp: 1, buttons: blank(12), axes: [0.07, -0.06, 0, 0, 0, 0, -1, 0, 0, 1.2857]},
    {index: 1, id: "Xbox Controller (STANDARD GAMEPAD Vendor: 045e Product: 0b13)", mapping: "standard",
     connected: true, timestamp: 1, buttons: blank(17), axes: [0, 0, 0, 0]},
  ];
  navigator.getGamepads = () => [window.__pads[0], window.__pads[1], null, null];
  let t = 0;
  setInterval(() => {
    window.__pads[0].axes[0] = (t++ % 2) ? 0.09 : 0.06;
    window.__pads[0].timestamp = performance.now();
  }, 4);
  window.__padPress = (p, i, v) => {
    window.__pads[p].buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
    window.__pads[p].timestamp = performance.now();
  };
  window.__padAxis = (p, i, v) => {
    window.__pads[p].axes[i] = v;
    window.__pads[p].timestamp = performance.now();
  };
})();
`;
const SLOTS_PADS_INIT = `
(() => {
  const blank = (n) => Array.from({length: n}, () => ({pressed: false, touched: false, value: 0}));
  const named = (index, name) => ({index, id: name + " (STANDARD GAMEPAD Vendor: 045e Product: 0b13)",
    mapping: "standard", connected: true, timestamp: 1, buttons: blank(17), axes: [0, 0, 0, 0]});
  window.__pads = [named(0, "Pad A"), named(1, "Pad B")];
  navigator.getGamepads = () => [window.__pads[0], window.__pads[1], null, null];
  window.__padPress = (p, i, v) => {
    window.__pads[p].buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
    window.__pads[p].timestamp = performance.now();
  };
})();
`;
const MASTER = 'e2e-gamepad-master';
const SLOT_TOKEN = 'e2e-gamepad-slot1-Rk7';
const SLOTS_TOKEN = 'e2e-gamepad-slots34-Qm2';
// The stick's jitter, 0.06 to 0.09 of full scale on the kernel device's axis.
const jitter = (v: number): boolean => Math.abs(v) >= 1500 && Math.abs(v) < 3500;

declare global {
    interface Window {
        __wireSent?: unknown[];
    }
}

/** Chromium with the synthetic pad injected, on the stream page. */
async function launch(url: string, mode: string, fragment = '', init = PAD_INIT): Promise<[Browser, Page, string[]]> {
    const browser = await launchChromium();
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: WIRE_TAP_JS });
    await ctx.addInitScript({ content: init });
    const page = await ctx.newPage();
    const errors: string[] = [];
    page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
    page.on('pageerror', (e) => errors.push(String(e)));
    await page.goto(`${url}/${fragment}`, { waitUntil: 'load' });
    return [browser, page, errors];
}

const video = async (page: Page, mode: string): Promise<unknown> => (mode === 'webrtc' ? waitWrVideo(page) : waitWsVideo(page));
const act = async (page: Page, action: string): Promise<void> => { await within(page.evaluate(`window.${action}`)); };

/** Pad input over one transport, and the kernel-side record. */
async function run(host: Host, mode: string, res: Checks): Promise<void> {
    const shim = shimEnv(`e2e-${mode}`);
    const { url } = await host.start({ label: mode, mode, env: shim.env });
    try {
        const [browser, page, errors] = await launch(url, mode);
        const v = await video(page, mode);
        res.check(`${mode}: video flowing`, Boolean(v), JSON.stringify(v));
        for (const action of ['__padPress(0, 1)', '__padPress(0, 0)', '__padPress(12, 1)', '__padPress(12, 0)', '__padAxis(0, -1)', '__padPress(6, 1)']) {
            await act(page, action);
            await sleep(250);
        }
        await sleep(500);
        const real = realConsole(errors).errors;
        res.check(`${mode}: console clean`, real.length === 0, JSON.stringify(real.slice(0, 2)));
        await browser.close();
    } finally {
        await host.stop();
    }
    const events = decode(shim.stream);
    const log = readFileSync(shim.log, 'utf8');
    res.check(`${mode}: kernel device created`, created(shim.log, STANDARD_XPAD) === 1);
    res.check(`${mode}: device is the standard pad`, log.includes('vendor=0x045e product=0x028e') && log.includes('Microsoft X-Box 360 pad'));
    for (const [name, event] of [['A press', [EV_KEY, BTN_A, 1]], ['A release', [EV_KEY, BTN_A, 0]], ['dpad up', [EV_ABS, ABS_HAT0Y, -1]],
        ['dpad release', [EV_ABS, ABS_HAT0Y, 0]], ['stick left', [EV_ABS, ABS_X, -32767]], ['left trigger', [EV_ABS, ABS_Z, 32767]]] as const) {
        res.check(`${mode}: ${name} reached the kernel device`, has(events, event));
    }
    process.stdout.write(`    ${mode} events: ${JSON.stringify(events.filter((e) => e[0] !== 4))}\n`);
    let synced = events.length % 2 === 0;
    for (let i = 0; i + 1 < events.length; i += 2) if (!(events[i + 1][0] === EV_SYN && events[i + 1][1] === 0 && events[i + 1][2] === 0)) synced = false;
    res.check(`${mode}: every event is framed by SYN_REPORT`, synced, `${events.length} events`);
}

/**
 * A #player2 link drives player 2's pad and no other. The slot a client may
 * drive is the one its own connection carries, which each transport learns
 * differently: the websockets handshake reads it off the query, and the
 * signaling HELLO carries it to the WebRTC gate. Driving a real pad through to
 * the kernel device is what proves that path end to end.
 */
async function runPlayerSlot(host: Host, mode: string, res: Checks): Promise<void> {
    const shim = shimEnv(`e2e-player2-${mode}`);
    const { url } = await host.start({ label: `player2-${mode}`, mode, env: shim.env });
    let serverLog: string;
    let sent: string[];
    try {
        const [browser, page] = await launch(url, mode, '#player2');
        const v = await video(page, mode);
        res.check(`${mode}: player-2 video flowing`, Boolean(v), JSON.stringify(v));
        for (const action of ['__padPress(0, 1)', '__padPress(0, 0)']) {
            await act(page, action);
            await sleep(250);
        }
        await sleep(500);
        // A pad announced before the server has registered this link's slot
        // is dropped by the slot gate, which knows of no slot to allow yet, and
        // the button sends that follow still route by index -- so the pad
        // drives its slot with no association recorded. The client's own
        // repair is to announce again, which is what a re-attach does.
        if (!(await host.log()).includes('virtual gamepad slot')) {
            await within(page.evaluate('window.webrtcInput && window.webrtcInput.resyncGamepads && window.webrtcInput.resyncGamepads()'));
            for (let i = 0; i < 20 && !(await host.log()).includes('virtual gamepad slot'); i++) await sleep(250);
        }
        // What the page sent for its pad, so a missing association on the
        // server can be told from an announcement the client never made.
        sent = await within(page.evaluate(() => (window.__wireSent ?? []).filter((m): m is string => typeof m === 'string' && m.startsWith('js,'))));
        await browser.close();
    } finally {
        serverLog = await host.log();
        await host.stop();
    }
    const events = decode(shim.stream);
    res.check(`${mode}: a player-2 link is given slot 1`, serverLog.includes('virtual gamepad slot 1') && !serverLog.includes('virtual gamepad slot 0'), `client sent ${JSON.stringify(sent.slice(0, 3))}`);
    res.check(`${mode}: its pad reaches the kernel device`, has(events, [EV_KEY, BTN_A, 1]), `${events.length} events`);
    res.check(`${mode}: no other slot was driven`, created(shim.log, STANDARD_XPAD) === 1);
}

/** Provision a controller token bound to `slot`; the HTTP status. */
async function postSlotToken(url: string, token = SLOT_TOKEN, slot: number | number[] = 1): Promise<number> {
    const r = await fetch(`${url}/api/tokens`, { method: 'POST', headers: { Authorization: `Bearer ${MASTER}`, 'Content-Type': 'application/json' },
        body: JSON.stringify({ [token]: { role: 'controller', slot } }) });
    return r.status;
}

/**
 * Two local pads drive the client's one slot with the pad taken up. A flight
 * stick at the first index jitters past the stick deadzone, parks its throttle
 * at an end and rests its hat off-center, beside a standard pad the user
 * presses. The pad in use must drive the slot alone: its press and its held
 * stick reach the kernel device, and nothing of the stick does. With `token`,
 * the page holds a controller token bound to slot 1, so the server gives it
 * the slot only with its verdict, after the page first saw its pads.
 */
async function runTwoPads(host: Host, mode: string, res: Checks, token: boolean): Promise<void> {
    const label = `${mode}${token ? ' with a slot token' : ''}`;
    const shim = shimEnv(`e2e-two-${mode}-${token ? 1 : 0}`);
    const { url } = await host.start({ label: `two-${mode}-${token ? 1 : 0}`, mode, env: { ...shim.env, ...(token ? { SELKIES_MASTER_TOKEN: MASTER } : {}) } });
    let serverLog: string;
    let held: InputEvent[];
    try {
        if (token) res.check(`${label}: token provisioned`, (await postSlotToken(url)) === 200);
        const [browser, page] = await launch(url, mode, token ? `#token=${SLOT_TOKEN}` : '', TWO_PADS_INIT);
        const v = await video(page, mode);
        res.check(`${label}: video flowing`, Boolean(v), JSON.stringify(v));
        await sleep(1000);
        for (const action of ['__padPress(1, 0, 1)', '__padPress(1, 0, 0)']) {
            await act(page, action);
            await sleep(250);
        }
        const mark = decode(shim.stream).length;
        await act(page, '__padAxis(1, 0, -1)');
        await sleep(1000);
        held = decode(shim.stream).slice(mark);
        await act(page, '__padAxis(1, 0, 0)');
        await sleep(500);
        await browser.close();
    } finally {
        serverLog = await host.log();
        await host.stop();
    }
    const events = decode(shim.stream);
    const xs = held.filter(([t, c]) => t === EV_ABS && c === ABS_X).map(([, , v]) => v);
    res.check(`${label}: the pad in use is the slot's controller`, serverLog.includes("'Xbox Controller (STANDARD GAMEPAD Vendor: 045e Product: 0b13)' (17b, 4a) is now associated with persistent virtual gamepad slot 0"));
    res.check(`${label}: its press reached the kernel device`, has(events, [EV_KEY, BTN_A, 1]) && has(events, [EV_KEY, BTN_A, 0]));
    res.check(`${label}: its stick stayed where it was held`, xs.length > 0 && xs.every((v) => v === -32767), `ABS_X while held: ${JSON.stringify(xs.slice(0, 6))}${xs.length > 6 ? '...' : ''} (${xs.length})`);
    const jittered = events.filter(([t, , v]) => t === EV_ABS && jitter(v)).length;
    res.check(`${label}: nothing of the flight stick reached it`, jittered === 0, `${jittered} jitter events`);
}

/**
 * A token holding slots 3 and 4: the page's pad 0 drives slot 3 and its pad 1
 * slot 4. The token table takes a list where it took a number, and the
 * verdict hands the page that list; the page announces each of its pads on its
 * own slot, in the browser's order, and the gate lets both through.
 */
async function runSlotList(host: Host, mode: string, res: Checks): Promise<void> {
    const label = `${mode} with slots 3 and 4`;
    const shim = shimEnv(`e2e-slots-${mode}`);
    const { url } = await host.start({ label: `slots-${mode}`, mode, env: { ...shim.env, SELKIES_MASTER_TOKEN: MASTER } });
    let serverLog: string;
    try {
        res.check(`${label}: token provisioned`, (await postSlotToken(url, SLOTS_TOKEN, [3, 4])) === 200);
        const [browser, page] = await launch(url, mode, `#token=${SLOTS_TOKEN}`, SLOTS_PADS_INIT);
        const v = await video(page, mode);
        res.check(`${label}: video flowing`, Boolean(v), JSON.stringify(v));
        await sleep(1000);
        for (const action of ['__padPress(0, 0, 1)', '__padPress(0, 0, 0)', '__padPress(1, 1, 1)', '__padPress(1, 1, 0)']) {
            await act(page, action);
            await sleep(250);
        }
        // The table takes slot 3 back while pad 0 holds A on it. The page's
        // own release of a slot no longer its is refused, so the server lets
        // go, at once rather than when the slot's heartbeat lapses (2 s).
        await act(page, '__padPress(0, 0, 1)');
        await sleep(500);
        const heldAt = decode(shim.stream).length;
        const until = Date.now() + 1000;
        res.check(`${label}: table re-slotted to 4 alone`, (await postSlotToken(url, SLOTS_TOKEN, [4])) === 200);
        const released = (): boolean => has(decode(shim.stream).slice(heldAt), [EV_KEY, BTN_A, 0]);
        while (Date.now() < until && !released()) await sleep(50);
        res.check(`${label}: taking slot 3 away releases what its pad held there within 1 s`, released());
        res.check(`${label}: and lets the pad go from it`, await host.saw("'Pad A (STANDARD GAMEPAD Vendor: 045e Product: 0b13)' disassociated from persistent virtual gamepad slot 2.", 5));
        await act(page, '__padPress(0, 0, 0)');
        await sleep(250);
        await browser.close();
    } finally {
        serverLog = await host.log();
        await host.stop();
    }
    const events = decode(shim.stream);
    const associated = (name: string, slot: number): boolean => serverLog.includes(`'${name} (STANDARD GAMEPAD Vendor: 045e Product: 0b13)' (17b, 4a) is now associated with persistent virtual gamepad slot ${slot}`);
    res.check(`${label}: pad 0 drives slot 3 and pad 1 slot 4`, associated('Pad A', 2) && associated('Pad B', 3));
    res.check(`${label}: no other slot was driven`, ![0, 1].some((i) => serverLog.includes(`virtual gamepad slot ${i}.`)));
    res.check(`${label}: both pads' presses reached the kernel`, has(events, [EV_KEY, BTN_A, 1]) && has(events, [EV_KEY, BTN_B, 1]));
}

test('gamepad_uinput', async () => {
    const res = new Checks('uinput');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        for (const mode of ['websockets', 'webrtc']) {
            await run(host, mode, res);
            await runPlayerSlot(host, mode, res);
            await runTwoPads(host, mode, res, false);
            await runTwoPads(host, mode, res, true);
            await runSlotList(host, mode, res);
        }
    } finally {
        await host.close();
    }
    res.finish();
});
