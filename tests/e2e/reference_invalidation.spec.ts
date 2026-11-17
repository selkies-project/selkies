/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A dropped frame costs the stream one prediction further back, not a keyframe.
//
// A WebRTC sender that falls behind its encoder for a moment drops frames
// before they are packetized. Told which frame went, the encoder predicts past
// it and the receiver decodes on; told nothing, the stream can only resume at a
// keyframe, which costs the link an intra picture and the viewer a frozen one.
// The load here is that moment, repeated: the server's runtime shares one CPU
// with busy processes in short bursts (the host's load,
// crates/selkies-suites/src/load.rs), each long enough to drop frames and too
// short for the receiver to ask for a keyframe on its own. The scene is the
// crate's motion-scene program, whose every frame spells its own index, so a
// decoded picture can be compared with the frame it claims to be. Counted
// throughout: the frames the server dropped, those of them the encoder was told
// to predict past, the keyframes decoded, and the picture-loss indications the
// receiver sent. Measured against the same load without the repair, which
// spends a keyframe for about every eight frames it drops: 456 and 621 drops
// and 69 and 72 keyframes there, 173 to 264 drops and none here.
//
// What the repair itself may spend depends on the stream that carried the
// load. A loss covering the frame where H.264's frame_num wraps is answered
// with a keyframe, and how many values frame_num takes is the encoder's
// sequence parameter set's to say, which the server logs with each key frame
// that changes it: 65536 on NVENC and 4096 on x264, whose own 256 and sixteen
// values pixelflux carries a byte wider, and on VA-API 65536 where pixelflux
// writes the set and 128 where radeonsi does. So is a burst that let go about
// as many frames as the encoder keeps references, since the frame it would
// predict past them from has left its window, so the ceiling is worked out
// from what each burst let go (keyframeCeiling). On NVENC a repaired load cost
// 0 to 3 keyframes, and one that asked a keyframe for every loss 46 to 64. On
// x264 it cost 0; x264's own sixteen values cost 0 and 3, and a keyframe for
// every loss 44 to 62. A GitHub runner slow enough that nine of 21 bursts ran
// past the window cost 12.
//
// E2E_ENGINE selects the browser, Chromium by default.
import { spawn } from 'node:child_process';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { engineFromEnv, firefoxPersistentContext, launchBrowser } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { bridgeCounter, corrupt, INIT_JS, sample, serverEnv, SETTLE_S, stats, waitClean, type Sampled } from '../lib/drops.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, suitesBin, type Load } from '../lib/host.ts';
import { frameNumRanges, logSince } from '../lib/serverlog.ts';
import { closeBrowser, waitWrVideo, within } from '../lib/stream.ts';

const WIDTH = 1280;
const HEIGHT = 720;
const ENGINE = engineFromEnv();
// Short bursts, inside the eight frames of references the encoder keeps (133
// ms at 60 fps) and well inside the ~400 ms a receiver waits before asking for
// a keyframe itself: each drops a handful of frames and nothing else.
const BURST_ON_S = 0.08;
const BURST_OFF_S = 0.42;
const LOAD_S = 24;
// The frames of references each encoder keeps at this size.
const REFERENCES = 8;
const SETTINGS = /Stream settings active -> .*?\| Encoder: ([^|]+?) \|/g;

/** Every readable sample over `seconds`. */
async function sampleFor(page: Page, seconds: number): Promise<Sampled[]> {
    const out: Sampled[] = [];
    for (const end = Date.now() + seconds * 1000; Date.now() < end;) {
        const s = await sample(page);
        if (s) out.push(s);
    }
    return out;
}

/**
 * The keyframes a working repair can spend on a load that let frames go in
 * `bursts` (the count let go in each burst that lost any, named to the
 * encoder or held back behind one that was), allowing three standard
 * deviations.
 *
 * A loss is answered with a keyframe when a frame from the lost one to the
 * newest encoded carries frame_num 0, which the browsers' FFmpeg decoder
 * cannot be predicted past, or when the frame before the loss, which the
 * encoder would predict the next frame from, has left its window of
 * REFERENCES frames; later reports of frames before that keyframe are
 * ignored, so a burst costs at most one. Every frame a burst let go was
 * encoded after that frame, as were the one that ended the burst and the one
 * the encoder has in hand when the word reaches it, so a burst that let go
 * REFERENCES - 2 or more counts as one. Frames reach the server's loop in
 * order, so a shorter burst that let go n frames had the encoder about n
 * frames past the first of them: its odds of covering a wrap are taken as
 * (n + REFERENCES) / frameNumRange, a span longer than any loss the
 * references still hold.
 */
