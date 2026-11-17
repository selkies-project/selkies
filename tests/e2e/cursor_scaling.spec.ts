/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Cursor geometry in Chromium, without a desktop or streaming server.
//
// Serves the real Input module and a decoded video/canvas fixture. CSS and
// canvas cursors follow window, buffer, fullscreen, and reconnect changes
// while clicks retain their stream coordinates. This measures DOM/canvas
// geometry and emitted input, not the physical operating-system cursor or
// server-side injection.
import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { REPO } from '../lib/env.ts';
import { serveTypeScript } from '../lib/static.ts';
import { within } from '../lib/stream.ts';

interface Cursor {
    css: string;
    width: number;
    height: number;
    hotspot: { x: number; y: number };
    transform: string;
}

const ev = <T>(page: Page, script: string): Promise<T> => within(page.evaluate<T>(script));

/** Wait for geometry delivery, then check both the bitmap and the hotspot. */
async function checkCursor(page: Page, res: Checks, label: string, browser: boolean, density: number, size = 48): Promise<void> {
    await page.waitForTimeout(150);
    const got = await ev<Cursor>(page, 'readCursor()');
    let correct: boolean;
    if (browser) {
        const hot = `${Math.floor(12 / density + 0.5)} ${Math.floor(6 / density + 0.5)}, default`;
        // CSS serialization may round the density, but the hotspot is integral.
        const shown = String(density);
        correct = got.css.includes(hot) && (density === 1 || got.css.includes(`${shown}dppx`) || got.css.includes(`${shown}x`));
    } else {
        correct = Math.abs(got.width - size / density) < 0.01 && Math.abs(got.height - size / density) < 0.01
            && Math.abs(got.hotspot.x - 12 / density) < 0.01 && Math.abs(got.hotspot.y - 6 / density) < 0.01;
    }
    res.check(label, correct, { width: got.width, height: got.height, hotspot: got.hotspot, transform: got.transform });
}

test('cursor_scaling', async () => {
    const res = new Checks('cursor-scaling-browser');
    const served = await serveTypeScript(REPO);
    try {
        const browser = await launchChromium();
        try {
            for (const dpr of [1, 1.25, 2]) {
                const context = await browser.newContext({ viewport: { width: 1920, height: 900 }, deviceScaleFactor: dpr });
                try {
                    for (const canvas of [false, true]) {
                        const page = await context.newPage();
                        await page.goto(`${served.url}/tests/tools/cursor_scaling_page.html${canvas ? '?canvas' : ''}`);
                        await page.waitForFunction('window.ready');
                        if (!canvas) res.check(`video DPR ${dpr}: metadata fallback`, (await ev<number>(page, 'input._cursorDensity()')) === dpr);
                        await ev(page, '(async () => { await decode(3840,2160); geometry(1600,900); })()');
                        for (const css of [true, false]) {
                            const label = `${canvas ? 'canvas' : 'video'} DPR ${dpr}, ${css ? 'CSS' : 'canvas'} cursor`;
                            await ev(page, `cursor(${css})`);
                            await checkCursor(page, res, `${label}: 4K fit`, css, 2.4);
                            await page.mouse.click(660, 300);
                            const messages = await ev<string[]>(page, "sent.filter(s => s.startsWith('m,'))");
                            res.check(`${label}: click maps through the letterbox`, messages.some((s) => s.split(',').slice(1, 4).join(',') === '1200,720,1'), messages.slice(-2));
                            if (!css) {
                                const got = await ev<Cursor>(page, 'readCursor()');
                                res.check(`${label}: drawn hotspot at the click`, got.transform === 'translate(655px, 297.5px)', got.transform);
                            }
                            await ev(page, 'geometry(1280,720)');
                            await checkCursor(page, res, `${label}: stationary resize`, css, 3);
                            await ev(page, 'geometry(3840/devicePixelRatio,2160/devicePixelRatio)');
                            await checkCursor(page, res, `${label}: 1:1`, css, dpr);
                            await ev(page, 'geometry(1600,900)');
                            await ev(page, 'document.documentElement.requestFullscreen()');
                            await checkCursor(page, res, `${label}: fullscreen`, css, 2.4);
                            await ev(page, 'document.exitFullscreen()');
                            await ev(page, '(async () => { await decode(1920,1080); })()');
                            await checkCursor(page, res, `${label}: decoded resize at same CSS size`, css, 1.2);
                            await ev(page, `cursor(${css},96)`);
                            await checkCursor(page, res, `${label}: larger application cursor`, css, 1.2, 96);
                            await ev(page, '(async () => { await decode(3840,2160); })()');
                            await ev(page, `(() => { window.replaced = input; makeInput(); return cursor(${css}); })()`);
                            await checkCursor(page, res, `${label}: replacement input`, css, 2.4);
                            await ev(page, 'window.retired = input; input.detach(); geometry(1280,720)');
                            await page.waitForTimeout(150);
                            res.check(`${label}: detached cursor stays hidden`, await ev<boolean>(page, "retired.cursorDiv.style.display === 'none' && "
                                + "!document.getElementById('overlay').style.cursor.includes('image-set') && "
                                + '![retired, replaced].some(i => observers.has(i._cursorResizeObserver) || observers.has(i._cursorMutationObserver))'));
                            await ev(page, `(() => { geometry(1600,900); makeInput(); return cursor(${css}); })()`);
                        }
                        await ev(page, '(async () => { await cursor(false,24); await cursor(true,96); await input.setUseBrowserCursors(false); })()');
                        await checkCursor(page, res, `DPR ${dpr}: mode switch retains current shape`, false, 2.4, 96);
                        await page.close();
                    }
                } finally {
                    await context.close();
                }
            }
        } finally {
            await browser.close();
        }
    } finally {
        await served.close();
    }
    res.finish();
});
