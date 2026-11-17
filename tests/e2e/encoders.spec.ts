/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The encoders past the defaults, decoded by a real browser on every cell.
//
// jpeg:   a server configured with jpeg. Over WebSockets the stream arrives as
//         several JPEG stripes per frame, decoded and presented by the video
//         worker, and the picture decodes. WebRTC carries no striped framing,
//         so there the configured encoder is refused with a log line and the
//         stream comes up on h264enc.
// cpu:    h264enc forced onto the software encoder (use_cpu): the stream is a
//         single full-frame stripe, the log names the CPU encoder, and the
//         picture decodes.
// switch: over WebSockets the dashboard's encoder select moves a live session
//         from the default encoder onto jpeg and back; the server restarts
//         the capture each way and the picture survives. Over WebRTC the same
//         select offers the full-frame encoders alone.
//
// The picture is two known colors painted on the server: X11 windows on the
// test display, or the Wayland observer's surface filled with one and
// carrying the other, sampled from the decoded frame in the page. Over
// WebSockets the page runs in Chromium; over WebRTC each block runs in
// Chromium, Firefox and WebKit, whose own RTP receivers decode and paint the
// stream, and the <video> must keep presenting frames while the screen
// changes. The cpu and switch servers run Turbo, so the H.264 frames read off
// the wire keep coming while the painted picture holds still. JPEG sends a
// still screen once and its paint-over once, then nothing, so the jpeg block
// paints a picture that keeps changing away from the sampled points.
import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { FF_E2E_PROFILE, openh264Version, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { openVideoSettings } from '../lib/dashboard.ts';
import { encoderField, ENGINES, lastStreamLine, openPage, TURBO } from '../lib/encode.ts';
import { Host } from '../lib/host.ts';
import { Picture } from '../lib/picture.ts';
import { waitWrVideo, waitWsVideo, within, type VideoSize } from '../lib/stream.ts';

function waitVideo(page: Page, mode: string): Promise<VideoSize | null> {
    return mode === 'websockets' ? waitWsVideo(page, 30) : waitWrVideo(page);
}

/** The divert state the core publishes. */
interface Divert {
    on: boolean;
    rows: number;
    fps: number;
}

/**
 * Poll the divert state until it matches. `presented` also waits for a frame
 * to have been presented: the row layout is published as the first stripes
 * are decoded, the frame rate over a window that closes later.
 */
async function waitDivert(page: Page, want: boolean, timeoutS = 15, presentedToo = false): Promise<Divert> {
    let state: Divert = { on: false, rows: 0, fps: 0 };
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        state = await within(page.evaluate((): Divert => ({
            on: Boolean(window.videoDivertOn),
            rows: Object.keys(window.videoStripeRows ?? {}).length,
            fps: window.fps ?? 0,
        })));
        const settled = state.rows > 0 && (!presentedToo || state.fps > 0);
        if (state.on === want && (!want || settled)) return state;
        await sleep(500);
    }
    return state;
}

/**
 * Stripe Y start -> height seen on the wire so far: the page tap merged with
 * the row layout the video worker reports while the divert holds.
 */
async function stripes(page: Page): Promise<Record<number, number>> {
    const seen = await within(page.evaluate(() => Object.assign({}, window.videoStripeRows ?? {}, window.__stripes ?? {})));
    const out: Record<number, number> = {};
    for (const [k, v] of Object.entries(seen)) out[Number(k)] = v;
    return out;
}

/** Poll the wire until it shows the striping asked for, or time runs out. */
async function waitStripes(page: Page, striped: boolean, timeoutS = 15): Promise<Record<number, number>> {
    let seen: Record<number, number> = {};
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        await within(page.evaluate(() => { window.__stripes = {}; window.videoStripeRows = {}; }));
        await sleep(1200);
        seen = await stripes(page);
        const n = Object.keys(seen).length;
        if (n && (n > 1) === striped) return seen;
    }
    return seen;
}

