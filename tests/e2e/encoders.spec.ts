/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The encoders past the defaults, decoded by a real browser on every cell.
//
// jpeg:   a server configured with jpeg. Over WebSockets the stream arrives as
//         several JPEG stripes per frame, decoded and presented by the video
//         worker, and the picture decodes. WebRTC carries no striped framing,
//         so there the configured encoder is refused with a log line and the
//         stream comes up on h264enc.
// cpu:    h264enc forced onto the software encoder (use_cpu): the stream is a
//         single full-frame stripe, the log names the CPU encoder, and the
//         picture decodes.
// switch: over WebSockets the dashboard's encoder select moves a live session
//         from the default encoder onto jpeg and back; the server restarts
//         the capture each way and the picture survives. Over WebRTC the same
//         select offers the full-frame encoders alone.
//
// The picture is two known colors painted on the server: X11 windows on the
// test display, or the Wayland observer's surface filled with one and
// carrying the other, sampled from the decoded frame in the page. Over
// WebSockets the page runs in Chromium; over WebRTC each block runs in
// Chromium, Firefox and WebKit, whose own RTP receivers decode and paint the
// stream, and the <video> must keep presenting frames while the screen
// changes. The cpu and switch servers run Turbo, so the H.264 frames read off
// the wire keep coming while the painted picture holds still. JPEG sends a
// still screen once and its paint-over once, then nothing, so the jpeg block
// paints a picture that keeps changing away from the sampled points.
import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { firefoxPersistentContext, FF_E2E_PROFILE, launchBrowser, openh264Version, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { openVideoSettings } from '../lib/dashboard.ts';
import { Host, type Held } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within, type VideoSize } from '../lib/stream.ts';

const WL_SOCKET = 'wayland-1';
const TURBO = { SELKIES_VIDEO_STREAMING_MODE: 'true' };
// The color painted on the server, and how far a decoded sample may stray
// from it (4:2:0 chroma, limited range, and two codecs' rounding).
const PAINT: readonly number[] = [40, 120, 220];
const PAINT_ARGB = 'ff2878dc';
const TOLERANCE = 24;
// A saturated second block guards the color matrix: a stream converted with
// one matrix and painted with another lands more than twenty levels off here,
// where the first block barely moves. Red rather than green, because libyuv,
// which Chromium and Firefox both convert through, clamps the BT.709
// Cb-to-blue coefficient to 2.0 from 2.112, which costs a green block eleven
// levels of blue and this one two.
const SATURATED: readonly number[] = [255, 0, 0];
const SATURATED_ARGB = 'ffff0000';
const SATURATED_TOLERANCE = 10;
// Where the blocks sit, and a spot well outside them.
const BLOCK = [100, 100, 300, 200] as const;
const BLOCK2 = [500, 100, 300, 200] as const;
const INSIDE = [250, 200] as const;
const OUTSIDE = [900, 600] as const;
const INSIDE2 = [650, 200] as const;
// A live picture changes on its own every LIVE_FLIP_MS: on X11 a small window
// clear of the sampled points flips between two grays, and the Wayland
// surface blinks to a second fill within the picture's tolerance. The <video>
// is counted on it for LIVE_S, LIVE_MIN_FRAMES at least.
const LIVE_S = 2;
const LIVE_FLIP_MS = 50;
const LIVE_MIN_FRAMES = 10;
const FLICKER = [1000, 400, 120, 80] as const;
const FLICKER_GRAYS = [0x404040, 0xC0C0C0] as const;
const PAINT2_ARGB = 'ff2c7cd8';

// Every 0x04 frame on the WebSocket carries its Y start and height in the
// header, a full-frame one only 0; JPEG stripes are counted by the video
// worker. Undiverted frames are read off the worker-socket handle (and a plain
// page socket, should the worker fall back); frames diverted straight to the
// video worker never surface here and are reported by
// window.videoStripeRows instead.
const STRIPE_TAP = `
(() => {
  window.__stripes = {};
  const tap = (e) => {
    if (!(e.data instanceof ArrayBuffer) || e.data.byteLength < 10) return;
    const v = new DataView(e.data);
    if (v.getUint8(0) !== 0x04) return;
    window.__stripes[v.getUint16(4, false)] = v.getUint16(8, false);
  };
  const WS = window.WebSocket;
  window.WebSocket = function(...a) {
    const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
    s.addEventListener('message', tap);
    return s;
  };
  window.WebSocket.prototype = WS.prototype;
  Object.setPrototypeOf(window.WebSocket, WS);
  let transport = null;
  Object.defineProperty(window, 'selkiesTransport', {
    configurable: true,
    get: () => transport,
    set: (v) => {
      transport = v;
      if (v && v.addEventListener && !v.__stripeTapped) {
        v.__stripeTapped = true;
        v.addEventListener('message', tap);
      }
    },
  });
})();
`;

