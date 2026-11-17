/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Client behaviors that must not differ by transport, driven from a dpr-2 browser.
//
// Resolution: an auto-mode HiDPI client asks for the window's physical size, a
// manual preset is requested as exact framebuffer pixels and shown, with
// "scale locally" off, at one stream pixel per device pixel, reset-to-window
// returns to the physical window size, and turning "scale locally" on in auto
// mode leaves the window-resize listener armed. The WebRTC video is drawn
// nearest-sampled exactly while it is shown 1:1, and smoothed once scaled to
// fit. A pixel ratio changed through DevTools emulation re-requests the stream
// at it. HiDPI: the flag either streams physical pixels and scales the
// desktop, or leaves the desktop unscaled and divides the request by the
// UI-scaling pick for the browser to stretch back -- never both. Clipboard: a
// server with the clipboard disabled must not arm the focus read (Chromium's
// permission prompt) or send any clipboard payload. Gamepad: a pad present
// before the channel opens honors the persisted gamepad toggle, and one pad's
// disconnect does not stop polling the others. Resize policy: with dynamic
// resizing disabled a manual resolution posted to the primary's page is
// neither requested nor applied, and the desktop keeps its size. Rendering:
// changing anti-aliasing applies to the visible sink immediately, including a
// static desktop that sends no new video frames. Title: without ui_title the
// browser tab takes the web root manifest's name, and an explicit ui_title
// names it instead.
//
// The checks read the wire: r,WxH / js,* / cw,cb messages are tapped at
// WebSocket.send and RTCDataChannel.send inside the page, clipboard reads at
// Clipboard.prototype, and the X root size confirms what the server realized.
//
// Both cores are checked: the websockets core is the reference the checks
// were written against, and the webrtc core must match it.
import { spawnSync } from 'node:child_process';
import { cpSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import type { Browser, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { CORE_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { buttonReports, PADS_INIT, PARITY_TAP_JS as WIRE_TAP, STORAGE_APP } from '../lib/pads.ts';
import { waitWrVideo, waitWsVideo, within, type VideoSize } from '../lib/stream.ts';

// CSS-px viewport of the dpr-2 browser: the physical size the auto path must
// request is twice this.
const VIEW_W = 1000;
const VIEW_H = 700;
const DPR = 2;
const PRESET_W = 1280;
const PRESET_H = 720;
const UI_TITLE = 'Parity Title';
// Unlike the page's own <title>, so the tab shows whether the manifest was read.
const MANIFEST_NAME = 'Parity Manifest';
// A size X11's modes round up (to 8-pixel cells), which the stream still carries exactly.
const ODD_W = 1278;
const ODD_H = 712;
const RESIZED_W = 1100;
const RESIZED_H = 680;
// A manual resolution the local density would never ask for, and the pick it
// derives: its shorter side against the 1080 rows 96 DPI is for. Neither 96
// nor the dpr's 192, so the desktop's DPI tells the two rules apart.
const MANUAL_W = 2560;
const MANUAL_H = 1440;
const MANUAL_DPI = 120;

declare global {
    interface Window {
        __resSent?: string[];
        __clipSent?: number;
        __clipReads?: number;
        __dprEvents?: string[];
        manualResolution?: boolean;
        manual_resolution?: boolean;
        clipboard_enabled?: boolean;
    }
}

interface Box {
    width: number;
    height: number;
    left: number;
    top: number;
}

interface Run {
    res: Checks;
    host: Host;
    url: string;
    mode: string;
}

const ev = <T>(page: Page, script: string): Promise<T> => within(page.evaluate<T>(script));

/** A dpr-2 page on the test server with the wire taps installed, loaded. */
async function newPage(browser: Browser, { url, mode }: Run, extraInit: string[] = []): Promise<Page> {
    const ctx = await browser.newContext({ viewport: { width: VIEW_W, height: VIEW_H }, deviceScaleFactor: DPR, permissions: [] });
    await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: url }).catch(() => undefined);
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: WIRE_TAP });
    for (const script of extraInit) await ctx.addInitScript({ content: script });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    // The dashboard can constrain media globally, as its Tailwind preflight does.
    await page.addStyleTag({ content: 'video { max-width: 100%; height: auto; }' });
    return page;
}

