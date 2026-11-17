/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * A picture of two known colors painted on the server, and the page's decoded
 * frame sampled against it: X11 windows on the test display, or the Wayland
 * observer's surface filled with one and carrying the other. A live picture
 * also changes on its own away from the sampled points.
 * @module
 */

import type { Page } from '@playwright/test';

import { sleep } from './env.ts';
import type { Checks } from './checks.ts';
import type { Held, Host } from './host.ts';
import { within } from './stream.ts';

const WL_SOCKET = 'wayland-1';
// The color painted on the server, and how far a decoded sample may stray
// from it (4:2:0 chroma, limited range, and two codecs' rounding).
export const PAINT: readonly number[] = [40, 120, 220];
export const PAINT_ARGB = 'ff2878dc';
const TOLERANCE = 24;
// A saturated second block guards the color matrix: a stream converted with
// one matrix and painted with another lands more than twenty levels off here,
// where the first block barely moves. Red rather than green, because libyuv,
// which Chromium and Firefox both convert through, clamps the BT.709
// Cb-to-blue coefficient to 2.0 from 2.112, which costs a green block eleven
// levels of blue and this one two.
export const SATURATED: readonly number[] = [255, 0, 0];
export const SATURATED_ARGB = 'ffff0000';
export const SATURATED_TOLERANCE = 10;
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

/** A decoded frame's sampled points. */
export interface Sample {
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
export function near(rgb: number[] | null | undefined, want: readonly number[], tolerance = TOLERANCE): boolean {
    return rgb != null && rgb.every((v, i) => Math.abs(v - want[i]) <= tolerance);
}

/**
 * The decoded picture at three points: whichever sink is showing (the
 * <video> a full-frame mode renders through, else the canvas), drawn once
 * into a scratch canvas.
 * @param page The client.
 * @param points The points read as `inside`, `outside` and `saturated`, x then y.
 * @returns The sample, or null with no sink yet.
 */
export function sampleAt(page: Page, points: readonly number[]): Promise<Sample | null> {
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
    }, points));
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
export class Picture {
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

    /**
     * Whether the <video> keeps presenting frames on the live picture.
     * @param res The checks.
     * @param tag The block's name.
     * @param page The client.
     * @param waived Why an engine cannot be held to it, which skips the check.
     */
    async keepsPresenting(res: Checks, tag: string, page: Page, waived = ''): Promise<void> {
        const name = `${tag}: frames keep presenting on a changing screen`;
        if (waived) {
            res.skip(name, waived);
            return;
        }
        const start = await presented(page);
        await sleep(LIVE_S * 1000);
        const frames = await presented(page);
        const live = { frames: frames === null ? null : frames - (start ?? 0), secs: LIVE_S, flips: Math.round(LIVE_S * 1000 / LIVE_FLIP_MS) };
        res.check(name, (live.frames ?? 0) >= LIVE_MIN_FRAMES, live);
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
            sample = await sampleAt(page, [...INSIDE, ...OUTSIDE, ...INSIDE2]);
            if (this.matches(sample, matrix)) return sample;
            await sleep(500);
        }
        return sample;
    }
}

