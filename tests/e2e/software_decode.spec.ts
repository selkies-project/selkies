/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The software-decode retry that sits ahead of the client's fallback ladder.
//
// A hardware H.264 decoder can accept its config and then fail at decode() --
// isConfigSupported does not predict it -- so the client retries the same
// encoder with hardwareAcceleration 'prefer-software' before the ladder
// closes the socket, degrades the encoder, and reloads. A decoder can also
// take its config and then neither output a frame nor error, so nothing
// signals the ladder; the silent block injects that and checks the no-output
// watchdog trips the same retry. These blocks inject exactly those failures
// into a real browser and check each arm of the decision.
//
// The nowebcodecs block is the rung below the ladder: an engine with no
// WebCodecs at all is not refused but pinned to the striped-JPEG encoder at
// pre-flight, and the dashboard offers it nothing else.
//
// The nosoftware block is an engine with no software H.264 decoder, whose
// refusal of the preference must not read as a refusal of the codec; the
// nostart block one that claims a software decoder and starts none, whose
// failure must not count as a crash.
//
// The newtab block follows the preference past its tab: it holds for the tab
// that met the failure, and a new tab of the same profile starts on hardware
// again.
//
// The cleared and broken blocks follow the ladder's JPEG past its tab: a fault
// that clears gives a new tab its video back, and one that stays takes it to
// JPEG at its first crash. The held block follows the settings a crash resets
// the same way: the user's own come back in a new tab, and one picked again in
// the crashing tab stays picked.
//
// The hidden block is a background tab: a hidden page drops what it decodes,
// so video still sent to it must not read as a silent decoder and take it
// through the retry and the ladder's reloads to JPEG, and a page whose socket
// opens hidden (a reload or a reconnect in a background tab) pauses its video
// as a hide would have, and resumes it when shown.
//
// Every server runs Turbo, so the still test display keeps sending the frames
// the blocks count as decoded.
import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchBrowser, launchChromium, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { openVideoSettings } from '../lib/dashboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { pageFps, waitWsVideo, within } from '../lib/stream.ts';

const TURBO = { SELKIES_VIDEO_STREAMING_MODE: 'true' };

// Storage namespace the client derives from origin + pathname.
const STORAGE_PREFIX_JS = "(location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_')";
const STORAGE_KEY_JS = `(${STORAGE_PREFIX_JS} + '_prefer_software_decode')`;
const ENCODER_KEY_JS = `(${STORAGE_PREFIX_JS} + '_encoder')`;

/**
 * Make every VideoDecoder in the page fail on a chosen path and record the
 * acceleration each decoder was configured with. hardware/all raise an
 * asynchronous decode error, once only at the first hardware decode of a load;
 * silent accepts the chunk on the hardware path but never outputs and never
 * errors, the case the no-output watchdog exists for. `loads` confines the
 * failure to the tab's first that many loads, a fault that clears up; 0 keeps
 * it for every load. `softH264` no is an engine with no software H.264
 * decoder, as Chromium without FFmpeg's video decoders: it refuses the
 * software preference for H.264, in isConfigSupported and at configure;
 * claims is one that takes the preference in isConfigSupported and then
 * starts no decoder for it.
 */
