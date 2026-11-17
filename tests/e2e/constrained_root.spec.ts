/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A second screen asked of an X server that will not grow its root.
//
// The layout engines ask for a framebuffer covering every display and then
// have to live with what the server gave back. A server whose root is capped
// below the request is the case that decides whether a session degrades or
// breaks: the primary has to keep streaming at a size that exists, and a
// secondary with nowhere to live has to be refused with a verdict the client
// treats as final -- a bare pipeline failure looks transient and the client
// reloads into a loop.
//
// The cap is real, not simulated: Xvfb fixes its maximum screen size at the
// initial allocation, so RRAddOutputMode and RRSetScreenSize past it fail the
// way a driver out of scanout memory fails. Both engines run against the same
// capped server, because this is exactly the path where they are apt to
// disagree.
//
// A stock Xvfb fixes its maximum screen size at the initial allocation, so a
// small private server IS a server that refuses to grow. The suite drives its
// own rather than the harness display, which is deliberately large. The Xvfb
// the images build sizes its framebuffer freely, so where that one is on the
// path the distribution's is tried next, and the suite skips on a host with
// no server that caps.
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';

import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { onPath } from '../lib/desktop.ts';
import { suiteOf, sleep } from '../lib/env.ts';
import { Host, type Held } from '../lib/host.ts';
import { launchClient, newPage } from '../lib/page.ts';
import { realConsole, waitWrVideo, waitWsVideo, type VideoSize } from '../lib/stream.ts';

// Below the browser viewport the pages run at, so the primary's own request
// is refused too, and small enough that no arrangement of two displays fits.
const CAP_W = 1024;
const CAP_H = 768;
const XVFB_EXTRA = ['+extension', 'COMPOSITE', '+extension', 'DAMAGE', '+extension', 'RANDR', '+extension', 'XFIXES', '+extension', 'XTEST', '-shmem', '-s', '0', '-dpms'];

/** The largest screen size this server will accept, per RandR. */
function rootMax(display: string): [number, number] | null {
    const out = spawnSync('xrandr', ['-display', display], { encoding: 'utf8' }).stdout ?? '';
    for (const line of out.split('\n')) {
        if (line.startsWith('Screen ') && line.includes('maximum')) {
            const [w, h] = line.split('maximum', 2)[1].trim().replace(/,$/, '').split(' x ');
            return [parseInt(w, 10), parseInt(h, 10)];
        }
    }
    return null;
}

async function run(host: Host, display: string, mode: string): Promise<Checks> {
    const res = new Checks(`constrained-root-${mode}`);
    const cap = rootMax(display);
    res.check('the test server really caps its root', cap !== null && cap[0] === CAP_W && cap[1] === CAP_H, cap);
    const { url } = await host.start({ label: mode, mode, display });
    const client = await launchClient(url, { mode });
    const { page } = client;
    const video = (timeoutS: number): Promise<VideoSize | null> => (mode === 'webrtc' ? waitWrVideo(page, timeoutS) : waitWsVideo(page, timeoutS));
    /** Whether the server currently has a running capture for the secondary. */
    const secondaryLive = (): Promise<boolean> => host.saw(mode === 'webrtc' ? "Secondary display 'display2' pipeline started" : "Capture started for 'display2'", 12);
    const insideCap = (root: [number, number] | null): boolean => root !== null && root[0] <= CAP_W && root[1] <= CAP_H;
    try {
        const info = await video(mode === 'webrtc' ? 45 : 20);
        res.check('primary streams although its own size was refused', info !== null, info);
        if (info === null) return res;
        let root = await host.rootSize(display);
        res.check('the root never grew past the cap', insideCap(root), root);
        res.check('the primary is streaming a region that exists', root !== null && info.w <= root[0] && info.h <= root[1], `stream ${info.w}x${info.h} root ${JSON.stringify(root)}`);

        // A secondary to the right starts where the root ends: there is no
        // region for it, and it has to be refused rather than laid out.
        let dpage = await newPage(client.context, url, { mode, hash: '#display2-right' });
        res.check('the unplaceable secondary is reported', await host.saw('does not fit the realized', 45), '');
        res.check('no capture is started outside the root', !(await secondaryLive()), '');
        await sleep(2000);
        let after = await video(15);
        res.check('the primary survives the refusal', after !== null, after);
        root = await host.rootSize(display);
        res.check('the root is still inside the cap', insideCap(root), root);
        await dpage.close();
        await sleep(3000);

        // To the left the arrangement pushes the PRIMARY off the root, which
        // must never cost the session its own screen.
        dpage = await newPage(client.context, url, { mode, hash: '#display2-left' });
        res.check('a primary pushed off the root is re-anchored', await host.saw('re-anchored at the origin', 45), '');
        res.check('the arrangement that caused it is refused', !(await secondaryLive()), '');
        await sleep(2000);
        after = await video(15);
        res.check('the primary still streams after re-anchoring', after !== null, after);
        await dpage.close();

        const real = realConsole(client.consoleErrors, client.notFound).errors;
        res.check('no console errors', real.length === 0, real.join('; ').slice(0, 200));
    } finally {
        await client.browser.close();
        await host.stop();
    }
    return res;
}

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const host = new Host(suiteOf(import.meta.filename));
        let xvfb: (Held & { display: string }) | null = null;
        let res: Checks;
        try {
            for (const binary of [...new Set(['Xvfb', '/usr/bin/Xvfb'])]) {
                if (!(binary.startsWith('/') ? existsSync(binary) : onPath(binary))) continue;
                const started = await host.xvfb(CAP_W, CAP_H, XVFB_EXTRA, binary);
                const max = rootMax(started.display);
                if (max !== null && max[0] === CAP_W && max[1] === CAP_H) {
                    xvfb = started;
                    break;
                }
                await started.release();
            }
            if (xvfb === null) {
                res = new Checks(`constrained-root-${mode}`);
                res.skip('the suite', 'no X server on this host caps its root at its initial size');
            } else {
                res = await run(host, xvfb.display, mode);
            }
        } finally {
            await xvfb?.release();
            await host.close();
        }
        res.finish();
    });
}
