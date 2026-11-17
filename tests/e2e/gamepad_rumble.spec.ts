/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Rumble end to end: an application's force feedback, played on the pad of
// the browser that drives the slot.
//
// An application under the Input Interposer uploads a rumble effect on the
// virtual pad's evdev node and plays it; the server mixes it and relays it to
// the one client whose pad drives that slot, and the page plays it on its
// pads. On each transport:
//
// - pads: the dual-rumble effect of a `vibrationActuator` (Chromium, WebKit)
//   or the pulse of Gecko's `hapticActuators`, on pads stubbed with actuators
//   that record what they are asked to play, so every engine is driven; a
//   #player2 page on the same server drives slot 1 and must hear nothing of
//   slot 0's.
// - takeover: a client that takes the slot while an effect without an end
//   plays is handed it at once rather than at the next renewal, and hears its
//   stop.
// - touch: the on-screen touch gamepad's pad vibrates the device through
//   `navigator.vibrate`, stubbed, since a desktop engine has no motor.
// - dashboards: each layout's Rumble toggle turns playing off in the core,
//   stopping what plays, and the core keeps the pick over a reload.
//
// The application is tools/ff_rumble_app.c.
import { spawn, spawnSync, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { join } from 'node:path';
import { createInterface, type Interface } from 'node:readline';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { firefoxPersistentContext, launchBrowser, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { barClosePanel, barTool } from '../lib/dashboard.ts';
import { DASHBOARD_DIST, REPO, sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host, runtimeDir } from '../lib/host.ts';
import { builtTool } from '../lib/keys.ts';
import { isRecord } from '../../web/selkies-web-core/lib/narrow.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const LAYOUTS = ['default', 'wish', 'edge'] as const;

/** The input interposer, built into the work directory. */
function buildInterposer(): string {
    const so = join(WORKDIR, 'selkies_input_interposer.so');
    const cc = spawnSync('gcc', ['-O2', '-shared', '-fPIC', '-o', so, join(REPO, 'addons', 'input-interposer', 'input_interposer.c'), '-ldl', '-pthread'], { encoding: 'utf8', timeout: 180_000 });
    if (cc.status !== 0) throw new Error(`gcc input_interposer: ${(cc.stderr ?? '').slice(0, 300)}`);
    return so;
}

// A standard pad whose motors record what they are asked to play, in the
// shape the engine gives its pads: `vibrationActuator` for Chromium and
// WebKit, Gecko's `hapticActuators` otherwise. A hidden pad is reported only
// once `__connectPad` announces it, so a test chooses when the page takes its
// slot.
const padInit = (gecko: boolean, hidden: boolean): string => `
window.__rumble = [];
const gecko = ${gecko};
const hidden = ${hidden};
const actuator = {
  type: 'dual-rumble', effects: ['dual-rumble'],
  playEffect(type, p) { window.__rumble.push([performance.now(), 'play', type, p.strongMagnitude, p.weakMagnitude, p.duration]); return Promise.resolve('complete'); },
  reset() { window.__rumble.push([performance.now(), 'reset']); return Promise.resolve('complete'); },
};
window.__pad = {
  index: 0, id: "Selkies Test Pad (STANDARD GAMEPAD Vendor: 045e Product: 028e)",
  mapping: "standard", connected: true, timestamp: 1,
  buttons: Array.from({length: 17}, () => ({pressed: false, touched: false, value: 0})),
  axes: [0, 0, 0, 0],
};
if (gecko) window.__pad.hapticActuators = [{ pulse(v, ms) { window.__rumble.push([performance.now(), 'pulse', v, ms]); return Promise.resolve(true); } }];
else window.__pad.vibrationActuator = actuator;
window.__padShown = !hidden;
navigator.getGamepads = () => [window.__padShown ? window.__pad : null, null, null, null];
window.__padPress = (i, v) => {
  window.__pad.buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
  window.__pad.timestamp = performance.now();
};
window.__connectPad = () => {
  window.__padShown = true;
  const e = new Event('gamepadconnected');
  Object.defineProperty(e, 'gamepad', {value: window.__pad});
  window.__padConnectedAt = performance.now();
  window.dispatchEvent(e);
};
`;

// The device's vibrator, for the touch gamepad's pad.
const VIBRATE_INIT = `
window.__vibes = [];
Object.defineProperty(navigator, 'vibrate', {configurable: true, value: (p) => {
  window.__vibes.push([performance.now(), p]); return true; }});
`;

// One touch on the touch gamepad's A button, for engines without CDP touch.
const SYNTH_TAP = `(([x, y]) => {
  const el = document.elementFromPoint(x, y);
  const legacy = (() => { try { new Touch({identifier: 0, target: el}); return false; } catch (e) { return true; } })();
  const t = legacy ? document.createTouch(window, el, 1, x, y, x, y) : new Touch({identifier: 1, target: el, clientX: x, clientY: y});
  const list = (ts) => legacy ? document.createTouchList(...ts) : ts;
  el.dispatchEvent(new TouchEvent('touchstart', {touches: list([t]), targetTouches: list([t]), changedTouches: list([t]), bubbles: true, cancelable: true}));
  el.dispatchEvent(new TouchEvent('touchend', {touches: list([]), targetTouches: list([]), changedTouches: list([t]), bubbles: true, cancelable: true}));
})`;

const FIREFOX_TOUCH_PREFS = { 'dom.w3c_touch_events.enabled': 1, 'dom.w3c_touch_events.legacy_apis.enabled': true };

// What the page's pad manager holds that gates a rumble: polling, the toggle, and what plays.
const PAD_STATE = `(() => {
  const m = window.webrtcInput && window.webrtcInput.gamepadManager;
  return m ? {active: m._active, enabled: m.rumbleEnabled, rumbling: m._rumbling} : null;
})()`;
const RUMBLE_STATE = 'window.webrtcInput ? window.webrtcInput.gamepadRumble : null';
const STORED = `(() => {
  for (let i = 0; i < localStorage.length; i++) {
    const k = localStorage.key(i);
    if (k.endsWith('_gamepad_rumble')) return localStorage.getItem(k);
  }
  return null;
})()`;

/** CLOCK_MONOTONIC in seconds: the clock the application stamps its writes on. */
const monotonic = (): number => Number(process.hrtime.bigint()) / 1e9;

type Call = readonly unknown[];

const evalJs = (page: Page, js: string): Promise<unknown> => within(page.evaluate(js));

async function calls(page: Page, name = '__rumble'): Promise<Call[]> {
    const got = await evalJs(page, `window.${name}`);
    return Array.isArray(got) ? got.filter((c): c is Call => Array.isArray(c)) : [];
}

const num = (v: unknown): number => (typeof v === 'number' ? v : NaN);
const round = (v: number | null, digits = 0): number | null => (v === null ? null : Number(v.toFixed(digits)));

/** CLOCK_MONOTONIC seconds minus the page's performance.now() seconds, from the tightest of a few round trips. */
async function pageOffset(page: Page): Promise<number> {
    let best: [number, number] | null = null;
    for (let i = 0; i < 15; i++) {
        const t0 = monotonic();
        const pn = num(await evalJs(page, 'performance.now()'));
        const t1 = monotonic();
        if (best === null || t1 - t0 < best[0]) best = [t1 - t0, (t0 + t1) / 2 - pn / 1000];
    }
    return best ? best[1] : 0;
}

function playsAndStops(all: readonly Call[]): [Call[], Call[]] {
    const plays = all.filter((c) => c[1] === 'play' || (c[1] === 'pulse' && num(c[2]) > 0));
    const stops = all.filter((c) => c[1] === 'reset' || (c[1] === 'pulse' && num(c[2]) === 0));
    return [plays, stops];
}

/** The application's environment: the interposer preloaded, pointed at the server's sockets. */
function appEnv(preload: string, runtime: string): NodeJS.ProcessEnv {
    const env: NodeJS.ProcessEnv = { ...process.env, LD_PRELOAD: preload, SELKIES_JS_SOCKET_PATH: runtime };
    return env;
}

const appBinary = (): string => builtTool('ff_rumble_app', []);

/** The one-shot application; the CLOCK_MONOTONIC time of its play, or null. */
function runApp(preload: string, runtime: string): number | null {
    const out = spawnSync(appBinary(), [], { env: appEnv(preload, runtime), encoding: 'utf8', timeout: 60_000 });
    const lines = (out.stdout ?? '').split('\n').filter((l) => l.startsWith('{'));
    const last = lines.at(-1);
    if (!last) return null;
    const parsed: unknown = JSON.parse(last);
    return parsed !== null && typeof parsed === 'object' && 'played' in parsed ? num(parsed.played) : null;
}

/** The endless application, driven over its standard input. */
class EndlessApp {
    private readonly proc: ChildProcessWithoutNullStreams;
    private readonly lines: Interface;
    private readonly queue: string[] = [];
    private waiter: (() => void) | null = null;

    constructor(preload: string, runtime: string) {
        this.proc = spawn(appBinary(), ['endless'], { env: appEnv(preload, runtime), stdio: ['pipe', 'pipe', 'pipe'] });
        this.lines = createInterface({ input: this.proc.stdout });
        this.lines.on('line', (l) => {
            this.queue.push(l);
            this.waiter?.();
        });
        this.proc.on('exit', () => this.waiter?.());
    }

    /** The next field `key` the application prints, or null when it ends first. */
    async next(key: string, timeoutS = 30): Promise<number | null> {
        const until = Date.now() + timeoutS * 1000;
        while (this.queue.length === 0) {
            if (this.proc.exitCode !== null || Date.now() > until) return null;
            await new Promise<void>((resolve) => {
                this.waiter = resolve;
                setTimeout(resolve, 200);
            });
            this.waiter = null;
        }
        const parsed: unknown = JSON.parse(this.queue.shift() ?? '{}');
        return isRecord(parsed) && key in parsed ? num(parsed[key]) : null;
    }

    stop(): void {
        if (this.proc.stdin.writable) this.proc.stdin.write('stop\n');
    }

    async close(): Promise<void> {
        this.stop();
        const gone = new Promise<void>((resolve) => {
            if (this.proc.exitCode !== null) resolve();
            else this.proc.once('exit', () => { resolve(); });
        });
        const timer = setTimeout(() => this.proc.kill('SIGKILL'), 10_000);
        await gone;
        clearTimeout(timer);
        this.lines.close();
    }
}

interface Opened {
    close: () => Promise<void>;
    ctx: BrowserContext;
    page: Page;
}

async function openPage(engine: Engine, url: string, mode: string, fragment = '', init = '', touch = false): Promise<Opened> {
    const viewport = { width: 1280, height: 720 };
    let ctx: BrowserContext;
    let browser: Browser | null = null;
    if (engine === 'firefox') {
        ctx = await firefoxPersistentContext({ viewport, ...(touch ? { prefs: FIREFOX_TOUCH_PREFS, hasTouch: true } : {}) });
    } else {
        browser = await launchBrowser(engine);
        ctx = await browser.newContext({ viewport, hasTouch: touch });
    }
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    if (init) await ctx.addInitScript({ content: init });
    const page = await ctx.newPage();
    await page.goto(`${url}/${fragment}`, { waitUntil: 'load' });
    const close = async (): Promise<void> => {
        try {
            await (browser ?? ctx).close();
        } catch {
            // already gone
        }
    };
    return { close, ctx, page };
}

const waitVideo = (page: Page, mode: string): Promise<unknown> => (mode === 'websockets' ? waitWsVideo(page, 40) : waitWrVideo(page, 60));

async function press(page: Page): Promise<void> {
    await evalJs(page, 'window.__padPress(0, 1)');
    await sleep(300);
    await evalJs(page, 'window.__padPress(0, 0)');
    await sleep(500);
}

const short = (v: unknown): string => JSON.stringify(v);

async function pads(url: string, mode: string, preload: string, runtime: string, res: Checks): Promise<void> {
    for (const engine of ['chromium', 'firefox', 'webkit'] as const) {
        const tag = `${engine} ${mode}`;
        const gecko = engine === 'firefox';
        const opened = await openPage(engine, url, mode, '', padInit(gecko, false));
        const closers = [opened];
        const { page } = opened;
        try {
            res.check(`${tag}: video up`, (await waitVideo(page, mode)) !== null);
            let other: Page | null = null;
            if (engine === 'chromium') {
                const o = await openPage(engine, url, mode, '#player2', padInit(gecko, false));
                closers.push(o);
                other = o.page;
                await waitVideo(other, mode);
                await press(other);
            }
            await press(page);
            const offset = await pageOffset(page);
            const played = runApp(preload, runtime);
            if (played === null) {
                res.check(`${tag}: the application played an effect`, false);
                continue;
            }
            await sleep(300);
            const all = await calls(page);
            const [plays, stops] = playsAndStops(all);
            const first = plays[0] ?? null;
            const want = first !== null && (gecko
                ? first[2] === 0.5 && num(first[3]) >= 380 && num(first[3]) <= 400
                : first[2] === 'dual-rumble' && first[3] === 0.5 && first[4] === 0.25 && num(first[5]) >= 380 && num(first[5]) <= 400);
            const lag = first ? (num(first[0]) / 1000 + offset - played) * 1000 : null;
            res.check(`${tag}: the page plays the application's rumble on its pad`, want, `${short(all.slice(0, 3))}, ${round(lag, 1)} ms after the write`);
            const stop = stops[0] ?? null;
            const held = stop && first ? num(stop[0]) - num(first[0]) : null;
            res.check(`${tag}: and stops it when the effect has run`, held !== null && held >= 350 && held <= 480, `after ${round(held)} ms`);
            if (other !== null) {
                const heard = await calls(other);
                res.check(`${tag}: a page driving another slot hears none of it`, heard.length === 0, heard);
            }
        } finally {
            for (const c of closers) await c.close();
        }
    }
}

async function takeover(url: string, mode: string, preload: string, runtime: string, res: Checks): Promise<void> {
    const tag = `chromium ${mode}`;
    let first: Opened | null = await openPage('chromium', url, mode, '', padInit(false, false));
    let app: EndlessApp | null = null;
    try {
        await waitVideo(first.page, mode);
        await press(first.page);
        app = new EndlessApp(preload, runtime);
        const played = (await app.next('played')) ?? NaN;
        await sleep(400);
        let [plays] = playsAndStops(await calls(first.page));
        const lease = (c: Call | undefined): boolean => c !== undefined && c[3] === 1 && c[4] === 0.5 && c[5] === 2000;
        res.check(`${tag}: an effect without an end plays for a lease at a time`, lease(plays[0]), plays.slice(0, 1));
        await first.close();
        first = null;
        await sleep(1000);
        const opened = await openPage('chromium', url, mode, '', padInit(false, true));
        try {
            const { page } = opened;
            await waitVideo(page, mode);
            await sleep(1000);
            // Take the slot 0.3 s past a renewal, so the next one is 0.7 s off.
            const phase = (((monotonic() - played) % 1) + 1) % 1;
            await sleep((((1.3 - phase) % 1) + 1) % 1 * 1000);
            await evalJs(page, 'window.__connectPad()');
            await sleep(900);
            const all = await calls(page);
            const taken = num(await evalJs(page, 'window.__padConnectedAt'));
            [plays] = playsAndStops(all);
            const lag = plays.length ? num(plays[0][0]) - taken : null;
            res.check(`${tag}: a client taking the slot mid-effect is handed it at once, not at the next renewal`,
                lag !== null && lag < 250 && lease(plays[0]), `${round(lag)} ms after taking the slot: ${short(plays.slice(0, 1))}`);
            app.stop();
            const stopped = (await app.next('stopped')) ?? NaN;
            await sleep(400);
            const offset = await pageOffset(page);
            const [, stops] = playsAndStops(await calls(page));
            const last = stops.at(-1);
            const stopLag = last ? (num(last[0]) / 1000 + offset - stopped) * 1000 : null;
            res.check(`${tag}: and hears its stop`, stopLag !== null && stopLag < 100, `${round(stopLag, 1)} ms after the write`);
        } finally {
            await opened.close();
        }
    } finally {
        await first?.close();
        await app?.close();
    }
}

async function touch(url: string, mode: string, preload: string, runtime: string, res: Checks, dashboard: string, engines: readonly Engine[]): Promise<void> {
    for (const engine of engines) {
        const tag = `${engine} ${mode} ${dashboard}`;
        const opened = await openPage(engine, url, mode, '', VIBRATE_INIT, true);
        const { page } = opened;
        try {
            await waitVideo(page, mode);
            await sleep(1000);
            await evalJs(page, "window.postMessage({type: 'toggleTouchGamepad'}, window.location.origin)");
            // The touch gamepad draws its buttons on its own time.
            let box: [number, number] | null = null;
            for (const until = Date.now() + 8000; box === null && Date.now() < until;) {
                await sleep(500);
                const got = await evalJs(page, `(() => {
                  const els = [...document.querySelectorAll('#universal-touch-gamepad-controls-overlay .touch-button')];
                  const a = els.find((e) => e.textContent.trim() === 'A');
                  if (!a) return null;
                  const r = a.getBoundingClientRect();
                  return [r.left + r.width / 2, r.top + r.height / 2];
                })()`);
                if (Array.isArray(got) && got.length === 2) box = [num(got[0]), num(got[1])];
            }
            if (box === null) {
                res.check(`${tag}: the touch gamepad shows its A button`, false);
                continue;
            }
            // The first touch connects the touch gamepad's pad, which takes the slot.
            if (engine === 'chromium') {
                const cdp = await opened.ctx.newCDPSession(page);
                await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [{ x: box[0], y: box[1], id: 1 }] });
                await sleep(60);
                await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] });
            } else {
                await evalJs(page, `${SYNTH_TAP}(${JSON.stringify(box)})`);
            }
            await sleep(1000);
            const offset = await pageOffset(page);
            const played = runApp(preload, runtime);
            await sleep(300);
            const vibes = await calls(page, '__vibes');
            const on = vibes.find((v) => num(v[1]) > 0) ?? null;
            const off = on ? (vibes.find((v) => num(v[0]) > num(on[0]) && v[1] === 0) ?? null) : null;
            const lag = on && played !== null ? (num(on[0]) / 1000 + offset - played) * 1000 : null;
            res.check(`${tag}: the touch gamepad vibrates the device for the effect's length`, on !== null && num(on[1]) >= 380 && num(on[1]) <= 400,
                `${short(vibes.slice(0, 3))}, ${round(lag, 1)} ms after the write`);
            const held = off && on ? num(off[0]) - num(on[0]) : null;
            res.check(`${tag}: and stops it when the effect has run`, held !== null && held >= 350 && held <= 480, `after ${round(held)} ms`);
        } finally {
            await opened.close();
        }
    }
}