function waitVideo(page: Page, mode: string): Promise<VideoSize | null> {
    return mode === 'webrtc' ? waitWrVideo(page) : waitWsVideo(page);
}

/** Whether a realized root size is within the CVT cell of `w`x`h`. */
function rootMatches(realized: [number, number] | null, w: number, h: number): boolean {
    return realized !== null && Math.abs(realized[0] - w) <= 16 && Math.abs(realized[1] - h) <= 16;
}

/** Poll the X root until it is within the CVT cell of `w`x`h`; the last size read. */
async function waitRoot(host: Host, w: number, h: number, timeoutS = 15): Promise<[number, number] | null> {
    let realized = await host.rootSize();
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        realized = await host.rootSize();
        if (rootMatches(realized, w, h)) return realized;
        await sleep(300);
    }
    return realized;
}

/**
 * Poll the resource database until Xft.dpi reads `want`; the last DPI read,
 * 96 where the resource is unset: X's own default, which is what an
 * application reads when nothing overrides it.
 */
async function waitDpi(want: number, timeoutS = 10): Promise<number> {
    let dpi = 96;
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        const out = spawnSync('xrdb', ['-query'], { encoding: 'utf8', env: { ...process.env, DISPLAY: process.env.E2E_DISPLAY ?? '' } }).stdout ?? '';
        const line = out.split('\n').find((l) => l.startsWith('Xft.dpi'));
        dpi = line ? Math.trunc(Number(line.split(':', 2)[1])) : 96;
        if (dpi === want) return dpi;
        await sleep(300);
    }
    return dpi;
}

const resSent = (page: Page): Promise<string[]> => within(page.evaluate(() => window.__resSent ?? []));

/** Poll until the page has put more than `seen` r, requests on the wire; every WxH requested so far. */
async function waitNewRequest(page: Page, seen: number, timeoutS = 8): Promise<string[]> {
    let sent = await resSent(page);
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until && sent.length <= seen;) {
        await sleep(250);
        sent = await resSent(page);
    }
    return sent;
}

/** Post a dashboard message into the page. */
async function post(page: Page, message: Record<string, unknown>): Promise<void> {
    await within(page.evaluate((m) => { window.postMessage(m, window.location.origin); }, message));
}

/** The focus event Chromium's local->server clipboard read hangs on. */
async function focusGesture(page: Page): Promise<void> {
    await page.bringToFront();
    await within(page.evaluate(() => window.dispatchEvent(new Event('focus'))));
}


/**
 * The visible sink relative to its container. The WebSockets canvas can be
 * hidden behind a video or worker canvas, so the sink actually displayed is
 * measured: inline dimensions alone do not catch a stylesheet clamping the
 * video while leaving its centering offsets intact.
 */
function sinkBox(page: Page, mode: string): Promise<Box | null> {
    const sinks = mode === 'webrtc' ? ['stream'] : ['videoStream', 'videoWorkerCanvas', 'videoCanvas'];
    return within(page.evaluate((ids): Box | null => {
        const el = ids.map((id) => document.getElementById(id)).find((e) => e !== null && getComputedStyle(e).display !== 'none' && e.getBoundingClientRect().width > 0);
        if (!el?.parentElement) return null;
        const box = el.getBoundingClientRect();
        const parent = el.parentElement.getBoundingClientRect();
        return { width: box.width, height: box.height, left: box.left - parent.left, top: box.top - parent.top };
    }, sinks));
}

/** The stream's own pixel size, polled until it is `want` or time runs out. */
async function streamSize(page: Page, mode: string, want: [number, number], timeoutS = 15): Promise<[number, number] | null> {
    let size: [number, number] | null = null;
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        size = await within(page.evaluate((m): [number, number] | null => {
            const el = document.getElementById(m === 'webrtc' ? 'stream' : 'videoCanvas');
            if (el instanceof HTMLVideoElement) return [el.videoWidth, el.videoHeight];
            if (el instanceof HTMLCanvasElement) return [el.width, el.height];
            return null;
        }, mode));
        if (size?.[0] === want[0] && size[1] === want[1]) break;
        await sleep(300);
    }
    return size;
}

