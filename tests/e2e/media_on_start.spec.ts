/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The session start policy end to end, over both transports in Chromium.
//
// A session starts with video, audio, and gamepad input on and the microphone
// and webcam off; the `*_on_start` settings change that for the session
// owner's primary page, and what starts off is not captured at all until the
// side menu turns it on. Each block starts a server with one setting flipped
// and reads both ends: the page's pipeline status messages and element state,
// and the server log for what was (not) captured. Video and audio are also
// switched on and off again through the dashboard's control messages, a shared
// viewer is shown exempt, and the gamepad block proves a browser's persisted
// toggle wins. The capture-gating blocks (defaults, video off, audio off) also
// run against the Wayland backend, whose captures start and stop through the
// compositor. Over WebSockets, video off also holds on a page whose decoder
// probe still runs when its socket opens, so the server's first settings land
// mid-open.
import type { Browser, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchBrowser } from '../lib/browsers.ts';
import { Checks, finishAll } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, runtimeDir } from '../lib/host.ts';
import { launchMic, statuses as micStatuses, unloadLeftoverVirtualMic, VIRTUAL_MIC, waitStatus as waitMic } from '../lib/microphone.ts';
import { buttonReports, PADS_INIT, PARITY_TAP_JS, STORAGE_APP } from '../lib/pads.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';
import { BLACK, build, camStatus, Device, frames, GREEN, launchCam, PublishedCamera, waitStatus as waitCam } from '../lib/webcam.ts';

// Records the last state the cores announced per pipeline.
const STATUS_JS = `
  window.__pipe = {};
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || (d.type !== 'sidebarButtonStatusUpdate' && d.type !== 'pipelineStatusUpdate')) return;
    for (const k of ['video', 'audio', 'microphone', 'webcam', 'gamepad']) {
      if (d[k] !== undefined) window.__pipe[k] = d[k];
    }
  });
`;

// Log lines that mark a capture starting or stopping, per transport.
const VIDEO_STARTED: Record<string, string> = { websockets: "Capture started for 'primary'", webrtc: "Started screen capture of 'primary'" };
const VIDEO_STOPPED: Record<string, string> = { websockets: "Received STOP_VIDEO for 'primary'. Stopping stream.", webrtc: "All consumers of display 'primary' are paused; capture stopped." };
const AUDIO_STARTED = 'Starting pcmflux audio pipeline';
const AUDIO_STOPPED = 'Stopping pcmflux audio pipeline';
const VIDEO_OFF_AT_START: Record<string, string> = { websockets: "Display 'primary' starts with video off", webrtc: 'Screen capture starts paused' };
const AUDIO_OFF_AT_START: Record<string, string> = { websockets: "settings applied for 'primary'", webrtc: 'Audio capture starts paused' };
// How long a capture that must not start is given to prove it.
const QUIET_S = 6;

// Holds the answer of the page's H.264 framing probe (lib/util.ts, a worker
// replying 'annexb' or 'avcc') for two seconds, as a slow first decoder does:
// the socket opens while the probe still runs, and the server's first
// messages land while the open handler waits on it.
const HELD_PROBE_JS = `
  (() => {
    const desc = Object.getOwnPropertyDescriptor(Worker.prototype, 'onmessage');
    Object.defineProperty(Worker.prototype, 'onmessage', {
      configurable: true,
      get() { return desc.get.call(this); },
      set(fn) {
        desc.set.call(this, typeof fn !== 'function' ? fn : (e) => {
          if (e.data === 'annexb' || e.data === 'avcc') setTimeout(() => fn.call(this, e), 2000);
          else fn.call(this, e);
        });
      },
    });
  })();
`;

type Pipe = Record<string, unknown>;

const js = (page: Page, code: string): Promise<unknown> => within(page.evaluate(code));
async function pipe(page: Page): Promise<Pipe> {
    const got = await js(page, 'window.__pipe');
    return got !== null && typeof got === 'object' ? { ...got } : {};
}

async function waitPipe(page: Page, key: string, value: boolean, timeoutS = 20): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if ((await pipe(page))[key] === value) return true;
        await sleep(250);
    }
    return false;
}