function isStriped(seen: Record<number, number>, frameH: number): boolean {
    const heights = Object.values(seen);
    return heights.length > 1 && Math.max(...heights) < frameH;
}

function isFullframe(seen: Record<number, number>, frameH: number): boolean {
    const keys = Object.keys(seen);
    return keys.length === 1 && keys[0] === '0' && seen[0] === frameH;
}

/** How many times pixelflux has printed its stream line. */
async function streamLineCount(host: Host): Promise<number> {
    return (await host.log()).split('Stream settings active').length - 1;
}

/** The stream line once pixelflux has printed more than `count` of them. */
async function waitStreamLine(host: Host, count: number, timeoutS = 20): Promise<string> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if ((await streamLineCount(host)) > count) return lastStreamLine(host);
        await sleep(500);
    }
    return '';
}

/**
 * Whether a stream line names the software H.264 encoder: pixelflux prints
 * it as CPU on X11 and by its library name on Wayland, never as a GPU.
 */
function cpuEncoder(line: string): boolean {
    const software = ['CPU', 'x264', 'OpenH264', 'openh264'].some((n) => line.includes(n));
    return software && !line.includes('NVENC') && !line.includes('VAAPI');
}

async function blockJpeg(host: Host, mode: string, wayland: boolean, res: Checks, engine: Engine): Promise<void> {
    const tag = engine === 'chromium' ? 'jpeg' : `jpeg ${engine}`;
    const { url } = await host.start({ label: `jpeg-${engine}`, mode, wayland, env: { SELKIES_ENCODER: 'jpeg' } });
    const picture = new Picture(host, wayland, true);
    try {
        await picture.paint();
        const [owner, page] = await openPage(url, mode, engine);
        try {
            const video = await waitVideo(page, mode);
            res.check(`${tag}: stream up`, Boolean(video), video);
            if (mode === 'websockets') {
                const enc = await within(page.evaluate(() => window.encoder ?? null));
                res.check(`${tag}: client follows the server's encoder`, enc === 'jpeg', enc);
                const seen = await waitStripes(page, true);
                res.check(`${tag}: several stripes per frame`, video !== null && isStriped(seen, video.h), seen);
                const divert = await waitDivert(page, true, 15, true);
                res.check(`${tag}: the video worker decodes and presents it`, divert.on && divert.rows > 1 && divert.fps > 0, divert);
            } else {
                res.check(`${tag}: refused for WebRTC, h264enc used instead`,
                    (await host.saw('not available for WebRTC', 5)) && (await host.saw('using "h264enc"', 5)), '');
            }
            const sample = await picture.wait(page);
            res.check(`${tag}: the painted picture decodes`, picture.matches(sample), sample);
            if (mode === 'webrtc') await picture.keepsPresenting(res, tag, page);
        } finally {
            await owner.close();
        }
    } finally {
        await picture.clear();
        await host.stop();
    }
}

async function blockCpu(host: Host, mode: string, wayland: boolean, res: Checks, engine: Engine): Promise<void> {
    const tag = engine === 'chromium' ? 'cpu' : `cpu ${engine}`;
    if (mode === 'webrtc' && engine === 'firefox' && !openh264Version()) {
        res.skip(`${tag}: no OpenH264 GMP plugin in ${FF_E2E_PROFILE}`, 'run tests/tools/fetch-openh264.sh to cover H.264 in Firefox');
        return;
    }
    const { url } = await host.start({ label: `cpu-${engine}`, mode, wayland, env: { SELKIES_ENCODER: 'h264enc', SELKIES_USE_CPU: 'true', ...TURBO } });
    const picture = new Picture(host, wayland, mode === 'webrtc');
    try {
        await picture.paint();
        const [owner, page] = await openPage(url, mode, engine);
        try {
            const video = await waitVideo(page, mode);
            res.check(`${tag}: stream up`, Boolean(video), video);
            const line = await waitStreamLine(host, 0);
            res.check(`${tag}: software encoder in use`, cpuEncoder(line), encoderField(line));
            if (mode === 'websockets') {
                const seen = await waitStripes(page, false);
                res.check(`${tag}: one full-frame stripe on the wire`, video !== null && isFullframe(seen, video.h), seen);
            }
            const sample = await picture.wait(page);
            res.check(`${tag}: the painted picture decodes`, picture.matches(sample), sample);
            if (mode === 'webrtc') await picture.keepsPresenting(res, tag, page);
        } finally {
            await owner.close();
        }
    } finally {
        await picture.clear();
        await host.stop();
    }
}