const sameSize = (a: [number, number] | null, w: number, h: number): boolean => a?.[0] === w && a[1] === h;

/** The WebRTC video's image-rendering, polled until it is `want`: the rule reruns when the stream's own size settles after the box. */
async function videoRendering(page: Page, want: string, timeoutS = 8): Promise<string | null> {
    let got: string | null = null;
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        got = await within(page.evaluate(() => document.getElementById('stream')?.style.imageRendering ?? null));
        if (got === want) break;
        await sleep(300);
    }
    return got;
}

interface Presented {
    rendering: string;
    frames: number;
}

/**
 * Change the visible filter in both directions while the desktop is idle. A
 * canvas hidden behind a video can hold the requested filter even while the
 * visible sink keeps its previous one. The video and its presented frame
 * count are read, so a later frame cannot make a stale filter appear correct.
 */
async function staticRenderingBlock(page: Page, { res, mode }: Run): Promise<void> {
    await post(page, { type: 'setManualResolution', width: PRESET_W, height: PRESET_H });
    await streamSize(page, mode, [PRESET_W, PRESET_H]);
    await post(page, { type: 'setScaleLocally', value: true });
    await post(page, { type: 'setAntiAliasing', value: true });
    await post(page, { type: 'settings', settings: { video_streaming_mode: false, use_paint_over_quality: false } });
    const probe = (): Promise<Presented | null> => within(page.evaluate((): Presented | null => {
        const v = [...document.querySelectorAll('video')].find((x) => getComputedStyle(x).display !== 'none' && x.videoWidth > 0);
        return v ? { rendering: getComputedStyle(v).imageRendering, frames: v.getVideoPlaybackQuality().totalVideoFrames } : null;
    }));
    try {
        let before: Presented | null = null;
        let after: Presented | null = null;
        for (const until = Date.now() + 15_000; Date.now() < until;) {
            before = await probe();
            await sleep(1000);
            after = await probe();
            if (before && after && before.frames === after.frames) break;
        }
        const idle = before !== null && after !== null && before.frames === after.frames;
        res.check('static rendering: the visible video receives no new frames', idle, after);
        if (!idle) return;
        for (const enabled of [false, true]) {
            before = await probe();
            await post(page, { type: 'setAntiAliasing', value: enabled });
            await sleep(200);
            after = await probe();
            const accepted = enabled ? ['auto'] : ['pixelated', 'crisp-edges'];
            const shown = enabled ? 'True' : 'False';
            res.check(`static rendering: anti-aliasing ${shown} reaches the visible video`, after !== null && accepted.includes(after.rendering), after);
            res.check(`static rendering: anti-aliasing ${shown} needs no new frame`, after !== null && before !== null && after.frames === before.frames, after);
        }
    } finally {
        await post(page, { type: 'settings', settings: { use_paint_over_quality: true } });
    }
}

