/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A keyboard-layout switch, seen from the application: us -> de -> ru.
//
// The client sends every key as the keysym the browser resolved on the user's
// own layout, so a German 'ä' or a Russian 'ф' has to arrive as that character
// whatever the server's layout is, and it has to arrive as the layout's own
// key (its keysym on its physical keycode, no overlay bind) once the server's
// layout matches the client's. Two switches drive that, on both transports:
//
// X11:     the keymap is deployment-owned, so the switch is a server-side
//          setxkbmap under a live handler (which learns of it only through
//          XkbNewKeyboardNotify); the client's keyboardLayout hint is noted
//          in the log and not applied. What arrives is read back through
//          libX11's own XLookupString on a focused window (tools/x11_keyobs.c),
//          on a private Xvfb so the layout changes touch nothing shared.
// Wayland: the client's keyboardLayout hint (the layout-map probe for de, the
//          language tag for ru) moves the seat's base layout, and the observer's
//          keys are resolved against the keymap the seat handed it, as a
//          Wayland application would (tools/xkb_lookup.c).
//
// The keys are pressed through CDP with the key/code a real de or ru keyboard
// produces, since Playwright's own keyboard knows only the US layout.
//
// A macOS client then types the same way through its Option key, which the
// layout uses as a level-3 shift while Blink reports it as a plain Alt: the
// chord has to arrive as the character it produced, and an Option over a key
// that produced none has to stay the Alt shortcut it is. Its Command chords go
// through Chromium, Firefox, and WebKit, with keyups arriving as Blink and
// WebKit deliver them on macOS: none for a key let go while Command is down,
// and none for Command itself when Spotlight takes it. A second chord under the
// same Command has to keep the Control it stands for, a key pressed twice under
// it has to arrive twice, and the application has to hold nothing Command
// withheld once an event shows Command up. A key held ten seconds with no
// Command has to stay held all the while.
import { spawnSync } from 'node:child_process';

