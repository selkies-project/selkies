/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Firefox and WebKit on the cells the browsers suite leaves out: the Wayland
// backend, and WebKit over WebRTC.
//
// firefox-ws-wl / webkit-ws-wl:
//     The WebSocket stream comes up on the Wayland backend and a key press and
//     a pointer move from the page reach the in-process compositor's seat.
// firefox-wr-wl:
//     The same over WebRTC; Firefox negotiates H.264 only with the OpenH264
//     GMP plugin that tests/tools/fetch-openh264.sh side-loads into the e2e
//     profile, so the cell skips (counted) without it.
// webkit-wr-x11:
//     WebKit over WebRTC against the X test display; input is observed in the
//     X keymap and pointer. A WebKit build that advertises no H.264 receive
//     capability, or that receives the RTP but never decodes a frame, skips
//     the stream-dependent checks with that reason rather than failing them.
//
// Every cell records what the engine cannot do here as a counted SKIP rather
// than a pass, so coverage claims stay honest.
import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { DECODER_ERROR_PATTERNS, engineLaunch, FF_E2E_PROFILE, openh264Version, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, type Observer } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within, type VideoSize } from '../lib/stream.ts';

const WL_SOCKET = 'wayland-1';
const KEY_X = 0x78;

// Counts WebSocket binary frames (headless rAF throttling zeroes the client's
// own fps) and keeps every RTCPeerConnection reachable for getStats.
const PAGE_JS = `
  window.__wsFrames = 0;
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
  window.__pcs = [];
  if (window.RTCPeerConnection) {
    const Orig = window.RTCPeerConnection;
    const Wrapped = function(...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    Wrapped.prototype = Orig.prototype;
    Object.setPrototypeOf(Wrapped, Orig);
    window.RTCPeerConnection = Wrapped;
  }
`;

const VIDEO_RTP_JS = `(async () => {
  const out = {received: 0, decoded: 0, codec: null};
  for (const pc of window.__pcs || []) {
    const rep = await pc.getStats();
    const byId = {};
    rep.forEach(r => byId[r.id] = r);
    rep.forEach(r => {
      if (r.type === 'inbound-rtp' && (r.kind === 'video' || r.mediaType === 'video')) {
        out.received += r.packetsReceived || 0;
        out.decoded += r.framesDecoded || 0;
        const c = byId[r.codecId];
        if (c) out.codec = c.mimeType;
      }
    });
  }
  return out;
})()`;

declare global {
    interface Window {
        __wsFrames?: number;
        videoChunksReceived?: number;
    }
}

/** Whether the engine lists H.264 among its RTP receive codecs. */
function h264Receivable(page: Page): Promise<boolean> {
    return within(page.evaluate(() => {
        try {
            return RTCRtpReceiver.getCapabilities('video')?.codecs.some((c) => /H264/i.test(c.mimeType)) ?? false;
        } catch {
            return false;
        }
    }));
}

function waitVideo(page: Page, mode: string): Promise<VideoSize | null> {
    return mode === 'webrtc' ? waitWrVideo(page, 45) : waitWsVideo(page, 25);
}

/** A key press and a pointer move from the page, observed at the server's seat (compositor) or X server. */
async function inputChecks(res: Checks, host: Host, page: Page, wayland: boolean, obs: Observer | null): Promise<void> {
    await page.mouse.click(640, 360);
    await sleep(500);
    let pressed = false;
    for (let i = 0; i < 4; i++) {
        // WebKit headless can drop a synthetic keydown on a throttled render
        // round; repeat the whole press rather than wait on one that never left.
        await page.keyboard.down('x');
        await sleep(800);
        pressed = wayland ? (await obs?.wait('kbd_key', 3, { state: 1 })) != null : (await host.keyHeld(KEY_X)) === true;
        if (pressed) break;
        await page.keyboard.up('x');
        await sleep(600);
    }
    res.check(`input: key press reached the ${wayland ? 'wayland seat' : 'X keymap'}`, pressed);
    await page.keyboard.up('x');
    await sleep(500);
    const released = wayland ? (await obs?.wait('kbd_key', 4, { state: 0 })) != null : (await host.keyHeld(KEY_X)) === false;
    res.check('input: key release observed', released);
    if (wayland) {
        // Pointer events reach the focused surface: the observer is fullscreen
        // on the output, so any in-stream position lands on it.
        await page.mouse.move(8, 8);
        await sleep(600);
        const ev = (await obs?.wait('ptr_motion', 4)) ?? null;
        res.check('input: pointer motion reached the wayland seat', ev !== null && 'x' in ev, ev);
    } else {
        await page.mouse.move(200, 150);
        await sleep(800);
        const after = await host.pointer();
        res.check('input: pointer moved in X (200,150)', after !== null && Math.abs(after[0] - 200) <= 4 && Math.abs(after[1] - 150) <= 4, after);
    }
}

