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
//                    client down to the cap.
//   upload-auto      6 Mbit/s uplink behind a fat first-hop buffer, no cap: a
//                    small request beside the upload crosses near its unloaded
//                    latency instead of waiting out the buffer.
//
// The spare-capacity contract lives on the download side because only there
// does the model bound the receiver: the server's writes block at the shaped
// link. An upload's receiver is the server's own socket, whose buffer loopback
// autotunes to megabytes and the relay cannot bound, so the sender never
// slows and the wall time measures the pacer's climb back from a queue it
// could not touch rather than the link. The same pacer serves both
// directions, so the download scenarios are the measure of it; upload-auto
// keeps the latency contract and a completion floor.

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
const BASE_URL = `http://127.0.0.1:${RELAY_PORT}`;

declare global {
    interface Window {
        fps?: number;
    }
}

type Transfer = (root: string) => Promise<[number, number]>;

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

/** Mbit/s and seconds of one curl transfer of the blob; 0 Mbit/s when curl fails. */
async function timedCurl(args: string[]): Promise<[number, number]> {
    const t0 = performance.now();
    const child = spawn('curl', ['-sS', '-o', '/dev/null', ...args], { stdio: 'ignore' });
    const timer = setTimeout(() => { child.kill('SIGKILL'); }, 180_000);
    const rc = await exited(child);
    clearTimeout(timer);
    const dt = (performance.now() - t0) / 1000;
    return [rc === 0 ? (BLOB_MB * 8) / dt : 0, dt];
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

const show = (samples: (number | null)[]): string => `[${samples.map((f) => (f === null ? 'None' : String(f))).join(', ')}]`;

/**
 * Boot server and relay, stream with churn, run `transfer(filesRoot)` while
 * sampling fps and small-request latency once a second. `rcvbuf` sizes the
 * relay's modeled first-hop queue; the default models a shallow hop.
 */
async function scenario(res: Checks, host: Host, tag: string, downKbit: number, upKbit: number, limitMbps: number,
    transfer: Transfer, rcvbuf = 0): Promise<[[number, number], (number | null)[], number[]]> {
    const filesRoot = join(WORKDIR, 'sat-files');
    mkdirSync(filesRoot, { recursive: true });
    const blob = join(filesRoot, 'blob.bin');
    if (!existsSync(blob) || statSync(blob).size !== BLOB_MB * 2 ** 20) writeFileSync(blob, randomBytes(BLOB_MB * 2 ** 20));
    const { port } = await host.start({
        label: tag, mode: 'websockets', wayland: false,
        env: { SELKIES_FILE_TRANSFER_LIMIT_MBPS: String(limitMbps), FILE_MANAGER_PATH: filesRoot },
    });
    const relay = spawn(suitesBin('bwrelay'), [String(RELAY_PORT), String(port), String(downKbit), String(upKbit)], {
        env: rcvbuf ? { ...process.env, BWRELAY_RCVBUF: String(rcvbuf) } : process.env,
        stdio: ['ignore', openSync(join(WORKDIR, 'bwrelay.log'), 'w'), 'inherit'],
    });
    const churn = damageWindow();
    const fps: (number | null)[] = [];
    const latency: number[] = [];
    let result: [number, number] = [0, 0];
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
            const running = transfer(filesRoot).then((r) => { result = r; done = true; });
            while (!done) {
                fps.push(await page.evaluate(() => window.fps ?? null));
                latency.push(await smallRequestMs());
                await Promise.race([sleep(1000), running]);
                if (!done) await sleep(0);
            }
            await running;
        } finally {
            await browser.close();
        }
    } finally {
        churn.kill('SIGTERM');
        relay.kill('SIGTERM');
        await host.stop();
    }
    return [result, fps, latency];
}

test('transfer_saturation', async () => {
    const res = new Checks('saturation');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        const dl: Transfer = () => timedCurl([`${BASE_URL}/api/files/blob.bin`]);

        let [[mbps, dt], fps] = await scenario(res, host, 'download-cc', 6000, 0, 0, dl);
        let [med, stall] = fpsHealth(fps);
        res.check('download-cc: download completes through the shared link', mbps > 1.0, `${mbps.toFixed(2)} Mbit/s in ${dt.toFixed(0)}s`);
        res.check('download-cc: stream keeps flowing beside the download', med >= 30 && stall <= 0.25,
            `median fps=${med} stall_frac=${stall.toFixed(2)} samples=${show(fps.slice(0, 12))}`);

        [[mbps, dt], fps] = await scenario(res, host, 'download-fat', 6000, 0, 0, dl, 1024 * 1024);
        [med, stall] = fpsHealth(fps);
        res.check("download-fat: the download takes the link's spare capacity behind a fat buffer", mbps >= 2.0,
            `${mbps.toFixed(2)} Mbit/s in ${dt.toFixed(0)}s`);
        res.check('download-fat: stream keeps flowing beside it', med >= 30 && stall <= 0.25,
            `median fps=${med} stall_frac=${stall.toFixed(2)} samples=${show(fps.slice(0, 12))}`);

        [[mbps], fps] = await scenario(res, host, 'download-static', 6000, 0, 3, dl);
        [med, stall] = fpsHealth(fps);
        res.check('download-static: static cap is honored', mbps >= 2.0 && mbps <= 3.8, `${mbps.toFixed(2)} Mbit/s`);
        res.check('download-static: stream stays clean under the cap', med >= 45 && stall <= 0.10,
            `median fps=${med} stall_frac=${stall.toFixed(2)}`);

        const up: Transfer = (root) => timedCurl(['-X', 'POST', '-H', 'X-Upload-Path: sat-upload.bin',
            '--data-binary', `@${join(root, 'blob.bin')}`, `${BASE_URL}/api/upload`]);

        [[mbps], fps] = await scenario(res, host, 'upload-static', 50000, 6000, 3, up);
        [med, stall] = fpsHealth(fps);
        res.check('upload-static: upload reads pace the client to the cap', mbps >= 2.2 && mbps <= 3.8, `${mbps.toFixed(2)} Mbit/s`);
        res.check('upload-static: stream unaffected on the wide downlink', med >= 45 && stall <= 0.10,
            `median fps=${med} stall_frac=${stall.toFixed(2)}`);

        // A fat first-hop buffer is what turns an upload into session-wide lag:
        // 1 MiB at 6 Mbit/s is well over a second of queue for anything the
        // client sends behind it.
        const [[autoMbps, autoDt], , lat] = await scenario(res, host, 'upload-auto', 50000, 6000, 0, up, 1024 * 1024);
        const clean = lat.filter((v) => !Number.isNaN(v));
        const medianMs = median(clean);
        res.check('upload-auto: the upload completes through the buffered link', autoMbps > 1.0, `${autoMbps.toFixed(2)} Mbit/s in ${autoDt.toFixed(0)}s`);
        res.check('upload-auto: a small request beside it is not stuck behind the queue', medianMs < 250,
            `median=${Number.isNaN(medianMs) ? 'nan' : medianMs.toFixed(0)}ms samples=${clean.length}`);
    } finally {
        await host.close();
    }
    res.finish();
});
