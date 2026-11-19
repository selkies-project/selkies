/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Gaming mode in a real browser: the pointer and the keyboard held, and a game fed.
//
// Gaming mode is where a game gets its input: fullscreen with the pointer
// locked, the keyboard locked so Escape reaches the session, and locked
// motion relayed as deltas. A headless engine takes synthetic input and holds
// no keyboard, so this drives the installed Chrome, windowed under openbox on
// a private X server of its own, with XTEST keys, clicks, and relative motion
// the way a mouse and a keyboard deliver them, while the server streams the
// test display or its Wayland compositor and an SDL2 window in relative mode
// over that desktop (tools/sdl_relative_probe.c) stands in for the game.
//
// What has to hold: the gaming mode chord fullscreens the page and locks the
// pointer; every relative move on the browser's display goes out as exactly
// that delta and reaches the game as exactly that delta, one event per
// message; a key reaches the game; a tap of Escape reaches it as Escape and
// leaves the mode standing, since the keyboard lock holds the key, and so do
// the browser's own Ctrl+T and Ctrl+W, which reach the game while the browser
// keeps its one tab, and, where the Keyboard Lock API holds the system's keys
// too, Alt+Tab, which goes to the session rather than switching windows on
// the browser's desktop; a held Escape ends the mode. A second page withholds
// the Keyboard Lock API the way Brave's Shields do, and there the client has
// to say so once and a single Escape has to end the mode, which is what a
// user of that browser gets.
//
// Firefox has no Keyboard Lock API; it holds its keys for a fullscreen asked
// for with `keyboardLock: "browser"`. The installed Firefox, where there is
// one, is driven through the same block, and must hold Escape, Ctrl+T, and
// Ctrl+W for the game without a notice; Alt+Tab stays the desktop's there.
//
// The -two selectors lay a second display beside the locked one and walk the
// remote pointer onto it, since a lock is one page's and the pointer is the
// whole desktop's: what has to hold is that the lock survives the crossing and
// the deltas keep going out, with the drawing side of it read off the pixels
// in crates/selkies-suites/tests/locked_cursor_across_displays.rs.
//
// The x11-dashboards selector serves each dashboard layout and enters the mode
// the way a user does there, by a click on its gaming mode button (the edge
// layout's in the fan its button opens): the keys have to reach the game then
// too, where the click left the keyboard on the button.
import { spawn, type ChildProcess } from 'node:child_process';

import type { Browser, Page } from '@playwright/test';
import { chromium, firefox, test } from '@playwright/test';

