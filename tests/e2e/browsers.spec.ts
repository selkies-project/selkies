/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Browser-engine matrix: Chromium (Chrome binary), Firefox, WebKit over the
// websockets transport (flow, audio, input, clipboard, resize, console
// health), plus a reduced WebRTC flow on Firefox (parity with the Chrome
// reference) and WebKit. Over WebSockets, video and audio reach the page and
// playback on every engine, through libopus in WASM where the engine has no
// WebCodecs audio, and play on the output device the page picks; a <video>
// sink the browser will not start without a user gesture starts on the next
// press. Over WebRTC, a manual resolution shown 1:1 is drawn nearest-sampled
// in Chromium only, since Firefox's compositor draws that slower than a
// smoothed video.
import type { Browser, BrowserContext, Page } from '@playwright/test';
import { chromium, test } from '@playwright/test';

import { analyze, record } from '../lib/audio.ts';
import { BROWSER_ARGS, DECODER_ERROR_PATTERNS, engineLaunch, FF_E2E_PROFILE, launchBrowser, openh264Version, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { pulseNullSink, pulseSine, pulseUnload } from '../lib/desktop.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { NO_WEBCODECS_AUDIO_JS, sendClipboardFromClient } from '../lib/page.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

// A field of the class the dashboards put on their root, whose keys the input
// core leaves to the page, and a point on it.
const NATIVE_FIELD_JS = `(() => {
  const f = document.createElement('input');
  f.id = 'native-field';
  f.className = 'allow-native-input';
  f.style.cssText = 'position:fixed;left:20px;top:20px;width:200px;height:30px;z-index:9999';
  document.body.appendChild(f);
})()`;
const NATIVE_FIELD_AT = [120, 35] as const;
const KEY_X = 0x78;

declare global {
    interface Window {
        __clipMsgs?: { text?: string }[];
        __wsFrames?: number;
        currentAudioBufferSize?: number;
        currentAudioLevel?: number;
    }
}

/** The first page of a context: Firefox's persistent one opens with one. */
async function firstPage(engine: Engine, ctx: BrowserContext): Promise<Page> {
    const pages = ctx.pages();
    return engine === 'firefox' && pages.length ? pages[0] : ctx.newPage();
}

async function closeAll(browser: Browser | null, ctx: BrowserContext): Promise<void> {
    if (browser) await browser.close();
    else await ctx.close();
}

/**
 * Press `key` and leave it down; whether the X keymap then shows it held.
 * Headless WebKit drops synthetic keydowns under load, so the whole press is
 * retried rather than waited on.
 */
async function hold(host: Host, page: Page, key: string): Promise<boolean | null> {
    let pressed: boolean | null = false;
    for (let i = 0; i < 4; i++) {
        await page.keyboard.down(key);
        await sleep(800);
        pressed = await host.keyHeld(key.charCodeAt(0));
        if (pressed === true) break;
        await page.keyboard.up(key);
        await sleep(600);
    }
    return pressed;
}

/**
 * The flow/input/clipboard/resize checklist on one browser engine. The server
 * runs Turbo, so the still test display keeps sending the frames the
 * WebSockets block counts.
 */
async function engineBlock(host: Host, engine: Engine, mode = 'websockets'): Promise<Checks> {
    const tag = `${engine}-${mode}`;
    const res = new Checks(tag);
    const { url } = await host.start({ label: tag, mode, env: { SELKIES_VIDEO_STREAMING_MODE: 'true' } });
    const [browser, ctx] = await engineLaunch(engine);
    try {
        await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        // WebKit doesn't implement the clipboard permissions schema at all.
        if (engine === 'chromium') await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: url }).catch(() => undefined);
        const page = await firstPage(engine, ctx);
        const consoleErrors: string[] = [];
        page.on('console', (m) => { if (m.type() === 'error') consoleErrors.push(m.text()); });
        page.on('pageerror', (e) => consoleErrors.push(String(e)));
        const notFound: string[] = [];
        page.on('response', (r) => { if (r.status() === 404) notFound.push(r.url()); });
        await page.addInitScript({ content: `
          window.__clipMsgs = [];
          window.__wsFrames = 0;
          window.addEventListener('message', (e) => {
            if (e.data && e.data.type === 'clipboardContentUpdate') window.__clipMsgs.push(e.data);
          });
          (() => {
            const WS = window.WebSocket;
            window.WebSocket = function(...a) {
              const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
              s.addEventListener('message', (e) => { if (e.data instanceof ArrayBuffer) window.__wsFrames++; });
              return s;
            };
            window.WebSocket.prototype = WS.prototype;
            Object.setPrototypeOf(window.WebSocket, WS);
          })();
        ` });
        await page.goto(url, { waitUntil: 'load' });
        // Headless WebKit drops synthetic key events after a long idle render
        // round; waitWsVideo polls for the painted canvas anyway.
        await sleep(engine === 'webkit' ? 2000 : 6000);

        if (mode === 'websockets') {
            const info = await waitWsVideo(page, 25);
            res.check('video: canvas painted', info !== null, info ?? 'no canvas>=640');
            let frames = 0;
            for (const until = Date.now() + 12_000; Date.now() < until;) {
                frames = await within(page.evaluate(() => window.videoChunksReceived || window.__wsFrames || 0));
                if (frames >= 24) break;
                await sleep(500);
            }
            res.check('video: WS frames flowing', frames >= 24, frames);
        } else {
            const info = await waitWrVideo(page, 45);
            res.check('video: <video> receiving', info !== null, info);
        }

        await page.mouse.click(640, 360);
        await sleep(500);
        if (mode === 'websockets') {
            // The capture sends nothing while the desktop is silent.
            const tone = pulseSine();
            try {
                let depth = 0;
                for (const until = Date.now() + 12_000; Date.now() < until;) {
                    depth = await within(page.evaluate(() => window.currentAudioBufferSize || 0));
                    if (depth > 0) break;
                    await sleep(500);
                }
                res.check('audio: packets reach playback', depth > 0, depth);
            } finally {
                pulseUnload(tone);
            }
        }
        let pressed = await hold(host, page, 'x');
        res.check('input: key held in X keymap', pressed === true, pressed);
        await page.keyboard.up('x');
        await sleep(300);
        res.check('input: key released in X keymap', (await host.keyHeld(KEY_X)) === false, '');
        await within(page.evaluate(NATIVE_FIELD_JS));
        pressed = await hold(host, page, 'x');
        await page.mouse.click(...NATIVE_FIELD_AT);
        await page.keyboard.up('x');
        await sleep(500);
        res.check('input: a key let go in a dashboard field is released', pressed === true && (await host.keyHeld(KEY_X)) === false, pressed);
        await within(page.evaluate(() => document.getElementById('native-field')?.remove()));
        await page.mouse.click(640, 360);
        await sleep(300);

        const push = `e2e-${tag}-s2c`;
        const owner = await host.ownClipboard(push);
        let got: string[] = [];
        for (const until = Date.now() + (engine === 'webkit' ? 10_000 : 6000); Date.now() < until;) {
            got = await within(page.evaluate(() => (window.__clipMsgs ?? []).map((m) => m.text ?? '')));
            if (got.includes(push)) break;
            await page.waitForTimeout(500);
        }
        await owner.release();
        res.check('clipboard: server push reached page', got.includes(push), JSON.stringify(got).slice(-120));

        const probe = `e2e-${tag}-c2s`;
        await sendClipboardFromClient(page, probe, engine);
        if (engine === 'webkit') {
            // Headless WebKit strips clipboardData from synthetic paste events
            // and has no system clipboard; a skip claims no coverage.
            res.skip('clipboard: client text reached server', 'headless WebKit has no system clipboard');
        } else {
            let text: string | null = null;
            for (const until = Date.now() + 8000; Date.now() < until;) {
                text = await host.x11Clipboard(3);
                if (text === probe) break;
                await sleep(500);
            }
            res.check('clipboard: client text reached server', text === probe, JSON.stringify(text));
        }

        // Headless WebKit's viewport quirks make the realized root size unreliable.
        if (engine !== 'webkit') {
            const [reqW, reqH] = [1242, 694];
            await page.setViewportSize({ width: reqW, height: reqH });
            let realized: [number, number] | null = null;
            for (const until = Date.now() + 12_000; Date.now() < until;) {
                realized = await host.rootSize();
                if (realized && Math.abs(realized[0] - reqW) <= 16 && Math.abs(realized[1] - reqH) <= 64) break;
                await sleep(500);
            }
            res.check('resize: X root follows browser', realized !== null && Math.abs(realized[0] - reqW) <= 16, `req=${reqW}x${reqH} actual=${JSON.stringify(realized)}`);
        }

        if (mode === 'webrtc') {
            // Inside the window: the page caps the video at its container.
            for (const message of [{ type: 'setScaleLocally', value: false }, { type: 'setManualResolution', width: 1024, height: 576 }]) {
                await within(page.evaluate((m) => { window.postMessage(m, window.location.origin); }, message));
            }
            let size: number[] | null = null;
            for (const until = Date.now() + 15_000; Date.now() < until;) {
                size = await within(page.evaluate(() => {
                    const v = document.getElementById('stream');
                    return v instanceof HTMLVideoElement ? [v.videoWidth, v.videoHeight] : null;
                }));
                if (size?.[0] === 1024 && size[1] === 576) break;
                await sleep(500);
            }
            await sleep(1000);
            const rendering = await within(page.evaluate(() => document.getElementById('stream')?.style.imageRendering ?? null));
            const want = engine === 'chromium' ? 'pixelated' : 'auto';
            res.check('render: a 1:1 manual resolution is nearest-sampled in Chromium only', size?.[0] === 1024 && size[1] === 576 && rendering === want,
                `stream=${JSON.stringify(size)} rendering=${rendering} want=${want}`);
        }

        const realErrors = consoleErrors.filter((e) => !DECODER_ERROR_PATTERNS.some((p) => e.includes(p)));
        const bad404 = notFound.filter((u) => !(u.endsWith('/manifest.json') || u.includes('favicon')));
        res.check('no console errors (filtered)', realErrors.length === 0, realErrors.join('; ').slice(0, 160));
        res.check('no unexpected 404s', bad404.length === 0, bad404.slice(0, 2));
    } finally {
        await closeAll(browser, ctx);
        await host.stop();
    }
    return res;
}

