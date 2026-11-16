/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The dashboard on the transport/backend cells the dashboards suite does not
// reach: the bar layouts (the toolbar and the edge button) on WebRTC and on
// Wayland, and every layout's admin UI gates on WebRTC and Wayland.
//
// wish-webrtc-x11 / wish-ws-wl / wish-webrtc-wl, and the same edge-* cells:
//   The bar renders its chrome and streams; the gamepads panel is shown by
//   default; a server clipboard change that arrives while the Clipboard panel
//   is closed seeds the panel, and text typed into the panel reaches the
//   server's selection (X CLIPBOARD or the compositor's); the settings card's
//   framerate slider and encoder menu each produce a change the server
//   acknowledges in its log, with video still arriving after the encoder
//   restart; the Apps panel opens and, with remote commands off, says so.
// gates-webrtc-x11 / gates-ws-wl / gates-webrtc-wl:
//   ui_show_sidebar=false hides the chrome of every layout;
//   ui_sidebar_show_shortcuts / _webcam / _gamepads=false hide the Shortcuts
//   section, the webcam toggle, and the gamepads section (the pads part of a
//   bar layout's panel) while the microphone and gamepad-input toggles stay.
import { chmodSync, mkdirSync, writeFileSync } from 'node:fs';
import { delimiter, join } from 'node:path';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { barCloseMenus, barGamepadPanel, barMenuText, barOpenMenuItem, CHROME, openBarSettingsTab } from '../lib/dashboard.ts';
import { wlCopy, wlPaste } from '../lib/desktop.ts';
import { DASHBOARD_DIST, sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host, type Held } from '../lib/host.ts';
import { realConsole, waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const WL_SOCKET = 'wayland-1';
const LAYOUTS = ['default', 'wish', 'edge'] as const;
// Encoders in the order a switch away from the default prefers them: JPEG
// needs no decoder at all.
const ENCODER_PREFERENCE = ['jpeg', 'h264enc', 'h265enc', 'vp8enc', 'vp9enc', 'av1enc'];

// Records the core's clipboard messages (a push that arrives while the panel
// is closed) and the server settings payload the dashboard renders from.
const PAGE_JS = `
  window.__clipMsgs = [];
  window.__serverSettings = null;
  window.addEventListener('message', (e) => {
    if (!e.data || !e.data.type) return;
    if (e.data.type === 'clipboardContentUpdate') window.__clipMsgs.push(e.data);
    if (e.data.type === 'serverSettings') window.__serverSettings = e.data.payload;
  });
`;

declare global {
    interface Window {
        __serverSettings?: { encoder?: { allowed?: string[] } } | null;
    }
}

/** The transport and backend a cell's name ends in. */
function cellParts(cell: string): [string, boolean] {
    const [, transport, backend] = cell.split('-');
    return [transport === 'webrtc' ? 'webrtc' : 'websockets', backend === 'wl'];
}

function waitVideo(page: Page, mode: string, timeoutS = 45): ReturnType<typeof waitWsVideo> {
    return mode === 'webrtc' ? waitWrVideo(page, timeoutS) : waitWsVideo(page, timeoutS);
}

/** Video units the page has taken in: WS chunks or decoded WebRTC frames. */
function videoFrames(page: Page, mode: string): Promise<number> {
    return within(page.evaluate((ws) => {
        if (ws) return window.videoChunksReceived || 0;
        const v = document.querySelector('video');
        return v ? v.getVideoPlaybackQuality().totalVideoFrames : 0;
    }, mode === 'websockets'));
}

/**
 * Video units that arrive past `since`. A still screen sends only what a
 * change or a restart sends, so a restart is counted from before it rather
 * than from after its key frame.
 */
async function newVideoFrames(page: Page, mode: string, timeoutS: number, since: number): Promise<number> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        const now = await videoFrames(page, mode);
        if (now > since) return now - since;
        await sleep(500);
    }
    return 0;
}

