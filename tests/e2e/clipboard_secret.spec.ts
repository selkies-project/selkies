/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A password copied in the session stays out of sight on the page, and off
// the local clipboard once the session is done with it.
//
// KeePassXC and KDE offer `x-kde-passwordManagerHint` = `secret` beside a
// password they copy, and a toolkit that validates mime types sees it only
// under a `text/` or `application/` prefix. The page shows such a copy masked
// in both dashboards' clipboard panes, with a button that copies it, and
// keeps nothing of it in storage or in the document. With seamless sync it
// writes it to the local clipboard and takes it back 60 seconds later, or as
// soon as the session's clipboard lets it go, but only while the local
// clipboard still holds it. That check runs where the engine allows one
// without a gesture or a prompt, Chromium with clipboard-read granted;
// Firefox and WebKit read nothing for it, which is checked as well. The
// session's copy comes from an X client offering the hint, or on Wayland from
// a data-control source on the capture compositor.
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createInterface } from 'node:readline';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { firefoxPersistentContext, launchBrowser, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { openClipboardPanel } from '../lib/dashboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host, runtimeDir, suitesBin, type Held } from '../lib/host.ts';
import { within } from '../lib/stream.ts';

const LAYOUTS = ['default', 'wish', 'edge'] as const;
const HINTS = ['x-kde-passwordManagerHint', 'text/x-kde-passwordManagerHint', 'application/x-kde-passwordManagerHint'];
const MASK = '•'.repeat(8);
const WL_SOCKET = 'wayland-1';
// The core's term for a secret on the local clipboard, plus the slack a
// delivery and the check take.
const TERM_S = 60;

const TAP_JS = `(() => {
  window.__clipMsgs = [];
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'clipboardContentUpdate') window.__clipMsgs.push(e.data);
  });
  window.__clipReads = 0;
  window.__clipWrites = [];
  if (navigator.clipboard && navigator.clipboard.readText) {
    const read = navigator.clipboard.readText.bind(navigator.clipboard);
    Object.defineProperty(navigator.clipboard, 'readText', { configurable: true,
      value: (...args) => { window.__clipReads++; return read(...args); } });
    const write = navigator.clipboard.writeText.bind(navigator.clipboard);
    Object.defineProperty(navigator.clipboard, 'writeText', { configurable: true,
      value: (text) => write(text).then(
        () => { window.__clipWrites.push([text, 'ok']); },
        (err) => { window.__clipWrites.push([text, err.name]); throw err; }) });
    const writeItems = navigator.clipboard.write.bind(navigator.clipboard);
    Object.defineProperty(navigator.clipboard, 'write', { configurable: true,
      value: (items) => writeItems(items).then(
        () => { window.__clipWrites.push(['<items>', 'ok']); },
        (err) => { window.__clipWrites.push(['<items>', err.name]); throw err; }) });
  }
})()`;

// Everything the page could have kept: web storage, every IndexedDB record,
// every cached response, and the document with its fields' values.
const KEPT_JS = `(async () => {
  const out = { local: {}, session: {}, idb: {}, caches: {} };
  for (let i = 0; i < localStorage.length; i++) out.local[localStorage.key(i)] = localStorage.getItem(localStorage.key(i));
  for (let i = 0; i < sessionStorage.length; i++) out.session[sessionStorage.key(i)] = sessionStorage.getItem(sessionStorage.key(i));
  if (indexedDB.databases) {
    for (const { name } of await indexedDB.databases()) {
      const db = await new Promise((res, rej) => { const r = indexedDB.open(name); r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error); });
      out.idb[name] = {};
      for (const store of db.objectStoreNames) {
        out.idb[name][store] = await new Promise((res) => {
          const r = db.transaction(store).objectStore(store).getAll();
          r.onsuccess = () => res(r.result); r.onerror = () => res(null);
        });
      }
      db.close();
    }
  }
  if (self.caches) {
    for (const key of await caches.keys()) {
      const cache = await caches.open(key);
      out.caches[key] = [];
      for (const req of await cache.keys()) {
        const resp = await cache.match(req);
        out.caches[key].push([req.url, resp ? await resp.text() : '']);
      }
    }
  }
  out.document = document.documentElement.outerHTML + '\\n'
    + [...document.querySelectorAll('textarea, input')].map((e) => e.value).join('\\n');
  out.messages = JSON.stringify(window.__clipMsgs || []);
  return JSON.stringify(out);
})()`;

const PERMISSIONS: Readonly<Record<Engine, string[]>> = { chromium: ['clipboard-read', 'clipboard-write'], firefox: ['clipboard-read'], webkit: [] };

const js = (page: Page, code: string): Promise<unknown> => within(page.evaluate(code));
const r = (v: unknown): string => JSON.stringify(v);

async function waitUntil<T>(fn: () => Promise<T>, timeoutS: number, stepS = 0.25): Promise<T> {
    const until = Date.now() + timeoutS * 1000;
    let value = await fn();
    while (!value && Date.now() < until) {
        await sleep(stepS * 1000);
        value = await fn();
    }
    return value;
}

/**
 * The session's clipboard owner: an X client, or a data-control source on the
 * capture compositor (crates/selkies-suites' wl-clipboard-owner), offering the
 * text and the hint beside it.
 */
class Owner {
    private held: Held | null = null;
    private app: ChildProcessWithoutNullStreams | null = null;

    static async take(host: Host, wayland: boolean, runtime: string, text: string, hint = HINTS[0], secret = true): Promise<Owner> {
        const owner = new Owner();
        const entries: Record<string, string> = secret ? { [hint]: 'secret' } : {};
        if (wayland) {
            const offers: [string, string][] = [['text/plain;charset=utf-8', text], ['text/plain', text], ...Object.entries(entries)];
            const app = spawn(suitesBin('wl-clipboard-owner'), [WL_SOCKET, ...offers.flatMap(([mime, value]) => ['--offer', mime, value])],
                { env: { ...process.env, XDG_RUNTIME_DIR: runtime }, stdio: ['pipe', 'pipe', 'pipe'] });
            owner.app = app;
            const said = await new Promise<string>((resolve) => {
                const lines = createInterface({ input: app.stdout });
                lines.once('line', (l) => { resolve(l.trim()); });
                app.once('exit', () => { resolve(''); });
            });
            if (said !== 'owned') throw new Error(`wl-clipboard-owner did not take the selection: ${r(said)}`);
        } else {
            owner.held = await host.ownClipboard(text, entries);
        }
        return owner;
    }

    /** Let the selection go, as a password manager clearing its copy does. */
    async release(): Promise<void> {
        const app = this.app;
        if (app !== null) {
            const gone = new Promise<void>((resolve) => {
                if (app.exitCode !== null || app.signalCode !== null) resolve();
                else app.once('exit', () => { resolve(); });
            });
            app.stdin.end();
            const timer = setTimeout(() => app.kill('SIGKILL'), 10_000);
            await gone;
            clearTimeout(timer);
        }
        await this.held?.release();
    }
}

const localText = async (page: Page): Promise<string> => String(await js(page, "navigator.clipboard.readText().catch((e) => 'ERR:' + e.name)"));

/** `text` as a check prints it, with the secret masked wherever it appears. */
const masked = (text: string, secret: string): string => text.split(secret).join('<the secret>');

/** Where the page keeps `secret` or its base64: nowhere, or the places. */
async function leaks(page: Page, secret: string): Promise<string[]> {
    const forms = [secret, Buffer.from(secret).toString('base64')];
    const parsed: unknown = JSON.parse(String(await js(page, KEPT_JS)));
    const kept = parsed !== null && typeof parsed === 'object' ? Object.entries(parsed) : [];
    return kept.filter(([, value]) => forms.some((form) => JSON.stringify(value).includes(form))).map(([where]) => where);
}

async function openPage(engine: Engine, url: string, mode: string): Promise<[() => Promise<void>, Page, boolean, Browser | null]> {
    let ctx: BrowserContext;
    let browser: Browser | null = null;
    if (engine === 'firefox') {
        ctx = await firefoxPersistentContext({ viewport: { width: 1440, height: 900 } });
    } else {
        browser = await launchBrowser(engine);
        ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    }
    const perms = PERMISSIONS[engine];
    if (perms.length) await ctx.grantPermissions(perms, { origin: url }).catch(() => undefined);
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: TAP_JS });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    // The clipboard needs the session's settings, which carry its clipboard policy.
    const connected = await waitUntil(async () => (await js(page, 'window.clipboard_enabled === true')) === true, 60, 0.5);
    const b = browser;
    return [() => (b ?? ctx).close(), page, connected, browser];
}