interface StripedState {
    divert: boolean;
    rows: number;
    chunks: number;
    fps: number;
    enc: string | null;
    workerCanvas: { w: number; shown: boolean } | null;
    mainShown: boolean | null;
}

/**
 * Striped JPEG on one engine: the video worker decodes, composites, and
 * presents it off the page, fps counting the worker's composites and the
 * worker canvas the visible sink. JPEG sends only damaged stripes, so the
 * display churns: on a still one fps counts the first frame's composites for
 * a second and then reads 0.
 */
async function stripedBlock(host: Host, engine: Engine): Promise<Checks> {
    const res = new Checks(`${engine}-striped`);
    const { url } = await host.start({ label: `striped-${engine}`, env: { SELKIES_ENCODER: 'jpeg' } });
    const churn = await host.churn();
    try {
        const [browser, ctx] = await engineLaunch(engine);
        try {
            await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
            const page = await firstPage(engine, ctx);
            const pageErrors: string[] = [];
            page.on('pageerror', (e) => pageErrors.push(String(e)));
            await page.goto(url, { waitUntil: 'load' });
            // Polled, not sampled once: which counter is live depends on the
            // divert, and an engine that takes it optimistically and falls back
            // on the first decode failure flips between them while the stream
            // is coming up. The worker's own rows and composites are what
            // settle it, and they trail the first chunks by as long as its
            // decoders take to come up.
            for (const until = Date.now() + 30_000; Date.now() < until;) {
                await sleep(1000);
                const settled = await within(page.evaluate(() => {
                    const c = document.getElementById('videoWorkerCanvas');
                    return Boolean(window.videoDivertOn) && Object.keys(window.videoStripeRows ?? {}).length > 1
                        && (window.fps ?? 0) > 0 && c instanceof HTMLCanvasElement && c.width >= 640;
                }));
                if (settled) break;
            }
            const state = await within(page.evaluate((): StripedState => {
                const w = document.getElementById('videoWorkerCanvas');
                const m = document.getElementById('videoCanvas');
                return {
                    divert: Boolean(window.videoDivertOn),
                    rows: Object.keys(window.videoStripeRows ?? {}).length,
                    chunks: window.videoChunksReceived ?? 0,
                    fps: window.fps ?? 0,
                    enc: window.encoder ?? null,
                    workerCanvas: w instanceof HTMLCanvasElement ? { w: w.width, shown: w.style.display !== 'none' } : null,
                    mainShown: m ? m.style.display !== 'none' : null,
                };
            }));
            res.check('striped stream reaches the client', state.enc === 'jpeg' && state.chunks > 0, state);
            res.check('the video worker takes the striped divert', state.divert && state.rows > 1, state);
            res.check('the worker presents (fps counts its composites)', state.fps > 0, state);
            res.check('the worker canvas is the visible sink', state.workerCanvas !== null && state.workerCanvas.shown && state.workerCanvas.w >= 640 && !state.mainShown, state);
            res.check('no page errors', pageErrors.length === 0, pageErrors.join('; ').slice(0, 160));
        } finally {
            await closeAll(browser, ctx);
        }
    } finally {
        await churn.release();
        await host.stop();
    }
    return res;
}

