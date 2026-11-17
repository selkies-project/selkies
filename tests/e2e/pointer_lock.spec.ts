/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Pointer lock against the server: the remote pointer travels by the deltas.
//
// A locked page gets no positions from the browser, only movementX/Y, and the
// client turns those into m2,dx,dy messages the server injects as relative
// motion (XTEST on X11, the compositor's relative pointer on Wayland). Here a
// real pointer lock is taken in the browser (the client's Ctrl+Shift+click on
// the stream) and every move the test makes while it holds must move the
// server pointer by exactly that much -- a single move, a run of small ones,
// and a negative one -- and releasing the lock must put the client back on
// absolute positions. The wire is tapped at WebSocket/RTCDataChannel send so
// the messages that carried the motion are checked as well as the pointer;
// the pointer itself is read from the X server or, on Wayland, from the
// observer surface the compositor delivers pointer events to. Locked deltas
// are scaled by the client from CSS pixels to stream pixels, so the
// expectations follow the stream size the server realized; that the stream
// matches the window at connect is its own check.
//
// Where the pointer ends up cannot tell a delta from a warp to the same spot,
// and a game can: it reads relative motion (XInput2 raw motion, the Wayland
// relative pointer) and never the position. So the last block puts an SDL2
// window in relative mouse mode over the desktop (tools/sdl_relative_probe.c),
// locks again, and requires every move to reach it as exactly the delta the
// wire carried, one event per message, and a key pressed under the lock to
// arrive as that key. Without a libSDL2 to load the block is skipped.
//
// A -collab selector drives the other page that holds the pointer: a viewer
// the server granted mouse and keyboard access, which keeps the viewer role
// and the input context together. Gaming mode is where its relative motion
// comes from, so there the mode is entered by the client's own Ctrl+Shift+X
// chord rather than by a click, and the deltas have to reach the desktop the
// same way a controller's do.
//
// A desktop selector runs that block under the session manager the images run
// the game under: openbox or kwin_x11 managing the X test display, or labwc or
// kwin_wayland nested on the capture compositor with the game as a client of
// the nested one, which then relays the motion it is given as a Wayland client
// itself. Without that manager installed the block is skipped. A nested
// kwin_wayland turns the seat's motion into absolute events and takes deltas
// only through org_kde_kwin_fake_input, which it advertises to a client while
// KWIN_WAYLAND_NO_PERMISSION_CHECKS is in its environment, as the KDE image
// sets it and this harness does.
//
// Headless Chromium's full build is driven (not the headless shell, whose
// locked movement deltas do not add up), on both transports and backends.
import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync, openSync, readFileSync, statSync, unlinkSync } from 'node:fs';
import { join } from 'node:path';

import type { Browser, Page } from '@playwright/test';
import { chromium, test } from '@playwright/test';

import { BROWSER_ARGS } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { onPath } from '../lib/desktop.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { GameProbe, moves, movesSince, wireDeltas, x11Managed } from '../lib/game.ts';
import { Host, runtimeDir, type Observer } from '../lib/host.ts';
import { WIRE_TAP_JS } from '../lib/page.ts';
import { waitWrVideo, waitWsVideo, within, type VideoSize } from '../lib/stream.ts';

// The secure-mode tokens the collaborator block provisions: a viewer holding
// mk_control is the collaborator the server grants an input context.
const MASTER_TOKEN = 'e2e-pointer-master';
const VIEW_TOKEN = 'e2e-pointer-view-Qx9';

const START = [640, 360] as const;
// The stream the server realizes for the browser window, and so the desktop
// the nested compositors are sized to.
const WINDOW = [1280, 720] as const;
// (dx, dy, repeat): one plain move, a run of small ones, and a move back.
const MOVES: readonly (readonly [number, number, number])[] = [[60, 40, 1], [5, -3, 10], [-200, 100, 1]];
// What the game window is shown: a flick each way, a one-axis nudge, a diagonal.
const GAME_MOVES: readonly (readonly [number, number])[] = [[12, 7], [-5, 9], [80, -30], [3, 0], [0, -4]];