function shimJs(failMode: string, loads = 0, softH264 = 'yes'): string {
    return `
    (() => {
      const Real = window.VideoDecoder;
      if (!Real) return;
      const MODE = ${loads} && Number(sessionStorage.getItem('__navs') || 0) > ${loads} ? 'none' : '${failMode}';
      const SOFT_H264 = '${softH264}';
      const softH264 = (cfg) => !!cfg && cfg.hardwareAcceleration === 'prefer-software'
        && /^avc1/.test(cfg.codec || '');
      const realSupported = Real.isConfigSupported.bind(Real);
      Real.isConfigSupported = (cfg) => SOFT_H264 === 'no' && softH264(cfg)
        ? Promise.resolve({ supported: false, config: cfg }) : realSupported(cfg);
      let failedOnce = false;
      const errorFor = new WeakMap(), softFor = new WeakMap();
      const record = (accel) => {
        const seen = JSON.parse(sessionStorage.getItem('__cfgs') || '[]');
        seen.push(accel);
        sessionStorage.setItem('__cfgs', JSON.stringify(seen));
      };
      const proto = Real.prototype;
      const origConfigure = proto.configure, origDecode = proto.decode;
      proto.configure = function (cfg) {
        const accel = (cfg && cfg.hardwareAcceleration) || 'default';
        softFor.set(this, accel === 'prefer-software');
        record(accel);
        if (SOFT_H264 !== 'yes' && softH264(cfg)) {
          // What Chromium reports when no decoder will start for the config.
          const cb = errorFor.get(this);
          if (cb) setTimeout(() => cb(new DOMException('Unsupported configuration.', 'OperationError')), 0);
          return;
        }
        return origConfigure.call(this, cfg);
      };
      proto.decode = function (chunk) {
        const soft = softFor.get(this) === true;
        const once = MODE === 'once' && !soft && !failedOnce;
        if (once) failedOnce = true;
        if (MODE === 'all' || (MODE === 'hardware' && !soft) || once) {
          const cb = errorFor.get(this);
          if (cb) setTimeout(() => cb(new DOMException('injected decode failure',
                                                       'EncodingError')), 0);
          return;
        }
        // Swallow the chunk on the hardware path: no frame out, no error raised.
        if (MODE === 'silent' && !soft) return;
        return origDecode.call(this, chunk);
      };
      window.VideoDecoder = new Proxy(Real, {
        construct(target, args) {
          const init = Object.assign({}, (args && args[0]) || {});
          const out = init.output;
          init.output = (frame) => {
            window.__decoded = (window.__decoded || 0) + 1;
            if (out) out(frame);
          };
          const inst = Reflect.construct(target, [init]);
          errorFor.set(inst, init.error);
          return inst;
        },
      });
    })();
    `;
}

// Every WebCodecs global removed, the way an engine without the API presents.
const NO_WEBCODECS_JS = `
  (() => {
    for (const name of ['VideoDecoder', 'VideoEncoder', 'VideoFrame', 'EncodedVideoChunk', 'ImageDecoder']) {
      try { Object.defineProperty(window, name, { value: undefined, configurable: true, writable: true }); } catch (e) {}
    }
  })();
`;

const NAV_JS = `
  (() => {
    const n = Number(sessionStorage.getItem('__navs') || 0) + 1;
    sessionStorage.setItem('__navs', String(n));
    // Where each load's decoders start in the configure record.
    const at = JSON.parse(sessionStorage.getItem('__navAt') || '[]');
    at.push(JSON.parse(sessionStorage.getItem('__cfgs') || '[]').length);
    sessionStorage.setItem('__navAt', JSON.stringify(at));
  })();
`;

const SEED_JS = `try { sessionStorage.setItem(${STORAGE_KEY_JS}, '1'); } catch (e) {}`;

const encoderJs = (encoder: string): string => `
  try {
    const app = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
    localStorage.setItem(app + '_encoder', '${encoder}');
  } catch (e) {}
`;

// A load in a background tab: hidden before the client runs, so no visibilitychange reaches it.
const HIDDEN_LOAD_JS = `
  Object.defineProperty(document, 'hidden', {configurable: true, get: () => true});
  Object.defineProperty(document, 'visibilityState', {configurable: true, get: () => 'hidden'});
`;

/**
 * The tab hidden or shown; `tell` fires the visibilitychange the browser
 * would. Untold, the page is hidden with nothing to pause its video, as when
 * video reaches a page already hidden.
 */
async function visibility(page: Page, hidden: boolean, tell: boolean): Promise<void> {
    await within(page.evaluate(([h, t]) => {
        Object.defineProperty(document, 'hidden', { configurable: true, get: () => h });
        Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => (h ? 'hidden' : 'visible') });
        if (t) document.dispatchEvent(new Event('visibilitychange'));
    }, [hidden, tell]));
}

const PICKS: Record<string, string> = { framerate: '120', video_crf: '18', manual_resolution: 'true', manual_width: '1280', manual_height: '720' };
const SEED_PICKS_JS = `(() => {
  if (sessionStorage.getItem('__picked')) return;
  sessionStorage.setItem('__picked', '1');
  const prefix = ${STORAGE_PREFIX_JS};
  for (const [k, v] of Object.entries(${JSON.stringify(PICKS)})) localStorage.setItem(prefix + '_' + k, v);
})();`;
const READ_PICKS_JS = `(() => {
  const prefix = ${STORAGE_PREFIX_JS}, out = {};
  for (const k of ${JSON.stringify(Object.keys(PICKS))}) out[k] = localStorage.getItem(prefix + '_' + k);
  return out;
})()`;

declare global {
    interface Window {
        stream_info?: { codec?: string };
        videoChunksReceived?: number;
    }
}

interface State {
    navs: number;
    cfgs: string[];
    navAt: number[];
    decoded: number;
    stored: string | null;
    codec: string | null;
    encoder: string | null;
}