declare global {
    interface Window {
        __stripes?: Record<string, number>;
        videoStripeRows?: Record<string, number>;
        videoDivertOn?: boolean;
        encoder?: string;
    }
}

/** A decoded frame's sampled points. */
interface Sample {
    kind: string;
    w: number;
    h: number;
    inside: number[] | null;
    outside: number[] | null;
    saturated: number[] | null;
}

/**
 * Whether `rgb` is within `tolerance` of `want` on every channel.
 * @param rgb A sample.
 * @param want The color.
 * @param tolerance The levels it may stray.
 * @returns True when it is.
 */
function near(rgb: number[] | null, want: readonly number[], tolerance = TOLERANCE): boolean {
    return rgb !== null && rgb.every((v, i) => Math.abs(v - want[i]) <= tolerance);
}

/**
 * The decoded picture at the three points: whichever sink is showing (the
 * <video> a full-frame mode renders through, else the canvas), drawn once
 * into a scratch canvas.
 * @param page The client.
 * @returns The sample, or null with no sink yet.
 */
function samplePage(page: Page): Promise<Sample | null> {
    return within(page.evaluate(([ix, iy, ox, oy, sx, sy]): Sample | null => {
        const v = document.querySelector('video');
        let src: HTMLVideoElement | HTMLCanvasElement | null = null;
        let w = 0;
        let h = 0;
        let kind = '';
        if (v && v.videoWidth > 0 && v.readyState >= 2 && v.style.display !== 'none') {
            src = v; w = v.videoWidth; h = v.videoHeight; kind = 'video';
        } else {
            for (const c of document.querySelectorAll('canvas')) {
                if (c.width >= 640 && c.style.display !== 'none') { src = c; w = c.width; h = c.height; kind = 'canvas'; break; }
            }
        }
        if (!src) return null;
        const oc = document.createElement('canvas');
        oc.width = w;
        oc.height = h;
        const ctx = oc.getContext('2d');
        if (!ctx) return null;
        ctx.drawImage(src, 0, 0, w, h);
        const d = ctx.getImageData(0, 0, w, h).data;
        // A point past a picture still smaller than the page reads as no sample.
        const px = (x: number, y: number): number[] | null => {
            if (x >= w || y >= h) return null;
            const i = (y * w + x) * 4;
            return [d[i], d[i + 1], d[i + 2]];
        };
        return { kind, w, h, inside: px(ix, iy), outside: px(ox, oy), saturated: px(sx, sy) };
    }, [...INSIDE, ...OUTSIDE, ...INSIDE2]));
}

/**
 * The frames the <video> presents, counted from the first call through
 * requestVideoFrameCallback, which every engine has.
 * @param page The client.
 * @returns The count, or null without a <video>.
 */
function presented(page: Page): Promise<number | null> {
    return within(page.evaluate(() => {
        const v = document.querySelector('video');
        if (!v || typeof v.requestVideoFrameCallback !== 'function') return null;
        if (Reflect.get(v, '__presented') === undefined) {
            Reflect.set(v, '__presented', 0);
            const tick = (): void => {
                Reflect.set(v, '__presented', Number(Reflect.get(v, '__presented')) + 1);
                v.requestVideoFrameCallback(tick);
            };
            v.requestVideoFrameCallback(tick);
        }
        return Number(Reflect.get(v, '__presented'));
    }));
}

/**
 * The painted picture as the page decodes it: the X11 blocks sit in a black
 * frame, the Wayland observer surface covers the frame in the first color
 * and carries the saturated block. A live picture also changes on its own
 * away from the sampled points.
 */
class Picture {
    private held: Held | null = null;
    private readonly host: Host;
    private readonly wayland: boolean;
    private readonly live: boolean;

    constructor(host: Host, wayland: boolean, live = false) {
        this.host = host;
        this.wayland = wayland;
        this.live = live;
    }

