/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// 7. Test the gamepad (first, the touch gamepad, and your real gamepad if you
// happen to have it), both on Firefox and Chrome (they each use different
// joypad interfaces) with https://hardwaretester.com/gamepad, or any other
// native client. You need to press all buttons and test four diagonal
// directions for each thumbstick.
//
// The real pad is a virtual one in the client page (the Gamepad API the client
// polls, tests/tools/pad_init.ts), and the touch gamepad is the dashboard's
// own, tapped and dragged with touches (CDP touch in Chrome, synthetic touches
// in WebKit; Firefox's persistent profile cannot emulate touch). Every button
// and four diagonals of each stick must reach a tester page in the session's
// own Chrome and Firefox, which read the pads through the session's joypad
// devices.
import type { BrowserContext, CDPSession, Page } from '@playwright/test';

import { fields } from '../../../web/selkies-web-core/lib/narrow.ts';
import { sleep } from '../../lib/env.ts';
import { padInitScript } from '../../lib/uinput.ts';
import { callJs, sessionBrowser, stopSessionBrowser, type Cell } from '../lib.ts';

export const ITEM = 7;
export const TITLE = 'gamepad: touch pad and virtual pad';
const DIAGONALS = [[1, 1], [1, -1], [-1, 1], [-1, -1]] as const;
const TOUCH_JS = `([type, x, y]) => {
  const el = document.elementFromPoint(x, y);
  if (!el) return false;
  const t = new Touch({identifier: 7, target: el, clientX: x, clientY: y, pageX: x, pageY: y});
  const list = type === 'touchend' ? [] : [t];
  el.dispatchEvent(new TouchEvent(type, {touches: list, targetTouches: list, changedTouches: [t],
                                          bubbles: true, cancelable: true}));
  return true;
}`;
const WANT_DIAGONALS = [0, 1].flatMap((s) => ['+', '-'].flatMap((a) => ['+', '-'].map((b) => `${s}:${a}${b}`)));

/** Every button index pressed and every stick diagonal seen, over all the session's pads. */
function union(report: Record<string, unknown> | null): [Set<number>, Set<string>] {
    const pressed = new Set<number>();
    const diag = new Set<string>();
    for (const pad of Object.values(report ?? {})) {
        const p = fields(pad);
        if (Array.isArray(p.pressed)) for (const i of p.pressed) pressed.add(Number(i));
        if (Array.isArray(p.diagonals)) for (const d of p.diagonals) diag.add(String(d));
    }
    return [pressed, diag];
}

async function driveVirtual(page: Page): Promise<void> {
    // A browser exposes a pad to pages only once it has seen input from it, so a
    // first press wakes the session's tester before the counted ones.
    await page.evaluate('window.__padPress(0, 1)');
    await sleep(300);
    await page.evaluate('window.__padPress(0, 0)');
    await sleep(1500);
    for (let i = 0; i < 17; i += 1) {
        await page.evaluate(`window.__padPress(${i}, 1)`);
        await sleep(300);
        await page.evaluate(`window.__padPress(${i}, 0)`);
        await sleep(150);
    }
    for (const stick of [0, 1]) {
        for (const [x, y] of DIAGONALS) {
            await page.evaluate(`window.__padAxis(${2 * stick}, ${0.95 * x}); window.__padAxis(${2 * stick + 1}, ${0.95 * y})`);
            await sleep(350);
            await page.evaluate(`window.__padAxis(${2 * stick}, 0); window.__padAxis(${2 * stick + 1}, 0)`);
            await sleep(200);
        }
    }
}

async function touch(page: Page, cdp: CDPSession | null, kind: string, x: number, y: number): Promise<void> {
    if (cdp !== null) {
        const type = kind === 'touchstart' ? 'touchStart' : kind === 'touchmove' ? 'touchMove' : 'touchEnd';
        await cdp.send('Input.dispatchTouchEvent', { type, touchPoints: kind === 'touchend' ? [] : [{ x, y, id: 7 }] });
    } else {
        await callJs(page, TOUCH_JS, [kind, x, y]);
    }
}

const rects = async (page: Page, selector: string): Promise<number[][]> => {
    const got: unknown = await page.evaluate(selector);
    return Array.isArray(got) ? got.map((r) => (Array.isArray(r) ? r.map(Number) : [])) : [];
};

/**
 * Tap every button of the touch gamepad, tap each stick (its click) and, where
 * touches can move, push it into its four diagonals; `[controls tapped, sticks]`.
 */
