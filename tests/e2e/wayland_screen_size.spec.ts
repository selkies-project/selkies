/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The session's screen is the size its page streams (Wayland).
//
// The primary's capture is a view over the session's screen (output 0), and a
// capture start sizes the view alone, so the screen has to be sized to it as
// well. A page whose size differs from the server's is resized into it; a
// page at the server's own size asks for no resize, and its capture still has
// to find the screen sized, or applications lay out for the compositor's
// first size while the stream shows the rest of the view empty. A fullscreen
// Wayland client (the crate's wl-observer) reads the screen's size. Chromium,
// both transports.
import type { Browser, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, type Observer } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo } from '../lib/stream.ts';

// The server's own size first, then one the page has to resize it to.
const SIZES = [[1920, 1080], [1280, 720]] as const;

async function streamSize(page: Page, mode: string): Promise<[number, number] | null> {
    const video = mode === 'webrtc' ? await waitWrVideo(page, 45) : await waitWsVideo(page, 45);
    return video ? [video.w, video.h] : null;
}

const same = (a: readonly number[] | null, b: readonly number[]): boolean => a !== null && a[0] === b[0] && a[1] === b[1];

async function run(host: Host, res: Checks, browser: Browser, mode: string, size: readonly [number, number]): Promise<void> {
    const [w, h] = size;
    const { url } = await host.start({ label: `${mode}-${w}x${h}`, mode, wayland: true });
    let obs: Observer | null = null;
    try {
        const ctx = await browser.newContext({ viewport: { width: w, height: h }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        const page = await ctx.newPage();
        await page.goto(url, { waitUntil: 'load' });
        let shown = await streamSize(page, mode);
        for (const until = Date.now() + 30_000; !same(shown, size) && Date.now() < until;) {
            await sleep(500);
            shown = await streamSize(page, mode);
        }
        res.check(`${mode} ${w}x${h}: the page streams at its size`, same(shown, size), shown ? `(${shown[0]}, ${shown[1]})` : 'None');
        await sleep(2000);
        obs = await host.observe(await host.captureSocket());
        const mapped = await obs.wait('mapped', 15);
        const screen = mapped !== null && mapped.configured === true ? [Number(mapped.w), Number(mapped.h)] : null;
        res.check(`${mode} ${w}x${h}: a fullscreen client fills the screen the page streams`, same(screen, size), screen ? `(${screen[0]}, ${screen[1]})` : 'None');
        await ctx.close();
    } finally {
        await obs?.release();
        await host.stop();
    }
}

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const res = new Checks(`wl-screen-size-${mode}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const browser = await launchChromium();
            try {
                for (const size of SIZES) await run(host, res, browser, mode, size);
            } finally {
                await browser.close();
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
