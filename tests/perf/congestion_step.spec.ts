/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A congested stretch and its recovery, as the user hears and sees them.
//
// A page streams a moving scene and a steady tone through a link that runs
// fast, drops to a fraction of the stream for a few seconds, and comes back,
// sampled in the page every 100 ms across the step and read per phase
// (before, during, the three seconds after recovery, and the rest).
//
// `websockets`: through a metered TCP relay (bwrelay, its rate stepped on its
// stdin), with the socket on the page, where every message it delivers is
// timed (its type byte and size), and the player's queue depth, the packets
// it dropped from its head and the silence it filled in. Read: how audio
// arrived (its rate, its longest gap, its densest 100 ms), what the player
// dropped and how deep it ran, and the gaps in audio that end with a large
// video message ahead of it (audio held behind video on the socket).
//
// `webrtc` and `webrtc-split`: Chromium behind UDP relays whose downlink rate
// steps, read from getStats: the audio the jitter buffer sped up (removed for
// acceleration) or stretched, what it concealed and how deep it ran, and the
// video frames decoded per 100 ms, summed over audio's receive streams (the
// server moves audio to a new SSRC once a queue drains). `webrtc-split` gives audio a stream id of
// its own and plays it from an element of its own, out of video's lip-sync
// group, which tells that group's share in what the recovery speeds up.
//
// Low latency means a recovery plays no backlog (#620): the player is not left
// to shed delay one packet per arrival, nor the jitter buffer to time-compress
// it, which is heard as a fast-forward, nor left holding the delay the queue
// taught it once the queue is gone, and frames held during the congestion
// are not shown back to back. The checks ask that of the recovery; the numbers
// are written out beside them (congestion-step-*.json in the work directory)
// for the runs that change it.
//
// Over WebSockets a second step follows with the scene turned light as the
// link returns, to a rate under the configured one: a light screen sends far
// under its cut target, so the server pads the stream to learn what the
// returned link carries, and the picture is not to age behind that padding.
// The scene's frame clock shows its age (the page reads each frame's index
// off the picture, lib/scene.ts).
//
// CONGESTION_STEP_HIGH_KBIT and _LOW_KBIT set the link (20000 and 1500),
// CONGESTION_STEP_LOW_S how long it stays low (5), CONGESTION_STEP_BACK_KBIT
// what it returns to (the high rate), CONGESTION_STEP_LIGHT_KBIT what the
// second step returns to (4000); SELKIES_ENCODER, SELKIES_VIDEO_BITRATE,
// SELKIES_CONGESTION_CONTROL and SELKIES_AUDIO_BITRATE pass to the server.
// For measuring how the rate comes back, over WebSockets:
// CONGESTION_STEP_NOISE is the height of the scene's noise band (a heavy
// scene that fills the rate); CONGESTION_STEP_RESUME_S has the scene turn
// light as the link returns (its noise band off, or with
// CONGESTION_STEP_STILL=1 no painting at all) and heavy again that many
// seconds later, for CONGESTION_STEP_RESUMED_S (20), and
// CONGESTION_STEP_GROW_KBIT has the link grow to that rate
// CONGESTION_STEP_GROW_AT_S (8) into the heavy stretch. The row then carries
// `resume`: the picture's age over the light stretch, the first five seconds
// of the heavy one and the rest, and the relay's rate each second.
import { spawn, type ChildProcess } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';