    async paint(): Promise<void> {
        if (this.wayland) {
            const env: Record<string, string> = { WLOBS_FILL: PAINT_ARGB, WLOBS_BLOCK: `${BLOCK2.join(',')},${SATURATED_ARGB}` };
            if (this.live) Object.assign(env, { WLOBS_FILL2: PAINT2_ARGB, WLOBS_BLINK_MS: String(LIVE_FLIP_MS) });
            const obs = await this.host.observe(WL_SOCKET, env);
            this.held = obs;
            await obs.ready(20);
            return;
        }
        const rgb = (argb: string): number => parseInt(argb.slice(2), 16);
        this.held = await this.host.paint([[...BLOCK, rgb(PAINT_ARGB)], [...BLOCK2, rgb(SATURATED_ARGB)]],
            this.live ? { paint: [...FLICKER, FLICKER_GRAYS[0]], second: FLICKER_GRAYS[1], periodMs: LIVE_FLIP_MS } : undefined);
    }

    async keepsPresenting(res: Checks, tag: string, page: Page): Promise<void> {
        const start = await presented(page);
        await sleep(LIVE_S * 1000);
        const frames = await presented(page);
        const live = { frames: frames === null ? null : frames - (start ?? 0), secs: LIVE_S, flips: Math.round(LIVE_S * 1000 / LIVE_FLIP_MS) };
        res.check(`${tag}: frames keep presenting on a changing screen`, (live.frames ?? 0) >= LIVE_MIN_FRAMES, live);
    }

    async clear(): Promise<void> {
        await this.held?.release();
        this.held = null;
    }

    /**
     * Whether the sample shows the painted picture; `matrix` also holds the
     * saturated block to its tolerance.
     */
    matches(sample: Sample | null, matrix = true): boolean {
        if (!sample) return false;
        const ground = this.wayland ? PAINT : [0, 0, 0];
        return near(sample.inside, PAINT) && near(sample.outside, ground)
            && (!matrix || near(sample.saturated, SATURATED, SATURATED_TOLERANCE));
    }

    async wait(page: Page, timeoutS = 20, matrix = true): Promise<Sample | null> {
        let sample: Sample | null = null;
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
            sample = await samplePage(page);
            if (this.matches(sample, matrix)) return sample;
            await sleep(500);
        }
        return sample;
    }
}

/**
 * A page of `engine` on the client in `mode`, with its browser or context to close.
 * @param url The server.
 * @param mode The transport.
 * @param engine The engine.
 * @returns What to close, and the page.
 */
async function openPage(url: string, mode: string, engine: Engine): Promise<[Browser | BrowserContext, Page]> {
    let owner: Browser | BrowserContext;
    let ctx: BrowserContext;
    if (engine === 'firefox') {
        ctx = await firefoxPersistentContext({ viewport: { width: 1280, height: 720 } });
        owner = ctx;
    } else {
        const browser = await launchBrowser(engine);
        owner = browser;
        ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
    }
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    // Firefox runs on one persistent profile: the encoder a previous block's
    // ladder stored must not become this block's pick.
    await ctx.addInitScript({ content: 'try { localStorage.clear(); } catch (e) {}' });
    await ctx.addInitScript({ content: STRIPE_TAP });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    return [owner, page];
}

function waitVideo(page: Page, mode: string): Promise<VideoSize | null> {
    return mode === 'websockets' ? waitWsVideo(page, 30) : waitWrVideo(page);
}

/** The divert state the core publishes. */
interface Divert {
    on: boolean;
    rows: number;
    fps: number;
}

/**
 * Poll the divert state until it matches. `presented` also waits for a frame
 * to have been presented: the row layout is published as the first stripes
 * are decoded, the frame rate over a window that closes later.
 */
async function waitDivert(page: Page, want: boolean, timeoutS = 15, presentedToo = false): Promise<Divert> {
    let state: Divert = { on: false, rows: 0, fps: 0 };
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        state = await within(page.evaluate((): Divert => ({
            on: Boolean(window.videoDivertOn),
            rows: Object.keys(window.videoStripeRows ?? {}).length,
            fps: window.fps ?? 0,
        })));
        const settled = state.rows > 0 && (!presentedToo || state.fps > 0);
        if (state.on === want && (!want || settled)) return state;
        await sleep(500);
    }
    return state;
}

/**
 * Stripe Y start -> height seen on the wire so far: the page tap merged with
 * the row layout the video worker reports while the divert holds.
 */
async function stripes(page: Page): Promise<Record<number, number>> {
    const seen = await within(page.evaluate(() => Object.assign({}, window.videoStripeRows ?? {}, window.__stripes ?? {})));
    const out: Record<number, number> = {};
    for (const [k, v] of Object.entries(seen)) out[Number(k)] = v;
    return out;
}

