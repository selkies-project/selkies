/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What the suites that make a WebRTC sender drop frames share
 * (video_drop_recovery, reference_invalidation): the server they run, the
 * decoded motion scene read against the frame it spells, the page's inbound
 * video counters, and the server's bridge counters.
 * @module
 */

import type { Page } from '@playwright/test';

import { sleep } from './env.ts';
import { checkPicture, CLEAN_FRACTION, CORRUPT_FRACTION, SAMPLE_JS } from './scene.ts';
import { within } from './stream.ts';

/** How long the picture is read once a load ends. */
export const SETTLE_S = 5;

// Keeps every RTCPeerConnection reachable for getStats and pins the transport.
export const INIT_JS = `
  window.__SELKIES_STREAMING_MODE__ = 'webrtc';
  window.__pcs = [];
  if (window.RTCPeerConnection) {
    const Orig = window.RTCPeerConnection;
    const Wrapped = function(...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    Wrapped.prototype = Orig.prototype;
    Object.setPrototypeOf(Wrapped, Orig);
    window.RTCPeerConnection = Wrapped;
  }
`;

const STATS_JS = `async () => {
  const out = {keyframes: 0, frames: 0, lost: 0, nacks: 0, plis: 0, dropped: 0, received: 0};
  for (const pc of window.__pcs || []) {
    const rep = await pc.getStats();
    rep.forEach(r => {
      if (r.type === 'inbound-rtp' && (r.kind === 'video' || r.mediaType === 'video')) {
        out.keyframes += r.keyFramesDecoded || 0;
        out.frames += r.framesDecoded || 0;
        out.lost += r.packetsLost || 0;
        out.nacks += r.nackCount || 0;
        out.plis += r.pliCount || 0;
        out.dropped += r.framesDropped || 0;
        out.received += r.packetsReceived || 0;
      }
    });
  }
  return out;
}`;

/** A decoded picture read against the frame its index names. */
export interface Sampled {
    index: number;
    ambiguous: number;
    mismatch: number;
    w: number;
    h: number;
}

/** The inbound video counters, summed over the page's connections. */
export interface Stats {
    keyframes: number;
    frames: number;
    lost: number;
    nacks: number;
    plis: number;
    dropped: number;
    received: number;
}

/** The decoded picture read against its own frame, or null while there is none. */
export async function sample(page: Page): Promise<Sampled | null> {
    try {
        return await checkPicture(page, SAMPLE_JS);
    } catch {
        return null;
    }
}

/** The page's inbound video counters. */
export function stats(page: Page): Promise<Stats> {
    return within(page.evaluate<Stats>(`(${STATS_JS})()`));
}

export function corrupt(s: Sampled): boolean {
    return s.ambiguous > 0 || s.mismatch > CORRUPT_FRACTION;
}

/** The first sample that decodes and matches its own frame. */
export async function waitClean(page: Page, timeoutS: number): Promise<Sampled | null> {
    let last: Sampled | null = null;
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        last = await sample(page);
        if (last && last.ambiguous === 0 && last.mismatch <= CLEAN_FRACTION) return last;
        await sleep(250);
    }
    return last;
}

/**
 * The primary display's bridge counter of that name from /api/metrics:
 * `dropped` for the frames it let go, `invalidated` for those of them the
 * encoder was told to predict past.
 * @param url The server.
 * @param name The counter.
 * @returns Its value, 0 where the server publishes no such counter, null where
 *     the metrics do not answer.
 */
export async function bridgeCounter(url: string, name: string): Promise<number | null> {
    let body: string;
    try {
        const res = await fetch(`${url}/api/metrics`);
        if (res.status !== 200) return null;
        body = await res.text();
    } catch {
        return null;
    }
    const m = new RegExp(`webrtc_bridge_${name}_frames\\{display="primary"\\}\\s+([0-9.eE+]+)`).exec(body);
    return m ? Math.trunc(Number(m[1])) : 0;
}

/** The server a drop load runs against: H.264 at a constant rate well past what the sender keeps up with on a shared CPU. */
export function serverEnv(cell: string): Record<string, string> {
    const env: Record<string, string> = {
        SELKIES_ENCODER: 'h264enc',
        SELKIES_FRAMERATE: '60,8-240',
        SELKIES_VIDEO_BITRATE: '40000,100-1000000',
        SELKIES_RATE_CONTROL_MODE: 'cbr',
        SELKIES_KEYFRAME_INTERVAL: '0',
        SELKIES_VIDEO_STREAMING_MODE: 'true',
        SELKIES_USE_PAINT_OVER_QUALITY: 'true',
        SELKIES_WEBRTC_PACER: 'true',
        SELKIES_CONGESTION_CONTROL: 'true',
        SELKIES_ENABLE_METRICS_HTTP: 'true',
    };
    if (cell === 'cpu') env.SELKIES_USE_CPU = 'true';
    return env;
}

