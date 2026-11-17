/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// 3. Make sure the second display works properly by moving a window to the
// right. Then, toggle force aligned resolution, native cursor style, and
// antialiasing. Try changing resolutions (both preset and manual, UI scaling,
// and HiDPI (it's supposed to be disabled automatically when a resolution is
// set), and the reset button as well. See if the desktops or windows respond.
//
// A second page opens `#display2`; a window of the session's Chrome is moved to
// the right of the primary and has to show up in the second display's stream:
// with xdotool on X11; on Wayland by a KWin script where KWin composes (the
// window a native client with no X id), else with xdotool on a Chrome run on
// XWayland, which the compositor places as the client asks. The sidebar's
// screen controls are then driven one by one, each checked on the session (its
// xrandr geometry, its Xft.dpi, and the tester page's own size, which is what a
// maximized window does) and on the page (its stream, its canvas style).
import type { Page } from '@playwright/test';

import { numberOr } from '../../../web/selkies-web-core/lib/narrow.ts';
import { sleep } from '../../lib/env.ts';
import { openSidebar, reveal, sessionBrowser, stopSessionBrowser, type Cell, type Sample } from '../lib.ts';

export const ITEM = 3;
export const TITLE = 'second display, screen settings';
const SOLID = [255, 0, 255] as const;

type Size = readonly [number, number];

/**
 * Place the window whose caption starts with `caption` at (x, y), w x h,
 * through a KWin script (KWin 6's scripting over the session's D-Bus); what it printed.
 */
async function kwinMove(cell: Cell, caption: string, x: number, y: number, w: number, h: number): Promise<string> {
    const js = `for (const win of workspace.windowList()) if (win.caption.startsWith(${JSON.stringify(caption)})) `
        + `{ win.frameGeometry = {x: ${x}, y: ${y}, width: ${w}, height: ${h}}; print('moved ' + win.caption); }`;
    await cell.target.put('/tmp/it-move.js', Buffer.from(js));
    return cell.target.out('q=$(command -v qdbus6 || command -v qdbus); '
        + '$q org.kde.KWin /Scripting org.kde.kwin.Scripting.unloadScript it-move >/dev/null 2>&1; '
        + 'id=$($q org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript /tmp/it-move.js it-move) && '
        + '$q org.kde.KWin /Scripting/Script$id org.kde.kwin.Script.run; sleep 1; '
        + '$q org.kde.KWin /Scripting org.kde.kwin.Scripting.unloadScript it-move; echo "script $id"');
}

/** The session's monitors as xrandr reports them: name -> [w, h, x, y]. */
async function geometry(cell: Cell): Promise<Record<string, number[]>> {
    const out = await cell.target.out('xrandr --listactivemonitors 2>/dev/null');
    const mons: Record<string, number[]> = {};
    // XWayland under KWin reports no physical size (-1).
    for (const m of out.matchAll(/\d+: \+?\*?(\S+) (\d+)\/-?\d+x(\d+)\/-?\d+\+(\d+)\+(\d+)/g)) {
        mons[m[1]] = [2, 3, 4, 5].map((i) => Number(m[i]));
    }
    return mons;
}

async function screen(cell: Cell): Promise<Size | null> {
    const m = /current (\d+) x (\d+)/.exec(await cell.target.out('xrandr --current 2>/dev/null | head -1'));
    return m ? [Number(m[1]), Number(m[2])] : null;
}

async function xft(cell: Cell): Promise<string> {
    return (await cell.target.out("xrdb -query 2>/dev/null | awk '/Xft.dpi/ {print $2}'")) || '96';
}

async function wait<T>(fn: () => Promise<T>, want: (got: T) => boolean, timeoutS = 15): Promise<T> {
    const until = Date.now() + timeoutS * 1000;
    let got = await fn();
    while (Date.now() < until && !want(got)) {
        await sleep(700);
        got = await fn();
    }
    return got;
}

const same = (a: Size | null, b: Size): boolean => a !== null && a[0] === b[0] && a[1] === b[1];

/** A computed style of the stream's sink, or for the cursor of the input overlay over it. */
async function style(page: Page, prop: string): Promise<string> {
    return String(await page.evaluate((p) => {
        if (p === 'cursor') {
            const el = document.getElementById('overlayInput');
            return el ? getComputedStyle(el).cursor.slice(0, 40) : '';
        }
        const v = document.querySelector('video');
        const c = [...document.querySelectorAll('canvas')].find((x) => x.width >= 320);
        const el = v && v.videoWidth > 0 && v.style.display !== 'none' ? v : c;
        return el ? getComputedStyle(el).getPropertyValue(p.replace(/[A-Z]/g, (u) => `-${u.toLowerCase()}`)) : '';
    }, prop));
}

async function on(page: Page, sel: string): Promise<boolean | null> {
    const el = page.locator(sel);
    if (!await el.count()) return null;
    return ((await el.first().getAttribute('aria-pressed')) ?? (await el.first().getAttribute('aria-checked'))) === 'true';
}

