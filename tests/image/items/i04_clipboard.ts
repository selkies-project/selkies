/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// 4. Use very large text clipboard payloads (>2MB) and image payloads. Check if
// the Image Upload (clipboard) button works as well.
//
// With Chrome's focus emulation off and the client headed on its own X display,
// the client's clipboard is the real X one, read and written there with xclip
// as a local application would. Both ways, for 2.5 MB of text and a multi-MB
// PNG: the session's side is xclip on the session's display (an application
// copying or pasting there). The page must hold focus for the browser to touch
// the clipboard at all, and is clicked first, as a user does; it is also left
// and re-entered, which is when Chromium reads a local copy. Upload Image goes
// through the clipboard panel's own button and file chooser.
//
// Headless WebKit has no X clipboard: there only what reaches the page is
// checked, and Upload Image.
import { createHash } from 'node:crypto';

import type { Page } from '@playwright/test';

import { fields, numberOr, stringOr } from '../../../web/selkies-web-core/lib/narrow.ts';
import { pickClipboardImage } from '../../lib/dashboard.ts';
import { sleep } from '../../lib/env.ts';
import { Host, type Held } from '../../lib/host.ts';
import { noisePng } from '../../lib/png.ts';
import { openSidebar, requireDisplay, sessionBrowser, stopSessionBrowser, type Cell } from '../lib.ts';

export const ITEM = 4;
export const TITLE = 'clipboard: >2 MB text, multi-MB images, Upload Image';
const CLASS: Record<string, string> = { chromium: 'google-chrome|chromium|Chromium', firefox: 'firefox|Firefox' };
const TMP = '/tmp/selkies-imagetest';

type Dims = readonly [number, number] | null;

/** A PNG's width and height from its first 24 bytes, or null for anything else. */
function dims(head: Buffer): Dims {
    if (head.length >= 24 && head.subarray(0, 8).equals(Buffer.from([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]))) {
        return [head.readUInt32BE(16), head.readUInt32BE(20)];
    }
    return null;
}

const asDims = (v: string | Dims): Dims => (typeof v === 'string' ? null : v);
const sameDims = (a: Dims, b: Dims): boolean => a !== null && b !== null && a[0] === b[0] && a[1] === b[1];

function bigText(tag: string): Buffer {
    const line = Buffer.from(`selkies image tier ${tag} ${'0123456789abcdef'.repeat(4)}\n`);
    const size = 2600 * 1024;
    return Buffer.concat(Array.from({ length: Math.floor(size / line.length) + 1 }, () => line)).subarray(0, size);
}

/** Real X focus on the client display: the browser's window, or a plain one of our own. */
class Focus {
    private host: Host | null = null;
    private other: (Held & { id: number }) | null = null;
    private readonly cell: Cell;

    constructor(cell: Cell) {
        this.cell = cell;
    }

    private async window(): Promise<string | null> {
        const out = (await this.cell.xclient(['xdotool', 'search', '--onlyvisible', '--class', CLASS[this.cell.engine] ?? ''])).stdout.toString().split(/\s+/).filter(Boolean);
        return out[0] ?? null;
    }

    async enter(page: Page): Promise<boolean> {
        const wid = await this.window();
        if (wid) await this.cell.xclient(['xdotool', 'windowfocus', '--sync', wid]);
        await page.bringToFront();
        // A click on the stream, as a user's first one: it lands on the session's pattern page.
        await page.mouse.click(900, 600);
        await sleep(500);
        return page.evaluate(() => document.hasFocus());
    }

    async leave(): Promise<void> {
        if (!this.other) {
            this.host = new Host('image');
            this.other = await this.host.managedProbe('it-focus', requireDisplay());
        }
        await this.cell.xclient(['xdotool', 'windowfocus', '--sync', String(this.other.id)]);
        await sleep(400);
    }

    async close(): Promise<void> {
        await this.other?.release();
        await this.host?.close();
    }
}

/** `[size, sha256 or PNG dims]` of what the session's side holds. */
type Read = [number, string | Dims];

interface SessionSide {
    prepare(page: Page, text: Buffer, image: Buffer): Promise<boolean>;
    copy(page: Page, kind: string): Promise<void>;
    read(page: Page, kind: string): Promise<Read>;
    done(): Promise<void>;
}

/** The session's side as an X application: xclip owns and reads the session's clipboard. */
class XclipSession implements SessionSide {
    private readonly cell: Cell;

    constructor(cell: Cell) {
        this.cell = cell;
    }

    async prepare(_page: Page, text: Buffer, image: Buffer): Promise<boolean> {
        return await this.cell.target.put(`${TMP}/clip.txt`, text) && this.cell.target.put(`${TMP}/clip.png`, image);
    }

    async copy(_page: Page, kind: string): Promise<void> {
        const [path, mime] = kind === 'text' ? [`${TMP}/clip.txt`, 'UTF8_STRING'] : [`${TMP}/clip.png`, 'image/png'];
        await this.cell.target.sh(`(setsid xclip -selection clipboard -t ${mime} -i ${path} > /dev/null 2>&1 &)`);
    }

