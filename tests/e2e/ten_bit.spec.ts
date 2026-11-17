/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// 10-bit is streamed only where the server encodes it and the engine shows it.
//
// video_10bit makes an encoder that carries it emit its codec's 10-bit
// profile (H.265 Main 10, VP9 profile 2, AV1 at 10 bits), and an engine that
// cannot decode that shows no picture rather than a worse one. Engines accept
// 10-bit decoder configurations they then fail to decode, so the client
// settles the question by decoding a 10-bit key frame before it asks the
// server for anything, and this suite holds it to the outcome rather than to
// its own answer: with the setting stored on before the page loads, the
// picture painted on the server has to decode in the page, at 10 bits where
// the client kept the setting and the host encodes them, at 8 bits on the same
// codec everywhere else. Chromium's WebRTC AV1 is one such: libwebrtc's dav1d
// wrapper drops every picture deeper than 8 bits, where WebCodecs decodes
// them, and a stream that opened at 10 bits never showed one.
//
// The host's encoders decide how much a run covers: a codec the encode node
// carries no 10 bits for runs on the build's software encoder where that codes
// them, and at 8 bits where neither does, and the run holds whichever applies.
// A -cpu suffix forces the software encoder. ws-refused and wr-refused drive
// an engine whose decoder gives no 10-bit picture back, whatever this one's
// really does, and ws-default the server's own unlocked default against it.
import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchBrowser, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { fields } from '../../web/selkies-web-core/lib/narrow.ts';
import { Host } from '../lib/host.ts';
import { Picture } from '../lib/picture.ts';
import { closeBrowser, waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const CODECS: Readonly<Record<string, readonly [string, string]>> = {
    av1: ['av1enc', 'AV1'], vp9: ['vp9enc', 'VP9'], h265: ['h265enc', 'H265'], h264: ['h264enc', 'H264'],
};

// A decoder that takes every configuration and gives no frame back for a
// 10-bit one: what Firefox does with VP9 profile 2, on any engine.
const REFUSE_TEN_BIT_JS = String.raw`
(() => {
  if (typeof VideoDecoder === 'undefined') return;
  const tenBit = (cfg) => /^(hev1\.2\.|vp09\.0[23]\.|av01\..*\.10$)/i.test((cfg && cfg.codec) || '');
  const configure = VideoDecoder.prototype.configure;
  VideoDecoder.prototype.configure = function (cfg) {
    this.__tenBit = tenBit(cfg);
    return configure.call(this, cfg);
  };
  const decode = VideoDecoder.prototype.decode;
  VideoDecoder.prototype.decode = function (chunk) {
    if (this.__tenBit) throw new DOMException('Decoding error', 'EncodingError');
    return decode.call(this, chunk);
  };
  const caps = RTCRtpReceiver.getCapabilities.bind(RTCRtpReceiver);
  RTCRtpReceiver.getCapabilities = (kind) => {
    const answer = caps(kind);
    if (!answer || kind !== 'video') return answer;
    return Object.assign({}, answer, {codecs: answer.codecs.filter((c) =>
      !/profile-id=[23]/.test(c.sdpFmtpLine || ''))});
  };
})();
`;

/** Stores the encoder, and 10-bit where asked, before the client's first line runs. */
function initScript(mode: string, encoder: string, stored = true): string {
    return `
window.__SELKIES_STREAMING_MODE__ = '${mode}';
(() => {
  const k = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
  localStorage.setItem(k + '_encoder', '${encoder}');
  ${stored ? "localStorage.setItem(k + '_video_10bit', 'true');" : ''}
})();
`;
}

// The server's settings payload, as the dashboard is handed it:
// encoder_backends says, for each codec and side, whether a session there runs
// at 10 bits.
const SETTINGS_JS = `
window.__serverSettings = null;
window.addEventListener('message', (e) => {
  if (e.data && e.data.type === 'serverSettings') window.__serverSettings = e.data.payload;
});
`;

declare global {
    interface Window {
        video_10bit?: boolean;
    }
}

/** What the page has stored for video_10bit. */
function stored10bit(page: Page): Promise<string | null> {
    return within(page.evaluate(() => {
        const k = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
        return localStorage.getItem(k + '_video_10bit');
    }));
}

/**
 * Whether the server encodes `codec` at 10 bits, 4:2:0, by what it tells the
 * page: on its encode node, or failing that in the build's software encoder,
 * which is where such a session then runs.
 */
async function hostCarries(page: Page, codec: string, software = false): Promise<boolean> {
    let backends: unknown = null;
    for (const until = Date.now() + 20_000; backends === null && Date.now() < until;) {
        backends = await within(page.evaluate((c) => {
            const settings = window.__serverSettings;
            const get = (o: unknown, key: string): unknown => (o !== null && typeof o === 'object' ? Reflect.get(o, key) : undefined);
            return get(get(get(settings, 'encoder_backends'), 'value'), c) ?? null;
        }, codec));
        if (backends === null) await sleep(500);
    }
    const sides = fields(fields(backends).ten_bit);
    const hardware = Boolean(fields(sides.hardware)['420']);
    const soft = Boolean(fields(sides.software)['420']);
    return (!software && hardware) || soft;
}

/** The encoder lines pixelflux printed, oldest first. */
async function encoderLines(host: Host): Promise<string[]> {
    return (await host.log()).split('\n').filter((l) => l.includes('] Encoder: '));
}

/** The newest encoder line once it names `bits`, else the newest one seen. */
async function waitDepth(host: Host, bits: number, timeoutS = 20): Promise<string> {
    for (const until = Date.now() + timeoutS * 1000; ;) {
        const lines = await encoderLines(host);
        const last = lines.at(-1);
        if (last?.includes(`${bits}-bit`)) return last;
        if (Date.now() >= until) return last ?? '';
        await sleep(500);
    }
}

interface Drive {
    engine: Engine;
    mode: string;
    codec: string;
    refused: boolean;
    stored: boolean;
    cpu: boolean;
}

/**
 * One engine on one codec: the painted picture decodes, at the depth the
 * client settled on and the host encodes.
 */
async function drive(res: Checks, url: string, host: Host, picture: Picture, d: Drive): Promise<void> {
    const [encoder, modeName] = CODECS[d.codec];
    const tag = `${d.engine}-${d.mode}-${d.codec}${d.cpu ? '-cpu' : ''}${d.refused ? '-refused' : ''}${d.stored ? '' : '-default'}`;
    const browser = await launchBrowser(d.engine);
    try {
        const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: SETTINGS_JS + initScript(d.mode, encoder, d.stored) + (d.refused ? REFUSE_TEN_BIT_JS : '') });
        const page = await ctx.newPage();
        const said: string[] = [];
        page.on('console', (m) => said.push(m.text()));
        await page.goto(`${url}${d.refused && d.mode === 'websockets' ? '/?offscreen_worker=false' : '/'}`, { waitUntil: 'load' });
        const video = d.mode === 'webrtc' ? await waitWrVideo(page, 45) : await waitWsVideo(page, 45);
        res.check(`[${tag}] the stream plays with 10-bit asked for`, Boolean(video), said.slice(-3));
        const carried = await hostCarries(page, d.codec, d.cpu);
        let encoderNow: string | null = null;
        for (let since = Date.now(); Date.now() - since < 4000;) {
            const now = await within(page.evaluate(() => window.encoder ?? null));
            if (now !== encoderNow) {
                encoderNow = now;
                since = Date.now();
            }
            await sleep(500);
        }
        if (encoderNow !== encoder) {
            // The engine does not decode the codec at all, which is the codec
            // ladder's to answer and says nothing of 10 bits.
            res.check(`[${tag}] the engine does not play ${d.codec}; the ladder moved to a codec it does`,
                picture.matches(await picture.wait(page)), encoderNow);
            return;
        }
        const kept = d.mode === 'websockets'
            ? (await stored10bit(page)) !== 'false'
            : Boolean(await within(page.evaluate(() => window.video_10bit)));
        if (d.refused) res.check(`[${tag}] an engine that shows no 10-bit picture has it turned off`, !kept, said.slice(-3));
        const want = kept && carried ? 10 : 8;
        const line = await waitDepth(host, want);
        res.check(`[${tag}] the server streams ${want}-bit ${modeName} (host carries 10-bit: ${carried ? 'True' : 'False'}, kept: ${kept ? 'True' : 'False'})`,
            (line.includes(`${want}-bit`) && line.includes(modeName)) || (!carried && !line.includes('10-bit')), line.slice(-120));
        const sample = await picture.wait(page);
        res.check(`[${tag}] the painted picture decodes`, picture.matches(sample), sample);
        const off = said.filter((t) => t.includes('10-bit is off'));
        if (d.mode === 'websockets') res.check(`[${tag}] turning it off is said once, and only then`, off.length === (kept ? 0 : 1), off.length ? off : kept);
    } finally {
        await closeBrowser(browser);
    }
}

