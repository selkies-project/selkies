/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A second screen comes up, and comes up in its own place, on every
// transport. The matrix drives a #display2 page far enough to see the config
// update and a video element; what it cannot see is where the server put that
// display. The secondary's offset is computed from the primary's size, and
// when the two disagree the compositor rejects the output as overlapping and
// the second screen never appears at all, so each combination here asserts
// the applied geometry. Pixel content is proven without a browser by
// crates/selkies-suites/tests/two_display_pixels.rs.
import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { desktopWindow, onPath, startOnDisplay, type Started } from '../lib/desktop.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { launchClient, newPage } from '../lib/page.ts';
import { realConsole, waitWrVideo, waitWsVideo, type VideoSize } from '../lib/stream.ts';

/** A display's rectangle on the desktop. */
interface Rect {
    x: number;
    y: number;
    w: number;
    h: number;
}

/**
 * A rectangle from the server's log, a Python-style literal
 * (`{'x': 0, 'y': 0, 'w': 1280, 'h': 720}`) whose quotes JSON takes double.
 * @param literal The literal.
 * @returns What it holds, or null.
 */
function parseLiteral(literal: string): unknown {
    try {
        return JSON.parse(literal.trim().replaceAll("'", '"'));
    } catch {
        return null;
    }
}

/**
 * A value as a rectangle, when it is one.
 * @param value Anything.
 * @returns The rectangle, or null.
 */
function rect(value: unknown): Rect | null {
    if (typeof value !== 'object' || value === null) return null;
    const x: unknown = Reflect.get(value, 'x');
    const y: unknown = Reflect.get(value, 'y');
    const w: unknown = Reflect.get(value, 'w');
    const h: unknown = Reflect.get(value, 'h');
    return typeof x === 'number' && typeof y === 'number' && typeof w === 'number' && typeof h === 'number' ? { x, y, w, h } : null;
}

/**
 * The layout the server applied, by display id: WebRTC logs the secondary's
 * pipeline with its rect, the primary being at the origin at its video size;
 * WebSockets logs the whole layout.
 * @param log The server's log.
 * @param mode The transport.
 * @param primary The primary's video size.
 * @returns The layout, or null.
 */
function serverLayout(log: string, mode: string, primary: VideoSize): Record<string, Rect> | null {
    const lines = log.split('\n');
    if (mode === 'webrtc') {
        const line = lines.findLast((l) => l.includes('pipeline started at'));
        const sec = line === undefined ? null : rect(parseLiteral(line.split('pipeline started at ')[1] ?? ''));
        if (line === undefined) return null;
        const layout: Record<string, Rect> = { primary: { x: 0, y: 0, w: primary.w, h: primary.h } };
        if (sec) layout.display2 = sec;
        return layout;
    }
    const line = lines.findLast((l) => l.includes('Layout calculated'));
    const tail = line?.split('Layouts: ')[1];
    if (tail === undefined) return null;
    const parsed = parseLiteral(tail);
    if (typeof parsed !== 'object' || parsed === null) return null;
    const layout: Record<string, Rect> = {};
    for (const [id, value] of Object.entries(parsed)) {
        const r = rect(value);
        if (r) layout[id] = r;
    }
    return layout;
}

/**
 * The primary's video size once it matches what the client asked for: the
 * first frames can still carry the size the display had before the client's
 * resolution request landed, and WebRTC renegotiates a moment later.
 * @param page The client.
 * @param mode The transport.
 * @returns The size, or the last one seen.
 */
async function primarySize(page: Page, mode: string): Promise<VideoSize | null> {
    const want = page.viewportSize()?.width;
    let info: VideoSize | null = null;
    for (const until = Date.now() + 20_000; Date.now() < until;) {
        info = mode === 'webrtc' ? await waitWrVideo(page, 5) : await waitWsVideo(page, 5);
        if (!want || (info && info.w === want)) return info;
        await sleep(500);
    }
    return info;
}

/**
 * The first extend restarts Openbox so it reads the new monitor set: the
 * server says so, the process it started with exits, and a fresh one manages.
 * Where the displays are outputs of their own there is no set to read, and
 * the manager the session started with stays.
 * @param res The checks.
 * @param host The host program.
 * @param wm The Openbox started for the block, or null.
 * @param mark The log's length before the extend.
 */
async function wmRestartCheck(res: Checks, host: Host, wm: Started | null, mark: number): Promise<void> {
    if (wm === null) {
        res.skip('the first extend restarts the window manager', 'no Openbox on the display');
        return;
    }
    // Which way the server went is in its own log: displays as outputs of their
    // own leave the manager alone, a set of logical monitors restarts it.
    let outputs = false;
    let said = false;
    for (const until = Date.now() + 20_000; Date.now() < until && !(said && (outputs || wm.exited() !== null));) {
        const log = await host.log(mark);
        outputs = log.includes('outputs of their own');
        said = outputs || log.includes('restarting openbox');
        await sleep(250);
    }
    const manager = await host.windowManager();
    const managing = manager?.pid ?? 0;
    const started = wm.child.pid ?? -1;
    if (outputs) {
        res.check('the first extend leaves the window manager alone where displays are outputs',
            said && wm.exited() === null && managing === started,
            `said=${said} exit=${wm.exited()} started=${started} managing=${managing}`);
        return;
    }
    res.check('the first extend restarts the window manager',
        said && wm.exited() !== null && managing !== 0 && managing !== started
        && (manager?.name ?? '').toLowerCase().startsWith('openbox'),
        `said=${said} old=${started} exit=${wm.exited()} new=${managing}`);
    if (managing !== 0 && managing !== started) process.kill(managing, 'SIGTERM');
}