const STATE_JS = `(() => ({
  navs: Number(sessionStorage.getItem('__navs') || 0),
  cfgs: JSON.parse(sessionStorage.getItem('__cfgs') || '[]'),
  navAt: JSON.parse(sessionStorage.getItem('__navAt') || '[]'),
  decoded: window.__decoded || 0,
  stored: (() => { try { return sessionStorage.getItem(${STORAGE_KEY_JS}); } catch (e) { return null; } })(),
  codec: (window.stream_info && window.stream_info.codec) || null,
  encoder: (() => { try { return localStorage.getItem(${ENCODER_KEY_JS}); } catch (e) { return null; } })(),
}))()`;

/** Page state, retried across the reloads the ladder triggers. */
async function readState(page: Page, retries = 6): Promise<State> {
    for (let attempt = 0; ; attempt++) {
        try {
            return await within(page.evaluate<State>(STATE_JS));
        } catch (e) {
            if (attempt === retries - 1) throw e;
            await sleep(1000);
        }
    }
}

/** Poll the page state until the predicate holds; the last state. */
async function waitFor(page: Page, predicate: (s: State) => boolean, timeoutS = 25): Promise<State> {
    let state: State | null = null;
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        state = await readState(page);
        if (predicate(state)) return state;
        await sleep(1000);
    }
    return state ?? readState(page);
}

const pageEval = <T>(page: Page, script: string): Promise<T> => within(page.evaluate<T>(script));

interface Client {
    browser: Browser;
    page: Page;
    lines: string[];
}

interface Opts {
    failMode?: string;
    seedPreference?: boolean;
    encoder?: string;
    query?: string;
    engine?: Engine;
    softH264?: string;
    hidden?: boolean;
}

/**
 * A browser with the decode-failure shim installed and the stream page
 * loaded; `lines` collects its console across reloads. The shim needs
 * WebCodecs, so on an engine without it (Playwright WebKit) it no-ops and the
 * client takes its jpeg path.
 */
async function openClient(url: string, o: Opts = {}): Promise<Client> {
    const engine = o.engine ?? 'chromium';
    const browser = engine === 'chromium' ? await launchChromium() : await launchBrowser(engine);
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
    await ctx.addInitScript({ content: NAV_JS });
    if (o.seedPreference) await ctx.addInitScript({ content: SEED_JS });
    if (o.encoder) await ctx.addInitScript({ content: encoderJs(o.encoder) });
    await ctx.addInitScript({ content: shimJs(o.failMode ?? 'none', 0, o.softH264 ?? 'yes') });
    if (o.hidden) await ctx.addInitScript({ content: HIDDEN_LOAD_JS });
    const page = await ctx.newPage();
    const lines: string[] = [];
    page.on('console', (m) => lines.push(m.text()));
    await page.goto(`${url}/${o.query ? `?${o.query}` : ''}`, { waitUntil: 'load' });
    return { browser, page, lines };
}