/**
 * One engine, transport, and backend through the flow checks. The server runs
 * Turbo, so the still test display keeps sending the frames counted.
 */
async function cellBlock(host: Host, cell: string): Promise<Checks> {
    const [name, transport, backend] = cell.split('-');
    const engine: Engine = name === 'firefox' ? 'firefox' : name === 'webkit' ? 'webkit' : 'chromium';
    const mode = transport === 'wr' ? 'webrtc' : 'websockets';
    const wayland = backend === 'wl';
    const res = new Checks(cell);
    const { url } = await host.start({ label: cell, mode, wayland, env: { SELKIES_VIDEO_STREAMING_MODE: 'true' } });
    const [browser, ctx] = await engineLaunch(engine);
    let obs: Observer | null = null;
    try {
        await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        await ctx.addInitScript({ content: PAGE_JS });
        const pages = ctx.pages();
        const page = engine === 'firefox' && pages.length ? pages[0] : await ctx.newPage();
        const consoleErrors: string[] = [];
        page.on('console', (m) => { if (m.type() === 'error') consoleErrors.push(m.text()); });
        page.on('pageerror', (e) => consoleErrors.push(String(e)));
        const notFound: string[] = [];
        page.on('response', (r) => { if (r.status() === 404) notFound.push(r.url()); });
        await page.goto(url, { waitUntil: 'load' });
        await sleep(engine === 'webkit' ? 2000 : 6000);
        if (mode === 'webrtc' && !(await h264Receivable(page))) {
            for (const n of ['video: <video> receiving', 'input: key press', 'input: pointer']) res.skip(n, `${engine} lists no H.264 RTP receive codec here`);
            return res;
        }
        const info = await waitVideo(page, mode);
        if (mode === 'webrtc' && info === null && engine === 'webkit') {
            const rtp = await within(page.evaluate<{ received: number; decoded: number; codec: string | null }>(VIDEO_RTP_JS));
            if (rtp.received > 0 && rtp.decoded === 0) {
                // The offer was accepted and RTP arrives, but this build has no
                // decoder behind its advertised H.264: nothing to assert.
                for (const n of ['video: <video> receiving', 'input: key press', 'input: pointer']) {
                    res.skip(n, `WebKit received ${rtp.received} video packets (${rtp.codec}) but decoded none: no H.264 decoder in this build`);
                }
                return res;
            }
            res.check('video: <video> receiving', false, rtp);
        } else if (mode === 'webrtc') {
            res.check('video: <video> receiving', info !== null, info);
        } else {
            res.check('video: canvas painted', info !== null, info ?? 'no canvas>=640');
            let frames = 0;
            for (const until = Date.now() + 12_000; Date.now() < until;) {
                // The socket runs in a worker, where the page tap cannot see it;
                // the client publishes its own count.
                frames = await within(page.evaluate(() => window.videoChunksReceived || window.__wsFrames || 0));
                if (frames >= 24) break;
                await sleep(500);
            }
            res.check('video: WS frames flowing', frames >= 24, frames);
        }
        if (wayland) {
            obs = await host.observe(WL_SOCKET);
            res.check('wl observer mapped', await obs.ready());
        }
        await inputChecks(res, host, page, wayland, obs);
        const realErrors = consoleErrors.filter((e) => !DECODER_ERROR_PATTERNS.some((p) => e.includes(p)));
        const bad404 = notFound.filter((u) => !(u.endsWith('/manifest.json') || u.includes('favicon')));
        res.check('no console errors (filtered)', realErrors.length === 0, realErrors.join('; ').slice(0, 160));
        res.check('no unexpected 404s', bad404.length === 0, bad404.slice(0, 2));
    } finally {
        await obs?.release();
        if (browser) await browser.close();
        else await ctx.close();
        await host.stop();
    }
    return res;
}

for (const cell of ['firefox-ws-wl', 'webkit-ws-wl', 'firefox-wr-wl', 'webkit-wr-x11']) {
    test(cell, async () => {
        const host = new Host(suiteOf(import.meta.filename));
        let res: Checks;
        try {
            if (cell === 'firefox-wr-wl' && !openh264Version()) {
                res = new Checks(cell);
                res.skip('video: <video> receiving', `firefox webrtc: no OpenH264 GMP plugin in ${FF_E2E_PROFILE}; run tests/tools/fetch-openh264.sh to cover H.264 in Firefox`);
            } else {
                res = await cellBlock(host, cell);
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
