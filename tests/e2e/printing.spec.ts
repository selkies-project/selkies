/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Printing end to end: a document printed in the session reaches the page.
//
// websockets / webrtc: a PDF spooled before the page connects is announced on
// connect; one printed through the session's own CUPS queue
// (`lp -d Selkies`) while the page is up is announced live, fetched whole,
// taken out of the spool, and, with automatic printing on, opened in a frame
// for the browser's print dialog (a headless page counts the print it asks of
// the frame, since headless Chrome crashes the PDF viewer's frame on one).
// Both dashboards list each document with its switch, print, and save
// controls; the switch off leaves the next document in the list only, and the
// save link hands the browser the same bytes. A second display page of the
// same browser is told nothing. The websockets block also drives the
// installed Chrome on the test display and sees its print preview open with
// the document.
//
// policy: `printing_enabled=false` starts no queue, announces nothing, and
// refuses the route; a shared viewer page is never told of a document.
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { request as httpRequest } from 'node:http';
import { delimiter, join } from 'node:path';

import type { Browser, Locator, Page } from '@playwright/test';
import { chromium, test } from '@playwright/test';

import { BROWSER_ARGS, launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { barOpenMenuItem } from '../lib/dashboard.ts';
import { onPath } from '../lib/desktop.ts';
import { DASHBOARD_DIST, sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host, runtimeDir } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const LAYOUTS = ['default', 'wish', 'edge'] as const;
const SPOOL = join(WORKDIR, 'print-spool');
const CUPS_PATH = `${process.env.PATH ?? ''}${delimiter}/usr/sbin`;
const SERVER_ENV = { SELKIES_PRINT_SPOOL_PATH: SPOOL };

// Records every document the core hands the dashboards.
const PRINT_JS = `
  window.__printed = [];
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'printDocument') window.__printed.push({ name: e.data.name, size: e.data.size, url: e.data.url });
  });
`;
const BLOB_JS = `(async (url) => {
    const bytes = new Uint8Array(await (await fetch(url)).arrayBuffer());
    let s = ''; for (const b of bytes) s += String.fromCharCode(b);
    return btoa(s);
})`;
const FRAMES_JS = 'document.querySelectorAll(\'iframe[src^="blob:"]\').length';
// Headless Chrome crashes the PDF viewer's frame when it is asked to print,
// and Playwright fails the page with it. A headless page counts the print it
// asks of a document's frame instead; the installed Chrome on the test
// display prints for real.
const HEADLESS_PRINT_JS = `
  window.__printCalls = 0;
  (() => {
    const real = Object.getOwnPropertyDescriptor(HTMLIFrameElement.prototype, 'contentWindow');
    Object.defineProperty(HTMLIFrameElement.prototype, 'contentWindow', {
      configurable: true,
      get() {
        const w = real.get.call(this);
        if (!w || !String(this.src).startsWith('blob:')) return w;
        return new Proxy(w, {
          get(target, key) {
            if (key === 'print') return () => { window.__printCalls++; };
            const value = Reflect.get(target, key);
            return typeof value === 'function' ? value.bind(target) : value;
          },
        });
      },
    });
  })();
`;
// Headless Chromium never closes a print dialog, so `afterprint` does not
// fire and the frames the core opens stay, each with a PDF viewer behind it.
// That viewer's process ends when the frame goes or when a download starts,
// and the driver reads either as the page crashing, failing whatever call is
// in flight.
const DROP_FRAMES_JS = 'document.querySelectorAll(\'iframe[src^="blob:"]\').forEach((f) => f.remove())';

// A one-page PDF drawing one rectangle, built by hand.
function samplePdf(): Buffer {
    const content = '0 0 1 rg 100 100 200 300 re f';
    const objects = [
        '<< /Type /Catalog /Pages 2 0 R >>',
        '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
        '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R >>',
        `<< /Length ${content.length} >>stream\n${content}\nendstream`,
    ];
    let out = '%PDF-1.4\n';
    const offsets: number[] = [];
    for (const [i, obj] of objects.entries()) {
        offsets.push(out.length);
        out += `${i + 1} 0 obj\n${obj}\nendobj\n`;
    }
    const xref = out.length;
    out += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
    out += offsets.map((o) => `${String(o).padStart(10, '0')} 00000 n \n`).join('');
    out += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
    return Buffer.from(out, 'latin1');
}

const PDF = samplePdf();
const sha = (data: Buffer): string => createHash('sha256').update(data).digest('hex');
const r = (v: unknown): string => JSON.stringify(v);

function freshSpool(): void {
    rmSync(SPOOL, { recursive: true, force: true });
    mkdirSync(SPOOL, { recursive: true });
}

/** A document dropped into the spool the way the backend does it. */
function spool(name: string, data: Buffer = PDF): string {
    const part = join(SPOOL, `.${name}.part`);
    writeFileSync(part, data);
    renameSync(part, join(SPOOL, name));
    return join(SPOOL, name);
}

const cupsSocket = (runtime: string): string => join(runtime, 'selkies-cups', 'cups.sock');

function lpOnPath(): boolean {
    return CUPS_PATH.split(delimiter).some((dir) => dir && existsSync(join(dir, 'lp')));
}

const queueAvailable = (runtime: string): boolean => existsSync(cupsSocket(runtime)) && lpOnPath();

/** Print `path` to the session's Selkies queue under `title`. */
function lp(runtime: string, title: string, path: string): boolean {
    const env = { PATH: CUPS_PATH, CUPS_SERVER: cupsSocket(runtime), HOME: WORKDIR };
    return spawnSync('lp', ['-d', 'Selkies', '-t', title, path], { env, timeout: 60_000 }).status === 0;
}

/** The status `/api/print/<name>` answers with, refusals included. */
function routeStatus(port: number, name: string): Promise<number> {
    return new Promise((resolve, reject) => {
        const req = httpRequest({ host: 'localhost', port, path: `/api/print/${encodeURIComponent(name)}`, timeout: 10_000 }, (res) => {
            res.resume();
            resolve(res.statusCode ?? 0);
        });
        req.on('error', reject);
        req.end();
    });
}

interface Printed {
    name: unknown;
    size: unknown;
    url: unknown;
}

async function printed(page: Page): Promise<Printed[]> {
    const got = await within(page.evaluate('window.__printed'));
    return Array.isArray(got) ? got.filter((d): d is Printed => d !== null && typeof d === 'object') : [];
}

async function waitPrinted(page: Page, count: number, timeoutS = 30): Promise<Printed[]> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        const got = await printed(page);
        if (got.length >= count) return got;
        await sleep(250);
    }
    return printed(page);
}