import type { Browser, BrowserContext, CDPSession, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { firefoxPersistentContext, launchBrowser, launchChromium, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { onPath } from '../lib/desktop.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { WlKeys, X11Keys, Xkb } from '../lib/keys.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const WL_SOCKET = 'wayland-1';
// Evdev codes of the keys the probes sit on; an X keycode is eight higher.
const KEY_A = 30;
const KEY_APOSTROPHE = 40;

// (layout, the character a user of it types, the DOM code of the key it sits
// on, the keysym the layout binds there, its evdev code). Every probe is typed
// under every layout: it arrives as its character throughout, and as the
// native key under its own layout, which only a handler that saw the switch
// can deliver.
const PROBES: readonly [string, string, string, number, number][] = [
    ['us', 'a', 'KeyA', 0x61, KEY_A],
    ['de', 'ä', 'Quote', 0xE4, KEY_APOSTROPHE],
    ['ru', 'ф', 'KeyA', 0x6C6, KEY_A],
];

// How a page announces its layout: Chromium's layout-map probe identifies
// the QWERTZ family, and a Russian client resolves through its language tag.
const LAYOUT_MAPS: Readonly<Record<string, Record<string, string>>> = {
    us: { KeyY: 'y', KeyZ: 'z', KeyQ: 'q', KeyA: 'a', Semicolon: ';' },
    de: { KeyY: 'z', KeyZ: 'y', Minus: 'ß' },
    ru: {},
};
const LOCALES: Readonly<Record<string, string>> = { us: 'en-US', de: 'de-DE', ru: 'ru-RU' };

// A macOS client, and the Option chords a US and a German macOS layout
// produce. CDP's Alt bit is all Blink gives Option: it never reports AltGraph.
const CDP_ALT = 1;
const MAC_INIT = "Object.defineProperty(navigator, 'platform', { get: () => 'MacIntel', configurable: true });";
const XK_ALT_L = 0xFFE9;
const XK_TAB = 0xFF09;
// What Blink and WebKit on macOS keep from the page: the keyup of a key let
// go while Command is down, and Command's own once Spotlight has taken it.
const MAC_COMMAND_KEYUPS = `
window.__loseCommandUp = false;
window.addEventListener('keyup', (e) => {
  const command = e.code === 'MetaLeft' || e.code === 'MetaRight';
  if (command ? window.__loseCommandUp : e.metaKey) {
    if (command) window.__loseCommandUp = false;
    e.stopImmediatePropagation();
  }
}, true);
`;
const COMMAND_ENGINES: readonly Engine[] = ['chromium', 'firefox', 'webkit'];
const XK_CONTROL_L = 0xFFE3;
const XK_SPACE = 0x20;
const XK_BACKSPACE = 0xFF08;
const XK_RETURN = 0xFF0D;
const XK_A = 0x61;
const XK_C = 0x63;
const XK_W = 0x77;
const LONG_HOLD_S = 10;

/** One CDP Input.dispatchKeyEvent's parameters, as much of them as the suite sends. */
interface KeyEvent {
    type: 'keyDown' | 'keyUp';
    key: string;
    code: string;
    modifiers: number;
    location?: number;
    text?: string;
    unmodifiedText?: string;
}

/** One CDP key event of an Option chord. */
function opt(type: KeyEvent['type'], key: string, code: string, text: string | null = null, location = 0, modifiers = CDP_ALT): KeyEvent {
    const event: KeyEvent = { type, key, code, modifiers };
    if (location) event.location = location;
    if (text !== null) Object.assign(event, { text, unmodifiedText: text });
    return event;
}

/** Option held down over one key, then both released. */
function optionChord(code: string, char: string | null): KeyEvent[] {
    const key = char ?? code;
    return [opt('keyDown', 'Alt', 'AltLeft', null, 1), opt('keyDown', key, code, char), opt('keyUp', key, code, char),
        opt('keyUp', 'Alt', 'AltLeft', null, 1, 0)];
}

// (name, the chord, the character it has to type, the keysym it has to press).
const OPTION_CHORDS: readonly [string, KeyEvent[], string | null, number | null][] = [
    ['Option+Z', optionChord('KeyZ', 'Ω'), 'Ω', null],
    ['Option+L on a German layout', optionChord('KeyL', '@'), '@', null],
    ['Option+Tab', optionChord('Tab', null), null, XK_TAB],
];

function layoutJs(layout: string): string {
    const entries = Object.entries(LAYOUT_MAPS[layout]).map(([k, v]) => `['${k}', '${v}']`).join(', ');
    return `(() => { const map = new Map([${entries}]); Object.defineProperty(navigator, 'keyboard', { value: { getLayoutMap: async () => map }, configurable: true }); })();`;
}

const hex = (n: number): string => `0x${n.toString(16)}`;

/** Press and release one key the way a keyboard with that layout does. */
async function tap(cdp: CDPSession, char: string, code: string): Promise<void> {
    for (const type of ['keyDown', 'keyUp'] as const) {
        await cdp.send('Input.dispatchKeyEvent', { type, key: char, code, text: char, unmodifiedText: char });
    }
}

/** A key event as the probes judge it: pressed, keycode, keysym. */
type Key = readonly [boolean, number, number];
type Tapper = (char: string, code: string) => Promise<Key[]>;
type Chorder = (chord: KeyEvent[]) => Promise<Key[]>;

/** A tap that returns what the X window received. */
function x11Tapper(cdp: CDPSession, obs: X11Keys): Tapper {
    return async (char, code) => {
        await obs.drain(0.02);
        await tap(cdp, char, code);
        return (await obs.drain(0.6)).map(([p, kc, , ks]): Key => [p, kc, ks]);
    };
}

/** A tap that returns what the Wayland observer's keys resolved to. */
function wlTapper(cdp: CDPSession, keys: WlKeys): Tapper {
    return async (char, code) => {
        const start = await keys.count();
        await tap(cdp, char, code);
        await sleep(600);
        return keys.since(start);
    };
}

async function openPage(browser: Browser, url: string, mode: string, layout: string, mac = false): Promise<Page> {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, locale: LOCALES[layout] });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: layoutJs(layout) });
    if (mac) await ctx.addInitScript({ content: MAC_INIT });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    return page;
}

/** A chord runner returning what the X window received. */
function x11Chorder(cdp: CDPSession, obs: X11Keys): Chorder {
    return async (chord) => {
        await obs.drain(0.02);
        for (const event of chord) await cdp.send('Input.dispatchKeyEvent', event);
        return (await obs.drain(0.8)).map(([p, kc, , ks]): Key => [p, kc, ks]);
    };
}

/**
 * A chord runner returning what the Wayland observer's keys resolved to,
 * read once its key events stop for 0.3 s: a loaded host can deliver a chord
 * past any fixed window, and the 5 s bound still fails one that never comes.
 */