type Preview = Record<string, unknown>;

async function lastPreview(page: Page): Promise<Preview> {
    const got = await js(page, '(window.__clipMsgs || []).slice(-1)[0] || null');
    return got !== null && typeof got === 'object' ? { ...got } : {};
}

/** What the dashboard's clipboard box shows. */
async function pane(page: Page, dashboard: string): Promise<string> {
    if (!(await openClipboardPanel(page, dashboard))) return '<no panel>';
    const area = page.locator('#dashboardClipboardTextarea').first();
    try {
        await area.waitFor({ state: 'visible', timeout: 4000 });
        return await area.inputValue();
    } catch (e) {
        return `<no textarea: ${String(e)}>`;
    }
}

async function closePanel(page: Page, dashboard: string): Promise<void> {
    if (dashboard !== 'default') {
        await page.keyboard.press('Escape');
        await sleep(300);
    }
}

const writes = async (page: Page): Promise<unknown[]> => {
    const got = await js(page, 'window.__clipWrites');
    return Array.isArray(got) ? got.map((w): unknown => w) : [];
};

interface Session {
    host: Host;
    wayland: boolean;
    runtime: string;
}

/** The copy reaches the page masked, is copied by the button, and is kept nowhere. */
async function maskedChecks(res: Checks, tag: string, page: Page, s: Session, dashboard: string, engine: Engine, n: number): Promise<void> {
    const secret = `pw-${tag}-${n}-Zq8!`;
    const hint = HINTS[n % HINTS.length];
    let owner = await Owner.take(s.host, s.wayland, s.runtime, secret, hint);
    try {
        const got = await waitUntil(async () => (await lastPreview(page)).secret === true, 10);
        const preview = await lastPreview(page);
        res.check(`${tag} a copy marked ${hint} reaches the page as the flag alone`, got && preview.text === '', masked(r(preview), secret));
        const shown = await pane(page, dashboard);
        res.check(`${tag} the clipboard pane shows it masked`, shown === MASK, masked(shown, secret).slice(0, 40));
        const button = page.locator('button:has-text("Copy hidden text")').first();
        await js(page, "window.postMessage({type: 'settings', settings: {clipboard_seamless: false}}, window.location.origin)");
        if (engine === 'chromium') await js(page, "navigator.clipboard.writeText('before the button')");
        let clicked = false;
        if (await button.count()) {
            await button.click();
            clicked = true;
        }
        // Headless WebKit reads nothing back, so there the write's own outcome decides.
        let copied = await waitUntil(async () => (await writes(page)).some((w) => r(w) === r([secret, 'ok'])), 5);
        if (engine !== 'webkit') copied = copied && (await waitUntil(async () => (await localText(page)) === secret, 5));
        res.check(`${tag} the pane's button copies it to this device`, clicked && copied, `clicked=${clicked ? 'True' : 'False'} writes=${(await writes(page)).length}`);
        await js(page, "window.postMessage({type: 'settings', settings: {clipboard_seamless: true}}, window.location.origin)");
        await closePanel(page, dashboard);
        const places = await leaks(page, secret);
        res.check(`${tag} the page keeps it in no storage, field, or message`, places.length === 0, places);
    } finally {
        await owner.release();
    }
    await sleep(1500);
    const ordinary = `ordinary-${tag}-${n}`;
    owner = await Owner.take(s.host, s.wayland, s.runtime, ordinary, HINTS[0], false);
    try {
        const got = await waitUntil(async () => (await lastPreview(page)).text === ordinary, 10);
        const shown = await pane(page, dashboard);
        res.check(`${tag} ordinary text is shown as it is`, got && shown === ordinary && (await lastPreview(page)).secret === false, masked(shown, secret).slice(0, 40));
        await closePanel(page, dashboard);
    } finally {
        await owner.release();
    }
    await sleep(1000);
}

