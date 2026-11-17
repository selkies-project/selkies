/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// One remote pointer under a held drag, across display pages and pen contact.
//
// Every pointermove of a button-held drag keeps streaming to the page the
// press landed on, with client coordinates far past its viewport. The client
// maps such positions onto the neighboring display instead of clamping at its
// own edge, which pins the remote pointer at the seam for as long as the
// button is held. Toward an edge with no neighbor the drag still clamps, and
// the union of display rectangles bounds it everywhere, the way a multihead X
// server bounds its pointer.
//
// Where the pointer has physically reached the neighbor's window, both axes
// come from the box that page published for itself on DISPLAY_CONFIG_UPDATE:
// the two browser windows sit at their own desktop origins, and nothing about
// the grabbed page's own coordinates says where the pointer is on the other
// one. The pages here are placed on a modeled desktop -- browser windows with
// their own origins and title bars, one of which moves mid-run -- and every
// event carries both frames, the way a browser delivers them. Short of that
// box the overshoot converts at the neighbor's CSS-to-remote scale instead.
//
// The primary page runs DPR 2 with a manual resolution (the configuration
// that maps input through the presented stream box rather than the viewport)
// and the secondary runs DPR 1, so the crossing also proves the per-display
// scale conversion. Checked over websockets by watching the X server's own
// pointer, then over WebRTC for transport parity; the vertical and
// left-neighbor arrangements are driven through the same mapping on synthetic
// layouts.
//
// The held button is the second half: a page runs an `Input` of its own
// against the one remote pointer, so a drag that crosses reaches a page that
// never saw the press and leaves one that never sees the release, and a
// stylus ends contact with a cancel rather than a release. Every one of them
// carries what the event says is held.
//
// Both backends: on X11 the crossing is read back from the X pointer, and on
// the Wayland backend, which has none to read, from the coordinates and mask
// the pages put on the wire -- the client's half of it, which is where the
// mapping and the held button live.
//
// Uses E2E_DISPLAY when set; otherwise starts a throwaway Xvfb wide enough
// for the two-display union.
import { spawn, spawnSync } from 'node:child_process';

