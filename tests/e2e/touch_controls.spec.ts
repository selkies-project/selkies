/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Touch clients' own controls in every layout: the special-key palette and
// the trackpad's speed, end to end.
//
// The palette, beside the soft modifier keys a touch client gets, holds the
// keys an on-screen keyboard lacks, a few chords, and chords the user adds and
// keeps. Each key or chord has to reach the session as those keys, pressed in
// order and released in reverse, a modifier held on a soft key has to stay
// held across a chord, and a chord the user added has to be there after a
// reload. On phone viewports, portrait and landscape, in direct touch and in
// trackpad mode, the palette's toggle has to stay in the viewport and take a
// click with the palette open or closed, and its keys and field have to take
// one too. The keys are read back from the X server, and on Wayland from the
// seat.
//
// Trackpad travel is accelerated by the finger's speed: a slow drag moves the
// pointer as far as the finger went, a fast one further, and the speed picked
// in the dashboard scales it and is kept. Every block runs in Chromium,
// Firefox, and WebKit on both transports: Chromium driven through CDP touch,
// Firefox and WebKit through synthetic touches.
import type { BrowserContext, Locator, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { firefoxPersistentContext, launchBrowser, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { barTool } from '../lib/dashboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host, monotonic, type Observer } from '../lib/host.ts';
import { PC_TAP_JS } from '../lib/page.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';
import { FIREFOX_TOUCH_PREFS, Fingers, XWatcher } from '../lib/touch.ts';

const LAYOUTS = ['default', 'wish', 'edge'] as const;
const ENGINES = ['chromium', 'firefox', 'webkit'] as const;
const XK: Record<string, number> = { F5: 0xFFC2, Tab: 0xFF09, Alt_L: 0xFFE9, Control_L: 0xFFE3, Shift_L: 0xFFE1, t: 0x74, Delete: 0xFFFF };

// Phone viewports, portrait and landscape: the palette has to stay in reach on them.
const PHONES = [[390, 844], [844, 390]] as const;

const RECT_JS = (sel: string): string => `((sel) => {
  const el = document.querySelector(sel);
  if (!el) return null;
  const r = el.getBoundingClientRect();
  const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
  return {x: Math.round(r.left), y: Math.round(r.top), w: Math.round(r.width), h: Math.round(r.height),
          inside: r.left >= 0 && r.top >= 0 && r.right <= innerWidth && r.bottom <= innerHeight,
          on_top: !!hit && (hit === el || el.contains(hit))};
})(${JSON.stringify(sel)})`;

const VK_LABEL = 'Virtual Keyboard';
// navigator.virtualKeyboard's show and hide, counted (the engine's own keyboard is the OS's, not the page's).
const VK_SPY = `(() => { const vk = navigator.virtualKeyboard; window.__vk = { shown: 0, hidden: 0 };
  if (vk) { vk.show = () => { window.__vk.shown++; }; vk.hide = () => { window.__vk.hidden++; }; } })()`;
// The event each focus of the hidden field ran within (`window.event`): iOS
// WebKit raises its keyboard only for a focus made while the page handles the
// user's own tap, so a later task's focus leaves it down.
const FOCUS_SPY = `(() => { window.__assistFocus = [];
  if (window.__assistFocusSpied) return;
  window.__assistFocusSpied = true;
  const real = HTMLElement.prototype.focus;
  HTMLElement.prototype.focus = function (...args) {
    if (this.id === 'keyboard-input-assist') {
      const e = window.event;
      window.__assistFocus.push(e ? (e.isTrusted ? 'trusted ' : '') + e.type : 'no event');
    }
    return real.apply(this, args);
  }; })()`;
const VK_READ = `(() => { const f = document.getElementById('keyboard-input-assist');
  return { ...window.__vk, api: !!navigator.virtualKeyboard, focused: document.activeElement === f,
           policy: f && f.getAttribute('virtualkeyboardpolicy'), within: window.__assistFocus }; })()`;

// A convertible's primary pointer, which Edge on a Surface turns fine when
// the keyboard and its touchpad are attached and coarse when they are
// detached: `__setPointerFine` flips it the same way, with the change events
// Edge sends, and `__setPointerFineSilently` without them, as an attach Edge
// does not announce.
const POINTER_POSTURE_JS = `
(() => {
  const real = window.matchMedia.bind(window);
  let fine = true;
  const lists = new Map();
  for (const [query, test] of [['(pointer: fine)', () => fine], ['(pointer: coarse)', () => !fine]]) {
    const listeners = new Set();
    lists.set(query, {
      media: query,
      get matches() { return test(); },
      onchange: null,
      addEventListener(type, fn) { if (type === 'change') listeners.add(fn); },
      removeEventListener(type, fn) { listeners.delete(fn); },
      addListener(fn) { listeners.add(fn); },
      removeListener(fn) { listeners.delete(fn); },
      dispatchEvent() { return true; },
      fire() { for (const fn of listeners) fn({ matches: test(), media: query }); },
    });
  }
  window.matchMedia = (query) => lists.get(String(query).trim()) || real(query);
  Object.defineProperty(Navigator.prototype, 'maxTouchPoints', { get: () => 10, configurable: true });
  window.__setPointerFine = (to) => { fine = to; for (const list of lists.values()) list.fire(); };
  window.__setPointerFineSilently = (to) => { fine = to; };
})();
`;

const js = (page: Page, code: string): Promise<unknown> => within(page.evaluate(code));
const r = (v: unknown): string => JSON.stringify(v);
const same = (a: unknown, b: unknown): boolean => JSON.stringify(a) === JSON.stringify(b);
const waitVideo = (page: Page, mode: string): Promise<unknown> => (mode === 'websockets' ? waitWsVideo(page, 40) : waitWrVideo(page, 60));
const dashboardEnv = (layout: string): Record<string, string> => ({ SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: layout });

interface Client {
    ctx: BrowserContext;
    page: Page;
    close: () => Promise<void>;
}

async function openClient(engine: Engine, url: string, mode: string, initJs = ''): Promise<Client> {
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
    if (initJs) await ctx.addInitScript({ content: initJs });
    // So a <video> that never plays can be told from a stream that never came.
    if (mode === 'webrtc') await ctx.addInitScript({ content: PC_TAP_JS });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    if (engine === 'firefox') {
        // Firefox runs on one persistent profile, so the touch mode, the
        // trackpad speed, and the chords an earlier block left would carry
        // into this one: each block starts from none of them.
        await js(page, `(() => {
          for (const k of Object.keys(localStorage)) {
            if (/_(trackpadMode|trackpad_speed|user_chords)$/.test(k)) localStorage.removeItem(k);
          }
        })()`);
        await page.goto(`${url}/`, { waitUntil: 'load' });
    }
    return { ctx, page, close };
}

/** One finger down and up on the stream, which is how a dashboard learns the screen takes touch and offers its touch controls. */
async function touchOnce(client: Client, engine: Engine): Promise<void> {
    const f = await Fingers.on(client.page, client.ctx, engine);
    await f.down([1, 900, 600]);
    await sleep(50);
    await f.up(1);
    await sleep(800);
}

/** A bar layout keeps its soft keys shut until the bar's button opens them. */
async function openSoftKeys(page: Page, dashboard: string): Promise<void> {
    if (dashboard === 'default' || (await page.locator('[data-soft-keys]').count())) return;
    const button = await barTool(page, '.soft-keys-button');
    if (button !== null) {
        await button.click();
        await sleep(600);
    }
}

const sidebarIsOpen = async (page: Page): Promise<boolean> => (await js(page, "!!document.querySelector('.sidebar.is-open')")) === true;

async function openSidebar(page: Page): Promise<void> {
    if (!(await sidebarIsOpen(page))) {
        await js(page, "window.postMessage({type: 'toggleDashboard'}, window.location.origin)");
        await sleep(800);
    }
}

async function closeSidebar(page: Page): Promise<void> {
    if (await sidebarIsOpen(page)) {
        await js(page, "window.postMessage({type: 'toggleDashboard'}, window.location.origin)");
        await sleep(800);
    }
}

async function openPalette(page: Page, dashboard: string): Promise<boolean> {
    if (dashboard === 'default') await openSidebar(page);
    await openSoftKeys(page, dashboard);
    const toggle = page.locator('.key-palette-toggle').first();
    if (!(await toggle.count())) return false;
    if ((await toggle.getAttribute('aria-expanded')) !== 'true') {
        await toggle.click();
        await sleep(400);
    }
    return (await page.locator("[data-code='F5']").count()) > 0;
}

type Key = [boolean, string];

/** The key events since `t`, as (press, keysym name) for the keycodes of interest. */
async function keysSince(watcher: XWatcher, t: number, codes: ReadonlyMap<number, string>): Promise<Key[]> {
    return (await watcher.since(t)).filter((e) => (e.kind === 'KeyPress' || e.kind === 'KeyRelease') && codes.has(e.detail))
        .map((e): Key => [e.kind === 'KeyPress', codes.get(e.detail) ?? '']);
}

async function keycodes(host: Host): Promise<Map<number, string>> {
    const out = new Map<number, string>();
    for (const [name, sym] of Object.entries(XK)) {
        const code = await host.keycode(sym);
        if (code !== null) out.set(code, name);
    }
    return out;
}

async function paletteBlock(host: Host, res: Checks, dashboard: string, engine: Engine, mode: string): Promise<void> {
    const tag = `${dashboard} ${engine} ${mode}`;
    const { url } = await host.start({ label: `palette-${dashboard}-${engine}-${mode}`, mode, env: dashboardEnv(dashboard) });
    let watcher: XWatcher | null = null;
    let client: Client | null = null;
    try {
        client = await openClient(engine, url, mode);
        const { page } = client;
        const video = await waitVideo(page, mode);
        res.check(`${tag}: video up`, video !== null, video);
        if (video === null) return;
        await touchOnce(client, engine);
        const opened = await openPalette(page, dashboard);
        res.check(`${tag}: a touch client is offered the key palette`, opened);
        if (!opened) return;
        watcher = await XWatcher.start(host);
        const codes = await keycodes(host);
        await sleep(300);

        let t0 = monotonic();
        await page.locator("[data-code='F5']").first().click();
        await sleep(600);
        let got = await keysSince(watcher, t0, codes);
        res.check(`${tag}: the palette's F5 reaches the session`, same(got, [[true, 'F5'], [false, 'F5']]), got);

        t0 = monotonic();
        await page.locator("[data-chord='Alt+Tab']").first().click();
        await sleep(600);
        got = await keysSince(watcher, t0, codes);
        res.check(`${tag}: Alt+Tab reaches it pressed in order and released in reverse`, same(got, [[true, 'Alt_L'], [true, 'Tab'], [false, 'Tab'], [false, 'Alt_L']]), got);

        const field = page.locator('.key-palette-input').first();
        const tTyped = monotonic();
        await field.click();
        await field.pressSequentially('ctrl+shift+t', { delay: 20 });
        await field.press('Enter');
        await sleep(500);
        const leaked = (await watcher.since(tTyped)).filter((e) => e.kind === 'KeyPress' || e.kind === 'KeyRelease');
        res.check(`${tag}: typing a chord into the palette sends nothing to the session`, leaked.length === 0, leaked.slice(0, 4));
        const added = page.locator("[data-chord='Ctrl+Shift+T']");
        const addedCount = await added.count();
        res.check(`${tag}: a chord the user writes is added, written back one way`, addedCount === 1, addedCount);
        t0 = monotonic();
        if (addedCount) await added.first().click();
        await sleep(600);
        got = await keysSince(watcher, t0, codes);
        res.check(`${tag}: and plays as its keys`, same(got, [[true, 'Control_L'], [true, 'Shift_L'], [true, 't'], [false, 't'], [false, 'Shift_L'], [false, 'Control_L']]), got);

        // CTL held on its soft key, then the palette's Del: Ctrl+Del, and CTL still held.
        const ctl = page.locator('text=/^CTR?L$/').first();
        t0 = monotonic();
        await ctl.click();
        await sleep(300);
        await page.locator("[data-code='Delete']").first().click();
        await sleep(600);
        const mid = await keysSince(watcher, t0, codes);
        await ctl.click();
        await sleep(400);
        got = await keysSince(watcher, t0, codes);
        res.check(`${tag}: a palette key under a held soft modifier goes out with it`,
            same(mid, [[true, 'Control_L'], [true, 'Delete'], [false, 'Delete']]) && same(got.at(-1), [false, 'Control_L']), got);

        await page.reload({ waitUntil: 'load' });
        await waitVideo(page, mode);
        await touchOnce(client, engine);
        await openPalette(page, dashboard);
        res.check(`${tag}: the user's chord is still there after a reload`, (await page.locator("[data-chord='Ctrl+Shift+T']").count()) === 1);
        for (const [width, height] of PHONES) {
            await page.setViewportSize({ width, height });
            await sleep(1500);
            for (const trackpad of [false, true]) {
                const how = trackpad ? 'trackpad mode' : 'direct touch';
                const [ok, detail] = await inReach(page, dashboard, trackpad);
                res.check(`${tag}: at ${width}x${height} in ${how}, the palette's toggle and keys are in reach`, ok, detail);
            }
        }
    } finally {
        await client?.close();
        await watcher?.close();
        await host.stop();
    }
}

async function visible(button: Locator): Promise<boolean> {
    return (await button.count()) > 0 && (await button.first().isVisible());
}

interface VkRead {
    shown: number;
    hidden: number;
    api: boolean;
    focused: boolean;
    policy: string | null;
    within: string[];
}

async function vkRead(page: Page): Promise<VkRead> {
    const got = await js(page, VK_READ);
    const f = got !== null && typeof got === 'object' ? got : {};
    const field = (k: string): unknown => (k in f ? Reflect.get(f, k) : undefined);
    const list = field('within');
    return {
        shown: Number(field('shown') ?? 0), hidden: Number(field('hidden') ?? 0), api: field('api') === true, focused: field('focused') === true,
        policy: typeof field('policy') === 'string' ? String(field('policy')) : null, within: Array.isArray(list) ? list.map(String) : [],
    };
}

/**
 * A tablet's attached keyboard keeps the system's on-screen one down, so the
 * button that pops it goes once a key only a keyboard has is pressed, not on a
 * letter, which an on-screen keyboard types too; each layout's own keyboard
 * control brings it back (the sidebar's tile, a bar layout's button).
 */
async function keyboardButtonBlock(host: Host, res: Checks, dashboard: string, engine: Engine): Promise<void> {
    const tag = `${dashboard} ${engine}`;
    const { url } = await host.start({ label: `keyboard-${dashboard}-${engine}`, mode: 'websockets', env: dashboardEnv(dashboard) });
    let client: Client | null = null;
    try {
        client = await openClient(engine, url, 'websockets');
        const { page } = client;
        const video = await waitWsVideo(page, 40);
        res.check(`${tag}: video up`, video !== null, video);
        if (video === null) return;
        await touchOnce(client, engine);
        const button = page.locator('[data-virtual-keyboard-button]');
        const shown = await visible(button);
        if (!shown) {
            // The dashboard offers it by `(pointer: coarse)`, which a
            // touch-emulating desktop engine may not report.
            process.stdout.write(`[${tag}] no keyboard button offered on this engine's emulation\n`);
            return;
        }
        res.check(`${tag}: a touch client is offered the keyboard button`, shown);
        await page.keyboard.press('a');
        await sleep(400);
        res.check(`${tag}: a letter leaves it, as an on-screen keyboard's would`, await visible(button));
        await page.keyboard.press('Escape');
        await sleep(400);
        res.check(`${tag}: a key only a keyboard has takes it away`, !(await visible(button)));
        if (dashboard === 'default') {
            await openSidebar(page);
            await page.locator('.keyboard-toggle-button').first().click();
        } else {
            await (await barTool(page, '.keyboard-toggle-button'))?.click();
        }
        await sleep(400);
        if (dashboard === 'default') await closeSidebar(page);
        res.check(`${tag}: the dashboard's own keyboard control brings it back`, await visible(button));
        // A press that travels drags the button and raises nothing; the taps
        // below find it where it was left.
        const box = await button.first().boundingBox();
        if (box) {
            const [x, y] = [box.x + box.width / 2, box.y + box.height / 2];
            await js(page, FOCUS_SPY);
            await page.mouse.move(x, y);
            await page.mouse.down();
            await page.mouse.move(x - 200, y - 150, { steps: 5 });
            await page.mouse.up();
            await sleep(300);
        }
        const left = await button.first().boundingBox();
        res.check(`${tag}: a drag moves the keyboard button with the pointer`,
            box !== null && left !== null && Math.abs(left.x - (box.x - 200)) <= 2 && Math.abs(left.y - (box.y - 150)) <= 2,
            { box, left });
        const focused = await js(page, 'window.__assistFocus');
        res.check(`${tag}: and raises no keyboard`, Array.isArray(focused) && focused.length === 0, focused);
        // Each button that raises the keyboard asks the engine for it outright
        // where it has the VirtualKeyboard API (Chromium on Windows leaves it
        // down for a field focused by script, and always with a keyboard
        // attached), with the hidden field focused under the manual policy,
        // and focuses the field within the tap's own click, which iOS WebKit
        // needs to raise its keyboard.
        const raisers: [string, string][] = [['the floating keyboard button', '[data-virtual-keyboard-button]']];
        if (dashboard !== 'default') raisers.push(["the soft keys' keyboard button", `[data-soft-keys] button[aria-label="${VK_LABEL}"]`]);
        for (const [name, selector] of raisers) {
            if (name.includes('soft keys') && !(await page.locator(selector).count())) await openSoftKeys(page, dashboard);
            await js(page, VK_SPY);
            await js(page, FOCUS_SPY);
            await page.locator(selector).first().tap();
            await sleep(500);
            const got = await vkRead(page);
            // An engine without the API raises it on the focus alone.
            const asked = got.api ? got.shown >= 1 && got.policy === 'manual' : true;
            res.check(`${tag}: ${name} raises the on-screen keyboard`, got.focused && asked, got);
            res.check(`${tag}: ${name} focuses the field within the tap's own click`, got.within.length > 0 && got.within.every((w) => w === 'trusted click'), got.within);
            await js(page, 'document.activeElement && document.activeElement.blur()');
            await sleep(300);
            if (got.api) res.check(`${tag}: and the field leaving focus lowers it`, (await vkRead(page)).hidden >= 1);
        }
    } finally {
        await client?.close();
        await host.stop();
    }
}

/**
 * A convertible says at once whether its keyboard is attached: the primary
 * pointer turns coarse when it is detached and fine when it is attached. The
 * keyboard button follows at once, with no key pressed and no touch needed,
 * and a touch does not bring it back while the keyboard is on. Where Edge
 * misses an attach, the button still goes: on the window's resize when the
 * pointer turned fine without its event, and on the touchpad's use when it
 * stayed coarse.
 */
async function keyboardPostureBlock(host: Host, res: Checks, dashboard: string, engine: Engine): Promise<void> {
    const tag = `${dashboard} ${engine} posture`;
    const { url } = await host.start({ label: `posture-${dashboard}-${engine}`, mode: 'websockets', env: dashboardEnv(dashboard) });
    let client: Client | null = null;
    try {
        client = await openClient(engine, url, 'websockets', POINTER_POSTURE_JS);
        const { page } = client;
        const video = await waitWsVideo(page, 40);
        res.check(`${tag}: video up`, video !== null, video);
        if (video === null) return;
        const button = page.locator('[data-virtual-keyboard-button]');
        const shown = async (): Promise<boolean> => {
            await sleep(400);
            return visible(button);
        };
        res.check(`${tag}: opened with the keyboard attached, no button`, !(await shown()));
        // The screen still takes touches with the keyboard on: a touch says nothing of the keyboard.
        await touchOnce(client, engine);
        res.check(`${tag}: nor once the screen is touched`, !(await shown()));
        await js(page, '__setPointerFine(false)');
        res.check(`${tag}: detaching the keyboard brings the button at once`, await shown());
        await js(page, '__setPointerFine(true)');
        res.check(`${tag}: attaching it takes the button away at once`, !(await shown()));
        await touchOnce(client, engine);
        res.check(`${tag}: a touch leaves it away while the keyboard is attached`, !(await shown()));
        await js(page, '__setPointerFine(false)');
        res.check(`${tag}: and detaching it again brings it back`, await shown());
        await js(page, "__setPointerFineSilently(true); window.dispatchEvent(new Event('resize'))");
        res.check(`${tag}: an attach announced by no event goes with the window's resize`, !(await shown()));
        await js(page, '__setPointerFine(false)');
        res.check(`${tag}: detached once more, the button is back`, await shown());
        const size = page.viewportSize() ?? { width: 1280, height: 720 };
        await page.mouse.move(size.width / 2, size.height / 2);
        await page.mouse.move(size.width / 2 + 40, size.height / 2 + 20);
        res.check(`${tag}: with the pointer left coarse, the touchpad in use takes the button away`, !(await shown()));
    } finally {
        await client?.close();
        await host.stop();
    }
}

/** Puts the page in trackpad mode or direct touch with the dashboard's own button. */
async function setTrackpad(page: Page, dashboard: string, on: boolean): Promise<void> {
    const active = await js(page, '!!(window.webrtcInput && window.webrtcInput._trackpadMode)');
    if (active !== on) await trackpadButton(null, page, dashboard);
}

/**
 * Whether the palette's toggle opens it, its keys and its field take a click,
 * and the toggle, in the viewport and uncovered, closes it again.
 */
async function inReach(page: Page, dashboard: string, trackpad: boolean): Promise<[boolean, string]> {
    await setTrackpad(page, dashboard, trackpad);
    if (dashboard === 'default') await openSidebar(page);
    await openSoftKeys(page, dashboard);
    const toggle = page.locator('.key-palette-toggle').first();
    const steps: string[] = [];
    let closed: unknown;
    let opened: unknown;
    let shut: boolean;
    try {
        if ((await toggle.getAttribute('aria-expanded', { timeout: 5000 })) === 'true') {
            await toggle.click({ timeout: 5000 });
            await sleep(300);
        }
        await toggle.scrollIntoViewIfNeeded({ timeout: 5000 });
        closed = await js(page, RECT_JS('.key-palette-toggle'));
        await toggle.click({ timeout: 5000 });
        await sleep(400);
        opened = await js(page, RECT_JS('.key-palette-toggle'));
        for (const sel of ["[data-code='F12']", "[data-chord='Ctrl+Shift+Esc']", '.key-palette-input']) {
            await page.locator(sel).first().click({ timeout: 5000 });
            steps.push(sel);
        }
        await toggle.click({ timeout: 5000 });
        await sleep(300);
        shut = (await toggle.getAttribute('aria-expanded')) === 'false';
    } catch (e) {
        return [false, `after ${r(steps)}: ${String(e instanceof Error ? e.message : e).split('\n')[0].slice(0, 160)}`];
    } finally {
        if (dashboard === 'default') await closeSidebar(page);
    }
    const good = (rect: unknown): boolean => rect !== null && typeof rect === 'object' && Reflect.get(rect, 'inside') === true && Reflect.get(rect, 'on_top') === true;
    return [good(closed) && good(opened) && shut, `closed ${r(closed)} open ${r(opened)} shut ${shut}`];
}

async function pointerX(host: Host): Promise<number> {
    return (await host.pointer())?.[0] ?? 0;
}

async function drag(f: Fingers, px: number, steps: number, pauseMs: number): Promise<void> {
    await f.down([1, 300, 360]);
    for (let i = 1; i <= steps; i++) {
        if (pauseMs) await sleep(pauseMs);
        await f.move([1, 300 + px * i / steps, 360]);
    }
    await sleep(50);
    await f.up(1);
    await sleep(500);
}

async function travel(host: Host, f: Fingers, px: number, steps: number, pauseMs: number): Promise<number> {
    await host.warp(200, 360);
    await sleep(200);
    const before = await pointerX(host);
    await drag(f, px, steps, pauseMs);
    return (await pointerX(host)) - before;
}

/**
 * Turns trackpad mode on with the dashboard's own button, as a user does: a
 * dashboard learns of the mode from its button, not from the core. The button
 * shows once the dashboard has seen a touch; with a client, one more touch is
 * made if it has not.
 */
async function trackpadButton(client: { client: Client; engine: Engine } | null, page: Page, dashboard: string): Promise<boolean> {
    if (dashboard === 'default') await openSidebar(page);
    let button: Locator | null = null;
    for (let attempt = 0; attempt < 3; attempt++) {
        if (dashboard === 'default') {
            const b = page.locator('.trackpad-mode-button').first();
            try {
                await b.waitFor({ state: 'attached', timeout: 3000 });
                button = b;
                break;
            } catch {
                // not drawn yet
            }
        } else {
            button = await barTool(page, '.trackpad-mode-button');
            if (button !== null) break;
            await sleep(3000);
        }
        if (client === null || attempt === 2) return false;
        await touchOnce(client.client, client.engine);
        if (dashboard === 'default') await openSidebar(page);
    }
    if (button === null) return false;
    await button.click();
    await sleep(600);
    if (dashboard === 'default') await closeSidebar(page);
    return true;
}

async function speedBlock(host: Host, res: Checks, dashboard: string, mode: string, engine: Engine): Promise<void> {
    const tag = `${dashboard} ${engine} ${mode}`;
    const { url } = await host.start({ label: `speed-${dashboard}-${engine}-${mode}`, mode, env: dashboardEnv(dashboard) });
    let client: Client | null = null;
    try {
        client = await openClient(engine, url, mode);
        const { page } = client;
        const video = await waitVideo(page, mode);
        res.check(`${tag}: video up`, video !== null, video);
        if (video === null) return;
        await touchOnce(client, engine);
        if (!(await trackpadButton({ client, engine }, page, dashboard))) {
            res.check(`${tag}: the dashboard offers trackpad mode`, false);
            return;
        }
        const f = await Fingers.on(page, client.ctx, engine);
        const slow = await travel(host, f, 100, 50, 20);
        // A flick: a few long steps sent back to back.
        const fast = await travel(host, f, 100, 4, 0);
        res.check(`${tag}: a slow trackpad drag moves the pointer as far as the finger went`, slow >= 90 && slow <= 110, `${slow} px for 100`);
        res.check(`${tag}: a fast one moves it further`, fast >= 1.8 * slow, `${fast} px for 100`);
        if (dashboard === 'default') await openSidebar(page);
        await openSoftKeys(page, dashboard);
        const picker = page.locator('#trackpadSpeedSelect').first();
        const offered = (await picker.count()) > 0;
        res.check(`${tag}: the dashboard offers the trackpad speed in trackpad mode`, offered);
        if (!offered) return;
        await picker.selectOption('2');
        await sleep(500);
        if (dashboard === 'default') await closeSidebar(page);
        const doubled = await travel(host, f, 100, 50, 20);
        res.check(`${tag}: twice the speed moves a slow drag twice as far`, doubled >= 180 && doubled <= 220, `${doubled} px for 100`);
        await page.reload({ waitUntil: 'load' });
        await waitVideo(page, mode);
        const kept = await js(page, 'window.webrtcInput ? window.webrtcInput.trackpadSpeed : null');
        res.check(`${tag}: the pick is kept over a reload`, kept === 2, kept);
    } finally {
        await client?.close();
        await host.stop();
    }
}

async function waylandBlock(host: Host, res: Checks, mode: string, engine: Engine): Promise<void> {
    const tag = `default wayland ${engine} ${mode}`;
    const { url } = await host.start({ label: `wl-${engine}-${mode}`, mode, wayland: true, env: dashboardEnv('default') });
    let obs: Observer | null = null;
    let client: Client | null = null;
    try {
        client = await openClient(engine, url, mode);
        const { page } = client;
        const video = await waitVideo(page, mode);
        res.check(`${tag}: video up`, video !== null, video);
        if (video === null) return;
        obs = await host.observe('wayland-1');
        res.check(`${tag}: observer mapped`, await obs.ready());
        // A tap on the observer gives it the keyboard focus.
        const f = await Fingers.on(page, client.ctx, engine);
        await f.down([1, 640, 360]);
        await sleep(60);
        await f.up(1);
        await sleep(800);
        if (!(await openPalette(page, 'default'))) {
            res.check(`${tag}: a touch client is offered the key palette`, false);
            return;
        }
        const mark = (await obs.lines()).length;
        await page.locator("[data-chord='Alt+Tab']").first().click();
        await sleep(800);
        const keys = (await obs.lines(mark)).filter((l) => l.kind === 'kbd_key').map((l) => [l.state, l.key]);
        // evdev codes: KEY_LEFTALT 56, KEY_TAB 15.
        res.check(`${tag}: Alt+Tab reaches the seat pressed in order and released in reverse`, same(keys, [[1, 56], [1, 15], [0, 15], [0, 56]]), keys);
    } finally {
        await client?.close();
        await obs?.release();
        await host.stop();
    }
}

for (const which of ['x11', 'wl']) {
    test(which, async () => {
        const res = new Checks(`touch-controls-${which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            if (which === 'x11') {
                for (const dashboard of LAYOUTS) {
                    for (const mode of ['websockets', 'webrtc']) {
                        for (const engine of ENGINES) {
                            await paletteBlock(host, res, dashboard, engine, mode);
                            await speedBlock(host, res, dashboard, mode, engine);
                        }
                    }
                }
                for (const dashboard of LAYOUTS) {
                    for (const engine of ENGINES) {
                        await keyboardButtonBlock(host, res, dashboard, engine);
                        await keyboardPostureBlock(host, res, dashboard, engine);
                    }
                }
            } else {
                for (const mode of ['websockets', 'webrtc']) for (const engine of ENGINES) await waylandBlock(host, res, mode, engine);
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
