/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The client's ack heartbeat on a still screen, in each engine, on both ack
// paths (turbo off).
//
// A damage-gated capture sends nothing while the screen is still, and the
// server reads a frame left unanswered for seconds as a stalled client, so the
// client repeats its last ack about once a second: that is what tells an idle
// client from a dead one. The socket worker sends those acks, which a
// page-side hook never sees, so the page reaches the server through a tap
// (tests/lib/wstap.ts) and the acks are counted on the wire. Full frames
// (h264enc) ack on receipt and JPEG acks what the video worker presented;
// both are covered, and after the still period the stream must resume with
// the first change on the screen. WebKit decodes full-frame H.264 only after
// the client's software-decode retry, so that case is seeded with the
// persisted preference rather than driven through the fallback ladder's
// reload.
//
// Uses E2E_DISPLAY when set; otherwise starts a private Xvfb.
import { createServer, type AddressInfo } from 'node:net';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { FF_E2E_PROFILE, firefoxPersistentContext, launchBrowser, openh264Version, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, type Held } from '../lib/host.ts';
import { waitWsVideo, within } from '../lib/stream.ts';
import { WsTap } from '../lib/wstap.ts';

// The software-decode preference the client keeps for its tab.
const SOFTWARE_DECODE_JS = "try { sessionStorage.setItem((location.origin + location.pathname)"
    + ".replace(/[^a-zA-Z0-9._-]/g, '_') + '_prefer_software_decode', '1'); } catch (e) {}";

declare global {
    interface Window {
        videoChunksReceived?: number;
        videoDivertOn?: boolean;
    }
}

/** A port nothing listens on now. */
async function freePort(): Promise<number> {
    const server = createServer();
    await new Promise<void>((resolve) => { server.listen(0, '127.0.0.1', () => { resolve(); }); });
    const address: AddressInfo | string | null = server.address();
    await new Promise<void>((resolve) => { server.close(() => { resolve(); }); });
    return address !== null && typeof address === 'object' ? address.port : 0;
}

/**
 * The frame acks the tap took from `mark` on, as [time, frame id], how many
 * times the page connected anew meanwhile (a reload), and how many keyframes
 * it asked for.
 */
function acksSince(tap: WsTap, mark: number): [[number, string][], number, number] {
    const out: [number, string][] = [];
    let reloads = 0;
    let requests = 0;
    for (const { at, text } of tap.messages.slice(mark)) {
        if (text.startsWith('CLIENT_FRAME_ACK')) out.push([at, text.split(/\s+/)[1] ?? '']);
        else if (text.startsWith('SETTINGS,')) reloads++;
        else if (text.startsWith('REQUEST_KEYFRAME')) requests++;
    }
    return [out, reloads, requests];
}

/**
 * Whether every frame ack the tap took from `mark` on says the page's clock as
 * the ack left and the bytes its socket had handed over by then, a count that
 * never falls on one connection; how many acks there were, and the first that
 * does not.
 */
function acksCounted(tap: WsTap, mark: number): [boolean, number, string] {
    let last = -1;
    let acks = 0;
    for (const { text } of tap.messages.slice(mark)) {
        if (text.startsWith('SETTINGS,')) last = -1;
        if (!text.startsWith('CLIENT_FRAME_ACK')) continue;
        const [, , , sent, received] = text.split(/\s+/);
        const count = /^\d+$/.test(received ?? '') ? Number(received) : NaN;
        if (!(Number(sent) > 0) || !(count >= last)) return [false, acks, text];
        last = count;
        acks += 1;
    }
    return [acks > 0, acks, ''];
}

/**
 * The heartbeat among the acks, as the times of its beats, and the longest run of acks
 * that are not: the page re-acks a frame on every tick for a second after it arrives (each
 * ack carries the page's clock and its count for the server's late-path gauge), and about
 * once a second after that, which is the heartbeat. An ack is a beat where none came in
 * the half second before it, the window's first where none follows it within one; the rest
 * are a fresh frame's, wherever in the window that frame arrived.
 */
function heartbeat(acks: readonly [number, string][]): [number[], number] {
    const beats: number[] = [];
    let [run, most] = [0, 0];
    acks.forEach(([at], i) => {
        const alone = i > 0 ? at - acks[i - 1][0] >= 0.5 : acks.length < 2 || acks[1][0] - at >= 0.5;
        if (alone) beats.push(at);
        run = alone ? 0 : run + 1;
        most = Math.max(most, run);
    });
    return [beats, most];
}

/** What to close and the page; Firefox runs on the persistent profile. */
async function openPage(engine: Engine, port: number, software: boolean): Promise<[Browser | BrowserContext, Page]> {
    let ctx: BrowserContext;
    let closer: Browser | BrowserContext;
    if (engine === 'firefox') {
        ctx = await firefoxPersistentContext({ viewport: { width: 1280, height: 720 } });
        closer = ctx;
    } else {
        const browser = await launchBrowser(engine);
        ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
        closer = browser;
    }
    await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
    if (software) await ctx.addInitScript({ content: SOFTWARE_DECODE_JS });
    const page = await ctx.newPage();
    await page.goto(`http://127.0.0.1:${port}/`, { waitUntil: 'load' });
    return [closer, page];
}

const median = (values: number[]): number | null => {
    if (!values.length) return null;
    const sorted = [...values].sort((a, b) => a - b);
    const mid = Math.floor(sorted.length / 2);
    return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
};