import type { Browser, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { onPath } from '../lib/desktop.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { WIRE_TAP_JS } from '../lib/page.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const PRIMARY_CSS = [1512, 806] as const; // DPR 2 -> remote 3024x1612
const SECONDARY_CSS = [1920, 936] as const; // DPR 1 -> remote 1920x936 at x=3024

// A browser window on the modeled desktop: its origin, then the chrome above its viewport.
type Win = readonly [number, number, number, number];

// The primary's browser window. The secondary's window carries a title bar
// the primary's does not, and is placed so its stream box starts where the
// primary's ends, the way Add Screen puts one against the next monitor; a
// letterboxed box puts that short of the viewport's own edge.
const PRIMARY_WINDOW: Win = [0, 0, 0, 0];
const SECONDARY_CHROME = [0, 123] as const;
// Where the run drags it afterwards: the monitor above and to the right, past
// a gap wide enough that the crossing spends some of its travel on neither.
const MOVED_OFFSET = [88, -768] as const;

// Persists the primary's manual resolution before load, as a stored user pick.
const MANUAL_INIT = `(() => {
  if (window.location.hash.startsWith('#display2')) return;
  const prefix = (window.location.origin + window.location.pathname)
    .replace(/[^a-zA-Z0-9._-]/g, '_');
  localStorage.setItem(prefix + '_manual_resolution', 'true');
  localStorage.setItem(prefix + '_manual_width', '${PRIMARY_CSS[0] * 2}');
  localStorage.setItem(prefix + '_manual_height', '${PRIMARY_CSS[1] * 2}');
  localStorage.setItem(prefix + '_scaleLocallyManual', 'true');
})()`;

// A window this page can be moved on, since the real one cannot be.
const desktopInit = (w: Win): string => `(() => {
  window.__desk = [${w[0]}, ${w[1]}];
  window.__inset = [${w[2]}, ${w[3]}];
  Object.defineProperty(window, 'screenX', {get: () => window.__desk[0]});
  Object.defineProperty(window, 'screenY', {get: () => window.__desk[1]});
})()`;

const LAYOUT_JS = 'window.webrtcInput && window.webrtcInput._layout';

// The offset between a page's client coordinates and the desktop's describes
// the page only while nothing scales one frame and not the other. Page zoom
// does, and so do screen coordinates reported in device pixels; the anchor has
// to notice rather than publish a box wrong by the pointer's own position.
const ANCHOR_JS = `(() => {
  const proto = Object.getPrototypeOf(window.webrtcInput);
  const feed = (k) => {
    const sent = [];
    const o = Object.assign(Object.create(proto), {
      send: (m) => sent.push(m),
      _layout: {own: {originX: 0, originY: 0, scaleX: 0, scaleY: 0}},
      _anchorX: null, _anchorY: null, _chromeInsetX: null, _chromeInsetY: null,
      _sampleClientX: -1e9, _sampleClientY: -1e9,
      _sampleAnchorX: NaN, _sampleAnchorY: NaN,
      _streamBox: () => ({left: 0, top: 0, scaleX: 1, scaleY: 1}),
    });
    for (let cx = 0; cx < 400; cx += 17) {
      o._noteScreenAnchor({clientX: cx, clientY: 0, screenX: 100 + k * cx, screenY: 0});
    }
    return sent.length;
  };
  return feed(1) > 0 && feed(1.5) === 0 && feed(2) === 0 && feed(0.5) === 0;
})()`;

// Drives the mapping helper itself over arrangements the session above does
// not realize: a lower neighbor at double scale, a left neighbor, two windows
// whose desktop origins share neither an edge nor a top, and two whose boxes
// overlap. Runs on a bare object, which publishes nothing, so the live page
// keeps its own layout.
const SYNTH_JS = `(() => {
  const proto = Object.getPrototypeOf(window.webrtcInput);
  const map = (layouts, own, rx, ry, sx, sy, gx, gy) => {
    const o = Object.assign(Object.create(proto),
                            {_layout: null, x: 0, y: 0, _watchScreenGeometry() {}});
    o.setDisplayLayouts(layouts, own);
    return o._mapToLayout(rx, ry, sx, sy, gx, gy) ? [o.x, o.y] : null;
  };
  const down = {primary: {x: 0, y: 0, w: 1000, h: 500, scale: 1},
                display2: {x: 0, y: 500, w: 800, h: 400, scale: 2}};
  // 40 CSS px below the seam at scale 2 -> 80 remote px into the neighbor.
  const a = map(down, 'primary', 300, 540, 1, 1);
  if (!a || a[0] !== 300 || a[1] !== 580) return false;
  // Sideways past the narrower neighbor clamps to the nearest display's last
  // pixel, here the primary's own bottom row: the row below it belongs to the
  // neighbor only as far as the neighbor is wide, and this is past that.
  const b = map(down, 'primary', 950, 540, 1, 1);
  if (!b || b[0] !== 950 || b[1] !== 499) return false;
  const left = {primary: {x: 600, y: 0, w: 1000, h: 500, scale: 2},
                display2: {x: 0, y: 0, w: 600, h: 500, scale: 1}};
  // 100 own-remote px past the left edge at own scale 2 -> 50 remote px.
  const c = map(left, 'primary', -100, 200, 2, 2);
  if (!c || c[0] !== -50 || c[1] !== 200) return false;
  // Windows 200 apart across and 400 up: the desktop position decides both
  // axes, including one the grabbed page still reads as inside its own height.
  const apart = {primary: {x: 0, y: 0, w: 1000, h: 500, scale: 1,
                           originX: 0, originY: 100, scaleX: 1, scaleY: 1},
                 display2: {x: 1000, y: 0, w: 800, h: 400, scale: 1,
                            originX: 1200, originY: -300, scaleX: 1, scaleY: 1}};
  const d = map(apart, 'primary', 1300, -150, 1, 1, 1300, -50);
  if (!d || d[0] !== 1100 || d[1] !== 250) return false;
  // Past that box -- the desktop beside the window -- the overshoot converts
  // and clamps as it does with no box published at all.
  const e = map(apart, 'primary', 2100, -150, 1, 1, 2100, -50);
  if (!e || e[0] !== 1799 || e[1] !== 0) return false;
  // Boxes that overlap on the desktop -- one window over the other, or an
  // engine reporting screen coordinates relative to its own window -- are
  // not crossed through: the overshoot converts as with no box at all.
  const over = {primary: {x: 0, y: 0, w: 1000, h: 500, scale: 1,
                          originX: 0, originY: 100, scaleX: 1, scaleY: 1},
                display2: {x: 1000, y: 0, w: 800, h: 400, scale: 1,
                           originX: 600, originY: 0, scaleX: 1, scaleY: 1}};
  const f = map(over, 'primary', 1300, 50, 1, 1, 1300, 150);
  if (!f || f[0] !== 1300 || f[1] !== 50) return false;
  // Inside its own rectangle the mapping stands aside.
  return map(down, 'primary', 500, 250, 1, 1) === null;
})()`;

type Rec = Record<string, unknown>;
const rec = (v: unknown): Rec => (v !== null && typeof v === 'object' && !Array.isArray(v) ? { ...v } : {});
const num = (v: unknown, fallback = 0): number => (typeof v === 'number' ? v : fallback);
const r = (v: unknown): string => JSON.stringify(v);
const same = (a: unknown, b: unknown): boolean => JSON.stringify(a) === JSON.stringify(b);
type Pt = [number, number];

interface Layout {
    raw: Rec;
    ownX: number;
    ownY: number;
    ownW: number;
    own: Rec;
    rects: Rec[];
}

async function layoutOf(page: Page): Promise<Layout | null> {
    const got = await within(page.evaluate(LAYOUT_JS));
    if (got === null || typeof got !== 'object') return null;
    const raw = rec(got);
    return {
        raw, ownX: num(raw.ownX), ownY: num(raw.ownY), ownW: num(raw.ownW), own: rec(raw.own),
        rects: Array.isArray(raw.rects) ? raw.rects.map(rec) : [],
    };
}

async function waitFor(pred: () => Promise<boolean>, timeoutS = 15): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (await pred()) return true;
        await sleep(250);
    }
    return false;
}