/** A fresh context of `browser` carrying `storage`, on its first load. */
async function freshTab(browser: Browser, storage: Awaited<ReturnType<BrowserContext['storageState']>> | undefined, url: string, scripts: string[]): Promise<[BrowserContext, Page]> {
    const ctx = await browser.newContext({ ...(storage ? { storageState: storage } : {}), viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
    for (const s of [NAV_JS, ...scripts]) await ctx.addInitScript({ content: s });
    const page = await ctx.newPage();
    await page.goto(url, { waitUntil: 'load' });
    return [ctx, page];
}

interface Block {
    res: Checks;
    host: Host;
    url: string;
    port: number;
    /** A fresh server on the same port, so a new tab's storage is what it streams from. */
    restart: (env?: Record<string, string>) => Promise<void>;
}

const has = (list: string[], item: string): boolean => list.includes(item);

/** Hardware decode fails: the client switches to software in place, keeps decoding, and does not reload. */
async function blockRetry({ res: r, url }: Block): Promise<void> {
    const { browser, page, lines } = await openClient(url, { failMode: 'hardware' });
    try {
        const state = await waitFor(page, (s) => s.decoded > 0 && has(s.cfgs, 'prefer-software'));
        r.check('switched to software decode', has(state.cfgs, 'prefer-software'), state.cfgs.slice(0, 6));
        r.check('frames decode after the switch', state.decoded > 0, state.decoded);
        r.check('preference stored for this tab', state.stored !== null, state.stored);
        // The ladder reloads 3s after a fatal error; outlast it.
        await sleep(6000);
        const after = await readState(page);
        r.check('page never reloaded', after.navs === 1, after.navs);
        r.check('still decoding', after.decoded > state.decoded, [state.decoded, after.decoded]);
        lines.length = 0;
        await page.reload({ waitUntil: 'load' });
        await waitFor(page, (s) => s.navs === 2 && s.decoded > 0);
        const said = lines.filter((l) => l.startsWith('[fallback]'));
        r.check('a reload of the tab says it decodes in software, and why', said.some((l) => l.includes('decodes video in software since') && l.includes('decoder error')), said);
    } finally {
        await browser.close();
    }
}

/**
 * An engine with no software H.264 decoder meets one hardware decode error:
 * the software retry it cannot take leaves the choice of decoder to the
 * engine, which decodes on, where reading the refused preference as a refused
 * codec would step the stream down the ladder for the rest of the tab's life.
 */
async function blockNosoftware({ res: r, url }: Block): Promise<void> {
    const { browser, page, lines } = await openClient(url, { failMode: 'once', softH264: 'no' });
    try {
        const state = await waitFor(page, (s) => s.cfgs.length > 1 && s.decoded > 0, 40);
        r.check('the failed decoder was rebuilt', state.cfgs.length > 1, state.cfgs.slice(0, 6));
        r.check('without the software preference the engine refuses', !has(state.cfgs, 'prefer-software'), state.cfgs.slice(0, 6));
        r.check('frames decode after the error', state.decoded > 0, state.decoded);
        await sleep(6000);
        const after = await readState(page);
        r.check('the stream stays H.264', after.codec === 'h264', after.codec);
        r.check('no fallback encoder stored', after.encoder === null, after.encoder);
        r.check('no software preference stored', after.stored === null, after.stored);
        r.check('page never reloaded', after.navs === 1, after.navs);
        r.check('still decoding', after.decoded > state.decoded, [state.decoded, after.decoded]);
        r.check('the console says why', lines.some((l) => l.includes('has no software decoder for avc1')));
    } finally {
        await browser.close();
    }
}

/**
 * An engine that takes the software preference for H.264 and then starts no
 * decoder for it meets one hardware decode error: the software retry puts out
 * no frame, and the decoder the engine picks gets the stream back without a
 * reload, where the crash ladder would reload the page on its safe settings
 * and count a crash toward JPEG.
 */
async function blockNostart({ res: r, url }: Block): Promise<void> {
    const { browser, page, lines } = await openClient(url, { failMode: 'once', softH264: 'claims' });
    try {
        const state = await waitFor(page, (s) => has(s.cfgs, 'prefer-software') && s.cfgs[s.cfgs.length - 1] === 'default' && s.decoded > 0, 40);
        r.check('software was tried', has(state.cfgs, 'prefer-software'), state.cfgs.slice(0, 6));
        r.check("then the engine's own decoder again", state.cfgs.length > 0 && state.cfgs[state.cfgs.length - 1] === 'default', state.cfgs.slice(-3));
        r.check('frames decode after the error', state.decoded > 0, state.decoded);
        await sleep(6000);
        const after = await readState(page);
        const crashes = await pageEval<string | null>(page, `localStorage.getItem(${STORAGE_PREFIX_JS} + '_crash_count')`);
        r.check('the stream stays H.264', after.codec === 'h264', after.codec);
        r.check('no fallback encoder stored', after.encoder === null, after.encoder);
        r.check('no software preference stored', after.stored === null, after.stored);
        r.check('no crash counted', crashes === null, crashes);
        r.check('page never reloaded', after.navs === 1, after.navs);
        r.check('still decoding', after.decoded > state.decoded, [state.decoded, after.decoded]);
        r.check('the console says why', lines.some((l) => l.includes('Software decode put out no frame')));
    } finally {
        await browser.close();
    }
}

/**
 * A tab's remembered preference is applied to the first decoder of its next
 * load, so a reload on a broken hardware path pays no failed decode.
 */
async function blockPersisted({ res: r, url }: Block): Promise<void> {
    const { browser, page } = await openClient(url, { failMode: 'hardware', seedPreference: true });
    try {
        const state = await waitFor(page, (s) => s.decoded > 0);
        r.check('first decoder is already software', state.cfgs[0] === 'prefer-software', state.cfgs.slice(0, 6));
        r.check('no hardware decoder was ever configured', !has(state.cfgs, 'default'), state.cfgs.slice(0, 6));
        r.check('frames decode from the start', state.decoded > 0, state.decoded);
        await sleep(6000);
        const after = await readState(page);
        r.check('page never reloaded', after.navs === 1, after.navs);
    } finally {
        await browser.close();
    }
}

/**
 * Hardware decode fails in one tab, which switches to software: a new tab of
 * the same profile, the fault gone, configures a hardware decoder first and
 * stores no preference.
 */
async function blockNewtab(b: Block): Promise<void> {
    const { res: r, url } = b;
    const client = await openClient(url, { failMode: 'hardware' });
    try {
        let state = await waitFor(client.page, (s) => s.decoded > 0 && has(s.cfgs, 'prefer-software'));
        r.check('the failing tab switched to software decode', has(state.cfgs, 'prefer-software'), state.cfgs.slice(0, 6));
        const storage = await client.page.context().storageState();
        await client.page.context().close();
        // A fresh server, so the new tab is the display's only client.
        await b.restart();
        const [, page] = await freshTab(client.browser, storage, `${url}/`, [shimJs('none')]);
        state = await waitFor(page, (s) => s.decoded > 0);
        r.check('a new tab decodes', state.decoded > 0, state.decoded);
        r.check('on a hardware decoder from the start', state.cfgs.length > 0 && !has(state.cfgs, 'prefer-software'), state.cfgs.slice(0, 6));
        r.check('with no preference stored', state.stored === null, state.stored);
    } finally {
        await client.browser.close();
    }
}

/** Software decode fails too: the ladder still runs, and the preference is dropped so the next load re-probes hardware. */
async function blockLadder({ res: r, url }: Block): Promise<void> {
    const { browser, page } = await openClient(url, { failMode: 'all' });
    try {
        // The reload is only half the event: wait for the decoder the reloaded
        // page configures, or a busy machine is read between the two and the
        // ladder looks like it stopped at software.
        const state = await waitFor(page, (s) => s.navs > 1 && s.navAt.length > 1 && s.cfgs.length > s.navAt[1], 60);
        r.check('software was tried first', has(state.cfgs, 'prefer-software'), state.cfgs.slice(0, 6));
        r.check('fallback ladder still reached', state.navs > 1, state.navs);
        // The preference is dropped on the way into the ladder, so the reloaded
        // page re-probes hardware rather than being pinned by a failure
        // software did not cause; it then arms its own retry, which sets the
        // key again. The first page may configure more than one software
        // decoder before the ladder reloads it, so the reloaded page's own
        // first decoder is read.
        const reloaded = state.navAt.length > 1 ? state.cfgs.slice(state.navAt[1]) : [];
        r.check('next load re-probes hardware', reloaded.length > 0 && reloaded[0] === 'default', { cfgs: state.cfgs.slice(0, 6), navAt: state.navAt });
    } finally {
        await browser.close();
    }
}

/** Nothing is injected: the retry must be invisible -- no software decoder and no stored preference on a client whose hardware decoder works. */
async function blockHealthy({ res: r, url }: Block): Promise<void> {
    const { browser, page } = await openClient(url);
    try {
        const state = await waitFor(page, (s) => s.decoded > 0);
        r.check('frames decode', state.decoded > 0, state.decoded);
        r.check('no software decoder configured', !has(state.cfgs, 'prefer-software'), state.cfgs.slice(0, 6));
        r.check('nothing persisted', state.stored === null, state.stored);
        await sleep(6000);
        const after = await readState(page);
        r.check('page never reloaded', after.navs === 1, after.navs);
    } finally {
        await browser.close();
    }
}

/**
 * No WebCodecs at all: the stream comes up as striped JPEG without a reload,
 * the encoder is pinned to jpeg for the session, and the dashboard's encoder
 * menu offers only what this engine can play. The next browser to take the
 * display, and a later visit of the same profile with WebCodecs, the way a
 * browser update brings it, stream video.
 */
async function blockNowebcodecs(b: Block): Promise<void> {
    const { res: r, url } = b;
    const browser = await launchChromium();
    try {
        const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: NAV_JS });
        await ctx.addInitScript({ content: NO_WEBCODECS_JS });
        let page = await ctx.newPage();
        const errors: string[] = [];
        page.on('pageerror', (e) => errors.push(String(e)));
        await page.goto(`${url}/`, { waitUntil: 'load' });
        const video = await waitWsVideo(page, 30);
        r.check('stream comes up without WebCodecs', Boolean(video), JSON.stringify(video));
        r.check('VideoDecoder really is absent', (await pageEval<string>(page, 'typeof window.VideoDecoder')) === 'undefined');
        const stored = await pageEval<string | null>(page, `localStorage.getItem(${ENCODER_KEY_JS})`);
        r.check('encoder pinned to jpeg', stored === 'jpeg', stored);
        const fps = await pageFps(page, 15);
        r.check('striped JPEG frames are painted', fps > 0, fps);
        const status = await pageEval<string>(page, "(() => { const el = document.querySelector('#status-display, .status-display, #status'); return el ? (el.classList.contains('hidden') ? '' : el.textContent) : ''; })()");
        r.check('no fatal WebCodecs message', !(status || '').includes('WebCodecs'), status);
        const navs = await pageEval<number>(page, "Number(sessionStorage.getItem('__navs') || 0)");
        r.check('page never reloaded', navs === 1, navs);
        const opened = await openVideoSettings(page, 'default');
        // The menu lists every allowed encoder and disables the ones this
        // engine cannot play, labeling them unsupported, so jpeg is the only
        // selectable option here.
        const options = opened ? await pageEval<{ value: string; disabled: boolean }[]>(page, "Array.from(document.querySelectorAll('#encoderSelect option')).map(o => ({ value: o.value, disabled: o.disabled }))") : [];
        const enabled = options.filter((o) => !o.disabled).map((o) => o.value);
        r.check('dashboard enables only the jpeg encoder', opened && enabled.length === 1 && enabled[0] === 'jpeg', JSON.stringify(options));
        r.check('no page errors', errors.length === 0, errors.join('; ').slice(0, 200));
        const storage = await ctx.storageState();
        await ctx.close();
        // The next controller, inside the reconnect grace, is a browser with
        // WebCodecs and nothing stored: the JPEG was the departed page's, not
        // the display's.
        const [other, otherPage] = await freshTab(browser, undefined, `${url}/`, []);
        page = otherPage;
        let state = await waitFor(page, (s) => s.codec !== null && s.codec !== 'jpeg', 30);
        r.check('the next browser to connect streams video', state.codec !== null && state.codec !== 'jpeg', state.codec);
        await other.close();
        // A fresh server, so what the later visit streams comes from its own storage.
        await b.restart({ SELKIES_WEB_ROOT: DASHBOARD_DIST });
        [, page] = await freshTab(browser, storage, `${url}/`, []);
        state = await waitFor(page, (s) => s.codec !== null);
        r.check('a later visit with WebCodecs streams video', state.codec !== null && state.codec !== 'jpeg', state.codec);
        r.check('and asks for no JPEG of its own', state.encoder !== 'jpeg', state.encoder);
    } finally {
        await browser.close();
    }
}