/** Auto -> preset -> exact box -> reset -> scale-locally + resize, at dpr 2. */
async function resolutionBlock(page: Page, { res, host, mode }: Run): Promise<void> {
    const [physW, physH] = [VIEW_W * DPR, VIEW_H * DPR];
    // The first realization rides the whole cold path; loaded runners stretch it.
    let realized = await waitRoot(host, physW, physH, 30);
    res.check('auto mode requests the physical window size', rootMatches(realized, physW, physH), `root=${JSON.stringify(realized)}`);
    let sent = await resSent(page);
    res.check('auto request on the wire is WxH * dpr', sent.includes(`${physW}x${physH}`), sent);
    if (mode === 'webrtc') {
        const got = await videoRendering(page, 'pixelated');
        res.check("a stream at the window's device pixels is drawn 1:1", got === 'pixelated', got);
    }

    let seen = sent.length;
    await post(page, { type: 'setManualResolution', width: PRESET_W, height: PRESET_H });
    sent = await waitNewRequest(page, seen);
    res.check('manual preset is requested as exact pixels', sent.length > seen && sent[sent.length - 1] === `${PRESET_W}x${PRESET_H}`, sent.slice(seen));
    realized = await waitRoot(host, PRESET_W, PRESET_H);
    res.check('manual preset realized on the server', rootMatches(realized, PRESET_W, PRESET_H), `root=${JSON.stringify(realized)}`);

    // Scale-locally off is one stream pixel per device pixel.
    await post(page, { type: 'setScaleLocally', value: false });
    await sleep(300);
    const box = await sinkBox(page, mode);
    const [wantW, wantH] = [PRESET_W / DPR, PRESET_H / DPR];
    const fits = box !== null && Math.abs(box.width - wantW) < 1 && Math.abs(box.height - wantH) < 1
        && box.left >= 0 && box.top >= 0 && box.left + box.width <= VIEW_W && box.top + box.height <= VIEW_H;
    res.check('exact box is the preset over the density, inside the viewport', fits, `${JSON.stringify(box)} want ${wantW}x${wantH}`);
    await post(page, { type: 'setUseCssScaling', value: true });
    await sleep(300);
    const offBox = await sinkBox(page, mode);
    res.check('exact box ignores the HiDPI flag', offBox !== null && JSON.stringify(offBox) === JSON.stringify(box), `${JSON.stringify(offBox)} after HiDPI off, ${JSON.stringify(box)} before`);
    await post(page, { type: 'setUseCssScaling', value: false });
    await sleep(300);
    if (mode === 'webrtc') {
        let got = await videoRendering(page, 'pixelated');
        res.check('an exact manual box is drawn 1:1', got === 'pixelated', got);
        await post(page, { type: 'setScaleLocally', value: true });
        got = await videoRendering(page, 'auto');
        res.check('a manual resolution scaled to fit is smoothed', got === 'auto', got);
        await post(page, { type: 'setScaleLocally', value: false });
        await sleep(300);
    }
    sent = await resSent(page);

    // An exact 4K stream exceeds this viewport even at DPR 2.
    seen = sent.length;
    await post(page, { type: 'setManualResolution', width: 3840, height: 2160 });
    await waitNewRequest(page, seen);
    let size = await streamSize(page, mode, [3840, 2160]);
    res.check('oversized manual stream is realized', sameSize(size, 3840, 2160), size);
    for (const fit of [false, true, false]) {
        await post(page, { type: 'setScaleLocally', value: fit });
        await sleep(300);
        const b = await sinkBox(page, mode);
        const [w, h] = fit ? [VIEW_W, VIEW_W * 2160 / 3840] : [3840 / DPR, 2160 / DPR];
        const expected: Box = { width: w, height: h, left: (VIEW_W - w) / 2, top: (VIEW_H - h) / 2 };
        const matches = b !== null && (['width', 'height', 'left', 'top'] as const).every((k) => Math.abs(b[k] - expected[k]) < 1);
        res.check(`oversized visible sink follows scale-locally=${fit ? 'True' : 'False'}`, matches, `${JSON.stringify(b)} want ${JSON.stringify(expected)}`);
    }
    sent = await resSent(page);

    seen = sent.length;
    await post(page, { type: 'setManualResolution', width: ODD_W, height: ODD_H });
    await waitNewRequest(page, seen);
    realized = await waitRoot(host, ODD_W, ODD_H);
    size = await streamSize(page, mode, [ODD_W, ODD_H]);
    res.check('a size the mode rounds up streams at the size asked for', sameSize(size, ODD_W, ODD_H), `stream ${JSON.stringify(size)}, root ${JSON.stringify(realized)}`);
    sent = await resSent(page);

    seen = sent.length;
    await post(page, { type: 'resetResolutionToWindow' });
    sent = await waitNewRequest(page, seen);
    res.check('reset-to-window re-requests the physical window size', sent.length > seen && sent[sent.length - 1] === `${physW}x${physH}`, sent.slice(seen));
    realized = await waitRoot(host, physW, physH);
    res.check('reset-to-window realized on the server', rootMatches(realized, physW, physH), `root=${JSON.stringify(realized)}`);
    const manual = await within(page.evaluate(() => window.manualResolution || window.manual_resolution || false));
    res.check('reset-to-window leaves manual mode', manual === false, manual);

    seen = sent.length;
    await post(page, { type: 'setScaleLocally', value: true });
    await sleep(300);
    const persisted = await ev<string | null>(page, `localStorage.getItem(${STORAGE_APP} + '_scaleLocallyManual')`);
    res.check('scale-locally choice persisted', persisted === 'true', persisted);
    await page.setViewportSize({ width: RESIZED_W, height: RESIZED_H });
    sent = await waitNewRequest(page, seen);
    const want = `${RESIZED_W * DPR}x${RESIZED_H * DPR}`;
    res.check('scale-locally in auto mode keeps the window resize armed', sent.length > seen && sent[sent.length - 1] === want, sent.slice(seen));
    realized = await waitRoot(host, RESIZED_W * DPR, RESIZED_H * DPR);
    res.check('window resize realized on the server', rootMatches(realized, RESIZED_W * DPR, RESIZED_H * DPR), `root=${JSON.stringify(realized)}`);
}

