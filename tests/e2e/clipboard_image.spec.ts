/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The image clipboard, both directions, against a live session.
//
// The dashboard's Upload Image button is the only way an image the user did
// not copy reaches the session clipboard. Choosing the file takes the
// window's focus while the dialog is open -- which closes the toolbar panel's
// menu -- and gives it back as the dialog closes, which fires the
// focus-driven local sync. Whether the image survives that is the whole
// feature, so the checks read the session's own clipboard rather than the
// message that carried the image to the core, in both dashboards; a JPEG has
// to reach it as a PNG as well, since that is the only image type most
// applications paste.
//
// The other way, a session image has to land on the local clipboard even
// when the user leaves the tab right after copying it, as someone switching
// to the application they copied it for does: the browser refuses a
// clipboard write from a page that has lost focus (Chromium) or its user
// activation (Firefox, WebKit), and a multi-megabyte image is still crossing
// the link by then; and a copy made while a larger one is still crossing has
// to be the one that lands. The page under test is taken out of Playwright's
// focus emulation, under which it never loses focus, and the local clipboard
// is read from the tab the user went to, before they come back.
import { spawn, spawnSync } from 'node:child_process';

import type { BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks, finishAll } from '../lib/checks.ts';
import { pickClipboardImage } from '../lib/dashboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host, runtimeDir, type Held } from '../lib/host.ts';
import { noisePng, pngSize, seededPng } from '../lib/png.ts';
import { within } from '../lib/stream.ts';

const LAYOUTS = ['default', 'wish', 'edge'] as const;
const WL_SOCKET = 'wayland-1';

const READ_LOCAL_IMAGE_JS = `(async () => {
  try {
    const items = await navigator.clipboard.read();
    for (const item of items) {
      for (const type of item.types) {
        if (!type.startsWith('image/')) continue;
        const blob = await item.getType(type);
        const bmp = await createImageBitmap(blob);
        return { type, size: blob.size, w: bmp.width, h: bmp.height };
      }
    }
    return null;
  } catch (err) { return 'read failed: ' + err.name; }
})()`;

interface LocalImage {
    type: string;
    size: number;
    w: number;
    h: number;
}

const isImage = (v: unknown): v is LocalImage => v !== null && typeof v === 'object' && 'w' in v && 'h' in v;

/** The session clipboard as a paste sees it: the X server's, or the capture compositor's. */
class Session {
    private readonly host: Host;
    private readonly wayland: boolean;
    private readonly runtime: string;

    constructor(host: Host, wayland: boolean, runtime: string) {
        this.host = host;
        this.wayland = wayland;
        this.runtime = runtime;
    }

    private get wlEnv(): NodeJS.ProcessEnv {
        return { ...process.env, WAYLAND_DISPLAY: WL_SOCKET, XDG_RUNTIME_DIR: this.runtime };
    }

    /** The offered targets and the image bytes: those of `want` when it names a type, else of the first image type offered. */
    async image(want = ''): Promise<[string[], string | null, Buffer | null]> {
        const pick = (targets: string[]): string | undefined => targets.find((t) => (want ? t === want : t.startsWith('image/')));
        if (this.wayland) {
            const listed = spawnSync('wl-paste', ['-l'], { encoding: 'utf8', timeout: 8000, env: this.wlEnv });
            const targets = (listed.stdout ?? '').split('\n').map((t) => t.trim()).filter(Boolean);
            const mime = pick(targets);
            if (mime === undefined) return [targets, null, null];
            const got = spawnSync('wl-paste', ['-t', mime], { timeout: 10_000, env: this.wlEnv });
            return [targets, mime, Buffer.isBuffer(got.stdout) ? got.stdout : null];
        }
        const targets = (await this.host.x11Targets()) ?? [];
        const mime = pick(targets);
        if (mime === undefined) return [targets, null, null];
        return [targets, mime, (await this.host.x11Convert(mime, 8)) ?? Buffer.alloc(0)];
    }

