/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What a page of the client shows of its stream: video on either transport,
 * and its console's errors, with the bound every page call is held to.
 * @module
 */

import type { Browser, BrowserContext, Page } from '@playwright/test';

import { PAGE_CALL_TIMEOUT_S, sleep } from './env.ts';
import { reportStall } from './stall.ts';

/** A page call the engine never answered: what the driver repeats a suite once for. */
export class PageStalled extends Error {}

/**
 * `call`, failing with `PageStalled` past `seconds` (the suites' page-call
 * bound by default): `evaluate` and `close` wait on the engine with no
 * deadline of their own.
 * @param call The browser call.
 * @param what Who did not answer, for the message.
 * @param seconds The bound; 0 waits for ever.
 * @returns What the call returned.
 */
export async function within<T>(call: Promise<T>, what = 'the page', seconds = PAGE_CALL_TIMEOUT_S): Promise<T> {
    if (seconds <= 0) return call;
    let timer: NodeJS.Timeout | undefined;
    const stalled = new Promise<never>((_, reject) => {
        timer = setTimeout(() => {
            const why = `${what} did not answer within ${seconds.toFixed(0)}s`;
            void reportStall(why).finally(() => { reject(new PageStalled(why)); });
        }, seconds * 1000);
    });
    try {
        return await Promise.race([call, stalled]);
    } finally {
        clearTimeout(timer);
    }
}

/** A video's size, and its audio track count on WebRTC (-1 when it has no stream). */
export interface VideoSize {
    w: number;
    h: number;
    audio?: number;
}

/**
 * Wait for WebSocket video: a stream-sized canvas and a received chunk. The
 * client sizes its canvas from layout messages before any frame arrives, so
 * a received chunk is required as well.
 * @param page The client.
 * @param timeoutS How long to wait.
 * @returns The canvas size, or null on a timeout or a page that stopped answering.
 */
export async function waitWsVideo(page: Page, timeoutS = 15): Promise<VideoSize | null> {
    const deadline = Date.now() + timeoutS * 1000;
    while (Date.now() < deadline) {
        try {
            const info = await within(page.evaluate((): VideoSize | null => {
                if (!(Number(Reflect.get(window, 'videoChunksReceived')) > 0)) return null;
                for (const c of document.querySelectorAll('canvas')) {
                    if (c.width >= 640) return { w: c.width, h: c.height };
                }
                return null;
            }));
            if (info) return info;
        } catch (e) {
            if (!(e instanceof PageStalled)) throw e;
            process.stdout.write(`[ws-video] ${e.message}\n`);
            return null;
        }
        await sleep(500);
    }
    return null;
}

/**
 * What the page's `<video>` and peer connections report, for a stream that
 * never showed: element state and the inbound counters.
 * @param page The client.
 * @returns The report, or why it could not be read.
 */
export async function wrVideoState(page: Page): Promise<unknown> {
    try {
        return await within(page.evaluate(async () => {
            const v = document.querySelector('video');
            const stream = v?.srcObject instanceof MediaStream ? v.srcObject : null;
            const video = v ? {
                readyState: v.readyState, w: v.videoWidth, h: v.videoHeight, paused: v.paused,
                error: v.error ? v.error.code : null,
                tracks: stream ? stream.getTracks().map((t) => `${t.kind}:${t.readyState}${t.muted ? ':muted' : ''}`) : null,
            } : null;
            const tapped: unknown = Reflect.get(window, '__pcs');
            const pcs = [];
            for (const pc of Array.isArray(tapped) ? tapped : []) {
                if (!(pc instanceof RTCPeerConnection)) continue;
                const inbound: Record<string, unknown>[] = [];
                let statsError: string | undefined;
                try {
                    const stats = await pc.getStats();
                    const codecs = new Map<unknown, string>();
                    stats.forEach((r: Record<string, unknown>) => {
                        const text = (v: unknown): string => (typeof v === 'string' ? v : '');
                        if (r.type === 'codec') codecs.set(r.id, `${text(r.mimeType)} ${text(r.sdpFmtpLine)}`);
                    });
                    stats.forEach((r: Record<string, unknown>) => {
                        if (r.type !== 'inbound-rtp') return;
                        inbound.push({
                            kind: r.kind, packets: r.packetsReceived, bytes: r.bytesReceived, lost: r.packetsLost,
                            frames: r.framesReceived, decoded: r.framesDecoded, dropped: r.framesDropped,
                            keyframes: r.keyFramesDecoded, pli: r.pliCount, decoder: r.decoderImplementation,
                            codec: codecs.get(r.codecId) ?? r.codecId,
                        });
                    });
                } catch (e) { statsError = String(e); }
                pcs.push({ connection: pc.connectionState, ice: pc.iceConnectionState, signaling: pc.signalingState, inbound, statsError });
            }
            return { video, pcs };
        }));
    } catch (e) {
        return `unreadable: ${String(e)}`;
    }
}