/** Whether `needle` shows up in the server log written after `mark`. */
function logSince(host: Host, mark: number, needle: string, timeoutS = 12): Promise<boolean> {
    return host.saw(needle, timeoutS, mark);
}

/** Open the bar's settings card on its Video tab, whose frame rate slider the block drives. */
async function openSettings(page: Page): Promise<boolean> {
    if (!(await openBarSettingsTab(page, 'Video', '#framerateSlider'))) return false;
    for (const until = Date.now() + 6000; Date.now() < until;) {
        if (await page.locator('#framerateSlider').count()) return true;
        await sleep(300);
    }
    return false;
}

/**
 * A PATH dir with a `selkies-proot` that answers `check`, so the apps panel
 * publishes while remote commands stay disabled (the default here).
 */
function appRunnerStubPath(): string {
    const dir = join(WORKDIR, 'dash-cmd-stub');
    mkdirSync(dir, { recursive: true });
    const stub = join(dir, 'selkies-proot');
    writeFileSync(stub, '#!/bin/sh\n[ "$1" = check ] && exit 0\nexit 1\n');
    chmodSync(stub, 0o755);
    return dir;
}

/** The tail of the log after a mark, for a check's detail. */
async function after(host: Host, mark: number): Promise<string> {
    return (await host.log(mark)).slice(-120);
}