async function blockSwitch(host: Host, mode: string, wayland: boolean, res: Checks, engine: Engine): Promise<void> {
    const tag = engine === 'chromium' ? 'switch' : `switch ${engine}`;
    const { url } = await host.start({ label: `switch-${engine}`, mode, wayland, env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, ...TURBO } });
    const picture = new Picture(host, wayland);
    try {
        await picture.paint();
        const [owner, page] = await openPage(url, mode, engine);
        try {
            const video = await waitVideo(page, mode);
            res.check(`${tag}: stream up`, Boolean(video), video);
            const opened = await openVideoSettings(page, 'default');
            const options = opened
                ? await within(page.evaluate(() => [...document.querySelectorAll('#encoderSelect option')].map((o) => (o instanceof HTMLOptionElement ? o.value : ''))))
                : [];
            if (mode === 'webrtc') {
                // WebRTC carries the full-frame encoders alone, those the engine's
                // RTP receiver takes; JPEG's striped framing never shows.
                const fullFrame = ['h264enc', 'h265enc', 'vp8enc', 'vp9enc', 'av1enc'];
                res.check(`${tag}: the dashboard offers WebRTC only full-frame encoders`,
                    opened && options.includes('h264enc') && options.every((o) => fullFrame.includes(o)), [opened, options]);
                return;
            }
            res.check(`${tag}: the dashboard offers jpeg and no striped H.264`, options.includes('jpeg') && !options.includes('h264enc-striped'), options);
            let seen = await waitStripes(page, false);
            res.check(`${tag}: default stream is full-frame`, video !== null && isFullframe(seen, video.h), seen);
            let before = await streamLineCount(host);
            await page.selectOption('#encoderSelect', 'jpeg');
            let line = await waitStreamLine(host, before);
            res.check(`${tag}: capture restarted on jpeg`, Boolean(line), encoderField(line));
            seen = await waitStripes(page, true);
            res.check(`${tag}: jpeg stripes on the wire`, video !== null && isStriped(seen, video.h), seen);
            const divert = await waitDivert(page, true);
            res.check(`${tag}: jpeg decodes in the video worker`, divert.on && divert.rows > 1, divert);
            let sample = await picture.wait(page);
            res.check(`${tag}: the picture decodes as jpeg`, picture.matches(sample), sample);
            before = await streamLineCount(host);
            await page.selectOption('#encoderSelect', 'h264enc');
            line = await waitStreamLine(host, before);
            res.check(`${tag}: capture restarted back on h264enc`, Boolean(line), encoderField(line));
            seen = await waitStripes(page, false);
            res.check(`${tag}: full-frame again on the wire`, video !== null && isFullframe(seen, video.h), seen);
            sample = await picture.wait(page);
            res.check(`${tag}: the picture decodes full-frame again`, picture.matches(sample), sample);
        } finally {
            await owner.close();
        }
    } finally {
        await picture.clear();
        await host.stop();
    }
}

for (const which of ['ws-x11', 'wr-x11', 'ws-wl', 'wr-wl']) {
    test(which, async () => {
        const [transport, backend] = which.split('-');
        const mode = transport === 'ws' ? 'websockets' : 'webrtc';
        const wayland = backend === 'wl';
        const res = new Checks(`encoders-${which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            for (const engine of mode === 'webrtc' ? ENGINES : ENGINES.slice(0, 1)) {
                for (const block of [blockJpeg, blockCpu, blockSwitch]) await block(host, mode, wayland, res, engine);
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}

