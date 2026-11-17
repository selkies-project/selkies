/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Which engines the client reads the local clipboard on, and when.
//
// Firefox and WebKit raise a paste prompt on every
// `navigator.clipboard.read()` or `readText()` outside a paste gesture, and
// Firefox's prompt swallows the next clicks and blocks tools that type into
// the page; the only silent read there is the `paste` event's own data.
// Chromium reads on focus once the permission is granted, and on a Ctrl+V
// chord, whose V waits for that send. The async read API is counted on each
// engine across the moments the client syncs the clipboard: load, focus, a
// Ctrl+V chord with nothing on the clipboard, and one with text on it, which
// is a real paste. With the client-to-session direction turned off by the
// server (`enable_clipboard=out`), nothing reads on any engine. A selector
// names one engine, so the registry's per-engine cases each drive a third of
// the matrix.
//
// Uses E2E_DISPLAY when set; otherwise starts a throwaway Xvfb.
import type { BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { FF_E2E_PROFILE, firefoxPersistentContext, launchBrowser, openh264Version, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { WIRE_TAP_JS } from '../lib/page.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

// Counts every entry into the async read API, whoever calls it.
const INSTRUMENT_JS = `(() => {
  window.__clipReads = [];
  const wrap = (name) => {
    if (!navigator.clipboard || typeof navigator.clipboard[name] !== 'function') return;
    const orig = navigator.clipboard[name].bind(navigator.clipboard);
    Object.defineProperty(navigator.clipboard, name, {
      configurable: true,
      value: function(...args) {
        window.__clipReads.push(name);
        return orig(...args);
      }
    });
  };
  wrap('read');
  wrap('readText');
})()`;

const PERMISSIONS: Readonly<Record<Engine, string[]>> = { chromium: ['clipboard-read', 'clipboard-write'], firefox: ['clipboard-read'], webkit: [] };

const js = (page: Page, code: string): Promise<unknown> => within(page.evaluate(code));
const reads = async (page: Page): Promise<number> => {
    const got = await js(page, 'window.__clipReads || []');
    return Array.isArray(got) ? got.length : 0;
};
const sent = async (page: Page): Promise<string[]> => {
    const got = await js(page, 'window.__wireSent || []');
    return Array.isArray(got) ? got.filter((d): d is string => typeof d === 'string') : [];
};

/** What to close and the page; Firefox runs on the persistent profile. */
async function openPage(engine: Engine, url: string, mode: string): Promise<[() => Promise<void>, Page]> {
    let ctx: BrowserContext;
    let close: () => Promise<void>;
    if (engine === 'firefox') {
        ctx = await firefoxPersistentContext({ viewport: { width: 1280, height: 720 } });
        close = () => ctx.close();
    } else {
        const browser = await launchBrowser(engine);
        ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
        close = () => browser.close();
    }
    const perms = PERMISSIONS[engine];
    if (perms.length) await ctx.grantPermissions(perms, { origin: url }).catch(() => undefined);
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: WIRE_TAP_JS });
    await ctx.addInitScript({ content: INSTRUMENT_JS });
    const page = await ctx.newPage();
    await page.goto(`${url}/`, { waitUntil: 'load' });
    return [close, page];
}

interface Counts {
    load: number;
    focus: number;
    chord: number;
    paste: number;
}

/** The moments a sync could read on, each followed by a settle. */
async function provoke(page: Page): Promise<Counts> {
    const load = await reads(page);
    await js(page, "window.dispatchEvent(new Event('blur'))");
    await js(page, "window.dispatchEvent(new Event('focus'))");
    await js(page, "window.dispatchEvent(new Event('focus'))");
    await sleep(1000);
    const focus = (await reads(page)) - load;
    await js(page, "document.getElementById('overlayInput').focus()");
    await page.keyboard.press('Control+v');
    await sleep(1000);
    const chord = (await reads(page)) - load - focus;
    // A real paste: the text is put on the clipboard with a write, which
    // raises no prompt, and the chord makes the browser fire the paste event
    // with that data on it.
    await js(page, "navigator.clipboard.writeText('pasted text').catch(() => {})");
    await sleep(500);
    await js(page, "document.getElementById('overlayInput').focus()");
    await js(page, 'window.__pasteMark = (window.__wireSent || []).length');
    await page.keyboard.press('Control+v');
    await sleep(1500);
    const paste = (await reads(page)) - load - focus - chord;
    return { load, focus, chord, paste };
}