const BLOCKS: readonly [string, string, boolean][] = [
    ['websockets-x11', 'websockets', false],
    ['webrtc-x11', 'webrtc', false],
    ['websockets-wl', 'websockets', true],
    ['webrtc-wl', 'webrtc', true],
];

for (const [selector, mode, wayland] of BLOCKS) {
    test(selector, async () => {
        const res = new Checks(`two-display-${mode}-${wayland ? 'wl' : 'x11'}`);
        const host = new Host(suiteOf(import.meta.filename));
        // Openbox on the X11 test display, the manager measured to read the
        // monitor set only as it starts.
        const wm = !wayland && onPath('openbox') ? startOnDisplay(['openbox', '--replace']) : null;
        // The block's steps, which end early once a check leaves nothing to check after it.
        const steps = async (): Promise<void> => {
            const server = await host.start({ label: selector, mode, wayland });
            const { browser, context, page, consoleErrors, notFound } = await launchClient(server.url, { mode });
            try {
                const first = mode === 'webrtc' ? await waitWrVideo(page, 45) : await waitWsVideo(page, 20);
                res.check('primary video flows', first !== null, first);
                if (!first) return;
                const info = await primarySize(page, mode) ?? first;

                const mark = await host.mark();
                const dpage = await newPage(context, server.url, { mode, hash: '#display2-right' });
                res.check('secondary capture started server-side', mode === 'webrtc'
                    ? await host.saw("Secondary display 'display2' pipeline started", 45)
                    : await host.saw("Capture started for 'display2'", 45), '');

                const layout = serverLayout(await host.log(), mode, info);
                res.check('both displays in the layout', layout !== null && 'primary' in layout && 'display2' in layout, layout);
                if (!layout || !('display2' in layout) || !('primary' in layout)) return;

                const prim = layout.primary;
                const sec = layout.display2;
                res.check('secondary is offset, not mirrored', sec.x !== prim.x || sec.y !== prim.y,
                    `primary @${prim.x},${prim.y} secondary @${sec.x},${sec.y}`);
                res.check('secondary sits beside the primary, no overlap',
                    sec.x >= prim.x + prim.w || sec.y >= prim.y + prim.h,
                    `primary ${prim.w}x${prim.h}@${prim.x},${prim.y} secondary ${sec.w}x${sec.h}@${sec.x},${sec.y}`);

                if (!wayland) {
                    const unionW = Math.max(...Object.values(layout).map((r) => r.x + r.w));
                    let grown: [number, number] | null = null;
                    for (const until = Date.now() + 30_000; Date.now() < until && grown === null;) {
                        const got = await host.rootSize();
                        if (got && got[0] >= unionW) grown = got;
                        else await sleep(500);
                    }
                    res.check('framebuffer grew to the union', grown !== null, unionW);
                    await wmRestartCheck(res, host, wm, mark);
                } else {
                    res.check('secondary output has its own geometry', sec.w > 0 && sec.h > 0, sec);
                }

                // A root that matches the monitors still shows a broken desktop when
                // the desktop window itself is placed off the layout origin: the far
                // screen is then left with no wallpaper, icons, or menu.
                const desk = onPath('wmctrl') ? desktopWindow() : null;
                if (!onPath('wmctrl')) {
                    res.skip('desktop window covers the layout', 'wmctrl not installed; cannot list X11 windows');
                } else if (desk === null) {
                    res.skip('desktop window covers the layout', wayland
                        ? 'a native Wayland session has no X11 desktop window'
                        : 'no pcmanfm-qt desktop window runs on this display');
                } else {
                    const [x, y, w, h] = desk;
                    const [rootW, rootH] = await host.rootSize() ?? [0, 0];
                    res.check('desktop window covers the layout', x === 0 && y === 0 && w >= rootW && h >= rootH,
                        `desktop ${w}x${h}@${x},${y} root ${rootW}x${rootH}`);
                }

                await dpage.close();
                await sleep(2000);
                const after = mode === 'webrtc' ? await waitWrVideo(page, 10) : await waitWsVideo(page, 10);
                res.check('primary survives secondary teardown', after !== null, after);

                const real = realConsole(consoleErrors, notFound);
                res.check('no console errors', real.errors.length === 0, real.errors.join('; ').slice(0, 200));
            } finally {
                await browser.close().catch(() => undefined);
            }
        };
        try {
            await steps();
        } finally {
            await host.close();
            wm?.stop();
        }
        res.finish();
    });
}