/**
 * Chromium takes a secret back when the session lets it go, and when its term
 * is up, only while the local clipboard still holds it.
 */
async function takeBackChecks(res: Checks, tag: string, page: Page, s: Session): Promise<void> {
    let secret = `pw-${tag}-release-Zq8!`;
    let owner = await Owner.take(s.host, s.wayland, s.runtime, secret);
    const landed = await waitUntil(async () => (await localText(page)) === secret, 10);
    res.check(`${tag} with seamless sync the secret lands on the local clipboard`, landed, masked(await localText(page), secret));
    const started = Date.now();
    await owner.release();
    const taken = await waitUntil(async () => (await localText(page)) === '', 8);
    res.check(`${tag} the session letting it go takes it off the local clipboard`, taken, `${r(masked(await localText(page), secret))} after ${((Date.now() - started) / 1000).toFixed(1)} s`);

    secret = `pw-${tag}-kept-Zq8!`;
    owner = await Owner.take(s.host, s.wayland, s.runtime, secret);
    await waitUntil(async () => (await localText(page)) === secret, 10);
    await js(page, "navigator.clipboard.writeText('a copy the user made')");
    await owner.release();
    await sleep(4000);
    res.check(`${tag} a copy the user made since is left alone when the session lets go`, (await localText(page)) === 'a copy the user made', masked(await localText(page), secret));

    secret = `pw-${tag}-term-Zq8!`;
    owner = await Owner.take(s.host, s.wayland, s.runtime, secret);
    try {
        await waitUntil(async () => (await localText(page)) === secret, 10);
        const landedAt = Date.now();
        await sleep((TERM_S - 5) * 1000);
        const early = await localText(page);
        const gone = await waitUntil(async () => (await localText(page)) === '', 12, 0.5);
        res.check(`${tag} a secret still held is taken back when its term is up, not before`, early === secret && gone,
            `at ${TERM_S - 5} s ${r(masked(early, secret).slice(0, 20))}, emptied after ${((Date.now() - landedAt) / 1000).toFixed(1)} s`);
    } finally {
        await owner.release();
    }
    await sleep(1500);

    secret = `pw-${tag}-term-kept-Zq8!`;
    owner = await Owner.take(s.host, s.wayland, s.runtime, secret);
    try {
        await waitUntil(async () => (await localText(page)) === secret, 10);
        await js(page, "navigator.clipboard.writeText('another copy the user made')");
        await sleep((TERM_S + 5) * 1000);
        res.check(`${tag} a copy the user made since is left alone when the term is up`, (await localText(page)) === 'another copy the user made', masked(await localText(page), secret));
    } finally {
        await owner.release();
    }
    await sleep(1000);
}