declare global {
    interface Window {
        __wireSent?: unknown[];
    }
}

type Pt = [number, number];

const locked = (page: Page): Promise<boolean> => within(page.evaluate(() => document.pointerLockElement !== null));

/** The full Chromium build on the stream page, with the wire tap installed. */
async function launch(url: string, mode: string, query = ''): Promise<[Browser, Page, string[]]> {
    const executablePath = process.env.E2E_CHROME || undefined;
    const browser = await chromium.launch({ headless: true, args: [...BROWSER_ARGS], ...(executablePath ? { executablePath } : { channel: 'chromium' }) });
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: WIRE_TAP_JS });
    const page = await ctx.newPage();
    const errors: string[] = [];
    page.on('pageerror', (e) => errors.push(String(e)));
    await page.goto(`${url}/${query}`, { waitUntil: 'load' });
    return [browser, page, errors];
}

/** Where the server's pointer is, read from the X server or the Wayland observer. */
class Pointer {
    private readonly host: Host;
    private readonly obs: Observer | null;

    constructor(host: Host, obs: Observer | null) {
        this.host = host;
        this.obs = obs;
    }

    async read(): Promise<Pt | null> {
        if (this.obs === null) return this.host.pointer();
        const seen = (await this.obs.lines()).filter((l) => l.kind === 'ptr_enter' || l.kind === 'ptr_motion');
        const last = seen.at(-1);
        return last ? [Number(last.x), Number(last.y)] : null;
    }

    /** Polled until it reaches `expected` or time runs out. */
    async wait(expected: Pt, timeoutS = 6): Promise<Pt | null> {
        let pos = await this.read();
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
            pos = await this.read();
            if (at(pos, expected)) return pos;
            await sleep(100);
        }
        return pos;
    }
}

function at(pos: Pt | null, expected: Pt): boolean {
    return pos !== null && Math.round(pos[0]) === expected[0] && Math.round(pos[1]) === expected[1];
}

const exited = (proc: ChildProcess): boolean => proc.exitCode !== null || proc.signalCode !== null;

/**
 * The session manager a desktop selector names, started the way the images
 * run it: openbox or kwin_x11 on the X test display; labwc or kwin_wayland
 * nested on the capture compositor, sized to the stream, publishing the socket
 * its own clients connect to.
 */
class Desktop {
    readonly name: string;
    readonly wayland: boolean;
    proc: ChildProcess | null = null;
    socket: string | null = null;
    why = '';
    private readonly runtime: string;

    constructor(name: string, wayland: boolean, runtime: string) {
        this.name = name;
        this.wayland = wayland;
        this.runtime = runtime;
    }

    private get binary(): string {
        return this.name === 'openbox' ? 'openbox' : this.name === 'kwin' ? (this.wayland ? 'kwin_wayland' : 'kwin_x11') : 'labwc';
    }

    private get logPath(): string {
        return join(WORKDIR, `${this.binary}.log`);
    }

