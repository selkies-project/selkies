/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A WebRTC sender that falls behind its encoder keeps the picture decodable.
//
// Encoded frames cross from the capture thread to the RTP sender through a
// depth-one bridge, so a sender that cannot keep up drops whole frames before
// they are packetized: no sequence number is spent, the receiver sees no loss
// and asks for nothing, and every delta frame after the drop references a
// picture it never received. The bridge has to close the reference chain
// itself: hold delta frames back until a keyframe it asked for arrives, and
// never let a later delta frame evict that keyframe.
//
// The scene is the crate's motion-scene program, whose every frame spells its
// own index, so a decoded picture can be compared with the frame it claims to
// be, and whose noise band keeps a constant-bitrate stream at its configured
// rate. The sender is made to fall behind by pinning the server's runtime to
// one CPU and sharing that CPU with busy processes in bursts (the host's
// load, crates/selkies-suites/src/load.rs). The browser's decoded pictures,
// its inbound-rtp counters, and the server's bridge-drop counter are read
// throughout.
//
// E2E_ENGINE selects the browser (chromium, the default, firefox, or webkit).
import { spawn } from 'node:child_process';

import type { Browser, BrowserContext } from '@playwright/test';
import { test } from '@playwright/test';

import { engineFromEnv, firefoxPersistentContext, launchBrowser } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { bridgeCounter, corrupt, INIT_JS, sample, serverEnv, SETTLE_S, stats, waitClean, type Sampled } from '../lib/drops.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, suitesBin, type Load } from '../lib/host.ts';
import { closeBrowser, waitWrVideo } from '../lib/stream.ts';

const WIDTH = 1280;
const HEIGHT = 720;
const ENGINE = engineFromEnv();
// The busy processes' on/off cadence.
const BURST_ON_S = 0.4;
const BURST_OFF_S = 0.6;
const LOAD_S = 20;
async function drive(res: Checks, host: Host, cell: string, display: string): Promise<void> {
    const tag = `${ENGINE}-${cell}`;
    const { url, pid } = await host.start({ label: cell, mode: 'webrtc', display, env: serverEnv(cell) });
    const threads = await host.threads(pid);
    const painter = spawn(suitesBin('motion-scene'), [display, String(WIDTH), String(HEIGHT), '60'], { stdio: 'ignore' });
    let load: Load | null = null;
    let browser: Browser | null = null;
    let ctx: BrowserContext;
    if (ENGINE === 'firefox') {
        ctx = await firefoxPersistentContext({ viewport: { width: WIDTH, height: HEIGHT } });
    } else {
        browser = await launchBrowser(ENGINE);
        ctx = await browser.newContext({ viewport: { width: WIDTH, height: HEIGHT }, deviceScaleFactor: 1 });
    }
    try {
        await ctx.addInitScript({ content: INIT_JS });
        const page = await ctx.newPage();
        await page.goto(`${url}/`, { waitUntil: 'load' });
        const info = await waitWrVideo(page, 45);
        res.check(`${tag}: video decodes`, Boolean(info), info);
        const first = await waitClean(page, 30);
        res.check(`${tag}: the decoded picture matches the frame it claims to be`, first !== null && !corrupt(first), first);
        if (!first) return;
        const stats0 = await stats(page);
        const drops0 = await bridgeCounter(url, 'dropped');
        res.check(`${tag}: the bridge-drop counter is published`, drops0 !== null, drops0);

        load = await host.load(threads);
        const samples: Sampled[] = [];
        const end = Date.now() + LOAD_S * 1000;
        let prev = { ...stats0, drops: drops0 ?? 0 };
        while (Date.now() < end) {
            const started = Date.now();
            await load.resume();
            const burst: Sampled[] = [];
            const plis: number[] = [];
            while (Date.now() < started + BURST_ON_S * 1000) {
                const s = await sample(page);
                if (s) burst.push(s);
            }
            await load.pause();
            for (const restEnd = Date.now() + BURST_OFF_S * 1000; Date.now() < restEnd;) {
                const s = await sample(page);
                if (s) burst.push(s);
                const st = await stats(page);
                if (st.plis > prev.plis + plis.length) plis.push(Math.round((Date.now() - started) / 10) / 100);
            }
            samples.push(...burst);
            const now = { ...(await stats(page)), drops: (await bridgeCounter(url, 'dropped')) ?? 0 };
            const at = ((started - (end - LOAD_S * 1000)) / 1000).toFixed(1);
            process.stdout.write(`      [${tag}] burst at +${at}s: drops +${now.drops - prev.drops}, keyframes +${now.keyframes - prev.keyframes}, `
                + `plis +${now.plis - prev.plis} at ${JSON.stringify(plis)}, corrupt ${burst.filter(corrupt).length}/${burst.length}\n`);
            prev = now;
        }
        await load.release();
        load = null;
        const after: Sampled[] = [];
        for (const until = Date.now() + SETTLE_S * 1000; Date.now() < until;) {
            const s = await sample(page);
            if (s) after.push(s);
            await sleep(200);
        }
        const stats1 = await stats(page);
        const drops1 = (await bridgeCounter(url, 'dropped')) ?? 0;
        const dropped = drops1 - (drops0 ?? 0);
        const keyframes = stats1.keyframes - stats0.keyframes;
        const bad = samples.filter(corrupt);
        const badAfter = after.filter(corrupt);
        const worst = Math.max(...[...samples, ...after].map((s) => s.mismatch));
        const detail = `drops +${dropped}, keyframes +${keyframes}, frames +${stats1.frames - stats0.frames}, `
            + `lost ${stats1.lost - stats0.lost}, nacks +${stats1.nacks - stats0.nacks}, `
            + `plis +${stats1.plis - stats0.plis}, decoder drops +${stats1.dropped - stats0.dropped}, `
            + `corrupt ${bad.length}/${samples.length} under load, `
            + `${badAfter.length}/${after.length} after; worst ${worst.toFixed(3)}`;
        process.stdout.write(`      [${tag}] ${detail}\n`);
        if (dropped <= 0) {
            res.skip(`${tag}: recovery from bridge drops`, `the sender never fell behind: ${detail}`);
            return;
        }
        res.check(`${tag}: every run of bridge drops is followed by a keyframe`, keyframes >= 1, detail);
        // Every decoded picture matches its own index: the encoder codes what
        // changed even when the rate budget cannot be met, and a broken
        // reference chain would corrupt every burst.
        res.check(`${tag}: no decoded picture shows a broken reference`, bad.length === 0 && badAfter.length === 0, detail);
        const last = after.at(-1);
        res.check(`${tag}: the picture is clean once the load ends`, last !== undefined && !corrupt(last), last ?? null);
    } finally {
        await load?.release();
        await closeBrowser(browser ?? ctx);
        painter.kill('SIGTERM');
        await host.stop();
    }
}

for (const cell of ['default', 'cpu']) {
    test(cell, async () => {
        const res = new Checks('video-drop-recovery');
        const host = new Host(suiteOf(import.meta.filename));
        try {
            let display = process.env.E2E_DISPLAY ?? '';
            if (!display) display = (await host.xvfb(WIDTH, HEIGHT)).display;
            await drive(res, host, cell, display);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
