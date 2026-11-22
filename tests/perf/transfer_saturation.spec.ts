/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// File-transfer saturation contracts on a shaped loopback link.
//
// Topology per scenario: the streaming page and the transfer client reach the
// server through the bwrelay program, whose token buckets are the bottleneck
// link. The relay bounds the receive buffer toward each receiver so the
// standing queue lives in the modeled path; loopback autotuning would
// otherwise hide it from the server's congestion gauge. The relay is a rate
// model: ACK-latency coupling between the directions only exists on a
// genuinely shaped link, so the contracts here are rate caps and fps floors,
// not latency assertions.
//
// Scenarios:
//   download-cc      6 Mbit/s downlink, adaptive pacing only: the stream must
//                    keep flowing while a download shares the link.
//   download-fat     the same behind a fat first-hop buffer (a consumer modem
//                    holding seconds of data): the gauge backs the download
//                    off as the session's round trip inflates and grows it
//                    back as the queue drains, so the transfer still takes the
//                    link's spare capacity beside the stream.
//   download-static  6 Mbit/s downlink, 3 Mbit/s static cap: the cap is
//                    honored and the stream stays clean.
//   upload-static    6 Mbit/s uplink, 3 Mbit/s cap: upload reads pace the
//                    client down to the cap, and the stream beside it loses
//                    no frame to the gate and none of its rate to the steer.
//   upload-auto      6 Mbit/s uplink behind a fat first-hop buffer, no cap:
//                    the buffer is never filled, so a small request beside
//                    the upload crosses near its unloaded latency instead of
//                    waiting it out, and the stream's acks come back as they
//                    did: no frame let go, no rate cut.
//   upload-long      the same for 80 MiB, two minutes: through the pacer's
//                    rises and backoffs the upload has the link it does not
//                    share, and the small requests and the stream are as
//                    beside none.
//   upload-narrow    6 Mbit/s each way: the stream fills its downlink and its
//                    steer keeps a queue standing there, which is none of the
//                    upload's: the upload has its uplink all the same.
//
// A receiver holds a sender by its window and by nothing else: what the
// sender may have on its way is what the receiver has acknowledged plus what
// it advertises. The relay ends each connection twice, so it admits a
// connection's bytes no further than its far end's own window, as the path
// between them would: a server that sizes an upload's window to its allowance
// bounds what stands in the client's first hop, whatever that hop would hold,
// and one that advertises megabytes has them fill it. The relay's side of the
// hop to the server announces an Ethernet segment size, since a loopback's
// 64 KiB one starts a receive window at a megabyte.

import { spawn, type ChildProcess } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, openSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host, suitesBin } from '../lib/host.ts';
import { waitWsVideo } from '../lib/stream.ts';

const RELAY_PORT = Number(process.env.E2E_SATURATION_PORT ?? '18215');
const BLOB_MB = 8;
/** The longer upload's blob: two minutes of the 6 Mbit/s uplink. */
const LONG_MB = 80;
const BASE_URL = `http://127.0.0.1:${RELAY_PORT}`;

declare global {
    interface Window {
        fps?: number;
    }
}

type Transfer = (root: string) => Promise<[number, number]>;