function keyframeCeiling(bursts: readonly number[], frameNumRange: number): number {
    const odds = bursts.map((n) => (n >= REFERENCES - 2 ? 1 : (n + REFERENCES) / frameNumRange));
    const sum = odds.reduce((a, p) => a + p, 0);
    const variance = odds.reduce((a, p) => a + p * (1 - p), 0);
    return Math.min(bursts.length, Math.ceil(sum + 3 * Math.sqrt(variance)));
}

interface Report {
    samples: Sampled[];
    dropped: number;
    named: number;
    bursts: [number, number][];
    encoders: string[];
    ranges: number[];
    keyframes: number;
    plis: number | null;
    frames: number;
}

/**
 * The checks both transports share: the drops happened, the encoder was told
 * which frames to predict past, the recovery cost no keyframe beyond the
 * frame_num wraps the losses covered, and every decoded picture was the frame
 * it claimed to be. A load that let nothing go fails as a load: there was no
 * loss to repair, so the repair's own checks are not run.
 */
function report(res: Checks, tag: string, r: Report): void {
    const bad = r.samples.filter(corrupt);
    const detail = `dropped ${r.dropped}, named ${r.named}, keyframes +${r.keyframes}, `
        + `${r.plis !== null ? `plis +${r.plis}, ` : ''}frames +${r.frames}, `
        + `corrupt ${bad.length}/${r.samples.length}`;
    process.stdout.write(`      [${tag}] ${detail}, on ${r.encoders.join(' then ') || 'no encoder logged'}, `
        + `frame_num ranges ${JSON.stringify(r.ranges)}, let go and named per burst ${JSON.stringify(r.bursts)}\n`);
    res.check(`${tag}: the load really dropped frames`, r.dropped > 0, r.dropped ? detail : `the load let no frame go on this host: ${detail}`);
    if (!r.dropped) {
        res.skip(`${tag}: the encoder was told which frames to predict past`, 'no drops to repair');
        res.skip(`${tag}: the drops cost at most one keyframe beyond the frame_num wraps`, 'no drops to repair');
    } else {
        res.check(`${tag}: the encoder was told which frames to predict past`, r.named > 0, detail);
    }
    // Beyond what the wraps and the deep bursts cost (keyframeCeiling), one is
    // the measured allowance -- a burst that lands badly still trips the
    // receiver's own keyframe timer now and then -- and every capture restart
    // opens with one. A repair that spends a keyframe on every loss costs about
    // one per burst, against a ceiling of a handful.
    res.check(`${tag}: the server logged the frame_num range the load ran on`, r.ranges.length > 0, `${JSON.stringify(r.ranges)} on ${JSON.stringify(r.encoders)}`);
    if (r.ranges.length && r.dropped) {
        const lowest = Math.min(...r.ranges);
        const ceiling = 1 + (r.encoders.length - 1) + keyframeCeiling(r.bursts.map(([n]) => n), lowest);
        res.check(`${tag}: the drops cost at most one keyframe beyond the frame_num wraps`, r.keyframes <= ceiling,
            `keyframes +${r.keyframes}, ceiling ${ceiling} on ${r.encoders.at(-1)} (frame_num range ${lowest}) from ${r.bursts.length} bursts`);
    }
    if (r.plis !== null) res.check(`${tag}: and the receiver asked for at most one`, r.plis <= 1, detail);
    res.check(`${tag}: the stream kept flowing`, r.frames >= 20 * LOAD_S, detail);
    res.check(`${tag}: every decoded picture was the frame it claimed to be`, bad.length === 0, detail);
}