import { BROWSER_ARGS, launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { onPath } from '../lib/desktop.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { GameProbe, moves, movesSince, wireDeltas, x11Managed } from '../lib/game.ts';
import { Host, runtimeDir, type Held } from '../lib/host.ts';
import { WIRE_TAP_JS } from '../lib/page.ts';
import { waitWsVideo, within } from '../lib/stream.ts';

const LAYOUTS = ['default', 'wish', 'edge'] as const;
const WINDOW = [1600, 900] as const;
const CENTER = [800, 450] as const;
// Deltas a hand makes: a flick each way, a one-axis nudge, a diagonal.
const MOVES: readonly (readonly [number, number])[] = [[12, 7], [-5, 9], [80, -30], [3, 0], [0, -4]];
const ESCAPE_HOLD_MS = 3000;
// The display beside the locked one: a headless page is the whole of it here,
// since the lock and the wire are what the two-display block reads.
const NEIGHBOR_CSS = [1280, 720] as const;
// How far past the seam the travel lands the remote pointer, and the step it travels in.
const OVER = 320;
const CROSS_STEP = 80;

const KEYSYMS: Record<string, number> = { w: 0x77, x: 0x78, t: 0x74, Escape: 0xff1b, Control_L: 0xffe3, Shift_L: 0xffe1, Alt_L: 0xffe9, Tab: 0xff09 };

const NOTICE_TAP_JS = `
window.__notices = [];
window.addEventListener('message', (e) => {
  const d = e.data;
  if (d && d.type === 'fileUpload' && d.payload && typeof d.payload.code === 'string') {
    window.__notices.push(d.payload.code);
  }
});
`;
// What Brave's Shields leave a page: no Keyboard API at all, and Brave's own
// navigator object to tell the browser by.
const SHIELDS_JS = `
Object.defineProperty(navigator, 'keyboard', { value: undefined, configurable: true });
Object.defineProperty(navigator, 'brave', { value: {}, configurable: true });
`;
const MODE_JS = '({ fullscreen: document.fullscreenElement !== null, locked: document.pointerLockElement !== null, gaming: !!(window.webrtcInput && window.webrtcInput.gamingMode) })';
const LAYOUT_JS = 'window.webrtcInput && window.webrtcInput._layout';
// Every pointer lock refusal the page meets, by the error's name: a lock
// refused for want of a user gesture (NotAllowedError) is the one asked for
// after fullscreen spent the gesture that entered gaming mode, which a strict
// engine (Edge on Windows) never grants, however often it is asked again.
const LOCK_TAP_JS = `(() => { window.__lockRefusals = []; const ask = Element.prototype.requestPointerLock;
  Element.prototype.requestPointerLock = function (...a) { const p = ask.apply(this, a);
    if (p && p.catch) p.catch((e) => window.__lockRefusals.push(e && e.name)); return p; }; })();`;
// The center of the first visible element a selector matches, in the browser's desktop's coordinates.
const SCREEN_JS = `((sel) => { const el = [...document.querySelectorAll(sel)].find((e) => e.getBoundingClientRect().width > 0);
  if (!el) return null; const r = el.getBoundingClientRect();
  const left = window.screenX + (window.outerWidth - window.innerWidth) / 2;
  const top = window.screenY + window.outerHeight - window.innerHeight;
  return [Math.round(left + r.x + r.width / 2), Math.round(top + r.y + r.height / 2)]; })`;

type Pt = [number, number];
type Rec = Record<string, unknown>;
const r = (v: unknown): string => JSON.stringify(v);
const js = (page: Page, code: string): Promise<unknown> => within(page.evaluate(code));
const rec = (v: unknown): Rec => (v !== null && typeof v === 'object' && !Array.isArray(v) ? { ...v } : {});

/** The browser's own desktop: a private X server managed by openbox, and the XTEST input that plays the user's hands on it. */
class Desk {
    readonly display: string;
    private readonly host: Host;
    private readonly xvfb: Held;
    private readonly wm: ChildProcess;

    private constructor(host: Host, xvfb: Held & { display: string }, wm: ChildProcess) {
        this.host = host;
        this.xvfb = xvfb;
        this.display = xvfb.display;
        this.wm = wm;
    }

    static async start(host: Host): Promise<Desk> {
        const xvfb = await host.xvfb(WINDOW[0], WINDOW[1]);
        const wm = spawn('openbox', ['--replace'], { env: { ...process.env, DISPLAY: xvfb.display }, stdio: 'ignore' });
        for (const until = Date.now() + 15_000; Date.now() < until && !x11Managed(xvfb.display);) await sleep(250);
        return new Desk(host, xvfb, wm);
    }

    async key(name: string, down: boolean): Promise<void> {
        await this.host.key(KEYSYMS[name], down, this.display);
    }

    async tap(name: string, holdMs = 60): Promise<void> {
        await this.key(name, true);
        await sleep(holdMs);
        await this.key(name, false);
    }

    async chord(...names: string[]): Promise<void> {
        for (const name of names) {
            await this.key(name, true);
            await sleep(30);
        }
        for (const name of [...names].reverse()) {
            await this.key(name, false);
            await sleep(30);
        }
    }

    async click(x: number, y: number): Promise<void> {
        await this.host.warp(x, y, this.display);
        await sleep(100);
        await this.host.button(1, true, this.display);
        await sleep(50);
        await this.host.button(1, false, this.display);
    }

    async move(dx: number, dy: number): Promise<void> {
        await this.host.motion(dx, dy, 1, 0, this.display);
    }

    async stop(): Promise<void> {
        this.wm.kill('SIGTERM');
        await this.xvfb.release();
    }
}

interface Mode {
    fullscreen: boolean;
    locked: boolean;
    gaming: boolean;
}

async function mode(page: Page): Promise<Mode> {
    const got = rec(await js(page, MODE_JS));
    return { fullscreen: got.fullscreen === true, locked: got.locked === true, gaming: got.gaming === true };
}

/** The page's mode flags, polled until the wanted ones hold or time runs out. */
async function waitMode(page: Page, timeoutS: number, want: Partial<Mode>): Promise<Mode> {
    let m = await mode(page);
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        m = await mode(page);
        if ((['fullscreen', 'locked', 'gaming'] as const).every((k) => want[k] === undefined || m[k] === want[k])) break;
        await sleep(100);
    }
    return m;
}

