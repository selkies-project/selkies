/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A pad left mid-press when the client goes away must be neutralized on both
// transports: on a graceful close through the peer/connection-gone path
// (reset_state), and on an ungraceful client death, where no close arrives on
// any layer, through the held-state heartbeat sweep.
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { killDescendants } from '../lib/procs.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';
import { BTN_A, decode, EV_KEY, has, padInitScript, shimEnv } from '../lib/uinput.ts';

const PAD = padInitScript();

for (const mode of ['websockets', 'webrtc']) {
    for (const death of ['close', 'kill']) {
        test(`${mode}-${death}`, async () => {
            const host = new Host(suiteOf(import.meta.filename));
            const res = new Checks('release');
            const shim = shimEnv(`release-${mode}-${death}`);
            try {
                const server = await host.start({ label: `release-${mode}-${death}`, mode, env: shim.env });
                const browser = await launchChromium();
                const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
                await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
                await ctx.addInitScript({ content: PAD });
                const page = await ctx.newPage();
                await page.goto(`${server.url}/`, { waitUntil: 'load' });
                await (mode === 'webrtc' ? waitWrVideo(page) : waitWsVideo(page));
                // Hold button A (index 0) without releasing it. The press rides the
                // input channel, which opens after the video on WebRTC, so it is
                // re-sent until the kernel device shows it held.
                const held: [number, number, number] = [EV_KEY, BTN_A, 1];
                for (const until = Date.now() + 15_000; Date.now() < until;) {
                    await within(page.evaluate(() => { window.__padPress(0, 1); }));
                    await sleep(400);
                    if (has(decode(shim.stream), held)) break;
                }
                res.check(`${mode}: button held${death === 'close' ? '' : ' before kill'}`, has(decode(shim.stream), held));
                if (death === 'close') {
                    await within(browser.close(), 'the browser');
                } else {
                    // A client machine dying mid-press sends no close on any layer (no
                    // TCP FIN, no SCTP teardown), so the release must come from the
                    // input-liveness sweep, not from transport teardown.
                    const killed = killDescendants((name) => name.includes('chrom') || name.includes('headless'));
                    res.check(`${mode}: the browser's processes were killed`, killed > 0, killed);
                    await within(browser.close(), 'the browser').catch(() => undefined);
                }
                const released: [number, number, number] = [EV_KEY, BTN_A, 0];
                const start = Date.now();
                while (Date.now() - start < 30_000 && !has(decode(shim.stream), released)) await sleep(1000);
                const label = death === 'close'
                    ? 'held button released when the client disappears'
                    : 'held button released on ungraceful kill';
                res.check(`${mode}: ${label}`, has(decode(shim.stream), released), `after ${((Date.now() - start) / 1000).toFixed(1)}s`);
            } finally {
                await host.close();
            }
            res.finish();
        });
    }
}