    async start(capture: string): Promise<boolean> {
        if (!onPath(this.binary)) {
            this.why = `${this.binary} not installed`;
            return false;
        }
        if (this.name === 'kwin' && !onPath('dbus-run-session')) {
            this.why = 'dbus-run-session not installed';
            return false;
        }
        const log = openSync(this.logPath, 'w');
        const run = (cmd: string[], env: NodeJS.ProcessEnv): void => {
            this.proc = spawn(cmd[0], cmd.slice(1), { env, stdio: ['ignore', log, log], detached: true });
        };
        if (!this.wayland) {
            const display = process.env.E2E_DISPLAY ?? '';
            run(this.name === 'openbox' ? ['openbox', '--replace'] : ['dbus-run-session', '--', 'kwin_x11', '--replace'], { ...process.env, DISPLAY: display });
            return this.wait(() => x11Managed(display), `${this.binary} never took the display`);
        }
        if (!capture) {
            this.why = 'the capture compositor announced no socket';
            return false;
        }
        const env: NodeJS.ProcessEnv = { PATH: process.env.PATH ?? '', HOME: process.env.HOME ?? '', XDG_RUNTIME_DIR: this.runtime, WAYLAND_DISPLAY: capture };
        if (this.name === 'labwc') {
            // labwc picks its socket name; its startup command runs with that
            // name exported, so the command is what publishes it.
            const marker = join(WORKDIR, 'labwc-session-socket');
            if (existsSync(marker)) unlinkSync(marker);
            run(['labwc', '-s', `sh -c 'echo "$WAYLAND_DISPLAY" > ${marker}'`], { ...env, WLR_BACKENDS: 'wayland', WLR_WL_OUTPUTS: '1' });
            if (!(await this.wait(() => existsSync(marker) && statSync(marker).size > 0, 'labwc never published its socket'))) return false;
            this.socket = readFileSync(marker, 'utf8').trim();
        } else {
            this.socket = `wayland-kwin-${process.pid}`;
            // KWin hides its restricted globals (fake input, screencast) from
            // clients it did not start unless told not to; pixelflux reaches a
            // nested KWin only under this, as the KDE images run it.
            run(['dbus-run-session', '--', 'kwin_wayland', '--wayland-display', capture, '--socket', this.socket, '--width', String(WINDOW[0]), '--height', String(WINDOW[1]),
                '--no-lockscreen', '--no-global-shortcuts', '--no-kactivities'], { ...env, KWIN_WAYLAND_NO_PERMISSION_CHECKS: '1' });
            const path = join(this.runtime, this.socket);
            if (!(await this.wait(() => existsSync(path), 'kwin_wayland never opened its socket'))) return false;
        }
        // The nested compositor's own window has to map on the capture
        // compositor before the pointer can reach anything inside it.
        await sleep(2000);
        return true;
    }

    private async wait(ready: () => boolean, why: string, timeoutS = 20): Promise<boolean> {
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
            if (ready()) return true;
            if (this.proc !== null && exited(this.proc)) {
                const tail = existsSync(this.logPath) ? readFileSync(this.logPath, 'utf8').trimEnd().split('\n').slice(-3).join('\n') : '';
                this.why = `${this.name} exited with ${this.proc.exitCode ?? this.proc.signalCode}: ${tail}`;
                return false;
            }
            await sleep(250);
        }
        this.why = why;
        return false;
    }

    /**
     * Ends the manager and everything it started: a KWin runs under
     * dbus-run-session, and ending only that would leave it holding the display
     * and its socket for the suites that follow.
     */
    async stop(): Promise<void> {
        const proc = this.proc;
        if (proc?.pid === undefined) return;
        for (const sig of ['SIGTERM', 'SIGKILL'] as const) {
            try {
                process.kill(-proc.pid, sig);
            } catch {
                return;
            }
            for (const until = Date.now() + 5000; Date.now() < until && !exited(proc);) await sleep(100);
            if (exited(proc)) return;
        }
    }
}

/** The client's own gesture, Ctrl+Shift+click on the stream, and whether it locked. */
async function takeLock(page: Page, where: Pt): Promise<boolean> {
    await page.keyboard.down('Control');
    await page.keyboard.down('Shift');
    await page.mouse.click(...where);
    await page.keyboard.up('Shift');
    await page.keyboard.up('Control');
    for (const until = Date.now() + 5000; Date.now() < until;) {
        if (await locked(page)) return true;
        await sleep(100);
    }
    return false;
}

/** The client's own chord, Ctrl+Shift+X, and whether it ended in a lock. */
async function enterGamingMode(page: Page): Promise<boolean> {
    await page.keyboard.down('Control');
    await page.keyboard.down('Shift');
    await page.keyboard.press('KeyX');
    await page.keyboard.up('Shift');
    await page.keyboard.up('Control');
    for (const until = Date.now() + 8000; Date.now() < until;) {
        if (await locked(page)) return true;
        await sleep(100);
    }
    return false;
}