    async read(_page: Page, kind: string): Promise<Read> {
        const mime = kind === 'text' ? 'UTF8_STRING' : 'image/png';
        const out = await this.cell.target.out(`f=${TMP}/clip-read; timeout 20 xclip -selection clipboard -t ${mime} -o > $f `
            + "2>/dev/null; stat -c %s $f; sha256sum < $f | cut -c1-64; head -c 24 $f | od -An -tx1 | tr -d ' \\n'");
        const parts = out.split(/\s+/).filter(Boolean);
        if (parts.length < 2) return [0, null];
        return [Number(parts[0]), kind === 'text' ? parts[1] : dims(Buffer.from(parts[2] ?? '', 'hex'))];
    }

    async done(): Promise<void> {
        await this.cell.target.sh(`rm -f ${TMP}/clip.txt ${TMP}/clip.png ${TMP}/clip-read`);
    }
}

/**
 * The session's side as a page in the session's own Chrome: it copies on a key
 * and reports what a paste brings, which is the Wayland desktop's clipboard as
 * its applications see it.
 */
class PageSession implements SessionSide {
    private readonly cell: Cell;

    constructor(cell: Cell) {
        this.cell = cell;
    }

    async prepare(_page: Page, text: Buffer, image: Buffer): Promise<boolean> {
        const t = this.cell.target;
        await t.put(`${TMP}/site/clip.txt`, text);
        await t.put(`${TMP}/site/clip.png`, image);
        await this.cell.clearReport('clip');
        await sessionBrowser(t, { start: '/clip.html', kiosk: true, profile: 'clip' });
        return (await this.cell.report('clip', 30, (r) => Boolean(r.ready))) !== null;
    }

    async copy(page: Page, kind: string): Promise<void> {
        // The click focuses the session's page first, as a user's would before a key:
        // a key right behind it can reach the session before its window has focus.
        await page.mouse.click(1100, 650);
        await sleep(500);
        await page.keyboard.press(kind === 'text' ? 't' : 'i');
    }

    async read(page: Page, kind: string): Promise<Read> {
        const pasted = (r: Record<string, unknown> | null): unknown[] => (Array.isArray(r?.pasted) ? r.pasted : []);
        const before = pasted(await this.cell.report('clip', 2)).length;
        await page.mouse.click(1100, 650);
        await sleep(500);
        await page.keyboard.press('Control+v');
        const rep = await this.cell.report('clip', 20, (r) => pasted(r).length > before);
        const last = fields(pasted(rep).at(-1));
        if (kind === 'text') {
            const t = fields(last.text);
            return [numberOr(t.size) ?? 0, stringOr(t.sha256) ?? null];
        }
        const i = fields(last.image);
        return [numberOr(i.size) ?? 0, 'w' in i ? [numberOr(i.w) ?? 0, numberOr(i.h) ?? 0] : null];
    }

    async done(): Promise<void> {
        await stopSessionBrowser(this.cell.target, 'chrome', 'clip');
        await this.cell.target.sh(`rm -f ${TMP}/site/clip.txt ${TMP}/site/clip.png`);
    }
}

async function clientOwn(cell: Cell, data: Buffer, mime: string): Promise<void> {
    await cell.xclient(['xclip', '-selection', 'clipboard', '-t', mime, '-i'], data, 30);
}

async function clientRead(cell: Cell, mime: string): Promise<Buffer> {
    return (await cell.xclient(['xclip', '-selection', 'clipboard', '-t', mime, '-o'], undefined, 30)).stdout;
}

async function wait<T>(fn: () => Promise<T>, ok: (got: T) => boolean, timeoutS: number): Promise<T> {
    const until = Date.now() + timeoutS * 1000;
    let got = await fn();
    while (Date.now() < until && !ok(got)) {
        await sleep(1000);
        got = await fn();
    }
    return got;
}

async function upload(page: Page, name: string, image: Buffer): Promise<boolean> {
    await openSidebar(page);
    const picked = await pickClipboardImage(page, 'default', { name, mimeType: 'image/png', buffer: image });
    await page.evaluate(() => { window.postMessage({ type: 'toggleDashboard' }, window.location.origin); });
    await sleep(1000);
    return picked;
}

