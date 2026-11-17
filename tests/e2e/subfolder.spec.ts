/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A deployment reverse-proxied under a subfolder prefix.
//
// Every server route moves under the prefix, so the client has to follow it:
// it derives the prefix from its own document path rather than assuming the
// root, and a client that gets that wrong asks a server that only answers
// /desk/api/... for /api/... and comes up dead with no visible cause. Both
// streaming cores and the dashboard compute it, so each payload is served
// under the prefix here and watched for where its requests, its websocket, and
// its static assets actually go.
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { CORE_DIST, DASHBOARD_DIST, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { realConsole, waitWrVideo, waitWsVideo } from '../lib/stream.ts';

const PREFIX = 'desk';

/** HTTP status of a GET against the server, 0 when unreachable. */
async function status(url: string, path: string): Promise<number> {
    try {
        return (await fetch(`${url}${path}`)).status;
    } catch {
        return 0;
    }
}

/** Serve one payload under the prefix and record where the client goes. */
async function drive(res: Checks, host: Host, label: string, webRoot: string, mode: string, layout = ''): Promise<void> {
    const { url } = await host.start({ label, mode, env: { SELKIES_WEB_ROOT: webRoot, SELKIES_SUBFOLDER: `/${PREFIX}/`, ...(layout ? { SELKIES_UI_LAYOUT: layout } : {}) } });
    try {
        res.check(`${label}: prefixed status answers`, (await status(url, `/${PREFIX}/api/status`)) === 200, '');
        const bare = await status(url, '/api/status');
        res.check(`${label}: the unprefixed route is gone`, bare === 404, bare);
        const browser = await launchChromium();
        try {
            const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
            if (mode === 'webrtc') await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'webrtc';" });
            const page = await ctx.newPage();
            const requests: string[] = [];
            const sockets: string[] = [];
            const consoleErrors: string[] = [];
            page.on('request', (r) => requests.push(r.url()));
            page.on('websocket', (ws) => sockets.push(ws.url()));
            page.on('console', (m) => { if (m.type() === 'error') consoleErrors.push(m.text()); });
            page.on('pageerror', (e) => consoleErrors.push(String(e)));
            await page.goto(`${url}/${PREFIX}/`, { waitUntil: 'load' });
            const info = mode === 'webrtc' ? await waitWrVideo(page, 60) : await waitWsVideo(page, 30);
            res.check(`${label}: video flows from the prefixed deployment`, info !== null, info);
            const own = requests.filter((u) => u.startsWith(url));
            const stray = own.filter((u) => !u.startsWith(`${url}/${PREFIX}/`));
            res.check(`${label}: every request stays under the prefix`, own.length > 0 && stray.length === 0, stray.slice(0, 3));
            res.check(`${label}: the websocket opens under the prefix`, sockets.length > 0 && sockets.every((u) => u.includes(`/${PREFIX}/`)), sockets.slice(0, 2));
            const real = realConsole(consoleErrors).errors;
            res.check(`${label}: console clean`, real.length === 0, JSON.stringify(real.slice(0, 2)));
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
}

test('subfolder', async () => {
    const res = new Checks('subfolder');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        await drive(res, host, 'core-ws', CORE_DIST, 'websockets');
        await drive(res, host, 'core-wr', CORE_DIST, 'webrtc');
        await drive(res, host, 'default', DASHBOARD_DIST, 'websockets', 'default');
        await drive(res, host, 'wish', DASHBOARD_DIST, 'websockets', 'wish');
    } finally {
        await host.close();
    }
    res.finish();
});
