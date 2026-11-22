/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The dashboard keeps streaming under a page policy of `worker-src 'none'`,
 * and a page whose policy allows workers keeps every worker path it has even
 * when its workers fail on their own.
 *
 * Chromium, Firefox, and WebKit reach the server through a loopback proxy
 * (`workerPolicyProxy`) rather than a Playwright route, since a page served
 * from a fulfilled route has no address space and its workers' WebSockets fail
 * the local network access checks. `control` passes every response as the
 * server sent it, `restricted` appends `worker-src 'none'` as one more policy,
 * so every policy is enforced at once, and `faulted` passes the responses as
 * `control` does but fires every worker's `error` event before the worker
 * reports, as a worker whose own script throws does. `restricted` reads the
 * session socket on the page after a `worker-src` violation, the others in
 * its worker with no violation and no fallback logged; `restricted` starts
 * audio on, which its page keeps off, having no decoder for it. Each holds, on the
 * first load and after a reload, the document's policies, a fresh socket the
 * server counts, the thread the socket is read on, a
 * software stream, the picture painted once the client sized the root, a key
 * and the pointer reaching X, and the clipboard both ways (rich markup + plain
 * text on Chromium where the engine reads on focus, plain paste and the
 * 192 KiB session push on Firefox and WebKit, with the local clipboard read
 * back on Chromium and Firefox and the write's own outcome plus preview
 * checked on headless WebKit), every payload named for its round so a copy
 * left over from a round before cannot pass.
 * @module
 */