import type { Browser, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchBrowser } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { pulseSine, pulseUnload } from '../lib/desktop.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host, suitesBin } from '../lib/host.ts';
import { pictureAge, SHOWN_JS } from '../lib/scene.ts';
import { closeBrowser, waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';
import { makeRelay, SHAPED_JS, SteppedShaper, type Relay } from '../lib/udprelay.ts';

const WIDTH = 1280;
const HEIGHT = 720;
const RELAY_PORT = Number(process.env.E2E_CONGESTION_STEP_PORT ?? '18215');
const HIGH_KBIT = Number(process.env.CONGESTION_STEP_HIGH_KBIT ?? '20000');
const LOW_KBIT = Number(process.env.CONGESTION_STEP_LOW_KBIT ?? '1500');
const LOW_S = Number(process.env.CONGESTION_STEP_LOW_S ?? '5');
const BACK_KBIT = Number(process.env.CONGESTION_STEP_BACK_KBIT ?? String(HIGH_KBIT));
const NOISE = process.env.CONGESTION_STEP_NOISE;
const RESUME_S = Number(process.env.CONGESTION_STEP_RESUME_S ?? '0');
const RESUMED_S = Number(process.env.CONGESTION_STEP_RESUMED_S ?? '20');
const STILL = process.env.CONGESTION_STEP_STILL === '1';
const GROW_KBIT = Number(process.env.CONGESTION_STEP_GROW_KBIT ?? '0');
const GROW_AT_S = Number(process.env.CONGESTION_STEP_GROW_AT_S ?? '8');
/** The second step: the link returns to this, under the configured rate, for this long, the scene light. */
const LIGHT_KBIT = Number(process.env.CONGESTION_STEP_LIGHT_KBIT ?? '4000');
const LIGHT_S = 20;
/** The oldest picture a light screen may show while the server sounds the returned link. */
const LIGHT_AGE_MS = 250;
/** The narrowed link's own queue drains first: the light screen is read from here on. */
const DRAINED_MS = 5000;
const SETTLE_S = 10;
const BEFORE_S = 10;
const AFTER_S = 15;
/** The recovery read on its own: what the link coming back delivers first. */
const RECOVERY_MS = 3000;
/** One Opus frame at the server's default. */
const FRAME_MS = 10;
/** A video message this large holds the socket for a while at the low rate. */
const LARGE = 32 * 1024;
const AUDIO = 0x01;
const PASSED = ['SELKIES_ENCODER', 'SELKIES_VIDEO_BITRATE', 'SELKIES_CONGESTION_CONTROL', 'SELKIES_AUDIO_BITRATE'];
const ENV: Record<string, string> = {
    SELKIES_ENCODER: 'h264enc',
    SELKIES_VIDEO_BITRATE: '8000',
    ...Object.fromEntries(PASSED.flatMap((k) => (process.env[k] ? [[k, process.env[k]]] : []))),
};

// The socket stays on the page (`?socket_worker=false`), where every message
// it delivers is timed; the player's stats are asked for every 100 ms on the
// port the page itself reads them from.
const TAP_JS = `(() => {
  const arrivals = [];
  window.__arrivals = arrivals;
  const OrigSocket = window.WebSocket;
  window.WebSocket = class extends OrigSocket {
    constructor(...args) {
      super(...args);
      this.addEventListener('message', (e) => {
        const d = e.data;
        if (d instanceof ArrayBuffer && d.byteLength) arrivals.push([performance.now(), new Uint8Array(d)[0], d.byteLength]);
        else if (d instanceof Blob) arrivals.push([performance.now(), -1, d.size]);
      });
    }
  };
  const OrigNode = window.AudioWorkletNode;
  if (OrigNode) {
    window.AudioWorkletNode = class extends OrigNode {
      constructor(...args) {
        super(...args);
        if (args[1] === 'audio-frame-processor') window.__playerPort = this.port;
      }
    };
  }
  const samples = [];
  window.__samples = samples;
  setInterval(() => {
    window.__playerPort?.postMessage({ type: 'getBufferSize' });
    samples.push([performance.now(), window.currentAudioBufferDuration ?? null, window.currentAudioWorkletDropped ?? null,
                  window.currentAudioUnderrunSamples ?? null]);
  }, 100);
})();`;

type Arrival = [number, number, number];
type Sample = [number, number | null, number | null, number | null];

const num = (v: unknown): number | null => (typeof v === 'number' ? v : null);

/** The page's arrivals, as numbers. */
function asArrivals(v: unknown): Arrival[] {
    return Array.isArray(v) ? v.flatMap((a): Arrival[] => (Array.isArray(a) ? [[num(a[0]) ?? 0, num(a[1]) ?? -1, num(a[2]) ?? 0]] : [])) : [];
}

/** The page's samples, as numbers or nulls. */
function asSamples(v: unknown): Sample[] {
    return Array.isArray(v) ? v.flatMap((a): Sample[] => (Array.isArray(a) ? [[num(a[0]) ?? 0, num(a[1]), num(a[2]), num(a[3])]] : [])) : [];
}

interface Phase {
    audio: number;
    audioPerS: number;
    longestGapMs: number;
    densest100ms: number;
    dropped: number;
    deepestMs: number;
    underrunMs: number;
    gapsBehindVideo: number;
    videoBytes: number;
}

/** What one stretch of the run looked like, from `from` to `to` on the page's clock. */
function phase(arrivals: readonly Arrival[], samples: readonly Sample[], from: number, to: number, sampleRate = 48_000): Phase {
    const audio = arrivals.filter(([t, type]) => type === AUDIO && t >= from && t < to).map(([t]) => t);
    let longest = 0;
    let gapsBehindVideo = 0;
    for (let i = 1; i < audio.length; i += 1) {
        const gap = audio[i] - audio[i - 1];
        longest = Math.max(longest, gap);
        if (gap > 6 * FRAME_MS && arrivals.some(([t, type, size]) => type !== AUDIO && size >= LARGE && t >= audio[i - 1] && t <= audio[i])) gapsBehindVideo += 1;
    }
    let densest = 0;
    for (let i = 0, j = 0; i < audio.length; i += 1) {
        while (audio[i] - audio[j] > 100) j += 1;
        densest = Math.max(densest, i - j + 1);
    }
    const inside = samples.filter(([t]) => t >= from && t < to);
    const first = inside[0];
    const last = inside.at(-1);
    const delta = (k: 2 | 3): number => (first && last ? (last[k] ?? 0) - (first[k] ?? 0) : 0);
    return {
        audio: audio.length,
        audioPerS: Math.round((audio.length * 1000) / Math.max(1, to - from)),
        longestGapMs: Math.round(longest),
        densest100ms: densest,
        dropped: delta(2),
        deepestMs: Math.round(Math.max(0, ...inside.map(([, ms]) => ms ?? 0))),
        underrunMs: Math.round((delta(3) * 1000) / sampleRate),
        gapsBehindVideo,
        videoBytes: arrivals.filter(([t, type]) => type !== AUDIO && t >= from && t < to).reduce((n, [, , size]) => n + size, 0),
    };
}

interface Scene {
    child: ChildProcess;
    /** The scene's first frame in wall-clock ms and its frame interval, once it has printed them. */
    clock: { t0: number; interval: number } | null;
}

function startScene(): Scene {
    const args = [process.env.E2E_DISPLAY ?? '', String(WIDTH), String(HEIGHT), '60', ...(NOISE === undefined ? [] : [NOISE])];
    const child = spawn(suitesBin('motion-scene'), args, { stdio: ['pipe', 'pipe', 'ignore'] });
    const scene: Scene = { child, clock: null };
    child.stdout?.setEncoding('utf8');
    child.stdout?.on('data', (chunk: string) => {
        const m = /t0 ([\d.]+) interval (\d+)/.exec(chunk);
        if (m && !scene.clock) scene.clock = { t0: Number(m[1]), interval: Number(m[2]) };
    });
    return scene;
}

/** [page clock, wall clock, frame index or -1] */
type Shown = [number, number, number];

function asShown(v: unknown): Shown[] {
    return Array.isArray(v) ? v.flatMap((a): Shown[] => (Array.isArray(a) ? [[num(a[0]) ?? 0, num(a[1]) ?? 0, num(a[2]) ?? -1]] : [])) : [];
}

interface Age {
    /** The median age of the picture on screen, in ms. */
    p50Ms: number | null;
    /** The oldest it was. */
    maxMs: number | null;
    /** Samples whose picture could not be read. */
    unread: number;
}

/** How old the picture on screen was from `from` to `to` on the page's clock. */
function ageOf(shown: readonly Shown[], scene: Scene, from: number, to: number): Age {
    const inside = shown.filter(([t]) => t >= from && t < to);
    const { clock } = scene;
    const ages = clock
        ? inside.flatMap(([, wall, index]) => { const a = pictureAge(wall, index, clock.t0, clock.interval); return a === null ? [] : [a]; }).sort((a, b) => a - b)
        : [];
    return {
        p50Ms: ages.length ? Math.round(ages[Math.floor(ages.length / 2)]) : null,
        maxMs: ages.length ? Math.round(ages[ages.length - 1]) : null,
        unread: inside.filter(([, , index]) => index < 0).length,
    };
}

async function wsStep(res: Checks, host: Host): Promise<void> {
    const { port } = await host.start({ label: 'websockets', mode: 'websockets', env: ENV });
    const tone = pulseSine();
    const scene = startScene();
    const painter = scene.child;
    const relay = spawn(suitesBin('bwrelay'), [String(RELAY_PORT), String(port), String(HIGH_KBIT)], { stdio: ['pipe', 'pipe', 'ignore'] });
    const link: string[] = [];
    /** [Node's clock in s, downlink Mbit/s] each second the relay reports. */
    const rates: [number, number][] = [];
    relay.stdout.setEncoding('utf8');
    relay.stdout.on('data', (chunk: string) => {
        for (const l of chunk.split('\n').filter(Boolean)) {
            const at = performance.now() / 1000;
            link.push(`${at.toFixed(1)} ${l}`);
            const m = /down=([\d.]+) Mbit/.exec(l);
            if (m) rates.push([at, Number(m[1])]);
        }
    });
    let browser: Browser | null = null;
    try {
        browser = await launchBrowser('chromium');
        const ctx = await browser.newContext({ viewport: { width: WIDTH, height: HEIGHT }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
        await ctx.addInitScript({ content: TAP_JS });
        await ctx.addInitScript({ content: SHOWN_JS });
        const page: Page = await ctx.newPage();
        await page.goto(`http://127.0.0.1:${RELAY_PORT}/?socket_worker=false`, { waitUntil: 'load', timeout: 90_000 });
        res.check('the page streams the scene through the link', (await waitWsVideo(page, 90)) !== null);
        await sleep(SETTLE_S * 1000);
        const now = (): Promise<number> => within(page.evaluate(() => performance.now()));
        const t0 = await now();
        await sleep(BEFORE_S * 1000);
        relay.stdin.write(`down ${LOW_KBIT}\n`);
        const down = await now();
        await sleep(LOW_S * 1000);
        relay.stdin.write(`down ${BACK_KBIT}\n`);
        if (RESUME_S > 0) painter.stdin?.write(STILL ? 'noise off\nstill on\n' : 'noise off\n');
        const back = await now();
        await sleep((RESUME_S > 0 ? RESUME_S : AFTER_S) * 1000);
        const end = await now();
        let resume = null;
        if (RESUME_S > 0) {
            painter.stdin?.write('still off\nnoise on\n');
            const heavy = await now();
            const nodeHeavy = performance.now() / 1000;
            if (GROW_KBIT > 0) {
                await sleep(GROW_AT_S * 1000);
                relay.stdin.write(`down ${GROW_KBIT}\n`);
                await sleep((RESUMED_S - GROW_AT_S) * 1000);
            } else await sleep(RESUMED_S * 1000);
            const done = await now();
            const shownAll = asShown(await within(page.evaluate('window.__shown')));
            resume = {
                rates: rates.filter(([at]) => at >= nodeHeavy).map(([, r]) => r),
                light: ageOf(shownAll, scene, back, heavy),
                climb: ageOf(shownAll, scene, heavy, heavy + 5000),
                resumed: ageOf(shownAll, scene, heavy + 5000, done),
            };
        }
        let light = null;
        if (RESUME_S <= 0) {
            relay.stdin.write(`down ${LOW_KBIT}\n`);
            await sleep(LOW_S * 1000);
            relay.stdin.write(`down ${LIGHT_KBIT}\n`);
            painter.stdin?.write('noise off\n');
            const from = await now();
            await sleep(LIGHT_S * 1000);
            light = ageOf(asShown(await within(page.evaluate('window.__shown'))), scene, from + DRAINED_MS, await now());
        }
        const arrivals = asArrivals(await within(page.evaluate('window.__arrivals'))).filter(([t]) => t >= t0);
        const samples = asSamples(await within(page.evaluate('window.__samples'))).filter(([t]) => t >= t0);
        const shown = asShown(await within(page.evaluate('window.__shown'))).filter(([t]) => t >= t0);
        const age = {
            before: ageOf(shown, scene, t0, down),
            congested: ageOf(shown, scene, down, back),
            recovery: ageOf(shown, scene, back, back + DRAINED_MS),
            after: ageOf(shown, scene, back + DRAINED_MS, end),
        };
        const phases = {
            before: phase(arrivals, samples, t0, down),
            congested: phase(arrivals, samples, down, back),
            recovery: phase(arrivals, samples, back, back + RECOVERY_MS),
            after: phase(arrivals, samples, back + RECOVERY_MS, end),
        };
        const row = { link: { highKbit: HIGH_KBIT, lowKbit: LOW_KBIT, lowS: LOW_S, backKbit: BACK_KBIT }, noise: NOISE ?? null, phases, age, light, resume };
        process.stdout.write(`${JSON.stringify(row)}\n`);
        writeFileSync(join(WORKDIR, 'congestion-step-websockets.json'), JSON.stringify({ ...row, t0, down, back, end, arrivals, samples, shown: asShown(await within(page.evaluate('window.__shown'))), clock: scene.clock, link }, null, 0));
        const { before, congested, recovery } = phases;
        res.check('audio crosses the link at its rate before the step', before.audioPerS >= 0.8 * (1000 / FRAME_MS), before);
        res.check('audio keeps arriving while the link is narrow', congested.longestGapMs < 200, congested);
        res.check('the recovery delivers no audio backlog in a burst', recovery.densest100ms <= 2 * (100 / FRAME_MS), recovery);
        res.check('the player sheds no backlog packet by packet after the recovery', recovery.dropped <= 2, recovery);
        if (light) {
            res.check('a light screen shows no picture held behind the padding that sounds the returned link',
                light.maxMs !== null && light.maxMs <= LIGHT_AGE_MS, light);
        }
        await closeBrowser(browser);
        browser = null;
    } finally {
        if (browser) await closeBrowser(browser);
        relay.kill('SIGKILL');
        painter.kill('SIGKILL');
        pulseUnload(tone);
    }
}

// The page's receiving stats every 100 ms: the audio jitter buffer's
// time-scaling (what it removed to catch up, what it stretched), what it
// concealed and how deep it ran, and the video's decoded and dropped frames.
const STATS_JS = `(() => {
  const rows = [];
  window.__rtcStats = rows;
  setInterval(async () => {
    const pc = (window.__pcs || []).at(-1);
    if (!pc) return;
    const t = performance.now();
    const r = await pc.getStats();
    // Audio summed over its receive streams: a new SSRC is a new stream of the same track.
    const audio = [t, 0, 0, 0, 0, 0, 0, 0];
    let streams = 0;
    r.forEach((s) => {
      if (s.type !== 'inbound-rtp' || s.kind !== 'audio') return;
      streams += 1;
      [s.removedSamplesForAcceleration, s.insertedSamplesForDeceleration, s.concealedSamples, s.jitterBufferDelay,
        s.jitterBufferEmittedCount, s.jitterBufferTargetDelay].forEach((v, i) => { audio[i + 2] += v || 0; });
    });
    if (streams) rows.push(audio);
    r.forEach((s) => {
      if (s.type !== 'inbound-rtp') return;
      if (s.kind === 'video') rows.push([t, 1, s.framesDecoded || 0, s.framesDropped || 0, s.totalInterFrameDelay || 0, 0, 0, 0]);
    });
  }, 100);
})();`;

// Audio out of video's sync group: the description the page takes gives every
// audio section a stream id of its own, and the page plays each audio track
// from an element of its own, as the client would with separate streams.
const SPLIT_JS = `(() => {
  const PC = window.RTCPeerConnection.prototype;
  const setRemote = PC.setRemoteDescription;
  PC.setRemoteDescription = function (desc) {
    if (!this.__splitPlayed) {
      this.__splitPlayed = true;
      this.addEventListener('track', (e) => {
        if (e.track.kind !== 'audio') return;
        const el = new Audio();
        el.srcObject = new MediaStream([e.track]);
        el.play().catch(() => {});
      });
    }
    if (desc && desc.sdp) {
      let audio = false;
      const lines = desc.sdp.split('\\r\\n').map((l) => {
        if (l.startsWith('m=')) audio = l.startsWith('m=audio');
        return audio && l.startsWith('a=msid:') ? 'a=msid:split-audio ' + l.split(' ')[1] : l;
      });
      desc = { type: desc.type, sdp: lines.join('\\r\\n') };
    }
    return setRemote.call(this, desc);
  };
})();`;

type Stat = [number, number, number, number, number, number, number, number];

function asStats(v: unknown): Stat[] {
    return Array.isArray(v)
        ? v.flatMap((a): Stat[] => {
            if (!Array.isArray(a) || a.length !== 8) return [];
            const n = (i: number): number => num(a[i]) ?? 0;
            return [[n(0), n(1), n(2), n(3), n(4), n(5), n(6), n(7)]];
        })
        : [];
}

interface RtcPhase {
    audioEmitted: number;
    acceleratedMs: number;
    deceleratedMs: number;
    concealedMs: number;
    bufferMs: number;
    targetMs: number;
    videoFps: number;
    densestFrames100ms: number;
    videoDropped: number;
}

/** What one stretch looked like at the receiver, from `from` to `to` on the page's clock. */
function rtcPhase(stats: readonly Stat[], from: number, to: number): RtcPhase {
    const pick = (kind: number): Stat[] => stats.filter(([t, k]) => k === kind && t >= from && t < to);
    const audio = pick(0);
    const video = pick(1);
    // What a counter gained over the rows; a stream that leaves takes its counts out of the sum.
    const d = (rows: Stat[], i: number): number => rows.slice(1).reduce((sum, row, k) => sum + Math.max(0, row[i] - rows[k][i]), 0);
    const ms = (samples: number): number => Math.round((samples * 1000) / 48_000);
    const emitted = d(audio, 6);
    let densest = 0;
    for (let i = 1; i < video.length; i += 1) densest = Math.max(densest, video[i][2] - video[i - 1][2]);
    return {
        audioEmitted: emitted,
        acceleratedMs: ms(d(audio, 2)),
        deceleratedMs: ms(d(audio, 3)),
        concealedMs: ms(d(audio, 4)),
        bufferMs: emitted > 0 ? Math.round((d(audio, 5) * 1000) / emitted) : 0,
        targetMs: emitted > 0 ? Math.round((d(audio, 7) * 1000) / emitted) : 0,
        videoFps: Math.round((d(video, 2) * 1000) / Math.max(1, to - from)),
        densestFrames100ms: densest,
        videoDropped: d(video, 3),
    };
}

async function wrStep(res: Checks, host: Host, split: boolean): Promise<void> {
    const name = split ? 'webrtc-split' : 'webrtc';
    const up = new SteppedShaper(HIGH_KBIT * 1000, 0.015);
    const down = new SteppedShaper(HIGH_KBIT * 1000, 0.015);
    const relays: Relay[] = [];
    const { url } = await host.start({ label: name, mode: 'webrtc', env: { ...ENV, SELKIES_STUN_HOST: '', SELKIES_TURN_REST_URI: '' } });
    const tone = pulseSine();
    const painter: ChildProcess = spawn(suitesBin('motion-scene'), [process.env.E2E_DISPLAY ?? '', String(WIDTH), String(HEIGHT), '60'], { stdio: 'ignore' });
    let browser: Browser | null = null;
    try {
        browser = await launchBrowser('chromium');
        const ctx = await browser.newContext({ viewport: { width: WIDTH, height: HEIGHT }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'webrtc';" });
        await ctx.exposeFunction('__shapedRelay', async (address: string, port: number) => {
            const relay = await makeRelay([address, Number(port)], up, down);
            relays.push(relay);
            return relay.address;
        });
        await ctx.addInitScript({ content: SHAPED_JS });
        if (split) await ctx.addInitScript({ content: SPLIT_JS });
        await ctx.addInitScript({ content: STATS_JS });
        const page: Page = await ctx.newPage();
        await page.goto(`${url}/`, { waitUntil: 'load', timeout: 90_000 });
        res.check('the page streams the scene through the link', Boolean(await waitWrVideo(page, 90)));
        await sleep(SETTLE_S * 1000);
        const now = (): Promise<number> => within(page.evaluate(() => performance.now()));
        const t0 = await now();
        await sleep(BEFORE_S * 1000);
        down.rate = LOW_KBIT * 1000;
        const narrowed = await now();
        await sleep(LOW_S * 1000);
        down.rate = HIGH_KBIT * 1000;
        const back = await now();
        await sleep(AFTER_S * 1000);
        const end = await now();
        const stats = asStats(await within(page.evaluate('window.__rtcStats'))).filter(([t]) => t >= t0);
        const phases = {
            before: rtcPhase(stats, t0, narrowed),
            congested: rtcPhase(stats, narrowed, back),
            recovery: rtcPhase(stats, back, back + RECOVERY_MS),
            after: rtcPhase(stats, back + RECOVERY_MS, end),
        };
        const row = { link: { highKbit: HIGH_KBIT, lowKbit: LOW_KBIT, lowS: LOW_S }, split, dropped: down.dropped, phases };
        process.stdout.write(`${JSON.stringify(row)}\n`);
        writeFileSync(join(WORKDIR, `congestion-step-${name}.json`), JSON.stringify({ ...row, t0, narrowed, back, end, stats }, null, 0));
        const { before, recovery, after } = phases;
        res.check('video and audio are received before the step', before.videoFps > 10 && before.audioEmitted > 0, before);
        res.check('the recovery plays no audio sped up', recovery.acceleratedMs <= 20, recovery);
        res.check('audio plays as late after the recovery as before the step',
            after.bufferMs <= Math.max(2 * before.bufferMs, before.bufferMs + 100), [before.bufferMs, after.bufferMs]);
        res.check('the recovery shows no held video frames back to back', recovery.densestFrames100ms <= 12, recovery);
        await closeBrowser(browser);
        browser = null;
    } finally {
        if (browser) await closeBrowser(browser);
        for (const relay of relays) relay.close();
        painter.kill('SIGKILL');
        pulseUnload(tone);
    }
}

test('websockets', async () => {
    const res = new Checks('congestion-step-websockets');
    const host = new Host(suiteOf(import.meta.filename));
    try {
        await wsStep(res, host);
    } finally {
        await host.close();
    }
    res.finish();
});

for (const split of [false, true]) {
    test(split ? 'webrtc-split' : 'webrtc', async () => {
        const res = new Checks(`congestion-step-${split ? 'webrtc-split' : 'webrtc'}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await wrStep(res, host, split);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