async function openPage(browser: Browser, url: string, shields: boolean): Promise<Page> {
    const ctx = await browser.newContext({ viewport: null });
    await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
    await ctx.addInitScript({ content: LOCK_TAP_JS });
    await ctx.addInitScript({ content: WIRE_TAP_JS });
    await ctx.addInitScript({ content: NOTICE_TAP_JS });
    if (shields) await ctx.addInitScript({ content: SHIELDS_JS });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    return page;
}

interface Session {
    host: Host;
    desk: Desk;
    url: string;
    wayland: boolean;
    capture: string;
    runtime: string;
}

/**
 * The game window over the desktop, with the pointer already on it, as a
 * player has it before locking: SDL counts relative motion only once
 * something has positioned its pointer, so the first delta of a lock taken
 * cold is dropped.
 */
async function gameWindow(res: Checks, s: Session, tag: string): Promise<GameProbe | null> {
    const probe = new GameProbe(s.wayland, s.runtime, s.capture || null);
    const relative = await probe.wait('relative_mode', 20);
    if (relative === null) {
        res.skip(`${tag}: a game window over the desktop takes relative mouse mode`, probe.unavailable());
        await probe.stop();
        return null;
    }
    res.check(`${tag}: a game window over the desktop takes relative mouse mode`, relative.on, relative);
    await s.desk.click(...CENTER);
    const entered = await probe.wait('window', 8, { event: 'enter' });
    res.check(`${tag}: the pointer reaches the game window`, entered !== null, probe.lines.slice(-3));
    return probe;
}

const keysOf = (lines: Rec[]): [unknown, unknown][] => lines.map((k) => [k.down, k.sym]);

