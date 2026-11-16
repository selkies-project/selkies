/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A kernel gamepad must survive a transport mode switch: applications hold
// the device open, so it may not be recreated (or lost) when the service
// restarts.
import { readFileSync } from 'node:fs';

import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';
import { BTN_Y, created, decode, EV_KEY, has, padInitScript, shimEnv, STANDARD_XPAD, type Shim } from '../lib/uinput.ts';

const PAD = padInitScript();

/**
 * Open a client on `mode` and press and release button Y.
 * @param url The server.
 * @param mode The transport.
 * @param shim The emulator under the server.
 * @param res The checks.
 * @param tag The checks' prefix.
 */
async function press(url: string, mode: string, shim: Shim, res: Checks, tag: string): Promise<void> {
    const browser = await launchChromium();
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: PAD });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    res.check(`${tag}: video flowing`, Boolean(await (mode === 'webrtc' ? waitWrVideo(page) : waitWsVideo(page))));
    const before = decode(shim.stream).length;
    await within(page.evaluate(() => { window.__padPress(3, 1); }));
    await sleep(400);
    await within(page.evaluate(() => { window.__padPress(3, 0); }));
    await sleep(400);
    const events = decode(shim.stream).slice(before);
    res.check(`${tag}: input reaches the kernel device`, has(events, [EV_KEY, BTN_Y, 1]), events.slice(0, 2));
    await within(browser.close(), 'the browser');
}

test('switch', async () => {
    const host = new Host(suiteOf(import.meta.filename));
    const res = new Checks('switch');
    const shim = shimEnv('switch');
    try {
        const server = await host.start({ label: 'switch', mode: 'websockets', env: shim.env });
        await press(server.url, 'websockets', shim, res, 'websockets');
        const switched = await fetch(`${server.url}/api/switch`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ mode: 'webrtc' }),
            signal: AbortSignal.timeout(10_000),
        });
        res.check('switch accepted', switched.status === 200, switched.status);
        await sleep(3000);
        await press(server.url, 'webrtc', shim, res, 'after switch');
        const made = created(shim.log, STANDARD_XPAD);
        res.check('kernel device created once', made === 1, `${made} creations of '${STANDARD_XPAD}'`);
        res.check('kernel device never destroyed by the switch', !readFileSync(shim.log, 'utf8').includes('DEV_DESTROY'));
    } finally {
        await host.close();
    }
    res.finish();
});
