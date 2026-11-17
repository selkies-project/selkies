/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Cross-transition test: start in webrtc, stream in browser, switch to
// websockets while a webrtc peer is connected, verify old peer cleanup + new
// transport works. Then the reverse. Ensures no capture-instance leak or port
// conflict across mode flips with an active client.
import type { Browser, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo } from '../lib/stream.ts';

/** Wait for video on the page via the given transport and record a check. */
async function verify(page: Page, mode: string, res: Checks, tag: string): Promise<void> {
    if (mode === 'webrtc') {
        const info = await waitWrVideo(page, 45);
        res.check(`${tag}: video via webrtc`, info !== null, info);
    } else {
        const info = await waitWsVideo(page, 20);
        res.check(`${tag}: video via websockets`, info !== null, info);
    }
}

async function pinned(browser: Browser, url: string, mode: string): Promise<Page> {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    const page = await ctx.newPage();
    await page.goto(url, { waitUntil: 'load' });
    return page;
}

async function switchTo(url: string, mode: string): Promise<[number, string]> {
    const r = await fetch(`${url}/api/switch`, { method: 'POST', body: JSON.stringify({ mode }), headers: { 'Content-Type': 'application/json' } });
    return [r.status, await r.text()];
}

test('cross_transition', async () => {
    const res = new Checks('cross');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        const { url } = await host.start({ label: 'cross', mode: 'webrtc' });
        const browser = await launchChromium();
        try {
            const page = await pinned(browser, url, 'webrtc');
            await verify(page, 'webrtc', res, 'initial webrtc');
            let [s, body] = await switchTo(url, 'websockets');
            res.check('switch to websockets returns 200', s === 200, body.slice(0, 40));
            await sleep(5000);
            // Fresh page pinned to websockets so it exercises the new transport.
            const p2 = await pinned(browser, url, 'websockets');
            await verify(p2, 'websockets', res, 'after flip to websockets');
            [s, body] = await switchTo(url, 'webrtc');
            res.check('switch back to webrtc returns 200', s === 200, body.slice(0, 40));
            await sleep(5000);
            const p3 = await pinned(browser, url, 'webrtc');
            await verify(p3, 'webrtc', res, 'after flip back to webrtc');
            // A client stored on the transport the server is not running gets
            // 409 from its websocket probe (built from the document's own path)
            // and moves itself onto the live one, so a stale tab heals instead
            // of retrying forever.
            const ctx4 = await browser.newContext({ viewport: { width: 1280, height: 720 } });
            await ctx4.addInitScript({ content: "try { const app = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_'); if (!localStorage.getItem(app + '_stream_mode')) localStorage.setItem(app + '_stream_mode', 'websockets'); } catch (e) {}" });
            const p4 = await ctx4.newPage();
            await p4.goto(url, { waitUntil: 'load' });
            // The flip reloads the page, which destroys the evaluation context
            // the wait is polling, so the wait itself is retried across it.
            let info = null;
            for (const until = Date.now() + 90_000; info === null && Date.now() < until;) {
                try {
                    info = await waitWrVideo(p4, 15);
                } catch {
                    await p4.waitForTimeout(1000);
                }
            }
            res.check('a client stored on the inactive transport flips itself over', info !== null, info);
            await ctx4.close();
            await sleep(2000);
            // Multiple flips must not accumulate signaling/capture instances.
            const starts = (await host.log()).split('started signaling server').length - 1;
            res.check("no duplicate 'Signaling server started'", starts <= 2, starts);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
        await host.close();
    }
    res.finish();
});
