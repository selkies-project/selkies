/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The poor-connection mark, end to end.
//
// A page whose link carries its stream is told nothing. A page behind a metered
// link slower than its stream is told its connection is poor and shows the
// mark, in each dashboard, and is told it is good again, the mark gone, once
// its stream fits the link: a low rate and a scene without the noise band,
// which a software encoder would not bring down to that rate (WebKit's page
// streams VP8). A still screen behind that link tells it nothing either way.
// Congestion control is off, or it would fit the stream to the link before the
// link could hold any of it back. Over WebRTC the crate's headless client
// (webrtc-peer) crosses a relay narrower than the stream, which the pacer meets
// by abandoning frames, and that client is told its connection is poor over
// its data channel; with the relay unshaped it is told nothing.
//
// The mark has its own setting, ui_show_connection_indicator: it hides the
// mark, and hiding the sidebar does not. On a touch page the mark sits clear of
// the soft keys.
//
// Tests: default, wish, edge (a dashboard each), engines (the default
// dashboard in Firefox and WebKit, whose session socket can run in a worker),
// webrtc, settings.
import { spawn, type ChildProcess } from 'node:child_process';
import { openSync } from 'node:fs';
import { join } from 'node:path';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchBrowser, launchChromium, type Engine } from '../lib/browsers.ts';
import { Checks, finishAll } from '../lib/checks.ts';
import { barTool } from '../lib/dashboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host, suitesBin } from '../lib/host.ts';
import { runSession } from '../lib/peer.ts';
import { closeBrowser, waitWsVideo } from '../lib/stream.ts';
import { fields, isRecord, parseJson } from '../../web/selkies-web-core/lib/narrow.ts';

const WIDTH = 1280;
const HEIGHT = 720;
const LAYOUTS = ['default', 'wish', 'edge'] as const;
const RELAY_PORT = Number(process.env.E2E_CONNECTION_PORT ?? '18213');
// Under the 8 Mbit/s a moving scene streams at, over the 500 kbit/s it is
// turned down to.
const LINK_KBIT = 2000;
const ENV = { SELKIES_CONGESTION_CONTROL: 'false' };
const MARK = '[role=status]';
const WS_MODE = "window.__SELKIES_STREAMING_MODE__ = 'websockets';";

declare global {
    interface Window {
        stream_client?: { connection?: string };
    }
}

function connection(page: Page): Promise<string | null> {
    return page.evaluate(() => window.stream_client?.connection ?? null);
}

async function markShown(page: Page): Promise<boolean> {
    return (await page.locator(MARK, { hasText: 'Poor connection' }).count()) > 0;
}

/** The verdicts the page held, sampled twice a second for `seconds`. */
async function watch(page: Page, seconds: number): Promise<(string | null)[]> {
    const seen: (string | null)[] = [];
    for (const until = performance.now() + seconds * 1000; performance.now() < until;) {
        seen.push(await connection(page));
        await sleep(500);
    }
    return seen;
}

/** Seconds until the page holds `verdict`, or null. */
async function waitFor(page: Page, verdict: string, seconds: number): Promise<number | null> {
    const start = performance.now();
    while (performance.now() - start < seconds * 1000) {
        if (await connection(page) === verdict) return (performance.now() - start) / 1000;
        await sleep(250);
    }
    return null;
}

const distinct = (values: readonly (string | null)[]): string[] => [...new Set(values.map(String))].sort();
const after = (took: number | null): string => (took === null ? 'never' : `after ${took.toFixed(1)} s`);

function painter(display: string, noise?: string): ChildProcess {
    return spawn(suitesBin('motion-scene'), [display, String(WIDTH), String(HEIGHT), '60', ...(noise === undefined ? [] : [noise])], { stdio: 'ignore' });
}