/**
 * Every decoder fails at decode() for a tab's first three loads and then
 * works, as through a GPU process restart or a driver hiccup: the crash ladder
 * still takes that tab to JPEG, which holds for that tab alone, so a new tab
 * of the profile streams video again. The page decodes on the main thread,
 * where the shim reaches its decoders.
 */
async function blockCleared(b: Block): Promise<void> {
    const { res: r, url } = b;
    const browser = await launchChromium();
    try {
        const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: NAV_JS });
        await ctx.addInitScript({ content: shimJs('all', 3) });
        let page = await ctx.newPage();
        const said: string[] = [];
        page.on('console', (m) => { if (m.text().startsWith('[fallback]')) said.push(m.text()); });
        await page.goto(`${url}/?offscreen_worker=false`, { waitUntil: 'load' });
        let state = await waitFor(page, (s) => s.navs > 3 && s.codec === 'jpeg', 90);
        r.check('the failing tab reaches JPEG at its fourth load', state.navs === 4 && state.codec === 'jpeg', { navs: state.navs, codec: state.codec });
        r.check('which says it streams JPEG for the third crash', said.some((l) => l.includes('streams the jpeg encoder (in place of') && l.includes('decoder crash 3')), said);
        r.check('and counts the crashes', said.some((l) => l.includes('since its last healthy session: 3')), said);
        await sleep(6000);
        const after = await readState(page);
        r.check('and stays there', after.navs === 4 && after.codec === 'jpeg', { navs: after.navs, codec: after.codec });
        const storage = await ctx.storageState();
        await ctx.close();
        // A fresh server, so what the new tab streams comes from its own storage.
        await b.restart();
        [, page] = await freshTab(browser, storage, `${url}/?offscreen_worker=false`, []);
        state = await waitFor(page, (s) => s.codec !== null);
        r.check('a new tab streams video once decode works', state.codec === 'h264', state.codec);
        r.check('with the pick the fallback replaced back', state.encoder === null, state.encoder);
        r.check('and frames present', (await pageFps(page, 15)) > 0);
    } finally {
        await browser.close();
    }
}