async function webrtc(res: Checks, host: Host, display: string): Promise<void> {
    const tag = `webrtc-${ENGINE}`;
    const { url, pid } = await host.start({ label: 'webrtc', mode: 'webrtc', display, env: serverEnv('default') });
    const threads = await host.threads(pid);
    const painter = spawn(suitesBin('motion-scene'), [display, String(WIDTH), String(HEIGHT), '60'], { stdio: 'ignore' });
    let load: Load | null = null;
    let browser: Browser | null = null;
    let ctx: BrowserContext;
    if (ENGINE === 'firefox') {
        // The persistent profile carries the OpenH264 plugin Firefox decodes H.264 with.
        ctx = await firefoxPersistentContext({ viewport: { width: WIDTH, height: HEIGHT } });
    } else {
        browser = await launchBrowser(ENGINE);
        ctx = await browser.newContext({ viewport: { width: WIDTH, height: HEIGHT }, deviceScaleFactor: 1 });
    }
    try {
        await ctx.addInitScript({ content: INIT_JS });
        const page = await ctx.newPage();
        await page.goto(`${url}/`, { waitUntil: 'load' });
        res.check(`${tag}: video decodes`, Boolean(await waitWrVideo(page, 45)));
        const first = await waitClean(page, 30);
        res.check(`${tag}: the decoded picture matches the frame it claims to be`, first !== null && !corrupt(first), first);
        if (!first) return;
        const sdp = await within(page.evaluate(() => {
            const out = { offer: false, answer: false };
            const dd = (d: RTCSessionDescription | null): boolean => d?.sdp.includes('dependency-descriptor-rtp-header-extension') ?? false;
            const pcs: unknown = Reflect.get(window, '__pcs');
            for (const pc of Array.isArray(pcs) ? pcs : []) {
                if (!(pc instanceof RTCPeerConnection)) continue;
                out.offer = out.offer || dd(pc.remoteDescription);
                out.answer = out.answer || dd(pc.localDescription);
            }
            return out;
        }));
        // Firefox's receiver implements no dependency descriptor, so it answers
        // without one; a frame dropped before it was sent leaves no gap for one
        // to bridge.
        const keeps = sdp.answer || ENGINE === 'firefox';
        res.check(`${tag}: the server offers the dependency descriptor and the browser keeps it`, sdp.offer && keeps, sdp);

        const before = await stats(page);
        const counter = async (name: string): Promise<number> => (await bridgeCounter(url, name)) ?? 0;
        const drops0 = await counter('dropped');
        const named0 = await counter('invalidated');
        const mark = (await host.log()).length;
        // Slowed, not stopped: a stall longer than the encoder's references
        // costs a keyframe however it is repaired.
        load = await host.load(threads, false);
        const samples: Sampled[] = [];
        let dropped = drops0;
        let named = named0;
        const bursts: [number, number][] = [];
        for (const end = Date.now() + LOAD_S * 1000; Date.now() < end;) {
            await load.resume();
            // Timed on its own: a sample still running when the burst should
            // end would stretch it past those references, and a broken
            // reference shows in every picture after it, which the samples
            // between bursts see.
            await sleep(BURST_ON_S * 1000);
            await load.pause();
            samples.push(...await sampleFor(page, BURST_OFF_S));
            const droppedNow = await counter('dropped');
            const namedNow = await counter('invalidated');
            if (namedNow > named) {
                bursts.push([droppedNow - dropped, namedNow - named]);
            } else if (droppedNow > dropped && bursts.length) {
                // Held behind a loss named earlier: the burst before goes on.
                const last = bursts[bursts.length - 1];
                bursts[bursts.length - 1] = [last[0] + droppedNow - dropped, last[1]];
            }
            dropped = droppedNow;
            named = namedNow;
        }
        await load.release();
        load = null;
        samples.push(...await sampleFor(page, SETTLE_S));
        const after = await stats(page);
        const log = await host.log();
        report(res, tag, {
            samples,
            dropped: (await counter('dropped')) - drops0,
            named: (await counter('invalidated')) - named0,
            bursts,
            encoders: logSince(SETTINGS, log, mark),
            ranges: frameNumRanges(log, mark),
            keyframes: after.keyframes - before.keyframes,
            plis: after.plis - before.plis,
            frames: after.frames - before.frames,
        });
    } finally {
        await load?.release();
        await closeBrowser(browser ?? ctx);
        painter.kill('SIGKILL');
        await host.stop();
    }
}

test('reference_invalidation', async () => {
    const res = new Checks('reference-invalidation');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        await webrtc(res, host, process.env.E2E_DISPLAY ?? '');
    } finally {
        await host.close();
    }
    res.finish();
});
