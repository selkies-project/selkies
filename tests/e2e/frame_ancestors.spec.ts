/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Which pages may show the client in a frame is the operator's to name.
//
// frame_ancestors is sent as the Content-Security-Policy frame-ancestors
// directive on every response, and the browser refuses a frame the list does
// not name. Unset or empty it is 'self', which shuts out a page on any other
// origin, so another site cannot lay the session under its own clicks; a
// platform that embeds the desktop from another origin lists it and gets its
// frame, and * lets any page frame it.
//
// A page on a second origin frames the client under each policy, and
// Chromium's own verdict is read from what the frame ends up showing.
import { Server } from 'node:http';
import type { AddressInfo } from 'node:net';

import type { Browser } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';

/** Whether the client came up inside the other origin's frame. */
async function framed(browser: Browser, embedUrl: string, url: string): Promise<boolean> {
    const page = await browser.newPage();
    try {
        await page.goto(embedUrl, { waitUntil: 'load' });
        for (const until = Date.now() + 10_000; Date.now() < until;) {
            const child = page.frames().find((f) => f !== page.mainFrame());
            if (child?.url().startsWith(`${url}/`)) {
                try {
                    if ((await child.evaluate(() => document.readyState)) === 'complete') return true;
                } catch {
                    // the frame is still coming up
                }
            }
            await sleep(250);
        }
        return false;
    } finally {
        await page.close();
    }
}

test('frame_ancestors', async () => {
    const res = new Checks('frame-ancestors');
    const host = new Host(suiteOf(import.meta.filename));
    // A page on another origin (localhost rather than the client's 127.0.0.1) that frames the client.
    let target = '';
    const embedder = new Server((_req, reply) => {
        const body = `<html><body><iframe id="f" src="${target}/"></iframe></body></html>`;
        reply.writeHead(200, { 'Content-Type': 'text/html', 'Content-Length': Buffer.byteLength(body) });
        reply.end(body);
    });
    await new Promise<void>((resolve) => { embedder.listen(0, '127.0.0.1', () => { resolve(); }); });
    const address: AddressInfo | string | null = embedder.address();
    const embedOrigin = `http://localhost:${address !== null && typeof address === 'object' ? address.port : 0}`;
    const cases: [string, string | null, string, boolean][] = [
        ['unset', null, "frame-ancestors 'self'", false],
        ['empty', '', "frame-ancestors 'self'", false],
        ["'self'", "'self'", "frame-ancestors 'self'", false],
        ['the embedding origin', `self, ${embedOrigin}`, `frame-ancestors 'self' ${embedOrigin}`, true],
        ['any page', '*', 'frame-ancestors *', true],
    ];
    const browser = await launchChromium();
    try {
        for (const [label, value, header, allowed] of cases) {
            const { url } = await host.start({ label: label.replace(/[^a-z]+/g, '-'), env: value !== null ? { SELKIES_FRAME_ANCESTORS: value } : {} });
            target = url;
            try {
                const sent = (await fetch(`${url}/`)).headers.get('Content-Security-Policy') ?? '';
                res.check(`${label}: the server sends ${header || 'no frame policy'}`, sent === header, sent);
                const shown = await framed(browser, `${embedOrigin}/`, url);
                res.check(`${label}: another origin's frame ${allowed ? 'shows' : 'is refused'} the client`, shown === allowed, `framed=${shown ? 'True' : 'False'}`);
            } finally {
                await host.stop();
            }
        }
    } finally {
        await browser.close();
        embedder.close();
        await host.close();
    }
    res.finish();
});
