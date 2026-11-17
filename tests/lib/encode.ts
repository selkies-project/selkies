/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the encoder suites share (encoders, codecs, ten_bit): the engines a
 * block runs in, a page of the core client with the stripe tap on its
 * sockets, and the stream line pixelflux prints for each pipeline it starts.
 * @module
 */

import { spawnSync } from 'node:child_process';

import type { Browser, BrowserContext, Page } from '@playwright/test';

import { firefoxPersistentContext, launchBrowser, type Engine } from './browsers.ts';
import type { Host } from './host.ts';

/** The engines a block runs in. */
export const ENGINES: readonly Engine[] = ['chromium', 'firefox', 'webkit'];

/** Turbo: every frame encoded, so a still picture still streams. */
export const TURBO: Readonly<Record<string, string>> = { SELKIES_VIDEO_STREAMING_MODE: 'true' };

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
    }
}

/**
 * A page of `engine` on the client in `mode`, with its browser or context to close.
 * @param url The server.
 * @param mode The transport.
 * @param engine The engine.
 * @returns What to close, and the page.
 */
export async function openPage(url: string, mode: string, engine: Engine): Promise<[Browser | BrowserContext, Page]> {
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

/**
 * The stream lines pixelflux printed, oldest first.
 * @param host The suite's host.
 * @param from Where in the log to start.
 * @returns The lines.
 */
export async function streamLines(host: Host, from = 0): Promise<string[]> {
    return (await host.log(from)).split('\n').filter((l) => l.includes('Stream settings active'));
}

/** The newest stream line pixelflux printed. */
export async function lastStreamLine(host: Host): Promise<string> {
    return (await streamLines(host)).at(-1) ?? '';
}

/**
 * The encoder part of a stream line, for a check's detail.
 * @param line The line.
 * @returns Its encoder part.
 */
export function encoderField(line: string): string {
    const at = Math.max(line.indexOf('Encoder:'), line.indexOf('Mode:'));
    return at >= 0 ? line.slice(at, at + 60) : line.slice(-80);
}

/**
 * The software H.264 encoder of the server's pixelflux build (x264 or
 * openh264), as its --version line names it ("software h264=x264,..."), or
 * null when it does not say.
 */
export function serverSoftwareEncoder(binary: string): string | null {
    const out = spawnSync(binary, ['--version'], { encoding: 'utf8', timeout: 60_000 });
    return /\bsoftware [^)]*\bh264=([A-Za-z0-9_-]+)/.exec(out.stdout ?? '')?.[1] ?? null;
}
