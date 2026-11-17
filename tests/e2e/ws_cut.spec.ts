/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A page riding over a cut of its session socket, end to end.
//
// The page reaches the server through a relay that can cut every connection
// at once, with no close frame either way: what an ingress does when it
// retires the process holding them (NRP's HAProxy cuts each socket at its
// hard-stop-after, ten minutes after a reload). The page reconnects in place
// at once, and a quick reconnect is one nobody notices: the picture stalls for
// no longer than the new socket takes, no notice covers it, playback keeps its
// context and runs on, the server restarts nothing of its audio capture, and
// keys typed in the gap reach the desktop in order. Keys and a button held
// across the cut stay held, the microphone stays on, and video and audio the
// user switched off stay off. Sound plays on through a cut while the video is
// paused (switched off, or a hidden tab's), though no frame comes to say the
// reconnect landed, and a shown tab gets its picture back. A socket back
// before its first frame (the relay holds the video) keeps the sound playing
// and shows the notice until the picture comes. A cut the server does not see
// (the page's side alone, a half-open connection) is ridden over the same way.
// A reconnect that cannot be quick (the relay refuses connections for a
// while) shows the notice once it has waited, and the stream comes back after.
//
// firefox runs the typing cut and the paused, hidden and late ones alone:
// Gecko routes the worker socket through the page's thread.
import { spawn, spawnSync } from 'node:child_process';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { chromium, test } from '@playwright/test';

