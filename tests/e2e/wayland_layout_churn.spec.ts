/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Layout passes that restart both Wayland displays back to back keep both
// streams alive. A user fullscreening either display over and over, or
// dragging a browser edge, has the server restart every live capture on each
// layout pass: the resized display's encoder session is reconfigured in place
// and the unchanged sibling's is left as it is. Two hardware sessions torn
// down and rebuilt back to back on one GPU is what has wedged an NVENC driver
// before, so this churns those passes on both transports: two pages toggle
// their viewports between a windowed and a fullscreen-like size, one at a
// time and both at once, with a nested labwc session composited too when one
// is installed, and every stream a pass changes has to decode new frames
// after it, each resized page's at the size it asked for: the resized
// displays, and the secondary when the primary's resize moves it along the
// row. A display a pass leaves alone shows a still screen, which owes its
// page no frame. A stream that stays frozen, or short of that size, is
// reported with gdb stack traces of the server where gdb is present, so a
// lockup that only shows on some hardware leaves its evidence behind.
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { openSync } from 'node:fs';
import { join } from 'node:path';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { onPath } from '../lib/desktop.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { launchClient, newPage } from '../lib/page.ts';
import { closeBrowser, waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const PASSES = 24;
const STALL_S = 15;
const MAX_GAP_S = 0.6;
const PRIMARY_SIZES: readonly [number, number][] = [[1600, 838], [1920, 1080]];
const SECONDARY_SIZES: readonly [number, number][] = [[1280, 720], [1920, 1080]];

declare global {
    interface Window {
        __streamSize?: [number, number];
    }
}

/**
 * A compositor refusal, a dropped frame, or a crash; the audio capture's own
 * retries are not this suite's to judge.
 * @param line A log line.
 * @returns Whether it is trouble.
 */
function logTrouble(line: string): boolean {
    if (line.startsWith('[pcmflux]')) return false;
    if (line.includes('[Wayland]') && ['rejected', 'failed', 'not signaled'].some((t) => line.includes(t))) return true;
    return ['HW Encode Error', 'Traceback', 'panicked'].some((t) => line.includes(t));
}

/**
 * labwc nested on the capture socket, as the images run it.
 * @param socket The compositor's socket.
 * @returns The session, or null without a labwc or when it did not stay up.
 */
async function spawnLabwc(socket: string): Promise<ChildProcess | null> {
    if (!onPath('labwc')) return null;
    const env: NodeJS.ProcessEnv = { ...process.env, WAYLAND_DISPLAY: socket, WLR_BACKENDS: 'wayland', XDG_RUNTIME_DIR: join(WORKDIR, 'run'), WLR_WL_OUTPUTS: '1' };
    delete env.DISPLAY;
    const log = openSync(join(WORKDIR, 'labwc.log'), 'w');
    const child = spawn('labwc', [], { env, stdio: ['ignore', log, log] });
    let exited = false;
    child.on('exit', () => { exited = true; });
    await sleep(2000);
    return exited ? null : child;
}

/**
 * Frames the page has taken in: decoded WebRTC video frames, or WebSocket video chunks.
 * @param page The client.
 * @param mode The transport.
 * @returns The count, or why it could not be read.
 */
async function framesTaken(page: Page, mode: string): Promise<number | string> {
    try {
        if (mode === 'websockets') return await within(page.evaluate(() => window.videoChunksReceived || 0));
        return await within(page.evaluate(() => {
            const v = document.querySelector('video');
            if (!v) return -1;
            return v.getVideoPlaybackQuality().totalVideoFrames;
        }));
    } catch (e) {
        return `evaluate failed: ${String(e)}`;
    }
}

/**
 * Tap the size the last WebSockets video frame's header carries (`u16` width
 * and height at offset 6), off the page's handle on its socket worker.
 * @param page The client.
 */
async function tapStreamSize(page: Page): Promise<void> {
    await within(page.evaluate(() => {
        // The handle is whatever the core publishes with an addEventListener,
        // not necessarily an EventTarget, and its events carry `data`.
        const t: unknown = Reflect.get(window, 'selkiesTransport');
        if (typeof t !== 'object' || t === null) return;
        const listen: unknown = Reflect.get(t, 'addEventListener');
        if (typeof listen !== 'function' || Reflect.get(t, '__sizeTapped') === true) return;
        Reflect.set(t, '__sizeTapped', true);
        Reflect.apply(listen, t, ['message', (e: unknown) => {
            const data: unknown = typeof e === 'object' && e !== null ? Reflect.get(e, 'data') : null;
            if (!(data instanceof ArrayBuffer) || data.byteLength < 12) return;
            const h = new Uint8Array(data, 0, 12);
            if (h[0] === 0x04) window.__streamSize = [(h[6] << 8) | h[7], (h[8] << 8) | h[9]];
        }]);
    }));
}

/**
 * The size of the stream the page takes in: its WebRTC video's, or its WebSockets frames'.
 * @param page The client.
 * @param mode The transport.
 * @returns The size, null before any, or why it could not be read.
 */
async function streamSize(page: Page, mode: string): Promise<[number, number] | null | string> {
    try {
        if (mode === 'websockets') return await within(page.evaluate(() => window.__streamSize ?? null));
        return await within(page.evaluate((): [number, number] | null => {
            const v = document.querySelector('video');
            return v && v.videoWidth > 0 ? [v.videoWidth, v.videoHeight] : null;
        }));
    } catch (e) {
        return `evaluate failed: ${String(e)}`;
    }
}

/**
 * Whether two sizes are the same.
 * @param a A size, or what stood in for one.
 * @param b A size.
 * @returns True when both are that size.
 */
function sameSize(a: [number, number] | null | string, b: readonly [number, number]): boolean {
    return Array.isArray(a) && a[0] === b[0] && a[1] === b[1];
}

/**
 * The pages that within `stallS` took in no frame past `before`, or, for one
 * `sizes` names, no stream of that size. A page sends its resize once its own
 * debounce runs out, which can be after its sibling's change has already
 * brought both pages new frames.
 * @param pages The pages a pass changed.
 * @param before Their frame counts before it.
 * @param sizes The sizes the resized ones asked for.
 * @param mode The transport.
 * @param stallS How long they are given.
 * @returns The names of those that stalled.
 */
async function streamsSettle(pages: Record<string, Page>, before: Record<string, number | string>,
    sizes: Record<string, readonly [number, number]>, mode: string, stallS: number): Promise<string[]> {
    const pending = new Set(Object.keys(pages));
    for (const until = Date.now() + stallS * 1000; pending.size && Date.now() < until;) {
        await sleep(250);
        for (const name of [...pending]) {
            const now = await framesTaken(pages[name], mode);
            const want = sizes[name];
            const sized = want === undefined || sameSize(await streamSize(pages[name], mode), want);
            const was = before[name];
            if (sized && typeof now === 'number' && typeof was === 'number' && now > was) pending.delete(name);
        }
    }
    return [...pending].sort();
}

/**
 * gdb stack traces of the server, when gdb is installed.
 * @param pid The server's process.
 * @returns The traces, or why there are none.
 */
function serverStacks(pid: number): string {
    if (!onPath('gdb')) return '(gdb not installed)';
    const run = spawnSync('gdb', ['-p', String(pid), '-batch', '-ex', 'set pagination off', '-ex', 'thread apply all bt'],
        { encoding: 'utf8', timeout: 240_000 });
    return `===== gdb pid ${pid} =====\n${run.stdout ?? ''}${run.stderr ?? ''}`;
}

/**
 * The gaps between passes: a fixed sequence in [0, 1), so a run is the same
 * churn each time (mulberry32, seeded 11).
 * @returns The next fraction.
 */
function gaps(): () => number {
    let state = 11;
    return () => {
        state = (state + 0x6D2B79F5) | 0;
        let t = Math.imul(state ^ (state >>> 15), 1 | state);
        t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
}

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const res = new Checks(`wl-layout-churn-${mode}`);
        const host = new Host(suiteOf(import.meta.filename));
        // Kept apart from the steps so the cleanup sees what they started.
        const nested: { labwc: ChildProcess | null } = { labwc: null };
        const steps = async (): Promise<void> => {
            const server = await host.start({ label: mode, mode, wayland: true });
            let socket = '';
            for (const until = Date.now() + 30_000; Date.now() < until && !socket;) {
                const line = (await host.log()).split('\n').find((l) => l.includes('Socket listening on:'));
                socket = line?.split('"')[1] ?? '';
                if (!socket) await sleep(500);
            }
            res.check('the compositor announced its socket', Boolean(socket), socket);
            nested.labwc = socket ? await spawnLabwc(socket) : null;
            if (nested.labwc === null) res.skip('a nested labwc session is composited', 'labwc not installed or not started');
            const { browser, context, page } = await launchClient(server.url, { mode });
            try {
                const wait = (p: Page, s: number) => (mode === 'websockets' ? waitWsVideo(p, s) : waitWrVideo(p, s));
                const info = await wait(page, 60);
                res.check('primary video flows', Boolean(info), info);
                const dpage = await newPage(context, server.url, { mode, hash: '#display2' });
                const info2 = await wait(dpage, 90);
                res.check('secondary video flows beside the primary', Boolean(info2), info2);
                if (!(info && info2)) return;
                const pages: Record<string, Page> = { primary: page, secondary: dpage };
                if (mode === 'websockets') for (const p of Object.values(pages)) await tapStreamSize(p);
                const gap = gaps();
                const mark = await host.mark();
                let frozen: string | null = null;
                // Both pages open at 1280x720, the secondary's first size, so its
                // toggle starts there: every pass asks a page for a size it does not
                // have. A pass that changes nothing restarts no capture.
                let pi = 1;
                let si = 0;
                for (let i = 0; i < PASSES; i++) {
                    const kind = (['primary', 'secondary', 'both'] as const)[i % 3];
                    const before: Record<string, number | string> = {};
                    for (const [name, p] of Object.entries(pages)) before[name] = await framesTaken(p, mode);
                    const sizes: Record<string, readonly [number, number]> = {};
                    if (kind === 'primary' || kind === 'both') {
                        pi ^= 1;
                        sizes.primary = PRIMARY_SIZES[pi];
                        await page.setViewportSize({ width: PRIMARY_SIZES[pi][0], height: PRIMARY_SIZES[pi][1] });
                    }
                    if (kind === 'secondary' || kind === 'both') {
                        si ^= 1;
                        sizes.secondary = SECONDARY_SIZES[si];
                        await dpage.setViewportSize({ width: SECONDARY_SIZES[si][0], height: SECONDARY_SIZES[si][1] });
                    }
                    // The streams this pass changed: the resized displays, and the
                    // secondary, whose output the primary's resize moves along the row.
                    const changed: Record<string, Page> = {};
                    for (const [name, p] of Object.entries(pages)) {
                        if (name in sizes || (name === 'secondary' && 'primary' in sizes)) changed[name] = p;
                    }
                    const stalled = await streamsSettle(changed, before, sizes, mode, STALL_S);
                    if (stalled.length) {
                        const left: string[] = [];
                        for (const name of stalled) {
                            left.push(`${name} at ${JSON.stringify(await streamSize(pages[name], mode))} for ${JSON.stringify(sizes[name] ?? 'its size')}`);
                        }
                        frozen = `pass ${i} (${kind}) left ${left.join('; ')} after ${STALL_S}s`;
                        process.stdout.write(`${serverStacks(server.pid)}\n`);
                        process.stdout.write(`===== server log tail =====\n${(await host.log()).split('\n').slice(-80).join('\n')}\n`);
                        break;
                    }
                    await sleep(gap() * MAX_GAP_S * 1000);
                }
                res.check(`every one of ${PASSES} layout passes kept the streams it changed decoding`, frozen === null, frozen ?? '');
                const trouble = (await host.log(mark)).split('\n').filter(logTrouble);
                res.check('the server refused or failed nothing during the passes', trouble.length === 0, trouble.slice(0, 3));
                if (frozen === null) await dpage.close();
            } finally {
                await closeBrowser(browser);
            }
        };
        try {
            await steps();
        } finally {
            nested.labwc?.kill('SIGTERM');
            await host.close();
        }
        res.finish();
    });
}
