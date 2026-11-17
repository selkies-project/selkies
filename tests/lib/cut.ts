/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the suites that cut a page's session socket share (ws_cut,
 * webrtc_cut): the relay that cuts every connection at once, the page hooks
 * that time the picture, the notice and playback around a cut, and the
 * readings of them.
 * @module
 */

import { connect, createServer, type Server, type Socket } from 'node:net';

import type { Page } from '@playwright/test';

import { sleep } from './env.ts';
import type { Host } from './host.ts';
import { within } from './stream.ts';

export const WIDTH = 1280;
export const HEIGHT = 720;
/** How long after a cut the page is watched. */
export const WINDOW_MS = 3000;

/** One relayed connection: the page's side and the server's. */
interface Pair {
    client: Socket;
    upstream: Socket;
    halfOpen: boolean;
}

/**
 * TCP from a port of its own to the server whose open connections `cut()`
 * aborts at once, so each side sees its stream end with no close frame.
 */
export class CutRelay {
    private readonly server: Server;
    private readonly target: number;
    private readonly pairs = new Set<Pair>();
    private refuseUntil = 0;
    private holdVideoUntil = 0;

    private constructor(server: Server, target: number) {
        this.server = server;
        this.target = target;
        server.on('connection', (client) => { this.handle(client); });
    }

    /**
     * Listen on `listen` for connections to relay to `target`.
     * @param listen The relay's port.
     * @param target The server's port.
     * @returns The listening relay.
     */
    static async start(listen: number, target: number): Promise<CutRelay> {
        const server = createServer();
        const relay = new CutRelay(server, target);
        await new Promise<void>((resolve, reject) => {
            server.once('error', (e) => { reject(new Error(`the relay could not listen on port ${listen}: ${String(e)}`)); });
            server.listen(listen, '127.0.0.1', () => { resolve(); });
        });
        return relay;
    }

    private handle(client: Socket): void {
        if (Date.now() < this.refuseUntil) {
            client.destroy();
            return;
        }
        const holding = Date.now() < this.holdVideoUntil;
        const upstream = connect(this.target, '127.0.0.1');
        const pair: Pair = { client, upstream, halfOpen: false };
        this.pairs.add(pair);
        const ended = (): void => { this.pairs.delete(pair); };
        client.on('error', () => undefined);
        upstream.on('error', () => undefined);
        // A half-open cut leaves the server's side open and unread.
        client.on('close', () => {
            if (pair.halfOpen) upstream.pause();
            else upstream.destroy();
            ended();
        });
        upstream.on('close', () => { client.destroy(); ended(); });
        client.pipe(upstream, { end: false });
        if (holding) this.holdVideo(upstream, client);
        else upstream.pipe(client, { end: false });
    }

    /**
     * The server's side of a socket, its video frames (binary, first byte 0x03
     * or 0x04) held until the hold ends; the handshake, text and audio pass at
     * once.
     */
    private holdVideo(src: Socket, dst: Socket): void {
        let buf = Buffer.alloc(0);
        const held: Buffer[] = [];
        let upgraded: boolean | null = null;
        const flush = (): void => {
            if (Date.now() >= this.holdVideoUntil && held.length) {
                dst.write(Buffer.concat(held));
                held.length = 0;
            }
        };
        const timer = setInterval(flush, 50);
        src.on('close', () => { clearInterval(timer); });
        src.on('data', (data: Buffer) => {
            buf = Buffer.concat([buf, data]);
            if (upgraded === null) {
                const end = buf.indexOf('\r\n\r\n');
                if (end < 0) return;
                upgraded = buf.subarray(0, 12).toString('latin1') === 'HTTP/1.1 101';
                dst.write(buf.subarray(0, end + 4));
                buf = buf.subarray(end + 4);
            }
            const holding = Date.now() < this.holdVideoUntil;
            flush();
            while (upgraded && buf.length >= 2) {
                let size = buf[1] & 0x7F;
                let at = 2;
                if (size >= 126) {
                    at = size === 126 ? 4 : 10;
                    if (buf.length < at) break;
                    size = size === 126 ? buf.readUInt16BE(2) : Number(buf.readBigUInt64BE(2));
                }
                if (buf.length < at + size) break;
                const frame = buf.subarray(0, at + size);
                buf = buf.subarray(at + size);
                if (holding && (frame[0] & 0x0F) === 2 && size && (frame[at] === 3 || frame[at] === 4)) held.push(Buffer.from(frame));
                else dst.write(frame);
            }
            if (!upgraded && buf.length) {
                dst.write(buf);
                buf = Buffer.alloc(0);
            }
        });
    }

    /**
     * Abort every open connection, the server's side too unless `serverSide`
     * is false, refuse new ones for `refuseS`, and hold the video frames of
     * those opened in the next `holdVideoS` for that long.
     * @returns How many connections there were.
     */
    cut(options: { serverSide?: boolean; refuseS?: number; holdVideoS?: number } = {}): number {
        const { serverSide = true, refuseS = 0, holdVideoS = 0 } = options;
        this.refuseUntil = Date.now() + refuseS * 1000;
        this.holdVideoUntil = Date.now() + holdVideoS * 1000;
        const count = this.pairs.size;
        for (const pair of [...this.pairs]) {
            pair.halfOpen = !serverSide;
            pair.client.destroy();
            if (serverSide) pair.upstream.destroy();
        }
        return count;
    }

