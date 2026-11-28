/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The host IME's composition opens at the remote text caret, and a toolkit
// application types as before (Wayland, both transports, and with the
// session's windows placed and framed by the server's own window manager).
//
// A GTK 3 window with one focused entry (tools/gtk_entry_probe.c) runs on the
// session's compositor with GTK's own input method context, which binds
// zwp_text_input_v3, enables it on the focused entry and reports the entry's
// cursor rectangle; its protocol log (WAYLAND_DEBUG) shows that it did. The
// page then types through every path a key takes (letters, shifted keys and
// punctuation, Backspace, characters off the US layout, and an IME
// composition committed through CDP, as Chromium's own IME bridge commits),
// and the entry has to hold exactly that text.
//
// The page places its overlay's caret where the server says the entry's caret
// is. The check goes back the other way: the pointer moved to the overlay's
// caret has to land, in the probe's own window coordinates, on the caret the
// entry draws. Under the window manager both probes keep their size and float,
// framed by GTK's own shadows, and a first one takes the free corner, so the
// one typed into lies away from the layout's origin and the caret has to be
// moved by where its window and its shadow lie. A page that connects while the caret stands still is told it at
// once, and places its overlay's caret there without a click.
import { spawn, type ChildProcessByStdio } from 'node:child_process';
import { createWriteStream, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import type { Readable } from 'node:stream';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { fields, numberOr, stringOr } from '../../web/selkies-web-core/lib/narrow.ts';
import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host, runtimeDir } from '../lib/host.ts';
import { builtTool } from '../lib/keys.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const WL_SOCKET = 'wayland-1';

/** One line the probe printed. */
interface ProbeLine {
    kind: string;
    text?: string;
    caret?: number[];
    x?: number;
    y?: number;
}

/** The GTK probe on the server's compositor, its lines kept as they come. */
class Probe {
    readonly lines: ProbeLine[] = [];
    readonly protocolLog: string;
    private readonly child: ChildProcessByStdio<null, Readable, Readable>;

    /**
     * @param runtime The server's runtime directory, where its socket is.
     * @param tag Names the protocol log.
     * @param fixed Whether the window keeps its size, which a window manager does not maximize.
     */
    constructor(runtime: string, tag: string, fixed: boolean) {
        this.protocolLog = join(WORKDIR, `gtk_entry_probe-${tag}.wayland.log`);
        const env: Record<string, string> = {};
        for (const key of ['PATH', 'HOME', 'LANG', 'XDG_DATA_DIRS']) {
            const value = process.env[key];
            if (value !== undefined) env[key] = value;
        }
        Object.assign(env, { XDG_RUNTIME_DIR: runtime, WAYLAND_DISPLAY: WL_SOCKET, GDK_BACKEND: 'wayland', WAYLAND_DEBUG: '1', NO_AT_BRIDGE: '1' });
        this.child = spawn(builtTool('gtk_entry_probe', ['gtk+-3.0']), fixed ? ['fixed'] : [], { env, stdio: ['ignore', 'pipe', 'pipe'] });
        this.child.stderr.pipe(createWriteStream(this.protocolLog));
        createInterface({ input: this.child.stdout }).on('line', (line) => {
            let parsed: unknown;
            try {
                parsed = JSON.parse(line);
            } catch {
                return;
            }
            const f = fields(parsed);
            const kind = stringOr(f.kind);
            if (kind === undefined) return;
            this.lines.push({ kind, text: stringOr(f.text), caret: Array.isArray(f.caret) ? f.caret.map(Number) : undefined, x: numberOr(f.x), y: numberOr(f.y) });
        });
    }

    /**
     * The first line from `from` on that `match` takes, waited for.
     * @param match Takes the line wanted.
     * @param timeoutS How long to wait.
     * @param from The first line looked at.
     * @returns The line, or null when none came.
     */
    async wait(match: (line: ProbeLine) => boolean, timeoutS: number, from = 0): Promise<ProbeLine | null> {
        const deadline = Date.now() + timeoutS * 1000;
        while (Date.now() < deadline) {
            const line = this.lines.slice(from).find(match);
            if (line) return line;
            await sleep(50);
        }
        return null;
    }

    /** @returns The entry's last text line. */
    lastText(): ProbeLine | undefined {
        return this.lines.filter((l) => l.kind === 'text').pop();
    }

    stop(): void {
        this.child.kill('SIGTERM');
    }
}

/**
 * Where the page's overlay puts its caret: the left of its line, the line's
 * top, and its height, in CSS pixels.
 * @param page The page.
 * @returns The three, or null without an overlay.
 */
function overlayCaret(page: Page): Promise<number[] | null> {
    return within(page.evaluate((): number[] | null => {
        const el = document.getElementById('overlayInput');
        if (!el) return null;
        const r = el.getBoundingClientRect();
        const cs = getComputedStyle(el);
        // An overlay nothing placed has a `normal` line, which reads as none.
        return [r.left + (parseFloat(cs.paddingLeft) || 0), r.top + (parseFloat(cs.paddingTop) || 0), parseFloat(cs.lineHeight) || 0];
    }));
}