/** Gaming mode with a keyboard the browser locks: the game is fed, Escape is held, and, with `shortcuts`, the browser's own Ctrl+T and Ctrl+W as well. */
async function played(res: Checks, desk: Desk, page: Page, probe: GameProbe, tag: string, shortcuts = false): Promise<void> {
    try {
        await sleep(300);
        const wireMark = (await moves(page)).length;
        const probeMark = probe.lines.length;
        for (const [dx, dy] of MOVES) {
            await desk.move(dx, dy);
            await sleep(100);
        }
        const wire = wireDeltas(await movesSince(page, wireMark));
        const seen = (await probe.events('motion', probeMark, MOVES.length, 6)).map((m): Pt => [Number(m.dx), Number(m.dy)]);
        // The engine rounds a locked movement its own way (a pixel can shift
        // between two events), so the count and the distance are what the
        // browser owes; the game must then see exactly what the wire carried.
        const travel: Pt = [wire.reduce((n, d) => n + d[0], 0), wire.reduce((n, d) => n + d[1], 0)];
        const moved: Pt = [MOVES.reduce((n, d) => n + d[0], 0), MOVES.reduce((n, d) => n + d[1], 0)];
        res.check(`${tag}: the moves of the mouse go out as relative motion adding up to the distance moved`, wire.length === MOVES.length && r(travel) === r(moved), `${r(wire)} travel ${r(travel)} moved ${r(moved)}`);
        res.check(`${tag}: every move reaches the game as the delta the wire carried, one event each`, r(seen) === r(wire), `game ${r(seen)} wire ${r(wire)}`);

        let keyMark = probe.lines.length;
        await desk.tap('w');
        let keys = keysOf(await probe.events('key', keyMark, 2, 6));
        res.check(`${tag}: a key reaches the game`, r(keys) === r([[true, 0x77], [false, 0x77]]), keys);

        keyMark = probe.lines.length;
        await desk.tap('Escape');
        keys = keysOf(await probe.events('key', keyMark, 2, 6));
        res.check(`${tag}: a tap of Escape reaches the game as Escape`, r(keys) === r([[true, 27], [false, 27]]), keys);
        await sleep(1000);
        let m = await mode(page);
        res.check(`${tag}: a tap of Escape leaves gaming mode standing`, m.fullscreen && m.locked && m.gaming, m);
        if (shortcuts) {
            for (const letter of ['t', 'w']) {
                keyMark = probe.lines.length;
                await desk.chord('Control_L', letter);
                keys = keysOf(await probe.events('key', keyMark, 4, 6));
                await sleep(800);
                m = await mode(page);
                const tabs = page.context().pages().length;
                res.check(`${tag}: Ctrl+${letter.toUpperCase()} reaches the game, and the browser keeps its one tab and gaming mode`,
                    keys.some(([down, sym]) => down === true && sym === letter.charCodeAt(0)) && tabs === 1 && m.fullscreen && m.gaming, `${r(keys)} ${tabs} ${r(m)}`);
            }
            // The browser's desktop switches windows on Alt+Tab (openbox binds
            // it): under the lock the chord goes to the session, whose own
            // window manager it is meant for. A fullscreen `keyboardLock` holds
            // the browser's shortcuts, not the desktop's, and pressing it there
            // would hand the window manager the focus the checks after it need.
            if (await js(page, '!!(navigator.keyboard && navigator.keyboard.lock)')) {
                const sentAll = async (): Promise<string[]> => {
                    const got = await js(page, 'window.__wireSent');
                    return Array.isArray(got) ? got.filter((d): d is string => typeof d === 'string') : [];
                };
                const mark = (await sentAll()).length;
                await desk.chord('Alt_L', 'Tab');
                await sleep(800);
                const sent = (await sentAll()).slice(mark).filter((d) => d.startsWith('kd,') || d.startsWith('ku,'));
                m = await mode(page);
                res.check(`${tag}: Alt+Tab goes to the session, not the browser's desktop, and gaming mode stands`,
                    sent.includes('kd,65513') && sent.includes('kd,65289') && sent.indexOf('kd,65513') < sent.indexOf('kd,65289') && m.fullscreen && m.gaming, `${r(sent)} ${r(m)}`);
            } else {
                res.skip(`${tag}: Alt+Tab goes to the session, not the browser's desktop`, "no Keyboard Lock API: a fullscreen keyboardLock holds the browser's shortcuts, not the desktop's window switching");
            }
        }
    } finally {
        await probe.stop();
    }

    await desk.key('Escape', true);
    await sleep(ESCAPE_HOLD_MS);
    await desk.key('Escape', false);
    const m = await waitMode(page, 10, { fullscreen: false });
    res.check(`${tag}: a held Escape ends gaming mode`, !m.fullscreen && !m.locked && !m.gaming, m);
    const notices = await js(page, 'window.__notices');
    res.check(`${tag}: a browser that locks the keyboard hears no notice about it`, r(notices) === '[]', notices);
}

/** Gaming mode without the Keyboard Lock API: said once, and one Escape ends it. */
async function shielded(res: Checks, desk: Desk, page: Page, tag: string): Promise<void> {
    const once = r(['keyboardLockBlockedByShields']);
    let notices = await js(page, 'window.__notices');
    res.check(`${tag}: entering without a keyboard lock says so once, naming the Shields`, r(notices) === once, notices);
    await desk.tap('Escape');
    const m = await waitMode(page, 10, { fullscreen: false });
    res.check(`${tag}: a single Escape ends gaming mode there`, !m.fullscreen && !m.locked && !m.gaming, m);
    notices = await js(page, 'window.__notices');
    res.check(`${tag}: the notice is not repeated`, r(notices) === once, notices);
}