/** A shared viewer's page, beside the controller's, takes nothing of the session's clipboard. */
async function viewerChecks(res: Checks, tag: string, browser: Browser, url: string, mode: string, s: Session): Promise<void> {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
    try {
        await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: url });
        await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        await ctx.addInitScript({ content: TAP_JS });
        const viewer = await ctx.newPage();
        await viewer.goto(`${url}/#shared`, { waitUntil: 'load' });
        await sleep(10_000);
        const seen = Number(await js(viewer, 'window.__clipMsgs.length'));
        for (const [n, secret] of [true, false].entries()) {
            const owner = await Owner.take(s.host, s.wayland, s.runtime, `${secret ? 'pw' : 'text'}-${tag}-viewer-${n}-Zq8!`, HINTS[0], secret);
            await sleep(3000);
            await owner.release();
            await sleep(1500);
        }
        const got = await js(viewer, `window.__clipMsgs.slice(${seen})`);
        const wrote = Number(await js(viewer, 'window.__clipWrites.length'));
        const previews = Array.isArray(got) ? got.length : 0;
        res.check(`${tag} a shared viewer's page takes nothing of the session's clipboard, a secret or not`, previews === 0 && wrote === 0, `${previews} previews, ${wrote} writes`);
    } finally {
        await ctx.close();
    }
}