    /** Stop listening, so a later relay can take the port. */
    async stop(): Promise<void> {
        this.cut();
        for (const pair of this.pairs) pair.upstream.destroy();
        await new Promise<void>((resolve) => { this.server.close(() => { resolve(); }); });
    }
}

/**
 * Timestamps (performance.now) of every change of the stream's picture with
 * its mean luma, of the status notice showing or hiding, the playback
 * contexts made, the playback level every 50 ms of each playback worklet the
 * page builds, and the microphone button's states.
 */
export const HOOKS = `
window.__cut = {frames: [], overlay: [], acs: 0, levels: [], mic: []};
// Absent from a realm that is not a page (Gecko runs the script in more of them).
if (window.AudioContext && window.AudioWorkletNode) {
  const AC = window.AudioContext;
  window.AudioContext = class extends AC {
    constructor(...a) { super(...a); window.__cut.acs++; }
  };
  const AWN = window.AudioWorkletNode;
  window.AudioWorkletNode = class extends AWN {
    constructor(...a) {
      super(...a);
      if (a[1] !== 'audio-frame-processor') return;
      const port = this.port;
      port.addEventListener('message', (e) => {
        if (e.data && e.data.type === 'audioBufferSize') window.__cut.levels.push([performance.now(), e.data.level]);
      });
      setInterval(() => { try { port.postMessage({type: 'getBufferSize'}); } catch (e) {} }, 50);
    }
  };
}
window.addEventListener('message', (e) => {
  const d = e.data;
  if (d && d.type === 'sidebarButtonStatusUpdate' && d.microphone !== undefined) window.__cut.mic.push(d.microphone);
});
(() => {
  const probe = document.createElement('canvas');
  probe.width = 32; probe.height = 18;
  let pctx = null, last = null, shown = null;
  const tick = () => {
    // The stream shows on a <video> (a track generator) or on a canvas, whichever is displayed.
    const c = [...document.querySelectorAll('video, canvas')].find((x) =>
      (x.videoWidth || x.width) >= 640 && getComputedStyle(x).display !== 'none');
    if (c) {
      try {
        pctx = pctx || probe.getContext('2d', {willReadFrequently: true});
        pctx.drawImage(c, 0, 0, 32, 18);
        const d = pctx.getImageData(0, 0, 32, 18).data;
        let h = 0, sum = 0;
        for (let i = 0; i < d.length; i += 4) { h = (h * 31 + d[i] + d[i + 1] * 7 + d[i + 2] * 13) | 0; sum += d[i + 1]; }
        if (h !== last) { last = h; window.__cut.frames.push([performance.now(), Math.round(sum / (d.length / 4))]); }
      } catch (e) { /* not drawable yet */ }
    }
    const s = document.getElementById('status-display');
    const now = !!s && !s.classList.contains('hidden') && getComputedStyle(s).display !== 'none'
      && getComputedStyle(s).visibility !== 'hidden' && !!s.textContent;
    if (now !== shown) { shown = now; window.__cut.overlay.push([performance.now(), now, s ? s.textContent : '']); }
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
})();
`;

/** What the hooks recorded. */
export interface CutState {
    /** [at, mean luma] per picture change. */
    frames: [number, number][];
    /** [at, shown, text] per change of the notice. */
    overlay: [number, boolean, string][];
    /** Playback contexts made. */
    acs: number;
    /** [at, level] per playback reading. */
    levels: [number, number][];
    /** The microphone button's states. */
    mic: boolean[];
}

declare global {
    interface Window {
        __cut?: CutState;
    }
}

/** What the hooks have recorded so far. */
export function cutState(page: Page): Promise<CutState> {
    return within(page.evaluate(() => window.__cut ?? { frames: [], overlay: [], acs: 0, levels: [], mic: [] }));
}

/** The longest the picture stood still from just before `after` to WINDOW_MS past it, in ms. */
export function longestStall(frames: readonly [number, number][], after: number): number {
    const times = frames.map(([t]) => t).filter((t) => after - 200 <= t && t <= after + WINDOW_MS);
    if (times.length < 2) return WINDOW_MS;
    return Math.max(...times.slice(1).map((t, i) => t - times[i]));
}

/** The longest playback stayed silent within WINDOW_MS of `after`, in ms. */
export function longestSilence(levels: readonly [number, number][], after: number, floor = 0.01): number {
    let longest = 0;
    let quietSince: number | null = null;
    for (const [t, level] of levels) {
        if (!(after <= t && t <= after + WINDOW_MS)) continue;
        if (level < floor) {
            quietSince ??= t;
            longest = Math.max(longest, t - quietSince);
        } else {
            quietSince = null;
        }
    }
    return longest;
}

/** The page's clock. */
export function pageNow(page: Page): Promise<number> {
    return within(page.evaluate(() => performance.now()));
}

/** Sleep until the page's clock reads `at`. */
export async function waitUntil(page: Page, at: number): Promise<void> {
    await sleep(Math.max(0, at - await pageNow(page)));
}

/** Post `message` (a script expression) to the page, as the dashboard does. */
export async function post(page: Page, message: string): Promise<void> {
    await within(page.evaluate(`window.postMessage(${message}, window.location.origin)`));
}

/**
 * The keycode of each character on the X display.
 * @returns keycode -> character.
 */
export async function keycodes(host: Host, chars: string): Promise<Map<number, string>> {
    const out = new Map<number, string>();
    for (const ch of chars) {
        const code = await host.keycode(ch.charCodeAt(0));
        if (code !== null) out.set(code, ch);
    }
    return out;
}
