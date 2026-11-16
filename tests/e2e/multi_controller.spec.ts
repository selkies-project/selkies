/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Two controllers on one display: a page of another tab joins the display's
// stream beside its owner instead of taking it over, on both transports. Both
// stream and both drive input; the owner keeps sizing the display while its
// user is at work, a reload of the owner's own page takes its connection back
// without touching the other, and once the owner is gone for good the page
// beside it owns the display and sets its size.
//
// settings / settings-wr: the display's stream settings between the two, over
// WebSockets and WebRTC: the page beside the owner starts on what the owner
// streams with, whatever it has stored, a pick of its user changes the display
// for both, and the owner's own later picks leave it be.
// nodisrupt: the page beside the owner keeps to its own stream over
// WebSockets: its audio starts and stops for itself alone, and its hidden tab
// pauses its own feed, while the owner's audio and video go on. Once the owner
// is gone for good, that page owns the display with full control.
// visible / visible-wr: the display goes to the page the user looks at.
// inuse / inuse-wr: the display goes to the page its user acts through, once
// the owner has sat unused a moment. Nobody is disconnected.
// older: a page from before the visibility hand-off, which leaves the
// server's PAGE_VISIBILITY unanswered, beside an owner whose hidden tab
// stopped the capture: the page gets the capture started again for it, while
// the hidden owner's own feed stays paused until it is shown.
// frozen-wr: an owner over WebRTC whose page never sends its STOP_VIDEO (a
// phone frozen on hide): once the display goes to the page in view, the old
// owner's feed pauses, and resumes when it is shown.
//
// Every server runs Turbo, so the still test display keeps each page's stream
// moving for the checks that count its frames.
import type { Browser, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { fields, isRecord, stringOr } from '../../web/selkies-web-core/lib/narrow.ts';
import { launchBrowser } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { pulseSine, pulseUnload } from '../lib/desktop.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { launchClient, PAGE_TAP_JS, PC_TAP_JS } from '../lib/page.ts';
import { Picture } from '../lib/picture.ts';
import { waitWrVideo, waitWsVideo, within, wrVideoState } from '../lib/stream.ts';

const TURBO = { SELKIES_VIDEO_STREAMING_MODE: 'true' };
type Size = [number, number];

declare global {
    interface Window {
        stream_info?: { codec?: string };
    }
}

/** Whether the page receives video over the next `seconds`: WebSockets chunks, or the WebRTC video's clock moving on. */
async function streaming(page: Page, mode: string, seconds = 2): Promise<boolean> {
    const probe = (m: string): Promise<number> => within(page.evaluate((ws) => (ws
        ? window.__wsFrames ?? 0
        : document.querySelector('video')?.currentTime ?? 0), m === 'websockets'));
    const before = await probe(mode);
    await sleep(seconds * 1000);
    return (await probe(mode)) > before;
}

/** Whether the page shows video within its timeout. */
async function video(page: Page, mode: string): Promise<boolean> {
    return Boolean(mode === 'websockets' ? await waitWsVideo(page, 30) : await waitWrVideo(page, 30));
}

/** Whether the page shows the server's fatal verdict. */
function terminated(page: Page): Promise<boolean> {
    return within(page.evaluate(() => (document.body.innerText || '').includes('Connection Terminated')));
}

function nearSize(size: Size | null, want: Size, slack = 16): boolean {
    return size !== null && Math.abs(size[0] - want[0]) <= slack && Math.abs(size[1] - want[1]) <= slack;
}

/** Poll the root size until it is within `slack` pixels of `want`; the last read. */
async function waitRoot(host: Host, want: Size, timeoutS = 20, slack = 16): Promise<Size | null> {
    let size = await host.rootSize();
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        size = await host.rootSize();
        if (nearSize(size, want, slack)) break;
        await sleep(500);
    }
    return size;
}

const NAV_JS = "sessionStorage.setItem('__navs', String(Number(sessionStorage.getItem('__navs') || 0) + 1));";
const STORAGE_PREFIX_JS = "(location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_')";

