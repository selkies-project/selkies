/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The scaling DPI the desktop ends up at must match what asked for it.
//
// Drives devicePixelRatio values through a real browser and reads back
// Xft.dpi, what the session's toolkits scale from. A density the stops do not
// name (a 3.5x phone) has to land on the nearest stop.
//
// A resolution the operator sets answers the question instead of the display:
// that framebuffer decides how large the desktop draws its UI, and the screen
// showing it says nothing about it. So the next blocks drive manual
// resolutions at dpr 1, where the density would ask for 96 whatever the size,
// and pin that a stored pick outranks both.
//
// Under CSS scaling with an automatic resolution a pick divides the
// resolution the page asks for instead, so the desktop has to stay at 96: the
// last block loads and reloads a 2x page that stored HiDPI off and 200%, on
// both transports, and the first settings it sends must not scale the desktop
// a second time.
import { spawnSync } from 'node:child_process';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { DASHBOARD_DIST, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

// The scaling_dpi stops the clients offer, in 25% steps.
const STOPS = [96, 120, 144, 168, 192, 216, 240, 264, 288];
const DPRS = [1.0, 1.25, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0];
// Manual resolutions and the pick each derives: the shorter side against the
// 1080 rows 96 DPI is for, snapped to a stop.
const MANUAL_DPIS: readonly (readonly [number, number, number])[] = [[3840, 2160, 192], [2560, 1440, 120], [1280, 720, 96]];
// A pick no derivation returns, so a desktop sitting at it can only have taken the stored one.
const PINNED_DPI = 168;

// A stored pick, written before the core reads its settings.
const PINNED_INIT = `(() => {
  const prefix = (window.location.origin + window.location.pathname)
    .replace(/[^a-zA-Z0-9._-]/g, '_');
  localStorage.setItem(prefix + '_scaling_dpi', '${PINNED_DPI}');
})()`;

// HiDPI off and 200% stored as a page that picked them leaves them, before the
// core reads its settings: the choice marked explicit, so the server's default
// does not resolve over it.
const CSS_SCALED_INIT = `(() => {
  const prefix = (window.location.origin + window.location.pathname)
    .replace(/[^a-zA-Z0-9._-]/g, '_');
  localStorage.setItem(prefix + '_useCssScaling', 'true');
  localStorage.setItem(prefix + '_useCssScaling_explicit_choice', 'true');
  localStorage.setItem(prefix + '_scaling_dpi', '192');
})()`;

// A server line applying 192 to the desktop: the WebSockets server's change
// line, or either transport's line for a density it wrote.
const APPLIED_192 = /(?:DPI changed from \S+ to 192\b|Successfully set DPI to 192\b)/;

/** The scaling stop nearest to a devicePixelRatio's implied DPI, quantized to quarter steps. */
function expected(dpr: number): number {
    const implied = Math.round(dpr * 4) * 24;
    return STOPS.reduce((best, stop) => (Math.abs(stop - implied) < Math.abs(best - implied) ? stop : best));
}

const display = (): string => process.env.E2E_DISPLAY ?? '';

/** Xft.dpi from the running resource database, or 96 when unset: X's own default. */
function xftDpi(): number {
    const out = spawnSync('xrdb', ['-query'], { encoding: 'utf8', env: { ...process.env, DISPLAY: display() } }).stdout ?? '';
    const line = out.split('\n').find((l) => l.startsWith('Xft.dpi'));
    return line ? parseInt(line.split(':')[1].trim(), 10) : 96;
}

async function open(browser: Browser, url: string, dpr: number, init = ''): Promise<[BrowserContext, Page]> {
    const ctx = await browser.newContext({ viewport: { width: 1000, height: 700 }, deviceScaleFactor: dpr });
    if (init) await ctx.addInitScript({ content: init });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load', timeout: 60_000 });
    await page.waitForTimeout(9000);
    return [ctx, page];
}

/**
 * Whether a page streams on its transport: one that never connected sent no
 * settings, so the desktop's density says nothing about it.
 */
const streams = async (page: Page, mode: string): Promise<boolean> =>
    (mode === 'webrtc' ? await waitWrVideo(page, 60) : await waitWsVideo(page, 45)) !== null;

const post = (page: Page, message: Record<string, unknown>): Promise<void> => within(page.evaluate((m) => { window.postMessage(m, window.location.origin); }, message));

test('dpi_accuracy', async () => {
    const res = new Checks('dpi-accuracy');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        const { url } = await host.start({ label: 'dpi', mode: 'websockets', env: { SELKIES_WEB_ROOT: DASHBOARD_DIST } });
        spawnSync('xrdb', ['-remove'], { env: { ...process.env, DISPLAY: display() } });
        const browser = await launchChromium();
        try {
            for (const dpr of DPRS) {
                const [ctx] = await open(browser, url, dpr);
                const got = xftDpi();
                const want = expected(dpr);
                res.check(`dpr ${dpr % 1 ? dpr : dpr.toFixed(1)} applies Xft.dpi ${want}`, got === want, `got ${got}`);
                await ctx.close();
            }

            // A live density change (the window dragged to another monitor, an
            // OS scaling change) must follow without a reload: the DPR watcher
            // re-derives the automatic scaling_dpi and pushes it.
            let [ctx, page] = await open(browser, url, 1.0);
            res.check('live dpr: starts at Xft.dpi 96', xftDpi() === 96, `got ${xftDpi()}`);
            const cdp = await ctx.newCDPSession(page);
            await cdp.send('Emulation.setDeviceMetricsOverride', { width: 1000, height: 700, deviceScaleFactor: 2.0, mobile: false });
            await page.waitForTimeout(9000);
            res.check('live dpr: 2.0 applies Xft.dpi 192 without reload', xftDpi() === 192, `got ${xftDpi()}`);
            await ctx.close();

            // A manual resolution on the automatic default: the framebuffer
            // asked for is what the desktop's UI is sized from, so the pick
            // follows it and not the dpr-1 screen showing it.
            [ctx, page] = await open(browser, url, 1.0);
            for (const [width, height, want] of MANUAL_DPIS) {
                await post(page, { type: 'setManualResolution', width, height });
                await page.waitForTimeout(9000);
                res.check(`a manual ${width}x${height} applies Xft.dpi ${want}`, xftDpi() === want, `got ${xftDpi()}`);
            }
            await post(page, { type: 'resetResolutionToWindow' });
            await page.waitForTimeout(9000);
            res.check('back on the window size the dpr answers again', xftDpi() === 96, `got ${xftDpi()}`);
            await ctx.close();

            // The derived value is a default: a stored pick governs whatever
            // the resolution is, which is what makes it configurable.
            [ctx, page] = await open(browser, url, 1.0, PINNED_INIT);
            res.check(`a stored pick applies Xft.dpi ${PINNED_DPI}`, xftDpi() === PINNED_DPI, `got ${xftDpi()}`);
            await post(page, { type: 'setManualResolution', width: 3840, height: 2160 });
            await page.waitForTimeout(9000);
            res.check('and a manual resolution does not re-derive over it', xftDpi() === PINNED_DPI, `got ${xftDpi()}`);
            await ctx.close();

            // CSS scaling with an automatic resolution: the stored 200% divides
            // the resolution the page asks for, so the desktop is asked for 96
            // from the first settings on, a reload included.
            for (const mode of ['websockets', 'webrtc']) {
                const started = mode === 'websockets' ? url
                    : (await host.start({ label: 'dpi-webrtc', mode, env: { SELKIES_WEB_ROOT: DASHBOARD_DIST } })).url;
                const mark = await host.mark();
                [ctx, page] = await open(browser, started, 2.0, `window.__SELKIES_STREAMING_MODE__ = '${mode}';\n${CSS_SCALED_INIT}`);
                res.check(`${mode}: a 2x page at 200% under CSS scaling streams`, await streams(page, mode), '');
                res.check(`${mode}: and keeps Xft.dpi 96`, xftDpi() === 96, `got ${xftDpi()}`);
                await page.reload({ waitUntil: 'load', timeout: 60_000 });
                await page.waitForTimeout(9000);
                res.check(`${mode}: reloaded, it streams again`, await streams(page, mode), '');
                res.check(`${mode}: and still keeps Xft.dpi 96`, xftDpi() === 96, `got ${xftDpi()}`);
                const applied = (await host.log(mark)).split('\n').filter((l) => APPLIED_192.test(l));
                res.check(`${mode}: no settings it sent applied 192`, applied.length === 0, applied.join(' | '));
                await ctx.close();
            }
        } finally {
            await browser.close();
        }
    } finally {
        await host.close();
    }
    res.finish();
});