async function block(host: Host, res: Checks, mode: string, wayland: boolean): Promise<void> {
    for (const [i, dashboard] of LAYOUTS.entries()) {
        const tag = `[${wayland ? 'wl' : mode}/${dashboard}]`;
        const { url, pid } = await host.start({ label: `${wayland ? 'wl' : mode}-${dashboard}`, mode, wayland, env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: dashboard } });
        const s: Session = { host, wayland, runtime: runtimeDir(pid) };
        const [close, page, connected, browser] = await openPage('chromium', url, mode);
        try {
            res.check(`${tag} the session's clipboard policy arrived`, connected);
            await maskedChecks(res, tag, page, s, dashboard, 'chromium', i + 1);
            if (dashboard === 'default') {
                await takeBackChecks(res, tag, page, s);
                if (browser !== null) await viewerChecks(res, tag, browser, url, mode, s);
            }
        } finally {
            await close();
            await host.stop();
        }
    }
}

/** Firefox and WebKit: masked and kept nowhere; nothing read to take it back. */
async function enginesBlock(host: Host, res: Checks): Promise<void> {
    const { url, pid } = await host.start({ label: 'engines', mode: 'websockets', env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: 'default' } });
    const s: Session = { host, wayland: false, runtime: runtimeDir(pid) };
    try {
        for (const engine of ['firefox', 'webkit'] as const) {
            const tag = `[websockets/${engine}]`;
            let opened: Awaited<ReturnType<typeof openPage>>;
            try {
                opened = await openPage(engine, url, 'websockets');
            } catch (e) {
                res.skip(`${tag} engine unavailable`, String(e).slice(0, 120));
                continue;
            }
            const [close, page, connected] = opened;
            try {
                res.check(`${tag} the session's clipboard policy arrived`, connected);
                await maskedChecks(res, tag, page, s, 'default', engine, engine === 'firefox' ? 3 : 4);
                const reads = Number(await js(page, 'window.__clipReads'));
                const owner = await Owner.take(host, false, s.runtime, `pw-${engine}-limit-Zq8!`);
                await waitUntil(async () => (await lastPreview(page)).secret === true, 10);
                await owner.release();
                await sleep(4000);
                const after = Number(await js(page, 'window.__clipReads'));
                res.check(`${tag} nothing is read to take a secret back, which would raise a paste prompt`, after === reads, `${after - reads} reads`);
            } finally {
                await close();
            }
        }
    } finally {
        await host.stop();
    }
}

for (const which of ['websockets', 'webrtc', 'wayland', 'engines']) {
    test(which, async () => {
        const res = new Checks(which === 'engines' ? 'clipsecret-engines' : `clipsecret-${which === 'wayland' ? 'wl' : which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            if (which === 'engines') await enginesBlock(host, res);
            else await block(host, res, which === 'wayland' ? 'websockets' : which, which === 'wayland');
        } finally {
            await host.close();
        }
        res.finish();
    });
}