const frames = async (page: Page): Promise<number> => Number(await within(page.evaluate(FRAMES_JS)));
const printCalls = async (page: Page): Promise<number> => Number(await within(page.evaluate('window.__printCalls || 0')));

/** Whether the page asked `count` documents' frames to print within `timeoutS`. */
async function waitPrintCalls(page: Page, count: number, timeoutS = 10): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if ((await printCalls(page)) >= count) return true;
        await sleep(250);
    }
    return false;
}

async function waitGone(path: string, timeoutS = 10): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (!existsSync(path)) return true;
        await sleep(200);
    }
    return !existsSync(path);
}

async function blobBytes(page: Page, url: unknown): Promise<Buffer> {
    try {
        return Buffer.from(String(await within(page.evaluate(`${BLOB_JS}(${r(url)})`))), 'base64');
    } catch (e) {
        process.stdout.write(`      (blob fetch: ${String(e)})\n`);
        return Buffer.alloc(0);
    }
}

/** A page on the dashboard with the print recorder installed; the installed Chrome on the test display when `headed`. */
async function newPage(url: string, mode: string, hash = '', headed = false): Promise<[Browser, Page]> {
    const browser = headed
        ? await chromium.launch({ headless: false, executablePath: onPath('google-chrome') ?? undefined, args: [...BROWSER_ARGS], env: { ...process.env, DISPLAY: process.env.E2E_DISPLAY ?? '' } })
        : await launchChromium();
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: PRINT_JS });
    if (!headed) await ctx.addInitScript({ content: HEADLESS_PRINT_JS });
    const page = await ctx.newPage();
    await page.goto(`${url}/${hash}`, { waitUntil: 'load' });
    return [browser, page];
}

const waitVideo = (page: Page, mode: string): Promise<unknown> => (mode === 'webrtc' ? waitWrVideo(page, 45) : waitWsVideo(page, 30));

/** Reach the printing controls: the sidebar's Printing section, or a bar layout's menu's Printing submenu. */
async function openPrintingUi(page: Page, dashboard: string): Promise<boolean> {
    if (dashboard === 'default') {
        try {
            if ((await page.locator('#printing-content').count()) === 0) {
                await page.locator('.toggle-handle').first().click();
                await sleep(600);
            }
            if ((await page.locator('#printing-content').count()) === 0) {
                await page.locator('.sidebar-section-header:has-text("Printing")').first().click();
                await sleep(600);
            }
            return (await page.locator('#printing-content').count()) > 0;
        } catch (e) {
            process.stdout.write(`      (printing section: ${String(e)})\n`);
            return false;
        }
    }
    for (let i = 0; i < 4; i++) {
        if ((await page.locator('[role="menu"]').count()) === 0) break;
        await page.keyboard.press('Escape');
        await sleep(300);
    }
    return barOpenMenuItem(page, 'Printing');
}