/** One bar layout's cell: stream, clipboard panel both ways, settings applied server-side, apps and gamepads panels. */
async function barBlock(res: Checks, host: Host, cell: string): Promise<void> {
    const [mode, wayland] = cellParts(cell);
    const { url } = await host.start({
        label: cell, mode, wayland,
        env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: cell.split('-')[0], PATH: `${appRunnerStubPath()}${delimiter}${process.env.PATH ?? ''}` },
    });
    const browser = await launchChromium();
    try {
        const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        await ctx.addInitScript({ content: PAGE_JS });
        await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: url }).catch(() => undefined);
        const page = await ctx.newPage();
        const consoleErrors: string[] = [];
        page.on('console', (m) => { if (m.type() === 'error') consoleErrors.push(m.text()); });
        page.on('pageerror', (e) => { consoleErrors.push(String(e)); });
        await page.goto(url, { waitUntil: 'load' });

        const info = await waitVideo(page, mode);
        res.check('video streams', info !== null, info);
        // The chrome arrives with the page; the Gamepads panel's pads part
        // follows the server settings payload.
        let card = false;
        for (const until = Date.now() + 10_000; Date.now() < until && !card;) {
            card = (await barGamepadPanel(page)).pads;
            await sleep(500);
        }
        res.check('dashboard chrome renders', (await page.locator(CHROME).count()) > 0);
        res.check('gamepad card shown by default', card);

        const push = `e2e-${cell}-s2c-${Math.floor(Date.now() / 1000)}`;
        let owner: Held | null = null;
        if (wayland) await wlCopy(WL_SOCKET, push);
        else owner = await host.ownClipboard(push);
        let got: (string | undefined)[] = [];
        for (const until = Date.now() + 8000; Date.now() < until;) {
            got = await within(page.evaluate(() => (window.__clipMsgs ?? []).map((m) => m.text)));
            if (got.includes(push)) break;
            await page.waitForTimeout(500);
        }
        await owner?.release();
        res.check('clipboard: server change reached the page while the panel was closed', got.includes(push), JSON.stringify(got).slice(-100));
        const opened = await barOpenMenuItem(page, 'Clipboard');
        let shown = '';
        const area = page.locator('#dashboardClipboardTextarea');
        if (opened) {
            try {
                await area.waitFor({ state: 'visible', timeout: 4000 });
                shown = await area.inputValue();
            } catch (e) {
                shown = `<no textarea: ${String(e)}>`;
            }
        }
        res.check('clipboard: panel opens seeded with that text', opened && shown === push, shown.slice(0, 80));
        const probe = `e2e-${cell}-c2s-${Math.floor(Date.now() / 1000)}`;
        let sent = false;
        if (opened && (await area.count())) {
            // The panel sends on blur; blurring the textarea in place keeps the
            // menu open, whereas a click elsewhere would close it.
            await area.fill(probe);
            await area.evaluate((el) => { if (el instanceof HTMLElement) el.blur(); });
            sent = true;
        }
        let text: string | null = null;
        for (const until = Date.now() + 10_000; Date.now() < until;) {
            text = wayland ? wlPaste(WL_SOCKET).trim() : await host.x11Clipboard(3);
            if (text === probe) break;
            await sleep(500);
        }
        res.check('clipboard: text typed into the panel reached the server', sent && text === probe, JSON.stringify(text).slice(0, 80));
        await barCloseMenus(page);

        res.check('settings panel opens', await openSettings(page));
        let mark = await host.mark();
        const slider = page.locator('#framerateSlider').first();
        await slider.focus();
        await sleep(200);
        for (let i = 0; i < 2; i++) {
            await page.keyboard.press('ArrowLeft');
            await sleep(150);
        }
        const line = mode === 'webrtc' ? 'Updated framerate to' : "Applying video settings for 'primary' live (no restart)";
        res.check('settings: framerate slider change acknowledged by the server', await logSince(host, mark, line), await after(host, mark));

        // The encoder menu renders only when the server offers a choice.
        const allowed = await within(page.evaluate(() => window.__serverSettings?.encoder?.allowed ?? []));
        await openBarSettingsTab(page, 'Video', '#encoderSelect');
        const menu = page.locator('#encoderSelect');
        let picked: string | null = null;
        let seen = 0;
        if (allowed.length <= 1) {
            res.check(`settings: no encoder menu when the server offers one encoder (${JSON.stringify(allowed)})`, (await menu.count()) === 0);
        } else if ((await menu.count()) === 0) {
            res.check('settings: the encoder menu is rendered', false, `allowed=${JSON.stringify(allowed)}`);
        } else {
            const current = await menu.inputValue();
            const offered = await within(page.evaluate(() => [...document.querySelectorAll('#encoderSelect option')]
                .filter((o) => o instanceof HTMLOptionElement && !o.disabled).map((o) => (o instanceof HTMLOptionElement ? o.value : ''))));
            for (const value of ENCODER_PREFERENCE) {
                if (value !== current && offered.includes(value)) {
                    mark = await host.mark();
                    seen = await videoFrames(page, mode);
                    await menu.selectOption(value, { timeout: 3000 });
                    picked = value;
                    break;
                }
            }
            res.check('settings: the encoder menu offers another encoder', picked !== null, `current=${current} offered=${JSON.stringify(offered)}`);
        }
        if (picked) {
            const restart = mode === 'webrtc' ? `encoder -> ${picked}; restarting screen capture`
                : "Video parameters changed for 'primary'. Restarting its capture stream";
            res.check(`settings: encoder change to ${picked} acknowledged by the server`, await logSince(host, mark, restart, 15), await after(host, mark));
            res.check('video still arrives after the encoder restart', (await newVideoFrames(page, mode, 15, seen)) > 0);
        }
        await page.keyboard.press('Escape');
        await sleep(300);

        let appsOpened = false;
        let notice = false;
        if (await barOpenMenuItem(page, 'Apps')) {
            for (const until = Date.now() + 6000; Date.now() < until && !appsOpened;) {
                appsOpened = (await page.locator('[role="dialog"]:has-text("Apps")').count()) > 0;
                await sleep(300);
            }
            const body = appsOpened ? await page.locator('[role="dialog"]').first().innerText() : '';
            notice = body.includes('remote commands are disabled');
        }
        res.check('apps panel opens', appsOpened);
        res.check('apps panel says remote commands are disabled (command_enabled off)', notice);

        const real = realConsole(consoleErrors).errors
            .filter((e) => !['ResizeObserver', 'Error getting media devices', 'Wake Lock'].some((s) => e.includes(s)));
        res.check('no console errors (filtered)', !real.length, real.join('; ').slice(0, 150));
    } finally {
        await browser.close();
    }
}