function wlChorder(cdp: CDPSession, keys: WlKeys): Chorder {
    return async (chord) => {
        const start = await keys.count();
        for (const event of chord) await cdp.send('Input.dispatchKeyEvent', event);
        let seen = 0;
        let settledAt = 0;
        for (const until = performance.now() + 5000; performance.now() < until;) {
            const count = (await keys.obs.lines(start)).filter((l) => l.kind === 'kbd_key').length;
            if (count !== seen) {
                seen = count;
                settledAt = performance.now();
            } else if (seen && performance.now() - settledAt >= 300) {
                break;
            }
            await sleep(20);
        }
        return keys.since(start);
    };
}

/** Type every Option chord and judge what the application received. */
async function checkOptionChords(res: Checks, label: string, xkb: Xkb, eventsAfter: Chorder): Promise<void> {
    for (const [name, chord, char, keysym] of OPTION_CHORDS) {
        const pressed = (await eventsAfter(chord)).filter(([down]) => down).map(([, , ks]) => ks);
        const seen = pressed.map(hex);
        if (char !== null) {
            let typed = false;
            for (const ks of pressed) if ((await xkb.char(ks)) === char) typed = true;
            res.check(`${label}: ${name} types the character it produced`, typed, seen);
            res.check(`${label}: ${name} wraps no Alt around it`, !pressed.includes(XK_ALT_L), seen);
        } else {
            res.check(`${label}: ${name} stays the Alt shortcut`, pressed.includes(XK_ALT_L) && keysym !== null && pressed.includes(keysym), seen);
        }
    }
}

/**
 * The keys an application holds, followed through the press and release
 * events it is delivered: `read` gives (pressed, keysym) for those that
 * arrived since it last ran.
 */
class Held {
    readonly down = new Set<number>();
    private readonly read: () => Promise<[boolean, number][]>;

    constructor(read: () => Promise<[boolean, number][]>) {
        this.read = read;
    }

    /** @returns Keysyms pressed since the last call, repeats included. */
    async presses(): Promise<number[]> {
        const out: number[] = [];
        for (const [pressed, keysym] of await this.read()) {
            if (pressed) {
                out.push(keysym);
                this.down.add(keysym);
            } else {
                this.down.delete(keysym);
            }
        }
        return out;
    }
}

/**
 * A macOS page on `engine` whose keyups arrive as macOS Blink and WebKit
 * deliver them, and what to close for it.
 */
async function commandPage(chromium: Browser, engine: Engine, url: string, mode: string): Promise<[Page, Browser | BrowserContext]> {
    let ctx: BrowserContext;
    let closer: Browser | BrowserContext;
    if (engine === 'firefox') {
        ctx = await firefoxPersistentContext({ viewport: { width: 1280, height: 720 } });
        closer = ctx;
    } else {
        const browser = engine === 'chromium' ? chromium : await launchBrowser(engine);
        ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
        closer = engine === 'chromium' ? ctx : browser;
    }
    for (const script of [`window.__SELKIES_STREAMING_MODE__ = '${mode}';`, MAC_INIT, MAC_COMMAND_KEYUPS]) await ctx.addInitScript({ content: script });
    const page = ctx.pages()[0] ?? await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    return [page, closer];
}

const hexSorted = (keys: Iterable<number>): string[] => [...keys].map(hex).sort();

/**
 * `before` leaves `keysym` down under Command (or, `tapped`, sends it as a tap
 * there), Spotlight takes Command's keyup, and `after` is the first the page
 * hears of Command being up.
 */
async function spotlight(res: Checks, name: string, page: Page, held: Held, keysym: number,
    before: () => Promise<void>, after: () => Promise<void>, tapped = false): Promise<void> {
    await before();
    await within(page.evaluate(() => { Reflect.set(window, '__loseCommandUp', true); }));
    await page.keyboard.up('Meta');
    await sleep(300);
    const pressed = await held.presses();
    const stuck = held.down.has(keysym);
    await after();
    await sleep(300);
    await held.presses();
    if (tapped) {
        res.check(`${name} once Spotlight took Command's keyup`,
            pressed.includes(keysym) && !stuck && !held.down.has(keysym) && !held.down.has(XK_CONTROL_L),
            `pressed ${pressed.includes(keysym)}, held before ${stuck}, after ${JSON.stringify(hexSorted(held.down))}`);
        return;
    }
    res.check(`${name} once Spotlight took Command's keyup`, stuck && !held.down.has(keysym), `held before ${stuck}, after ${held.down.has(keysym)}`);
}