/**
 * A page of the session, its video flowing.
 * @param browser The browser.
 * @param url The server.
 * @param mode Its transport.
 * @returns The page, and whether its video flows.
 */
async function openPage(browser: Browser, url: string, mode: string): Promise<[BrowserContext, Page, boolean]> {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    return [ctx, page, Boolean(mode === 'websockets' ? await waitWsVideo(page) : await waitWrVideo(page))];
}

async function run(res: Checks, host: Host, mode: string, wm: boolean): Promise<void> {
    const label = wm ? `${mode}-wm` : mode;
    const { url, pid } = await host.start({ label, mode, wayland: true, env: wm ? { SELKIES_WM: 'true', SELKIES_WM_MAXIMIZE: 'false' } : {} });
    const decoy = wm ? new Probe(runtimeDir(pid), `${label}-first`, true) : null;
    if (decoy) res.check('a first window is mapped', await decoy.wait((l) => l.kind === 'ready', 20));
    const probe = new Probe(runtimeDir(pid), label, wm);
    const browser = await launchChromium();
    try {
        const [ctx, page, flowing] = await openPage(browser, url, mode);
        res.check('video flowing', flowing);
        res.check('the GTK entry has the keyboard focus', await probe.wait((l) => l.kind === 'ready', 20), probe.lines.slice(-2));
        // Not a click, which could land on another window or the desktop where the window
        // manager places the probe.
        await page.focus('#overlayInput');
        await sleep(500);

        await page.keyboard.type('Hello, World!');
        await page.keyboard.press('Backspace');
        await page.keyboard.type('?');
        let want = 'Hello, World?';
        let got = await probe.wait((l) => l.kind === 'text' && l.text === want, 10);
        res.check('letters, shifted keys, punctuation, and Backspace reach the entry', got, probe.lastText()?.text);

        const cdp = await ctx.newCDPSession(page);
        await page.keyboard.type('äß');
        for (const stage of ['ㅎ', '한', '한ㄱ', '한글']) {
            await cdp.send('Input.imeSetComposition', { text: stage, selectionStart: stage.length, selectionEnd: stage.length });
            await sleep(80);
        }
        await cdp.send('Input.insertText', { text: '한글' });
        want = 'Hello, World?äß한글';
        got = await probe.wait((l) => l.kind === 'text' && l.text === want, 10);
        res.check('characters off the layout and an IME commit reach the entry once each', got, probe.lastText()?.text);

        const protocol = readFileSync(probe.protocolLog, 'utf8');
        res.check('GTK enables zwp_text_input_v3 and reports its cursor rectangle',
            /zwp_text_input_v3#?@?\d*\.enable\(\)/.test(protocol) && /zwp_text_input_v3#?@?\d*\.set_cursor_rectangle\(/.test(protocol),
            protocol.split('\n').filter((l) => l.includes('zwp_text_input')).slice(-2));

        await sleep(500);
        const caret = probe.lastText()?.caret ?? [];
        const overlay = await overlayCaret(page);
        if (!overlay) {
            res.check("the overlay's caret sits on the entry's caret", false, 'no overlay');
            return;
        }
        const [ox, oy, line] = overlay;
        const from = probe.lines.length;
        await page.mouse.move(ox + 1, oy + line / 2);
        const at = await probe.wait((l) => l.kind === 'pointer', 5, from);
        const [cx, cy, ch] = caret;
        const near = at !== null && Math.abs((at.x ?? -1e9) - cx) <= 3 && (at.y ?? -1e9) >= cy - 2 && (at.y ?? 1e9) <= cy + ch + 2;
        res.check("the pointer moved to the overlay's caret lands on the entry's caret", near,
            { pointer: at && [at.x, at.y], caret, overlay: overlay.map(Math.round) });
        // Where the window's surface lies: the caret drawn in it and the overlay's, a few
        // pixels apart where the surface sits at the origin, as GTK's line is taller than Pango's.
        if (wm) res.check("the window's surface lies away from the layout's origin", line > 0 && (Math.abs(ox - cx) > 10 || Math.abs(oy - cy) > 10), { caret, overlay: overlay.map(Math.round) });

        await ctx.close();
        const [, late] = await openPage(browser, url, mode);
        await sleep(1000);
        const joined = await overlayCaret(late);
        res.check("a page joining later puts its overlay's caret there too", joined !== null && joined[2] > 0 && Math.abs(joined[0] - ox) <= 1 && Math.abs(joined[1] - oy) <= 1,
            { joined: joined?.map(Math.round), overlay: overlay.map(Math.round) });
    } finally {
        probe.stop();
        decoy?.stop();
        await browser.close();
        await host.stop();
    }
}

for (const which of ['ws', 'wr', 'wm']) {
    test(which, async () => {
        const mode = which === 'wr' ? 'webrtc' : 'websockets';
        const res = new Checks(`wayland-text-caret-${which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await run(res, host, mode, which === 'wm');
        } finally {
            await host.close();
        }
        res.finish();
    });
}
