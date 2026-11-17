/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Popups of clients on pixelflux's own compositor, end to end.
//
// A client (the crate's wl-popup-client) opens a popup below a 300x40 parent
// at the top left: a layer surface's popup, as a panel opens its menu, or a
// window's. The popup has to show in the stream, below its parent, and the
// pointer moved and clicked over it has to reach the popup's own surface, not
// its parent's.
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, type Observer } from '../lib/host.ts';
import { launchClient } from '../lib/page.ts';
import { decodePng, rgbAt } from '../lib/png.ts';
import { waitWsVideo } from '../lib/stream.ts';

const PARENT = [0x20, 0x60, 0xc0] as const;
const POPUP = [0xe0, 0x80, 0x20] as const;

const near = (pixel: readonly number[], want: readonly number[], tol = 40): boolean => want.every((v, i) => Math.abs(pixel[i] - v) <= tol);

/** Open the client's popup in `mode` ("layer" or "window") and check what the stream shows and where the pointer lands. */
async function popupBlock(host: Host, res: Checks, mode: string): Promise<void> {
    const { url } = await host.start({ label: mode, mode: 'websockets', wayland: true });
    let client: Observer | null = null;
    try {
        const browser = await launchClient(url, { mode: 'websockets' });
        try {
            const { page } = browser;
            res.check('video up', (await waitWsVideo(page)) !== null);
            client = await host.observe('wayland-1', { POPUP_MODE: mode }, 'wl-popup-client');
            const mappedSurfaces = async (): Promise<Set<unknown>> => new Set((await client?.lines() ?? []).filter((e) => e.kind === 'mapped').map((e) => e.surface));
            for (const until = Date.now() + 10_000; Date.now() < until && (await mappedSurfaces()).size < 2;) await sleep(200);
            const mapped = await mappedSurfaces();
            res.check('the parent and its popup map', mapped.has('parent') && mapped.has('popup'), (await client.lines()).slice(0, 6));
            await sleep(2000);
            const shot = decodePng(await page.screenshot());
            const bar = rgbAt(shot, 250, 20);
            const popup = rgbAt(shot, 100, 110);
            res.check('the parent shows', near(bar, PARENT), bar);
            res.check('the popup shows below it', near(popup, POPUP), popup);
            await page.mouse.move(100, 100);
            await sleep(600);
            await page.mouse.move(110, 105);
            await sleep(600);
            const moves = (await client.lines()).filter((e) => e.kind === 'ptr_enter' || e.kind === 'ptr_motion');
            res.check('motion over the popup reaches the popup', moves.some((e) => e.surface === 'popup'), moves.slice(-3));
            await page.mouse.click(110, 105);
            await sleep(600);
            const buttons = (await client.lines()).filter((e) => e.kind === 'ptr_button');
            res.check('a click on the popup reaches the popup', buttons.some((e) => e.surface === 'popup'), buttons.slice(-2));
        } finally {
            await browser.browser.close();
        }
    } finally {
        await client?.release();
        await host.stop();
    }
}

for (const mode of ['layer', 'window']) {
    test(mode, async () => {
        const res = new Checks(`popup-${mode}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await popupBlock(host, res, mode);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