/**
 * A crash's safe settings hold for the crashing tab alone. With the user's
 * frame rate, quality and manual size stored, every decoder fails for the
 * tab's first load: it reloads on the ladder's safe values, a quality picked
 * again there stays picked, and a new tab of the profile has the rest of the
 * user's settings back.
 */
async function blockHeld(b: Block): Promise<void> {
    const { res: r, url } = b;
    const browser = await launchChromium();
    try {
        const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
        for (const s of [NAV_JS, SEED_PICKS_JS, shimJs('all', 1)]) await ctx.addInitScript({ content: s });
        let page = await ctx.newPage();
        await page.goto(`${url}/?offscreen_worker=false`, { waitUntil: 'load' });
        const state = await waitFor(page, (s) => s.navs >= 2 && s.codec !== null, 60);
        r.check('the crash reloads the tab', state.navs >= 2, { navs: state.navs });
        const held = await pageEval<Record<string, string | null>>(page, READ_PICKS_JS);
        const safe = { framerate: '60', video_crf: '25', manual_resolution: 'false', manual_width: null, manual_height: null };
        r.check("on the ladder's safe values", JSON.stringify(held) === JSON.stringify(safe), held);
        await pageEval(page, "window.postMessage({type: 'settings', settings: {video_crf: 30}}, window.location.origin)");
        await sleep(1000);
        const storage = await ctx.storageState();
        await ctx.close();
        // A fresh server, so what the new tab streams comes from its own storage.
        await b.restart();
        [, page] = await freshTab(browser, storage, `${url}/?offscreen_worker=false`, []);
        await waitFor(page, (s) => s.codec !== null);
        const picks = await pageEval<Record<string, string | null>>(page, READ_PICKS_JS);
        const kept = ['framerate', 'manual_resolution', 'manual_width', 'manual_height'];
        r.check("a new tab has the user's frame rate and manual size back", kept.every((k) => picks[k] === PICKS[k]), picks);
        r.check('and the quality picked again in the crashing tab', picks.video_crf === '30', picks);
    } finally {
        await browser.close();
    }
}

