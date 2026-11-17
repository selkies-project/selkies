/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What a page says about encoders its browser cannot play.
//
// Each dashboard layout's encoder menu lists every encoder the server allows
// and disables the ones this browser cannot play on the transport, each marked
// as unsupported by the browser, so a missing option reads as the browser's
// limit and never as a server feature left out. A stream the browser cannot
// play at all, from an encoder the server holds, is said on the page for the
// owner and a viewer alike: the WebSocket core learns it from the stream, the
// WebRTC core is told by the server once the browser's answer left the codec
// out. Chromium on Linux plays no H.265, which is the case driven here.
import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { openVideoSettings } from '../lib/dashboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within, type VideoSize } from '../lib/stream.ts';

const LAYOUTS = ['default', 'wish', 'edge'] as const;
const MODES = ['websockets', 'webrtc'] as const;
const UNSUPPORTED = 'Unsupported Browser';
const NOTICE = 'H.265 video, which this browser cannot decode';
// The key-frame requests the page sends through its transport (window.selkiesTransport).
const KEYFRAME_TAP_JS = `
  window.__keyframeAsks = 0;
  (() => {
    let transport = null;
    Object.defineProperty(window, 'selkiesTransport', {
      configurable: true,
      get: () => transport,
      set: (v) => {
        transport = v;
        if (v && v.send) {
          const send = v.send.bind(v);
          v.send = (m) => { if (m === 'REQUEST_KEYFRAME') window.__keyframeAsks++; return send(m); };
        }
      },
    });
  })();
`;
const H265 = 'H.265';
const H264 = 'H.264';

declare global {
    interface Window {
        __keyframeAsks?: number;
    }
}

function waitVideo(page: Page, mode: string): Promise<VideoSize | null> {
    return mode === 'webrtc' ? waitWrVideo(page, 45) : waitWsVideo(page, 30);
}

/**
 * The encoder menu's options as [text, disabled], the video settings opened
 * in `layout` first; null when the select never renders.
 */
async function encoderMenu(page: Page, layout: string): Promise<[string, boolean][] | null> {
    if (!(await openVideoSettings(page, layout, '#encoderSelect'))) return null;
    for (const until = Date.now() + 10_000; Date.now() < until;) {
        const options = await within(page.evaluate(() => [...document.querySelectorAll('#encoderSelect option')]
            .map((o): [string, boolean] => [o.textContent, o instanceof HTMLOptionElement && o.disabled])));
        if (options.length) return options;
        await sleep(300);
    }
    return null;
}

async function menuBlock(host: Host, dashboard: string, mode: string): Promise<Checks> {
    const res = new Checks(`encoder-menu-${dashboard}-${mode}`);
    const { url } = await host.start({ label: `menu-${dashboard}-${mode}`, mode, env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: dashboard } });
    try {
        const browser = await launchChromium();
        try {
            const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
            await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
            const page = await ctx.newPage();
            await page.goto(url, { waitUntil: 'load' });
            res.check('video streams', (await waitVideo(page, mode)) !== null);
            const entries = await encoderMenu(page, dashboard);
            const byLabel = new Map<string, [string, boolean]>();
            for (const [text, disabled] of entries ?? []) {
                byLabel.set(text.replace(UNSUPPORTED, '').trim().replace(/^[()[\]]+|[()[\]]+$/g, '').trim(), [text, disabled]);
            }
            // Every full-frame codec, and JPEG over WebSockets only.
            res.check('every encoder the server allows is listed', entries !== null && entries.length === (mode === 'webrtc' ? 5 : 6), entries);
            const h265 = byLabel.get(H265);
            res.check('H.265, which this browser does not play, is listed disabled and marked as unsupported here',
                h265 !== undefined && h265[1] && h265[0].includes(UNSUPPORTED), h265 ?? null);
            const h264 = byLabel.get(H264);
            res.check('H.264 is listed, enabled, and unmarked', h264 !== undefined && !h264[1] && !h264[0].includes(UNSUPPORTED), h264 ?? null);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

/** The status bar's text once it shows the undecodable-stream notice, else whatever it last showed. */
async function waitNotice(page: Page, timeoutS = 40): Promise<string | null> {
    let shown: string | null = null;
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        shown = await within(page.evaluate(() => {
            const e = document.getElementById('status-display');
            return e && !e.classList.contains('hidden') ? e.textContent : null;
        }));
        if (shown?.includes(NOTICE)) return shown;
        await sleep(500);
    }
    return shown;
}

/**
 * The server holding H.265 for browsers that play none: the owner's page and a
 * viewer's both say so instead of staying black and quiet, and then ask for no
 * key frame, which nothing they could play would answer.
 */
async function pinnedBlock(host: Host, mode: string): Promise<Checks> {
    const res = new Checks(`encoder-pinned-${mode}`);
    const { url } = await host.start({ label: `pinned-${mode}`, mode, env: { SELKIES_ENCODER: 'h265enc' } });
    const browser = await launchChromium();
    try {
        for (const [role, hash] of [['owner', ''], ['viewer', '#shared']] as const) {
            const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
            try {
                await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
                await ctx.addInitScript({ content: KEYFRAME_TAP_JS });
                const page = await ctx.newPage();
                const said: string[] = [];
                page.on('console', (m) => said.push(m.text()));
                await page.goto(`${url}/${hash}`, { waitUntil: 'load' });
                const shown = await waitNotice(page);
                res.check(`${role}: the page says the session streams H.265 video it cannot decode`, shown?.includes(NOTICE), shown ?? said.slice(-3));
                if (mode === 'websockets') {
                    const before = await within(page.evaluate(() => window.__keyframeAsks ?? 0));
                    await sleep(5000);
                    const asks = (await within(page.evaluate(() => window.__keyframeAsks ?? 0))) - before;
                    res.check(`${role}: then it asks for no key frame`, asks === 0, `${asks} in 5 s`);
                }
            } finally {
                await ctx.close();
            }
        }
    } finally {
        await browser.close();
        await host.stop();
    }
    return res;
}

for (const which of ['menu', 'pinned']) {
    test(which, async () => {
        const host = new Host(suiteOf(import.meta.filename));
        const failures: string[] = [];
        // Each block prints its count as it ends, as it would on its own.
        const ran = (res: Checks): void => {
            try {
                res.finish();
            } catch (e) {
                failures.push(String(e));
            }
        };
        try {
            if (which === 'menu') {
                for (const dashboard of LAYOUTS) {
                    for (const mode of MODES) ran(await menuBlock(host, dashboard, mode));
                }
            } else {
                for (const mode of MODES) ran(await pinnedBlock(host, mode));
            }
        } finally {
            await host.close();
        }
        if (failures.length) throw new Error(failures.join('\n'));
    });
}