const post = (page: Page, message: Record<string, unknown>): Promise<unknown> => within(page.evaluate((m) => { window.postMessage(m, window.location.origin); }, message));
const videoUp = async (page: Page, mode: string, timeoutS = 20): Promise<boolean> => Boolean(mode === 'webrtc' ? await waitWrVideo(page, timeoutS) : await waitWsVideo(page, timeoutS));

/** Whether any video reached the page so far, without waiting. */
async function videoSeen(page: Page, mode: string): Promise<boolean> {
    if (mode === 'webrtc') return Boolean(await js(page, "(() => { const v = document.querySelector('video'); return !!(v && v.videoWidth > 0); })()"));
    return Boolean(await js(page, 'window.videoChunksReceived > 0'));
}

async function openPage(browser: Browser, url: string, mode: string, hash = '', extraInit: readonly string[] = []): Promise<Page> {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, permissions: [] });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: STATUS_JS });
    for (const script of extraInit) await ctx.addInitScript({ content: script });
    const page = await ctx.newPage();
    await page.goto(`${url}/${hash}`, { waitUntil: 'load' });
    return page;
}

const absent = async (host: Host, needle: string, timeoutS: number): Promise<boolean> => !(await host.saw(needle, timeoutS));

async function withServer(host: Host, label: string, mode: string, wayland: boolean, env: Record<string, string>, body: (url: string, browser: Browser) => Promise<void>): Promise<void> {
    const { url } = await host.start({ label, mode, wayland, env });
    try {
        const browser = await launchBrowser('chromium');
        try {
            await body(url, browser);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
}

async function defaultsBlock(host: Host, res: Checks, mode: string, wayland: boolean): Promise<void> {
    await withServer(host, res.block, mode, wayland, { SELKIES_DEBUG: 'true' }, async (url, browser) => {
        const page = await openPage(browser, url, mode);
        res.check('video flows', await videoUp(page, mode));
        res.check('audio capture started', await host.saw(AUDIO_STARTED, 15));
        await sleep(2000);
        const state = await pipe(page);
        // A state the dashboards already assume is not announced; only a
        // change is, so nothing may have been announced off.
        res.check('the page announces nothing off and no device on', state.video !== false && state.audio !== false && state.microphone !== true && state.webcam !== true && state.gamepad !== false, state);
    });
}

async function videoOffBlock(host: Host, res: Checks, mode: string, wayland: boolean): Promise<void> {
    await withServer(host, res.block, mode, wayland, { SELKIES_VIDEO_ON_START: 'false', SELKIES_DEBUG: 'true' }, async (url, browser) => {
        const page = await openPage(browser, url, mode);
        res.check('page announces video off', await waitPipe(page, 'video', false), await pipe(page));
        res.check('server registers the page with video off', await host.saw(VIDEO_OFF_AT_START[mode], 15));
        res.check('audio still starts', await host.saw(AUDIO_STARTED, 15));
        res.check('no screen capture starts', await absent(host, VIDEO_STARTED[mode], QUIET_S));
        res.check('no video reaches the page', !(await videoSeen(page, mode)));
        res.check('the status overlay is not left waiting for a stream', Boolean(await js(page, "(() => { const s = document.getElementById('status-display'); return !s || s.classList.contains('hidden'); })()")));
        await post(page, { type: 'pipelineControl', pipeline: 'video', enabled: true });
        res.check('toggle on: the capture starts', await host.saw(VIDEO_STARTED[mode], 15));
        res.check('toggle on: video flows', await videoUp(page, mode, 30));
        res.check('toggle on: page announces video on', await waitPipe(page, 'video', true), await pipe(page));
        await post(page, { type: 'pipelineControl', pipeline: 'video', enabled: false });
        res.check('toggle off: the capture stops', await host.saw(VIDEO_STOPPED[mode], 15));
        res.check('toggle off: page announces video off', await waitPipe(page, 'video', false), await pipe(page));

        const viewer = await openPage(browser, url, mode, '#shared');
        res.check('a shared viewer joining the video-off session gets the stream', await videoUp(viewer, mode, 30));
        res.check("the owner's page stays without video", !(await videoSeen(page, mode)) || (await pipe(page)).video === false);
        await viewer.context().close();
        await page.context().close();
    });
}

/** Video off at start, on a page still waiting on its decoder probe when its socket opens: the start policy the first server_settings applies stands. */
async function heldProbeBlock(host: Host, res: Checks, mode: string): Promise<void> {
    await withServer(host, res.block, mode, false, { SELKIES_VIDEO_ON_START: 'false', SELKIES_DEBUG: 'true' }, async (url, browser) => {
        const page = await openPage(browser, url, mode, '', [HELD_PROBE_JS]);
        res.check('server registers the page with video off', await host.saw(VIDEO_OFF_AT_START[mode], 15));
        await sleep(3000);
        res.check('page announces video off', await waitPipe(page, 'video', false), await pipe(page));
        await post(page, { type: 'pipelineControl', pipeline: 'video', enabled: true });
        res.check('toggle on: the capture starts', await host.saw(VIDEO_STARTED[mode], 15));
        res.check('toggle on: video flows', await videoUp(page, mode, 30));
        await page.context().close();
    });
}

async function audioOffBlock(host: Host, res: Checks, mode: string, wayland: boolean): Promise<void> {
    await withServer(host, res.block, mode, wayland, { SELKIES_AUDIO_ON_START: 'false', SELKIES_DEBUG: 'true' }, async (url, browser) => {
        const page = await openPage(browser, url, mode);
        const muted = async (want: boolean): Promise<boolean> => Boolean(await js(page, `(() => { const v = document.querySelector('video'); return !!(v && ${want ? 'v.muted' : '!v.muted'}); })()`));
        res.check('video flows', await videoUp(page, mode));
        res.check('page announces audio off', await waitPipe(page, 'audio', false), await pipe(page));
        res.check('server sees the page connect', await host.saw(AUDIO_OFF_AT_START[mode], 15));
        res.check('no audio capture starts', await absent(host, AUDIO_STARTED, QUIET_S));
        if (mode === 'webrtc') res.check('the element carrying audio is muted', await muted(true));
        await post(page, { type: 'pipelineControl', pipeline: 'audio', enabled: true });
        res.check('toggle on: the audio capture starts', await host.saw(AUDIO_STARTED, 15));
        res.check('toggle on: page announces audio on', await waitPipe(page, 'audio', true), await pipe(page));
        if (mode === 'webrtc') res.check('toggle on: the element is unmuted', await muted(false));
        await post(page, { type: 'pipelineControl', pipeline: 'audio', enabled: false });
        res.check('toggle off: the audio capture stops', await host.saw(AUDIO_STOPPED, 15));
        res.check('toggle off: page announces audio off', await waitPipe(page, 'audio', false), await pipe(page));

        const viewer = await openPage(browser, url, mode, '#shared');
        res.check('a shared viewer joining gets audio started for it', await host.saw(AUDIO_STARTED, 20));
        await viewer.context().close();
        await page.context().close();
    });
}

async function microphoneBlock(host: Host, res: Checks, mode: string): Promise<void> {
    unloadLeftoverVirtualMic();
    const { url } = await host.start({ label: res.block, mode, env: { SELKIES_MICROPHONE_ON_START: 'true', SELKIES_DEBUG: 'true' } });
    try {
        const [browser, page, errors] = await launchMic(url, mode);
        try {
            await js(page, STATUS_JS);
            res.check('video flows', await videoUp(page, mode));
            res.check('the microphone comes on without a toggle', await waitMic(page, true, 25), JSON.stringify(await micStatuses(page)));
            res.check('the virtual microphone is provisioned', await host.saw(`Virtual microphone '${VIRTUAL_MIC}' is ready`, 25));
            res.check('no page errors', errors.length === 0, errors.join('; ').slice(0, 200));
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    unloadLeftoverVirtualMic();
}

async function webcamBlock(host: Host, res: Checks, mode: string): Promise<void> {
    build();
    const cam = await new PublishedCamera(await frames({ kind: 'flat', color: GREEN })).start();
    try {
        const { url, pid } = await host.start({ label: res.block, mode, env: { SELKIES_WEBCAM_ENABLED: 'true', SELKIES_WEBCAM_ON_START: 'true', SELKIES_WEBCAM_PIXEL_FORMAT: 'I420' } });
        const device = new Device(runtimeDir(pid));
        const c = await launchCam('chromium', url, cam.sockDir, mode);
        try {
            res.check('video flows', await videoUp(c.page, mode));
            res.check('the webcam comes on without a toggle', await waitCam(c.page, true, 25), JSON.stringify(await camStatus(c.page)));
            const got = await device.waitForPicture([[[640, 360], GREEN], [[20, 20], BLACK]]);
            res.check('the camera reaches /dev/video0', got.rc === 0 && got.fields.frames === '30', `rc=${got.rc} frames=${got.fields.frames ?? 'None'} err=${got.fields.error ?? ''}`);
            res.check('no page errors', c.errors.length === 0, c.errors.join('; ').slice(0, 200));
        } finally {
            await c.close();
        }
    } finally {
        await host.stop();
        await cam.stop();
    }
}

async function gamepadOffBlock(host: Host, res: Checks, mode: string): Promise<void> {
    await withServer(host, res.block, mode, false, { SELKIES_GAMEPAD_ON_START: 'false' }, async (url, browser) => {
        const press = async (page: Page, button: number): Promise<void> => {
            await js(page, `window.__padPress(0, ${button}, 1)`);
            await sleep(400);
            await js(page, `window.__padPress(0, ${button}, 0)`);
            await sleep(600);
        };
        let page = await openPage(browser, url, mode, '', [PARITY_TAP_JS, PADS_INIT]);
        res.check('video flows', await videoUp(page, mode));
        res.check('page announces gamepad off', await waitPipe(page, 'gamepad', false), await pipe(page));
        await sleep(1000);
        await press(page, 0);
        const before = await buttonReports(page);
        res.check('a pressed pad is not polled', before === 0, before);
        await post(page, { type: 'gamepadControl', enabled: true });
        await sleep(300);
        await press(page, 1);
        const after = await buttonReports(page);
        res.check('toggle on: button reports reach the wire', after > before, after);
        await page.context().close();

        const remembered = `localStorage.setItem(${STORAGE_APP} + '_isGamepadEnabled', 'true');`;
        page = await openPage(browser, url, mode, '', [PARITY_TAP_JS, PADS_INIT, remembered]);
        res.check('remembered page: video flows', await videoUp(page, mode));
        await sleep(1000);
        await press(page, 0);
        res.check("a browser's remembered toggle wins over the policy", (await buttonReports(page)) > 0, await buttonReports(page));
        res.check('remembered page announces gamepad on', (await pipe(page)).gamepad !== false, await pipe(page));
        await page.context().close();
    });
}

for (const selector of ['websockets', 'webrtc', 'websockets-wl', 'webrtc-wl']) {
    test(selector, async () => {
        const mode = selector.replace('-wl', '');
        const wayland = selector.endsWith('-wl');
        const suffix = wayland ? '-wl' : '';
        const host = new Host(suiteOf(import.meta.filename));
        const blocks: Checks[] = [];
        const block = async (name: string, run: (res: Checks) => Promise<void>): Promise<void> => {
            const res = new Checks(name);
            blocks.push(res);
            await run(res);
        };
        try {
            await block(`start-defaults-${mode}${suffix}`, (res) => defaultsBlock(host, res, mode, wayland));
            await block(`video-off-${mode}${suffix}`, (res) => videoOffBlock(host, res, mode, wayland));
            await block(`audio-off-${mode}${suffix}`, (res) => audioOffBlock(host, res, mode, wayland));
            if (!wayland) {
                await block(`microphone-on-${mode}`, (res) => microphoneBlock(host, res, mode));
                await block(`webcam-on-${mode}`, (res) => webcamBlock(host, res, mode));
                await block(`gamepad-off-${mode}`, (res) => gamepadOffBlock(host, res, mode));
                if (mode === 'websockets') await block(`video-off-held-probe-${mode}`, (res) => heldProbeBlock(host, res, mode));
            }
        } finally {
            await host.close();
        }
        finishAll(blocks);
    });
}