/** A display page in its own context, so each can hold its own DPR. */
async function newDisplayContext(browser: Browser, url: string, mode: string, css: readonly [number, number], dpr: number, window: Win, hash = ''): Promise<Page> {
    const ctx = await browser.newContext({ viewport: { width: css[0], height: css[1] }, deviceScaleFactor: dpr });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: WIRE_TAP_JS });
    await ctx.addInitScript({ content: MANUAL_INIT });
    await ctx.addInitScript({ content: desktopInit(window) });
    const page = await ctx.newPage();
    await page.goto(`${url}/${hash}`, { waitUntil: 'load' });
    return page;
}

/**
 * Dispatches a synthetic mouse event the way a captured drag delivers it: the
 * press on the stream overlay, everything after on the window, client
 * coordinates free to leave the viewport, and the desktop position of the
 * same point alongside them as every browser event carries. `onStream`
 * delivers to the overlay instead, which is what the page the pointer arrives
 * on gets: the browser targets whatever sits under the pointer, and on that
 * page the stream does.
 */
async function mouse(page: Page, kind: string, cx: number, cy: number, held: boolean, onStream = false): Promise<void> {
    await within(page.evaluate(`(([kind, cx, cy, held, onStream]) => {
      const ev = new MouseEvent(kind, {button: 0, buttons: held ? 1 : 0,
        clientX: cx, clientY: cy,
        screenX: window.__desk[0] + window.__inset[0] + cx,
        screenY: window.__desk[1] + window.__inset[1] + cy, bubbles: true});
      if (onStream || kind === 'mousedown') {
        document.getElementById('overlayInput').dispatchEvent(ev);
      } else {
        window.dispatchEvent(ev);
      }
    })(${r([kind, cx, cy, held, onStream])})`));
}

async function motions(page: Page): Promise<string[]> {
    const got = await within(page.evaluate("(window.__wireSent || []).filter(d => d.startsWith && d.startsWith('m,'))"));
    return Array.isArray(got) ? got.filter((m): m is string => typeof m === 'string') : [];
}

/**
 * The point the page's last motion asked for, in the layout's own space: its
 * display origin plus the coordinate it sent. What the Wayland backend is
 * read by, having no X pointer to query. It is the client's half of the
 * crossing -- the mapping under test -- and not the compositor's answer to it.
 */
async function wirePos(page: Page): Promise<Pt> {
    const sent = (await motions(page)).at(-1) ?? 'm,0,0,0,0';
    const layout = await layoutOf(page);
    const parts = sent.split(',');
    return [(layout?.ownX ?? 0) + parseInt(parts[1], 10), (layout?.ownY ?? 0) + parseInt(parts[2], 10)];
}

/** The button mask the page's last pointer message carried. */
async function wireMask(page: Page): Promise<number> {
    return parseInt(((await motions(page)).at(-1) ?? 'm,0,0,0,0').split(',')[3], 10);
}

/**
 * The buttons the page's last pointer message carried, lowest first. The
 * eraser is not among them: it rides a bit of its own that the server folds
 * onto the primary button, which the wire never shows.
 */
async function wireButtons(page: Page): Promise<number[]> {
    const mask = await wireMask(page);
    return [1, 2, 3, 4, 5].filter((b) => mask & (1 << (b - 1)));
}

class Drag {
    readonly host: Host;
    readonly wayland: boolean;
    readonly display: string | undefined;

    constructor(host: Host, wayland: boolean, display: string | undefined) {
        this.host = host;
        this.wayland = wayland;
        this.display = display;
    }

    async xPos(): Promise<Pt> {
        return (await this.host.pointer(this.display)) ?? [0, 0];
    }

