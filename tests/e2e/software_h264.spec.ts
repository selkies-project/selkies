/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Software H.264 streams with whichever encoder the server's pixelflux was
// built with: libx264 (the default build) or Cisco OpenH264 (a
// PIXELFLUX_ENABLE_GPL=0 build). Selkies never chooses between them -- it
// takes the one its pixelflux was built with -- so the same session has to
// come up, name that encoder in the server log, and decode in a real browser
// on either build: h264enc forced onto the CPU, on X11 and on Wayland.
//
// SELKIES_TEST_SERVER names the server whose pixelflux build is under test;
// its --version line names the encoder to expect.
import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { serverSoftwareEncoder } from '../lib/encode.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWsVideo, within } from '../lib/stream.ts';

// Counts decoded VideoFrames and records every decoder configuration on the
// page, which is where a full-frame session decodes, beside what the client
// publishes -- the wire counter and which sink element is on screen.
const INSTRUMENT_JS = `
  window.__decoded = 0;
  window.__cfgs = [];
  window.__decodeErrors = [];
  (() => {
    const Real = window.VideoDecoder;
    if (!Real) return;
    const proto = Real.prototype;
    const origConfigure = proto.configure;
    proto.configure = function (cfg) {
      window.__cfgs.push({w: cfg && cfg.codedWidth, h: cfg && cfg.codedHeight});
      return origConfigure.call(this, cfg);
    };
    window.VideoDecoder = new Proxy(Real, {
      construct(target, args) {
        const init = Object.assign({}, (args && args[0]) || {});
        const out = init.output;
        const err = init.error;
        init.output = (frame) => { window.__decoded++; if (out) out(frame); };
        init.error = (e) => { window.__decodeErrors.push(String(e && e.message || e)); if (err) err(e); };
        return Reflect.construct(target, [init]);
      },
    });
  })();
`;

declare global {
    interface Window {
        __decoded?: number;
        __cfgs?: { w?: number; h?: number }[];
        __decodeErrors?: string[];
        videoChunksReceived?: number;
    }
}

interface State {
    decoded: number;
    cfgs: { w?: number; h?: number }[];
    errors: string[];
    chunks: number;
    sinks: string[];
}

/** Page decode counters plus what the client publishes: the wire chunk count and the sinks on screen. */
function readState(page: Page): Promise<State> {
    return within(page.evaluate((): State => ({
        decoded: window.__decoded ?? 0,
        cfgs: window.__cfgs ?? [],
        errors: window.__decodeErrors ?? [],
        chunks: window.videoChunksReceived ?? 0,
        sinks: ['videoCanvas', 'videoWorkerCanvas', 'videoStream'].filter((id) => {
            const el = document.getElementById(id);
            return el !== null && getComputedStyle(el).display !== 'none';
        }),
    })));
}

/**
 * The sinks on screen once one is left; a second shows during warm-up, while
 * the canvas still covers a sink that has yet to render.
 */
async function waitSinks(page: Page, timeoutS = 25): Promise<string[]> {
    const until = Date.now() + timeoutS * 1000;
    let sinks = (await readState(page)).sinks;
    while (Date.now() < until && sinks.length !== 1) {
        await sleep(500);
        sinks = (await readState(page)).sinks;
    }
    return sinks;
}

/** Poll the page state until `predicate` holds or time runs out; the last state. */
async function waitState(page: Page, predicate: (s: State) => boolean, timeoutS = 30): Promise<State> {
    const until = Date.now() + timeoutS * 1000;
    let state = await readState(page);
    while (Date.now() < until && !predicate(state)) {
        await sleep(500);
        state = await readState(page);
    }
    return state;
}

/**
 * Stream software H.264 on one backend and check that the build's encoder is
 * named in the logs and decodes in the browser. The server runs Turbo, so the
 * still test display keeps sending the frames counted.
 */
async function runBlock(r: Checks, host: Host, wayland: boolean): Promise<void> {
    const encoder = serverSoftwareEncoder(await host.serverBinary());
    r.check('server pixelflux names its software H.264 encoder', encoder === 'x264' || encoder === 'openh264', encoder);
    const { url } = await host.start({ label: wayland ? 'wl' : 'x11', mode: 'websockets', wayland, env: { SELKIES_USE_CPU: 'true', SELKIES_VIDEO_STREAMING_MODE: 'true' } });
    try {
        const browser = await launchChromium();
        try {
            const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
            await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
            await ctx.addInitScript({ content: INSTRUMENT_JS });
            const page = await ctx.newPage();
            await page.goto(`${url}/`, { waitUntil: 'load' });
            const info = await waitWsVideo(page, 30);
            r.check('h264enc: canvas painted', info !== null, info);
            const start = await readState(page);
            const state = await waitState(page, (s) => s.chunks >= start.chunks + 24 && s.decoded >= 12);
            r.check('h264enc: frames flow', state.chunks >= start.chunks + 24, [start.chunks, state.chunks]);
            r.check('h264enc: frames decode', state.decoded >= 12, state.decoded);
            r.check('h264enc: no decoder errors', state.errors.length === 0, state.errors.slice(0, 3));
            const heights = [...new Set(state.cfgs.map((c) => c.h).filter((h): h is number => Boolean(h)))].sort((a, b) => a - b);
            r.check('h264enc: one full-frame decoder geometry', heights.length === 1, heights);
            const fullSinks = await waitSinks(page);
            r.check('h264enc: one video sink on screen', fullSinks.length === 1, fullSinks);
            const log = await host.log();
            r.check('server names the software encoder for the session', log.includes(`encodes H.264 in software (${encoder})`), encoder);
            const streamLine = wayland ? `Mode: H264 (${encoder})` : `Encoder: CPU (${encoder})`;
            r.check("pixelflux's stream-settings line names the software encoder", log.includes(streamLine), streamLine);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
}

for (const name of ['x11', 'wl']) {
    test(name, async () => {
        process.stdout.write(`=== ${name} ===\n`);
        const res = new Checks(`software-h264-${name}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await runBlock(res, host, name === 'wl');
        } catch (e) {
            res.check('block completed', false, String(e));
        } finally {
            await host.close();
        }
        res.finish();
    });
}