/** The stored values a page starts with, written before its first load only. */
function seedJs(seed: Record<string, string>): string {
    return `(() => {
  if (sessionStorage.getItem('__seeded')) return;
  sessionStorage.setItem('__seeded', '1');
  const app = ${STORAGE_PREFIX_JS};
  for (const [k, v] of Object.entries(${JSON.stringify(seed)})) localStorage.setItem(app + '_' + k, v);
})();`;
}

/** What a page streams with and has stored. */
interface State {
    encoder: string | null;
    codec: string | null;
    stored: string | null;
    crf: string | null;
    navs: number;
}

function state(page: Page): Promise<State> {
    return within(page.evaluate((): State => {
        const app = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
        return {
            encoder: window.encoder ?? null,
            codec: window.stream_info?.codec ?? null,
            stored: localStorage.getItem(`${app}_encoder`),
            crf: localStorage.getItem(`${app}_video_crf`),
            navs: Number(sessionStorage.getItem('__navs') ?? 0),
        };
    }));
}

/** Poll the page's state until `predicate` holds; the last state read. */
async function waitState(page: Page, predicate: (s: State) => boolean, timeoutS = 20): Promise<State> {
    let s = await state(page);
    for (const until = Date.now() + timeoutS * 1000; !predicate(s) && Date.now() < until;) {
        await sleep(500);
        s = await state(page);
    }
    return s;
}

// A page from before the visibility hand-off: its socket on the page
// (socket_worker=false), the server's PAGE_VISIBILITY never reaches its handler.
const OLDER_PAGE_JS = `(() => {
  const WS = window.WebSocket;
  const handler = Object.getOwnPropertyDescriptor(WS.prototype, 'onmessage');
  window.WebSocket = function(...a) {
    const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
    Object.defineProperty(s, 'onmessage', {
      configurable: true,
      get() { return handler.get.call(s); },
      set(fn) { handler.set.call(s, fn ? (e) => { if (e.data !== 'PAGE_VISIBILITY') fn.call(s, e); } : fn); },
    });
    return s;
  };
  window.WebSocket.prototype = WS.prototype;
  Object.setPrototypeOf(window.WebSocket, WS);
})();`;

// A page frozen on hide: its STOP_VIDEO never leaves.
const FROZEN_PAGE_JS = `(() => {
  const send = RTCDataChannel.prototype.send;
  RTCDataChannel.prototype.send = function (d) { if (d === 'STOP_VIDEO') return; return send.call(this, d); };
})();`;

/** What `openPage` takes beyond the seed. */
interface PageOptions {
    storage?: Awaited<ReturnType<import('@playwright/test').BrowserContext['storageState']>>;
    mode?: string;
    older?: boolean;
    frozen?: boolean;
}

/**
 * A controller page of its own browser context, with `seed` stored before its
 * first load; from before the visibility hand-off where `older`, one that
 * never sends its STOP_VIDEO where `frozen`.
 */