import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { ENGINES, firefoxPersistentContext, launchBrowser, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { gestures, readRich, writeRich } from '../lib/clipboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host, type Held } from '../lib/host.ts';
import { sendClipboardFromClient } from '../lib/page.ts';
import { Picture } from '../lib/picture.ts';
import { within } from '../lib/stream.ts';
import { workerPolicyProxy, type WorkerPolicyProxy } from '../lib/worker-policy.ts';

const RESTRICTION = "worker-src 'none'";
/** What the client logs when audio packets reach a page with no decoder to take them. */
const AUDIO_UNDECODED = /AudioDecoderWorker not ready|Cannot initialize decoder audio/;
/** What the client logs when it moves a worker's job onto the page. */
const FALLBACK = /Content-Security-Policy (refuses|refused)|socket worker unavailable/;
/** The page policies the cases run under. */
const MODES = ['control', 'faulted', 'restricted'] as const;
type Mode = typeof MODES[number];
const CSP = 'content-security-policy';
const LARGE = 192 * 1024;
const VIOLATION = 'csp-violation ';
const CONNECTED = /Client \S+ connected \(/g;
/** The keys pressed in each round, by the keysym X reports them under. */
const KEYS: readonly (readonly [string, number])[] = [['x', 0x78], ['y', 0x79]];
const PERMISSIONS: Readonly<Record<Engine, string[]>> = {
    chromium: ['clipboard-read', 'clipboard-write'],
    firefox: ['clipboard-read'],
    webkit: [],
};

const CLIP_TAP_JS = `(() => {
  window.__clipMsgs = [];
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'clipboardContentUpdate') window.__clipMsgs.push(e.data);
  });
  window.__clipWrites = [];
  if (navigator.clipboard && navigator.clipboard.write) {
    const writeItems = navigator.clipboard.write.bind(navigator.clipboard);
    Object.defineProperty(navigator.clipboard, 'write', { configurable: true,
      value: (items) => writeItems(items).then(
        () => { window.__clipWrites.push('ok'); },
        (err) => { window.__clipWrites.push(err && err.name ? err.name : 'err'); throw err; }) });
  }
})()`;

/**
 * Fires every worker's `error` event before it reports, as a worker whose own
 * script throws does, with no policy violation, counting each in `__faults`.
 */
const FAULT_JS = `(() => {
  window.__faults = 0;
  const Native = window.Worker;
  window.Worker = class extends Native {
    constructor(...args) {
      super(...args);
      let reported = false;
      this.addEventListener('message', () => { reported = true; });
      setTimeout(() => {
        if (reported) return;
        window.__faults++;
        this.dispatchEvent(new ErrorEvent('error', { message: 'Uncaught Error: injected fault' }));
      }, 0);
    }
  };
})()`;

declare global {
    interface Window {
        __clipMsgs?: { text?: string }[];
        __clipWrites?: string[];
        __faults?: number;
    }
}

/** A response as the browser got it. */
interface Seen {
    url: string;
    kind: string;
    status: number;
    policy: string | null;
}

/**
 * The first answer `probe` gives that `ok` takes, polled for `ms`.
 * @returns That answer, or the last one.
 */
async function poll<T>(probe: () => Promise<T>, ok: (v: T) => boolean, ms = 10_000): Promise<T> {
    let got = await probe();
    for (const until = Date.now() + ms; !ok(got) && Date.now() < until;) {
        await sleep(400);
        got = await probe();
    }
    return got;
}

/** The browser's clipboard text, or the read's failure. */
function localText(page: Page): Promise<string> {
    return within(page.evaluate(async () => {
        try {
            return await navigator.clipboard.readText();
        } catch (err) {
            return `read failed: ${err instanceof Error ? err.name : String(err)}`;
        }
    }));
}

/** The shown sink's box on the page and its picture's size, or null with none shown. */
function sinkBox(page: Page): Promise<{ left: number; top: number; w: number; h: number; iw: number; ih: number } | null> {
    return within(page.evaluate(() => {
        const v = document.querySelector('video');
        const sink = v && v.videoWidth > 0 && v.style.display !== 'none'
            ? { el: v, w: v.videoWidth, h: v.videoHeight }
            : [...document.querySelectorAll('canvas')].filter((c) => c.width >= 640 && c.style.display !== 'none').map((c) => ({ el: c, w: c.width, h: c.height }))[0];
        if (!sink) return null;
        const r = sink.el.getBoundingClientRect();
        return { left: r.left, top: r.top, w: r.width, h: r.height, iw: sink.w, ih: sink.h };
    }));
}

/** Where on the X root a page point lands through the shown sink. */
async function mapped(page: Page, x: number, y: number): Promise<[number, number] | null> {
    const box = await sinkBox(page);
    if (!box || !box.w || !box.h) return null;
    return [(x - box.left) * box.iw / box.w, (y - box.top) * box.ih / box.h];
}

/** The X root once the client sized it: unchanged between two reads and within a CVT cell of the shown picture. */
async function settledRoot(host: Host, page: Page, ms: number): Promise<{ root: [number, number] | null; sink: [number, number] | null; settled: boolean }> {
    let last: [number, number] | null = null;
    const read = async (): Promise<{ root: [number, number] | null; sink: [number, number] | null; settled: boolean }> => {
        const [root, box] = [await host.rootSize(), await sinkBox(page)];
        const sink: [number, number] | null = box ? [box.iw, box.ih] : null;
        const still = root !== null && last !== null && root[0] === last[0] && root[1] === last[1];
        last = root;
        const fits = root !== null && sink !== null && Math.abs(root[0] - sink[0]) <= 16 && Math.abs(root[1] - sink[1]) <= 16;
        return { root, sink, settled: still && fits };
    };
    return poll(read, (r) => r.settled, ms);
}

/** What `stream_info` says of the encoder's hardware, or undefined before it arrives. */
function hardware(page: Page): Promise<unknown> {
    return within(page.evaluate(() => {
        const info: unknown = Reflect.get(window, 'stream_info');
        return typeof info === 'object' && info !== null && 'hardware' in info ? info.hardware : undefined;
    }));
}

/** Press `key` and leave it down; headless WebKit can drop a synthetic keydown on a throttled render round, so retry up to 4 times. */
async function holdKey(host: Host, page: Page, key: string, keysym: number, pace: number): Promise<boolean> {
    const tries = pace >= 1 ? 4 : 1;
    for (let i = 0; i < tries; i++) {
        await page.keyboard.down(key);
        const down = await poll(() => host.keyHeld(keysym), (h) => h === true, 2000 * pace);
        if (down === true) return true;
        await page.keyboard.up(key);
        await sleep(400 * pace);
    }
    return false;
}

/** What a round drives and where it records: the clipboard owners it takes stay held until the caller releases them. */
interface Session {
    engine: Engine;
    host: Host;
    page: Page;
    picture: Picture;
    res: Checks;
    owners: Held[];
}

/** One load's round: its number, the payloads' nonce, and how much of its waits to spend. */
interface Round {
    n: number;
    nonce: string;
    pace: number;
}

/** One round on the loaded page: picture, key, pointer, and the clipboard both ways, its payloads named by `nonce`. */
async function round({ engine, host, page, picture, res, owners }: Session, { n, nonce, pace }: Round): Promise<void> {
    const tag = `round ${n}`;
    const sample = await picture.wait(page, 30 * pace);
    res.check(`${tag}: the page decodes the painted picture`, picture.matches(sample), sample);

    const [key, keysym] = KEYS[n - 1];
    await page.mouse.click(700, 450);
    await sleep(500 * pace);
    const down = await holdKey(host, page, key, keysym, pace);
    await page.keyboard.up(key);
    const up = await poll(() => host.keyHeld(keysym), (h) => h === false, 5000 * pace);
    res.check(`${tag}: '${key}' is held in X while pressed and released after`, down === true && up === false, { down, up });

    for (const [x, y] of [[220, 170], [520, 380]] as const) {
        const want = await mapped(page, x, y);
        await page.mouse.move(x, y);
        const near = (p: [number, number] | null): boolean => want !== null && p !== null && Math.abs(p[0] - want[0]) <= 4 && Math.abs(p[1] - want[1]) <= 4;
        const at = await poll(() => host.pointer(), near, 5000 * pace);
        res.check(`${tag}: the pointer moved to (${x},${y}) lands where the stream shows it`, near(at), { at, want });
    }

    const plain = `rich ${nonce}`;
    if (engine === 'chromium') {
        const html = `<b>${plain}</b>`;
        await writeRich(page, html, plain);
        const gotHtml = await poll(async () => (await host.x11Convert('text/html'))?.toString() ?? null, (v) => v === html, 10_000 * pace);
        const gotPlain = (await host.x11Convert('UTF8_STRING'))?.toString() ?? null;
        res.check(`${tag}: a rich browser copy reaches the session as its exact markup`, gotHtml === html, gotHtml);
        res.check(`${tag}: and as its exact plain text`, gotPlain === plain, gotPlain);
    } else {
        await sendClipboardFromClient(page, plain, engine);
        const gotPlain = await poll(async () => (await host.x11Convert('UTF8_STRING'))?.toString() ?? null, (v) => v === plain, 10_000 * pace);
        res.check(`${tag}: a browser copy reaches the session as its exact plain text`, gotPlain === plain, gotPlain);
    }

    if (engine !== 'webkit') {
        const before = await localText(page);
        res.check(`${tag}: the browser clipboard holds this round's copy before the session's`, before === plain, before.slice(0, 80));
    }
    const big = `big-${nonce}-${'z'.repeat(LARGE)}`;
    const writesBefore = await within(page.evaluate(() => (window.__clipWrites ?? []).length));
    owners.push(await host.ownClipboard(big));
    if (engine === 'webkit') {
        const previewed = await poll(async () => {
            await gestures(page, [700, 450], 1, 1500 * pace);
            return within(page.evaluate((want: string) => {
                const msgs = window.__clipMsgs ?? [];
                const writes = window.__clipWrites ?? [];
                return { matched: msgs.some((m) => m.text === want), writes: writes.slice() };
            }, big));
        }, (v) => v.matched && v.writes.slice(writesBefore).includes('ok'), 20_000 * pace);
        res.check(`${tag}: a ${big.length}-byte session copy is decoded and written in the browser`,
            previewed.matched && previewed.writes.slice(writesBefore).includes('ok'), previewed);
    } else {
        const local = await poll(async () => {
            await gestures(page, [700, 450], 1, 1500 * pace);
            return localText(page);
        }, (v) => v === big, 20_000 * pace);
        res.check(`${tag}: a ${big.length}-byte session copy lands whole in the browser clipboard`, local === big, { length: local.length, head: local.slice(0, 40) });
        const rich = await readRich(page);
        res.check(`${tag}: no stale markup rides with the session's plain copy`, rich === null, rich);
    }
}

/** One load's console and what the case expects of it. */
interface Load {
    n: number;
    mode: Mode;
    lines: string[];
}

/**
 * Checks which thread the session socket is read on and what the load logged:
 * the page after a `worker-src` violation under `restricted`, its worker with
 * neither a violation nor a fallback otherwise, no audio sent to a page with
 * no decoder for it, and `faulted` having fired at least one worker's `error`
 * event so the case is not vacuous.
 */
async function sessionPath(page: Page, res: Checks, { n, mode, lines }: Load): Promise<void> {
    const restrict = mode === 'restricted';
    const onPage = await within(page.evaluate(() => Reflect.get(window, 'selkiesTransport') instanceof WebSocket));
    res.check(`load ${n}: the session socket is read ${restrict ? 'on the page' : 'in its worker'}`, onPage === restrict, { onPage });
    const violations = lines.filter((l) => l.includes(VIOLATION)).length;
    const fallbacks = lines.filter((l) => FALLBACK.test(l));
    res.check(`load ${n}: ${restrict ? 'a worker-src violation is reported and the client logs its fallback' : 'no violation is reported and no fallback logged'}`,
        restrict ? violations >= 1 && fallbacks.length >= 1 : violations === 0 && fallbacks.length === 0, { violations, fallbacks: fallbacks.slice(0, 3) });
    const undecoded = lines.filter((l) => AUDIO_UNDECODED.test(l)).length;
    res.check(`load ${n}: no audio packet reaches the page with no decoder to take it`, undecoded === 0, { undecoded });
    if (mode === 'faulted') {
        const faults = await within(page.evaluate(() => window.__faults ?? 0));
        res.check(`load ${n}: the workers' error events fired before they reported`, faults >= 1, { faults });
    }
}

/** Opens a browser context and page for `engine`, using the persistent profile for Firefox, its workers faulted where `fault` says. */
async function openEngine(engine: Engine, url: string, fault: boolean): Promise<[() => Promise<void>, Page]> {
    const viewport = { width: 1440, height: 900 };
    let browser: Browser | null = null;
    let ctx: BrowserContext;
    if (engine === 'firefox') {
        ctx = await firefoxPersistentContext({ viewport });
    } else {
        browser = await launchBrowser(engine);
        ctx = await browser.newContext({ viewport });
    }
    const perms = PERMISSIONS[engine];
    if (perms.length) await ctx.grantPermissions(perms, { origin: url }).catch(() => undefined);
    await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets'; try { localStorage.clear(); sessionStorage.clear(); } catch (e) {}" });
    await ctx.addInitScript({ content: CLIP_TAP_JS });
    if (fault) await ctx.addInitScript({ content: FAULT_JS });
    await ctx.addInitScript((prefix: string) => {
        document.addEventListener('securitypolicyviolation', (e) => {
            console.warn(prefix + JSON.stringify({ directive: e.effectiveDirective, blocked: e.blockedURI, disposition: e.disposition, source: e.sourceFile }));
        });
    }, VIOLATION);
    const pages = ctx.pages();
    const page = engine === 'firefox' && pages.length ? pages[0] : await ctx.newPage();
    const close = (): Promise<void> => within((browser ?? ctx).close(), 'the browser');
    return [close, page];
}

for (const modeName of MODES) {
    const restrict = modeName === 'restricted';
    for (const engine of ENGINES) {
        const name = engine === 'chromium' ? modeName : `${modeName}-${engine}`;
        test(name, async () => {
            const info = test.info();
            const res = new Checks(`worker-src-${name}`);
            const host = new Host(suiteOf(import.meta.filename));
            const seen: Seen[] = [];
            const reads: Promise<void>[] = [];
            const sockets: string[] = [];
            const consoleLines: string[] = [];
            const pageErrors: string[] = [];
            let proxy: WorkerPolicyProxy | null = null;
            try {
                const upstream = await host.start({
                    label: name, mode: 'websockets',
                    // Audio starts on under the restricting policy, whose page has no decoder for it.
                    env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_USE_CPU: 'true', SELKIES_AUDIO_ON_START: restrict ? 'true' : 'false' },
                });
                proxy = await workerPolicyProxy(upstream.url, restrict);
                const { url, responses } = proxy;
                const picture = new Picture(host, false);
                const [closeBrowser, page] = await openEngine(engine, url, modeName === 'faulted');
                const owners: Held[] = [];
                try {
                    page.on('console', (m) => { consoleLines.push(`${m.type()}: ${m.text()}`); });
                    page.on('pageerror', (e) => { pageErrors.push(`${e.name}: ${e.message}`); });
                    page.on('websocket', (ws) => { sockets.push(ws.url()); });
                    page.on('response', (r) => {
                        const entry: Seen = { url: r.url(), kind: r.request().resourceType(), status: r.status(), policy: null };
                        seen.push(entry);
                        reads.push(r.headerValue(CSP).then((v) => { entry.policy = v; }, (e: unknown) => { entry.policy = `unread: ${String(e)}`; }));
                    });
                    for (const n of [1, 2]) {
                        const [at, sent, mark, logged] = [seen.length, responses.length, await host.mark(), consoleLines.length];
                        if (n === 1) await page.goto(`${url}/`, { waitUntil: 'load', timeout: 30_000 });
                        else await page.reload({ waitUntil: 'load', timeout: 30_000 });
                        await Promise.all(reads);
                        const doc = seen.slice(at).find((d) => d.kind === 'document' && d.url.startsWith(url));
                        const path = doc ? new URL(doc.url).pathname + new URL(doc.url).search : null;
                        const upstreamPolicy = responses.slice(sent).find((p) => p.path === path)?.sent ?? [];
                        const want = [...(restrict ? [...upstreamPolicy, RESTRICTION] : upstreamPolicy)].sort();
                        const got = doc?.policy ? doc.policy.split(',').map((s) => s.trim()).filter(Boolean).sort() : [];
                        res.check(`load ${n}: the document carries ${restrict ? "the server's policies and worker-src 'none'" : "the server's policies alone"}`,
                            doc !== undefined && got.length === want.length && got.every((v, i) => v === want[i]), { got: doc?.policy, want });
                        const connects = await poll(async () => (await host.log(mark)).match(CONNECTED)?.length ?? 0, (c) => c >= 1, 15_000);
                        res.check(`load ${n}: the server takes a new client socket through the proxy`, connects >= 1, { connects, sockets });
                        // A load with no socket can receive nothing; its waits then stay short, so a failure reports
                        // itself instead of spending the test's whole budget before the checks are made.
                        const pace = connects >= 1 ? 1 : 0.1;
                        const hw = await poll(() => hardware(page), (v) => v !== undefined, 15_000 * pace);
                        res.check(`load ${n}: the stream is encoded in software`, hw === false, hw);
                        const size = await settledRoot(host, page, 20_000 * pace);
                        res.check(`load ${n}: the client sized the X root to its picture`, size.settled, size);
                        await picture.clear();
                        await picture.paint();
                        await round({ engine, host, page, picture, res, owners }, { n, nonce: `${name}-round${n}`, pace });
                        await sessionPath(page, res, { n, mode: modeName, lines: consoleLines.slice(logged) });
                        for (let owner = owners.pop(); owner; owner = owners.pop()) await owner.release();
                    }
                    res.check('no uncaught page errors', pageErrors.length === 0, pageErrors.slice(0, 3));
                } finally {
                    for (let owner = owners.pop(); owner; owner = owners.pop()) await owner.release();
                    await Promise.allSettled(reads);
                    await info.attach('responses.json', { body: JSON.stringify(responses, null, 2), contentType: 'application/json' });
                    await info.attach('browser-responses.json', { body: JSON.stringify(seen, null, 2), contentType: 'application/json' });
                    await info.attach('csp-violations.txt', { body: consoleLines.filter((l) => l.includes(VIOLATION)).join('\n'), contentType: 'text/plain' });
                    await info.attach('console.txt', { body: consoleLines.join('\n'), contentType: 'text/plain' });
                    await info.attach('pageerrors.txt', { body: pageErrors.join('\n'), contentType: 'text/plain' });
                    try {
                        await closeBrowser();
                    } finally {
                        await picture.clear();
                    }
                }
            } finally {
                try {
                    await host.close();
                } finally {
                    await proxy?.close();
                }
            }
            res.finish();
        });
    }
}