    async pos(page: Page): Promise<Pt> {
        return this.wayland ? wirePos(page) : this.xPos();
    }

    /** One held move, then the pointer read back from the X server, or from the wire where there is no X pointer to read. */
    async movedTo(page: Page, cx: number, cy: number): Promise<Pt> {
        await mouse(page, 'mousemove', cx, cy, true);
        await sleep(300);
        return this.pos(page);
    }

    /** The same, hovering the stream this page shows: what the pointer reaches through the page that owns the position, with no crossing in it. */
    async hoveredTo(page: Page, cx: number, cy: number): Promise<Pt> {
        await mouse(page, 'mousemove', cx, cy, false, true);
        await sleep(300);
        return this.pos(page);
    }

    /** Buttons the remote pointer holds: from the X server, or from the last message `page` sent where there is no X pointer to ask. */
    async held(page: Page): Promise<number[]> {
        return this.wayland ? wireButtons(page) : this.host.buttonsHeld(this.display);
    }
}

/** Desktop position of a client point on `window`'s page. */
const desktop = (window: Win, cx: number, cy: number): Pt => [window[0] + window[2] + cx, window[1] + window[3] + cy];
/** Client point on `window`'s page for a desktop position. */
const clientOn = (window: Win, gx: number, gy: number): Pt => [gx - window[0] - window[2], gy - window[1] - window[3]];

/** Moves the browser window this page runs in, chrome and all. Nothing on the page hears about it: the box it publishes is what carries the move. */
async function placeWindow(page: Page, x: number, y: number): Promise<void> {
    await within(page.evaluate(`window.__desk = [${x}, ${y}]`));
}

/** The layout rectangle `page` holds for the display across the seam. */
async function neighborRect(page: Page, seam: number): Promise<Rec> {
    return (await layoutOf(page))?.rects.find((rect) => rect.x === seam) ?? {};
}

const waitVideo = (page: Page, mode: string, timeoutS = 45): Promise<unknown> => (mode === 'webrtc' ? waitWrVideo(page, timeoutS) : waitWsVideo(page, timeoutS));

/** Dispatches one stylus pointer event on the stream overlay. */
async function pen(page: Page, kind: string, button: number, buttons: number, cx: number, cy: number): Promise<void> {
    await within(page.evaluate(`(([kind, button, buttons, cx, cy]) => {
      document.getElementById('overlayInput').dispatchEvent(new PointerEvent(kind, {
        pointerType: 'pen', pointerId: 3, isPrimary: true, button, buttons,
        clientX: cx, clientY: cy,
        screenX: window.__desk[0] + window.__inset[0] + cx,
        screenY: window.__desk[1] + window.__inset[1] + cy,
        bubbles: true, cancelable: true}));
    })(${r([kind, button, buttons, cx, cy])})`));
    await sleep(300);
}

/**
 * A stylus drives the same one pointer, and its contact can end unsaid.
 * Contact reaches the mouse path, but the browser can end it with a cancel
 * carrying button -1 and nothing held, or simply stop reporting the tip on
 * the next move. Either way what the event says is held is what is left, and
 * the eraser presses as the tip does, since neither X nor wl_pointer has a
 * button for it.
 */
async function penContact(res: Checks, mode: string, page: Page, d: Drag): Promise<void> {
    await pen(page, 'pointerdown', 0, 1, 700, 400);
    let held = await d.held(page);
    res.check(`[${mode}] pen contact presses`, same(held, [1]), held);
    await pen(page, 'pointermove', -1, 0, 720, 400);
    held = await d.held(page);
    res.check(`[${mode}] contact the page never saw end is not still held`, held.length === 0, held);
    await pen(page, 'pointerdown', 0, 1, 700, 400);
    await pen(page, 'pointercancel', -1, 0, 700, 400);
    held = await d.held(page);
    res.check(`[${mode}] a cancel releases with no pointerup to follow`, held.length === 0, held);

    await pen(page, 'pointerdown', 5, 32, 700, 400);
    if (d.wayland) {
        // Only the fold onto the primary button makes the eraser press, and
        // it happens where the pointer is injected; the wire carries the bit.
        const mask = await wireMask(page);
        res.check(`[${mode}] the eraser reaches the server on its own bit`, mask === 32, mask);
    } else {
        held = await d.held(page);
        res.check(`[${mode}] the eraser presses like the tip`, same(held, [1]), held);
    }
    await pen(page, 'pointerup', 5, 0, 700, 400);
    if (d.wayland) {
        const mask = await wireMask(page);
        res.check(`[${mode}] the eraser lifts`, mask === 0, mask);
    } else {
        held = await d.held(page);
        res.check(`[${mode}] the eraser lifts`, held.length === 0, held);
    }
}