async function clickToggle(page: Page, sel: string): Promise<boolean | null> {
    const el = page.locator(sel);
    if (!await reveal(page, sel)) return null;
    await el.first().click({ timeout: 5000 });
    await sleep(1500);
    return on(page, sel);
}

function near(rgb: readonly number[] | undefined, want: readonly number[]): boolean {
    return Array.isArray(rgb) && want.every((b, i) => Math.abs(rgb[i] - b) <= 30);
}

/** The tester window's size, once the pattern page reports `want`. */
async function windowAt(cell: Cell, want: Size): Promise<Size | null> {
    const rep = await cell.report('pattern', 10, (r) => r.w === want[0] && r.h === want[1]);
    return rep ? [numberOr(rep.w) ?? 0, numberOr(rep.h) ?? 0] : null;
}

export async function run(cell: Cell): Promise<void> {
    const R = cell.res;
    const page = await cell.open();
    const tools = Array.isArray(cell.facts.tools) ? cell.facts.tools.map(String) : [];
    if (cell.backend === 'wayland' && !tools.includes('xrandr')) R.skip('session geometry', 'no xrandr in the Wayland session');
    // The second display, and a window moved onto it.
    const second = await cell.open('#display2');
    const mons = await wait(() => geometry(cell), (m) => Object.keys(m).length >= 2, 20);
    const count = Object.keys(mons).length;
    R.check('#display2 adds a second monitor to the session', count >= 2, mons);
    const primary = Object.values(mons).sort((a, b) => a[2] - b[2])[0] ?? [1280, 720, 0, 0];
    const kwin = cell.backend === 'wayland' && tools.includes('kwin_wayland') && (tools.includes('qdbus6') || tools.includes('qdbus'));
    const onSecond = (): Promise<Sample | null> => wait(() => cell.sample(second, [[0.15, 0.3]]), (s) => s !== null && near(s.points[0], SOLID), 15);
    if (count >= 2 && kwin) {
        await sessionBrowser(cell.target, { start: '/solid.html', kiosk: false, profile: 'solid' });
        await sleep(5000);
        const x = primary[0] + 100;
        const said = await kwinMove(cell, 'it-solid', x, 100, 400, 300);
        await sleep(3000);
        const on2 = await onSecond();
        R.check('a window moved to the right of the primary shows on the second display',
            on2 !== null && near(on2.points[0], SOLID), { kwin: said, x, sample: on2?.points ?? null });
        await stopSessionBrowser(cell.target, 'chrome', 'solid');
    } else if (count >= 2 && tools.includes('xdotool')) {
        const xwayland = cell.backend === 'wayland' ? '--ozone-platform=x11' : '';
        await sessionBrowser(cell.target, { start: '/solid.html', kiosk: false, profile: 'solid', flags: xwayland });
        const wid = await wait(() => cell.target.out("xdotool search --name '^it-solid' 2>/dev/null | head -1"), Boolean, 20);
        if (wid) {
            const x = primary[0] + 100;
            await cell.target.sh(`xdotool windowsize ${wid} 400 300; xdotool windowmove ${wid} ${x} 100`);
            await sleep(3000);
            const on2 = await onSecond();
            R.check('a window moved to the right of the primary shows on the second display',
                on2 !== null && near(on2.points[0], SOLID), { window: wid, x, sample: on2?.points ?? null });
        } else {
            R.skip('a window moved onto the second display', `no X window of the session's Chrome on ${cell.backend} (no XWayland?)`);
        }
        await stopSessionBrowser(cell.target, 'chrome', 'solid');
    } else {
        R.skip('a window moved onto the second display', `needs a second monitor and xdotool, or KWin's scripting on Wayland (${JSON.stringify(mons)})`);
    }
    await second.close();

    await openSidebar(page);
    const base = await screen(cell);
    // Force aligned resolution: the desktop snaps to a multiple of 16.
    await page.setViewportSize({ width: 1277, height: 715 });
    await sleep(3000);
    const odd = await screen(cell);
    let toggled = await clickToggle(page, '#forceAlignedResolutionToggle');
    await page.setViewportSize({ width: 1275, height: 713 });
    const aligned = await wait(() => screen(cell), (g) => g !== null && g[0] % 16 === 0 && g[1] % 16 === 0, 15);
    R.check('force aligned resolution snaps the desktop to multiples of 16',
        toggled === true && aligned !== null && aligned[0] % 16 === 0 && aligned[1] % 16 === 0,
        `${JSON.stringify(odd)} -> ${JSON.stringify(aligned)}`);
    await clickToggle(page, '#forceAlignedResolutionToggle');
    await page.setViewportSize({ width: 1280, height: 720 });
    await sleep(2000);

    // Native cursor style and antialiasing: the stream element's own CSS follows each.
    let before = await style(page, 'cursor');
    toggled = await clickToggle(page, '#useBrowserCursorsToggle');
    await page.mouse.move(640, 360);
    await sleep(1500);
    let after = await style(page, 'cursor');
    R.check('native cursor style changes the cursor the page shows', toggled !== null && before !== after,
        `${JSON.stringify(before)} -> ${JSON.stringify(after)}`);
    await clickToggle(page, '#useBrowserCursorsToggle');

    // A preset resolution turns HiDPI off and sizes the desktop; the window follows.
    // Left as the dashboard derives it: a user's own pick of HiDPI outranks the derivation.
    const hidpi = await on(page, '#hidpiToggle');
    const presets = await reveal(page, '#resolutionPresetSelect')
        ? await page.locator('#resolutionPresetSelect option').evaluateAll((els) => els.map((e) => (e instanceof HTMLOptionElement ? e.value : '')))
        : [];
    const pick = presets.find((p) => p.startsWith('1600x900')) ?? presets.find((p) => p.includes('x'));
    if (pick) {
        await page.selectOption('#resolutionPresetSelect', pick, { timeout: 5000 });
        const [w, h] = pick.split('x').map(Number);
        const want: Size = [w, h];
        const got = await wait(() => screen(cell), (g) => same(g, want), 15);
        const win = await windowAt(cell, want);
        R.check(`the preset ${pick} sizes the desktop and the maximized window follows`, same(got, want) && win !== null,
            { screen: got, window: win });
        const now = await on(page, '#hidpiToggle');
        R.check('HiDPI turns itself off once a resolution is set', hidpi === null || !now,
            `hidpi before ${String(hidpi)}, after ${String(now)}`);
    } else {
        R.skip('preset resolution', `no presets offered (${JSON.stringify(presets.slice(0, 5))})`);
    }

    // Manual resolution.
    if (await reveal(page, '#manualWidthInput')) {
        // Typed as a user types (a scripted fill is not an edit React sees in every engine).
        for (const [sel, value] of [['#manualWidthInput', '1440'], ['#manualHeightInput', '810']]) {
            await page.click(sel, { timeout: 5000 });
            await page.keyboard.press('Control+a');
            await page.keyboard.type(value);
        }
        await page.locator('button:has-text("Set Manual Resolution")').first().click({ timeout: 5000 });
        const want: Size = [1440, 810];
        const got = await wait(() => screen(cell), (g) => same(g, want), 15);
        const win = await windowAt(cell, want);
        R.check('a manual 1440x810 sizes the desktop and the window follows', same(got, want) && win !== null, { screen: got, window: win });
        // Scaled to the page, the picture is smoothed only while antialiasing is on.
        before = await style(page, 'imageRendering');
        toggled = await clickToggle(page, '#antiAliasingToggle');
        after = await style(page, 'imageRendering');
        R.check('antialiasing changes how the scaled stream is drawn', toggled !== null && before !== after,
            `${JSON.stringify(before)} -> ${JSON.stringify(after)}`);
        await clickToggle(page, '#antiAliasingToggle');
    } else {
        R.skip('manual resolution', 'no manual width field');
    }

    // UI scaling sets the desktop's DPI.
    if (await reveal(page, '#uiScalingSelect')) {
        const values = await page.locator('#uiScalingSelect option').evaluateAll((els) => els.map((e) => (e instanceof HTMLOptionElement ? e.value : '')));
        const scale = values.includes('144') ? '144' : values.at(-1);
        if (scale) {
            await page.selectOption('#uiScalingSelect', scale, { timeout: 5000 });
            // X11 desktops read Xft.dpi; a Wayland one scales its output, which its apps see as their pixel ratio.
            const wantDpr = Number(scale) / 96;
            const reached = (v: [string, number]): boolean => v[0] === scale || Math.abs(v[1] - wantDpr) < 0.01;
            const got = await wait(async (): Promise<[string, number]> => [await xft(cell), numberOr((await cell.report('pattern', 1))?.dpr) ?? 0], reached, 15);
            R.check(`UI scaling ${scale} dpi reaches the desktop`, reached(got), `Xft.dpi ${got[0]}, the window's pixel ratio ${got[1]}`);
            await page.selectOption('#uiScalingSelect', values.includes('96') ? '96' : values[0], { timeout: 5000 });
        }
    } else {
        R.skip('UI scaling', 'no scaling menu');
    }

    // Reset: back to the window's own size.
    if (await reveal(page, 'button.reset-button')) {
        await page.locator('button.reset-button').first().click({ timeout: 5000 });
        const got = await wait(() => screen(cell), (g) => same(g, [1280, 720]), 15);
        R.check("reset gives the desktop the window's size again", same(got, [1280, 720]), `${JSON.stringify(got)} (was ${JSON.stringify(base)})`);
    } else {
        R.skip('reset', 'no reset button');
    }
    await cell.go('/pattern.html');
}