async function wsBlock(host: Host, dashboard: string, engine: Engine = 'chromium'): Promise<Checks> {
    const res = new Checks(`connection-${dashboard}${engine === 'chromium' ? '' : `-${engine}`}`);
    const display = process.env.E2E_DISPLAY ?? ':0';
    const { url, port } = await host.start({
        label: `${dashboard}-${engine}`, mode: 'websockets',
        env: { ...ENV, SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: dashboard },
    });
    let scene: ChildProcess | null = painter(display);
    const relay = spawn(suitesBin('bwrelay'), [String(RELAY_PORT), String(port), String(LINK_KBIT)], {
        stdio: ['ignore', openSync(join(WORKDIR, `bwrelay-${dashboard}-${engine}.log`), 'w'), 'inherit'],
    });
    try {
        const browser = await launchBrowser(engine);
        try {
            const ctx = await browser.newContext({ viewport: { width: WIDTH, height: HEIGHT } });
            await ctx.addInitScript({ content: WS_MODE });
            let page = await ctx.newPage();
            await page.goto(url, { waitUntil: 'load' });
            res.check('a page streams the moving scene', await waitWsVideo(page, 45) !== null);
            const fast = await watch(page, 20);
            res.check('a page whose link carries the stream is never told its connection is poor',
                fast.length > 0 && fast.every((v) => v === 'ok') && !await markShown(page), distinct(fast));
            await page.close();

            page = await ctx.newPage();
            await page.goto(`http://127.0.0.1:${RELAY_PORT}`, { waitUntil: 'load', timeout: 90_000 });
            res.check('a page streams over the metered link', await waitWsVideo(page, 90) !== null);
            let took = await waitFor(page, 'poor', 90);
            res.check('a page behind a link slower than its stream is told its connection is poor', took !== null, after(took));
            res.check('and the dashboard shows the mark', took !== null && await markShown(page));
            scene.kill('SIGKILL');
            scene = painter(display, '0');
            await page.evaluate(() => { window.postMessage({ type: 'settings', settings: { video_bitrate: 500 } }, location.origin); });
            took = await waitFor(page, 'ok', 40);
            res.check('once its stream fits the link it is told the connection is good again', took !== null, after(took));
            res.check('and the mark goes', took !== null && !await markShown(page));
            scene.kill('SIGKILL');
            scene = null;
            await page.evaluate(() => { window.postMessage({ type: 'settings', settings: { video_bitrate: 8000 } }, location.origin); });
            const still = await watch(page, 30);
            res.check('a still screen behind the link is not judged', still.length > 0 && still.every((v) => v === 'ok'), distinct(still));
        } finally {
            await closeBrowser(browser);
        }
    } finally {
        scene?.kill('SIGKILL');
        relay.kill('SIGKILL');
        await host.stop();
    }
    return res;
}

async function webrtcBlock(host: Host): Promise<Checks> {
    const res = new Checks('connection-webrtc');
    const { display } = await host.xvfb(WIDTH, HEIGHT);
    const scene = painter(display);
    try {
        for (const shaped of [false, true]) {
            const { port } = await host.start({
                label: shaped ? 'webrtc-shaped' : 'webrtc', mode: 'webrtc', display, env: {
                    SELKIES_WEBRTC_PACER: 'true', SELKIES_CONGESTION_CONTROL: 'false', SELKIES_STUN_HOST: '',
                    SELKIES_TURN_REST_URI: '', SELKIES_VIDEO_BITRATE: '4000',
                },
            });
            // The verdicts the client was sent over its data channel while the scene moves.
            const told: boolean[] = [];
            try {
                await runSession(`ws://127.0.0.1:${port}/api/ws`, {}, shaped ? { rate_bps: 2.5e6, delay_s: 0.03 } : {}, 25, 10, {
                    onMessage: ({ text }) => {
                        if (!text.includes('"connection"')) return;
                        const message = fields(parseJson(text));
                        if (message.type === 'connection' && isRecord(message.data)) told.push(Boolean(message.data.poor));
                    },
                });
            } finally {
                await host.stop();
            }
            if (shaped) {
                res.check('a client behind a link narrower than its stream is told its connection is poor', told[0] === true, told);
            } else {
                res.check('a client whose link carries the stream is told nothing', told.length === 0, told);
            }
        }
    } finally {
        scene.kill('SIGKILL');
    }
    return res;
}