    /** Put `data` on the session clipboard as image/png, as an application would; what ends the owner. */
    async own(data: Buffer): Promise<() => Promise<void>> {
        if (this.wayland) {
            await new Promise<void>((resolve) => {
                const copy = spawn('wl-copy', ['-t', 'image/png'], { env: this.wlEnv, stdio: ['pipe', 'ignore', 'ignore'] });
                copy.on('exit', () => { resolve(); });
                copy.on('error', () => { resolve(); });
                copy.stdin.end(data);
            });
            return () => Promise.resolve();
        }
        const owner: Held = await this.host.ownClipboard(null, { 'image/png': data });
        return () => owner.release();
    }
}

/** A JPEG, as a photo picked from disk arrives, encoded by the browser. */
async function jpeg(page: Page, width: number, height: number): Promise<Buffer> {
    const bytes = await within(page.evaluate(async ([w, h]) => {
        const canvas = new OffscreenCanvas(w, h);
        const g = canvas.getContext('2d');
        if (g) {
            g.fillStyle = 'rgb(20, 120, 200)';
            g.fillRect(0, 0, w, h);
        }
        const blob = await canvas.convertToBlob({ type: 'image/jpeg' });
        return Array.from(new Uint8Array(await blob.arrayBuffer()));
    }, [width, height]));
    return Buffer.from(bytes);
}

/**
 * Upload `data` through the dashboard's button and read the session clipboard
 * back. The dialog's own refocus as it closes fires the focus read, which is
 * what the upload has to survive.
 */
async function upload(page: Page, session: Session, dashboard: string, name: string, mimeType: string, buffer: Buffer, want = ''): Promise<[string[], string | null, Buffer | null] | null> {
    if (!(await pickClipboardImage(page, dashboard, { name, mimeType, buffer }))) return null;
    await within(page.evaluate(() => { window.dispatchEvent(new Event('focus')); }));
    await sleep(5000);
    return session.image(want);
}

/**
 * Switch to another tab `leaveAfterS` seconds from now, wait for the transfer,
 * and read the local clipboard from there, before coming back. Coming back
 * would land a write the page stashed for its next focus, which is exactly
 * what the user who pasted meanwhile did not get.
 */
async function leaveAndRead(ctx: BrowserContext, page: Page, url: string, leaveAfterS: number): Promise<{ image: unknown; 'page kept focus': unknown }> {
    await sleep(leaveAfterS * 1000);
    const other = await ctx.newPage();
    try {
        // A still image of the same origin: a document to read from that starts no client.
        await other.goto(`${url}/icon-512.png`, { waitUntil: 'load' });
    } catch {
        // the read below says what there was
    }
    await other.bringToFront();
    await sleep(300);
    const focused = await within(page.evaluate(() => document.hasFocus()));
    await sleep(6000);
    const local = await within(other.evaluate(READ_LOCAL_IMAGE_JS));
    await other.close();
    await page.bringToFront();
    await sleep(500);
    return { image: local, 'page kept focus': focused };
}

const count = (log: string, needle: string): number => log.split(needle).length - 1;
const describe = (data: Buffer | null): number => data?.length ?? 0;

