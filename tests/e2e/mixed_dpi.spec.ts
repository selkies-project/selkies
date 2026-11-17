/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Two displays on screens of different pixel density, on X11.
//
// The desktop keeps one DPI, the primary's, so a secondary streams no denser
// than the primary's published scale: a denser screen asks for the primary's
// density and the browser stretches the stream, which keeps the UI one size
// across the screens, while a less dense screen keeps its own density and
// shows the UI larger, since the primary's would ask for a buffer beyond its
// pixels. Checked both ways round (a denser primary, then a denser
// secondary), on both transports: the monitor the server realizes for the
// secondary, the secondary's buffer against its CSS box, the scale it
// publishes with the layout, and where a pointer on it lands.
//
// Uses E2E_DISPLAY when set; otherwise starts a throwaway Xvfb wide enough
// for the two-display union.
import { spawnSync } from 'node:child_process';

import type { Browser, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { WIRE_TAP_JS } from '../lib/page.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const PRIMARY_CSS = [1512, 806] as const;
const SECONDARY_CSS = [1920, 936] as const;

// A window this page can report a desktop position for, since the real one
// cannot be moved: the pages sit side by side with no chrome above them.
const desktopInit = (x: number): string => `(() => {
  window.__desk = [${x}, 0];
  window.__inset = [0, 0];
  Object.defineProperty(window, 'screenX', {get: () => window.__desk[0]});
  Object.defineProperty(window, 'screenY', {get: () => 0});
})()`;
const LAYOUT_JS = 'window.webrtcInput && window.webrtcInput._layout';
const SINK_JS = `(() => {
  const canvas = document.getElementById('videoCanvas');
  const video = document.getElementById('stream');
  const buffer = (canvas && canvas.width > 0) ? [canvas.width, canvas.height]
                 : (video && video.videoWidth > 0) ? [video.videoWidth, video.videoHeight] : null;
  if (!buffer) return null;
  // Whichever sink is drawn: the canvas, the worker's canvas, or the video.
  let css = [0, 0];
  for (const id of ['videoCanvas', 'videoWorkerCanvas', 'videoStream', 'stream']) {
    const el = document.getElementById(id);
    const r = el && el.getBoundingClientRect ? el.getBoundingClientRect() : null;
    if (r && r.width > 0) { css = [Math.round(r.width), Math.round(r.height)]; break; }
  }
  return {w: buffer[0], h: buffer[1], cssW: css[0], cssH: css[1],
          density: window.webrtcInput ? window.webrtcInput._cursorDensity() : null};
})()`;

type Rec = Record<string, unknown>;
const rec = (v: unknown): Rec => (v !== null && typeof v === 'object' && !Array.isArray(v) ? { ...v } : {});
const r = (v: unknown): string => JSON.stringify(v);
type Monitor = [number, number, number, number];

async function waitFor(pred: () => boolean | Promise<boolean>, timeoutS = 15): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (await pred()) return true;
        await sleep(250);
    }
    return false;
}

/**
 * The RandR monitors the server publishes, by role: "primary" is the one
 * flagged primary and "display2" the other, whether they are the outputs the
 * displays hold or the logical monitors a server without such outputs gets;
 * each maps to (w, h, x, y).
 */
function monitors(display: string): Record<string, Monitor> {
    const out = spawnSync('xrandr', ['--display', display, '--listmonitors'], { encoding: 'utf8' }).stdout ?? '';
    const found: Record<string, Monitor> = {};
    for (const line of out.split('\n')) {
        const parts = line.trim().split(/\s+/);
        if (parts.length < 3 || !/^\d+$/.test(parts[0].replace(/:$/, ''))) continue;
        const role = parts[1].includes('*') ? 'primary' : 'display2';
        const [size, x, y] = parts[2].split('+');
        const [w, h] = size.split('x').map((v) => parseInt(v.split('/')[0], 10));
        found[role] ??= [w, h, parseInt(x, 10), parseInt(y, 10)];
    }
    return found;
}

interface DisplayPage {
    page: Page;
    errors: string[];
}

async function newDisplayContext(browser: Browser, url: string, mode: string, css: readonly [number, number], dpr: number, deskX: number, hash = ''): Promise<DisplayPage> {
    const ctx = await browser.newContext({ viewport: { width: css[0], height: css[1] }, deviceScaleFactor: dpr });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: WIRE_TAP_JS });
    await ctx.addInitScript({ content: desktopInit(deskX) });
    const page = await ctx.newPage();
    const errors: string[] = [];
    page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
    await page.goto(`${url}/${hash}`, { waitUntil: 'load' });
    return { page, errors };
}

const waitVideo = (page: Page, mode: string, timeoutS = 45): Promise<unknown> => (mode === 'webrtc' ? waitWrVideo(page, timeoutS) : waitWsVideo(page, timeoutS));
const layout = async (page: Page): Promise<Rec> => rec(await within(page.evaluate(LAYOUT_JS)));

async function secondaryRect(page: Page): Promise<Rec> {
    const rects = (await layout(page)).rects;
    return (Array.isArray(rects) ? rects.map(rec) : []).find((x) => Number(x.x) > 0) ?? {};
}

/** The page's own rectangle in the layout, carrying the scale it published. */
const ownRect = async (page: Page): Promise<Rec> => rec((await layout(page)).own);