function relativeSum(messages: readonly string[]): Pt {
    let dx = 0;
    let dy = 0;
    for (const m of messages) {
        if (!m.startsWith('m2,')) continue;
        const parts = m.split(',');
        dx += Number(parts[1]);
        dy += Number(parts[2]);
    }
    return [dx, dy];
}

/** What a game over the desktop sees of a locked pointer and the keyboard. */
async function gameView(page: Page, wayland: boolean, res: Checks, runtime: string, socket: string | null): Promise<void> {
    const probe = new GameProbe(wayland, runtime, socket);
    try {
        const mode = await probe.wait('relative_mode', 20);
        if (mode === null) {
            res.skip('a game window over the desktop takes relative mouse mode', probe.unavailable());
            return;
        }
        res.check('a game window over the desktop takes relative mouse mode', mode.on, mode);
        // The window has to hold the pointer before it may lock it: an
        // absolute move puts the pointer over it and a click gives it the keyboard.
        await sleep(500);
        await page.mouse.move(START[0] + 1, START[1] + 1);
        await page.mouse.click(START[0] + 1, START[1] + 1);
        const entered = await probe.wait('window', 8, { event: 'enter' });
        res.check('the pointer reaches the game window', entered !== null, probe.lines.slice(-3));
        res.check('the game window locks the pointer again', await takeLock(page, [START[0], START[1]]));
        await sleep(500);
        const probeMark = probe.lines.length;
        const wireMark = (await moves(page)).length;
        let cursor: Pt = [START[0], START[1]];
        for (const [dx, dy] of GAME_MOVES) {
            cursor = [cursor[0] + dx, cursor[1] + dy];
            await page.mouse.move(...cursor);
            await sleep(80);
        }
        const wire = wireDeltas(await movesSince(page, wireMark));
        const seen = (await probe.events('motion', probeMark, wire.length, 6)).map((m): Pt => [Number(m.dx), Number(m.dy)]);
        res.check('the wire carried every game move as relative motion', wire.length === GAME_MOVES.length, wire);
        res.check('every locked move reaches the game as the delta the wire carried, one event each', JSON.stringify(seen) === JSON.stringify(wire), `game ${JSON.stringify(seen)} wire ${JSON.stringify(wire)}`);
        const keyMark = probe.lines.length;
        await page.keyboard.press('w');
        const keys = (await probe.events('key', keyMark, 2, 6)).map((k) => [k.down, k.sym]);
        res.check('a key pressed under the lock reaches the game', JSON.stringify(keys) === JSON.stringify([[true, 0x77], [false, 0x77]]), keys);
        await within(page.evaluate(() => { document.exitPointerLock(); }));
        await sleep(300);
    } finally {
        await probe.stop();
    }
}

/** The stream's dimensions, polled until they match `size` or time runs out. */
async function waitStreamSize(page: Page, mode: string, size: Pt, timeoutS = 8): Promise<VideoSize | null> {
    let info: VideoSize | null = null;
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        info = mode === 'websockets' ? await waitWsVideo(page, 2) : await waitWrVideo(page, 2);
        if (info && info.w === size[0] && info.h === size[1]) return info;
        await sleep(500);
    }
    return info;
}

const label = (dx: number, dy: number, repeat: number): string => (repeat > 1 ? `${repeat} x (${dx},${dy})` : `(${dx},${dy})`);
const pt = (p: Pt): string => `(${p[0]}, ${p[1]})`;

