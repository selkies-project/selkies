/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Every video codec, decoded by each browser engine, over both transports.
//
// For each codec the server is started with SELKIES_ENCODER set to its
// encoder and the page opens in Chromium, Firefox, or WebKit. Over WebSockets
// the stream must come up and the painted picture decode whatever the engine
// can do: an engine whose WebCodecs decoder takes the codec keeps it (the
// page's encoder stays the requested one and the server's stream line names
// the codec), one that refuses it steps down the client's ladder to h264enc
// and the picture still decodes there. The engine's own answer to
// VideoDecoder.isConfigSupported decides which of the two is required, except
// that an engine which takes the configuration and refuses the stream at
// decode (the page says so) is held to the refusal outcome. Over WebRTC the
// engine's own RTP receiver decides (RTCRtpReceiver.getCapabilities): a codec
// it takes is negotiated and streamed, one it declines is answered with H.264
// and the display moves to h264enc, logged by the server.
//
// The picture is a known color the test paints on the server, sampled from
// the decoded frame in the page, as the encoders suite does
// (tests/lib/picture.ts); over WebRTC the <video> must then keep presenting
// frames while the screen changes. The server runs Turbo, so the frame rate
// read over WebSockets is the stream's while the painted picture holds still.
//
// A selector may carry the engine as a third part (ws-x11-firefox); without
// one every engine runs.
//
// av1crop (ws-x11-av1crop, also run last by ws-x11 and ws-x11-chromium) holds
// the AV1 picture a hardware VA-API encoder codes to the size it was asked
// for: some drivers pad the coded size (AMD VCN 4 codes 1920x1080 as
// 1920x1082 and 1366x768 as 1408x768) and Chromium's decoder ignores the
// render size that trims it, so the page itself must crop. It skips without a
// VA-API AV1 encoder.
import { existsSync } from 'node:fs';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { sleep, suiteOf } from '../lib/env.ts';
import { closeBrowser, waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';
import { FF_E2E_PROFILE, launchBrowser, openh264Version, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { encoderField, ENGINES, lastStreamLine, openPage, streamLines, TURBO } from '../lib/encode.ts';
import { Host } from '../lib/host.ts';
import { near, PAINT, PAINT_ARGB, Picture, SATURATED, SATURATED_ARGB, SATURATED_TOLERANCE, sampleAt, type Sample } from '../lib/picture.ts';

// (encoder, the stream line's mode, the WebCodecs codec string, the RTP mime type)
const CODECS: readonly (readonly [string, string, string, string])[] = [
    ['h264enc', 'H264', 'avc1.64001F', 'video/H264'],
    ['h265enc', 'H265', 'hev1.1.6.L93.B0', 'video/H265'],
    ['vp8enc', 'VP8', 'vp8', 'video/VP8'],
    ['vp9enc', 'VP9', 'vp09.00.31.08', 'video/VP9'],
    ['av1enc', 'AV1', 'av01.0.05M.08', 'video/AV1'],
];

// What the AV1 crop block streams: sizes where AMD pads the coded height or
// width, with a bar painted over the last rows and one over the last columns.
const CROP_SIZES: readonly (readonly [number, number])[] = [[1920, 1080], [1366, 768]];
const CROP_BAR = 8;

/** Whether the engine's WebCodecs decoder takes `codec`. */
function probe(page: Page, codec: string): Promise<boolean> {
    return within(page.evaluate(async (c) => {
        if (typeof VideoDecoder === 'undefined') return false;
        try {
            const s = await VideoDecoder.isConfigSupported({ codec: c, codedWidth: 1280, codedHeight: 720 });
            return Boolean(s.supported);
        } catch {
            return false;
        }
    }, codec));
}

/** Whether the engine's RTP receiver takes `mime`. */
function rtpProbe(page: Page, mime: string): Promise<boolean> {
    return within(page.evaluate((m) => {
        try {
            const caps = RTCRtpReceiver.getCapabilities('video');
            return Boolean(caps) && (caps?.codecs ?? []).some((c) => c.mimeType.toLowerCase() === m.toLowerCase());
        } catch {
            return false;
        }
    }, mime));
}

/**
 * The server's stream line naming `modeName` once one is printed, else the
 * last one seen. A pipeline prints its line from its capture thread when the
 * encoder comes up, so a stopped pipeline's can land after its replacement's:
 * the newest line alone does not say what streams now.
 */
async function waitStreamMode(host: Host, modeName: string, timeoutS = 15): Promise<string> {
    for (const until = Date.now() + timeoutS * 1000; ;) {
        const lines = await streamLines(host);
        const named = lines.filter((l) => l.includes(`Mode: ${modeName}`));
        if (named.length) return named[named.length - 1];
        if (Date.now() >= until) return lines.at(-1) ?? '';
        await sleep(500);
    }
}

function pageEncoder(page: Page): Promise<string | null> {
    return within(page.evaluate(() => window.encoder ?? null));
}

/**
 * The page's encoder once it has stopped moving for a few seconds: the
 * ladder answers a refusal within a second of the first frame.
 */
async function waitSettledEncoder(page: Page, timeoutS = 20): Promise<string | null> {
    let last: string | null = null;
    let since = Date.now();
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        const enc = await pageEncoder(page);
        if (enc !== last) {
            last = enc;
            since = Date.now();
        } else if (Date.now() - since > 4000) {
            return enc;
        }
        await sleep(500);
    }
    return last;
}

async function blockCodec(host: Host, mode: string, wayland: boolean, engine: Engine, codec: readonly [string, string, string, string], res: Checks): Promise<void> {
    const [encoder, modeName, probeCodec, rtpMime] = codec;
    const tag = `${engine} ${encoder}`;
    // An H.265 stream Firefox declines moves to H.264, which it takes over RTP from OpenH264 alone.
    if (mode === 'webrtc' && engine === 'firefox' && (encoder === 'h264enc' || encoder === 'h265enc') && !openh264Version()) {
        res.skip(`${tag}: no OpenH264 GMP plugin in ${FF_E2E_PROFILE}`, 'run tests/tools/fetch-openh264.sh to cover H.264 in Firefox');
        return;
    }
    // Every stream declares the matrix it converts with -- BT.709, or BT.601
    // for VP8, whose keyframe header carries a single bit that can name no
    // other. Two engines paint some streams with a matrix of their own, twenty
    // levels off on the saturated block through no fault of the stream:
    // WebKit's GStreamer ports take a WebCodecs VP8 stream as BT.709 above 576
    // lines (over WebRTC the color-space header extension reaches them), and
    // Firefox before 157 decodes WebRTC AV1 through libwebrtc's dav1d, whose
    // frames it paints as BT.601 whatever they declare.
    let matrix = !(engine === 'webkit' && encoder === 'vp8enc' && mode === 'websockets');
    // WebKit's GStreamer ports hand WebRTC VP8 to libwebrtc's decoder where
    // GStreamer's best is vp8dec and stamp its frames with their render time,
    // zero under the stream's zero playout delay: the player presents the
    // first few and drops the rest as late. Safari stamps them with the
    // capture time.
    const held = engine === 'webkit' && encoder === 'vp8enc' ? "WebKit's GStreamer player drops the VP8 frames libwebrtc decodes as late" : '';
    // The codec under test is the default; the ladder's rungs stay allowed.
    const { url } = await host.start({ label: `${engine}-${encoder}`, mode, wayland, env: { SELKIES_ENCODER: `${encoder},h264enc,jpeg`, ...TURBO } });
    const picture = new Picture(host, wayland, mode === 'webrtc');
    try {
        await picture.paint();
        const [owner, page] = await openPage(url, mode, engine);
        const said: string[] = [];
        page.on('console', (m) => said.push(m.text()));
        try {
            if (mode === 'webrtc') {
                const taken = await rtpProbe(page, rtpMime);
                const video = await waitWrVideo(page);
                res.check(`${tag}: stream up`, Boolean(video), video);
                if (taken && engine === 'firefox' && encoder === 'av1enc') {
                    const version = /Firefox\/(\d+)/.exec(await within(page.evaluate(() => navigator.userAgent)));
                    matrix = version !== null && Number(version[1]) >= 157;
                }
                let line: string;
                if (taken) {
                    res.check(`${tag}: the browser took ${modeName} over RTP`, await host.saw(`negotiated ${rtpMime}`, 10), '');
                    line = await waitStreamMode(host, modeName);
                    res.check(`${tag}: the server streams ${modeName}`, line.includes(`Mode: ${modeName}`), encoderField(line));
                } else {
                    res.check(`${tag}: the browser declined it, so the display moved to h264enc`,
                        (await host.saw('is not decoded by a WebRTC peer', 10)) && (await host.saw('using "h264enc"', 10)), '');
                    line = await waitStreamMode(host, 'H264');
                    res.check(`${tag}: the server streams H.264 instead`, line.includes('Mode: H264'), encoderField(line));
                }
                const sample = await picture.wait(page, 20, matrix);
                res.check(`${tag}: the painted picture decodes`, picture.matches(sample, matrix), sample);
                await picture.keepsPresenting(res, tag, page, taken ? held : '');
                process.stdout.write(`      ${tag}: rtp=${taken ? 'yes' : 'no'} ${encoderField(line)}\n`);
                return;
            }
            let supported = await probe(page, probeCodec);
            const video = await waitWsVideo(page, 30);
            res.check(`${tag}: stream up`, Boolean(video), video);
            const settled = await waitSettledEncoder(page);
            let line = await lastStreamLine(host);
            const refusedAtDecode = said.some((t) => t.includes('refused at decode'));
            if (refusedAtDecode) supported = false;
            if (supported) {
                res.check(`${tag}: the engine decodes it, so the codec is kept`, settled === encoder, `page encoder ${settled}`);
                res.check(`${tag}: the server streams ${modeName}`, line.includes(`Mode: ${modeName}`), encoderField(line));
            } else {
                res.check(`${tag}: the engine refuses it, so the ladder steps to h264enc`, settled === 'h264enc' || settled === 'jpeg', `page encoder ${settled}`);
                // The page settles before the capture restarts behind its request;
                // on Wayland the compositor's restart is the slower of the two.
                line = await waitStreamMode(host, settled === 'h264enc' ? 'H264' : 'JPEG');
                res.check(`${tag}: the server followed the fallback`, line.includes('Mode: H264') || line.includes('Mode: JPEG'), encoderField(line));
            }
            let fps = 0;
            for (let i = 0; i < 20; i++) {
                if (line.includes('Mode: JPEG')) {
                    // JPEG sends only what damage covers, so a still picture presents
                    // no frames: repaint it so the rate measures the stream.
                    await picture.clear();
                    await picture.paint();
                }
                fps = await within(page.evaluate(() => window.fps ?? 0));
                if (fps > 0) break;
                await sleep(500);
            }
            res.check(`${tag}: frames present`, fps > 0, `fps ${fps}`);
            const sample = await picture.wait(page, 20, matrix);
            res.check(`${tag}: the painted picture decodes`, picture.matches(sample, matrix), sample);
            const probed = refusedAtDecode ? 'refused at decode' : (supported ? 'yes' : 'no');
            process.stdout.write(`      ${tag}: probe=${probed} settled=${settled} ${encoderField(line)}\n`);
        } finally {
            await owner.close();
        }
    } finally {
        await picture.clear();
        await host.stop();
    }
}

/** The AV1 picture the page shows is the configured size, edges intact. */
async function blockAv1Crop(host: Host, res: Checks): Promise<void> {
    const node = process.env.E2E_VAAPI_NODE ?? '/dev/dri/renderD128';
    if (!existsSync(node)) {
        res.skip('av1 crop: needs a VA-API AV1 encoder', `${node} does not exist`);
        return;
    }
    const rgb = (argb: string): number => parseInt(argb.slice(2), 16);
    for (const [w, h] of CROP_SIZES) {
        const tag = `chromium av1enc ${w}x${h}`;
        // No fallback rungs: a stream that left AV1 would crop nothing.
        const { url } = await host.start({ label: `av1crop-${w}x${h}`, env: { SELKIES_ENCODER: 'av1enc', DRI_NODE: node } });
        // A bar over the last CROP_BAR rows and another over the last columns,
        // so the edge a padded picture would push out of view is a known color.
        const bars = await host.paint([[0, h - CROP_BAR, w, CROP_BAR, rgb(PAINT_ARGB)], [w - CROP_BAR, 0, CROP_BAR, h, rgb(SATURATED_ARGB)]]);
        try {
            const browser = await launchBrowser('chromium');
            try {
                const ctx = await browser.newContext({ viewport: { width: w, height: h }, deviceScaleFactor: 1 });
                await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
                const page = await ctx.newPage();
                await page.goto(`${url}/`, { waitUntil: 'load' });
                const video = await waitWsVideo(page, 30);
                const line = await waitStreamMode(host, 'AV1');
                if (!video || !line.includes('VAAPI')) {
                    res.skip(`${tag}: needs a VA-API AV1 encoder`, encoderField(line));
                    continue;
                }
                const points = [Math.floor(w / 2), h - 1, w - 1, Math.floor(h / 2), Math.floor(w / 2), Math.floor(h / 2)];
                let got: Sample | null = null;
                for (const until = Date.now() + 20_000; Date.now() < until;) {
                    got = await sampleAt(page, points);
                    if (got && got.w === w && got.h === h) break;
                    await sleep(500);
                }
                res.check(`${tag}: the picture is the configured size`, got?.w === w && got.h === h, `${got?.w}x${got?.h}`);
                res.check(`${tag}: the last rows keep their color`, near(got?.inside, PAINT), got?.inside);
                res.check(`${tag}: the last columns keep their color`, near(got?.outside, SATURATED, SATURATED_TOLERANCE), got?.outside);
                res.check(`${tag}: the middle is still the ground`, near(got?.saturated, [0, 0, 0]), got?.saturated);
            } finally {
                await closeBrowser(browser);
            }
        } finally {
            await bars.release();
            await host.stop();
        }
    }
}

const CELLS = ['ws-x11', 'ws-wl', 'wr-x11'];
const SELECTORS = [...CELLS, ...CELLS.flatMap((c) => ENGINES.map((e) => `${c}-${e}`)), 'ws-x11-av1crop'];

for (const cell of SELECTORS) {
    test(cell, async () => {
        const [transport, backend, which = 'all'] = cell.split('-');
        const mode = transport === 'ws' ? 'websockets' : 'webrtc';
        const wayland = backend === 'wl';
        const res = new Checks(`codecs ${cell}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            if (which === 'av1crop') {
                await blockAv1Crop(host, res);
            } else {
                const engines = which === 'all' ? ENGINES : ENGINES.filter((e) => e === which);
                for (const engine of engines) {
                    for (const codec of CODECS) await blockCodec(host, mode, wayland, engine, codec, res);
                }
                if (mode === 'websockets' && !wayland && engines.includes('chromium')) await blockAv1Crop(host, res);
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