/**
 * The press lands on one page and everything after it on the neighbor. What
 * the browser does once the pointer crosses into the other window: it
 * delivers there instead, so the page that owns the grab stops hearing about
 * the drag and the neighbor hears about it having missed the press. The
 * neighbor holds an `Input` of its own whose mask starts empty, and a move
 * reporting that mask would release the button under the window being
 * dragged. It carries what the event says is held instead, and the page left
 * behind drops its own stale mask rather than pressing again on the next
 * hover.
 */
async function handoff(res: Checks, mode: string, page: Page, dpage: Page, seam: number, d: Drag): Promise<void> {
    await mouse(page, 'mousedown', 700, 400, true);
    await sleep(300);
    let held = await d.held(page);
    res.check(`[${mode}] the press lands`, same(held, [1]), held);
    await within(dpage.evaluate('window.__wireSent.length = 0'));
    for (const cx of [300, 700]) {
        await mouse(dpage, 'mousemove', cx, 500, true, true);
        await sleep(300);
        const pos = await d.pos(dpage);
        held = await d.held(dpage);
        res.check(`[${mode}] the drag stays held on the page it crossed to`, same(held, [1]) && pos[0] > seam, `${r(held)} ${r(pos)} seam=${seam}`);
    }
    const carried = (await motions(dpage)).filter((m) => m.split(',')[3] === '1');
    res.check(`[${mode}] the neighbor's own wire carries the held button`, carried.length >= 2, carried.slice(0, 2));

    await mouse(dpage, 'mouseup', 700, 500, false, true);
    await sleep(300);
    held = await d.held(dpage);
    res.check(`[${mode}] the only release, on the page that ends the drag`, held.length === 0, held);

    await mouse(page, 'mousemove', 700, 400, false, true);
    await sleep(300);
    held = await d.held(page);
    res.check(`[${mode}] the page left behind does not press again`, held.length === 0, held);
}

/**
 * A window the session's window manager owns follows a held drag across the
 * seam. The crossing checked above is the client's half of the gesture. What
 * a user sees is the window under the grab, which the window manager moves
 * itself and which has to travel exactly as far as the pointer -- across the
 * seam, where the overshoot converts at the neighbor's scale rather than the
 * grabbed page's, so equal client steps are not equal remote ones.
 *
 * The window manager started here already has both displays, so a monitor
 * set it never re-read is not what this exercises; that lives in the monitor
 * change integration suite, at the layer the re-read happens on.
 *
 * X11 only: on the Wayland backend the session compositor owns the window and
 * the seam is its own (`crates/pixelflux/tests/wayland_seam.rs`).
 */
async function managedWindowDrag(res: Checks, mode: string, page: Page, seam: number, d: Drag, toClient: (x: number, y: number) => Pt): Promise<void> {
    const name = `[${mode}] a managed window follows the drag across the seam`;
    const wm = ['openbox', 'xfwm4'].find((w) => onPath(w) !== null);
    if (wm === undefined || onPath('xdotool') === null) {
        res.skip(name, 'no window manager or xdotool on PATH');
        return;
    }
    const display = d.display ?? process.env.E2E_DISPLAY ?? '';
    const env = { ...process.env, DISPLAY: display };
    const proc = spawn(wm, ['--replace'], { env, stdio: 'ignore' });
    let probe: Awaited<ReturnType<Host['managedProbe']>> | null = null;
    try {
        probe = await d.host.managedProbe('selkies-drag-probe', d.display);
        // The reparent is what says a window manager took it; without one
        // there is no frame to grab and nothing this check could mean.
        let frame: [number, number, number, number] | null = null;
        for (const until = Date.now() + 15_000; Date.now() < until && frame === null;) {
            frame = await probe.frame();
            if (frame === null) await sleep(250);
        }
        if (frame === null) {
            res.skip(name, `${wm} did not take the probe window`);
            return;
        }
        // Placed so the grab point starts inside the primary and the travel
        // below carries it well past the seam.
        spawnSync('xdotool', ['windowmove', String(probe.id), String(seam - 500), '200'], { env, encoding: 'utf8' });
        await sleep(600);
        const before = (await probe.frame()) ?? frame;
        // A point on the frame's own titlebar, above the client area.
        const grab: Pt = [before[0] + 60, before[1] + Math.max(2, Math.floor((before[3] - 260) / 2))];
        // What the window has to match is the pointer, not the client-side
        // request: past the seam the overshoot converts at the neighbor's own
        // scale, so equal client steps are not equal remote ones.
        const travel = 800;
        let last: Pt = toClient(...grab);
        await mouse(page, 'mousedown', ...last, true);
        await sleep(300);
        const pointerFrom = await d.xPos();
        for (let step = 100; step <= travel; step += 100) {
            last = toClient(grab[0] + step, grab[1]);
            await mouse(page, 'mousemove', ...last, true);
            await sleep(120);
        }
        await sleep(400);
        const pointerTo = await d.xPos();
        await mouse(page, 'mouseup', ...last, false);
        await sleep(600);
        const after = (await probe.frame()) ?? before;
        const moved = after[0] - before[0];
        const carried = pointerTo[0] - pointerFrom[0];
        res.check(name, Math.abs(moved - carried) <= 12 && pointerTo[0] > seam && seam > pointerFrom[0],
            `${wm}: window moved ${moved}, pointer ${carried} (${pointerFrom[0]} -> ${pointerTo[0]}), seam=${seam}`);
    } finally {
        await probe?.release();
        proc.kill('SIGTERM');
        const timer = setTimeout(() => proc.kill('SIGKILL'), 10_000);
        if (proc.exitCode === null && proc.signalCode === null) await new Promise((resolve) => { proc.once('exit', resolve); });
        clearTimeout(timer);
    }
}