/** The admin UI gates on one transport/backend, for every layout. */
async function gatesBlock(res: Checks, host: Host, cell: string): Promise<void> {
    const [mode, wayland] = cellParts(cell);
    for (const dashboard of LAYOUTS) {
        let { url } = await host.start({
            label: `${cell}-${dashboard}-hidden`, mode, wayland,
            env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: dashboard, SELKIES_UI_SHOW_SIDEBAR: 'false' },
        });
        let browser = await launchChromium();
        let ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
        await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        let page = await ctx.newPage();
        await page.goto(url, { waitUntil: 'load' });
        res.check(`${dashboard}: video streams with the sidebar hidden`, (await waitVideo(page, mode)) !== null);
        await sleep(2000);
        const chrome = await within(page.evaluate((sel) => document.querySelector(sel) !== null, CHROME));
        res.check(`${dashboard}: ui_show_sidebar=false hides chrome`, !chrome, chrome);
        await browser.close();

        ({ url } = await host.start({
            label: `${cell}-${dashboard}-gates`, mode, wayland,
            env: {
                SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: dashboard,
                SELKIES_UI_SIDEBAR_SHOW_SHORTCUTS: 'false', SELKIES_UI_SIDEBAR_SHOW_WEBCAM: 'false', SELKIES_UI_SIDEBAR_SHOW_GAMEPADS: 'false',
            },
        }));
        browser = await launchChromium();
        ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
        await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        page = await ctx.newPage();
        await page.goto(url, { waitUntil: 'load' });
        res.check(`${dashboard}: video streams`, (await waitVideo(page, mode)) !== null);
        // The gates ride the server settings payload, which follows the stream
        // connection; give it a moment to render.
        await sleep(3000);
        let body: string;
        let mic: boolean;
        let cam: boolean;
        let pad: boolean;
        let section: boolean;
        if (dashboard === 'default') {
            await page.locator('.toggle-handle').first().click().catch(() => undefined);
            await sleep(800);
            body = await within(page.evaluate(() => document.body.innerText));
            [mic, cam, pad] = await within(page.evaluate((): [boolean, boolean, boolean] => [
                document.querySelector('button[title$="Microphone"]') !== null,
                document.querySelector('button[title$="Webcam"]') !== null,
                document.querySelector('button[title$="Gamepad Input"]') !== null]));
            section = (await page.locator('.sidebar-section-header:has-text("Gamepads")').count()) > 0;
        } else {
            body = await within(page.evaluate(() => document.body.innerText));
            const menu = await barMenuText(page);
            body += menu;
            const gamepads = await barGamepadPanel(page);
            mic = menu.includes('Microphone');
            cam = menu.includes('Webcam');
            pad = gamepads.input;
            section = gamepads.pads;
        }
        res.check(`${dashboard}: ui_sidebar_show_shortcuts=false hides the Shortcuts section`, !body.includes('Shortcuts'));
        res.check(`${dashboard}: core buttons reachable (microphone toggle present)`, mic);
        res.check(`${dashboard}: ui_sidebar_show_webcam=false hides the webcam toggle`, !cam);
        res.check(`${dashboard}: ui_sidebar_show_gamepads=false keeps the gamepad input toggle`, pad);
        res.check(`${dashboard}: ui_sidebar_show_gamepads=false hides the gamepads section`, !section);
        await browser.close();
    }
}

const CELLS = ['wish-webrtc-x11', 'wish-ws-wl', 'wish-webrtc-wl', 'edge-webrtc-x11', 'edge-ws-wl', 'edge-webrtc-wl',
    'gates-webrtc-x11', 'gates-ws-wl', 'gates-webrtc-wl'];

for (const cell of CELLS) {
    test(cell, async () => {
        const res = new Checks(cell);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await (cell.startsWith('gates-') ? gatesBlock(res, host, cell) : barBlock(res, host, cell));
        } finally {
            await host.close();
        }
        res.finish();
    });
}