/** The page's multi-display layout, once both rectangles have reached it. */
async function waitLayout(page: Page, timeoutS: number): Promise<Rec> {
    let layout: Rec = {};
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        layout = rec(await js(page, LAYOUT_JS));
        if (Array.isArray(layout.rects) && layout.rects.length === 2) break;
        await sleep(250);
    }
    return layout;
}

/** The lock is asked for on the gesture that entered the mode: never refused for want of one. */
async function gestureCheck(res: Checks, page: Page, tag: string): Promise<void> {
    const refused = await js(page, 'window.__lockRefusals || []');
    res.check(`${tag}: the pointer lock is asked for on the gesture that entered gaming mode, never refused for want of one`, !(Array.isArray(refused) && refused.includes('NotAllowedError')), refused);
}

/** Focus the stream and press the chord; true once the page is fullscreen and locked. */
async function enterGamingMode(res: Checks, desk: Desk, page: Page, tag: string): Promise<boolean> {
    await desk.click(...CENTER);
    await sleep(500);
    await desk.chord('Control_L', 'Shift_L', 'x');
    const m = await waitMode(page, 15, { fullscreen: true, locked: true });
    const ok = m.fullscreen && m.locked && m.gaming;
    res.check(`${tag}: the gaming mode chord fullscreens the page and locks the pointer`, ok, m);
    await gestureCheck(res, page, tag);
    if (ok) await sleep(500);
    return ok;
}

async function screenAt(page: Page, selector: string): Promise<Pt | null> {
    const got = await js(page, `${SCREEN_JS}(${r(selector)})`);
    return Array.isArray(got) && got.length === 2 ? [Number(got[0]), Number(got[1])] : null;
}

/**
 * Open the sidebar where the layout has one, or the edge button's fan, and
 * click its gaming mode button, or the toolbar's. The edge button slides out
 * of its corner under the pointer, so where it is is read again for a second
 * click where the first did not open the fan.
 */
async function enterByButton(res: Checks, desk: Desk, page: Page, tag: string): Promise<boolean> {
    await desk.click(...CENTER);
    await sleep(500);
    const handle = await screenAt(page, '.toggle-handle');
    if (handle) {
        await desk.click(...handle);
        await sleep(1000);
        const modes = await screenAt(page, '.sidebar-menu-button');
        if (modes) {
            await desk.click(...modes);
            await sleep(800);
        }
    }
    for (let i = 0; i < 2; i++) {
        const edge = await screenAt(page, '.edge-button');
        if (!edge || (await page.locator('.edge-button[aria-expanded="true"]').count())) break;
        await desk.click(...edge);
        await sleep(800);
    }
    const button = await screenAt(page, '.gaming-mode-button');
    if (!button) {
        res.check(`${tag}: the dashboard shows its gaming mode button`, false, button);
        return false;
    }
    await desk.click(...button);
    const m = await waitMode(page, 15, { fullscreen: true, locked: true });
    const ok = m.fullscreen && m.locked && m.gaming;
    res.check(`${tag}: the gaming mode button fullscreens the page and locks the pointer`, ok, m);
    await gestureCheck(res, page, tag);
    if (ok) await sleep(500);
    return ok;
}

/**
 * Gaming mode on the primary, and the remote pointer walked onto the
 * neighbor. One lock covers the whole desktop with nothing written for it: a
 * locked page has only deltas to send and sends them wherever they land, the
 * server bounds the one pointer by the union of the display rectangles rather
 * than by the locked page's own, and each capture draws the cursor from that
 * one position -- which locked_cursor_across_displays.rs reads off the pixels
 * each display is sent. The half a browser holds is what this checks: the lock
 * does not break when the pointer it can no longer place leaves the display
 * the page shows, and no absolute position goes out in place of the deltas.
 * The crossing itself is read from the X pointer; on the Wayland backend,
 * which has none to read, the seam is pixelflux's wayland_seam test's.
 */