export async function run(cell: Cell): Promise<void> {
    const R = cell.res;
    const page = await cell.open();
    const real = cell.engine in CLASS;
    const focus = new Focus(cell);
    const tools = Array.isArray(cell.facts.tools) ? cell.facts.tools.map(String) : [];
    const x11 = cell.backend === 'x11' && tools.includes('xclip');
    const session: SessionSide = x11 ? new XclipSession(cell) : new PageSession(cell);
    const tag = `${cell.transport}-${cell.backend}-${cell.engine}-${Math.floor(Date.now() / 1000)}`;
    const text = bigText(tag);
    const image = noisePng(1100, 800, tag.length);
    try {
        if (!await session.prepare(page, text, image)) {
            R.check("the session's copying application is up", false, "neither xclip nor the session's Chrome");
            return;
        }

        // Session -> client: text, then an image, copied by an application of the session's.
        if (real) R.check('the page has real focus after a click', await focus.enter(page), 'document.hasFocus() false');
        await session.copy(page, 'text');
        const kib = (b: Buffer): number => Math.floor(b.length / 1024);
        if (real) {
            let got = await wait(() => clientRead(cell, 'UTF8_STRING'), (b) => b.equals(text), 25);
            if (!got.equals(text)) {
                await page.mouse.click(900, 600);
                got = await wait(() => clientRead(cell, 'UTF8_STRING'), (b) => b.equals(text), 10);
            }
            R.check(`${kib(text)} KiB of text copied in the session lands in the local clipboard`, got.equals(text), `${got.length} bytes locally`);
        } else {
            await page.mouse.click(900, 600);
            const n = await wait(async () => numberOr(await page.evaluate("Math.max(0, ...window.__itWrites.filter(w => w.type.startsWith('text')).map(w => w.size))")) ?? 0,
                (v) => v >= text.length, 25);
            const got: unknown = await page.evaluate('window.__it.clip.map(d => JSON.stringify(d).length)');
            R.check(`${kib(text)} KiB of text copied in the session is written to the local clipboard`, n >= text.length,
                `largest text write ${n}; clipboard updates to the dashboard ${JSON.stringify(got)}`);
        }
        await session.copy(page, 'image');
        if (real) {
            const want = dims(image);
            let got = await wait(() => clientRead(cell, 'image/png'), (b) => sameDims(dims(b), want), 25);
            if (!sameDims(dims(got), want)) {
                await page.mouse.click(900, 600);
                got = await wait(() => clientRead(cell, 'image/png'), (b) => sameDims(dims(b), want), 10);
            }
            R.check(`a ${kib(image)} KiB image copied in the session lands in the local clipboard`, sameDims(dims(got), want),
                `${got.length} bytes, ${JSON.stringify(dims(got))}`);
        }

        // Client -> session: a local application's copy, pasted where the user pastes.
        if (real) {
            const text2 = bigText(`${tag}-up`);
            await clientOwn(cell, text2, 'UTF8_STRING');
            await focus.leave();
            await focus.enter(page);
            if (cell.engine !== 'chromium' && x11) await page.keyboard.press('Control+v');
            const sha = createHash('sha256').update(text2).digest('hex');
            const got = await wait(() => session.read(page, 'text'), (g) => g[0] === text2.length && g[1] === sha, x11 ? 25 : 1);
            R.check(`${kib(text2)} KiB of text copied locally reaches the session's clipboard`, got[0] === text2.length && got[1] === sha,
                `${got[0]} bytes in the session`);
            const image2 = noisePng(1000, 700, tag.length + 1);
            await clientOwn(cell, image2, 'image/png');
            await focus.leave();
            await focus.enter(page);
            if (cell.engine !== 'chromium' && x11) await page.keyboard.press('Control+v');
            const got2 = await wait(() => session.read(page, 'image'), (g) => sameDims(asDims(g[1]), dims(image2)), x11 ? 30 : 1);
            R.check(`a ${kib(image2)} KiB image copied locally reaches the session's clipboard`, sameDims(asDims(got2[1]), dims(image2)),
                `${got2[0]} bytes, ${JSON.stringify(got2[1])}`);
        } else {
            R.skip('local clipboard both ways', 'headless WebKit has no X clipboard to copy from or into');
        }

        // Upload Image: the panel's button and its file chooser.
        const image3 = noisePng(900, 640, tag.length + 2);
        const picked = await upload(page, 'upload.png', image3);
        const got3 = await wait(() => session.read(page, 'image'), (g) => sameDims(asDims(g[1]), dims(image3)), x11 ? 30 : 1);
        R.check("Upload Image puts the picked image on the session's clipboard", picked && sameDims(asDims(got3[1]), dims(image3)),
            `picked=${picked}, ${got3[0]} bytes, ${JSON.stringify(got3[1])} in the session`);

        // One Ctrl+V into an application of the session's has to paste what was just put on the clipboard: the
        // checks above read until it arrives, and a paste that brings it only on a later press passes them.
        const app: SessionSide = session instanceof PageSession ? session : new PageSession(cell);
        if (app === session || await app.prepare(page, text, image)) {
            for (const how of real ? ['upload', 'local'] : ['upload']) {
                const image4 = noisePng(1200, 720 + (how === 'local' ? 16 : 0), tag.length + 3);
                if (how === 'local') {
                    await clientOwn(cell, image4, 'image/png');
                    await focus.leave();
                    await focus.enter(page);
                } else {
                    await upload(page, 'first.png', image4);
                }
                const first = await app.read(page, 'image');
                R.check(`the first Ctrl+V pastes an image ${how === 'local' ? 'copied locally' : 'from Upload Image'} into the session's browser`,
                    sameDims(asDims(first[1]), dims(image4)), `${first[0]} bytes, ${JSON.stringify(first[1])}`);
            }
            if (app !== session) await app.done();
        } else {
            R.check("the session's browser is up for a paste", false, "the session's Chrome did not report ready");
        }
        await session.done();
    } finally {
        await focus.close();
    }
}