async function run(host: Host, mode: string, wayland: boolean, desktop: string | null, res: Checks): Promise<void> {
    const { url, pid } = await host.start({ label: 'lock', mode, wayland });
    const runtime = runtimeDir(pid);
    let obs: Observer | null = null;
    let capture = '';
    try {
        if (wayland) {
            capture = await host.captureSocket();
            res.check('the capture compositor announced its socket', capture !== '', capture);
            obs = await host.observe(capture);
            res.check('wl observer mapped', await obs.ready(20));
        }
        const pointer = new Pointer(host, obs);
        const [browser, page, errors] = await launch(url, mode);
        try {
            let video = mode === 'websockets' ? await waitWsVideo(page, 30) : await waitWrVideo(page);
            res.check('video flowing', Boolean(video), video);
            video = (await waitStreamSize(page, mode, [1280, 720])) ?? video;
            res.check('stream follows the 1280x720 window at connect', video !== null && video.w === 1280 && video.h === 720, video);
            // Locked deltas and positions are scaled by the client onto the
            // stream the server realized; a stream that did not follow the
            // window still has to move the pointer by exactly what it scaled to.
            const scale = video ? video.w / 1280 : 1;
            const server = (css: Pt): Pt => [Math.round(css[0] * scale), Math.round(css[1] * scale)];
            await sleep(1000);
            await page.mouse.move(...START);
            res.check('absolute move lands the pointer at the start', at(await pointer.wait(server([START[0], START[1]])), server([START[0], START[1]])), await pointer.read());
            res.check('Ctrl+Shift+click takes the pointer lock', await takeLock(page, [START[0], START[1]]));
            await sleep(500);

            let [x, y]: Pt = [START[0], START[1]];
            let cursor: Pt = [x, y];
            for (const [dx, dy, repeat] of MOVES) {
                const mark = (await moves(page)).length;
                for (let i = 0; i < repeat; i++) {
                    cursor = [cursor[0] + dx, cursor[1] + dy];
                    await page.mouse.move(...cursor);
                    await sleep(50);
                }
                [x, y] = [x + dx * repeat, y + dy * repeat];
                const want = server([x, y]);
                const pos = await pointer.wait(want);
                res.check(`locked move ${label(dx, dy, repeat)} carries the server pointer to ${pt(want)}`, at(pos, want), pos);
                const sent = await movesSince(page, mark);
                res.check(`locked move ${label(dx, dy, repeat)} went out as relative motion`, sent.length > 0 && sent.every((m) => m.startsWith('m2,')), sent.slice(0, 4));
                const sum = relativeSum(sent);
                const total = server([dx * repeat, dy * repeat]);
                res.check(`locked move ${label(dx, dy, repeat)} deltas add up on the wire`, sum[0] === total[0] && sum[1] === total[1], sum);
            }

            const mark = (await moves(page)).length;
            await within(page.evaluate(() => { document.exitPointerLock(); }));
            for (const until = Date.now() + 5000; Date.now() < until && (await locked(page));) await sleep(100);
            res.check('lock released', !(await locked(page)));
            await sleep(300);
            const target: Pt = [300, 200];
            await page.mouse.move(...target);
            const pos = await pointer.wait(server(target));
            res.check('after release an absolute move lands where it says', at(pos, server(target)), pos);
            const sent = await movesSince(page, mark);
            res.check('after release the motion goes out as positions', sent.length > 0 && sent[sent.length - 1].startsWith('m,'), sent.slice(-2));
            const session = desktop ? new Desktop(desktop, wayland, runtime) : null;
            try {
                if (session === null) {
                    await gameView(page, wayland, res, runtime, capture || null);
                } else if (await session.start(capture)) {
                    res.check(`${desktop} manages the session`, true, session.socket ?? '');
                    await gameView(page, wayland, res, runtime, session.socket);
                } else {
                    res.skip(`${desktop} manages the session`, session.why);
                }
            } finally {
                await session?.stop();
            }
            res.check('no page errors', errors.length === 0, errors.join('; ').slice(0, 200));
        } finally {
            await browser.close();
        }
    } finally {
        await obs?.release();
        await host.stop();
    }
}

/**
 * A viewer the server granted mouse and keyboard access, in gaming mode. The
 * grant attaches the page's input context while it keeps the viewer role, and
 * the server takes relative motion from it (m2 is in the collaborator's
 * allowed prefixes), so what has to hold is that gaming mode holds the pointer
 * for such a page at all and that its deltas reach the desktop.
 */