/** A key held for LONG_HOLD_S stays held, and repeats where the server repeats. */
async function longHold(res: Checks, label: string, page: Page, held: Held, repeats: boolean): Promise<void> {
    const kb = page.keyboard;
    await kb.down('w');
    await sleep(LONG_HOLD_S * 1000);
    const pressed = await held.presses();
    const count = pressed.filter((k) => k === XK_W).length;
    res.check(`${label}: a key held ${LONG_HOLD_S} s stays held`, held.down.has(XK_W) && (!repeats || count > 1), `${count} presses`);
    await kb.up('w');
    await sleep(300);
    await held.presses();
    res.check(`${label}: and goes at its keyup`, !held.down.has(XK_W), '');
}

/**
 * Command chords with the macOS keyups missing, then a long plain hold. A
 * key pressed under Command goes as a tap in every engine on macOS: Safari
 * and Firefox lose its keyup there, and the one Chrome delivers finds nothing
 * held. `repeats`: whether the server repeats a held key itself (X11), which
 * the long hold then has to show.
 */
async function checkCommandChords(res: Checks, label: string, page: Page, held: Held, repeats: boolean): Promise<void> {
    const kb = page.keyboard;
    await kb.down('Meta');
    await kb.press('a');
    await kb.press('c');
    await sleep(300);
    let pressed = await held.presses();
    res.check(`${label}: Cmd+A then Cmd+C holds Control for the C`, pressed.includes(XK_C) && held.down.has(XK_CONTROL_L),
        `pressed ${JSON.stringify(pressed.map(hex))}, held ${JSON.stringify(hexSorted(held.down))}`);
    await kb.up('Meta');
    await sleep(300);
    await held.presses();
    res.check(`${label}: Command's keyup lets go of the chord`, ![XK_A, XK_C, XK_CONTROL_L].some((k) => held.down.has(k)), [...held.down].map(hex));

    await spotlight(res, `${label}: a space rolled into Cmd+Space goes at the next key`, page, held, XK_SPACE,
        async () => { await kb.down(' '); await kb.down('Meta'); await kb.up(' '); }, () => kb.press('Escape'));
    await spotlight(res, `${label}: a Cmd+Return's Return is a tap, and its Control goes at a click`, page, held, XK_RETURN,
        async () => { await kb.down('Meta'); await kb.press('Enter'); }, () => page.mouse.click(640, 360), true);
    await kb.down('Meta');
    await kb.press('Backspace');
    await kb.press('Backspace');
    await sleep(300);
    pressed = await held.presses();
    res.check(`${label}: Cmd+Backspace pressed twice deletes twice`,
        pressed.filter((k) => k === XK_BACKSPACE).length === 2 && held.down.has(XK_CONTROL_L),
        `pressed ${JSON.stringify(pressed.map(hex))}, held ${JSON.stringify(hexSorted(held.down))}`);
    await kb.up('Meta');
    await sleep(300);
    await held.presses();
    await longHold(res, label, page, held, repeats);
}

function waitVideo(page: Page, mode: string): ReturnType<typeof waitWsVideo> {
    return mode === 'websockets' ? waitWsVideo(page, 30) : waitWrVideo(page);
}

/**
 * Type every probe under `layout` and judge what arrived.
 * @param keycodeBase What the reader adds to an evdev code (8 on X11).
 */
async function checkProbes(res: Checks, layout: string, xkb: Xkb, eventsAfter: Tapper, keycodeBase: number): Promise<void> {
    for (const [owner, char, code, native, evdev] of PROBES) {
        const events = await eventsAfter(char, code);
        const got: [number, number][] = [];
        for (const [pressed, kc, ks] of events) if (pressed && (await xkb.char(ks)) === char) got.push([kc, ks]);
        res.check(`${layout}: '${char}' arrives as '${char}'`, got.length > 0, events.map(([, kc, ks]) => [kc, hex(ks)]));
        if (owner === layout) {
            res.check(`${layout}: '${char}' is the layout's own key`, got.length > 0 && got[0][0] === evdev + keycodeBase && got[0][1] === native, got);
        }
    }
}

function setxkbmap(display: string, layout: string): void {
    const run = spawnSync('setxkbmap', ['-display', display, layout], { timeout: 20_000 });
    if (run.status !== 0) throw new Error(`setxkbmap ${layout} exited ${run.status}`);
}