async function openPage(url: string, seed: Record<string, string>, o: PageOptions = {}): Promise<[Browser, Page, string[]]> {
    const browser = await launchBrowser('chromium');
    const ctx = await browser.newContext({ storageState: o.storage, viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${o.mode ?? 'websockets'}';` });
    await ctx.addInitScript({ content: NAV_JS });
    await ctx.addInitScript({ content: PAGE_TAP_JS + PC_TAP_JS });
    if (Object.keys(seed).length) await ctx.addInitScript({ content: seedJs(seed) });
    if (o.older) await ctx.addInitScript({ content: OLDER_PAGE_JS });
    if (o.frozen) await ctx.addInitScript({ content: FROZEN_PAGE_JS });
    const page = await ctx.newPage();
    const lines: string[] = [];
    page.on('console', (m) => { lines.push(m.text()); });
    await page.goto(`${url}${o.older ? '/?socket_worker=false' : '/'}`, { waitUntil: 'load' });
    return [browser, page, lines];
}

/** What a dashboard posts for its user's pick. */
async function pick(page: Page, settings: Record<string, unknown>): Promise<void> {
    await within(page.evaluate((s) => { window.postMessage({ type: 'settings', settings: s }, window.location.origin); }, settings));
}

/** The console lines of a decoder fallback or crash. */
function crashed(lines: readonly string[]): string[] {
    return lines.filter((l) => l.includes('FATAL DECODER') || l.includes('Primary client fallback'));
}

/** The codec the page's WebRTC video arrives in, as its inbound stats name it. */
async function wrCodec(page: Page): Promise<string> {
    const s = await wrVideoState(page);
    if (!isRecord(s) || !Array.isArray(s.pcs)) return '';
    for (const pc of s.pcs) {
        const inbound = fields(pc).inbound;
        for (const i of Array.isArray(inbound) ? inbound : []) {
            if (fields(i).kind === 'video') return stringOr(fields(i).codec) ?? '';
        }
    }
    return '';
}

/** Poll the page's inbound video codec until it names `name`; the last one read. */
async function waitCodec(page: Page, name: string, timeoutS = 30): Promise<string> {
    let codec = await wrCodec(page);
    for (const until = Date.now() + timeoutS * 1000; !codec.includes(name) && Date.now() < until;) {
        await sleep(1000);
        codec = await wrCodec(page);
    }
    return codec;
}

/** What a dashboard posts for its user's pipeline toggle. */
async function control(page: Page, pipeline: string, enabled: boolean): Promise<void> {
    await within(page.evaluate(([p, on]) => {
        window.postMessage({ type: 'pipelineControl', pipeline: p, enabled: on }, window.location.origin);
    }, [pipeline, enabled] as const));
}

/** The video frames the page has received. */
function frames(page: Page): Promise<number> {
    return within(page.evaluate(() => (window.__wsTypes ?? {})[4] ?? 0));
}

/** How many video frames the page receives over the next `seconds`. */
async function gained(page: Page, seconds = 2.5): Promise<number> {
    const before = await frames(page);
    await sleep(seconds * 1000);
    return (await frames(page)) - before;
}

function bufferDepth(page: Page): Promise<number> {
    return within(page.evaluate(() => window.currentAudioBufferSize || 0));
}

/** Whether audio fills the page's playback buffer within `timeoutS`. */
async function hears(page: Page, timeoutS = 10): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if ((await bufferDepth(page)) > 0) return true;
        await sleep(250);
    }
    return false;
}

/** Whether the page's playback buffer, drained for `settleS`, stays empty for `spanS`. */
async function silent(page: Page, settleS = 2, spanS = 2.5): Promise<boolean> {
    await sleep(settleS * 1000);
    for (const until = Date.now() + spanS * 1000; Date.now() < until;) {
        if ((await bufferDepth(page)) > 0) return false;
        await sleep(250);
    }
    return true;
}

/** The audio and video state messages the server has sent the page. */
function states(page: Page): Promise<string[]> {
    return within(page.evaluate(() => window.__wsStates ?? []));
}

/** Hide or show the page's tab, as the browser tells it. */
async function hide(page: Page, hidden: boolean): Promise<void> {
    await within(page.evaluate((h) => {
        Object.defineProperty(document, 'hidden', { configurable: true, get: () => h });
        Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => (h ? 'hidden' : 'visible') });
        document.dispatchEvent(new Event('visibilitychange'));
    }, hidden));
}

/** The video frames the page's peer connections have received. */
function wrFrames(page: Page): Promise<number> {
    return within(page.evaluate(async () => {
        let n = 0;
        const pcs: unknown = Reflect.get(window, '__pcs');
        for (const pc of Array.isArray(pcs) ? pcs : []) {
            if (!(pc instanceof RTCPeerConnection)) continue;
            (await pc.getStats()).forEach((r: Record<string, unknown>) => {
                if (r.type === 'inbound-rtp' && r.kind === 'video') n += Number(r.framesReceived ?? 0);
            });
        }
        return n;
    }));
}