/**
 * A pixel ratio changed through DevTools emulation re-requests the stream at
 * the new density, as a real display change does, and changing it back
 * restores the physical size. Such a change fires no resize and, in Chromium,
 * no resolution media query, so only the page's poll of the live value sees it.
 */
async function emulatedDensityBlock(page: Page, { res }: Run): Promise<void> {
    const cdp = await page.context().newCDPSession(page);
    const events = await within(page.evaluate(() => {
        window.__dprEvents = [];
        addEventListener('resize', () => window.__dprEvents?.push('resize'));
        matchMedia(`(resolution: ${devicePixelRatio}dppx)`).addEventListener('change', () => window.__dprEvents?.push('query'));
        return true;
    }));
    for (const [dsf, want] of [[1, `${RESIZED_W}x${RESIZED_H}`], [DPR, `${RESIZED_W * DPR}x${RESIZED_H * DPR}`]] as const) {
        const seen = (await resSent(page)).length;
        await cdp.send('Emulation.setDeviceMetricsOverride', { width: RESIZED_W, height: RESIZED_H, deviceScaleFactor: dsf, mobile: false });
        const sent = await waitNewRequest(page, seen);
        const fired = events ? await within(page.evaluate(() => window.__dprEvents?.splice(0) ?? [])) : null;
        res.check(`an emulated pixel ratio of ${dsf} re-requests the stream at it`, sent.length > seen && sent[sent.length - 1] === want, `${JSON.stringify(sent.slice(seen))} want ${want}; events ${JSON.stringify(fired)}`);
    }
    // Left attached at the context's own metrics: detaching drops the
    // override, and the page with it to the bare window's size.
}

/**
 * The HiDPI flag decides one thing, on either transport. On, the stream is
 * the window's physical pixels and the desktop is scaled to the UI-scaling
 * pick, so the remote UI comes out the size of the local one. Off, the remote
 * UI is not scaled and the pick divides the resolution asked for instead,
 * which the browser stretches back: scaling on both sides at once drew the
 * remote UI at the pick twice over. A manual resolution is the exact
 * framebuffer either way -- a toggle must not swing the number the operator
 * asked for -- so there the pick has no request to divide and reaches the
 * desktop whatever the flag says, which is one application and not two. The
 * pick here is the automatic default, this display's own scaling as a DPI.
 */