/**
 * Where frames are presented is said, including when the sink is not the one
 * asked for. The video worker is the first choice on every engine, so a
 * session that ends up on the page canvas because the worker could not start
 * is the one whose sink most needs naming. The worker is blocked the way a
 * content policy that forbids blob workers blocks it: the constructor throws.
 *
 * The generator sink itself is out of reach here: it needs
 * VideoTrackGenerator in a worker, which WebKit gates on a preference that
 * defaults on under PLATFORM(COCOA) alone, so the build this matrix launches
 * reports a canvas sink however the shipping browser behaves. What a browser
 * that has it settles on is read from the line this check makes it print.
 */
async function sinkBlock(host: Host, engine: Engine): Promise<Checks> {
    const res = new Checks(`sink-${engine}`);
    const { url } = await host.start({ label: `sink-${engine}` });
    try {
        const browser = await launchBrowser(engine);
        try {
            const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
            await ctx.addInitScript({ content: "window.Worker = function () { throw new Error('blocked'); };" });
            const page = await ctx.newPage();
            const said: string[] = [];
            page.on('console', (m) => said.push(m.text()));
            await page.goto(url, { waitUntil: 'load' });
            res.check(`[${engine}] the stream plays without the worker`, Boolean(await waitWsVideo(page, 45)), '');
            const sinks = said.filter((t) => t.includes('video sink:'));
            res.check(`[${engine}] the sink it fell back to is named`, sinks.some((t) => t.includes('canvas on the page') || t.includes('MediaStreamTrackGenerator')), sinks.slice(0, 2));
            res.check(`[${engine}] and named once, not per handshake`, sinks.length === new Set(sinks).size, sinks);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

// Safari's default autoplay setting: a media element plays only once a user
// gesture has started it, muted or not; playback begun any other way is
// paused. The gesture is a trusted press or key within the last second, since
// the suite's autoplay switch has Chromium report user activation without one.
const GESTURE_ONLY_PLAY_JS = `(() => {
  let pressedAt = -Infinity;
  for (const type of ['pointerdown', 'pointerup', 'touchend', 'keydown', 'click']) {
    window.addEventListener(type, (e) => { if (e.isTrusted) pressedAt = performance.now(); }, true);
  }
  const play = HTMLMediaElement.prototype.play;
  const started = new WeakSet();
  const allowed = (el) => {
    if (performance.now() - pressedAt < 1000) started.add(el);
    return started.has(el);
  };
  HTMLMediaElement.prototype.play = function () {
    if (allowed(this)) return play.call(this);
    return Promise.reject(new DOMException('The request is not allowed by the user agent', 'NotAllowedError'));
  };
  document.addEventListener('play', (e) => { if (!allowed(e.target)) e.target.pause(); }, true);
})()`;

interface VideoSink {
    paused: boolean;
    frames: number;
    time: number;
}

function videoSink(page: Page): Promise<VideoSink | null> {
    return within(page.evaluate((): VideoSink | null => {
        const v = document.getElementById('videoStream');
        if (!(v instanceof HTMLVideoElement) || !v.srcObject || v.style.display === 'none') return null;
        return { paused: v.paused, frames: v.getVideoPlaybackQuality().totalVideoFrames, time: v.currentTime };
    }));
}

/**
 * A <video> sink the browser will not start without a user gesture starts on
 * the next press over the stream, as Safari asks of a page under its default
 * autoplay setting. Chromium stands in, with playback refused the way Safari
 * refuses it, since its generator sink feeds a <video> as Safari's does and
 * Playwright's WebKit has no generator sink at all. The still test display
 * sent its frames while the sink was paused, so what the started sink shows is
 * the key frame the press asks for.
 */
async function gestureBlock(host: Host, engine: Engine): Promise<Checks> {
    const res = new Checks(`gesture-${engine}`);
    const { url } = await host.start({ label: `gesture-${engine}` });
    try {
        const [browser, ctx] = await engineLaunch(engine);
        try {
            await ctx.addInitScript({ content: GESTURE_ONLY_PLAY_JS });
            const page = await ctx.newPage();
            const said: string[] = [];
            page.on('console', (m) => said.push(m.text()));
            await page.goto(url, { waitUntil: 'load' });
            res.check(`[${engine}] frames arrive`, Boolean(await waitWsVideo(page, 45)), '');
            let sink: VideoSink | null = null;
            for (const until = Date.now() + 15_000; Date.now() < until && sink === null;) {
                sink = await videoSink(page);
                await sleep(500);
            }
            if (sink === null) {
                res.skip(`[${engine}] <video> sink`, 'this browser presents through a canvas here');
                return res;
            }
            await sleep(3000);
            const before = await videoSink(page);
            res.check(`[${engine}] without a gesture the sink stays paused`, before?.paused, before);
            res.check(`[${engine}] and the refusal is said`, said.some((t) => t.includes('Video playback refused')), '');
            await page.mouse.click(640, 360);
            await sleep(3000);
            const after = await videoSink(page);
            res.check(`[${engine}] a press starts it`, after !== null && !after.paused, after);
            await sleep(2000);
            const later = await videoSink(page);
            res.check(`[${engine}] and frames are shown`, later !== null && before !== null && later.frames > before.frames, [before, later]);
            res.check(`[${engine}] the start is said`, said.some((t) => t.includes('Video playback started on a user gesture')), '');
        } finally {
            await closeAll(browser, ctx);
        }
    } finally {
        await host.stop();
    }
    return res;
}

/**
 * An engine without WebCodecs audio plays the stream's sound through libopus
 * in WASM: the decode worker says it fell back, and the playback worklet is
 * fed PCM that carries the desktop's tone. Surround comes as pcmflux's
 * multistream packets, split and decoded a stream at a time.
 */
async function wasmBlock(host: Host, engine: Engine, channels = 2): Promise<Checks> {
    const res = new Checks(`wasm-${engine}-${channels}ch`);
    const { url } = await host.start({ label: `wasm-${engine}-${channels}`, env: channels !== 2 ? { SELKIES_AUDIO_CHANNELS: String(channels) } : {} });
    try {
        const [browser, ctx] = await engineLaunch(engine);
        try {
            await ctx.addInitScript({ content: NO_WEBCODECS_AUDIO_JS });
            const page = await firstPage(engine, ctx);
            const said: string[] = [];
            const errors: string[] = [];
            page.on('console', (m) => said.push(m.text()));
            page.on('pageerror', (e) => errors.push(String(e)));
            await page.goto(url, { waitUntil: 'load' });
            res.check(`[${engine}] video up`, Boolean(await waitWsVideo(page, 45)), '');
            await page.mouse.click(640, 360);
            const tone = pulseSine();
            let depth = 0;
            let level = 0;
            try {
                for (const until = Date.now() + 15_000; Date.now() < until;) {
                    depth = await within(page.evaluate(() => window.currentAudioBufferSize || 0));
                    level = await within(page.evaluate(() => window.currentAudioLevel || 0));
                    if (depth > 0 && level > 0) break;
                    await sleep(500);
                }
            } finally {
                pulseUnload(tone);
            }
            res.check(`[${engine}] the decode worker fell back on libopus in WASM`, said.some((t) => t.includes('decodes on libopus in WASM')), said.filter((t) => t.includes('Audio Decoder')).slice(0, 3));
            res.check(`[${engine}] ${channels}-channel packets reach playback`, depth > 0, depth);
            res.check(`[${engine}] and play as sound, not silence`, level > 0, level);
            res.check(`[${engine}] no page errors`, errors.length === 0, errors.join('; ').slice(0, 200));
        } finally {
            await closeAll(browser, ctx);
        }
    } finally {
        await host.stop();
    }
    return res;
}

/**
 * The output device a page picks carries its sound. Two null sinks stand in
 * for speakers; the page is told to play to one and then the other
 * (audioDeviceSelected), and the desktop's tone is heard on that sink's
 * monitor and not on the other's. Chromium picks through
 * AudioContext.setSinkId, Firefox, which has none, through a media element's.
 */
async function outputBlock(host: Host, engine: Engine): Promise<Checks> {
    const res = new Checks(`output-${engine}`);
    const names = ['selkies_pick_a', 'selkies_pick_b'] as const;
    const sinks = names.map((n) => pulseNullSink(n, { sink_properties: `device.description=${n}` }));
    let tone: string | null = null;
    const { url } = await host.start({ label: `output-${engine}` });
    try {
        let browser: Browser | null;
        let ctx: BrowserContext;
        if (engine === 'chromium') {
            // Heard, not muted: the shared flags mute Chromium's output, and
            // headless Chromium plays nothing to the sound server, so it runs
            // on the test display.
            const executablePath = process.env.E2E_CHROME || undefined;
            browser = await chromium.launch({ headless: false, args: BROWSER_ARGS.filter((a) => a !== '--mute-audio'),
                env: { ...process.env, DISPLAY: process.env.E2E_DISPLAY ?? '' }, ...(executablePath ? { executablePath } : {}) });
            ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
            await ctx.grantPermissions(['microphone'], { origin: url });
        } else {
            // The microphone grant that names the outputs, without a prompt no one answers.
            [browser, ctx] = await engineLaunch(engine, { 'media.navigator.permission.disabled': true });
        }
        try {
            const page = await firstPage(engine, ctx);
            const errors: string[] = [];
            const said: string[] = [];
            page.on('pageerror', (e) => errors.push(String(e)));
            page.on('console', (m) => { if (m.text().toLowerCase().includes('output')) said.push(m.text()); });
            await page.goto(url, { waitUntil: 'load' });
            res.check(`[${engine}] video up`, Boolean(await waitWsVideo(page, 45)), '');
            await page.mouse.click(640, 360);
            tone = pulseSine();
            // Output devices are named to a page that holds a microphone grant.
            const outputs = await within(page.evaluate(async () => {
                try {
                    const s = await navigator.mediaDevices.getUserMedia({ audio: true });
                    s.getTracks().forEach((t) => { t.stop(); });
                } catch {
                    // named or not, list them
                }
                return (await navigator.mediaDevices.enumerateDevices()).filter((d) => d.kind === 'audiooutput').map((d): [string, string] => [d.deviceId, d.label]);
            }));
            for (const [i, target] of names.entries()) {
                const other = names[1 - i];
                const device = outputs.find((d) => d[1].includes(target));
                res.check(`[${engine}] ${target} is offered as an output`, device !== undefined, outputs);
                if (!device) continue;
                await within(page.evaluate((id) => { window.postMessage({ type: 'audioDeviceSelected', context: 'output', deviceId: id }, location.origin); }, device[0]));
                await sleep(2500);
                const heard = Object.fromEntries(names.map((n) => [n, analyze(record(`${n}.monitor`, 2), 440)]));
                res.check(`[${engine}] picked, ${target} plays the desktop's tone`, heard[target].rms > 150 && heard[target].ratio > 0.5, [heard[target], said.slice(-2)]);
                res.check(`[${engine}] and ${other} goes quiet`, heard[other].rms < 50, heard[other]);
            }
            res.check(`[${engine}] no page errors`, errors.length === 0, errors.join('; ').slice(0, 200));
        } finally {
            await closeAll(browser, ctx);
        }
    } finally {
        pulseUnload(tone);
        for (const id of sinks) pulseUnload(id);
        await host.stop();
    }
    return res;
}

const RUNS: Record<string, (host: Host) => Promise<Checks[]>> = {
    'chromium-ws': async (h) => [await engineBlock(h, 'chromium')],
    'firefox-ws': async (h) => [await engineBlock(h, 'firefox')],
    'webkit-ws': async (h) => [await engineBlock(h, 'webkit')],
    sink: async (h) => [await sinkBlock(h, 'webkit'), await sinkBlock(h, 'chromium')],
    gesture: async (h) => [await gestureBlock(h, 'chromium')],
    output: async (h) => [await outputBlock(h, 'chromium'), await outputBlock(h, 'firefox')],
    wasm: async (h) => [await wasmBlock(h, 'chromium'), await wasmBlock(h, 'firefox'), await wasmBlock(h, 'webkit'), await wasmBlock(h, 'chromium', 6)],
    striped: async (h) => [await stripedBlock(h, 'chromium'), await stripedBlock(h, 'firefox'), await stripedBlock(h, 'webkit')],
    'chromium-wr': async (h) => [await engineBlock(h, 'chromium', 'webrtc')],
    'firefox-wr': async (h) => {
        if (openh264Version()) return [await engineBlock(h, 'firefox', 'webrtc')];
        const res = new Checks('firefox-webrtc');
        res.skip('firefox webrtc', `no OpenH264 GMP plugin in ${FF_E2E_PROFILE}; run tests/tools/fetch-openh264.sh to cover H.264 in Firefox`);
        return [res];
    },
};

for (const [which, run] of Object.entries(RUNS)) {
    test(which, async () => {
        const host = new Host(suiteOf(import.meta.filename));
        const failures: string[] = [];
        try {
            for (const res of await run(host)) {
                try {
                    res.finish();
                } catch (e) {
                    failures.push(String(e));
                }
            }
        } finally {
            await host.close();
        }
        if (failures.length) throw new Error(failures.join('\n'));
    });
}
