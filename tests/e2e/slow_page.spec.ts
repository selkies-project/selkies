/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A page on a slow link beside the display's owner: one encoder, and each page
// what its link carries.
//
// Every page of a display is served from one encoder (an SFU). A page whose
// link cannot carry the stream falls behind and has frames dropped for it;
// where the encoder names each frame's reference, the page is resynced on the
// first frame predicting past the drop, so the shared stream carries no key
// frame for it. The owner, on a link with room, decodes on undisturbed, and
// the slow page keeps receiving frames, lagging by about a second rather than
// by the seconds of stream a byte budget holds.
//
// The slow page is a second page beside the owner: over WebSockets behind a
// metered TCP relay (the bwrelay program), over WebRTC behind narrow UDP
// relays in this process, which the page reaches the server through alone:
// its init script points the server's candidates at them and withholds its
// own, so the server meets it only through them. The webrtc-firefox block
// makes it Firefox, which answers without the dependency descriptor (on the
// OpenH264 profile, its relays on the default route's address, since Firefox
// gathers there alone and pairs none of its candidates with a loopback one).
// The owner's key frames are counted on its own page. The scene is the
// motion-scene program, whose every frame spells its own index, so the
// pictures both pages decode are checked against the frames they claim to be,
// and the slow page's lag behind the owner read off the two pages' pictures.
// The -wayland blocks run the same on pixelflux's compositor, the scene drawn
// by its Wayland twin (wl-motion-scene).
import { spawn, type ChildProcess } from 'node:child_process';
import { createSocket } from 'node:dgram';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';