async function run(res: Checks, host: Host, mode: string): Promise<void> {
    const { url } = await host.start({ label: mode, mode, env: TURBO });
    const o = await launchClient(url, { mode });
    const owner = o.page;
    res.check(`${mode}: the owner streams`, await video(owner, mode));
    const ownerSize = await waitRoot(host, [1280, 720]);
    const t = await launchClient(url, { mode });
    const other = t.page;
    res.check(`${mode}: a page of another tab streams too`, await video(other, mode));
    await sleep(2000);
    res.check(`${mode}: and the owner goes on streaming, not terminated`, !(await terminated(owner)) && await streaming(owner, mode));
    res.check(`${mode}: nor is the page beside it`, !(await terminated(other)) && await streaming(other, mode));

    // The owner's user at work keeps the display from following the page beside it.
    await owner.mouse.move(640, 360);
    await other.mouse.move(200, 150);
    await sleep(500);
    const first = await host.pointer() ?? [0, 0];
    await other.mouse.move(600, 400);
    await sleep(500);
    const second = await host.pointer() ?? [0, 0];
    res.check(`${mode}: the page beside it drives the pointer`, second[0] - first[0] > 200 && second[1] - first[1] > 100,
        `${JSON.stringify(first)} -> ${JSON.stringify(second)}`);

    await other.setViewportSize({ width: 1024, height: 640 });
    await sleep(3000);
    const now = await host.rootSize();
    res.check(`${mode}: its resize leaves the display at the owner's size`, JSON.stringify(now) === JSON.stringify(ownerSize),
        `${JSON.stringify(now)} against ${JSON.stringify(ownerSize)}`);

    await owner.reload({ waitUntil: 'load' });
    res.check(`${mode}: the owner's reload streams again`, await video(owner, mode));
    await sleep(2000);
    res.check(`${mode}: and takes its own connection back, leaving the page beside it be`,
        !(await terminated(other)) && await streaming(other, mode) && await streaming(owner, mode));

    await o.browser.close();
    const size = await waitRoot(host, [1024, 640], 25);
    res.check(`${mode}: once the owner is gone for good, the page beside it sizes the display`, nearSize(size, [1024, 640]), size);
    res.check(`${mode}: and still streams`, await streaming(other, mode));
    await t.browser.close();
    await host.stop();
}

async function runSettings(res: Checks, host: Host): Promise<void> {
    let { url } = await host.start({ label: 'settings', mode: 'websockets', env: TURBO });
    const [ownerBrowser, owner, ownerLines] = await openPage(url, { encoder: 'jpeg', video_crf: '30' });
    res.check('settings: the owner streams', await video(owner, 'websockets'));
    let s = await waitState(owner, (x) => x.codec === 'jpeg');
    res.check('settings: on the encoder it picked', s.codec === 'jpeg', s);
    const [otherBrowser, other, otherLines] = await openPage(url, { encoder: 'h264enc', video_crf: '20' });
    res.check('settings: a page of another tab streams too', await video(other, 'websockets'));
    s = await waitState(other, (x) => x.encoder === 'jpeg' && x.crf === '30');
    res.check('settings: and starts on what the owner streams with, whatever it stored',
        s.encoder === 'jpeg' && s.stored === 'jpeg' && s.crf === '30', s);
    await sleep(10_000);
    s = await state(other);
    res.check('settings: without a decoder fallback or a reload', s.navs === 1 && !crashed(otherLines).length, [s.navs, crashed(otherLines)]);
    s = await state(owner);
    res.check("settings: while the display keeps the owner's encoder", s.codec === 'jpeg', s);

    await pick(other, { encoder: 'h264enc' });
    s = await waitState(owner, (x) => x.codec === 'h264' && x.encoder === 'h264enc');
    res.check('settings: a pick on the page beside the owner changes the display', s.codec === 'h264', s);
    res.check('settings: and the owner follows it', s.encoder === 'h264enc' && s.stored === 'h264enc', s);
    res.check('settings: both go on streaming', await streaming(owner, 'websockets') && await streaming(other, 'websockets'));

    await pick(owner, { video_crf: 35 });
    s = await waitState(other, (x) => x.crf === '35');
    res.check("settings: the owner's own later pick reaches the page beside it", s.crf === '35', s);
    await sleep(3000);
    s = await state(owner);
    res.check('settings: and leaves the encoder picked beside it be', s.codec === 'h264', s);
    for (const [name, page, lines] of [['owner', owner, ownerLines], ['page beside it', other, otherLines]] as const) {
        s = await state(page);
        res.check(`settings: the ${name} never fell back or reloaded`, s.navs === 1 && !crashed(lines).length, [s.navs, crashed(lines)]);
    }
    const storage = await owner.context().storageState();
    await otherBrowser.close();
    await ownerBrowser.close();
    // A later visit of the owner's browser, the display's last session gone: its own
    // picks, under the same origin, so on the same port.
    ({ url } = await host.start({ label: 'settings-later', mode: 'websockets', env: TURBO, port: Number(new URL(url).port) }));
    const [browser, page] = await openPage(url, {}, { storage });
    res.check("settings: a later visit of the owner's browser streams", await video(page, 'websockets'));
    s = await waitState(page, (x) => x.codec === 'jpeg');
    res.check('settings: on its own picks again', s.codec === 'jpeg' && s.stored === 'jpeg' && s.crf === '35', s);
    await browser.close();
    await host.stop();
}

