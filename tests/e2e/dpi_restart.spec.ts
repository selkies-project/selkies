/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The desktop's density across a server restart.
//
// On X11 a page's density is written into the home as Xft resources, which
// the image loads again at the next start, so the desktop comes up at the
// last page's density while the server starts knowing none. The server reads
// the desktop before writing to it: a page at unity brings a desktop left at
// 2x back down, a page at that density rewrites nothing, an operator-set
// density is applied at startup even at unity, and a fresh home at unity
// stays untouched. On Wayland a density is a compositor scale that nothing
// persists: a page streams at its own density after a restart, and resources
// an X11 session left in the home are neither applied nor rewritten. Both
// transports.
//
// The X11 half runs on a throwaway Xvfb of its own.
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, statSync, unlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import type { Browser, BrowserContext } from '@playwright/test';
import { test } from '@playwright/test';

import { launchBrowser } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const FILES = ['.Xresources', '.xsettingsd'] as const;

const xEnv = (display: string): NodeJS.ProcessEnv => ({ DISPLAY: display, PATH: process.env.PATH ?? '' });

/** Xft.dpi in the display's resource database, null when unset. */
function xftDpi(display: string): number | null {
    const out = spawnSync('xrdb', ['-query'], { encoding: 'utf8', env: xEnv(display) }).stdout ?? '';
    const line = out.split('\n').find((l) => l.startsWith('Xft.dpi:'));
    return line ? parseInt(line.split(':', 2)[1], 10) : null;
}

/** The resources a session left in the home; null is a home that never had a density. */
function persist(home: string, dpi: number | null): void {
    for (const name of FILES) {
        try {
            unlinkSync(join(home, name));
        } catch {
            // not there
        }
    }
    if (dpi === null) return;
    writeFileSync(join(home, '.Xresources'), `Xft.dpi:   ${dpi}\n`);
    writeFileSync(join(home, '.xsettingsd'), `Xft/DPI ${dpi * 1024}\n`);
}

/** The state a restart finds on X11: the persisted resources, merged into the server as an image does at session start. */
function leaveDesktopAt(display: string, home: string, dpi: number | null): void {
    spawnSync('xrdb', ['-remove'], { env: xEnv(display) });
    persist(home, dpi);
    if (dpi !== null) {
        const merged = spawnSync('xrdb', ['-merge', join(home, '.Xresources')], { env: xEnv(display) });
        if (merged.status !== 0) throw new Error('xrdb -merge failed');
    }
}

type Persisted = Record<string, [number | null, string]>;

/** Each persisted file's density and mtime. */
function persisted(home: string): Persisted {
    const found: Persisted = {};
    for (const name of FILES) {
        const path = join(home, name);
        let text: string;
        try {
            text = readFileSync(path, 'utf8');
        } catch {
            continue;
        }
        const [key, unit] = name === '.xsettingsd' ? ['Xft/DPI', 1024] : ['Xft.dpi', 1];
        const values = text.split('\n').map((l) => l.replace(/:/g, ' ').split(/\s+/).filter(Boolean))
            .filter((f) => f.length === 2 && f[0] === key && /^\d+$/.test(f[1])).map((f) => Math.floor(parseInt(f[1], 10) / unit));
        found[name] = [values.at(-1) ?? null, String(statSync(path, { bigint: true }).mtimeNs)];
    }
    return found;
}

const densities = (home: string): Set<number | null> => new Set(Object.values(persisted(home)).map(([dpi]) => dpi));
const only = (set: Set<number | null>, dpi: number): boolean => set.size === 1 && set.has(dpi);
const same = (a: unknown, b: unknown): boolean => JSON.stringify(a) === JSON.stringify(b);

async function waitFor(pred: () => boolean | Promise<boolean>, timeoutS = 20): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (await pred()) return true;
        await sleep(250);
    }
    return false;
}

const BUFFER_WIDTH_JS = `(() => {
  const c = document.getElementById('videoCanvas'), v = document.getElementById('stream');
  return (c && c.width > 0) ? c.width : (v && v.videoWidth > 0) ? v.videoWidth : 0;
})()`;

/** Stream pixels per CSS pixel of the page's 1280-wide viewport. */
async function streamScale(ctx: BrowserContext): Promise<number> {
    return Number(await within(ctx.pages()[0].evaluate(BUFFER_WIDTH_JS))) / 1280;
}

/** A streaming page on a screen of the given density; its context. */
async function pageAt(browser: Browser, url: string, mode: string, dpr: number, log: string): Promise<BrowserContext> {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: dpr });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    const video = mode === 'webrtc' ? await waitWrVideo(page, 45) : await waitWsVideo(page, 45);
    if (!video) {
        await ctx.close();
        throw new Error(`no ${mode} video for a ${dpr}x page; see ${log}`);
    }
    return ctx;
}