/**
 * Every decoder fails at decode() on every load of every tab: the crash
 * ladder still ends on JPEG and stays there, and a new tab of the profile, the
 * fault still there, takes JPEG at its first crash rather than walking the
 * ladder again.
 */
async function blockBroken(b: Block): Promise<void> {
    const { res: r, url } = b;
    const browser = await launchChromium();
    try {
        let storage: Awaited<ReturnType<BrowserContext['storageState']>> | undefined;
        for (const [tab, loads] of [['the first tab', 4], ['a new tab', 2]] as const) {
            // A fresh server, so what the new tab streams comes from its own storage.
            if (storage !== undefined) await b.restart();
            const [ctx, page] = await freshTab(browser, storage, `${url}/?offscreen_worker=false`, [shimJs('all')]);
            await waitFor(page, (s) => s.codec === 'jpeg' && s.navs >= loads, 90);
            await sleep(6000);
            const after = await readState(page);
            r.check(`${tab} settles on JPEG at load ${loads}`, after.navs === loads && after.codec === 'jpeg', { navs: after.navs, codec: after.codec });
            storage = await ctx.storageState();
            await ctx.close();
        }
    } finally {
        await browser.close();
    }
}

/**
 * Hardware decode accepts the stream and then produces nothing, never
 * erroring: the no-output watchdog trips the same software retry a decode
 * error would, software decodes, and the page does not reload. The failure
 * shim only reaches page-scope decoders, so the video worker is disabled and
 * the full-frame page decoder, whose silent stall nothing else would report,
 * is what is exercised.
 */
async function blockSilent({ res: r, url }: Block): Promise<void> {
    const { browser, page } = await openClient(url, { failMode: 'silent', encoder: 'h264enc', query: 'offscreen_worker=false' });
    try {
        const state = await waitFor(page, (s) => s.decoded > 0 && has(s.cfgs, 'prefer-software'), 40);
        r.check('hardware decode was tried first', state.cfgs[0] === 'default', state.cfgs.slice(0, 6));
        r.check('watchdog switched to software after the silent stall', has(state.cfgs, 'prefer-software'), state.cfgs.slice(0, 6));
        r.check('frames decode once software takes over', state.decoded > 0, state.decoded);
        await sleep(6000);
        const after = await readState(page);
        r.check('page never reloaded', after.navs === 1, after.navs);
        r.check('still decoding', after.decoded > state.decoded, [state.decoded, after.decoded]);
    } finally {
        await browser.close();
    }
}