// One engine on one transport and codec per run, as the browser matrix is driven.
const SELECTORS = ['ws-chromium-av1', 'wr-chromium-av1', 'ws-chromium-vp9', 'wr-chromium-vp9',
    'ws-firefox-vp9', 'ws-firefox-av1', 'ws-refused-av1', 'wr-refused-vp9',
    'ws-default-av1', 'ws-chromium-av1-cpu', 'wr-chromium-av1-cpu',
    'ws-chromium-vp9-cpu', 'ws-chromium-h264', 'wr-chromium-h264'];

for (const selector of SELECTORS) {
    test(selector, async () => {
        const parts = selector.split('-');
        const mode = parts[0] === 'wr' ? 'webrtc' : 'websockets';
        const which = parts[1];
        const codec = parts[2] ?? 'av1';
        const cpu = parts.at(-1) === 'cpu';
        const res = new Checks(`ten-bit-${selector}`);
        const refused = which === 'refused' || which === 'default';
        const env: Record<string, string> = { SELKIES_ENCODER: `${CODECS[codec][0]},h264enc,jpeg` };
        if (cpu) env.SELKIES_USE_CPU = 'true';
        if (which === 'default') env.SELKIES_VIDEO_10BIT = 'true';
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const { url } = await host.start({ label: selector, mode, env });
            const picture = new Picture(host, false);
            try {
                await picture.paint();
                const engine: Engine = refused ? 'chromium' : (which === 'firefox' ? 'firefox' : which === 'webkit' ? 'webkit' : 'chromium');
                await drive(res, url, host, picture, { engine, mode, codec, refused, stored: which !== 'default', cpu });
            } finally {
                await picture.clear();
                await host.stop();
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