async function runSettingsWr(res: Checks, host: Host): Promise<void> {
    let { url } = await host.start({ label: 'settings-wr', mode: 'webrtc', env: TURBO });
    const [ownerBrowser, owner] = await openPage(url, { encoder: 'vp8enc', video_crf: '30' }, { mode: 'webrtc' });
    res.check('settings-wr: the owner streams', await video(owner, 'webrtc'));
    let codec = await waitCodec(owner, 'VP8');
    res.check('settings-wr: on the encoder it picked', codec.includes('VP8'), codec);
    const [otherBrowser, other] = await openPage(url, { encoder: 'h264enc', video_crf: '20' }, { mode: 'webrtc' });
    res.check('settings-wr: a page of another tab streams too', await video(other, 'webrtc'));
    let s = await waitState(other, (x) => x.stored === 'vp8enc' && x.crf === '30');
    res.check('settings-wr: and starts on what the owner streams with, whatever it stored', s.stored === 'vp8enc' && s.crf === '30', s);
    await sleep(5000);
    codec = await wrCodec(owner);
    res.check("settings-wr: while the display keeps the owner's encoder", codec.includes('VP8'), codec);

    await pick(other, { encoder: 'h264enc' });
    codec = await waitCodec(owner, 'H264');
    res.check('settings-wr: a pick on the page beside the owner changes the display', codec.includes('H264'), codec);
    s = await waitState(owner, (x) => x.stored === 'h264enc');
    res.check('settings-wr: and the owner follows it', s.stored === 'h264enc', s);
    res.check('settings-wr: both go on streaming', await streaming(owner, 'webrtc') && await streaming(other, 'webrtc'));

    await pick(owner, { video_crf: 35 });
    s = await waitState(other, (x) => x.crf === '35');
    res.check("settings-wr: the owner's own later pick reaches the page beside it", s.crf === '35', s);
    await sleep(3000);
    codec = await wrCodec(owner);
    res.check('settings-wr: and leaves the encoder picked beside it be', codec.includes('H264'), codec);
    for (const [name, page] of [['owner', owner], ['page beside it', other]] as const) {
        s = await state(page);
        res.check(`settings-wr: the ${name} never reloaded`, s.navs === 1, s.navs);
    }
    const storage = await owner.context().storageState();
    await otherBrowser.close();
    await ownerBrowser.close();
    ({ url } = await host.start({ label: 'settings-wr-later', mode: 'webrtc', env: TURBO, port: Number(new URL(url).port) }));
    const [browser, page] = await openPage(url, {}, { storage, mode: 'webrtc' });
    res.check("settings-wr: a later visit of the owner's browser streams", await video(page, 'webrtc'));
    s = await state(page);
    codec = await waitCodec(page, 'VP8');
    res.check('settings-wr: on its own picks again', codec.includes('VP8') && s.stored === 'vp8enc' && s.crf === '35', [codec, s]);
    await browser.close();
    await host.stop();
}