import { BROWSER_ARGS, launchBrowser, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { CutRelay, cutState, HEIGHT, HOOKS, keycodes, longestSilence, longestStall, pageNow, post, waitUntil, WIDTH } from '../lib/cut.ts';
import { pulseSine, pulseUnload } from '../lib/desktop.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host, suitesBin, type InputWatch } from '../lib/host.ts';
import { closeBrowser, waitWsVideo, within } from '../lib/stream.ts';

const RELAY_PORT = Number(process.env.E2E_CUT_PORT ?? '18214');
// A drop counts as a reconnect in place only for a socket that held this long
// (the client's RECONNECT_STABLE_MS), so cuts are spaced past it.
const STABLE_S = 11;
// The longest the picture may stall and playback stay silent through a quick
// reconnect, by engine: Gecko holds a socket to a host whose last one failed
// for 200 to 400 ms first (network.websocket.delay-failed-reconnects).
const QUICK_MS: Record<string, [number, number]> = { chromium: [250, 120], firefox: [550, 400] };
const CUT_REPORT = /The connection to wss?:\/\/\S+ was interrupted while the page was loading\.$/;
const SHIFT_L = 0xffe1;

/** pcmflux's playback streams on the sound server (the microphone's), by index. */
function micPlaybacks(): string[] {
    const listing = spawnSync('pactl', ['list', 'sink-inputs'], { encoding: 'utf8', timeout: 10_000 }).stdout ?? '';
    return listing.split('Sink Input #').slice(1).filter((b) => b.includes('application.name = "pcmflux"')).map((b) => b.split('\n', 1)[0].trim());
}

interface Session {
    res: Checks;
    host: Host;
    page: Page;
    relay: CutRelay;
    watches: InputWatch[];
}

/**
 * Type through a cut: two keys before it, four in its first 0.7 s, two after
 * the stream is back; every one arrives, in order.
 */
async function typingCut(s: Session, label: string, engine: Engine, serverSide = true): Promise<void> {
    const { res, host, page, relay } = s;
    const watch = await host.watchInput();
    s.watches.push(watch);
    await page.mouse.move(WIDTH / 2, HEIGHT / 2);
    await page.mouse.click(WIDTH / 2, HEIGHT / 2);
    await sleep(1000);
    const codes = await keycodes(host, 'abcdefgh');
    await page.keyboard.press('a');
    await page.keyboard.press('b');
    await sleep(500);
    const offset = await host.mark();
    const contexts = (await cutState(page)).acs;
    const cutAt = await pageNow(page);
    res.check(`${label}: the relay cut the page's connections`, relay.cut({ serverSide }) > 0);
    for (const [delay, ch] of [[50, 'c'], [200, 'd'], [450, 'e'], [700, 'f']] as const) {
        await waitUntil(page, cutAt + delay);
        await page.keyboard.press(ch);
    }
    await sleep(4000);
    await page.keyboard.press('g');
    await page.keyboard.press('h');
    await sleep(1000);
    const events = await watch.events();
    const state = await cutState(page);
    const log = await host.log(offset);
    const arrived = events.filter((e) => e.kind === 'KeyPress' && codes.has(e.detail)).map((e) => codes.get(e.detail)).join('');
    res.check(`${label}: every key typed before, through, and after the cut reaches the desktop in order`, arrived === 'abcdefgh', arrived);
    const [stallMs, silenceMs] = QUICK_MS[engine];
    const stall = longestStall(state.frames, cutAt);
    res.check(`${label}: the picture stalls no longer than the new socket takes`, stall < stallMs, `${stall.toFixed(0)} ms`);
    const shown = state.overlay.filter((o) => o[0] > cutAt && o[1]);
    res.check(`${label}: no notice covers a quick reconnect`, shown.length === 0, shown.slice(0, 2));
    res.check(`${label}: playback keeps its context`, state.acs === contexts, [contexts, state.acs]);
    const silence = longestSilence(state.levels, cutAt);
    res.check(`${label}: playback runs on through the cut`, silence < silenceMs, `${silence.toFixed(0)} ms silent`);
    res.check(`${label}: the server restarts nothing of its audio capture`, !log.includes('RED gate changed') && !log.includes('Stopping pcmflux'),
        log.split('\n').filter((l) => l.includes('RED gate') || l.includes('Stopping pcmflux')).slice(0, 2));
    if (serverSide) {
        res.check(`${label}: the server sees a cut, and the page reclaims its display in the grace`,
            log.includes('code 1006') && log.includes('claimed by a new connection'), '');
    }
}

/** A key and a mouse button held across the cut stay held, and let go when the user lets go. */
async function heldCut(s: Session): Promise<void> {
    const { res, host, page, relay } = s;
    const read = async (): Promise<[boolean | null, number[]]> => [await host.keyHeld(SHIFT_L), await host.buttonsHeld()];
    const is = (r: [boolean | null, number[]], key: boolean, buttons: number[]): boolean => r[0] === key && JSON.stringify(r[1]) === JSON.stringify(buttons);
    await page.mouse.move(WIDTH / 2, HEIGHT / 2);
    await page.keyboard.down('Shift');
    await page.mouse.down();
    await sleep(600);
    const before = await read();
    relay.cut();
    await sleep(1500);
    const during = await read();
    await page.mouse.up();
    await page.keyboard.up('Shift');
    await sleep(600);
    const after = await read();
    res.check('held: a key and a button held before the cut', is(before, true, [1]), before);
    res.check('held: both are still held once the page is back', is(during, true, [1]), during);
    res.check('held: both let go when the user lets go', is(after, false, []), after);
}

/**
 * Sound plays on through a cut while the video is paused, as a hidden tab
 * pauses it: no frame comes to say the reconnect landed, and none has to.
 */
async function pausedCut(s: Session): Promise<void> {
    const { res, page, relay } = s;
    await post(page, "{type: 'pipelineControl', pipeline: 'video', enabled: false}");
    await sleep(2000);
    const cutAt = await pageNow(page);
    relay.cut();
    await sleep(6000);
    const state = await cutState(page);
    const loud = state.levels.filter(([t, lv]) => t > cutAt + 3000 && lv >= 0.01);
    res.check("paused: sound plays on past the notice's wait", loud.length > 10, loud.length);
    const shown = state.overlay.filter((o) => o[0] > cutAt && o[1]);
    res.check('paused: no notice covers the reconnect', shown.length === 0, shown.slice(0, 2));
    await post(page, "{type: 'pipelineControl', pipeline: 'video', enabled: true}");
    await sleep(3000);
}

/** Hide or show the page's tab, as the browser tells it. */
async function hide(page: Page, hidden: boolean): Promise<void> {
    await within(page.evaluate((h) => {
        Object.defineProperty(document, 'hidden', { configurable: true, get: () => h });
        Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => (h ? 'hidden' : 'visible') });
        document.dispatchEvent(new Event('visibilitychange'));
    }, hidden));
}

/**
 * A hidden tab's paused video brings no frame through a cut: the sound plays
 * on, no notice waits for it, and the picture comes back when the tab is shown.
 */