async function runMode(host: Host, res: Checks, browser: Browser, mode: string, home: string, display: string): Promise<void> {
    const applied = mode === 'websockets' ? 'DPI changed from 192 to 96' : 'Successfully set DPI to 96';
    const start = async (label: string, env: Record<string, string> = {}): Promise<{ url: string; log: string }> => {
        await host.stop();
        return host.start({ label, mode, display, env: { HOME: home, ...env } });
    };

    leaveDesktopAt(display, home, 192);
    let { url, log } = await start(`${mode}-down`);
    let ctx = await pageAt(browser, url, mode, 1, log);
    res.check(`${mode}: a 1x page brings a desktop left at 192 back to 96`, await waitFor(() => xftDpi(display) === 96), xftDpi(display));
    res.check(`${mode}: the persisted resources follow`, await waitFor(() => only(densities(home), 96)), persisted(home));
    res.check(`${mode}: the page's density read as a change from 192`, (await host.log()).includes(applied), applied);
    await ctx.close();

    leaveDesktopAt(display, home, 192);
    ({ url, log } = await start(`${mode}-kept`));
    await sleep(3000);
    const before = persisted(home);
    ctx = await pageAt(browser, url, mode, 2, log);
    await sleep(3000);
    res.check(`${mode}: a 2x page leaves a desktop at 192 where it is`, xftDpi(display) === 192, xftDpi(display));
    res.check(`${mode}: and rewrites nothing`, same(persisted(home), before), [before, persisted(home)]);
    await ctx.close();

    leaveDesktopAt(display, home, 192);
    ({ url, log } = await start(`${mode}-operator`, { SELKIES_SCALING_DPI: '96' }));
    res.check(`${mode}: an operator-set 96 resets a desktop left at 192 before any page`, await waitFor(() => xftDpi(display) === 96 && only(densities(home), 96)), [xftDpi(display), persisted(home)]);
    ctx = await pageAt(browser, url, mode, 2, log);
    await sleep(3000);
    res.check(`${mode}: and a 2x page cannot move it`, xftDpi(display) === 96 && only(densities(home), 96), [xftDpi(display), persisted(home)]);
    await ctx.close();

    leaveDesktopAt(display, home, null);
    ({ url, log } = await start(`${mode}-fresh`));
    ctx = await pageAt(browser, url, mode, 1, log);
    await sleep(3000);
    res.check(`${mode}: a fresh home at unity is left untouched`, xftDpi(display) === null && Object.keys(persisted(home)).length === 0, [xftDpi(display), persisted(home)]);
    await ctx.close();
}

async function runWayland(host: Host, res: Checks, browser: Browser, mode: string, home: string): Promise<void> {
    persist(home, 192);
    await host.stop();
    let { url, log } = await host.start({ label: `${mode}-wl-2x`, mode, wayland: true, env: { HOME: home } });
    const before = persisted(home);
    let ctx = await pageAt(browser, url, mode, 2, log);
    res.check(`${mode} on Wayland: a 2x page streams at its own density`, await waitFor(async () => Math.abs((await streamScale(ctx)) - 2) < 0.05), await streamScale(ctx));
    await ctx.close();

    await host.stop();
    ({ url, log } = await host.start({ label: `${mode}-wl-1x`, mode, wayland: true, env: { HOME: home } }));
    ctx = await pageAt(browser, url, mode, 1, log);
    res.check(`${mode} on Wayland: after a restart a 1x page streams at unity`, await waitFor(async () => Math.abs((await streamScale(ctx)) - 1) < 0.05), await streamScale(ctx));
    res.check(`${mode} on Wayland: X resources left in the home are neither applied nor rewritten`, same(persisted(home), before), [before, persisted(home)]);
    await ctx.close();
}

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const res = new Checks('dpi-restart');
        const host = new Host(suiteOf(import.meta.filename));
        const home = mkdtempSync(join(tmpdir(), 'selkies-dpi-home-'));
        try {
            const xvfb = await host.xvfb(1280, 720);
            const browser = await launchBrowser('chromium');
            try {
                await runMode(host, res, browser, mode, home, xvfb.display);
                await runWayland(host, res, browser, mode, home);
            } finally {
                await browser.close();
                await host.stop();
                await xvfb.release();
            }
        } finally {
            rmSync(home, { recursive: true, force: true });
            await host.close();
        }
        res.finish();
    });
}