async function runVisible(res: Checks, host: Host, mode: string): Promise<void> {
    const tag = `visible-${mode}`;
    const { url } = await host.start({ label: tag, mode, env: TURBO });
    const o = await launchClient(url, { mode });
    const owner = o.page;
    await owner.setViewportSize({ width: 800, height: 600 });
    res.check(`${tag}: the owner streams`, await video(owner, mode));
    res.check(`${tag}: at its size`, nearSize(await waitRoot(host, [800, 600]), [800, 600]), await host.rootSize());
    await hide(owner, true);
    await sleep(2000);

    const t = await launchClient(url, { mode });
    const other = t.page;
    res.check(`${tag}: a page opened beside the hidden owner streams`, await video(other, mode));
    let size = await waitRoot(host, [1280, 720]);
    res.check(`${tag}: and owns the display at its own size`, nearSize(size, [1280, 720]), size);
    res.check(`${tag}: the hidden owner is not terminated`, !(await terminated(owner)));

    await hide(owner, false);
    await sleep(3000);
    res.check(`${tag}: shown again, the owner leaves the display with the page in view`, nearSize(await host.rootSize(), [1280, 720]), await host.rootSize());
    res.check(`${tag}: and streams beside it`, await streaming(owner, mode));

    await hide(other, true);
    size = await waitRoot(host, [800, 600]);
    res.check(`${tag}: that page hidden, the owner owns the display again at its size`, nearSize(size, [800, 600]), size);
    res.check(`${tag}: neither page is terminated`, !(await terminated(owner)) && !(await terminated(other)));

    // Both hidden, the capture stops; the page shown first owns the display and starts it.
    await hide(owner, true);
    await sleep(2000);
    await hide(other, false);
    size = await waitRoot(host, [1280, 720]);
    res.check(`${tag}: both pages hidden, the one shown first owns the display`, nearSize(size, [1280, 720]), size);
    res.check(`${tag}: and streams`, await streaming(other, mode));

    await hide(owner, false);
    await sleep(1000);
    await hide(other, true);
    size = await waitRoot(host, [800, 600]);
    res.check(`${tag}: and the owner owns it again once that page hides`, nearSize(size, [800, 600]), size);
    await hide(other, false);
    await sleep(2000);
    await owner.reload({ waitUntil: 'load' });
    res.check(`${tag}: the owner's reload streams again`, await video(owner, mode));
    await sleep(3000);
    res.check(`${tag}: and keeps the display, the page beside it in view`, nearSize(await host.rootSize(), [800, 600]), await host.rootSize());
    res.check(`${tag}: which goes on streaming`, !(await terminated(other)) && await streaming(other, mode));
    await o.browser.close();
    await t.browser.close();
    await host.stop();
}