async function hiddenCut(s: Session): Promise<void> {
    const { res, page, relay } = s;
    await hide(page, true);
    await sleep(2000);
    const cutAt = await pageNow(page);
    relay.cut();
    await sleep(6000);
    let state = await cutState(page);
    const loud = state.levels.filter(([t, lv]) => t > cutAt + 3000 && lv >= 0.01);
    res.check("hidden: sound plays on past the notice's wait", loud.length > 10, loud.length);
    const shown = state.overlay.filter((o) => o[0] > cutAt && o[1]);
    res.check('hidden: no notice waits for the paused video', shown.length === 0, shown.slice(0, 2));
    const shownAt = await pageNow(page);
    await hide(page, false);
    await sleep(4000);
    state = await cutState(page);
    const moving = state.frames.filter((f) => f[0] > shownAt + 1000);
    res.check('hidden: the picture comes back when the tab is shown', moving.length > 30, moving.length);
}

/**
 * A socket back before its first frame: the notice shows once it has waited,
 * the sound plays on, and both settle when the picture comes.
 */
async function lateCut(s: Session): Promise<void> {
    const { res, page, relay } = s;
    const cutAt = await pageNow(page);
    relay.cut({ holdVideoS: 3 });
    await sleep(7000);
    const state = await cutState(page);
    const shown = state.overlay.filter((o) => o[0] > cutAt && o[1]).map((o) => o[0] - cutAt);
    res.check('late: the notice shows once the first frame is late', shown.length > 0 && shown[0] >= 1400, shown.slice(0, 2).map(Math.round));
    const hidden = state.overlay.filter((o) => o[0] > cutAt && !o[1]).map((o) => o[0] - cutAt);
    res.check('late: and goes with the first frame', shown.length > 0 && hidden.length > 0 && hidden[hidden.length - 1] > shown[0], hidden.slice(-2).map(Math.round));
    const loud = state.levels.filter(([t, lv]) => t > cutAt + 2000 && lv >= 0.01);
    res.check("late: sound plays on past the notice's wait", loud.length > 10, loud.length);
    const moving = state.frames.filter((f) => f[0] > cutAt + 4000);
    res.check('late: the picture moves again', moving.length > 30, moving.length);
}

/** The microphone the user switched on stays on, and video and audio the user switched off stay off. */
async function togglesCut(s: Session): Promise<void> {
    const { res, page, relay } = s;
    await post(page, "{type: 'pipelineControl', pipeline: 'microphone', enabled: true}");
    for (const until = Date.now() + 15_000; Date.now() < until && !micPlaybacks().length;) await sleep(250);
    const first = micPlaybacks();
    res.check('toggles: the microphone reaches the server', first.length > 0, first);
    await post(page, "{type: 'pipelineControl', pipeline: 'video', enabled: false}");
    await post(page, "{type: 'pipelineControl', pipeline: 'audio', enabled: false}");
    await sleep(STABLE_S * 1000);
    const cutAt = await pageNow(page);
    relay.cut();
    const fresh = (): string[] => micPlaybacks().filter((m) => !first.includes(m));
    for (const until = Date.now() + 15_000; Date.now() < until && !fresh().length;) await sleep(250);
    res.check('toggles: the microphone is on again on the new socket', fresh().length > 0, [first, micPlaybacks()]);
    const mic = (await cutState(page)).mic;
    res.check('toggles: and the page shows it on', mic.length > 0 && mic[mic.length - 1], mic.slice(-3));
    await sleep(3000);
    const state = await cutState(page);
    const moved = state.frames.filter((f) => f[0] > cutAt + 1000);
    res.check('toggles: video the user stopped stays stopped', moved.length === 0, `${moved.length} picture changes`);
    const loud = state.levels.filter(([t, lv]) => t > cutAt + 1000 && lv >= 0.01);
    res.check('toggles: audio the user stopped stays stopped', loud.length === 0, `${loud.length} audible samples`);
    await post(page, "{type: 'pipelineControl', pipeline: 'microphone', enabled: false}");
    await post(page, "{type: 'pipelineControl', pipeline: 'video', enabled: true}");
    await post(page, "{type: 'pipelineControl', pipeline: 'audio', enabled: true}");
    await sleep(3000);
    const now = await pageNow(page);
    const moving = (await cutState(page)).frames.filter((f) => f[0] > now - 2000);
    res.check('toggles: video comes back when the user starts it', moving.length > 30, moving.length);
}

