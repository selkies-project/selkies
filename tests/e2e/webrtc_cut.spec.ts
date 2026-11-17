/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A WebRTC page riding over a cut of its signaling socket, end to end.
//
// The page reaches the server through a relay that can cut every connection
// at once, with no close frame either way, as an ingress retiring its
// connections does; the media path (ICE, straight to the server) never passes
// it. The session carries on: the picture and sound run on through the cut,
// the page keeps its peer connection and the server its media session (no
// session ends, none starts), no notice shows, and keys typed in the gap reach
// the desktop in order; with the video switched off the sound plays on. A cut
// the server does not see (the page's side alone, a half-open connection)
// resumes the same way. A relay refusing the page past the server's hold ends
// the session, and the page starts a new one.
import { spawn } from 'node:child_process';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { FF_E2E_PROFILE, firefoxPersistentContext, launchBrowser, openh264Version, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { CutRelay, cutState, HEIGHT, HOOKS, keycodes, longestStall, pageNow, post, waitUntil, WIDTH } from '../lib/cut.ts';
import { pulseSine, pulseUnload } from '../lib/desktop.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host, suitesBin, type InputWatch } from '../lib/host.ts';
import { PC_TAP_JS } from '../lib/page.ts';
import { closeBrowser, waitWrVideo, within } from '../lib/stream.ts';

// How long the signaling registry holds a vanished peer's session (selkies-rtc's SESSION_HOLD).
const SESSION_HOLD_S = 3;
const RELAY_PORT = Number(process.env.E2E_CUT_PORT ?? '18215');
// Cuts are spaced so each meets a settled session.
const SETTLE_S = 5;
// The longest the picture may stand still through a resumed cut: the scene paints at 60 fps.
const STALL_MS = 200;

interface Heard {
    packets: number;
    samples: number;
    concealed: number;
}

/** The sound the page's live peer connections received. */
function audio(page: Page): Promise<Heard> {
    return within(page.evaluate<Heard>(`(async () => {
  const out = {packets: 0, samples: 0, concealed: 0};
  for (const pc of window.__pcs || []) {
    if (pc.connectionState === 'closed') continue;
    (await pc.getStats()).forEach((r) => {
      if (r.type !== 'inbound-rtp' || r.kind !== 'audio') return;
      out.packets += r.packetsReceived || 0;
      out.samples += r.totalSamplesReceived || 0;
      out.concealed += r.concealedSamples || 0;
    });
  }
  return out;
})()`));
}

/** The connection state of each peer connection the page made. */
function pcs(page: Page): Promise<string[]> {
    return within(page.evaluate<string[]>('(window.__pcs || []).map((pc) => pc.connectionState)'));
}

function sessionLines(log: string): string[] {
    return log.split('\n').filter((l) => l.includes('resumed its session') || l.includes('Session started for peer') || l.includes('session ended') || l.includes('is held'));
}

interface Session {
    res: Checks;
    host: Host;
    page: Page;
    relay: CutRelay;
    watches: InputWatch[];
}

/**
 * Type through a cut: two keys before it, four in its first 0.7 s, two after;
 * every one arrives, in order, and the stream runs on.
 */
async function typingCut(s: Session, label: string, serverSide = true): Promise<void> {
    const { res, host, page, relay } = s;
    const watch = await host.watchInput();
    s.watches.push(watch);
    await page.mouse.click(WIDTH / 2, HEIGHT / 2);
    await sleep(1000);
    const codes = await keycodes(host, 'abcdefgh');
    await page.keyboard.press('a');
    await page.keyboard.press('b');
    await sleep(500);
    const offset = await host.mark();
    const before = await pcs(page);
    const heard0 = await audio(page);
    const cutAt = await pageNow(page);
    res.check(`${label}: the relay cut the page's signaling socket`, relay.cut({ serverSide }) > 0);
    for (const [delay, ch] of [[50, 'c'], [200, 'd'], [450, 'e'], [700, 'f']] as const) {
        await waitUntil(page, cutAt + delay);
        await page.keyboard.press(ch);
    }
    await sleep(3500);
    const heard1 = await audio(page);
    const heard = { packets: heard1.packets - heard0.packets, samples: heard1.samples - heard0.samples, concealed: heard1.concealed - heard0.concealed };
    await page.keyboard.press('g');
    await page.keyboard.press('h');
    await sleep(1000);
    const events = await watch.events();
    const state = await cutState(page);
    const log = await host.log(offset);
    const arrived = events.filter((e) => e.kind === 'KeyPress' && codes.has(e.detail)).map((e) => codes.get(e.detail)).join('');
    res.check(`${label}: every key typed before, through, and after the cut reaches the desktop in order`, arrived === 'abcdefgh', arrived);
    const stall = longestStall(state.frames, cutAt);
    res.check(`${label}: the picture runs on through the cut`, stall < STALL_MS, `${stall.toFixed(0)} ms`);
    const shown = state.overlay.filter((o) => o[0] > cutAt && o[1]);
    res.check(`${label}: no notice shows`, shown.length === 0, shown.slice(0, 2));
    res.check(`${label}: sound plays on through the cut`, heard.packets > 100 && heard.concealed < 0.05 * Math.max(heard.samples, 1), heard);
    const now = await pcs(page);
    const kept = JSON.stringify(now) === JSON.stringify(before) && JSON.stringify(before) === JSON.stringify([...before.slice(0, -1), 'connected']);
    res.check(`${label}: the page keeps its peer connection`, kept, [before, now]);
    const lines = sessionLines(log);
    res.check(`${label}: the server resumes the session: none ends, none starts`,
        lines.some((l) => l.includes('resumed its session')) && !lines.some((l) => l.includes('Session started') || l.includes('session ended')), lines.slice(0, 4));
    if (serverSide) res.check(`${label}: the server saw the cut and held the session for it`, lines.some((l) => l.includes('is held')), lines.slice(0, 4));
}

/** Sound plays on through a cut while the video is switched off, and no notice waits for a frame. */
async function pausedCut(s: Session): Promise<void> {
    const { res, page, relay } = s;
    await post(page, "{type: 'pipelineControl', pipeline: 'video', enabled: false}");
    await sleep(2000);
    const before = await audio(page);
    const cutAt = await pageNow(page);
    relay.cut();
    await sleep(6000);
    const after = await audio(page);
    res.check('paused: sound plays on through the cut', after.packets - before.packets > 200, [before, after]);
    const shown = (await cutState(page)).overlay.filter((o) => o[0] > cutAt && o[1]);
    res.check('paused: no notice covers the reconnect', shown.length === 0, shown.slice(0, 2));
    await post(page, "{type: 'pipelineControl', pipeline: 'video', enabled: true}");
    await sleep(3000);
}

/** A cut the page cannot come back from within the hold ends the session, and the page starts a new one. */
async function slowCut(s: Session): Promise<void> {
    const { res, host, page, relay } = s;
    const count = (await pcs(page)).length;
    const offset = await host.mark();
    const cutAt = await pageNow(page);
    relay.cut({ refuseS: SESSION_HOLD_S + 2 });
    await sleep((SESSION_HOLD_S + 10) * 1000);
    const state = await cutState(page);
    const lines = sessionLines(await host.log(offset));
    res.check('slow: a session nobody resumes within the hold ends',
        lines.some((l) => l.includes('session ended')) && !lines.some((l) => l.includes('resumed')), lines.slice(0, 4));
    const now = await pcs(page);
    res.check('slow: the page starts a new one', lines.some((l) => l.includes('Session started')) && now.length === count + 1, [lines.slice(-2), now]);
    const moving = state.frames.filter((f) => f[0] > cutAt + (SESSION_HOLD_S + 6) * 1000);
    res.check('slow: the picture moves again', moving.length > 30, moving.length);
}

async function session(engine: Engine): Promise<void> {
    const res = new Checks(`webrtc-cut-${engine}`);
    if (engine === 'firefox' && !openh264Version()) {
        res.skip(`no OpenH264 GMP plugin in ${FF_E2E_PROFILE}`, 'run tests/tools/fetch-openh264.sh to cover H.264 in Firefox');
        res.finish();
        return;
    }
    const host = new Host(suiteOf(import.meta.filename));
    let tone: string | null = null;
    let relay: CutRelay | null = null;
    const watches: InputWatch[] = [];
    try {
        const { port } = await host.start({ label: engine, mode: 'webrtc', env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_STUN_HOST: '', SELKIES_TURN_REST_URI: '' } });
        relay = await CutRelay.start(RELAY_PORT, port);
        const painter = spawn(suitesBin('motion-scene'), [process.env.E2E_DISPLAY ?? '', String(WIDTH), String(HEIGHT), '60'], { stdio: 'ignore' });
        try {
            tone = pulseSine();
            const viewport = { width: WIDTH, height: HEIGHT };
            let owner: Browser | BrowserContext;
            let ctx: BrowserContext;
            if (engine === 'firefox') {
                ctx = await firefoxPersistentContext({ viewport, deviceScaleFactor: 1 });
                owner = ctx;
            } else {
                const browser = await launchBrowser('chromium');
                owner = browser;
                ctx = await browser.newContext({ viewport, deviceScaleFactor: 1 });
            }
            await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'webrtc';" });
            await ctx.addInitScript({ content: PC_TAP_JS });
            await ctx.addInitScript({ content: HOOKS });
            const page = await ctx.newPage();
            const errors: string[] = [];
            page.on('pageerror', (e) => errors.push(String(e)));
            await page.goto(`http://127.0.0.1:${RELAY_PORT}`, { waitUntil: 'load', timeout: 90_000 });
            res.check('the page streams with its signaling through the relay', Boolean(await waitWrVideo(page, 60)));
            await page.mouse.click(WIDTH / 2, HEIGHT / 2);
            await sleep(SETTLE_S * 1000);
            const s: Session = { res, host, page, relay, watches };
            await typingCut(s, 'cut');
            await sleep(SETTLE_S * 1000);
            await typingCut(s, 'again');
            if (engine === 'chromium') {
                await sleep(SETTLE_S * 1000);
                await typingCut(s, 'half-open', false);
                await sleep(SETTLE_S * 1000);
                await pausedCut(s);
                await sleep(SETTLE_S * 1000);
                await slowCut(s);
            }
            res.check('no page errors', errors.length === 0, errors.join('; ').slice(0, 300));
            await closeBrowser(owner);
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