/**
 * Wait for a decoding WebRTC `<video>`; on a timeout, what the page received
 * and decoded is printed, so a stream that never showed can be read from the
 * run's output.
 * @param page The client.
 * @param timeoutS How long to wait.
 * @returns The video's size and audio tracks, or null.
 */
export async function waitWrVideo(page: Page, timeoutS = 45): Promise<VideoSize | null> {
    const deadline = Date.now() + timeoutS * 1000;
    while (Date.now() < deadline) {
        try {
            const info = await within(page.evaluate((): VideoSize | null => {
                const v = document.querySelector('video');
                if (v && v.readyState >= 2 && v.videoWidth > 0) {
                    const stream = v.srcObject instanceof MediaStream ? v.srcObject : null;
                    return { w: v.videoWidth, h: v.videoHeight, audio: stream ? stream.getAudioTracks().length : -1 };
                }
                return null;
            }));
            if (info) return info;
        } catch (e) {
            if (!(e instanceof PageStalled)) throw e;
            process.stdout.write(`[wr-video] ${e.message}\n`);
            return null;
        }
        await sleep(500);
    }
    process.stdout.write(`[wr-video] no frame shown in ${timeoutS.toFixed(0)}s: ${JSON.stringify(await wrVideoState(page))}\n`);
    return null;
}

/**
 * The console errors and 404s that are not known noise: a failed resource
 * load, the client's own 409 transport probe, and the browser's manifest and
 * favicon probes.
 * @param errors The console errors seen.
 * @param notFound The URLs answered 404.
 * @returns The real ones of each.
 */
export function realConsole(errors: readonly string[], notFound: readonly string[] = []): { errors: string[]; notFound: string[] } {
    const benign = ['Failed to load resource', 'Unexpected server response:'];
    return {
        errors: errors.filter((e) => !benign.some((b) => e.includes(b))),
        notFound: notFound.filter((u) => !(u.endsWith('/manifest.json') || u.includes('favicon'))),
    };
}

/**
 * Close a browser or a persistent context, abandoning one that will not go:
 * the checks are done by the time anything closes, so a browser that does
 * not answer is left to the driver to reap.
 * @param closer The browser or context.
 * @param timeoutS How long it is given.
 * @returns Whether it closed on its own.
 */
export async function closeBrowser(closer: Browser | BrowserContext, timeoutS = 30): Promise<boolean> {
    try {
        await within(closer.close(), 'the browser', timeoutS);
        return true;
    } catch (e) {
        if (!(e instanceof PageStalled)) return true;
        process.stdout.write(`note: the browser did not close within ${timeoutS.toFixed(0)}s; abandoning it\n`);
        return false;
    }
}

declare global {
    interface Window {
        fps?: number;
    }
}

/**
 * The first non-zero fps the client reports.
 * @param page The client.
 * @param timeoutS How long to wait.
 * @returns The fps, or 0 when none came or the page stopped answering.
 */
export async function pageFps(page: Page, timeoutS = 10): Promise<number> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        try {
            const fps = await within(page.evaluate(() => window.fps ?? 0));
            if (fps > 0) return fps;
        } catch (e) {
            if (!(e instanceof PageStalled)) throw e;
            process.stdout.write(`[page-fps] ${e.message}\n`);
            return 0;
        }
        await sleep(500);
    }
    return 0;
}