/** A reconnect that cannot be quick shows the notice once it has waited, and the stream comes back. */
async function slowCut(s: Session): Promise<void> {
    const { res, page, relay } = s;
    const cutAt = await pageNow(page);
    relay.cut({ refuseS: 3 });
    await sleep(8000);
    const state = await cutState(page);
    const shown = state.overlay.filter((o) => o[0] > cutAt && o[1]).map((o) => o[0] - cutAt);
    res.check('slow: the notice shows once the reconnect has waited', shown.length > 0 && shown[0] >= 1400, shown.slice(0, 2).map(Math.round));
    const hidden = state.overlay.filter((o) => o[0] > cutAt && !o[1]).map((o) => o[0] - cutAt);
    res.check('slow: and goes once the stream is back', shown.length > 0 && hidden.length > 0 && hidden[hidden.length - 1] > shown[0], hidden.slice(-2).map(Math.round));
    const moving = state.frames.filter((f) => f[0] > cutAt + 4000);
    res.check('slow: the picture moves again', moving.length > 30, moving.length);
    const loud = state.levels.filter(([t, lv]) => t > cutAt + 6000 && lv >= 0.01);
    res.check('slow: and sound plays again', loud.length > 10, loud.length);
}

async function launch(engine: Engine): Promise<[Browser, BrowserContext]> {
    let browser: Browser;
    let ctx: BrowserContext;
    if (engine === 'chromium') {
        // The fake device plays a tone into getUserMedia; the headless shell has no media capture.
        const executablePath = process.env.E2E_CHROME || undefined;
        browser = await chromium.launch({ headless: true, args: [...BROWSER_ARGS, '--use-fake-device-for-media-stream'], ...(executablePath ? { executablePath } : { channel: 'chromium' }) });
        ctx = await browser.newContext({ viewport: { width: WIDTH, height: HEIGHT }, permissions: ['microphone'] });
    } else {
        browser = await launchBrowser(engine);
        ctx = await browser.newContext({ viewport: { width: WIDTH, height: HEIGHT } });
    }
    await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
    await ctx.addInitScript({ content: HOOKS });
    return [browser, ctx];
}

async function session(engine: Engine): Promise<void> {
    const res = new Checks(`ws-cut-${engine}`);
    const host = new Host(suiteOf(import.meta.filename));
    let tone: string | null = null;
    let relay: CutRelay | null = null;
    const watches: InputWatch[] = [];
    try {
        const { port } = await host.start({ label: engine, mode: 'websockets', env: { SELKIES_WEB_ROOT: DASHBOARD_DIST } });
        relay = await CutRelay.start(RELAY_PORT, port);
        const painter = spawn(suitesBin('motion-scene'), [process.env.E2E_DISPLAY ?? '', String(WIDTH), String(HEIGHT), '60'], { stdio: 'ignore' });
        try {
            tone = pulseSine();
            const [browser, ctx] = await launch(engine);
            const page = await ctx.newPage();
            const errors: string[] = [];
            // Gecko now and then reports a socket the cut drops as an error of its
            // own, "interrupted while the page was loading" (1 run in 16 on 4
            // CPUs); the check is for the page's.
            page.on('pageerror', (e) => { if (!CUT_REPORT.test(String(e))) errors.push(String(e)); });
            await page.goto(`http://127.0.0.1:${RELAY_PORT}`, { waitUntil: 'load', timeout: 90_000 });
            res.check('the page streams through the relay', (await waitWsVideo(page, 60)) !== null);
            await page.mouse.click(WIDTH / 2, HEIGHT / 2);
            await sleep(STABLE_S * 1000);
            const s: Session = { res, host, page, relay, watches };
            await typingCut(s, 'cut', engine);
            if (engine === 'chromium') {
                await sleep(STABLE_S * 1000);
                await typingCut(s, 'half-open', engine, false);
                await sleep(STABLE_S * 1000);
                await heldCut(s);
            }
            await sleep(STABLE_S * 1000);
            await pausedCut(s);
            await sleep(STABLE_S * 1000);
            await hiddenCut(s);
            await sleep(STABLE_S * 1000);
            await lateCut(s);
            if (engine === 'chromium') {
                await sleep(STABLE_S * 1000);
                await togglesCut(s);
                await sleep(STABLE_S * 1000);
                await slowCut(s);
            }
            res.check('no page errors', errors.length === 0, errors.join('; ').slice(0, 300));
            await closeBrowser(browser);
        } finally {
            painter.kill('SIGKILL');
        }
    } finally {
        for (const w of watches) await w.release();
        pulseUnload(tone);
        await host.stop();
        await relay?.stop();
        await host.close();
    }
    res.finish();
}

for (const engine of ['chromium', 'firefox'] as const) {
    test(engine, async () => { await session(engine); });
}