/** What the server logs of a transfer it served, as it starts or ends. */
const TRANSFER_LINE = /Download '|HTTP upload finished/;
/** The steer's log line for a cut of the video's rate on a queue it read. */
const STEER_CUT = /video bitrate \d+ -> \d+ kbps \(round trip/g;

/** A program's exit, without blocking the samples taken meanwhile. */
function exited(child: ChildProcess): Promise<number | null> {
    return new Promise((resolve) => {
        if (child.exitCode !== null) {
            resolve(child.exitCode);
            return;
        }
        child.once('exit', (code) => { resolve(code); });
    });
}

/**
 * Sustained but light screen damage: the video must genuinely use the link
 * without demanding all of it, or a 6 Mbit/s scenario has no headroom for the
 * transfer under test and measures only its own saturation.
 */
function damageWindow(): ChildProcess {
    return spawn('glxgears', ['-geometry', '900x700+100+50'], {
        env: { ...process.env, DISPLAY: process.env.E2E_DISPLAY ?? '' },
        stdio: 'ignore',
    });
}

/** How long a small request takes right now, in milliseconds; NaN when it fails. */
async function smallRequestMs(): Promise<number> {
    const t0 = performance.now();
    try {
        const r = await fetch(`${BASE_URL}/api/health`, { signal: AbortSignal.timeout(30_000) });
        await r.arrayBuffer();
        return r.status === 200 ? performance.now() - t0 : NaN;
    } catch {
        return NaN;
    }
}

/** Mbit/s and seconds of one curl transfer of a blob of `megabytes`; 0 Mbit/s when curl fails. */
async function timedCurl(args: string[], megabytes = BLOB_MB): Promise<[number, number]> {
    const t0 = performance.now();
    const child = spawn('curl', ['-sS', '-o', '/dev/null', ...args], { stdio: 'ignore' });
    // Ended after three minutes, or where a longer blob is still short of 2 Mbit/s.
    const timer = setTimeout(() => { child.kill('SIGKILL'); }, Math.max(180_000, megabytes * 4_000));
    const rc = await exited(child);
    clearTimeout(timer);
    const dt = (performance.now() - t0) / 1000;
    return [rc === 0 ? (megabytes * 8) / dt : 0, dt];
}

function median(values: number[]): number {
    const s = [...values].sort((a, b) => a - b);
    if (!s.length) return NaN;
    const m = s.length >> 1;
    return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
}

/** The median fps and the share of stalled samples, ends of the run left out. */
function fpsHealth(samples: (number | null)[]): [number, number] {
    const mid = samples.length > 4 ? samples.slice(2, -1) : samples;
    const stall = mid.filter((f) => !f || f < 10).length / Math.max(1, mid.length);
    return [mid.length ? median(mid.map((f) => f ?? 0)) : 0, stall];
}

/** The file a scenario's blob of `megabytes` is kept in. */
const blobName = (megabytes: number): string => (megabytes === BLOB_MB ? 'blob.bin' : `blob-${megabytes}.bin`);

const show = (samples: (number | null)[]): string => `[${samples.map((f) => (f === null ? 'None' : String(f))).join(', ')}]`;

/**
 * Boot server and relay, stream with churn, run `transfer(filesRoot)` while
 * sampling fps and small-request latency once a second, and count the times
 * the gate let frames go and the steer cut the video's rate over the transfer.
 * `rcvbuf` sizes the relay's modeled first-hop queue; the default models a
 * shallow hop. The transfer's blob is `megabytes` large (`blobName`).
 */
async function scenario(res: Checks, host: Host, tag: string, downKbit: number, upKbit: number, limitMbps: number,
    transfer: Transfer, rcvbuf = 0, megabytes = BLOB_MB): Promise<[[number, number], (number | null)[], number[], number, number]> {
    const filesRoot = join(WORKDIR, 'sat-files');
    mkdirSync(filesRoot, { recursive: true });
    const blob = join(filesRoot, blobName(megabytes));
    if (!existsSync(blob) || statSync(blob).size !== megabytes * 2 ** 20) writeFileSync(blob, randomBytes(megabytes * 2 ** 20));
    const { port } = await host.start({
        label: tag, mode: 'websockets', wayland: false,
        env: { SELKIES_FILE_TRANSFER_LIMIT_MBPS: String(limitMbps), FILE_MANAGER_PATH: filesRoot },
    });
    const relay = spawn(suitesBin('bwrelay'), [String(RELAY_PORT), String(port), String(downKbit), String(upKbit)], {
        env: rcvbuf ? { ...process.env, BWRELAY_RCVBUF: String(rcvbuf) } : process.env,
        // One log a case: the figures of each transfer's link are kept, not the last one's.
        stdio: ['ignore', openSync(join(WORKDIR, `bwrelay-${tag}.log`), 'w'), 'inherit'],
    });
    const churn = damageWindow();
    const fps: (number | null)[] = [];
    const latency: number[] = [];
    let result: [number, number] = [0, 0];
    let flushes: number;
    let cuts: number;
    try {
        for (let i = 0; i < 50 && Number.isNaN(await smallRequestMs()); i++) await sleep(200);
        const browser = await launchChromium();
        try {
            const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
            await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
            const page = await ctx.newPage();
            await page.goto(`${BASE_URL}/`, { waitUntil: 'load' });
            const info = await waitWsVideo(page);
            res.check(`${tag}: stream up`, info !== null, info);
            await page.waitForTimeout(3000);
            let done = false;
            const mark = await host.mark();
            const running = transfer(filesRoot).then((r) => { result = r; done = true; });
            while (!done) {
                fps.push(await page.evaluate(() => window.fps ?? null));
                latency.push(await smallRequestMs());
                await Promise.race([sleep(1000), running]);
                if (!done) await sleep(0);
            }
            await running;
            // The gate's line is the one crates/selkies-suites' stall_recovery counts as it
            // shutting, and the steer logs each cut of the video's rate with the round trip
            // it read. A log that does not hold the transfer's own line was not read over
            // the transfer, and counts as no reading rather than as no flush and no cut.
            const log = await host.log(mark);
            const read = TRANSFER_LINE.test(log);
            flushes = read ? log.split('Backpressure TRIGGERED').length - 1 : Number.NaN;
            cuts = read ? (log.match(STEER_CUT) ?? []).length : Number.NaN;
        } finally {
            await browser.close();
        }
    } finally {
        churn.kill('SIGTERM');
        relay.kill('SIGTERM');
        await host.stop();
    }
    return [result, fps, latency, flushes, cuts];
}

test('transfer_saturation', async () => {
    const res = new Checks('saturation');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        const dl: Transfer = () => timedCurl([`${BASE_URL}/api/files/blob.bin`]);

        // What a download has of the link is to be its own, not what a stream stalled beside
        // it leaves: each case tells how often the gate let frames go (`let go`).
        let [[mbps, dt], fps, , letGo] = await scenario(res, host, 'download-cc', 6000, 0, 0, dl);
        let [med, stall] = fpsHealth(fps);
        res.check('download-cc: download completes through the shared link', mbps > 1.0, `${mbps.toFixed(2)} Mbit/s in ${dt.toFixed(0)}s`);
        res.check('download-cc: stream keeps flowing beside the download', med >= 30 && stall <= 0.25,
            `median fps=${med} stall_frac=${stall.toFixed(2)} let go=${letGo} samples=${show(fps.slice(0, 12))}`);

        [[mbps, dt], fps, , letGo] = await scenario(res, host, 'download-fat', 6000, 0, 0, dl, 1024 * 1024);
        [med, stall] = fpsHealth(fps);
        res.check("download-fat: the download takes the link's spare capacity behind a fat buffer", mbps >= 2.0,
            `${mbps.toFixed(2)} Mbit/s in ${dt.toFixed(0)}s`);
        res.check('download-fat: stream keeps flowing beside it', med >= 30 && stall <= 0.25,
            `median fps=${med} stall_frac=${stall.toFixed(2)} let go=${letGo} samples=${show(fps.slice(0, 12))}`);

        [[mbps], fps, , letGo] = await scenario(res, host, 'download-static', 6000, 0, 3, dl);
        [med, stall] = fpsHealth(fps);
        res.check('download-static: static cap is honored', mbps >= 2.0 && mbps <= 3.8, `${mbps.toFixed(2)} Mbit/s`);
        res.check('download-static: stream stays clean under the cap', med >= 45 && stall <= 0.10,
            `median fps=${med} stall_frac=${stall.toFixed(2)} let go=${letGo}`);

        const up: Transfer = (root) => timedCurl(['-X', 'POST', '-H', 'X-Upload-Path: sat-upload.bin',
            '--data-binary', `@${join(root, 'blob.bin')}`, `${BASE_URL}/api/upload`]);

        const capped = await scenario(res, host, 'upload-static', 50000, 6000, 3, up);
        [[mbps], fps] = capped;
        const flushes = capped[3];
        [med, stall] = fpsHealth(fps);
        res.check('upload-static: upload reads pace the client to the cap', mbps >= 2.2 && mbps <= 3.8, `${mbps.toFixed(2)} Mbit/s`);
        res.check('upload-static: stream unaffected on the wide downlink', med >= 45 && stall <= 0.10,
            `median fps=${med} stall_frac=${stall.toFixed(2)}`);
        // The upload shares the page's way back and none of the stream's own path: nothing of
        // the stream is to be held, let go by the gate or cut by the steer beside it.
        res.check('upload-static: the gate lets no frame go beside the upload', flushes === 0, `${flushes} flushes`);
        res.check('upload-static: the steer cuts nothing beside the upload', capped[4] === 0, `${capped[4]} cuts`);

        // A fat first-hop buffer is what turns an upload into session-wide lag:
        // 1 MiB at 6 Mbit/s is well over a second of queue for anything the
        // client sends behind it, the page's acks among it.
        const [[autoMbps, autoDt], , lat, autoFlushes, autoCuts] = await scenario(res, host, 'upload-auto', 50000, 6000, 0, up, 1024 * 1024);
        const clean = lat.filter((v) => !Number.isNaN(v));
        const medianMs = median(clean);
        res.check('upload-auto: the upload completes through the buffered link', autoMbps > 1.0, `${autoMbps.toFixed(2)} Mbit/s in ${autoDt.toFixed(0)}s`);
        res.check('upload-auto: a small request beside it is not stuck behind the queue', medianMs < 250,
            `median=${Number.isNaN(medianMs) ? 'nan' : medianMs.toFixed(0)}ms samples=${clean.length} worst=${Math.max(...clean, 0).toFixed(0)}ms`);
        // The buffer is not filled, at the upload's start no more than later: the acks come
        // back as before it, and the gate has no silence to take for a path gone silent.
        res.check('upload-auto: the gate lets no frame go beside the upload', autoFlushes === 0, `${autoFlushes} flushes`);
        res.check('upload-auto: the steer cuts nothing beside the upload', autoCuts === 0, `${autoCuts} cuts`);

        // Longer than the pacer takes to find the link and back off from it a few times: what
        // an upload costs the session is read over its course, not its first seconds.
        const long: Transfer = (root) => timedCurl(['-X', 'POST', '-H', 'X-Upload-Path: sat-upload-long.bin',
            '--data-binary', `@${join(root, blobName(LONG_MB))}`, `${BASE_URL}/api/upload`], LONG_MB);
        const [[longMbps, longDt], , longLat, longFlushes, longCuts] = await scenario(res, host, 'upload-long', 50000, 6000, 0, long, 1024 * 1024, LONG_MB);
        const beside = longLat.filter((v) => !Number.isNaN(v));
        const stuck = beside.filter((v) => v >= 250).length;
        res.check('upload-long: the upload has the link it does not share', longMbps >= 4.0, `${longMbps.toFixed(2)} Mbit/s in ${longDt.toFixed(0)}s`);
        res.check('upload-long: small requests beside it cross as beside none', beside.length > 0 && stuck * 10 <= beside.length,
            `${stuck} of ${beside.length} at 250 ms or over, median=${median(beside).toFixed(0)}ms worst=${Math.max(...beside, 0).toFixed(0)}ms`);
        res.check('upload-long: the gate lets no frame go beside the upload', longFlushes === 0, `${longFlushes} flushes`);
        res.check('upload-long: the steer cuts nothing beside the upload', longCuts === 0, `${longCuts} cuts`);

        // A stream that fills its downlink stands a queue there by its steer's own work,
        // and a ping's round trip carries it: the upload is read against its own way, the
        // uplink, which it has to itself here.
        const [[narrowMbps, narrowDt], narrowFps] = await scenario(res, host, 'upload-narrow', 6000, 6000, 0, up);
        const [narrowMed, narrowStall] = fpsHealth(narrowFps);
        res.check('upload-narrow: the upload has its uplink beside a stream that fills its downlink', narrowMbps >= 4.0,
            `${narrowMbps.toFixed(2)} Mbit/s in ${narrowDt.toFixed(0)}s`);
        res.check('upload-narrow: stream keeps flowing beside the upload', narrowMed >= 30 && narrowStall <= 0.25,
            `median fps=${narrowMed} stall_frac=${narrowStall.toFixed(2)}`);
    } finally {
        await host.close();
    }
    res.finish();
});