async function block(host: Host, res: Checks, mode: string, wayland: boolean, dashboard: string): Promise<void> {
    const tag = res.block;
    const uploaded = seededPng(23);
    const { url, pid } = await host.start({ label: `${wayland ? 'wl' : mode}-${dashboard}`, mode, wayland, env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: dashboard, SELKIES_DEBUG: 'true' } });
    const session = new Session(host, wayland, runtimeDir(pid));
    const browser = await launchChromium();
    const owners: (() => Promise<void>)[] = [];
    try {
        const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: url }).catch(() => undefined);
        const page = await ctx.newPage();
        await page.goto(url, { waitUntil: 'load' });
        await sleep(12_000);
        // Something the user copied locally and has not synced: the value
        // the focus read would put back over the upload.
        await within(page.evaluate(() => navigator.clipboard.writeText('local text, not the image')));
        await sleep(1000);
        const got = await upload(page, session, dashboard, 'clip.png', 'image/png', uploaded);
        if (got === null) {
            res.skip(`${tag}: the upload path`, 'no Upload Image button in the panel');
            return;
        }
        let [targets, mime, data] = got;
        res.check('an uploaded image reaches the session clipboard', data !== null && data.equals(uploaded), `${mime} ${describe(data)} bytes, offered ${JSON.stringify(targets)}`);

        const photo = await jpeg(page, 40, 30);
        [targets, mime, data] = (await upload(page, session, dashboard, 'photo.jpg', 'image/jpeg', photo, 'image/png')) ?? [[], null, null];
        const size = pngSize(data);
        res.check('an uploaded JPEG is offered to the session as PNG as well', mime === 'image/png' && size !== null && size[0] === 40 && size[1] === 30 && targets.includes('image/jpeg'),
            `${mime} ${JSON.stringify(size)}, offered ${JSON.stringify(targets)}`);
        if (dashboard !== 'default') return;

        const copied = seededPng(91);
        owners.push(await session.own(copied));
        // Two gestures: the write is refused without a user activation, and
        // the payload has to have arrived before the one that lands it.
        for (let i = 0; i < 2; i++) {
            await page.mouse.move(300, 300);
            await page.mouse.down();
            await page.mouse.up();
            await sleep(2500);
        }
        const local = await within(page.evaluate(READ_LOCAL_IMAGE_JS));
        // The browser re-encodes what it writes, so the size is its own; that
        // an image is there at all is what the push had to achieve.
        res.check('a session image reaches the local clipboard', isImage(local) && local.size > 0, local);

        // Copying the same image again is a fresh copy, not this server's own
        // write coming back: a client that failed to apply the first one has
        // nothing else to wait for. Counted in the log, since the client
        // suppresses a local write of content it already holds.
        const sends = count(await host.log(), 'Clipboard changed. Sending content');
        await owners.pop()?.();
        await sleep(1000);
        owners.push(await session.own(copied));
        await sleep(4000);
        const again = count(await host.log(), 'Clipboard changed. Sending content');
        res.check('the same image copied again is sent again', again > sends, `${sends} sends, then ${again}`);

        // From here the page can lose focus as a real window does.
        const cdp = await ctx.newCDPSession(page);
        await cdp.send('Emulation.setFocusEmulationEnabled', { enabled: false });
        await page.bringToFront();
        await sleep(500);
        const big = noisePng(1000, 700, 7);
        owners.push(await session.own(big));
        let seen = await leaveAndRead(ctx, page, url, 0.4);
        res.check(`a ${Math.floor(big.length / 1024)} KiB session image lands locally though the user left the tab right after copying it`,
            isImage(seen.image) && seen.image.w === 1000 && seen.image.h === 700 && seen['page kept focus'] === false, seen);

        const bigger = noisePng(1200, 800, 11);
        owners.push(await session.own(bigger));
        await sleep(250);
        const small = seededPng(57);
        owners.push(await session.own(small));
        seen = await leaveAndRead(ctx, page, url, 0.4);
        res.check('a copy made while a larger one is still crossing the link is the one that lands, though the user left right after it',
            isImage(seen.image) && seen.image.w === 8 && seen.image.h === 8, seen);
    } finally {
        for (const stop of owners) await stop();
        await browser.close();
        await host.stop();
    }
}

for (const which of ['websockets', 'webrtc', 'wayland']) {
    test(which, async () => {
        const [mode, wayland] = which === 'wayland' ? ['websockets', true] : [which, false];
        const host = new Host(suiteOf(import.meta.filename));
        const blocks: Checks[] = [];
        try {
            for (const dashboard of LAYOUTS) {
                const res = new Checks(`clipimage-${wayland ? 'wl' : mode}-${dashboard}`);
                blocks.push(res);
                await block(host, res, mode, wayland, dashboard);
            }
        } finally {
            await host.close();
        }
        finishAll(blocks);
    });
}