/** Poll the wire until it shows the striping asked for, or time runs out. */
async function waitStripes(page: Page, striped: boolean, timeoutS = 15): Promise<Record<number, number>> {
    let seen: Record<number, number> = {};
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        await within(page.evaluate(() => { window.__stripes = {}; window.videoStripeRows = {}; }));
        await sleep(1200);
        seen = await stripes(page);
        const n = Object.keys(seen).length;
        if (n && (n > 1) === striped) return seen;
    }
    return seen;
}

function isStriped(seen: Record<number, number>, frameH: number): boolean {
    const heights = Object.values(seen);
    return heights.length > 1 && Math.max(...heights) < frameH;
}

function isFullframe(seen: Record<number, number>, frameH: number): boolean {
    const keys = Object.keys(seen);
    return keys.length === 1 && keys[0] === '0' && seen[0] === frameH;
}

/** How many times pixelflux has printed its stream line. */
async function streamLines(host: Host): Promise<number> {
    return (await host.log()).split('Stream settings active').length - 1;
}

/** The newest stream line pixelflux printed. */
async function lastStreamLine(host: Host): Promise<string> {
    return (await host.log()).split('\n').filter((l) => l.includes('Stream settings active')).at(-1) ?? '';
}

/** The stream line once pixelflux has printed more than `count` of them. */
async function waitStreamLine(host: Host, count: number, timeoutS = 20): Promise<string> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if ((await streamLines(host)) > count) return lastStreamLine(host);
        await sleep(500);
    }
    return '';
}

/** The encoder part of a stream line, for a check's detail. */
function encoderField(line: string): string {
    const at = Math.max(line.indexOf('Encoder:'), line.indexOf('Mode:'));
    return at >= 0 ? line.slice(at, at + 60) : line.slice(-80);
}

/**
 * Whether a stream line names the software H.264 encoder: pixelflux prints
 * it as CPU on X11 and by its library name on Wayland, never as a GPU.
 */
function cpuEncoder(line: string): boolean {
    const software = ['CPU', 'x264', 'OpenH264', 'openh264'].some((n) => line.includes(n));
    return software && !line.includes('NVENC') && !line.includes('VAAPI');
}

async function blockJpeg(host: Host, mode: string, wayland: boolean, res: Checks, engine: Engine): Promise<void> {
    const tag = engine === 'chromium' ? 'jpeg' : `jpeg ${engine}`;
    const { url } = await host.start({ label: `jpeg-${engine}`, mode, wayland, env: { SELKIES_ENCODER: 'jpeg' } });
    const picture = new Picture(host, wayland, true);
    try {
        await picture.paint();
        const [owner, page] = await openPage(url, mode, engine);
        try {
            const video = await waitVideo(page, mode);
            res.check(`${tag}: stream up`, Boolean(video), video);
            if (mode === 'websockets') {
                const enc = await within(page.evaluate(() => window.encoder ?? null));
                res.check(`${tag}: client follows the server's encoder`, enc === 'jpeg', enc);
                const seen = await waitStripes(page, true);
                res.check(`${tag}: several stripes per frame`, video !== null && isStriped(seen, video.h), seen);
                const divert = await waitDivert(page, true, 15, true);
                res.check(`${tag}: the video worker decodes and presents it`, divert.on && divert.rows > 1 && divert.fps > 0, divert);
            } else {
                res.check(`${tag}: refused for WebRTC, h264enc used instead`,
                    (await host.saw('not available for WebRTC', 5)) && (await host.saw('using "h264enc"', 5)), '');
            }
            const sample = await picture.wait(page);
            res.check(`${tag}: the painted picture decodes`, picture.matches(sample), sample);
            if (mode === 'webrtc') await picture.keepsPresenting(res, tag, page);
        } finally {
            await owner.close();
        }
    } finally {
        await picture.clear();
        await host.stop();
    }
}