/** The display scales this page has reported to the server, in order. */
async function published(page: Page): Promise<unknown[]> {
    const sent = await within(page.evaluate("(window.__wireSent || []).filter(d => typeof d === 'string' && d.startsWith('SETTINGS,'))"));
    return (Array.isArray(sent) ? sent : []).map((m): unknown => rec(JSON.parse(String(m).slice('SETTINGS,'.length))).displayScale);
}

/** Hovers the stream at a client point and reads the X pointer back. */
async function hover(host: Host, page: Page, cx: number, cy: number, display: string | undefined): Promise<[number, number]> {
    await within(page.evaluate(`(([cx, cy]) => {
      const ev = new MouseEvent('mousemove', {buttons: 0, clientX: cx, clientY: cy,
        screenX: window.__desk[0] + cx, screenY: cy, bubbles: true});
      document.getElementById('overlayInput').dispatchEvent(ev);
    })(${r([cx, cy])})`));
    await sleep(300);
    return (await host.pointer(display)) ?? [0, 0];
}

/** One arrangement: the secondary must end up at its own density, or at the primary's where its screen is the denser one. */
async function densityCase(host: Host, res: Checks, mode: string, browser: Browser, url: string, display: string, own: string | undefined, primaryDpr: number, secondaryDpr: number): Promise<void> {
    const tag = `[${mode}] primary x${primaryDpr}, secondary x${secondaryDpr}:`;
    const density = Math.min(secondaryDpr, primaryDpr);
    const primary = await newDisplayContext(browser, url, mode, PRIMARY_CSS, primaryDpr, 0);
    const { page } = primary;
    res.check(`${tag} primary video flows`, Boolean(await waitVideo(page, mode)), '');
    const primaryW = PRIMARY_CSS[0] * primaryDpr;
    const secondary = await newDisplayContext(browser, url, mode, SECONDARY_CSS, secondaryDpr, PRIMARY_CSS[0], '#display2-right');
    const dpage = secondary.page;
    res.check(`${tag} secondary video flows`, Boolean(await waitVideo(dpage, mode)), '');
    const want = [SECONDARY_CSS[0] * density, SECONDARY_CSS[1] * density];
    try {
        const realized = await waitFor(() => {
            const m = monitors(display).display2;
            return m !== undefined && m[0] === want[0] && m[1] === want[1];
        }, 30);
        const got = monitors(display);
        res.check(`${tag} the secondary's monitor is its CSS size at x${density}`, realized, `want ${r(want)} monitors=${r(got)}`);
        res.check(`${tag} the primary's monitor keeps its own density`, got.primary?.[0] === primaryW, got);
        res.check(`${tag} the secondary sits at the primary's right edge`, got.display2?.[2] === primaryW, got);

        // The page draws that many stream pixels into its CSS box, and maps
        // the cursor and pointer at the same density.
        const sinkOf = async (): Promise<Rec> => rec(await within(dpage.evaluate(SINK_JS)));
        const sinkOk = await waitFor(async () => (await sinkOf()).w === want[0], 15);
        const sink = await sinkOf();
        res.check(`${tag} the secondary's buffer is at x${density} in its own CSS box`, sinkOk && Math.abs(Number(sink.cssW ?? 0) - SECONDARY_CSS[0]) <= 2 && Math.abs(Number(sink.density ?? 0) - density) < 0.01, sink);
        // The cross-display drag converts through these, so each page has to
        // publish the ratio its box draws at.
        res.check(`${tag} the primary publishes its own density`, await waitFor(async () => (await ownRect(page)).scale === primaryDpr, 15), `primary published ${r(await published(page))}`);
        const laid = await waitFor(async () => (await secondaryRect(page)).scale === density, 15);
        res.check(`${tag} the secondary publishes the scale it draws at with the layout`, laid, `primary published ${r(await published(page))}, secondary ${r(await published(dpage))}`);

        // A point on the secondary lands where the density puts it.
        const pos = await hover(host, dpage, 100, 100, own);
        const expect = [primaryW + 100 * density, 100 * density];
        res.check(`${tag} a pointer on the secondary maps at x${density}`, Math.abs(pos[0] - expect[0]) <= 2 && Math.abs(pos[1] - expect[1]) <= 2, `got ${r(pos)} expected ${r(expect)}`);
        const errors = [...primary.errors, ...secondary.errors].filter((m) => !m.includes('favicon') && !m.includes('ERR_') && !m.includes('404'));
        res.check(`${tag} no console errors`, errors.length === 0, errors.slice(0, 3));
    } finally {
        await dpage.context().close();
        await page.context().close();
        // The secondary's monitor is gone with its page before the next case.
        await waitFor(() => !('display2' in monitors(display)), 15);
    }
}

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const res = new Checks('mixed-dpi');
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const xvfb = process.env.E2E_DISPLAY ? null : await host.xvfb(8192, 4096);
            const display = xvfb?.display ?? process.env.E2E_DISPLAY ?? '';
            try {
                const { url } = await host.start({ label: mode, mode, ...(xvfb ? { display: xvfb.display } : {}) });
                const browser = await launchChromium();
                try {
                    await densityCase(host, res, mode, browser, url, display, xvfb?.display, 2, 1);
                    await densityCase(host, res, mode, browser, url, display, xvfb?.display, 1, 2);
                } finally {
                    await browser.close();
                    await host.stop();
                }
            } finally {
                await xvfb?.release();
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