async function hidpiBlock(page: Page, { res }: Run): Promise<void> {
    const pick = DPR * 96;
    const [cssW, cssH] = await within(page.evaluate((): [number, number] => [window.innerWidth, window.innerHeight]));
    const onDpi = await waitDpi(pick);
    res.check('HiDPI on scales the desktop to the pick', onDpi === pick, `Xft.dpi=${onDpi} want ${pick}`);

    let seen = (await resSent(page)).length;
    await post(page, { type: 'setUseCssScaling', value: true });
    let sent = await waitNewRequest(page, seen);
    const offRequest = sent.length > seen ? sent[sent.length - 1] : '';
    res.check('HiDPI off asks for the window\'s CSS size', offRequest === `${cssW}x${cssH}`, sent.slice(seen));
    const offDpi = await waitDpi(96);
    res.check('HiDPI off leaves the desktop unscaled', offDpi === 96, `Xft.dpi=${offDpi}`);

    // In auto mode the flag decides sharpness, not size: a window is drawn at
    // the DPI and shown at the CSS box over the stream, so the same ratio
    // either way is the same window on screen -- half the pixels, and the
    // desktop rescaled by nothing.
    const [onPx, offPx] = [cssW * DPR, cssW];
    res.check('the flag leaves a window the size it had', offDpi * onPx === onDpi * offPx, `${offDpi} DPI over ${offPx}px vs ${onDpi} over ${onPx}px`);

    // A manual resolution is the exact framebuffer whatever the flag says, so
    // the pick has no request to divide there and governs the desktop
    // instead: one application, not the two CSS scaling used to make. On its
    // automatic default the pick is that framebuffer's own, since it decides
    // how large the desktop draws its UI and the screen showing it says
    // nothing about it.
    seen = sent.length;
    await post(page, { type: 'setManualResolution', width: MANUAL_W, height: MANUAL_H });
    sent = await waitNewRequest(page, seen);
    res.check('HiDPI off asks for a manual resolution exactly', sent.length > seen && sent[sent.length - 1] === `${MANUAL_W}x${MANUAL_H}`, sent.slice(seen));
    const manualDpi = await waitDpi(MANUAL_DPI);
    res.check("and the resolution's own pick reaches the desktop there", manualDpi === MANUAL_DPI, `Xft.dpi=${manualDpi} want ${MANUAL_DPI}`);

    seen = sent.length;
    await post(page, { type: 'resetResolutionToWindow' });
    sent = await waitNewRequest(page, seen);
    res.check('back on the window size the request is physical again', sent.length > seen && sent[sent.length - 1] === `${cssW}x${cssH}`, sent.slice(seen));
    const windowDpi = await waitDpi(96);
    res.check('and the desktop is unscaled again', windowDpi === 96, `Xft.dpi=${windowDpi}`);

    seen = sent.length;
    await post(page, { type: 'setUseCssScaling', value: false });
    sent = await waitNewRequest(page, seen);
    res.check('HiDPI on again asks for the physical size', sent.length > seen && sent[sent.length - 1] === `${cssW * DPR}x${cssH * DPR}`, sent.slice(seen));
    const backDpi = await waitDpi(pick);
    res.check('HiDPI on again scales the desktop', backDpi === pick, `Xft.dpi=${backDpi} want ${pick}`);
}

/** With the clipboard on, the focus gesture reads the local clipboard. */
async function clipboardEnabledBlock(page: Page, { res }: Run): Promise<void> {
    const enabled = await within(page.evaluate(() => window.clipboard_enabled));
    res.check('clipboard_enabled mirrored from the server (on)', enabled === true, enabled);
    await focusGesture(page);
    let reads = 0;
    for (const until = Date.now() + 5000; Date.now() < until && !reads;) {
        reads = await within(page.evaluate(() => window.__clipReads ?? 0));
        await sleep(250);
    }
    res.check('clipboard on: focus gesture reads the local clipboard', reads > 0, reads);
}

interface Field {
    mode: string | null;
    policy?: string | null;
    type: string;
    readOnly?: boolean;
}

/**
 * The overlay collects the stream's taps without opening a soft keyboard. It
 * is a real text input laid over the video, so a mobile engine would open its
 * keyboard on every tap of the session -- over the picture, and with no way to
 * dismiss it. Focus, key events, and IME composition are unaffected by these
 * two attributes; the off-screen assist input is what deliberately opens one,
 * so it must not carry them.
 */