async function clickRumbleToggle(page: Page, dashboard: string): Promise<boolean> {
    const toggle = page.locator('#gamepadRumbleToggle');
    if (dashboard === 'default') {
        try {
            await page.locator('.toggle-handle').first().click();
            await sleep(800);
        } catch {
            // the sidebar may be open already
        }
        if (!(await toggle.count())) {
            const header = page.locator('.sidebar-section-header:has-text("Gamepads")').first();
            if (!(await header.count())) return false;
            await header.scrollIntoViewIfNeeded();
            await header.click();
            await sleep(800);
        }
        if (!(await toggle.count())) return false;
        await toggle.first().scrollIntoViewIfNeeded();
        await toggle.first().click();
        await sleep(500);
        return true;
    }
    // A bar layout keeps it in the Gamepads panel its button opens.
    const panel = await barTool(page, 'button[aria-label="Gamepads"]');
    if (panel === null) return false;
    await panel.click();
    await sleep(600);
    const ok = (await toggle.count()) > 0;
    if (ok) {
        await toggle.first().click();
        await sleep(500);
    }
    await barClosePanel(page, 'button[aria-label="Gamepads"]');
    return ok;
}

const dashboardEnv = (layout: string): Record<string, string> => ({ SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: layout });

async function dashboards(host: Host, mode: string, preload: string, res: Checks): Promise<void> {
    for (const dashboard of LAYOUTS) {
        const tag = `${dashboard} ${mode}`;
        await host.stop();
        const { url, pid } = await host.start({ label: `toggle-${mode}-${dashboard}`, mode, env: dashboardEnv(dashboard) });
        const runtime = runtimeDir(pid);
        const opened = await openPage('chromium', url, mode, '', padInit(false, false));
        const { page } = opened;
        try {
            await waitVideo(page, mode);
            await sleep(1000);
            await press(page);
            const state = await evalJs(page, RUMBLE_STATE);
            res.check(`${tag}: rumble starts on`, state === true, state);
            const offset = await pageOffset(page);
            const app = new EndlessApp(preload, runtime);
            try {
                const played = (await app.next('played')) ?? NaN;
                // The toggle has to find the effect playing: wait for it, up to well past its bound.
                let [plays] = playsAndStops(await calls(page));
                for (const until = Date.now() + 3000; plays.length === 0 && Date.now() < until;) {
                    await sleep(50);
                    [plays] = playsAndStops(await calls(page));
                }
                const lag = plays.length ? (num(plays[0][0]) / 1000 + offset - played) * 1000 : null;
                res.check(`${tag}: the page plays the application's endless effect`, lag !== null && lag < 400,
                    plays.length ? `${round(lag, 1)} ms after the write` : `nothing in 3 s; pads ${short(await evalJs(page, PAD_STATE))}`);
                const clicked = await clickRumbleToggle(page, dashboard);
                res.check(`${tag}: the dashboard offers a Rumble toggle`, clicked, clicked);
                await sleep(600);
                const all = await calls(page);
                const [playsNow, stops] = playsAndStops(all);
                plays = playsNow;
                const lastPlay = plays.at(-1);
                const lastStop = stops.at(-1);
                res.check(`${tag}: turning it off stops what plays`,
                    lastPlay !== undefined && lastStop !== undefined && num(lastStop[0]) > num(lastPlay[0]) && (await evalJs(page, RUMBLE_STATE)) === false, all.slice(-2));
                const n = plays.length;
                await sleep(1300);
                [plays] = playsAndStops(await calls(page));
                res.check(`${tag}: and nothing plays while it is off, renewals included`, plays.length === n, plays.slice(n));
                app.stop();
                await app.next('stopped');
            } finally {
                await app.close();
            }
            const stored = await evalJs(page, STORED);
            res.check(`${tag}: the core keeps the pick`, stored === 'false', stored);
            await page.reload({ waitUntil: 'load' });
            await waitVideo(page, mode);
            await sleep(1000);
            const after = await evalJs(page, RUMBLE_STATE);
            res.check(`${tag}: a reload comes back with rumble off`, after === false, after);
        } finally {
            await opened.close();
        }
    }
}

async function run(host: Host, mode: string, res: Checks): Promise<void> {
    const preload = buildInterposer();
    try {
        let { url, pid } = await host.start({ label: mode, mode });
        let runtime = runtimeDir(pid);
        await pads(url, mode, preload, runtime, res);
        await takeover(url, mode, preload, runtime, res);
        // The touch gamepad lives in the dashboard.
        for (const layout of LAYOUTS) {
            await host.stop();
            ({ url, pid } = await host.start({ label: `touch-${mode}-${layout}`, mode, env: dashboardEnv(layout) }));
            runtime = runtimeDir(pid);
            await touch(url, mode, preload, runtime, res, layout, layout === 'default' ? ['chromium', 'firefox', 'webkit'] : ['chromium']);
        }
        await dashboards(host, mode, preload, res);
    } finally {
        await host.stop();
    }
}

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const res = new Checks('gamepad-rumble-e2e');
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await run(host, mode, res);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
