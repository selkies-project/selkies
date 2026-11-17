/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The browser's side of the clipboard suites: a page allowed to read and
// write the system clipboard, and formatted copies written to and read from
// it.
import type { Browser, Page } from '@playwright/test';
import { chromium } from '@playwright/test';

import { launchBrowser } from './browsers.ts';
import { sleep } from './env.ts';
import { within } from './stream.ts';

/**
 * A Chromium page on the session at `url`, allowed to read and write the
 * clipboard, given the time a dashboard takes to settle.
 * @param url The server.
 * @param mode The transport the page takes at load.
 * @param plain Whether the browser is Playwright's plain launch (with no
 *     sandbox) rather than the shared switches.
 * @returns The browser and the page.
 */
export async function pageWithClipboard(url: string, mode: string, plain = false): Promise<[Browser, Page]> {
    const browser = plain ? await chromium.launch({ headless: true, args: ['--no-sandbox'] }) : await launchBrowser('chromium');
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: url }).catch(() => undefined);
    const page = await ctx.newPage();
    await page.goto(url, { waitUntil: 'load' });
    await sleep(12_000);
    return [browser, page];
}

/**
 * Write a formatted copy to the browser's clipboard, then hand the page the
 * focus that has it read the clipboard.
 * @param page The page.
 * @param html The markup.
 * @param text Its plain text.
 */
export async function writeRich(page: Page, html: string, text: string): Promise<void> {
    await within(page.evaluate(async ([h, t]) => {
        await navigator.clipboard.write([new ClipboardItem({
            'text/html': new Blob([h], { type: 'text/html' }),
            'text/plain': new Blob([t], { type: 'text/plain' }),
        })]);
    }, [html, text]));
    await sleep(600);
    await page.bringToFront();
    await within(page.evaluate(() => { window.dispatchEvent(new Event('focus')); }));
    await sleep(4000);
}

/** A formatted copy on the browser's clipboard. */
export interface Rich {
    html: string;
    text: string | null;
}

/**
 * The first formatted item on the browser's clipboard.
 * @param page The page.
 * @returns Its markup and plain text, null when it holds none, or the read's failure.
 */
export async function readRich(page: Page): Promise<Rich | string | null> {
    return within(page.evaluate(async (): Promise<Rich | string | null> => {
        try {
            const items = await navigator.clipboard.read();
            for (const item of items) {
                if (!item.types.includes('text/html')) continue;
                return {
                    html: await (await item.getType('text/html')).text(),
                    text: item.types.includes('text/plain') ? await (await item.getType('text/plain')).text() : null,
                };
            }
            return null;
        } catch (err) {
            return `read failed: ${err instanceof Error ? err.name : String(err)}`;
        }
    }));
}

/**
 * Clicks on the stream: the local write needs a user activation, and the
 * payload must have arrived before the one that lands it.
 * @param page The page.
 * @param at Where on the stream.
 * @param n How many.
 * @param gapMs The wait after each.
 */
export async function gestures(page: Page, at: readonly [number, number] = [700, 450], n = 2, gapMs = 2000): Promise<void> {
    for (let i = 0; i < n; i++) {
        await page.mouse.move(at[0], at[1]);
        await page.mouse.down();
        await page.mouse.up();
        await sleep(gapMs);
    }
}