async function runInuse(res: Checks, host: Host, mode: string): Promise<void> {
    const tag = `inuse-${mode}`;
    const { url } = await host.start({ label: tag, mode, env: TURBO });
    const o = await launchClient(url, { mode });
    const owner = o.page;
    await owner.setViewportSize({ width: 800, height: 600 });
    res.check(`${tag}: the owner streams`, await video(owner, mode));
    res.check(`${tag}: at its size`, nearSize(await waitRoot(host, [800, 600]), [800, 600]), await host.rootSize());

    const t = await launchClient(url, { mode });
    const other = t.page;
    res.check(`${tag}: a page opened beside it streams`, await video(other, mode));
    await sleep(2000);
    res.check(`${tag}: and leaves the display be until its user acts`, nearSize(await host.rootSize(), [800, 600]), await host.rootSize());
    await other.mouse.move(640, 360);
    let size = await waitRoot(host, [1280, 720]);
    res.check(`${tag}: its user acting, with the owner's idle, the display follows to its size`, nearSize(size, [1280, 720]), size);
    res.check(`${tag}: the old owner, in view, is not terminated and streams beside it`, !(await terminated(owner)) && await streaming(owner, mode));

    await other.mouse.move(600, 300);
    await owner.mouse.move(400, 300);
    await sleep(2000);
    res.check(`${tag}: the old owner's user acting while that page is in use leaves it be`, nearSize(await host.rootSize(), [1280, 720]), await host.rootSize());
    await sleep(2000);
    await owner.mouse.move(300, 200);
    size = await waitRoot(host, [800, 600]);
    res.check(`${tag}: once that page sat unused, the old owner's user takes the display back`, nearSize(size, [800, 600]), size);
    res.check(`${tag}: neither page is terminated, both streaming`, !(await terminated(owner)) && !(await terminated(other))
        && await streaming(owner, mode) && await streaming(other, mode));
    await o.browser.close();
    await t.browser.close();
    await host.stop();
}

async function runNodisrupt(res: Checks, host: Host): Promise<void> {
    const { url } = await host.start({ label: 'nodisrupt', mode: 'websockets', env: TURBO });
    const tone = pulseSine();
    const picture = new Picture(host, false);
    try {
        const [ownerBrowser, owner] = await openPage(url, {});
        res.check('nodisrupt: the owner streams', await video(owner, 'websockets'));
        await owner.mouse.click(640, 360);
        res.check('nodisrupt: and hears the session', await hears(owner));
        await control(owner, 'audio', false);
        res.check('nodisrupt: until its user turns its audio off', await silent(owner));
        let told = (await states(owner)).length;
        const [otherBrowser, other] = await openPage(url, {});
        res.check('nodisrupt: a page of another tab streams too', await video(other, 'websockets'));
        await owner.mouse.move(600, 300);
        await other.mouse.click(640, 360);
        res.check('nodisrupt: and hears the session by its own start', await hears(other));
        let since = (await states(owner)).slice(told);
        res.check("nodisrupt: while the owner's audio stays off", await silent(owner, 0) && !since.includes('AUDIO_STARTED'), since);
        await control(owner, 'audio', true);
        res.check('nodisrupt: until the owner turns it back on', await hears(owner));
        told = (await states(owner)).length;
        await control(other, 'audio', false);
        res.check('nodisrupt: the page beside it turning its audio off stops its own', await silent(other), (await states(other)).slice(-2));
        const keeps = await hears(owner, 3);
        since = (await states(owner)).slice(told);
        res.check("nodisrupt: and leaves the owner's playing", keeps && !since.includes('AUDIO_STOPPED'), since);

        await hide(other, true);
        await sleep(1500);
        const beforeOwner = await frames(owner);
        const beforeOther = await frames(other);
        await picture.paint();
        await sleep(2500);
        const otherGain = (await frames(other)) - beforeOther;
        res.check('nodisrupt: the page beside it hidden gets no video', otherGain === 0, otherGain);
        const ownerGain = (await frames(owner)) - beforeOwner;
        res.check("nodisrupt: while the owner's goes on", ownerGain > 0, ownerGain);
        told = (await states(other)).length;
        await hide(other, false);
        await sleep(2000);
        await picture.clear();
        since = (await states(other)).slice(told);
        res.check('nodisrupt: shown again, it resumes on a fresh start', since.includes('PIPELINE_RESETTING primary') && await gained(other) > 0, since);

        await ownerBrowser.close();
        res.check('nodisrupt: once the owner is gone for good, the page beside it owns the display',
            await host.saw("The controller beside 'primary' owns it now.", 25));
        await sleep(3000);
        await hide(other, true);
        res.check('nodisrupt: with full control: hidden, it stops the stream', await host.saw("Received STOP_VIDEO for 'primary'. Stopping stream.", 10));
        await hide(other, false);
        res.check('nodisrupt: and shown, it starts it again',
            await host.saw("Received START_VIDEO for 'primary'. Starting its stream.", 10) && await gained(other) > 0);
        await otherBrowser.close();
    } finally {
        await picture.clear();
        pulseUnload(tone);
        await host.stop();
    }
}