/** Documents listed: one save link each, whatever else the row offers. */
async function rows(page: Page, dashboard: string): Promise<number> {
    return dashboard === 'default' ? page.locator('#printing-content .print-job').count() : page.locator('[role="menu"] a[download]').count();
}

/** Drop the preview frames and wait for their viewers to go, so the teardown lands between calls rather than inside the next one. */
async function dropFrames(page: Page, timeoutS = 10): Promise<void> {
    await within(page.evaluate(DROP_FRAMES_JS));
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until && (await frames(page));) await sleep(200);
    await sleep(1000);
}

/** Click a control; the sidebar scrolls, so a section below the fold takes a dispatched click rather than a pointer one. */
async function press(control: Locator): Promise<void> {
    await control.dispatchEvent('click');
    await sleep(500);
}

/** A document row's icon control, named by its title ("Print" or "Save"). */
function rowControl(page: Page, dashboard: string, label: string, index = 0): Locator {
    if (dashboard === 'default') return page.locator('#printing-content .print-job').nth(index).locator(`[title="${label}"]`);
    return page.locator(`[role="menu"] :is(button, a)[title="${label}"]`).nth(index);
}

/** Whether Chrome's print preview is showing a PDF: its own page and the document it renders are targets of the browser. */
async function printPreviewOpen(browser: Browser): Promise<boolean> {
    const cdp = await browser.newBrowserCDPSession();
    for (let i = 0; i < 40; i++) {
        const { targetInfos } = await cdp.send('Target.getTargets');
        const urls = targetInfos.map((t) => t.url);
        if (urls.some((u) => u.startsWith('chrome://print')) && urls.some((u) => u.includes('print.pdf'))) return true;
        await sleep(250);
    }
    return false;
}

/** The sample PDF written outside the spool, for lp to read. */
function writeSample(name: string): string {
    const path = join(WORKDIR, name);
    writeFileSync(path, PDF);
    return path;
}

const dash = (layout: string | null, env: Record<string, string> = {}): Record<string, string> => ({ SELKIES_WEB_ROOT: DASHBOARD_DIST, ...(layout ? { SELKIES_UI_LAYOUT: layout } : {}), ...SERVER_ENV, ...env });
const names = (got: Printed[]): unknown[] => got.map((d) => d.name);

async function dashboardRound(host: Host, res: Checks, mode: string, dashboard: string): Promise<void> {
    freshSpool();
    const pending = spool('Pending.pdf');
    const { url, pid } = await host.start({ label: `${dashboard}-${mode}`, mode, env: dash(dashboard) });
    const runtime = runtimeDir(pid);
    const [browser, page] = await newPage(url, mode);
    try {
        res.check(`${dashboard}: video streams over ${mode}`, (await waitVideo(page, mode)) !== null);
        let got = await waitPrinted(page, 1);
        res.check(`${dashboard}: a document spooled before the connection is announced on connect`, r(names(got)) === r(['Pending.pdf']), got);
        res.check(`${dashboard}: the page holds the document's bytes`, got.length > 0 && (await blobBytes(page, got[0].url)).equals(PDF));
        res.check(`${dashboard}: the document left the spool once taken`, await waitGone(pending));
        res.check(`${dashboard}: automatic printing opened it in a frame and printed it`, (await frames(page)) === 1 && (await waitPrintCalls(page, 1)), [await frames(page), await printCalls(page)]);
        res.check(`${dashboard}: the printing controls appear with the document`, (await openPrintingUi(page, dashboard)) && (await rows(page, dashboard)) === 1, await rows(page, dashboard));

        await press(page.locator('#printAutoToggle'));
        if (queueAvailable(runtime)) {
            res.check(`${dashboard}: lp prints to the Selkies queue`, lp(runtime, 'Quarterly report', writeSample('quarterly.pdf')));
        } else {
            res.skip(`${dashboard}: lp prints to the Selkies queue`, 'no cupsd on this host, spooling directly');
            spool('Quarterly report.pdf');
        }
        got = await waitPrinted(page, 2);
        res.check(`${dashboard}: a document printed while connected is announced live as a PDF`,
            r(names(got)) === r(['Pending.pdf', 'Quarterly report.pdf']) && (await blobBytes(page, got[1].url)).subarray(0, 4).toString() === '%PDF', got);
        res.check(`${dashboard}: with automatic printing off it waits in the list`, (await frames(page)) === 1 && (await openPrintingUi(page, dashboard)) && (await rows(page, dashboard)) === 2,
            [await frames(page), await rows(page, dashboard)]);
        await press(rowControl(page, dashboard, 'Print', 1));
        await sleep(1000);
        res.check(`${dashboard}: the print button opens it in a frame and prints it`, (await frames(page)) === 2 && (await waitPrintCalls(page, 2)), [await frames(page), await printCalls(page)]);
        await dropFrames(page);
        // A bar layout's panel is a menu, and printing from it closed it.
        await openPrintingUi(page, dashboard);
        const [download] = await Promise.all([page.waitForEvent('download', { timeout: 15_000 }), press(rowControl(page, dashboard, 'Save', 0))]);
        const saved = await download.path();
        res.check(`${dashboard}: the save link hands the browser the same bytes under the document's name`, download.suggestedFilename() === 'Pending.pdf' && sha(readFileSync(saved)) === sha(PDF), download.suggestedFilename());

        const second = await browser.contexts()[0].newPage();
        await second.goto(`${url}/#display2`, { waitUntil: 'load' });
        await sleep(3000);
        spool('Third.pdf');
        got = await waitPrinted(page, 3);
        await sleep(1000);
        const told = await printed(second);
        res.check(`${dashboard}: a second display page of the same browser is told nothing`, got.length === 3 && told.length === 0, told);
        await second.close();
    } finally {
        await browser.close();
        await host.stop();
    }
}

