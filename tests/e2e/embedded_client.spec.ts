/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The web client the server carries: served from memory where no web_root
// holds one.
//
// Every other suite points web_root at a client built in the tree. Here none
// is set, so the server answers from the table compiled into it, and says so
// at start. The entry page and a content-hashed script it loads come back
// gzip-encoded with their entity tags and dates, a revalidation is a 304, and
// a client naming no encoding gets the same bytes inflated. A browser loading
// the page streams over the transport the block names (websockets, webrtc),
// and a second load takes the hashed assets from its cache without a request,
// while the entry page and the core it loads under a fixed name are
// revalidated.
import { spawnSync } from 'node:child_process';
import { request } from 'node:http';
import { gunzipSync } from 'node:zlib';

import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

interface Got {
    status: number;
    headers: Record<string, string | undefined>;
    body: Buffer;
}

/** A GET of `path`, a 304 included, its body as sent: fetch would inflate it. */
function get(url: string, path: string, headers: Record<string, string>): Promise<Got> {
    return new Promise((resolve, reject) => {
        const req = request(`${url}${path}`, { headers, timeout: 10_000 }, (r) => {
            const chunks: Buffer[] = [];
            r.on('data', (c: Buffer) => chunks.push(c));
            r.on('end', () => {
                const flat: Record<string, string | undefined> = {};
                for (const [k, v] of Object.entries(r.headers)) flat[k.toLowerCase()] = Array.isArray(v) ? v.join(', ') : v;
                resolve({ status: r.statusCode ?? 0, headers: flat, body: Buffer.concat(chunks) });
            });
        });
        req.on('error', reject);
        req.end();
    });
}

/** The entry page and a hashed script by encoding and validator. */
async function httpBlock(res: Checks, url: string): Promise<void> {
    let got = await get(url, '/', { 'Accept-Encoding': 'gzip' });
    const page = got.headers['content-encoding'] === 'gzip' ? gunzipSync(got.body) : got.body;
    res.check('the entry page is served', got.status === 200 && page.includes('selkies-core.js'), got.status);
    res.check('the entry page is revalidated on every load', got.headers['cache-control'] === 'no-cache', got.headers['cache-control'] ?? null);
    const found = /src="\.\/(assets\/[^"]+\.js)"/.exec(page.toString('latin1'));
    res.check('the entry page loads a content-hashed script', found !== null, page.subarray(0, 120).toString('latin1'));
    if (found === null) return;
    const asset = `/${found[1]}`;
    got = await get(url, asset, { 'Accept-Encoding': 'gzip, deflate, br' });
    const tag = got.headers.etag ?? '';
    const body = got.body;
    res.check(`${asset} goes out gzip-encoded to a client that takes it`,
        got.status === 200 && got.headers['content-encoding'] === 'gzip' && (got.headers.vary ?? '').toLowerCase() === 'accept-encoding',
        [got.status, got.headers['content-encoding'] ?? null, got.headers.vary ?? null]);
    res.check(`${asset} carries a strong tag and a date and no Cache-Control`,
        tag.startsWith('"') && Boolean(got.headers['last-modified']) && got.headers['cache-control'] === undefined,
        [tag, got.headers['last-modified'] ?? null, got.headers['cache-control'] ?? null]);
    got = await get(url, asset, {});
    res.check(`${asset} goes out as it is to a client naming no encoding`,
        got.status === 200 && got.headers['content-encoding'] === undefined && got.body.equals(gunzipSync(body)),
        [got.status, got.headers['content-encoding'] ?? null, got.body.length]);
    got = await get(url, asset, { 'Accept-Encoding': 'gzip', 'If-None-Match': tag });
    res.check(`${asset} revalidates with a 304`, got.status === 304 && got.body.length === 0, got.status);
}

/** The page streams, and a second load reaches the server only for what revalidates. */
async function browserBlock(res: Checks, url: string, mode: string): Promise<void> {
    const browser = await launchChromium();
    try {
        const context = await browser.newContext({ viewport: { width: 1280, height: 720 } });
        await context.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        let page = await context.newPage();
        try {
            await page.goto(`${url}/`, { waitUntil: 'load' });
        } catch (e) {
            res.check(`the page streams over ${mode}`, false, String(e));
            return;
        }
        const video = mode === 'websockets' ? await waitWsVideo(page, 30) : await waitWrVideo(page, 45);
        res.check(`the page streams over ${mode}`, video !== null, video);
        await page.close();
        page = await context.newPage();
        await page.goto(`${url}/`, { waitUntil: 'load' });
        const loads = await within(page.evaluate(() => [...performance.getEntriesByType('resource'), ...performance.getEntriesByType('navigation')]
            .map((e) => ({ name: new URL(e.name).pathname, transfer: e instanceof PerformanceResourceTiming ? e.transferSize : 0 }))));
        const assets = loads.filter((e) => e.name.startsWith('/assets/'));
        const fetched = assets.filter((e) => e.transfer > 0).map((e) => e.name);
        res.check("a second load takes every hashed asset from the browser's cache", assets.length > 0 && fetched.length === 0, fetched.length ? fetched.slice(0, 3) : assets.length);
        const asked = Object.fromEntries(loads.filter((e) => e.name === '/' || e.name === '/selkies-core.js').map((e) => [e.name, e.transfer]));
        res.check('and asks the server again for the entry page and the core', Object.keys(asked).length === 2 && Object.values(asked).every((size) => size > 0), asked);
    } finally {
        await browser.close();
    }
}

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const res = new Checks(`embedded-client-${mode}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const version = spawnSync(await host.serverBinary(), ['--version'], { encoding: 'utf8' }).stdout ?? '';
            res.check('the server under test embeds a web client', version.includes('web client: embedded ('), version.trim().split('\n').slice(-1));
            const { url } = await host.start({ label: mode, mode, env: { SELKIES_WEB_ROOT: '' } });
            try {
                const said = await host.saw('Serving the web client from this executable');
                res.check('the server says it serves the client it embeds', said, (await host.log()).split('\n').filter((l) => l.includes('web client')).slice(0, 1));
                await httpBlock(res, url);
                await browserBlock(res, url, mode);
            } finally {
                await host.stop();
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