async function softKeyboardBlock(page: Page, { res }: Run): Promise<void> {
    const overlay = await within(page.evaluate((): Field | null => {
        const e = document.getElementById('overlayInput');
        return e instanceof HTMLInputElement || e instanceof HTMLTextAreaElement
            ? { mode: e.getAttribute('inputmode'), policy: e.getAttribute('virtualkeyboardpolicy'), type: e.type, readOnly: e.readOnly }
            : null;
    }));
    res.check('the stream overlay asks for no virtual keyboard', overlay?.mode === 'none', JSON.stringify(overlay));
    res.check("the stream overlay keeps the keyboard the page's to open", overlay?.policy === 'manual', JSON.stringify(overlay));
    res.check('the stream overlay stays editable, for IME composition', overlay?.readOnly === false, JSON.stringify(overlay));
    const assist = await within(page.evaluate((): Field | null => {
        const e = document.getElementById('keyboard-input-assist');
        return e instanceof HTMLInputElement ? { mode: e.getAttribute('inputmode'), type: e.type } : null;
    }));
    res.check('the assist input still opens one on purpose', assist !== null && (assist.mode === null || assist.mode === 'text'), JSON.stringify(assist));
    res.check('both cores build the assist input the same way', assist?.type === 'search', JSON.stringify(assist));
}

const press = async (page: Page, p: number, i: number, v: number): Promise<void> => {
    await within(page.evaluate(`window.__padPress(${p}, ${i}, ${v})`));
};

/** Persisted toggle before channel open, then a disconnect of one pad. */
async function gamepadBlock(browser: Browser, run: Run): Promise<void> {
    const { res, mode } = run;
    const toggleOff = `localStorage.setItem(${STORAGE_APP} + '_isGamepadEnabled', 'false');`;
    const page = await newPage(browser, run, [PADS_INIT, toggleOff]);
    try {
        res.check('gamepad page: video flowing', Boolean(await waitVideo(page, mode)));
        await sleep(1000);
        await press(page, 0, 0, 1);
        await sleep(400);
        await press(page, 0, 0, 0);
        await sleep(600);
        const before = await buttonReports(page);
        res.check('persisted toggle off: a pad present before connect is not polled', before === 0, before);

        await post(page, { type: 'gamepadControl', enabled: true });
        await sleep(300);
        await press(page, 0, 1, 1);
        await sleep(400);
        await press(page, 0, 1, 0);
        await sleep(600);
        const afterOn = await buttonReports(page);
        res.check('toggle on: button reports reach the wire', afterOn > before, afterOn);

        await within(page.evaluate("window.__pads[1] = null; window.dispatchEvent(new Event('gamepaddisconnected'));"));
        await sleep(500);
        await press(page, 0, 2, 1);
        await sleep(400);
        await press(page, 0, 2, 0);
        await sleep(600);
        const afterDc = await buttonReports(page);
        res.check("one pad's disconnect keeps the other pad polled", afterDc > afterOn, `${afterOn} -> ${afterDc}`);
    } finally {
        await page.context().close();
    }
}

/** enable_clipboard=false: no local read is armed and nothing is sent. */
async function clipboardDisabledBlock(browser: Browser, run: Run): Promise<void> {
    const { res, mode } = run;
    const page = await newPage(browser, run);
    try {
        res.check('clipboard-off page: video flowing', Boolean(await waitVideo(page, mode)));
        await sleep(1000);
        const enabled = await within(page.evaluate(() => window.clipboard_enabled));
        res.check('clipboard_enabled mirrored from the server (off)', enabled === false, enabled);
        await focusGesture(page);
        await within(page.evaluate(() => { navigator.clipboard.writeText('parity-probe').catch(() => undefined); }));
        await focusGesture(page);
        await post(page, { type: 'clipboardUpdateFromUI', text: 'parity-probe-ui' });
        await sleep(2500);
        const reads = await within(page.evaluate(() => window.__clipReads ?? 0));
        const sent = await within(page.evaluate(() => window.__clipSent ?? 0));
        res.check('clipboard off: focus gesture reads nothing', reads === 0, reads);
        res.check('clipboard off: no clipboard payload sent', sent === 0, sent);
    } finally {
        await page.context().close();
    }
}