async function transportBlock(host: Host, res: Checks, mode: string): Promise<void> {
    for (const dashboard of LAYOUTS) await dashboardRound(host, res, mode, dashboard);
    if (mode !== 'websockets') return;
    if (onPath('google-chrome') && process.env.E2E_DISPLAY) {
        freshSpool();
        spool('Preview.pdf');
        const { url } = await host.start({ label: 'chrome', mode, env: dash(null) });
        const [browser, page] = await newPage(url, mode, '', true);
        try {
            res.check('Chrome: video streams', (await waitVideo(page, mode)) !== null);
            res.check('Chrome: the document arrives', (await waitPrinted(page, 1)).length === 1);
            res.check("Chrome: the browser's print preview opens with the document", await printPreviewOpen(browser));
        } finally {
            await browser.close();
            await host.stop();
        }
    } else {
        res.skip("Chrome: the browser's print preview opens with the document", 'google-chrome or the test display is missing');
    }
}

async function policyBlock(host: Host, res: Checks): Promise<void> {
    freshSpool();
    spool('Off.pdf');
    const off = await host.start({ label: 'policy-off', mode: 'websockets', env: dash(null, { SELKIES_PRINTING_ENABLED: 'false' }) });
    const port = Number(new URL(off.url).port);
    res.check('printing off: no queue is started', !existsSync(cupsSocket(runtimeDir(off.pid))));
    const status = await routeStatus(port, 'Off.pdf');
    res.check('printing off: the route refuses', status === 403, await routeStatus(port, 'Off.pdf'));
    let [browser, page] = await newPage(off.url, 'websockets');
    try {
        res.check('printing off: the page streams', (await waitVideo(page, 'websockets')) !== null);
        await sleep(3000);
        res.check('printing off: nothing is announced and the document stays put', (await printed(page)).length === 0 && existsSync(join(SPOOL, 'Off.pdf')));
        res.check('printing off: no printing section', (await page.locator('.sidebar-section-header:has-text("Printing")').count()) === 0);
    } finally {
        await browser.close();
    }

    freshSpool();
    await host.stop();
    const { url } = await host.start({ label: 'policy-viewer', mode: 'websockets', env: dash(null) });
    [browser, page] = await newPage(url, 'websockets');
    const [viewerBrowser, viewer] = await newPage(url, 'websockets', '#shared');
    try {
        res.check('the owner page streams', (await waitVideo(page, 'websockets')) !== null);
        res.check('the viewer page streams', (await waitVideo(viewer, 'websockets')) !== null);
        spool('Owner.pdf');
        const got = await waitPrinted(page, 1);
        await sleep(2000);
        const told = await printed(viewer);
        res.check('a document goes to the owner and never to a shared viewer', r(names(got)) === r(['Owner.pdf']) && told.length === 0, told);
    } finally {
        await browser.close();
        await viewerBrowser.close();
    }
}

for (const which of ['websockets', 'webrtc', 'policy']) {
    test(which, async () => {
        const res = new Checks(`printing-${which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            if (which === 'policy') await policyBlock(host, res);
            else await transportBlock(host, res, which);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