async function runX11(res: Checks, host: Host, mode: string): Promise<void> {
    const xkb = new Xkb();
    const xvfb = await host.xvfb(1280, 720, ['-s', '0', '-dpms']);
    const display = xvfb.display;
    let obs: X11Keys | null = null;
    try {
        setxkbmap(display, 'us');
        const { url } = await host.start({ label: mode, mode, display });
        obs = new X11Keys(display);
        await obs.focused();
        const browser = await launchChromium();
        try {
            for (const [layout] of PROBES) {
                setxkbmap(display, layout);
                await sleep(500);
                const page = await openPage(browser, url, mode, layout);
                res.check(`${layout}: video flowing`, Boolean(await waitVideo(page, mode)));
                res.check(`${layout}: hint noted, X11 keymap left to the deployment`, await host.saw(`keyboard layout hint '${layout}' noted (X11 keymap`, 15), '');
                await page.mouse.click(640, 360);
                await sleep(500);
                await checkProbes(res, layout, xkb, x11Tapper(await page.context().newCDPSession(page), obs), 8);
                await page.context().close();
            }
            setxkbmap(display, 'us');
            await sleep(500);
            const page = await openPage(browser, url, mode, 'us', true);
            res.check('macOS: video flowing', Boolean(await waitVideo(page, mode)));
            await page.mouse.click(640, 360);
            await sleep(500);
            await checkOptionChords(res, 'x11', xkb, x11Chorder(await page.context().newCDPSession(page), obs));
            await page.context().close();
            for (const engine of COMMAND_ENGINES) {
                const [cpage, closer] = await commandPage(browser, engine, url, mode);
                res.check(`macOS ${engine}: video flowing`, Boolean(await waitVideo(cpage, mode)));
                await cpage.mouse.click(640, 360);
                await sleep(500);
                await obs.drain(0.1);
                const reader = obs;
                const held = new Held(async () => (await reader.drain(0.05)).map(([down, , , ks]): [boolean, number] => [down, ks]));
                await checkCommandChords(res, `x11 ${engine}`, cpage, held, true);
                await closer.close();
            }
        } finally {
            await browser.close();
        }
    } finally {
        obs?.close();
        xkb.close();
        await host.stop();
        await xvfb.release();
    }
}

async function runWayland(res: Checks, host: Host, mode: string): Promise<void> {
    const xkb = new Xkb();
    const { url } = await host.start({ label: mode, mode, wayland: true });
    const obs = await host.observe(WL_SOCKET, { WLOBS_DURATION: '600' });
    const keys = new WlKeys(obs, xkb);
    try {
        res.check('wl observer mapped', await obs.ready(20));
        const browser = await launchChromium();
        try {
            for (const [layout] of PROBES) {
                const page = await openPage(browser, url, mode, layout);
                res.check(`${layout}: video flowing`, Boolean(await waitVideo(page, mode)));
                res.check(`${layout}: seat base layout follows the client hint`, await host.saw(`Wayland base layout set to '${layout}'`, 15), '');
                await page.mouse.click(640, 360);
                await sleep(500);
                await checkProbes(res, layout, xkb, wlTapper(await page.context().newCDPSession(page), keys), 0);
                await page.context().close();
            }
            const page = await openPage(browser, url, mode, 'us', true);
            res.check('macOS: video flowing', Boolean(await waitVideo(page, mode)));
            await page.mouse.click(640, 360);
            await sleep(500);
            await checkOptionChords(res, 'wayland', xkb, wlChorder(await page.context().newCDPSession(page), keys));
            await page.context().close();
            for (const engine of COMMAND_ENGINES) {
                const [cpage, closer] = await commandPage(browser, engine, url, mode);
                res.check(`macOS ${engine}: video flowing`, Boolean(await waitVideo(cpage, mode)));
                await cpage.mouse.click(640, 360);
                await sleep(500);
                await keys.since(0);
                const held = new Held(async () => (await keys.since(0)).map(([down, , ks]): [boolean, number] => [down, ks]));
                await checkCommandChords(res, `wayland ${engine}`, cpage, held, false);
                await closer.close();
            }
        } finally {
            await browser.close();
        }
    } finally {
        await obs.release();
        xkb.close();
        await host.stop();
    }
}

for (const which of ['ws-x11', 'wr-x11', 'ws-wl', 'wr-wl']) {
    test(which, async () => {
        // Both are what the suite reads keys back through; without them it has nothing to say.
        if (!onPath('setxkbmap')) throw new Error('setxkbmap is not installed');
        const [transport, backend] = which.split('-');
        const mode = transport === 'ws' ? 'websockets' : 'webrtc';
        const res = new Checks(`keyboard-layout-${which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await (backend === 'x11' ? runX11(res, host, mode) : runWayland(res, host, mode));
        } finally {
            await host.close();
        }
        res.finish();
    });
}
