/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A page opened at a URL carrying user:pass@ (a link that logs in) drops them
// before its scripts run. Left in document.URL, they make every relative
// fetch() throw (the dashboard's /api/status probe, the mode switch, the
// manifest), and Chrome and Safari run a module that a chunk imports back a
// second time. The browser keeps the login it took from the URL, so the page
// reloads at the same URL without them and streams as it would have.
//
// Each engine opens the dashboard that way under Basic auth: it ends at the
// URL without the credentials, its /api/status probe published the mode, it
// streams with one core connected, and a relative fetch answers. Chromium does
// the same on the core's own page. WebKit attaches no Basic credentials to a
// WebSocket handshake, which presents the socket cookie the page load was
// answered with instead; it also streams at a plain URL with the login given
// to the browser.
import { randomBytes } from 'node:crypto';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchBrowser, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWsVideo, within } from '../lib/stream.ts';

// Drawn per run, URL-safe, so they stand in user:pass@ as they are.
const USER = `user-${randomBytes(4).toString('hex')}`;
const PASSWORD = randomBytes(16).toString('base64url');

/** The document's URL once a page that reloads itself has stopped navigating. */
async function settledUrl(page: Page, seconds = 15): Promise<string> {
    let url = '';
    for (const until = Date.now() + seconds * 1000; Date.now() < until;) {
        try {
            url = await within(page.evaluate(() => (document.readyState === 'complete' ? document.URL : '')));
            if (url && !url.includes('@')) return url;
        } catch {
            // navigating
        }
        await sleep(250);
    }
    return url;
}

async function visit(res: Checks, host: Host, url: string, engine: Engine, pageName: string, inUrl = true): Promise<void> {
    const browser = await launchBrowser(engine);
    try {
        const page = await browser.newPage({ viewport: { width: 1280, height: 800 }, ...(inUrl ? {} : { httpCredentials: { username: USER, password: PASSWORD } }) });
        const logFrom = await host.mark();
        await page.goto(`${inUrl ? url.replace('://', `://${USER}:${PASSWORD}@`) : url}/`, { waitUntil: 'domcontentloaded' });
        const tag = `${engine}, ${pageName}${inUrl ? '' : ', the login given to the browser'}`;
        const ended = await settledUrl(page);
        res.check(`${tag}: the page ends at the URL without the credentials`, ended !== '' && !ended.includes('@'), ended);
        const info = await waitWsVideo(page, 30);
        res.check(`${tag}: the stream comes up`, info !== null, info);
        if (pageName === 'dashboard') {
            const mode = await within(page.evaluate(() => Reflect.get(window, '__SELKIES_STREAMING_MODE__') as unknown));
            res.check(`${tag}: the /api/status probe published the mode`, mode === 'websockets', mode);
        }
        await page.waitForTimeout(2000);
        const sockets = [...(await host.log(logFrom)).matchAll(/Client \S+ connected \(/g)].length;
        res.check(`${tag}: one core connected`, sockets === 1, `${sockets} stream sockets`);
        const status = await within(page.evaluate(() => fetch('manifest.json').then((r) => r.status, (e: unknown) => String(e))));
        res.check(`${tag}: a relative fetch answers`, status === 200 || status === 404, status);
    } finally {
        await browser.close();
    }
}

test('credentials_in_url', async () => {
    const res = new Checks('credentials-in-url');
    const host = new Host(suiteOf(import.meta.filename));
    const env = { SELKIES_ENABLE_BASIC_AUTH: 'true', SELKIES_BASIC_AUTH_USER: USER, SELKIES_BASIC_AUTH_PASSWORD: PASSWORD };
    try {
        let { url } = await host.start({ label: 'dashboard', mode: 'websockets', env: { ...env, SELKIES_WEB_ROOT: DASHBOARD_DIST } });
        for (const engine of ['chromium', 'firefox', 'webkit'] as const) await visit(res, host, url, engine, 'dashboard');
        await visit(res, host, url, 'webkit', 'dashboard', false);
        await host.stop();
        ({ url } = await host.start({ label: 'core', mode: 'websockets', env }));
        await visit(res, host, url, 'chromium', 'core page');
    } finally {
        await host.stop();
        await host.close();
    }
    res.finish();
});