/** The scale the server last realized for `displayId`, from its own log; null where the log carries none for it. */
function realizedScale(displayId: string, log: string): number | null {
    const hits = [...log.matchAll(new RegExp(`realized geometry for '${displayId}': \\d+x\\d+ @ scale ([0-9.]+)`, 'g'))];
    const last = hits.at(-1);
    return last ? parseFloat(last[1]) : null;
}

const tail = (log: string, n: number): string => log.trimEnd().split('\n').slice(-n).join('\n');

/**
 * A secondary restored on a screen of another density: on X11 it is refused
 * the desktop's DPI, on Wayland its own screen takes it. On X11 the desktop
 * has one DPI and the primary's page owns it; a secondary shown at another
 * device pixel ratio re-derives its own on every restore, and the WebRTC core
 * sends that re-derivation as the bare sync verb, off the SETTINGS path the
 * rule was first put on. Driven by changing the page's device pixel ratio
 * under it, which is what moving its window between such screens does.
 */
async function secondaryDensity(res: Checks, mode: string, dpage: Page, d: Drag): Promise<void> {
    const host = d.host;
    const mark = (await host.log()).length;
    const cdp = await dpage.context().newCDPSession(dpage);
    await cdp.send('Emulation.setDeviceMetricsOverride', { width: SECONDARY_CSS[0], height: SECONDARY_CSS[1], deviceScaleFactor: 2, mobile: false });
    if (d.wayland) {
        // The scale the server realized for each display, which is what a
        // Wayland session carries per screen: the nested compositor's own
        // `scaled to` line is printed only where there is one to nest in.
        const before = realizedScale('primary', await host.log());
        const scaled = await waitFor(async () => realizedScale('display2', (await host.log()).slice(mark)) === 2.0, 20);
        res.check(`[${mode}] a secondary rederiving its density scales its own screen`, scaled, tail(await host.log(), 3));
        await sleep(2000);
        const now = realizedScale('primary', (await host.log()).slice(mark));
        res.check(`[${mode}] and the primary's screen keeps its scale`, now === null || now === before, `was ${before}, now ${now}`);
    } else {
        const refused = await waitFor(async () => (await host.log()).indexOf("Ignoring DPI 192 from 'display2'", mark) >= 0, 15);
        res.check(`[${mode}] a secondary rederiving its density is refused`, refused, tail(await host.log(), 3));
        await sleep(2000);
        res.check(`[${mode}] and the desktop is not rescaled for it`, (await host.log()).indexOf('set DPI to 192', mark) < 0, '');
    }
    await cdp.detach();
}