async function crossed(res: Checks, s: Session, page: Page, tag: string): Promise<void> {
    const browser = await launchChromium();
    try {
        const ctx = await browser.newContext({ viewport: { width: NEIGHBOR_CSS[0], height: NEIGHBOR_CSS[1] }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
        const dpage = await ctx.newPage();
        await dpage.goto(`${s.url}/#display2-right`, { waitUntil: 'load' });
        res.check(`${tag}: the neighbor's video flows`, Boolean(await waitWsVideo(dpage, 30)), '');
        if (!(await enterGamingMode(res, s.desk, page, tag))) return;
        // Fullscreen resized the stream the seam is measured from, so the
        // layout is read after the lock, not before it.
        const layout = await waitLayout(page, 30);
        const rects = Array.isArray(layout.rects) ? layout.rects.map(rec) : [];
        res.check(`${tag}: the locked page is given both rectangles`, rects.length === 2, layout);
        if (rects.length !== 2) return;
        const seam = Number(layout.ownW);
        const rect = rects.find((x) => x.x === seam);
        if (rect === undefined) {
            res.check(`${tag}: the neighbor is laid out at the seam`, false, layout);
            return;
        }
        const start = s.wayland ? null : await s.host.pointer();
        const travel = start ? seam + OVER - start[0] : seam;
        const steps: [number, number][] = [...Array<[number, number]>(Math.floor(travel / CROSS_STEP)).fill([CROSS_STEP, 0]), [travel % CROSS_STEP, 0]];
        const motions = steps.filter((m) => m[0]);
        let mark = (await moves(page)).length;
        for (const [dx, dy] of motions) {
            await s.desk.move(dx, dy);
            await sleep(50);
        }
        await sleep(800);
        const sent = await movesSince(page, mark);
        let wire = wireDeltas(sent);
        const positions = sent.filter((m) => m.startsWith('m,'));
        // A page that lost the lock has positions to send again, so the
        // absence of them is the crossing being a locked one. The distance
        // travels through the page's own CSS-to-stream scale, which rounds each
        // event its own way, so the count is what is owed exactly.
        const sum = wire.reduce((n, d) => n + d[0], 0);
        res.check(`${tag}: the travel goes out as one delta a move, with no position among them`, wire.length === motions.length && positions.length === 0 && Math.abs(sum - travel) <= motions.length,
            `${wire.length}/${motions.length} deltas summing ${sum} of ${travel}, positions ${r(positions.slice(0, 2))}`);
        const m = await mode(page);
        res.check(`${tag}: the lock holds with the pointer on the neighbor`, m.fullscreen && m.locked && m.gaming, m);
        if (start === null) return;
        const pos = (await s.host.pointer()) ?? [0, 0];
        res.check(`${tag}: the deltas carry the remote pointer onto the neighbor`, Number(rect.x) <= pos[0] && pos[0] < Number(rect.x) + Number(rect.w), `${r(start)} -> ${r(pos)}, neighbor at ${String(rect.x)} wide ${String(rect.w)}`);
        // A hand's worth of movement once it is there: relative input on the
        // neighbor is the other half of what the lock is holding. What the
        // pointer owes is the wire, injected verbatim, whatever the page's
        // scale made of the hand's own distance.
        mark = (await moves(page)).length;
        for (const [dx, dy] of MOVES) {
            await s.desk.move(dx, dy);
            await sleep(100);
        }
        await sleep(500);
        wire = wireDeltas(await movesSince(page, mark));
        const moved = (await s.host.pointer()) ?? [0, 0];
        const want: Pt = [pos[0] + wire.reduce((n, d) => n + d[0], 0), pos[1] + wire.reduce((n, d) => n + d[1], 0)];
        res.check(`${tag}: the pointer keeps taking the deltas while it is there`, r(moved) === r(want) && wire.length === MOVES.length, `${r(pos)} -> ${r(moved)} want ${r(want)} from ${r(wire)}`);
    } finally {
        await browser.close();
    }
}

type Enter = (res: Checks, desk: Desk, page: Page, tag: string) => Promise<boolean>;

/** The same gaming mode in the installed Firefox, which holds the keys only for a fullscreen asked for with `keyboardLock: "browser"`. */
async function inFirefox(res: Checks, s: Session, enter: Enter, at: string): Promise<void> {
    const binary = process.env.E2E_FIREFOX || onPath('firefox');
    if (!binary) {
        res.skip('firefox', 'no installed Firefox (E2E_FIREFOX or firefox on PATH)');
        return;
    }
    // The first-run notices would open over the page. A release Firefox speaks
    // WebDriver BiDi, not Juggler, wherever it was found.
    const browser = await firefox.launch({
        headless: false, executablePath: binary, channel: 'moz-firefox', args: ['--allow-downgrade'],
        firefoxUserPrefs: { 'media.autoplay.default': 0, 'full-screen-api.warning.timeout': 0, 'datareporting.policy.dataSubmissionPolicyBypassNotification': true, 'termsofuse.bypassNotification': true },
        env: { ...process.env, DISPLAY: s.desk.display },
    });
    try {
        const page = await openPage(browser, s.url, false);
        const video = await waitWsVideo(page, 30);
        res.check(`${at}firefox: video flowing`, Boolean(video), video);
        const probe = await gameWindow(res, s, `${at}firefox`);
        if (probe !== null && (await enter(res, s.desk, page, `${at}firefox`))) await played(res, s.desk, page, probe, `${at}firefox`, true);
        await probe?.stop();
        await page.context().close();
    } finally {
        await browser.close();
    }
}

async function run(host: Host, res: Checks, wayland: boolean, two: boolean, layout: string): Promise<void> {
    if (!onPath('openbox')) {
        res.skip('the suite', 'openbox is not installed, and a fullscreen browser window needs a window manager');
        return;
    }
    const chrome = process.env.E2E_CHROME || onPath('google-chrome');
    if (!chrome) {
        res.skip('the suite', 'no installed Chrome (E2E_CHROME or google-chrome on PATH) to hold a real keyboard lock');
        return;
    }
    // A layout serves that dashboard and enters by its button; the test page alone takes the chord.
    const enter: Enter = layout ? enterByButton : enterGamingMode;
    const at = layout ? `${layout} ` : '';
    const { url, pid } = await host.start({ label: layout || (wayland ? 'wl' : 'x11'), mode: 'websockets', wayland, env: layout ? { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: layout } : {} });
    let desk: Desk | null = null;
    try {
        let capture = '';
        if (wayland) {
            capture = await host.captureSocket();
            res.check('the capture compositor announced its socket', Boolean(capture), capture);
        }
        desk = await Desk.start(host);
        const s: Session = { host, desk, url, wayland, capture, runtime: runtimeDir(pid) };
        const browser = await chromium.launch({
            executablePath: chrome, headless: false,
            args: [...BROWSER_ARGS, '--window-position=0,0', `--window-size=${WINDOW[0]},${WINDOW[1]}`, '--disable-features=KeyboardAndPointerLockPrompt'],
            env: { ...process.env, DISPLAY: desk.display },
        });
        try {
            let page = await openPage(browser, url, false);
            let video = await waitWsVideo(page, 30);
            res.check(`${at}chrome: video flowing`, Boolean(video), video);
            if (two) {
                await crossed(res, s, page, 'chrome');
                await page.context().close();
                return;
            }
            const probe = await gameWindow(res, s, `${at}chrome`);
            if (probe !== null && (await enter(res, desk, page, `${at}chrome`))) await played(res, desk, page, probe, `${at}chrome`, true);
            await probe?.stop();
            await page.context().close();

            page = await openPage(browser, url, true);
            video = await waitWsVideo(page, 30);
            res.check(`${at}shields: video flowing`, Boolean(video), video);
            if (await enter(res, desk, page, `${at}shields`)) await shielded(res, desk, page, `${at}shields`);
            await page.context().close();
        } finally {
            await browser.close();
        }
        if (!two) await inFirefox(res, s, enter, at);
    } finally {
        await desk?.stop();
        await host.stop();
    }
}

for (const which of ['x11', 'wl', 'x11-two', 'wl-two', 'x11-dashboards']) {
    test(which, async () => {
        const res = new Checks(`gaming-mode-${which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            if (which === 'x11-dashboards') {
                for (const layout of LAYOUTS) await run(host, res, false, false, layout);
            } else {
                await run(host, res, which.startsWith('wl'), which.endsWith('-two'), '');
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