async function runOlder(res: Checks, host: Host): Promise<void> {
    const { url } = await host.start({ label: 'older', mode: 'websockets', env: TURBO });
    const picture = new Picture(host, false);
    try {
        const [ownerBrowser, owner] = await openPage(url, {});
        res.check('older: the owner streams', await video(owner, 'websockets'));
        await hide(owner, true);
        res.check('older: hidden with nobody beside it, it stops the stream', await host.saw("Received STOP_VIDEO for 'primary'. Stopping stream.", 10));
        const [otherBrowser, other] = await openPage(url, {}, { older: true });
        res.check('older: a page that cannot take the display joins beside it', await host.saw("joins 'primary' beside its owner", 20));
        await picture.paint();
        res.check('older: and gets the capture the hidden owner stopped', await video(other, 'websockets') && await gained(other) > 0, await frames(other));
        const before = await frames(owner);
        await picture.clear();
        await picture.paint();
        await sleep(2500);
        const gain = (await frames(owner)) - before;
        res.check("older: while the hidden owner's own feed stays paused", gain === 0, gain);
        await hide(owner, false);
        await sleep(1500);
        res.check("older: shown again, the owner's feed resumes", await gained(owner) > 0);
        await otherBrowser.close();
        await ownerBrowser.close();
    } finally {
        await picture.clear();
        await host.stop();
    }
}

async function runFrozenWr(res: Checks, host: Host): Promise<void> {
    const { url } = await host.start({ label: 'frozen-wr', mode: 'webrtc', env: TURBO });
    const picture = new Picture(host, false, true);
    try {
        const [ownerBrowser, owner] = await openPage(url, {}, { mode: 'webrtc', frozen: true });
        res.check('frozen-wr: the owner streams', await video(owner, 'webrtc'));
        const [otherBrowser, other] = await openPage(url, {}, { mode: 'webrtc' });
        res.check('frozen-wr: a page of another tab streams beside it', await video(other, 'webrtc'));
        await picture.paint();
        await hide(owner, true);
        res.check('frozen-wr: hidden with no STOP_VIDEO, the owner hands the display to the page in view', await host.saw('owns the display now', 15));
        await sleep(1000);
        let before = await wrFrames(owner);
        await sleep(2500);
        let gain = (await wrFrames(owner)) - before;
        res.check('frozen-wr: and its own feed pauses', gain <= 1, gain);
        await hide(owner, false);
        await sleep(1500);
        before = await wrFrames(owner);
        await sleep(2500);
        gain = (await wrFrames(owner)) - before;
        res.check('frozen-wr: shown again, its feed resumes', gain > 0, gain);
        await otherBrowser.close();
        await ownerBrowser.close();
    } finally {
        await picture.clear();
        await host.stop();
    }
}

const BLOCKS: Record<string, (res: Checks, host: Host) => Promise<void>> = {
    websockets: (r, h) => run(r, h, 'websockets'),
    webrtc: (r, h) => run(r, h, 'webrtc'),
    settings: runSettings,
    'settings-wr': runSettingsWr,
    nodisrupt: runNodisrupt,
    visible: (r, h) => runVisible(r, h, 'websockets'),
    'visible-wr': (r, h) => runVisible(r, h, 'webrtc'),
    inuse: (r, h) => runInuse(r, h, 'websockets'),
    'inuse-wr': (r, h) => runInuse(r, h, 'webrtc'),
    older: runOlder,
    'frozen-wr': runFrozenWr,
};

for (const [selector, block] of Object.entries(BLOCKS)) {
    test(selector, async () => {
        const res = new Checks(`multi-controller-${selector}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await block(res, host);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