const total = (c: Counts): number => c.load + c.focus + c.chord + c.paste;

async function drive(host: Host, res: Checks, mode: string, engine: Engine, clipboardIn: boolean, display: string | undefined): Promise<void> {
    const tag = `[${mode}/${engine}${clipboardIn ? '' : ', clipboard in off'}]`;
    if (engine === 'firefox' && mode === 'webrtc' && !openh264Version()) {
        res.skip(`${tag} no OpenH264 GMP plugin in ${FF_E2E_PROFILE}`, 'run tests/tools/fetch-openh264.sh to cover H.264 in Firefox');
        return;
    }
    const { url } = await host.start({ label: `${engine}-${mode}${clipboardIn ? '' : '-out'}`, mode, ...(display ? { display } : {}), env: clipboardIn ? {} : { SELKIES_ENABLE_CLIPBOARD: 'out' } });
    try {
        let close: () => Promise<void>;
        let page: Page;
        try {
            [close, page] = await openPage(engine, url, mode);
        } catch (e) {
            res.skip(`${tag} engine unavailable`, String(e).slice(0, 120));
            return;
        }
        try {
            const video = mode === 'webrtc' ? await waitWrVideo(page, 90) : await waitWsVideo(page, 60);
            res.check(`${tag} video flows`, Boolean(video), '');
            const counts = await provoke(page);
            const prompts = engine !== 'chromium';
            if (!clipboardIn) {
                res.check(`${tag} nothing reads the clipboard`, total(counts) === 0, counts);
            } else if (prompts) {
                res.check(`${tag} the async read API is never entered`, total(counts) === 0, counts);
            } else {
                res.check(`${tag} a focus reads once the permission is granted`, counts.focus >= 1, counts);
                res.check(`${tag} a Ctrl+V chord reads the clipboard itself`, counts.chord >= 1 && counts.paste >= 1, counts);
            }
            if (clipboardIn) {
                const writes = (await sent(page)).filter((d) => d.startsWith('cw')).length;
                const policy = await js(page, '[window.clipboard_enabled, document.activeElement && document.activeElement.id]');
                res.check(`${tag} the pasted text reaches the session`, writes > 0, `sent=${writes} policy/active=${JSON.stringify(policy)}`);
                // The content is read after its chord's keydown (by the paste
                // event, or by the keydown's own read in Chromium), so an
                // unheld V would paste before it arrived; and the text a paste
                // event inserts into the stream's input must not be typed as
                // well.
                const mark = Number(await js(page, 'window.__pasteMark || 0'));
                const tail = (await sent(page)).slice(mark);
                const writeAt = tail.findIndex((d) => d.startsWith('cw'));
                const vAt = tail.indexOf('kd,118');
                const typed = tail.filter((d) => /^kd,\d+$/.test(d) && Number(d.slice(3)) > 32 && Number(d.slice(3)) < 127 && d !== 'kd,118');
                res.check(`${tag} one Ctrl+V sends what it pastes ahead of its V and types none of it`, writeAt >= 0 && vAt >= 0 && writeAt < vAt && typed.length === 0,
                    `write at ${writeAt < 0 ? 'None' : writeAt}, V at ${vAt < 0 ? 'None' : vAt}, typed ${JSON.stringify(typed.slice(0, 8))}`);
            }
        } finally {
            await close();
        }
    } finally {
        await host.stop();
    }
}

for (const engine of ['firefox', 'chromium', 'webkit'] as const) {
    test(engine, async () => {
        const res = new Checks('clipboard-reads');
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const xvfb = process.env.E2E_DISPLAY ? null : await host.xvfb();
            try {
                for (const mode of ['websockets', 'webrtc']) await drive(host, res, mode, engine, true, xvfb?.display);
                // With the client-to-session direction off, no engine reads at all.
                await drive(host, res, 'websockets', engine, false, xvfb?.display);
            } finally {
                await xvfb?.release();
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