const chunks = (page: Page): Promise<number> => within(page.evaluate(() => window.videoChunksReceived ?? 0));

async function drive(res: Checks, host: Host, tap: WsTap, display: string, engine: Engine, encoder: string): Promise<void> {
    const tag = `[${engine}/${encoder}]`;
    if (engine === 'firefox' && encoder === 'h264enc' && !openh264Version()) {
        res.skip(`${tag} no OpenH264 GMP plugin in ${FF_E2E_PROFILE}`, 'run tests/tools/fetch-openh264.sh to cover H.264 in Firefox');
        return;
    }
    let churn: Held | null = await host.churn(900, 700, display);
    let closer: Browser | BrowserContext;
    let page: Page;
    try {
        [closer, page] = await openPage(engine, tap.port, engine === 'webkit' && encoder === 'h264enc');
    } catch (e) {
        res.skip(`${tag} engine unavailable`, String(e).slice(0, 120));
        await churn.release();
        return;
    }
    try {
        res.check(`${tag} video flows`, Boolean(await waitWsVideo(page, 60)), '');
        const motionMark = tap.messages.length;
        await sleep(3000);
        const divert = await within(page.evaluate(() => Boolean(window.videoDivertOn)));
        // A decoder that keeps up acks a stream of new ids; one that does not
        // (Playwright's WebKit on full-frame H.264) leaves frames unacked, and
        // only the heartbeat and the stall verdict mean anything there.
        const moving = new Set(acksSince(tap, motionMark)[0].map(([, id]) => id)).size;
        const decodes = moving >= 40;
        const serverMark = await host.mark();
        await churn.release();
        churn = null;
        // The paint-over burst that follows the last change has to be over.
        await sleep(2500);
        const mark = tap.messages.length;
        const chunks0 = await chunks(page);
        await sleep(8000);
        const [acks, reloads, requests] = acksSince(tap, mark);
        const chunks1 = await chunks(page);
        const ids = new Set(acks.map(([, id]) => id));
        // A chunk that lands in the window (the one the check below allows, or a key frame
        // a decoder asked for) is re-acked on every tick for its first second: 20 acks,
        // which are not the heartbeat's, and cost it a beat.
        const [beats, most] = heartbeat(acks);
        const gaps = beats.slice(1).map((t, i) => t - beats[i]);
        const mid = median(gaps);
        // A decoder that keeps asking for keyframes is sent them; anything
        // beyond those is the capture sending on its own.
        if (decodes) {
            res.check(`${tag} the still screen sends nothing unasked and the page stays up`,
                reloads === 0 && chunks1 - chunks0 >= 0 && chunks1 - chunks0 <= requests + 1,
                `${chunks1 - chunks0} chunks in 8 s, ${requests} keyframe requests, ${reloads} reloads`);
        } else {
            res.skip(`${tag} the still screen sends nothing unasked`, `the decoder acked ${moving} distinct ids in 3 s of motion, so what it is sent says nothing`);
        }
        res.check(`${tag} an unchanged id is re-acked about once a second (worker divert=${divert ? 'True' : 'False'})`,
            beats.length >= 7 - ids.size && beats.length <= 10 && ids.size <= 2 && most <= 25 && mid !== null && mid >= 0.8 && mid <= 1.3,
            `${beats.length} beats of ${acks.length} acks, ids=${JSON.stringify([...ids].sort())}, median gap=${mid === null ? null : Math.round(mid * 100) / 100} s, ${most} acks of a fresh id`);
        // The server reads what its path holds of the stream off these, on either ack path.
        const [counted, counts, odd] = acksCounted(tap, motionMark);
        res.check(`${tag} every ack says the page's clock and what its socket has handed over`, counted,
            counted ? `${counts} acks` : `after ${counts} acks: ${odd.slice(0, 80)}`);
        const stalls = (await host.log(serverMark)).split('Client stall for').length - 1;
        res.check(`${tag} the server declares no stall`, stalls === 0, `${stalls} stalls`);
        if (!decodes) {
            res.skip(`${tag} the first change on the screen resumes the stream`, 'same decoder');
            return;
        }
        churn = await host.churn(900, 700, display);
        const t0 = Date.now();
        let first: number | null = null;
        while (Date.now() - t0 < 5000) {
            if ((await chunks(page)) > chunks1 + 2) {
                first = (Date.now() - t0) / 1000;
                break;
            }
            await sleep(100);
        }
        res.check(`${tag} the first change on the screen resumes the stream`, first !== null && first < 2,
            `after ${first === null ? null : Math.round(first * 100) / 100} s`);
    } finally {
        await closer.close();
        await churn?.release();
    }
}

for (const engine of ['chromium', 'firefox', 'webkit'] as const) {
    test(engine, async () => {
        const res = new Checks('ack-heartbeat');
        const host = new Host(suiteOf(import.meta.filename));
        let tap: WsTap | null = null;
        try {
            const display = process.env.E2E_DISPLAY || (await host.xvfb()).display;
            const port = await freePort();
            tap = await WsTap.start(port);
            for (const encoder of ['jpeg', 'h264enc']) {
                await host.start({ label: encoder, mode: 'websockets', display, port, env: { SELKIES_USE_CPU: 'true', SELKIES_VIDEO_STREAMING_MODE: 'false', SELKIES_ENCODER: encoder } });
                await drive(res, host, tap, display, engine, encoder);
                await host.stop();
            }
        } finally {
            await tap?.stop();
            await host.close();
        }
        res.finish();
    });
}
