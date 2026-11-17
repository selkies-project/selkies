/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Touch input end to end: a browser client on a touch screen drives the
// server, and what the session receives is read back from the X server or the
// Wayland seat.
//
// Trackpad mode has to click as a tap lifts rather than when the double-tap
// window closes, keep a quick double tap two clicks, scroll by the distance
// two fingers travel whatever rate the digitizer reports at, hand a
// two-finger gesture down to the finger left on the glass, right-click on a
// two-finger tap and on a finger held still and lifted, middle-click on three
// fingers, and send a pinch as Ctrl+wheel; direct touch scrolls and pinches
// the same way. Chromium is driven through CDP multi-touch, which enters the
// engine's own touch pipeline. Firefox and WebKit take synthetic TouchEvents
// dispatched on the stream overlay, which exercise the client's handlers in
// those engines but not their touch pipelines.
//
// On Wayland the pinch is also a check of the server: keys reach the seat
// through the keyboard worker and wheel clicks straight from the message, so
// the Control a pinch is wrapped in has to be down before its click arrives.
//
// Trackpad mode draws the cursor on the page, where the server echoes the
// pointer, with nothing composited into the video: the drawn cursor has to
// sit where the session's pointer is after the finger moves it, and follow a
// warp no page sent.
import type { BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { firefoxPersistentContext, launchBrowser, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, monotonic, type Observer } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';
import { FIREFOX_TOUCH_PREFS, Fingers, XWatcher } from '../lib/touch.ts';

// Where the page draws trackpad mode's cursor, in the stream's pixels, and
// whether it had the cursor composited instead; null while none is drawn.
const PAGE_CURSOR_JS = String.raw`(() => {
  const input = window.webrtcInput;
  if (!input || input.cursorDiv.style.display !== 'block') return null;
  const m = /translate\(([-\d.]+)px, ([-\d.]+)px\)/.exec(input.cursorDiv.style.transform || '');
  const box = input._streamBox();
  if (!m || !box) return null;
  const x = Number(m[1]) + input.cursorHotspot.x, y = Number(m[2]) + input.cursorHotspot.y;
  return [(x - box.left) * box.scaleX, (y - box.top) * box.scaleY, !!input._echoComposited];
})()`;

type Drawn = [number, number, boolean] | null;

const near = (drawn: Drawn, at: readonly [number, number] | null, slack = 2): boolean =>
    drawn !== null && at !== null && Math.abs(drawn[0] - at[0]) <= slack && Math.abs(drawn[1] - at[1]) <= slack;

async function drawnCursor(page: Page): Promise<Drawn> {
    const got = await within(page.evaluate(PAGE_CURSOR_JS));
    return Array.isArray(got) && got.length === 3 ? [Number(got[0]), Number(got[1]), Boolean(got[2])] : null;
}

/** One finger drawn slowly down and to the right across the trackpad. */
async function trackpadStroke(f: Fingers, x = 300, y = 300): Promise<void> {
    await f.down([1, x, y]);
    for (let i = 1; i <= 10; i++) {
        await sleep(16);
        await f.move([1, x + 8 * i, y + 4 * i]);
    }
    await f.up(1);
}

async function setMode(page: Page, trackpad: boolean): Promise<void> {
    const kind = trackpad ? 'touchinput:trackpad' : 'touchinput:touch';
    await within(page.evaluate(`window.postMessage({type: '${kind}'}, window.location.origin)`));
    await sleep(400);
}

async function twoFingerSwipe(f: Fingers, dy: number, steps: number, x = 600, y = 500, gap = 8): Promise<void> {
    await f.down([1, x, y]);
    await f.down([2, x + 100, y]);
    for (let i = 1; i <= steps; i++) {
        await sleep(gap);
        await f.move([1, x, y + dy * i / steps], [2, x + 100, y + dy * i / steps]);
    }
    await sleep(gap);
    await f.up(1, 2);
}

async function pinch(f: Fingers, start: number, end: number, steps = 20): Promise<void> {
    const [cx, cy] = [640, 400];
    await f.down([1, cx - start / 2, cy]);
    await f.down([2, cx + start / 2, cy]);
    for (let i = 1; i <= steps; i++) {
        await sleep(16);
        const d = start + (end - start) * i / steps;
        await f.move([1, cx - d / 2, cy], [2, cx + d / 2, cy]);
    }
    await sleep(16);
    await f.up(1, 2);
}

const waitVideo = (page: Page, mode: string): Promise<unknown> => (mode === 'websockets' ? waitWsVideo(page, 40) : waitWrVideo(page, 60));
const round = (v: number | null, digits = 0): number | null => (v === null ? null : Number(v.toFixed(digits)));
const r = (v: unknown): string => JSON.stringify(v);

async function open(engine: Engine, url: string, mode: string): Promise<{ ctx: BrowserContext; page: Page; close: () => Promise<void> }> {
    const viewport = { width: 1280, height: 720 };
    let ctx: BrowserContext;
    let close: () => Promise<void>;
    if (engine === 'firefox') {
        ctx = await firefoxPersistentContext({ viewport, hasTouch: true, prefs: FIREFOX_TOUCH_PREFS });
        close = () => ctx.close();
    } else {
        const browser = await launchBrowser(engine);
        ctx = await browser.newContext({ viewport, deviceScaleFactor: 1, hasTouch: true });
        close = () => browser.close();
    }
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    return { ctx, page, close };
}

async function x11Block(host: Host, res: Checks, engine: Engine, mode: string): Promise<void> {
    const tag = `${engine} ${mode}`;
    const { url } = await host.start({ label: `${engine}-${mode}`, mode });
    let watcher: XWatcher | null = null;
    let opened: Awaited<ReturnType<typeof open>> | null = null;
    try {
        opened = await open(engine, url, mode);
        const { page, ctx } = opened;
        const video = await waitVideo(page, mode);
        res.check(`${tag}: video up`, video !== null, video);
        if (video === null) return;
        const hasTouch = await within(page.evaluate("'ontouchstart' in window && typeof TouchEvent === 'function'"));
        if (hasTouch !== true) {
            res.check(`${tag}: the engine has touch events`, false, 'no ontouchstart/TouchEvent');
            return;
        }
        await sleep(1000);
        watcher = await XWatcher.start(host);
        // Trackpad motion is relative: from wherever a resize left the pointer
        // (a corner, after the screen shrank to the page), a move toward that
        // edge goes nowhere.
        await host.warp(640, 360);
        const f = await Fingers.on(page, ctx, engine);

        // --- trackpad mode ---------------------------------------------
        await setMode(page, true);
        await f.down([1, 500, 400]);
        await sleep(80);
        const tLift = monotonic();
        await f.up(1);
        await sleep(600);
        const press = await watcher.since(tLift, 'ButtonPress', 1);
        const release = await watcher.since(tLift, 'ButtonRelease', 1);
        const lag = press.length ? (press[0].at - tLift) * 1000 : null;
        res.check(`${tag}: a trackpad tap clicks as the finger lifts`, press.length === 1 && lag !== null && lag < 100, `press ${round(lag, 1)} ms`);
        res.check(`${tag}: and releases the click`, press.length ? release.length === 1 && release[0].at > press[0].at : false, `${release.length} releases`);

        let t0 = monotonic();
        for (let i = 0; i < 2; i++) {
            await f.down([1, 500, 400]);
            await sleep(60);
            await f.up(1);
            await sleep(90);
        }
        await sleep(600);
        const clicks = await watcher.since(t0, 'ButtonPress', 1);
        res.check(`${tag}: a quick double tap is two clicks`, clicks.length === 2, `${clicks.length} presses`);

        const notches = async (): Promise<number[]> => {
            const counts: number[] = [];
            for (const steps of [10, 50]) {
                const t = monotonic();
                await twoFingerSwipe(f, -200, steps);
                await sleep(600);
                counts.push((await watcher?.since(t, 'ButtonPress', 5))?.length ?? 0);
            }
            return counts;
        };
        let counts = await notches();
        res.check(`${tag}: a 200 px two-finger scroll scrolls as far in 10 events as in 50`, Math.min(...counts) >= 3 && Math.max(...counts) - Math.min(...counts) <= 1, `notches ${r(counts)}`);

        await f.down([1, 400, 500]);
        await f.down([2, 500, 500]);
        for (let i = 1; i <= 10; i++) {
            await sleep(16);
            await f.move([1, 400, 500 - 10 * i], [2, 500, 500 - 10 * i]);
        }
        await f.up(2);
        await sleep(300);
        t0 = monotonic();
        for (let i = 1; i <= 10; i++) {
            await sleep(16);
            await f.move([1, 400 + 10 * i, 400]);
        }
        await sleep(400);
        const motion = await watcher.since(t0, 'MotionNotify');
        const moved = motion.length >= 2 ? motion[motion.length - 1].rootX - motion[0].rootX : 0;
        await f.up(1);
        res.check(`${tag}: the finger left after a two-finger scroll moves the pointer`, motion.length >= 5 && moved >= 60, `${motion.length} motions, ${moved} px`);

        t0 = monotonic();
        await f.down([1, 500, 400]);
        await f.down([2, 560, 400]);
        await f.down([3, 620, 400]);
        await sleep(60);
        await f.up(1, 2, 3);
        await sleep(500);
        const middle = await watcher.since(t0, 'ButtonPress', 2);
        res.check(`${tag}: a three-finger tap is a middle click`, middle.length === 1, `${middle.length} presses`);

        t0 = monotonic();
        await f.down([1, 500, 400]);
        await f.down([2, 560, 400]);
        await sleep(60);
        await f.up(1, 2);
        await sleep(500);
        let right = await watcher.since(t0, 'ButtonPress', 3);
        let left = await watcher.since(t0, 'ButtonPress', 1);
        res.check(`${tag}: a two-finger tap is a right click`, right.length === 1 && left.length === 0, `${right.length} right, ${left.length} left presses`);

        t0 = monotonic();
        await f.down([1, 500, 400]);
        await sleep(1000);
        const held = await watcher.since(t0, 'ButtonPress');
        await f.up(1);
        await sleep(500);
        right = await watcher.since(t0, 'ButtonPress', 3);
        left = await watcher.since(t0, 'ButtonPress', 1);
        res.check(`${tag}: a finger held still and lifted is a right click, as it lifts`, held.length === 0 && right.length === 1 && left.length === 0,
            `${held.length} presses while held, then ${right.length} right, ${left.length} left`);

        t0 = monotonic();
        await pinch(f, 100, 200);
        await sleep(600);
        let ev = (await watcher.since(t0)).filter((e) => e.kind !== 'MotionNotify');
        let zoom = ev.filter((e) => e.kind === 'ButtonPress' && e.detail === 4);
        const control = watcher.control;
        const inside = zoom.every((z) => ev.some((k) => k.kind === 'KeyPress' && k.detail === control && k.at <= z.at));
        res.check(`${tag}: a trackpad pinch out is Ctrl+wheel up`, zoom.length >= 4 && zoom.length <= 6 && inside, `${zoom.length} notches, control held ${inside}`);

        await trackpadStroke(f);
        await sleep(400);
        const drawn = await drawnCursor(page);
        const p = await host.pointer();
        res.check(`${tag}: trackpad mode draws the cursor on the page, where the pointer is`, near(drawn, p), `page ${r(drawn)}, server ${r(p)}`);
        res.check(`${tag}: with nothing composited into the video`, drawn !== null && !drawn[2], drawn);
        await host.warp(320, 200);
        t0 = monotonic();
        let warped: Drawn = null;
        while (monotonic() - t0 < 1.5 && !near(warped, [320, 200], 1)) {
            await sleep(10);
            warped = await drawnCursor(page);
        }
        res.check(`${tag}: a warp no page sent moves it too`, near(warped, [320, 200], 1), `page ${r(warped)} after ${Math.round((monotonic() - t0) * 1000)} ms`);

        // --- direct touch ----------------------------------------------
        await setMode(page, false);
        counts = await notches();
        res.check(`${tag}: direct touch scrolls a 200 px swipe as far in 10 events as in 50`, Math.min(...counts) >= 3 && Math.max(...counts) - Math.min(...counts) <= 1, `notches ${r(counts)}`);
        t0 = monotonic();
        await pinch(f, 300, 150);
        await sleep(600);
        ev = (await watcher.since(t0)).filter((e) => e.kind !== 'MotionNotify');
        zoom = ev.filter((e) => e.kind === 'ButtonPress' && e.detail === 5);
        const controls = ev.filter((e) => e.kind === 'KeyPress' && e.detail === control);
        res.check(`${tag}: a direct-touch pinch in is Ctrl+wheel down`, zoom.length >= 4 && zoom.length <= 6 && controls.length > 0 && controls[0].at <= zoom[0].at,
            `${zoom.length} notches, ${controls.length} Control presses`);
    } finally {
        await watcher?.close();
        await opened?.close();
        await host.stop();
    }
}

/** The pinch's Control is down on the Wayland seat before its wheel click. */
async function waylandBlock(host: Host, res: Checks, mode: string): Promise<void> {
    const tag = `wayland ${mode}`;
    const { url } = await host.start({ label: `wl-${mode}`, mode, wayland: true });
    let obs: Observer | null = null;
    let opened: Awaited<ReturnType<typeof open>> | null = null;
    try {
        opened = await open('chromium', url, mode);
        const { page, ctx } = opened;
        const video = await waitVideo(page, mode);
        res.check(`${tag}: video up`, video !== null, video);
        if (video === null) return;
        obs = await host.observe('wayland-1');
        res.check(`${tag}: observer mapped`, await obs.ready());
        const f = await Fingers.on(page, ctx, 'chromium');
        await setMode(page, true);
        // A move and a tap, to give the observer the pointer and keyboard focus.
        await f.down([1, 600, 360]);
        for (let i = 1; i <= 5; i++) {
            await sleep(16);
            await f.move([1, 600 + 10 * i, 360]);
        }
        await f.up(1);
        await sleep(400);
        await f.down([1, 640, 360]);
        await sleep(60);
        await f.up(1);
        await sleep(1000);
        let mark = (await obs.lines()).length;
        await pinch(f, 100, 200);
        await sleep(1000);
        const ev = (await obs.lines(mark)).filter((l) => l.kind === 'kbd_key' || l.kind === 'ptr_axis');
        const firstAxis = ev.findIndex((l) => l.kind === 'ptr_axis');
        const ctrlDown = ev.findIndex((l) => l.kind === 'kbd_key' && l.state === 1);
        res.check(`${tag}: a pinch reaches the seat as wheel clicks`, firstAxis >= 0, ev.slice(0, 6));
        res.check(`${tag}: with Control down before the first of them`, ctrlDown >= 0 && firstAxis >= 0 && ctrlDown < firstAxis, ev.slice(0, 6));
        await f.down([1, 500, 400]);
        await sleep(60);
        mark = (await obs.lines()).length;
        const tLift = monotonic();
        await f.up(1);
        let lag: number | null = null;
        while (monotonic() - tLift < 3) {
            if ((await obs.lines(mark)).some((l) => l.kind === 'ptr_button' && l.state === 1)) {
                lag = (monotonic() - tLift) * 1000;
                break;
            }
            await sleep(2);
        }
        res.check(`${tag}: a trackpad tap clicks on the seat as the finger lifts`, lag !== null && lag < 100, `press seen after ${round(lag)} ms`);
        await trackpadStroke(f);
        await sleep(400);
        const drawn = await drawnCursor(page);
        const seat = (await obs.lines()).filter((l) => l.kind === 'ptr_motion');
        const last = seat.at(-1);
        const at: [number, number] | null = last ? [Number(last.x), Number(last.y)] : null;
        res.check(`${tag}: trackpad mode draws the cursor on the page, where the seat's pointer is`, near(drawn, at), `page ${r(drawn)}, seat ${r(at)}`);
        res.check(`${tag}: with nothing composited into the video`, drawn !== null && !drawn[2], drawn);
    } finally {
        await obs?.release();
        await opened?.close();
        await host.stop();
    }
}

for (const which of ['x11', 'wl']) {
    test(which, async () => {
        const res = new Checks(`touch-${which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            for (const mode of ['websockets', 'webrtc']) {
                if (which === 'x11') {
                    for (const engine of ['chromium', 'firefox', 'webkit'] as const) await x11Block(host, res, engine, mode);
                } else {
                    await waylandBlock(host, res, mode);
                }
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