import { firefoxPersistentContext, FF_E2E_PROFILE, launchBrowser, openh264Version, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { suiteOf, WORKDIR } from '../lib/env.ts';
import { Host, suitesBin } from '../lib/host.ts';
import { PC_TAP_JS } from '../lib/page.ts';
import { checkPicture, CORRUPT_FRACTION, type Checked } from '../lib/scene.ts';
import { closeBrowser, waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';
import { makeRelay, Shaper, SHAPED_JS, type Relay } from '../lib/udprelay.ts';

const WIDTH = 1280;
const HEIGHT = 720;
const RELAY_PORT = Number(process.env.E2E_SLOW_PAGE_PORT ?? '18214');
// A third of the 8 Mbit/s the moving scene streams at.
const LINK_KBIT = Number(process.env.E2E_SLOW_PAGE_KBIT ?? '2500');
const WINDOW_S = 30;
const SETTLE_S = 10;
// Congestion control off: it would fit the stream to the owner's link, which
// has room, and the narrow link is the second page's alone.
const ENV = { SELKIES_CONGESTION_CONTROL: 'false', SELKIES_ENCODER: 'h264enc', SELKIES_VIDEO_BITRATE: '8000' };

// Each video frame a page receives over its socket: whether it is a key frame
// or an anchor (FRAME_ANCHOR), and when it came.
const FRAME_TAP_JS = `
  window.__keys = 0;
  window.__anchors = 0;
  window.__frames = [];
  (() => {
    let last = -1;
    const tap = (e) => {
      if (!(e.data instanceof ArrayBuffer) || e.data.byteLength < 12) return;
      const h = new Uint8Array(e.data, 0, 12);
      if (h[0] !== 0x04) return;
      const id = (h[2] << 8) | h[3];
      if (id === last) return;
      last = id;
      window.__lastId = id;
      if ((h[1] & 0x0F) === 0x01) window.__keys++;
      if (h[1] & 0x08) window.__anchors++;
      window.__frames.push(performance.now());
      if (window.__frames.length > 40000) window.__frames.splice(0, 20000);
    };
    const WS = window.WebSocket;
    window.WebSocket = function(...a) {
      const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
      s.addEventListener('message', tap);
      return s;
    };
    window.WebSocket.prototype = WS.prototype;
    Object.setPrototypeOf(window.WebSocket, WS);
    // The websockets transport runs its socket in a worker; its receive side is the page handle.
    let transport = null;
    Object.defineProperty(window, 'selkiesTransport', {
      configurable: true,
      get: () => transport,
      set: (v) => { transport = v; if (v && v.addEventListener) v.addEventListener('message', tap); },
    });
  })();
`;

declare global {
    interface Window {
        __keys?: number;
        __anchors?: number;
        __frames?: number[];
    }
}

/** The inbound video counters of a page's peer connections. */
interface Inbound {
    keyframes: number;
    decoded: number;
    plis: number;
    freezes: number;
    frozen: number;
}

function inbound(page: Page): Promise<Inbound> {
    return within(page.evaluate(async (): Promise<Inbound> => {
        const out = { keyframes: 0, decoded: 0, plis: 0, freezes: 0, frozen: 0 };
        const pcs: unknown = Reflect.get(window, '__pcs');
        for (const pc of Array.isArray(pcs) ? pcs : []) {
            if (!(pc instanceof RTCPeerConnection)) continue;
            (await pc.getStats()).forEach((r: Record<string, unknown>) => {
                if (r.type !== 'inbound-rtp' || r.kind !== 'video') return;
                out.keyframes += Number(r.keyFramesDecoded ?? 0);
                out.decoded += Number(r.framesDecoded ?? 0);
                out.plis += Number(r.pliCount ?? 0);
                out.freezes += Number(r.freezeCount ?? 0);
                out.frozen += Number(r.totalFreezesDuration ?? 0);
            });
        }
        return out;
    }));
}

/** The two pages' pictures sampled together about once a second, as (owner, slow page). */
async function pictures(owner: Page, slow: Page, seconds: number): Promise<[Checked, Checked][]> {
    const out: [Checked, Checked][] = [];
    for (const end = Date.now() + seconds * 1000; Date.now() < end;) {
        const a = await checkPicture(owner);
        const b = await checkPicture(slow);
        if (a && b && !a.ambiguous && !b.ambiguous) out.push([a, b]);
        await owner.waitForTimeout(1000);
    }
    return out;
}

/** How many frames of the scene the slow page's picture trails the owner's, per sample; the page read second may show a newer one. */
function lagFrames(samples: readonly [Checked, Checked][]): number[] {
    return samples.map(([a, b]) => (((a.index - b.index + 0x8000) & 0xFFFF) - 0x8000)).sort((x, y) => x - y);
}

/**
 * How many of the owner's pictures show no later frame than its previous
 * sample did. The scene moves on sixty frames between two samples, so a
 * picture that does not is a stale frame, which still passes for the frame
 * it claims to be.
 */
function stuck(samples: readonly [Checked, Checked][]): number {
    let n = 0;
    for (let i = 1; i < samples.length; i++) {
        const d = (samples[i][0].index - samples[i - 1][0].index) & 0xFFFF;
        if (!(d > 0 && d < 0x8000)) n++;
    }
    return n;
}

/** The longest gap between frames; infinity for under two. */
function stalls(times: readonly number[]): number {
    let worst = -Infinity;
    for (let i = 1; i < times.length; i++) worst = Math.max(worst, times[i] - times[i - 1]);
    return worst === -Infinity ? Infinity : worst;
}

/** The server log of a block's window, beside the run's other logs. */
function keepLog(name: string, text: string): void {
    writeFileSync(join(WORKDIR, `slow-page-${name}.log`), text);
}

/**
 * The moving scene on the display under test, to end at the end. A Wayland
 * scene that never maps is a failed check naming what it printed: the
 * desktop behind it stands still, which would otherwise read as a stalled
 * stream of wrong pictures.
 */
async function scene(res: Checks, host: Host, wayland: boolean): Promise<{ stop: () => Promise<void> }> {
    if (!wayland) {
        const painter: ChildProcess = spawn(suitesBin('motion-scene'), [process.env.E2E_DISPLAY ?? '', String(WIDTH), String(HEIGHT), '60'], { stdio: 'ignore' });
        return { stop: () => { painter.kill('SIGKILL'); return Promise.resolve(); } };
    }
    const client = await host.observe('wayland-1', { WLOBS_DURATION: '900' }, 'wl-motion-scene');
    res.check('the scene maps its window', await client.ready(30), (await client.output()).slice(-3));
    return { stop: () => client.release() };
}

function median(values: readonly number[]): number | undefined {
    return values[Math.floor(values.length / 2)];
}

async function wsBlock(res: Checks, host: Host, wayland: boolean): Promise<void> {
    const { port } = await host.start({ label: `websockets${wayland ? '-wayland' : ''}`, mode: 'websockets', wayland, env: ENV });
    let painter: { stop: () => Promise<void> } | null = null;
    let relay: ChildProcess | null = null;
    try {
        painter = await scene(res, host, wayland);
        relay = spawn(suitesBin('bwrelay'), [String(RELAY_PORT), String(port), String(LINK_KBIT)], { stdio: 'ignore' });
        const pages: [Browser, Page][] = [];
        for (const url of [`http://127.0.0.1:${port}/`, `http://127.0.0.1:${RELAY_PORT}/`]) {
            const browser = await launchBrowser('chromium');
            const ctx = await browser.newContext({ viewport: { width: WIDTH, height: HEIGHT }, deviceScaleFactor: 1 });
            await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
            await ctx.addInitScript({ content: FRAME_TAP_JS });
            const page = await ctx.newPage();
            await page.goto(url, { waitUntil: 'load', timeout: 90_000 });
            pages.push([browser, page]);
            if (pages.length === 1) res.check('the owner streams the moving scene', (await waitWsVideo(page, 45)) !== null);
        }
        const owner = pages[0][1];
        const slow = pages[1][1];
        res.check('a page beside it streams over a link a third of the stream', (await waitWsVideo(slow, 90)) !== null);
        await owner.waitForTimeout(SETTLE_S * 1000);
        const keys0 = await within(owner.evaluate(() => window.__keys ?? 0));
        const since = await within(slow.evaluate(() => performance.now()));
        const anchors0 = await within(slow.evaluate(() => window.__anchors ?? 0));
        const mark = await host.mark();
        const samples = await pictures(owner, slow, WINDOW_S);
        const keys = (await within(owner.evaluate(() => window.__keys ?? 0))) - keys0;
        const anchors = (await within(slow.evaluate(() => window.__anchors ?? 0))) - anchors0;
        const frames = await within(slow.evaluate((t0) => (window.__frames ?? []).filter((t) => t >= t0), since));
        const log = await host.log(mark);
        keepLog('websockets', log);
        const lags = lagFrames(samples);
        const worst = samples.length ? Math.max(...samples.map(([, b]) => b.mismatch)) : 1;
        const ownerWorst = samples.length ? Math.max(...samples.map(([a]) => a.mismatch)) : 1;
        const lag = median(lags);
        res.check("the owner's stream carries no key frame for the slow page", keys <= 1, `${keys} key frames`);
        res.check('the slow page keeps receiving frames', frames.length >= WINDOW_S * 5, `${frames.length} frames`);
        res.check('without a stall past two seconds', stalls(frames) < 2000, `${stalls(frames).toFixed(0)} ms`);
        res.check('every picture it decodes is the frame it claims to be', samples.length >= Math.floor(WINDOW_S / 2) && worst < CORRUPT_FRACTION,
            `${samples.length} samples, worst ${worst.toFixed(3)}`);
        res.check('and so is every picture the owner decodes', ownerWorst < CORRUPT_FRACTION, `worst ${ownerWorst.toFixed(3)}`);
        res.check('whose picture moves on between samples', stuck(samples) === 0, `${stuck(samples)} stuck`);
        res.check('it trails the owner by under a second', lag !== undefined && lag < 60, lags);
        res.check('the drops were answered by the encoder predicting past them', log.includes('lost by a client; the encoder predicts past it'));
        process.stdout.write(`      owner: ${keys} key frames in ${WINDOW_S} s; slow page: ${frames.length} frames (${anchors} anchors), `
            + `longest stall ${stalls(frames).toFixed(0)} ms, lag ${lag ?? '?'} frames (median), worst picture ${worst.toFixed(3)} `
            + `(the owner's ${ownerWorst.toFixed(3)})\n`);
        for (const [browser] of pages) await closeBrowser(browser);
    } finally {
        await painter?.stop();
        relay?.kill('SIGKILL');
        await host.stop();
    }
}

/** The address of the interface the default route leaves by (no packet is sent finding it). */
async function defaultAddress(): Promise<string> {
    const s = createSocket('udp4');
    try {
        await new Promise<void>((done, fail) => { s.once('error', fail); s.connect(9, '192.0.2.1', done); });
        return s.address().address;
    } catch {
        return '127.0.0.1';
    } finally {
        s.close();
    }
}

async function wrBlock(res: Checks, host: Host, engine: Engine, wayland: boolean): Promise<void> {
    if (engine === 'firefox' && !openh264Version()) {
        res.skip(`no OpenH264 GMP plugin in ${FF_E2E_PROFILE}`, 'run tests/tools/fetch-openh264.sh to cover H.264 in Firefox');
        return;
    }
    const up = new Shaper();
    const down = new Shaper(LINK_KBIT * 1000, 0.03);
    const listen = engine === 'firefox' ? await defaultAddress() : '127.0.0.1';
    const relays: Relay[] = [];
    const { url } = await host.start({
        label: `webrtc${engine === 'chromium' ? '' : `-${engine}`}${wayland ? '-wayland' : ''}`, mode: 'webrtc', wayland,
        env: { ...ENV, SELKIES_WEBRTC_PACER: 'true', SELKIES_STUN_HOST: '', SELKIES_TURN_REST_URI: '' },
    });
    let painter: { stop: () => Promise<void> } | null = null;
    try {
        painter = await scene(res, host, wayland);
        const pages: [Browser | BrowserContext, Page][] = [];
        for (const script of [PC_TAP_JS, SHAPED_JS]) {
            const viewport = { width: WIDTH, height: HEIGHT };
            let owner: Browser | BrowserContext;
            let ctx: BrowserContext;
            if (script === SHAPED_JS && engine === 'firefox') {
                ctx = await firefoxPersistentContext({ viewport });
                owner = ctx;
            } else {
                const browser = await launchBrowser('chromium');
                ctx = await browser.newContext({ viewport, deviceScaleFactor: 1 });
                owner = browser;
            }
            await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'webrtc';" });
            if (script === SHAPED_JS) {
                // A relay toward one of the server's candidates, shaped on the way to the page.
                await ctx.exposeFunction('__shapedRelay', async (address: string, port: number) => {
                    const relay = await makeRelay([address, Number(port)], up, down, listen);
                    relays.push(relay);
                    return relay.address;
                });
            }
            await ctx.addInitScript({ content: script });
            const page = await ctx.newPage();
            await page.goto(`${url}/`, { waitUntil: 'load', timeout: 90_000 });
            pages.push([owner, page]);
            if (pages.length === 1) res.check('the owner streams the moving scene', Boolean(await waitWrVideo(page)));
        }
        const owner = pages[0][1];
        const slow = pages[1][1];
        res.check('a page beside it streams over a link a third of the stream', Boolean(await waitWrVideo(slow, 90)));
        await owner.waitForTimeout(SETTLE_S * 1000);
        const before = [await inbound(owner), await inbound(slow)];
        const mark = await host.mark();
        const samples = await pictures(owner, slow, WINDOW_S);
        const after = [await inbound(owner), await inbound(slow)];
        const log = await host.log(mark);
        keepLog('webrtc', log);
        const delta = (a: Inbound, b: Inbound): Inbound => ({
            keyframes: a.keyframes - b.keyframes, decoded: a.decoded - b.decoded, plis: a.plis - b.plis,
            freezes: a.freezes - b.freezes, frozen: a.frozen - b.frozen,
        });
        const own = delta(after[0], before[0]);
        const sl = delta(after[1], before[1]);
        const lags = lagFrames(samples);
        const lag = median(lags);
        const worst = samples.length ? Math.max(...samples.map(([, b]) => b.mismatch)) : 1;
        const ownerWorst = samples.length ? Math.max(...samples.map(([a]) => a.mismatch)) : 1;
        // A page sent a skipped picture for each frame left out (Firefox) decodes every
        // frame the owner does, and more by the lag it makes up in the window.
        res.check('the owner decodes on beside it', own.decoded >= Math.max(WINDOW_S * 10, sl.decoded - Math.max(0, ...lags)), own);
        res.check("the owner's stream carries no key frame for the slow page", own.keyframes <= 1, `${own.keyframes} key frames`);
        res.check('the slow page keeps decoding frames', sl.decoded >= WINDOW_S * 5, sl);
        res.check('without asking for a key frame', sl.plis === 0, sl);
        res.check('every picture it decodes is the frame it claims to be', samples.length >= Math.floor(WINDOW_S / 2) && worst < CORRUPT_FRACTION,
            `${samples.length} samples, worst ${worst.toFixed(3)}`);
        res.check('and so is every picture the owner decodes', ownerWorst < CORRUPT_FRACTION, `worst ${ownerWorst.toFixed(3)}`);
        res.check('whose picture moves on between samples', stuck(samples) === 0, `${stuck(samples)} stuck`);
        res.check('it trails the owner by under a second', lag !== undefined && lag < 60, lags);
        const cuts = log.split('the sender resyncs its peer').length - 1;
        res.check('its drops were answered by the encoder predicting past them',
            log.includes('lost by a peer; the encoder predicts past it') && !log.includes('GOP reset'), [cuts, log.split('GOP reset').length - 1]);
        process.stdout.write(`      owner: ${own.keyframes} key frames, ${own.decoded} decoded; slow page: ${sl.decoded} decoded, `
            + `${sl.keyframes} key frames, ${sl.plis} PLIs, ${sl.freezes} freezes (${sl.frozen.toFixed(1)} s), lag ${lag ?? '?'} frames (median), `
            + `worst picture ${worst.toFixed(3)} (the owner's ${ownerWorst.toFixed(3)}); pacer cuts ${cuts}\n`);
        for (const [closer] of pages) await closeBrowser(closer);
    } finally {
        await painter?.stop();
        for (const relay of relays) relay.close();
        await host.stop();
    }
}

const BLOCKS: Record<string, [string, (res: Checks, host: Host) => Promise<void>]> = {
    websockets: ['slow-page-websockets', (r, h) => wsBlock(r, h, false)],
    webrtc: ['slow-page-webrtc', (r, h) => wrBlock(r, h, 'chromium', false)],
    'webrtc-firefox': ['slow-page-webrtc-firefox', (r, h) => wrBlock(r, h, 'firefox', false)],
    'websockets-wayland': ['slow-page-websockets-wayland', (r, h) => wsBlock(r, h, true)],
    'webrtc-wayland': ['slow-page-webrtc-wayland', (r, h) => wrBlock(r, h, 'chromium', true)],
};

for (const [selector, [block, run]] of Object.entries(BLOCKS)) {
    test(selector, async () => {
        const res = new Checks(block);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await run(res, host);
        } finally {
            await host.close();
        }
        res.finish();
    });
}