async function blockCpu(host: Host, mode: string, wayland: boolean, res: Checks, engine: Engine): Promise<void> {
    const tag = engine === 'chromium' ? 'cpu' : `cpu ${engine}`;
    if (mode === 'webrtc' && engine === 'firefox' && !openh264Version()) {
        res.skip(`${tag}: no OpenH264 GMP plugin in ${FF_E2E_PROFILE}`, 'run tests/tools/fetch-openh264.sh to cover H.264 in Firefox');
        return;
    }
    const { url } = await host.start({ label: `cpu-${engine}`, mode, wayland, env: { SELKIES_ENCODER: 'h264enc', SELKIES_USE_CPU: 'true', ...TURBO } });
    const picture = new Picture(host, wayland, mode === 'webrtc');
    try {
        await picture.paint();
        const [owner, page] = await openPage(url, mode, engine);
        try {
            const video = await waitVideo(page, mode);
            res.check(`${tag}: stream up`, Boolean(video), video);
            const line = await waitStreamLine(host, 0);
            res.check(`${tag}: software encoder in use`, cpuEncoder(line), encoderField(line));
            if (mode === 'websockets') {
                const seen = await waitStripes(page, false);
                res.check(`${tag}: one full-frame stripe on the wire`, video !== null && isFullframe(seen, video.h), seen);
            }
            const sample = await picture.wait(page);
            res.check(`${tag}: the painted picture decodes`, picture.matches(sample), sample);
            if (mode === 'webrtc') await picture.keepsPresenting(res, tag, page);
        } finally {
            await owner.close();
        }
    } finally {
        await picture.clear();
        await host.stop();
    }
}

async function blockSwitch(host: Host, mode: string, wayland: boolean, res: Checks, engine: Engine): Promise<void> {
    const tag = engine === 'chromium' ? 'switch' : `switch ${engine}`;
    const { url } = await host.start({ label: `switch-${engine}`, mode, wayland, env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, ...TURBO } });
    const picture = new Picture(host, wayland);
    try {
        await picture.paint();
        const [owner, page] = await openPage(url, mode, engine);
        try {
            const video = await waitVideo(page, mode);
            res.check(`${tag}: stream up`, Boolean(video), video);
            const opened = await openVideoSettings(page, 'default');
            const options = opened
                ? await within(page.evaluate(() => [...document.querySelectorAll('#encoderSelect option')].map((o) => (o instanceof HTMLOptionElement ? o.value : ''))))
                : [];
            if (mode === 'webrtc') {
                // WebRTC carries the full-frame encoders alone, those the engine's
                // RTP receiver takes; JPEG's striped framing never shows.
                const fullFrame = ['h264enc', 'h265enc', 'vp8enc', 'vp9enc', 'av1enc'];
                res.check(`${tag}: the dashboard offers WebRTC only full-frame encoders`,
                    opened && options.includes('h264enc') && options.every((o) => fullFrame.includes(o)), [opened, options]);
                return;
            }
            res.check(`${tag}: the dashboard offers jpeg and no striped H.264`, options.includes('jpeg') && !options.includes('h264enc-striped'), options);
            let seen = await waitStripes(page, false);
            res.check(`${tag}: default stream is full-frame`, video !== null && isFullframe(seen, video.h), seen);
            let before = await streamLines(host);
            await page.selectOption('#encoderSelect', 'jpeg');
            let line = await waitStreamLine(host, before);
            res.check(`${tag}: capture restarted on jpeg`, Boolean(line), encoderField(line));
            seen = await waitStripes(page, true);
            res.check(`${tag}: jpeg stripes on the wire`, video !== null && isStriped(seen, video.h), seen);
            const divert = await waitDivert(page, true);
            res.check(`${tag}: jpeg decodes in the video worker`, divert.on && divert.rows > 1, divert);
            let sample = await picture.wait(page);
            res.check(`${tag}: the picture decodes as jpeg`, picture.matches(sample), sample);
            before = await streamLines(host);
            await page.selectOption('#encoderSelect', 'h264enc');
            line = await waitStreamLine(host, before);
            res.check(`${tag}: capture restarted back on h264enc`, Boolean(line), encoderField(line));
            seen = await waitStripes(page, false);
            res.check(`${tag}: full-frame again on the wire`, video !== null && isFullframe(seen, video.h), seen);
            sample = await picture.wait(page);
            res.check(`${tag}: the picture decodes full-frame again`, picture.matches(sample), sample);
        } finally {
            await owner.close();
        }
    } finally {
        await picture.clear();
        await host.stop();
    }
}

const ENGINES: readonly Engine[] = ['chromium', 'firefox', 'webkit'];

for (const which of ['ws-x11', 'wr-x11', 'ws-wl', 'wr-wl']) {
    test(which, async () => {
        const [transport, backend] = which.split('-');
        const mode = transport === 'ws' ? 'websockets' : 'webrtc';
        const wayland = backend === 'wl';
        const res = new Checks(`encoders-${which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            for (const engine of mode === 'webrtc' ? ENGINES : ENGINES.slice(0, 1)) {
                for (const block of [blockJpeg, blockCpu, blockSwitch]) await block(host, mode, wayland, res, engine);
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}

