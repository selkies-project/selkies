/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The X server's fake vblank follows the stream on both transports.
//
// While a page streams the X11 display, the root window carries the capture's
// frame rate as _FAKE_SCREEN_MILLIHZ (millihertz) and _FAKE_SCREEN_FPS (whole
// frames), a live frame-rate change moves them and the output's mode with it,
// and they are gone once the page leaves. The Xvfb the images build paces the
// clients that wait on Present by them (never below the rate it started at
// unless at most 0.1% slower), so a vsynced application presents as fast as
// the session streams; pixelflux publishes the properties on any server,
// which is what is read here.
import { spawnSync } from 'node:child_process';

import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { launchClient, settingsChange } from '../lib/page.ts';
import { waitWrVideo, waitWsVideo } from '../lib/stream.ts';

const RATE_ATOMS = ['_FAKE_SCREEN_FPS', '_FAKE_SCREEN_MILLIHZ'] as const;
type Rate = [number | null, number | null];
const NONE: Rate = [null, null];

/** The root window's _FAKE_SCREEN_FPS and _FAKE_SCREEN_MILLIHZ, null where absent. */
async function fakeRate(host: Host): Promise<Rate> {
    return [await host.rootCardinal(RATE_ATOMS[0]), await host.rootCardinal(RATE_ATOMS[1])];
}

const same = (a: Rate, b: Rate): boolean => a[0] === b[0] && a[1] === b[1];

async function settles(host: Host, want: Rate, timeoutS = 15): Promise<Rate> {
    const until = Date.now() + timeoutS * 1000;
    let got = await fakeRate(host);
    while (!same(got, want) && Date.now() < until) {
        await sleep(250);
        got = await fakeRate(host);
    }
    return got;
}

/** The refresh of the mode the test X server's output shows, 0 if none. */
function outputRefresh(): number {
    const out = spawnSync('xrandr', [], { env: { ...process.env, DISPLAY: process.env.E2E_DISPLAY ?? '' }, encoding: 'utf8', timeout: 10_000 }).stdout ?? '';
    const rate = /(\d+\.\d+)\*/.exec(out);
    return rate ? Number(rate[1]) : 0;
}

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const res = new Checks(`vblank-${mode}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            // A server killed mid-stream, as an earlier suite's SIGKILL does,
            // never takes its rate back, and the check is of this server.
            for (const name of RATE_ATOMS) await host.rootCardinal(name, null);
            const { url } = await host.start({ label: mode, mode, env: { SELKIES_FRAMERATE: '144' } });
            try {
                const before = await settles(host, NONE, 3);
                res.check('nothing is published before a page streams', same(before, NONE), await fakeRate(host));
                const { browser, page } = await launchClient(url, { mode });
                try {
                    const info = mode === 'webrtc' ? await waitWrVideo(page, 45) : await waitWsVideo(page, 20);
                    res.check('the page streams', info !== null, info);
                    let got = await settles(host, [144, 144000]);
                    res.check("the stream's rate is published", same(got, [144, 144000]), got);
                    await settingsChange(page, { framerate: 90 });
                    got = await settles(host, [90, 90000]);
                    res.check('a live frame-rate change follows', same(got, [90, 90000]), got);
                    const rate = outputRefresh();
                    res.check("so does the display's refresh", Math.abs(rate - 90) <= 0.9, rate);
                    await settingsChange(page, { framerate: 59.94 });
                    got = await settles(host, [60, 59940]);
                    res.check('a fractional rate is published to the millihertz', same(got, [60, 59940]), got);
                } finally {
                    await browser.close();
                }
                const got = await settles(host, NONE, 30);
                res.check('the rate is taken back once the page leaves', same(got, NONE), got);
            } finally {
                await host.stop();
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