// A poor verdict as the core records one, for the dashboard to show.
function forcePoor(): void {
    window.stream_client = window.stream_client ?? {};
    window.stream_client.connection = 'poor';
    window.dispatchEvent(new Event('selkies-stream-stats'));
}

// Whether the mark and the soft keys overlap, or null without either.
function overlap(): boolean | null {
    const m = document.querySelector('.connection-indicator');
    const k = document.querySelector('[data-soft-keys]');
    if (!m || !k) return null;
    const a = m.getBoundingClientRect();
    const b = k.getBoundingClientRect();
    return a.right > b.left && a.left < b.right && a.bottom > b.top && a.top < b.bottom;
}

async function settingsBlock(host: Host, dashboard: string): Promise<Checks> {
    const res = new Checks(`connection-settings-${dashboard}`);
    const cases: [string, string, Record<string, string>, boolean][] = [
        ['the setting hides the mark', 'no-indicator', { SELKIES_UI_SHOW_CONNECTION_INDICATOR: 'false' }, false],
        ['hiding the sidebar leaves the mark', 'no-sidebar', { SELKIES_UI_SHOW_SIDEBAR: 'false' }, true],
    ];
    const base = { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: dashboard };
    for (const [label, tag, env, shown] of cases) {
        const { url } = await host.start({ label: `settings-${dashboard}-${tag}`, mode: 'websockets', env: { ...base, ...env } });
        try {
            const browser = await launchChromium();
            try {
                const ctx = await browser.newContext({ viewport: { width: WIDTH, height: HEIGHT } });
                await ctx.addInitScript({ content: WS_MODE });
                const page = await ctx.newPage();
                await page.goto(url, { waitUntil: 'load' });
                await waitWsVideo(page, 45);
                await page.evaluate(forcePoor);
                await sleep(1000);
                const seen = await markShown(page);
                res.check(label, seen === shown, seen);
            } finally {
                await closeBrowser(browser);
            }
        } finally {
            await host.stop();
        }
    }
    if (dashboard !== 'default') {
        const { url } = await host.start({ label: `touch-${dashboard}`, mode: 'websockets', env: base });
        try {
            const browser = await launchChromium();
            try {
                for (const [name, viewport] of [['landscape', { width: 844, height: 390 }], ['portrait', { width: 390, height: 844 }]] as const) {
                    const ctx = await browser.newContext({ viewport, hasTouch: true, isMobile: true });
                    await ctx.addInitScript({ content: WS_MODE });
                    const page = await ctx.newPage();
                    await page.goto(url, { waitUntil: 'load' });
                    await waitWsVideo(page, 45);
                    await page.evaluate(forcePoor);
                    // The soft keys open from the bar's button; shut, there is nothing to overlap.
                    const keys = await barTool(page, '.soft-keys-button');
                    if (keys !== null) await keys.click();
                    await sleep(1000);
                    const overlaps = await page.evaluate(overlap);
                    res.check(`a touch phone (${name}) shows the mark clear of the soft keys`, await markShown(page) && overlaps === false, overlaps);
                    await ctx.close();
                }
            } finally {
                await closeBrowser(browser);
            }
        } finally {
            await host.stop();
        }
    }
    return res;
}

const BLOCKS: Record<string, (host: Host) => Promise<Checks[]>> = {
    ...Object.fromEntries(LAYOUTS.map((layout) => [layout, async (host: Host) => [await wsBlock(host, layout)]])),
    engines: async (host) => [await wsBlock(host, 'default', 'firefox'), await wsBlock(host, 'default', 'webkit')],
    webrtc: async (host) => [await webrtcBlock(host)],
    settings: async (host) => {
        const blocks: Checks[] = [];
        for (const layout of LAYOUTS) blocks.push(await settingsBlock(host, layout));
        return blocks;
    },
};

for (const [name, run] of Object.entries(BLOCKS)) {
    test(name, async () => {
        const host = new Host(suiteOf(import.meta.filename));
        let blocks: Checks[];
        try {
            blocks = await run(host);
        } finally {
            await host.close();
        }
        finishAll(blocks);
    });
}