/**
 * enable_resize=false: a manual resolution posted to the primary's page, as an
 * embedding front end posts one, is neither requested nor applied, and the
 * desktop keeps its size.
 */
async function pinnedBlock(browser: Browser, run: Run): Promise<void> {
    const { res, host, mode } = run;
    const page = await newPage(browser, run);
    try {
        res.check('pinned page: video flowing', Boolean(await waitVideo(page, mode)));
        await sleep(1000);
        const root = await host.rootSize();
        const seen = (await resSent(page)).length;
        await post(page, { type: 'setManualResolution', width: PRESET_W, height: PRESET_H });
        await sleep(4000);
        const sent = (await resSent(page)).slice(seen);
        res.check('pinned: a manual resolution is not requested', sent.length === 0, sent);
        const manual = await within(page.evaluate(() => window.manualResolution || window.manual_resolution || false));
        res.check('pinned: the page stays out of manual mode', manual === false, manual);
        const after = await host.rootSize();
        res.check('pinned: the desktop keeps its size', JSON.stringify(after) === JSON.stringify(root), `${JSON.stringify(root)} -> ${JSON.stringify(after)}`);
    } finally {
        await page.context().close();
    }
}

/** A copy of the core's web client whose manifest names the app MANIFEST_NAME. */
function manifestWebRoot(): string {
    const root = join(mkdtempSync(join(tmpdir(), 'selkies-core-parity-')), 'web');
    cpSync(CORE_DIST, root, { recursive: true });
    writeFileSync(join(root, 'manifest.json'), JSON.stringify({ name: MANIFEST_NAME }));
    return root;
}

/** ui_title set: the tab takes it over the manifest's name. */
async function titleBlock(browser: Browser, run: Run): Promise<void> {
    const { res, mode } = run;
    const page = await newPage(browser, run);
    try {
        res.check('titled page: video flowing', Boolean(await waitVideo(page, mode)));
        const title = await page.title();
        res.check('ui_title names the tab', title === UI_TITLE, title);
    } finally {
        await page.context().close();
    }
}

for (const mode of ['webrtc', 'websockets']) {
    test(mode, async () => {
        const res = new Checks(`core-parity-${mode}`);
        const host = new Host(suiteOf(import.meta.filename));
        const webRoot = manifestWebRoot();
        try {
            const first = await host.start({ label: mode, mode, env: { SELKIES_WEB_ROOT: webRoot } });
            let run: Run = { res, host, url: first.url, mode };
            let browser = await launchChromium();
            try {
                const page = await newPage(browser, run);
                res.check('video flowing', Boolean(await waitVideo(page, mode)));
                await sleep(1000);
                const title = await page.title();
                res.check("without ui_title the tab takes the manifest's name", title === MANIFEST_NAME, title);
                await resolutionBlock(page, run);
                await emulatedDensityBlock(page, run);
                await hidpiBlock(page, run);
                await staticRenderingBlock(page, run);
                await clipboardEnabledBlock(page, run);
                await softKeyboardBlock(page, run);
                await page.context().close();
                await gamepadBlock(browser, run);
            } finally {
                await browser.close();
            }
            await host.stop();
            const second = await host.start({ label: `${mode}-off`, mode, env: { SELKIES_WEB_ROOT: webRoot, SELKIES_ENABLE_CLIPBOARD: 'false', SELKIES_ENABLE_RESIZE: 'false', SELKIES_UI_TITLE: UI_TITLE } });
            run = { res, host, url: second.url, mode };
            browser = await launchChromium();
            try {
                await clipboardDisabledBlock(browser, run);
                await pinnedBlock(browser, run);
                await titleBlock(browser, run);
            } finally {
                await browser.close();
            }
        } finally {
            await host.stop();
            await host.close();
        }
        res.finish();
    });
}
