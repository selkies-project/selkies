/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Auth-refresh flow: a live page whose server comes back requiring Basic auth
// must reload itself into the browser's credential prompt (the 401 challenge)
// instead of reconnecting forever against rejected websockets. The reload
// comes from the core's same-origin 401 guard; the guard's loop-breaker caps
// how many reloads a broken credential can cause, so exactly one arrives here.
//
// The page's background requests get the 401 without the Basic scheme, so
// they fail at once; the reload's page load meets the challenge and raises the
// credential prompt, which a headless engine has nobody to answer. The page
// therefore carries a password the server refuses: Playwright offers it once,
// then cancels the prompt, which is what a user who cannot log in does.
import { test } from '@playwright/test';

import { launchBrowser } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWsVideo } from '../lib/stream.ts';

test('auth_refresh', async () => {
    const res = new Checks('auth-refresh');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        const { url, port } = await host.start({ label: 'open', mode: 'websockets', env: { SELKIES_ENABLE_BASIC_AUTH: 'false' } });
        const browser = await launchBrowser('chromium');
        try {
            const page = await browser.newPage({ viewport: { width: 1280, height: 800 }, httpCredentials: { username: 'selkies', password: 'wrong' } });
            const navigates: string[] = [];
            page.on('request', (r) => { if (r.isNavigationRequest() && r.frame() === page.mainFrame()) navigates.push(r.url()); });
            await page.goto(`${url}/`, { waitUntil: 'domcontentloaded' });
            const info = await waitWsVideo(page);
            res.check('stream up before the auth flip', info !== null, info);
            const baseNavs = navigates.length;

            await host.stop();
            await host.start({ label: 'auth', mode: 'websockets', port, env: { SELKIES_ENABLE_BASIC_AUTH: 'true', SELKIES_BASIC_AUTH_USER: 'selkies', SELKIES_BASIC_AUTH_PASSWORD: 'hunter2' } });

            let reloaded = false;
            for (const until = Date.now() + 75_000; Date.now() < until;) {
                await sleep(1000);
                try {
                    if (navigates.length > baseNavs || (await page.content()).includes('Authorization Required')) {
                        reloaded = true;
                        break;
                    }
                } catch {
                    reloaded = true;
                    break;
                }
            }
            res.check('page reloads into the auth challenge', reloaded, `navs=${JSON.stringify(navigates.slice(baseNavs))}`);
            await page.waitForTimeout(6000);
            const extra = navigates.length - baseNavs;
            res.check('reload storm stays capped', extra <= 3, `reloads=${extra}`);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
        await host.close();
    }
    res.finish();
});