async function runCollab(host: Host, mode: string, res: Checks): Promise<void> {
    const { url } = await host.start({ label: 'collab', mode, env: { SELKIES_MASTER_TOKEN: MASTER_TOKEN } });
    try {
        const reply = await fetch(`${url}/api/tokens`, { method: 'POST', headers: { Authorization: `Bearer ${MASTER_TOKEN}`, 'Content-Type': 'application/json' },
            body: JSON.stringify({ [VIEW_TOKEN]: { role: 'viewer', slot: null, mk_control: true } }) });
        res.check('the viewer token is provisioned with mouse and keyboard access', reply.status === 200, reply.status);
        const pointer = new Pointer(host, null);
        const [browser, page, errors] = await launch(url, mode, `?token=${VIEW_TOKEN}`);
        try {
            const video = mode === 'websockets' ? await waitWsVideo(page, 30) : await waitWrVideo(page);
            res.check('video flowing', Boolean(video), video);
            const state = (): Promise<unknown> => within(page.evaluate('window.webrtcInput ? [window.webrtcInput.isSharedMode, window.webrtcInput.isInputAttached()] : null'));
            let granted = false;
            for (const until = Date.now() + 20_000; Date.now() < until && !granted;) {
                granted = JSON.stringify(await state()) === '[true,true]';
                if (!granted) await sleep(250);
            }
            res.check('the page keeps the viewer role and holds the granted input context', granted, await state());
            await sleep(1000);
            let cursor: Pt = [START[0], START[1]];
            await page.mouse.move(...cursor);
            await sleep(500);
            res.check('Ctrl+Shift+X locks the pointer for the collaborator', await enterGamingMode(page));
            await sleep(500);
            const readable = await pointer.read();
            res.check('the desktop pointer is readable', readable !== null, readable);
            // How far a delta travels is the stream box the client measured,
            // which a viewer's own window does not decide; what the pointer has
            // to do is move by the deltas that went out, in the direction the
            // hand moved.
            for (const [dx, dy, repeat] of MOVES) {
                const mark = (await moves(page)).length;
                const before = (await pointer.read()) ?? [0, 0];
                for (let i = 0; i < repeat; i++) {
                    cursor = [cursor[0] + dx, cursor[1] + dy];
                    await page.mouse.move(...cursor);
                    await sleep(50);
                }
                const l = label(dx, dy, repeat);
                const sent = await movesSince(page, mark);
                res.check(`the collaborator's locked move ${l} went out as relative motion`, sent.length > 0 && sent.every((m) => m.startsWith('m2,')), sent.slice(0, 4));
                const total = relativeSum(sent);
                res.check(`the collaborator's locked move ${l} carries the hand's direction`, total[0] * dx * repeat > 0 && total[1] * dy * repeat > 0, total);
                const expected: Pt = [before[0] + total[0], before[1] + total[1]];
                const pos = await pointer.wait(expected);
                res.check(`the collaborator's locked move ${l} moves the desktop pointer by what the wire carried`, at(pos, expected), `${JSON.stringify(pos)} wanted ${JSON.stringify(expected)}`);
            }
            res.check('no page errors', errors.length === 0, errors.join('; ').slice(0, 200));
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
}

const SELECTORS = ['ws-x11', 'wr-x11', 'ws-wl', 'wr-wl', 'ws-x11-openbox', 'ws-x11-kwin', 'ws-wl-labwc', 'ws-wl-kwin', 'ws-x11-collab', 'wr-x11-collab'];

for (const which of SELECTORS) {
    test(which, async () => {
        const [transport, backend, extra] = which.split('-');
        const res = new Checks(`pointer-lock-${which}`);
        const mode = transport === 'ws' ? 'websockets' : 'webrtc';
        const host = new Host(suiteOf(import.meta.filename));
        try {
            if (extra === 'collab') await runCollab(host, mode, res);
            else await run(host, mode, backend === 'wl', extra ?? null, res);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
