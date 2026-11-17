/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A page keeps the rate congestion control held its display at, and a
// restarted server starts the page's next stream there, on both transports.
//
// With congestion control on, a display's steered rate that held for 10 s is
// told to its controller (CC_RATE over WebSockets, a cc_rate data-channel
// message over WebRTC), which keeps it in localStorage per origin and display.
// Only a tick that carried frames counts toward the hold, so the crate's
// motion-scene program keeps the screen moving while the page waits for the
// rate. A page with a rate kept sends it when it connects (ccStartKbps in its
// WebSockets SETTINGS, cc_start_kbps in its WebRTC HELLO), and a restarted
// server starts the display there rather than at the configured rate. A rate
// a day old starts nothing, and one the user overrides by setting a bitrate is
// dropped.
import { spawn } from 'node:child_process';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, suitesBin } from '../lib/host.ts';
import { launchClient } from '../lib/page.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const ENV = { SELKIES_CONGESTION_CONTROL: 'true' };
const WIDTH = 1280;
const HEIGHT = 720;
const KEY_JS = "(location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_') + '_cc_start'";
const SEEDED = 'starting at 2000 kbps, from the rate its page last held';

/** The start rate the page keeps for its primary display, or {}. */
function kept(page: Page): Promise<{ kbps?: number }> {
    return within(page.evaluate<{ kbps?: number }>(`JSON.parse(localStorage.getItem(${KEY_JS}) || '{}')`));
}

const isKept = (k: { kbps?: number }): boolean => Object.keys(k).length > 0;

/** Put a start rate `ageS` old in the page's storage. */
async function keep(page: Page, kbps: number, ageS = 0): Promise<void> {
    await within(page.evaluate(`localStorage.setItem(${KEY_JS}, JSON.stringify({kbps: ${kbps}, at: Date.now() - ${ageS * 1000}}))`));
}

async function video(page: Page, mode: string): Promise<boolean> {
    return Boolean(mode === 'websockets' ? await waitWsVideo(page, 30) : await waitWrVideo(page, 30));
}

async function run(res: Checks, host: Host, mode: string): Promise<void> {
    let starts = 0;
    const start = async (port?: number): Promise<number> => {
        starts++;
        return (await host.start({ label: `${mode}-${starts}`, mode, env: ENV, ...(port ? { port } : {}) })).port;
    };
    const port = await start();
    /** Restart the server and reload the page against it; whether video came back. */
    const restart = async (page: Page): Promise<boolean> => {
        await host.stop();
        await start(port);
        await page.reload({ waitUntil: 'load' });
        return video(page, mode);
    };
    try {
        const { browser, page } = await launchClient(`http://127.0.0.1:${port}`, { mode });
        try {
            res.check(`${mode}: video flowing`, await video(page, mode));
            const painter = spawn(suitesBin('motion-scene'), [process.env.E2E_DISPLAY ?? '', String(WIDTH), String(HEIGHT), '60'], { stdio: 'ignore' });
            let got: { kbps?: number } = {};
            try {
                for (const until = Date.now() + 25_000; Date.now() < until && !isKept(got);) {
                    got = await kept(page);
                    await sleep(500);
                }
            } finally {
                painter.kill('SIGKILL');
            }
            const kbps = got.kbps ?? 0;
            res.check(`${mode}: the page keeps the rate its display held`, kbps >= 100 && kbps <= 100000, got);

            await keep(page, 2000);
            res.check(`${mode}: a restarted server streams again`, await restart(page));
            res.check(`${mode}: and starts the display at the rate the page kept`, await host.saw(SEEDED, 10), (await host.log()).split('\n').slice(-10).join('\n'));

            await keep(page, 2000, 25 * 3600);
            res.check(`${mode}: after a restart with a rate a day old`, await restart(page));
            await sleep(5000);
            res.check(`${mode}: the display starts at the configured rate`, !(await host.log()).includes(SEEDED));

            await keep(page, 2000);
            await within(page.evaluate("window.postMessage({type: 'settings', settings: {video_bitrate: 6000}}, window.location.origin)"));
            for (const until = Date.now() + 5000; Date.now() < until && isKept(await kept(page));) await sleep(200);
            const left = await kept(page);
            res.check(`${mode}: a bitrate the user sets drops the kept rate`, !isKept(left), left);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
}

test('cc_start_rate', async () => {
    const res = new Checks('cc-start-rate');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        for (const mode of ['websockets', 'webrtc']) await run(res, host, mode);
    } finally {
        await host.close();
    }
    res.finish();
});