const chunksOf = (page: Page): Promise<number> => within(page.evaluate(() => window.videoChunksReceived ?? 0));

/**
 * A background tab. Its video still flowing, a hidden page drops every decoded
 * frame, which must not read as a silent decoder: no software retry, no
 * reload, no ladder. A page whose socket opens hidden, as a reload or a
 * reconnect in a background tab does, pauses its video as a hide would have
 * and resumes it when shown, on its own encoder. The page's own full-frame
 * decoder is the one that drops frames while hidden, so the video worker is
 * disabled, as in silent.
 */
async function blockHidden({ res: r, host, url }: Block): Promise<void> {
    let client = await openClient(url, { encoder: 'h264enc', query: 'offscreen_worker=false' });
    try {
        const { page } = client;
        const state = await waitFor(page, (s) => s.decoded > 0, 30);
        r.check('hidden: the page decodes before its tab hides', state.decoded > 0, state);
        const chunks = await chunksOf(page);
        await visibility(page, true, false);
        await sleep(22_000);
        const hidden = await readState(page);
        r.check('hidden: video still reached the hidden page', (await chunksOf(page)) > chunks + 30);
        r.check('hidden: which neither retried in software nor reloaded', hidden.navs === 1 && !has(hidden.cfgs, 'prefer-software'), [hidden.navs, hidden.cfgs.slice(0, 6)]);
        await visibility(page, false, true);
        const shown = await waitFor(page, (s) => s.decoded > hidden.decoded + 30, 15);
        r.check('hidden: shown, it decodes on', shown.decoded > hidden.decoded + 30, [hidden.decoded, shown.decoded]);
    } finally {
        await client.browser.close();
    }

    const mark = await host.mark();
    client = await openClient(url, { encoder: 'h264enc', query: 'offscreen_worker=false', hidden: true });
    try {
        const { page } = client;
        const paused = "Received STOP_VIDEO for 'primary'";
        const sawPause = await host.saw(paused, 20, mark);
        r.check('hidden on connect: the page pauses its video', sawPause);
        await sleep(2000);
        const chunks = await chunksOf(page);
        await sleep(20_000);
        const hidden = await readState(page);
        // On the one load: a reload would start the count over.
        r.check('hidden on connect: no video reaches it while hidden', hidden.navs === 1 && (await chunksOf(page)) - chunks <= 2, hidden.navs);
        r.check('hidden on connect: no software retry, no reload', hidden.navs === 1 && !has(hidden.cfgs, 'prefer-software'), [hidden.navs, hidden.cfgs.slice(0, 6)]);
        await visibility(page, false, true);
        const shown = await waitFor(page, (s) => s.decoded > hidden.decoded + 30, 20);
        // JPEG never reaches a VideoDecoder, so decoded frames are the H.264 stream's.
        r.check('hidden on connect: shown, it streams H.264 again', shown.decoded > hidden.decoded + 30, [hidden.decoded, shown.decoded]);
    } finally {
        await client.browser.close();
    }
}

const BLOCKS: Record<string, [(b: Block) => Promise<void>, Record<string, string>?]> = {
    retry: [blockRetry], nosoftware: [blockNosoftware], nostart: [blockNostart],
    persisted: [blockPersisted], newtab: [blockNewtab],
    ladder: [blockLadder], healthy: [blockHealthy],
    silent: [blockSilent],
    nowebcodecs: [blockNowebcodecs, { SELKIES_WEB_ROOT: DASHBOARD_DIST }],
    cleared: [blockCleared], broken: [blockBroken], held: [blockHeld],
    hidden: [blockHidden],
};

for (const [name, [block, extra]] of Object.entries(BLOCKS)) {
    test(name, async () => {
        process.stdout.write(`=== ${name} ===\n`);
        const res = new Checks(name);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const env = { ...TURBO, ...(extra ?? {}) };
            const { url, port } = await host.start({ label: name, mode: 'websockets', env });
            let starts = 1;
            const restart = async (more: Record<string, string> = {}): Promise<void> => {
                await host.stop();
                starts++;
                await host.start({ label: `${name}-${starts}`, mode: 'websockets', port, env: { ...TURBO, ...more } });
            };
            await block({ res, host, url, port, restart });
        } catch (e) {
            res.check('block completed', false, String(e));
        } finally {
            await host.stop();
            await host.close();
        }
        res.finish();
    });
}