/** One transport's crossing checks; the full set runs on websockets. */
async function drive(res: Checks, host: Host, mode: string, wayland: boolean, display: string | undefined): Promise<void> {
    const full = mode === 'websockets';
    const d = new Drag(host, wayland, display);
    const { url } = await host.start({ label: mode, mode, wayland, ...(display ? { display } : {}) });
    const browser = await launchChromium();
    try {
        const page = await newDisplayContext(browser, url, mode, PRIMARY_CSS, 2, PRIMARY_WINDOW);
        res.check(`[${mode}] primary video flows`, Boolean(await waitVideo(page, mode)), '');

        if (full) {
            // Alone, the page has no multi-display layout and the wire is what
            // it always was: unclamped window math or the sink clamp.
            res.check('single display keeps no layout', (await layoutOf(page)) === null, '');
        }

        const dpage = await newDisplayContext(browser, url, mode, SECONDARY_CSS, 1, [PRIMARY_CSS[0], -SECONDARY_CHROME[1], ...SECONDARY_CHROME], '#display2-right');
        res.check(`[${mode}] secondary video flows`, Boolean(await waitVideo(dpage, mode)), '');
        // The layout is awaited at the secondary's own density, not merely at two rectangles.
        await waitFor(async () => {
            const l = await layoutOf(page);
            return l !== null && l.rects.length === 2 && l.rects.some((rect) => rect.x === l.raw.ownW && rect.scale === 1 && rect.w === SECONDARY_CSS[0]);
        }, 30);
        const layout = await layoutOf(page);
        const gotLayout = layout !== null && layout.rects.length === 2;
        res.check(`[${mode}] both rectangles reach the grabbed page`, gotLayout, layout?.raw ?? {});
        if (!gotLayout) return;
        const seam = layout.ownW;
        const unionR = Math.max(...layout.rects.map((rect) => num(rect.x) + num(rect.w)));
        const d2 = layout.rects.find((rect) => rect.x === seam);
        res.check(`[${mode}] secondary laid out at the seam at its own density`, d2 !== undefined && d2.scale === 1 && d2.w === SECONDARY_CSS[0], layout.raw);

        // Each page anchors itself on two events that agree about where its
        // viewport sits, then publishes the box it draws the stream in; the
        // grabbed page maps a crossing through the neighbor's.
        for (const pg of [page, dpage]) {
            await d.hoveredTo(pg, 700, 400);
            await d.hoveredTo(pg, 760, 430);
        }
        const gotBox = await waitFor(async () => (await neighborRect(page, seam)).scaleX === 1, 15);
        const ownBox = (await layoutOf(page))?.own ?? {};
        res.check(`[${mode}] the neighbor publishes the box it draws in`, gotBox && num(ownBox.scaleX) > 0, `own=${r(ownBox)} neighbor=${r(await neighborRect(page, seam))}`);
        if (!gotBox || !num(ownBox.scaleX)) return;

        // Windows edge to edge: the secondary's box starts where the
        // primary's ends, its own title bar accounted for.
        const boxLeft = Math.round(num(ownBox.originX) + num(ownBox.w) / num(ownBox.scaleX));
        const boxTop = Math.round(num(ownBox.originY));
        const second: Win = [boxLeft - SECONDARY_CHROME[0], boxTop - SECONDARY_CHROME[1], ...SECONDARY_CHROME];
        await placeWindow(dpage, second[0], second[1]);
        const placed = await waitFor(async () => {
            const n = await neighborRect(page, seam);
            return n.originX === boxLeft && n.originY === boxTop;
        }, 10);
        res.check(`[${mode}] a window the user moves republishes its box`, placed, `box at ${boxLeft},${boxTop}: ${r(await neighborRect(page, seam))}`);

        // The held drag: press well inside, then cross the right edge. The
        // page's own live mapping (measured from two in-bounds samples)
        // locates the edge in client coordinates, so the letterboxed WebRTC
        // video box maps as exactly as the websockets canvas.
        await mouse(page, 'mousedown', 700, 400, true);
        const p1 = await d.movedTo(page, 700, 400);
        const p2 = await d.movedTo(page, 1100, 400);
        const ownScale = (p2[0] - p1[0]) / 400;
        res.check(`[${mode}] in-bounds motion maps linearly`, ownScale > 0, `${r(p1)} ${r(p2)}`);
        if (ownScale <= 0) return;
        const edge = Math.round((700 - p1[0] / ownScale) + seam / ownScale);
        if (full) {
            res.check('manual DPR-2 mapping is exact', Math.abs(p1[0] - 1400) <= 2 && Math.abs(p1[1] - 800) <= 2 && edge === PRIMARY_CSS[0], `${r(p1)} edge=${edge}`);
        }
        const d2Scale = num((await neighborRect(page, seam)).scaleX, NaN);
        const over = await d.movedTo(page, edge + 300, 400);
        const far = await d.movedTo(page, edge + 1088, 400);
        res.check(`[${mode}] held drag crosses the seam`, over[0] > seam, `${r(over)} seam=${seam}`);
        res.check(`[${mode}] overshoot travels at the neighbor's scale`, Math.abs(over[0] - (seam + 300 * d2Scale)) <= 4 && Math.abs(far[0] - (seam + 1088 * d2Scale)) <= 4,
            `${r(over)} ${r(far)} seam=${seam} scale=${d2Scale}`);

        if (full) {
            const clamped = await d.movedTo(page, edge + 3000, 400);
            res.check("far overshoot clamps at the union's edge", Math.abs(clamped[0] - (unionR - 1)) <= 1, `${r(clamped)} union=${unionR}`);
            // A neighbor shorter than this display leaves a corner that
            // belongs to neither, and the desktop has no pixel there: the
            // crossing lands on the last pixel of a display, whichever.
            const low = await d.movedTo(page, 2000, 1000);
            const inside = layout.rects.filter((rect) => num(rect.x) <= low[0] && low[0] < num(rect.x) + num(rect.w) && num(rect.y) <= low[1] && low[1] < num(rect.y) + num(rect.h));
            res.check('a drag into the corner beside a shorter neighbor stays on a display', inside.length > 0, `${r(low)} rects=${r(layout.rects)}`);
            const left = await d.movedTo(page, -300, 400);
            res.check('an edge with no neighbor still clamps', left[0] === 0, left);
        }

        const end = await d.movedTo(page, edge + 1088, 400);
        await mouse(page, 'mouseup', edge + 1088, 400, false);
        await sleep(300);
        const released = await d.pos(page);
        res.check(`[${mode}] release lands across the seam`, released[0] > seam && Math.abs(released[0] - end[0]) <= 2, `end=${r(end)} released=${r(released)}`);

        // The pointer is physically over the neighbor's window by now, so the
        // crossing has to reach the pixel that page shows there. Read through
        // its own mapping, which has no crossing in it.
        let spot = desktop(PRIMARY_WINDOW, edge + 1088, 400);
        let neighbor = await d.hoveredTo(dpage, ...clientOn(second, ...spot));
        res.check(`[${mode}] a crossing lands where the neighbor puts the same spot`, Math.abs(neighbor[0] - end[0]) <= 1 && Math.abs(neighbor[1] - end[1]) <= 1,
            `drag=${r(end)} neighbor=${r(neighbor)} desktop=${r(spot)}`);
        const sent = await motions(page);
        const crossing = sent.filter((m) => parseInt(m.split(',')[1], 10) > seam);
        res.check(`[${mode}] the wire itself carries the crossing`, crossing.length > 0 && crossing[crossing.length - 1].split(',')[3] === '0', crossing.length ? crossing.slice(-1) : sent.slice(-3));

        await handoff(res, mode, page, dpage, seam, d);
        await penContact(res, mode, page, d);

        if (full) {
            // The window dragged onto a monitor of its own, above and right of
            // where it was. The pages share no edge and no top now, and the
            // grabbed page's own coordinates say nothing about where the
            // pointer is on the other one: only the published box does.
            const moved: Win = [second[0] + MOVED_OFFSET[0], second[1] + MOVED_OFFSET[1], ...SECONDARY_CHROME];
            await placeWindow(dpage, moved[0], moved[1]);
            const movedOk = await waitFor(async () => (await neighborRect(page, seam)).originY === boxTop + MOVED_OFFSET[1], 10);
            res.check('a window moved with no pointer over it republishes its box', movedOk, `${r(moved)} ${r(await neighborRect(page, seam))}`);
            spot = desktop(moved, 400, 845);
            await mouse(page, 'mousedown', 700, 400, true);
            const crossed = await d.movedTo(page, ...clientOn(PRIMARY_WINDOW, ...spot));
            await mouse(page, 'mouseup', ...clientOn(PRIMARY_WINDOW, ...spot), false);
            await sleep(300);
            neighbor = await d.hoveredTo(dpage, ...clientOn(moved, ...spot));
            res.check('a crossing onto a window at its own desktop origin lands there', Math.abs(crossed[0] - neighbor[0]) <= 1 && Math.abs(crossed[1] - neighbor[1]) <= 1,
                `drag=${r(crossed)} neighbor=${r(neighbor)} desktop=${r(spot)}`);

            res.check('vertical and left arrangements map the same way', (await within(page.evaluate(SYNTH_JS))) === true, 'see SYNTH_JS');
            res.check('a client frame that scales against the desktop publishes nothing', (await within(page.evaluate(ANCHOR_JS))) === true, 'see ANCHOR_JS');
        }

        if (!wayland) {
            await managedWindowDrag(res, mode, page, seam, d, (sx, sy) => [Math.round(700 + (sx - p1[0]) / ownScale), Math.round(400 + (sy - p1[1]) / ownScale)]);
        }
        await secondaryDensity(res, mode, dpage, d);
    } finally {
        await browser.close();
        await host.stop();
    }
}

for (const backend of ['x11', 'wl']) {
    test(backend, async () => {
        const wayland = backend === 'wl';
        const res = new Checks(`cross-display-drag-${backend}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const xvfb = process.env.E2E_DISPLAY ? null : await host.xvfb(8192, 4096);
            try {
                for (const mode of ['websockets', 'webrtc']) await drive(res, host, mode, wayland, xvfb?.display);
            } finally {
                await xvfb?.release();
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
