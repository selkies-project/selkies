/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Formatted content survives the trip, and the switches over it hold.
//
// A copy carrying markup travels with the plain text its source wrote, in
// both directions, so a rich editor on either side keeps the styling and a
// plain field gets the text. The panel's switches are then driven from the
// server: with seamless sync off nothing moves on its own, and with the
// client's chords handed over, Control+Shift+F reaches the session instead of
// the browser.
import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { gestures, pageWithClipboard, readRich, writeRich } from '../lib/clipboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { within } from '../lib/stream.ts';

const HTML = '<b>rich copy</b>';
const PLAIN = 'rich copy';
const BACK_HTML = '<i>from the session</i>';
const BACK_PLAIN = 'from the session';
const XK_F = 0x66;

const dash = (env: Record<string, string> = {}): Record<string, string> => ({ SELKIES_WEB_ROOT: DASHBOARD_DIST, ...env });
const text = (b: Buffer | null): string | null => b?.toString() ?? null;
const isRich = (v: unknown): v is { html: string; text: string | null } => v !== null && typeof v === 'object';

async function block(host: Host, mode: string, res: Checks): Promise<void> {
    const { url } = await host.start({ label: mode, mode, env: dash() });
    const [browser, page] = await pageWithClipboard(url, mode, true);
    let source: Awaited<ReturnType<Host['ownClipboard']>> | null = null;
    try {
        await writeRich(page, HTML, PLAIN);
        const html = await host.x11Convert('text/html');
        const plain = await host.x11Convert('UTF8_STRING');
        res.check('a rich copy reaches the session as markup', text(html) === HTML, text(html));
        res.check('its plain text reaches the session beside the markup', text(plain) === PLAIN, text(plain));

        source = await host.ownClipboard(BACK_PLAIN, { 'text/html': BACK_HTML, 'text/plain': BACK_PLAIN });
        // Two gestures: the write needs a user activation, and the payload
        // has to have arrived before the one that lands it.
        await gestures(page, [300, 300], 2, 2500);
        const local = await readRich(page);
        res.check('a rich copy out of the session reaches the browser as markup', isRich(local) && local.html === BACK_HTML, local);
        res.check('its plain text reaches the browser beside the markup', isRich(local) && local.text === BACK_PLAIN, local);
    } finally {
        await source?.release();
        await browser.close();
    }
}

/** Hold Control+Shift+F, and report what took it: the browser, the session. */
async function chord(host: Host, page: Page): Promise<[unknown, boolean | null]> {
    await page.mouse.move(300, 300);
    await page.mouse.down();
    await page.mouse.up();
    await sleep(1000);
    await page.keyboard.down('Control');
    await page.keyboard.down('Shift');
    await page.keyboard.down('f');
    await sleep(1500);
    const held = await host.keyHeld(XK_F);
    const fullscreen = await within(page.evaluate(() => document.fullscreenElement !== null));
    for (const key of ['f', 'Shift', 'Control']) await page.keyboard.up(key);
    return [fullscreen, held];
}

async function policyBlock(host: Host, res: Checks): Promise<void> {
    const sentinel = 'nothing moved on its own';
    let { url } = await host.start({ label: 'policy-seamless', mode: 'websockets', env: dash({ SELKIES_CLIPBOARD_SEAMLESS: 'false' }) });
    const source = await host.ownClipboard(sentinel, { 'text/plain': sentinel });
    let [browser, page] = await pageWithClipboard(url, 'websockets', true);
    try {
        await writeRich(page, '<b>should stay here</b>', 'should stay here');
        const plain = await host.x11Convert('UTF8_STRING');
        res.check('with seamless sync off a browser copy leaves the session clipboard alone', text(plain) === sentinel, text(plain));
        const local = await readRich(page);
        res.check("and the session's own copy is not written to the browser either", !(isRich(local) && local.text === sentinel), local);
    } finally {
        await source.release();
        await browser.close();
    }

    // Reading markup out of the browser is the same clipboard read images
    // need, so the image switch decides whether a rich copy leaves the
    // browser as one.
    await host.stop();
    ({ url } = await host.start({ label: 'policy-binary', mode: 'websockets', env: dash({ SELKIES_ENABLE_BINARY_CLIPBOARD: 'false' }) }));
    [browser, page] = await pageWithClipboard(url, 'websockets', true);
    try {
        await writeRich(page, '<b>plain only</b>', 'plain only');
        res.check('with the image clipboard off a rich copy still reaches the session as text', text(await host.x11Convert('UTF8_STRING')) === 'plain only', text(await host.x11Convert('UTF8_STRING')));
        res.check('and it carries no markup', (await host.x11Convert('text/html')) === null, text(await host.x11Convert('text/html')));
    } finally {
        await browser.close();
    }

    // The panel writes the choice and the core stores it; a reload has to find it.
    await host.stop();
    ({ url } = await host.start({ label: 'policy-reload', mode: 'websockets', env: dash() }));
    [browser, page] = await pageWithClipboard(url, 'websockets', true);
    try {
        await within(page.evaluate(() => { window.postMessage({ type: 'settings', settings: { keyboard_shortcuts: false } }, window.location.origin); }));
        await sleep(1500);
        await page.reload({ waitUntil: 'load' });
        await sleep(12_000);
        const [fullscreen, held] = await chord(host, page);
        res.check('a chord choice made in the panel survives a reload', fullscreen === false && held === true, `fullscreen=${String(fullscreen)} held in the session=${String(held)}`);
    } finally {
        await browser.close();
    }

    for (const [shortcuts, takesBrowser] of [['true', true], ['false', false]] as const) {
        await host.stop();
        ({ url } = await host.start({ label: `policy-shortcuts-${shortcuts}`, mode: 'websockets', env: dash({ SELKIES_KEYBOARD_SHORTCUTS: shortcuts }) }));
        [browser, page] = await pageWithClipboard(url, 'websockets', true);
        try {
            const [fullscreen, held] = await chord(host, page);
            const where = shortcuts === 'true' ? 'the browser' : 'the session';
            res.check(`with the chords kept ${shortcuts}, Control+Shift+F goes to ${where}`, fullscreen === takesBrowser && held === !takesBrowser,
                `fullscreen=${String(fullscreen)} held in the session=${String(held)}`);
        } finally {
            await browser.close();
        }
    }
}

for (const which of ['websockets', 'webrtc', 'policy']) {
    test(which, async () => {
        const res = new Checks(`cliprich-${which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            if (which === 'policy') await policyBlock(host, res);
            else await block(host, which, res);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
