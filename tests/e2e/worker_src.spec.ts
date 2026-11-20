/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The dashboard keeps streaming under a page policy of `worker-src 'none'`.
 *
 * Chromium reaches the server through a loopback proxy (`workerPolicyProxy`)
 * rather than a Playwright route, since a page served from a fulfilled route
 * has no address space and its workers' WebSockets fail the local network
 * access checks. `control` passes every response as the server sent it,
 * `restricted` appends `worker-src 'none'` as one more policy, so every policy
 * is enforced at once. Each holds, on the first load and after a reload, the
 * document's policies, a fresh socket the server counts, a software stream,
 * the picture painted once the client sized the root, a key and the pointer
 * reaching X, a rich copy from the browser to the session, and a large copy
 * from the session to the browser's own clipboard, every payload named for
 * its round so a copy left over from a round before cannot pass.
 * @module
 */
import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchBrowser } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { gestures, readRich, writeRich } from '../lib/clipboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host, type Held } from '../lib/host.ts';
import { Picture } from '../lib/picture.ts';
import { within } from '../lib/stream.ts';
import { workerPolicyProxy, type WorkerPolicyProxy } from '../lib/worker-policy.ts';

const RESTRICTION = "worker-src 'none'";
const CSP = 'content-security-policy';
const LARGE = 192 * 1024;
const VIOLATION = 'csp-violation ';
const CONNECTED = /Client \S+ connected \(/g;
/** The keys pressed in each round, by the keysym X reports them under. */
const KEYS: readonly (readonly [string, number])[] = [['x', 0x78], ['y', 0x79]];

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

/** What a round drives and where it records: the clipboard owners it takes stay held until the caller releases them. */
interface Session {
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
async function round({ host, page, picture, res, owners }: Session, { n, nonce, pace }: Round): Promise<void> {
    const tag = `round ${n}`;
    const sample = await picture.wait(page, 30 * pace);
    res.check(`${tag}: the page decodes the painted picture`, picture.matches(sample), sample);

    const [key, keysym] = KEYS[n - 1];
    await page.mouse.click(700, 450);
    await sleep(500);
    await page.keyboard.down(key);
    const down = await poll(() => host.keyHeld(keysym), (h) => h === true, 5000);
    await page.keyboard.up(key);
    const up = await poll(() => host.keyHeld(keysym), (h) => h === false, 5000);
    res.check(`${tag}: '${key}' is held in X while pressed and released after`, down === true && up === false, { down, up });

    for (const [x, y] of [[220, 170], [520, 380]] as const) {
        const want = await mapped(page, x, y);
        await page.mouse.move(x, y);
        const near = (p: [number, number] | null): boolean => want !== null && p !== null && Math.abs(p[0] - want[0]) <= 4 && Math.abs(p[1] - want[1]) <= 4;
        const at = await poll(() => host.pointer(), near, 5000);
        res.check(`${tag}: the pointer moved to (${x},${y}) lands where the stream shows it`, near(at), { at, want });
    }

    const html = `<b>rich ${nonce}</b>`;
    const plain = `rich ${nonce}`;
    await writeRich(page, html, plain);
    const gotHtml = await poll(async () => (await host.x11Convert('text/html'))?.toString() ?? null, (v) => v === html);
    const gotPlain = (await host.x11Convert('UTF8_STRING'))?.toString() ?? null;
    res.check(`${tag}: a rich browser copy reaches the session as its exact markup`, gotHtml === html, gotHtml);
    res.check(`${tag}: and as its exact plain text`, gotPlain === plain, gotPlain);

    const before = await localText(page);
    res.check(`${tag}: the browser clipboard holds this round's copy before the session's`, before === plain, before.slice(0, 80));
    const big = `big-${nonce}-${'z'.repeat(LARGE)}`;
    owners.push(await host.ownClipboard(big));
    const local = await poll(async () => {
        await gestures(page, [700, 450], 1, 1500);
        return localText(page);
    }, (v) => v === big, 20_000);
    res.check(`${tag}: a ${big.length}-byte session copy lands whole in the browser clipboard`, local === big, { length: local.length, head: local.slice(0, 40) });
    const rich = await readRich(page);
    res.check(`${tag}: no stale markup rides with the session's plain copy`, rich === null, rich);
}

for (const restrict of [false, true]) {
    const name = restrict ? 'restricted' : 'control';
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
                env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_USE_CPU: 'true', SELKIES_AUDIO_ON_START: 'false' },
            });
            proxy = await workerPolicyProxy(upstream.url, restrict);
            const { url, responses } = proxy;
            const picture = new Picture(host, false);
            const browser = await launchBrowser('chromium');
            const owners: Held[] = [];
            try {
                const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
                await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: url });
                await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
                await ctx.addInitScript((prefix: string) => {
                    document.addEventListener('securitypolicyviolation', (e) => {
                        console.warn(prefix + JSON.stringify({ directive: e.effectiveDirective, blocked: e.blockedURI, disposition: e.disposition, source: e.sourceFile }));
                    });
                }, VIOLATION);
                const page = await ctx.newPage();
                page.on('console', (m) => { consoleLines.push(`${m.type()}: ${m.text()}`); });
                page.on('pageerror', (e) => { pageErrors.push(`${e.name}: ${e.message}`); });
                page.on('websocket', (ws) => { sockets.push(ws.url()); });
                page.on('response', (r) => {
                    const entry: Seen = { url: r.url(), kind: r.request().resourceType(), status: r.status(), policy: null };
                    seen.push(entry);
                    reads.push(r.headerValue(CSP).then((v) => { entry.policy = v; }, (e: unknown) => { entry.policy = `unread: ${String(e)}`; }));
                });
                for (const n of [1, 2]) {
                    const [at, sent, mark] = [seen.length, responses.length, await host.mark()];
                    if (n === 1) await page.goto(`${url}/`, { waitUntil: 'load', timeout: 30_000 });
                    else await page.reload({ waitUntil: 'load', timeout: 30_000 });
                    await Promise.all(reads);
                    const doc = seen.slice(at).find((d) => d.kind === 'document' && d.url.startsWith(url));
                    const path = doc ? new URL(doc.url).pathname + new URL(doc.url).search : null;
                    const upstreamPolicy = responses.slice(sent).find((p) => p.path === path)?.sent ?? [];
                    const want = (restrict ? [...upstreamPolicy, RESTRICTION] : upstreamPolicy).join(', ') || null;
                    res.check(`load ${n}: the document carries ${restrict ? "the server's policies and worker-src 'none'" : "the server's policies alone"}`,
                        doc !== undefined && doc.policy === want, { got: doc?.policy, want });
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
                    await round({ host, page, picture, res, owners }, { n, nonce: `${name}-round${n}`, pace });
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
                    await within(browser.close(), 'the browser');
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