async function driveTouch(page: Page, cdp: CDPSession | null, moves: boolean): Promise<[number, number]> {
    const buttons = await rects(page, `[...document.querySelectorAll(
        '#universal-touch-gamepad-controls-overlay .touch-button')]
        .map(e => { const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; })`);
    // An analog trigger reads how far the finger is along it: swept end to end, so its value passes full.
    const triggers = await rects(page, `[...document.querySelectorAll(
        '#universal-touch-gamepad-controls-overlay .touch-analog-trigger')]
        .map(e => { const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + 2, r.y + r.height - 2]; })`);
    for (const [x, top, bottom] of triggers) {
        if (cdp === null) {
            await page.touchscreen.tap(x, (top + bottom) / 2);
            continue;
        }
        await touch(page, cdp, 'touchstart', x, top);
        for (const f of [0.25, 0.5, 0.75, 1.0]) {
            await sleep(60);
            await touch(page, cdp, 'touchmove', x, top + (bottom - top) * f);
        }
        await sleep(150);
        for (const f of [0.75, 0.5, 0.25, 0.0]) {
            await sleep(60);
            await touch(page, cdp, 'touchmove', x, top + (bottom - top) * f);
        }
        await touch(page, cdp, 'touchend', x, top);
        await sleep(200);
    }
    const sticks = await rects(page, `[...document.querySelectorAll(
        '#universal-touch-gamepad-controls-overlay .touch-joystick-base')]
        .map(e => { const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2, r.width / 2]; })`);
    // Buttons held as a finger does; a stick's click is a tap under its 250 ms threshold.
    const taps: [number, number, number][] = [...buttons.map((r): [number, number, number] => [r[0], r[1], 250]), ...sticks.map((r): [number, number, number] => [r[0], r[1], 100])];
    for (const [x, y, hold] of taps) {
        if (cdp === null) {
            await page.touchscreen.tap(x, y);
        } else {
            await touch(page, cdp, 'touchstart', x, y);
            await sleep(hold);
            await touch(page, cdp, 'touchend', x, y);
        }
        await sleep(200);
    }
    for (const [x, y, r] of moves ? sticks : []) {
        for (const [dx, dy] of DIAGONALS) {
            await touch(page, cdp, 'touchstart', x, y);
            await sleep(250);
            for (const step of [0.4, 0.8, 1.0]) {
                await touch(page, cdp, 'touchmove', x + dx * r * step, y + dy * r * step);
                await sleep(80);
            }
            await sleep(300);
            await touch(page, cdp, 'touchend', x + dx * r, y + dy * r);
            await sleep(200);
        }
    }
    return [buttons.length + triggers.length, sticks.length];
}

export async function run(cell: Cell): Promise<void> {
    const R = cell.res;
    for (const which of ['chrome', 'firefox']) {
        await cell.clearReport('gamepad');
        if (!await sessionBrowser(cell.target, { which, start: '/gamepad.html', kiosk: false, profile: 'pad' })) {
            R.skip(`a gamepad tester in the session's ${which}`, 'not installed in the image');
            continue;
        }
        await sleep(4000);
        // The virtual pad.
        let page = await cell.open('', { init: [padInitScript()] });
        await sleep(2000);
        await driveVirtual(page);
        await sleep(1500);
        let [pressed, diag] = union(await cell.report('gamepad', 10));
        const sorted = (s: Set<number> | Set<string>): (number | string)[] => [...s].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
        R.check(`every button of a virtual pad reaches the session's ${which}`, pressed.size >= 17, `${pressed.size} of 17: ${JSON.stringify(sorted(pressed))}`);
        R.check(`four diagonals of both sticks reach the session's ${which}`, WANT_DIAGONALS.every((d) => diag.has(d)), sorted(diag));
        await page.close();
        // The touch gamepad.
        if (cell.engine === 'firefox') {
            R.skip(`the touch gamepad into the session's ${which}`, "Firefox's persistent profile emulates no touch");
        } else {
            // A fresh tester: what the virtual pad pressed must not count for the touch pad.
            await cell.go('/gamepad.html', 'pad');
            await sleep(3000);
            await cell.clearReport('gamepad');
            let tctx: BrowserContext | null = null;
            const browser = cell.ctx?.browser();
            if (cell.engine === 'webkit' && browser) {
                tctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, hasTouch: true, ignoreHTTPSErrors: true });
            }
            page = await cell.open('', tctx ? { ctx: tctx } : {});
            let cdp: CDPSession | null = null;
            if (cell.engine === 'chromium' && cell.ctx) {
                cdp = await cell.ctx.newCDPSession(page);
                await cdp.send('Emulation.setTouchEmulationEnabled', { enabled: true, maxTouchPoints: 5 });
            }
            await page.evaluate(() => {
                window.postMessage({ type: 'TOUCH_GAMEPAD_SETUP', payload: { targetDivId: 'touch-gamepad-host', visible: true } }, window.location.origin);
            });
            await sleep(1500);
            const moves = cdp !== null;
            const [tapped, sticks] = await driveTouch(page, cdp, moves);
            await sleep(1500);
            [pressed, diag] = union(await cell.report('gamepad', 10));
            const detail = `${pressed.size} indices for ${tapped} buttons and ${sticks} sticks: ${JSON.stringify(sorted(pressed))}`;
            if (moves) {
                R.check(`every touch gamepad button and stick click reaches the session's ${which}`, tapped > 0 && pressed.size >= tapped + sticks, detail);
                R.check(`four diagonals of both touch sticks reach the session's ${which}`, WANT_DIAGONALS.every((d) => diag.has(d)), sorted(diag));
            } else {
                R.skip(`every touch gamepad button into the session's ${which}`,
                    `Playwright's WebKit taps last no time at all, shorter than the client's pad poll (${detail})`);
                R.skip(`touch stick diagonals into the session's ${which}`, "WebKit has no Touch constructor and Playwright's touchscreen only taps");
            }
            await page.close();
            await tctx?.close();
        }
        await stopSessionBrowser(cell.target, which, 'pad');
    }
}
