/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Dashboard e2e: the dashboard in each of its layouts, the sidebar (default),
// the toolbar (wish), and the edge button (edge), covering the settings loop,
// the translucency knob, mode switching, and the admin UI gates in each; the
// edge button's own fan, dock, and figures strip; and the layout toggle
// stepping through all three.
import { spawnSync } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import type { Browser, BrowserContext, Locator, Page } from '@playwright/test';
import { chromium, test } from '@playwright/test';

import { BROWSER_ARGS, launchBrowser, launchChromium, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import {
    BAR_MENU_TRIGGERS, barCloseMenus, barClosePanel, barGamepadPanel, barMenuText, barOpenMenuItem, barShow, barTool, CHROME,
    closeDialogs, EDGE_BUTTON, EDGE_FAN, EDGE_MORE, edgeCloseFan, edgeFanOpen, edgeFanPage, edgeOpenFan, isEdge,
    openClipboardPanel, openScreenSettings, openVideoSettings, pickClipboardImage, pickRateControl, sidebarClose, sidebarMenuItem,
    sidebarMenuClose, sidebarOpen, sidebarSection, switchOn, waitChunk,
} from '../lib/dashboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const ev = <T>(page: Page, script: string): Promise<T> => within(page.evaluate<T>(script));

/** Post a dashboard message into the page. */
async function post(page: Page, message: Record<string, unknown>): Promise<void> {
    await within(page.evaluate((m) => { window.postMessage(m, window.location.origin); }, message));
}

const sidebarIsOpen = (page: Page): Promise<boolean> => ev<boolean>(page, "!!document.querySelector('.sidebar.is-open')");

/** Open or fold the sidebar through the core's message. */
async function toggleDashboard(page: Page, wait = 800): Promise<void> {
    await post(page, { type: 'toggleDashboard' });
    await sleep(wait);
}

/** An HTTP request to the server; status and body. */
async function curl(url: string, path: string, method = 'GET', data?: unknown): Promise<[number, string]> {
    try {
        const res = await fetch(`${url}${path}`, {
            method,
            ...(data !== undefined ? { body: JSON.stringify(data), headers: { 'Content-Type': 'application/json' } } : {}),
        });
        return [res.status, await res.text()];
    } catch (e) {
        return [0, String(e)];
    }
}

declare global {
    interface Window {
        __clipMsgs?: { text?: string }[];
        __clipImages?: number[];
        __clipRefusals?: string[];
        __gaming?: unknown[];
        stream_stats?: { latest?: { t?: number; server?: unknown; send_ms?: unknown } };
    }
}

interface Overlay {
    figures: number;
    pointer: string;
    t: number | null;
    server: boolean;
    sent: boolean;
}

/**
 * The stats section's switch lays the strip over the stream, which keeps the
 * numbers coming with the sidebar shut, shows the server's figures, takes no
 * pointer input, and goes with the switch.
 */
async function statsOverlayCheck(page: Page, res: Checks): Promise<void> {
    const overlay = (): Promise<Overlay | null> => within(page.evaluate((): Overlay | null => {
        const el = document.querySelector('.stream-strip');
        const latest = window.stream_stats?.latest;
        return el ? {
            figures: el.querySelectorAll('.stream-strip-figure').length,
            pointer: getComputedStyle(el).pointerEvents,
            t: latest?.t ?? null, server: Boolean(latest?.server),
            sent: typeof latest?.send_ms === 'number',
        } : null;
    }));
    try {
        await sidebarSection(page, 'Stats');
        await page.locator('#statsStripToggle').click();
        await sleep(500);
        await sidebarClose(page);
        await sleep(4000);
        const first = await overlay();
        await sleep(2500);
        const later = await overlay();
        res.check("the stats section's switch lays a strip over the stream that takes no pointer input", first !== null && first.figures >= 3 && first.pointer === 'none', first);
        res.check("which keeps the server's figures coming with the sidebar shut, the time to the wire among them", later !== null && later.server && later.sent && later.t !== first?.t, later);
        await sidebarOpen(page);
        await page.locator('#statsStripToggle').click();
        await sleep(500);
        const gone = await overlay();
        await sidebarSection(page, 'Stats', false);
        await sidebarClose(page);
        res.check('and goes with the switch', gone === null, gone);
    } catch (e) {
        res.check('the stats overlay check ran', false, String(e).slice(0, 200));
    }
}

/**
 * A server clipboard change that arrives while the bar's Clipboard panel is
 * closed must show in the textarea when the panel is opened: the panel mounts
 * lazily, so it seeds from the last clipboardContentUpdate.
 */
async function barClipboardSeedCheck(host: Host, page: Page, res: Checks): Promise<void> {
    const push = `e2e-bar-clip-${Math.floor(Date.now() / 1000)}`;
    const owner = await host.ownClipboard(push);
    let got: string[] = [];
    for (const until = Date.now() + 8000; Date.now() < until;) {
        got = await within(page.evaluate(() => (window.__clipMsgs ?? []).map((m) => m.text ?? '')));
        if (got.includes(push)) break;
        await page.waitForTimeout(500);
    }
    await owner.release();
    res.check('clipboard event reached the page while the panel was closed', got.includes(push), JSON.stringify(got).slice(-120));
    const opened = await barOpenMenuItem(page, 'Clipboard');
    let shown = '';
    if (opened) {
        try {
            const area = page.locator('#dashboardClipboardTextarea');
            await area.waitFor({ state: 'visible', timeout: 4000 });
            shown = await area.inputValue();
        } catch (e) {
            shown = `<no textarea: ${String(e)}>`;
        }
    }
    res.check('clipboard panel opens with the text that arrived while closed', opened && shown === push, shown.slice(0, 80));
    await barCloseMenus(page);
}

interface Measured {
    left: number;
    top: number;
    bottom: number;
    last_bottom: number;
    shown_bottom: number;
    viewport: number;
}

/**
 * The detailed stats overlay fits a small window and reaches its last row by
 * scrolling within, and it still drags. At 1280x720 the detailed view is
 * taller than the window below its top, so without a height of its own its
 * last rows sit below the window, where the drag clamp, pinning its top at 0,
 * cannot bring them.
 */
async function barStatsFitCheck(page: Page, res: Checks): Promise<void> {
    const measure = (): Promise<Measured | null> => within(page.evaluate((): Measured | null => {
        const h3 = [...document.querySelectorAll('.stats-overlay h3')].find((h) => h instanceof HTMLElement && h.innerText.trim() === 'Stats');
        const panel = h3?.closest('[class*="rounded"]');
        if (!panel) return null;
        const scroller = [panel, ...panel.querySelectorAll('div')].find((el) => /auto|scroll/.test(getComputedStyle(el).overflowY)) ?? panel;
        scroller.scrollTop = scroller.scrollHeight;
        const box = panel.getBoundingClientRect();
        const last = scroller.lastElementChild?.getBoundingClientRect();
        return {
            left: Math.round(box.left), top: Math.round(box.top), bottom: Math.round(box.bottom),
            last_bottom: Math.round(last?.bottom ?? 0), shown_bottom: Math.round(scroller.getBoundingClientRect().bottom),
            viewport: innerHeight,
        };
    }));
    await page.setViewportSize({ width: 1280, height: 720 });
    try {
        await page.mouse.move(640, 5);
        await sleep(600);
        let stats = await barTool(page, '.stats-button');
        if (stats === null) {
            res.check('the bar offers the stats', false, 'no stats button');
            return;
        }
        await stats.evaluate((b) => { if (b instanceof HTMLElement) b.click(); });
        await sleep(500);
        await within(page.evaluate(() => {
            const b = document.querySelector('.stats-overlay svg.lucide-chevron-down')?.closest('button');
            b?.click();
        }));
        await sleep(1000);
        const got = await measure();
        res.check('the detailed stats fit a 720-px window and scroll to their last row', got !== null && got.bottom <= got.viewport && got.last_bottom <= got.shown_bottom, got);
        if (got) {
            await page.mouse.move(got.left + 120, got.top + 14);
            await page.mouse.down();
            await page.mouse.move(got.left + 220, got.top + 4, { steps: 5 });
            // Read while the button is still down: a transition on the panel
            // would draw it behind the place the drag set until the pointer rests.
            const trail = await within(page.evaluate(() => {
                const el = document.querySelector('.stats-overlay');
                return el instanceof HTMLElement ? Math.round(el.getBoundingClientRect().left - parseFloat(el.style.left)) : NaN;
            }));
            await page.mouse.up();
            await sleep(300);
            const moved = await measure();
            res.check('the detailed stats follow the pointer as it moves, not after it rests', Math.abs(trail) <= 1, trail);
            res.check('the detailed stats still drag, and stay in the window', moved !== null && moved.left === got.left + 100 && moved.bottom <= moved.viewport, [got, moved]);
        }
        // The drag pressed outside the edge fan, which shut it.
        stats = await barTool(page, '.stats-button');
        if (stats !== null) await stats.evaluate((b) => { if (b instanceof HTMLElement) b.click(); });
        await sleep(500);
    } finally {
        await page.setViewportSize({ width: 1440, height: 900 });
    }
}

interface Rect {
    left: number;
    top: number;
    right: number;
    bottom: number;
    w: number;
    h: number;
}

/** Where the edge button is docked, before the transform that sinks it into its edge at rest. */
function edgeDock(page: Page): Promise<Rect | null> {
    return within(page.evaluate((): Rect | null => {
        const b = document.querySelector('.edge-button');
        if (!(b instanceof HTMLElement)) return null;
        const left = parseFloat(b.style.left);
        const top = parseFloat(b.style.top);
        return { left, top, right: left + b.offsetWidth, bottom: top + b.offsetHeight, w: innerWidth, h: innerHeight };
    }));
}

/**
 * Drag the edge button with the mouse to (x, y) and let go. The pointer goes
 * to what of the button shows, which brings it out of its edge, and presses
 * the middle of the button once it is out: the button is round, so where the
 * sunk one showed may lie outside it then.
 */
async function edgeDrag(page: Page, x: number, y: number): Promise<void> {
    const view = page.viewportSize() ?? { width: 1440, height: 900 };
    for (let i = 0; i < 2; i++) {
        const box = await page.locator(EDGE_BUTTON).first().boundingBox();
        if (!box) break;
        const cx = Math.min(Math.max(box.x + box.width / 2, 2), view.width - 2);
        const cy = Math.min(Math.max(box.y + box.height / 2, 2), view.height - 2);
        await page.mouse.move(cx, cy);
        await sleep(400);
    }
    await page.mouse.down();
    await page.mouse.move(x, y, { steps: 10 });
    await page.mouse.up();
    await sleep(600);
}

/**
 * The chrome the user drags (the sidebar's handle, the toolbar, the edge
 * button) keeps its place in proportion to the window: dragged to the far
 * edge, it is still on screen in a window that shrank, and back at that edge
 * in one that grew. The edge button is dragged across to the middle of the
 * window's left edge, and read where it docks rather than as it sinks.
 */
async function resizeCheck(page: Page, res: Checks, dashboard: string): Promise<void> {
    const chrome = dashboard === 'default' ? '.toggle-handle' : dashboard === 'edge' ? EDGE_BUTTON : '.toolbar';
    const rect = (): Promise<Rect | null> => (dashboard === 'edge' ? edgeDock(page) : within(page.evaluate((sel): Rect | null => {
        const el = document.querySelector(sel);
        if (!el) return null;
        const b = el.getBoundingClientRect();
        return { left: b.left, top: b.top, right: b.right, bottom: b.bottom, w: innerWidth, h: innerHeight };
    }, chrome)));
    const inside = (r: Rect | null): boolean => r !== null && r.left >= 0 && r.top >= 0 && r.right <= r.w + 1 && r.bottom <= r.h + 1;
    await page.setViewportSize({ width: 1440, height: 900 });
    try {
        await sleep(300);
        if (dashboard === 'edge') {
            await edgeCloseFan(page);
            await edgeDrag(page, 1, 450);
        } else {
            // The handle is dragged by itself, the bar by its grip, the Hand button.
            const grip = await page.locator(dashboard === 'default' ? chrome : '.toolbar button.cursor-grab').first().boundingBox();
            if (grip) {
                const [cx, cy] = [grip.x + grip.width / 2, grip.y + grip.height / 2];
                await page.mouse.move(cx, cy);
                await page.mouse.down();
                await page.mouse.move(cx + (dashboard === 'default' ? 0 : 2000), cy + (dashboard === 'default' ? 2000 : 0), { steps: 8 });
                await page.mouse.up();
                await sleep(300);
            }
        }
        const far = await rect();
        const atEdge = far !== null && (dashboard === 'default' ? Math.abs(far.bottom - far.h) <= 1
            : dashboard === 'edge' ? Math.abs(far.left) <= 1 && (far.top + far.bottom) / 2 / far.h > 0.4 && (far.top + far.bottom) / 2 / far.h < 0.6
                : Math.abs(far.right - far.w) <= 1);
        res.check(`${dashboard}: dragged to the far edge, the chrome stops at it`, atEdge && inside(far), far);
        await page.setViewportSize({ width: 800, height: 500 });
        await sleep(500);
        const small = await rect();
        res.check(`${dashboard}: a window that shrank keeps the chrome on screen`, inside(small), small);
        await page.setViewportSize({ width: 1440, height: 900 });
        await sleep(500);
        const back = await rect();
        res.check(`${dashboard}: a window that grew puts the chrome back in proportion`,
            inside(back) && back !== null && far !== null && Math.abs(back.top - far.top) <= 1 && Math.abs(back.left - far.left) <= 1, [far, back]);
    } finally {
        await page.setViewportSize({ width: 1440, height: 900 });
    }
}

/**
 * A client demoted to viewer by the server gets no sidebar: an open sidebar
 * folds, the handle goes, and Ctrl+Shift+M (the core's own chord, and the
 * toggleDashboard message it posts) opens nothing.
 */
async function viewerCheck(page: Page, res: Checks): Promise<void> {
    // Positive control: the chord opens and closes the sidebar for a controller.
    await page.mouse.click(700, 450);
    await sleep(300);
    await page.keyboard.press('Control+Shift+M');
    await sleep(800);
    const chordOpens = await sidebarIsOpen(page);
    res.check('Ctrl+Shift+M opens the sidebar for a controller', chordOpens, chordOpens);
    if (!chordOpens) {
        await page.locator('.toggle-handle').first().click().catch(() => undefined);
        await sleep(600);
    }
    await post(page, { type: 'clientRoleUpdate', role: 'viewer' });
    await sleep(600);
    const folded = !(await sidebarIsOpen(page));
    const handle = await page.locator('.toggle-handle').count();
    res.check('viewer demotion folds the sidebar and removes the handle', folded && handle === 0, `open=${!folded ? 'True' : 'False'} handle=${handle}`);
    await page.keyboard.press('Control+Shift+M');
    await sleep(600);
    await post(page, { type: 'toggleDashboard' });
    await sleep(600);
    const reopened = await sidebarIsOpen(page);
    res.check('Ctrl+Shift+M does not open the sidebar for a viewer', !reopened, reopened);
}

interface Present {
    gaming: boolean;
    trackpad: boolean;
    headerTrackpad: boolean;
}

/**
 * Gaming mode stays reachable on a touch client, and by its chord. The
 * sidebar header's menu carries the fullscreen and gaming-mode pair on
 * every client; the trackpad switch joins the keyboard button's there (in a bar layout
 * the bar's touch group, or the edge fan's, with no second one in the soft
 * keys), and appears only once touch is seen. The Ctrl+Shift+X chord is the
 * core's own, so it works whatever the dashboard shows.
 */
async function gamingModeCheck(page: Page, res: Checks, dashboard: string): Promise<void> {
    const present = async (): Promise<Present> => {
        if (dashboard === 'default') {
            await sidebarOpen(page);
            const got = {
                gaming: (await sidebarMenuItem(page, '.gaming-mode-button')) !== null,
                trackpad: (await sidebarMenuItem(page, '.trackpad-mode-button')) !== null,
                headerTrackpad: (await page.locator('.header-controls .trackpad-mode-button').count()) > 0,
            };
            await sidebarMenuClose(page);
            return got;
        }
        return {
            gaming: (await barTool(page, '.gaming-mode-button')) !== null,
            trackpad: (await barTool(page, '.trackpad-mode-button')) !== null,
            headerTrackpad: (await page.locator('.fixed.bottom-4 button:has(svg.lucide-touchpad)').count()) > 0,
        };
    };
    const before = await present();
    await ev(page, "window.dispatchEvent(new TouchEvent('touchstart', {bubbles: true}))");
    await sleep(1000);
    const after = await present();
    res.check('gaming mode button shown without touch', before.gaming, before);
    res.check('gaming mode button survives touch detection', after.gaming, after);
    res.check('trackpad button appears with touch, among the modes', after.trackpad, after);
    res.check('the header carries no trackpad button', !after.headerTrackpad, after);

    // requestFullscreen needs a real display; the mode the input handler
    // publishes is what the chord has to move.
    await ev(page, `(() => {
      Element.prototype.requestFullscreen = function () { return Promise.resolve(); };
      Document.prototype.requestFullscreen = function () { return Promise.resolve(); };
      window.__gaming = [];
      window.addEventListener('message', (e) => {
        if (e.data && e.data.type === 'gamingModeUpdate') window.__gaming.push(e.data.active);
      });
    })()`);
    const gaming = (): Promise<boolean> => ev<boolean>(page, '!!(window.webrtcInput && window.webrtcInput.gamingMode)');
    await page.keyboard.press('Control+Shift+X');
    await sleep(1000);
    const entered = await gaming();
    await page.keyboard.press('Control+Shift+X');
    await sleep(1000);
    const left = await gaming();
    const posted = await within(page.evaluate(() => window.__gaming ?? []));
    res.check('Ctrl+Shift+X enters gaming mode', entered, posted);
    res.check('Ctrl+Shift+X leaves gaming mode', entered && !left, posted);
}

interface Ink {
    box: number;
    w: number;
    h: number;
}

interface Tiles {
    n: number;
    widths: number[];
    rows: number;
    keyRow: number;
    keyRowIcons: number;
}

const spread = (v: number[]): number => (v.length ? Math.max(...v) - Math.min(...v) : Infinity);

/**
 * Pin the sidebar layout: the header's one menu, holding the modes and the
 * appearance controls, icon parity of its fullscreen and gaming mode items,
 * uniform stream tiles, and a files dialog that follows the theme. Runs after
 * gamingModeCheck: the touchstart it dispatches is what makes the keyboard and
 * trackpad switches render, so the menu's modes are at their fullest four here.
 */
async function sidebarLayoutCheck(page: Page, res: Checks): Promise<void> {
    if (!(await sidebarIsOpen(page))) await toggleDashboard(page);
    await sidebarMenuItem(page, '.fullscreen-button');
    const popup = '.sidebar-menu-popup';
    const items = await page.locator(`${popup} :is(.fullscreen-button, .gaming-mode-button, .keyboard-toggle-button, .trackpad-mode-button)`).count();
    const appearance = await page.locator(`${popup} :is(.layout-toggle, .translucency-button, .theme-toggle)`).count();
    const header = await page.locator('.sidebar-header .header-controls > *').count();
    res.check("the header carries one menu, holding the four modes and the layout, translucency, and theme controls",
        header === 1 && items === 4 && appearance === 3, { header, items, appearance });
    const icons = await within(page.evaluate((): { fs: Ink; gm: Ink } | null => {
        const ink = (svg: SVGSVGElement, strokeUnits: number): Ink => {
            const b = svg.getBBox();
            const scale = svg.clientWidth / svg.viewBox.baseVal.width;
            return { box: svg.clientWidth, w: (b.width + strokeUnits) * scale, h: (b.height + strokeUnits) * scale };
        };
        const fs = document.querySelector('.fullscreen-button svg');
        const gm = document.querySelector('.gaming-mode-button svg');
        if (!(fs instanceof SVGSVGElement) || !(gm instanceof SVGSVGElement)) return null;
        // Both icons are stroked: their ink is the geometry and a stroke's width (2 units).
        return { fs: ink(fs, 2), gm: ink(gm, 2) };
    }));
    const same = icons !== null && icons.fs.box === icons.gm.box && Math.abs(icons.fs.w - icons.gm.w) <= 1 && Math.abs(icons.fs.h - icons.gm.h) <= 1;
    res.check('fullscreen and gaming icons draw at one size', same, icons);
    await sidebarMenuClose(page);

    const tiles = await within(page.evaluate((): Tiles => {
        const box = (sel: string): DOMRect[] => [...document.querySelectorAll(sel)].map((e) => e.getBoundingClientRect());
        const r = box('.sidebar-action-buttons .action-button');
        return {
            n: r.length,
            widths: r.map((x) => Math.round(x.width * 10) / 10),
            rows: new Set(r.map((x) => Math.round(x.top))).size,
            keyRow: document.querySelectorAll('.sidebar-mobile-key-actions .mobile-key-button:not(.key-palette-toggle)').length,
            // The palette's disclosure carries a chevron; every other icon would be an icon button in the key row.
            keyRowIcons: [...document.querySelectorAll('.sidebar-mobile-key-actions svg')].filter((s) => !s.closest('.key-palette-toggle')).length,
        };
    }));
    res.check('the five stream tiles stand in one row at one width', tiles.n === 5 && tiles.rows === 1 && spread(tiles.widths) <= 1, tiles);
    res.check('the key row holds the five soft keys and no icon buttons', tiles.keyRow === 5 && tiles.keyRowIcons === 0, tiles);

    let opened = false;
    try {
        await page.locator('.sidebar-section-header:has-text("Files")').first().click();
        await sleep(600);
        await page.locator('button:has-text("Download Files")').first().click();
        await page.locator('[role="dialog"]:has-text("Files")').first().waitFor({ timeout: 10_000 });
        opened = true;
    } catch {
        // reported below
    }
    if (!opened) {
        res.check('files dialog opens', false, 'no files dialog');
        return;
    }
    const palette = (): Promise<{ bg: string; closeInside: boolean; dark: boolean } | null> => within(page.evaluate(() => {
        const dialog = document.querySelector('[data-slot="dialog-content"]');
        if (!dialog) return null;
        const close = dialog.querySelector('[data-slot="dialog-close"]');
        const d = dialog.getBoundingClientRect();
        const c = close ? close.getBoundingClientRect() : null;
        return { bg: getComputedStyle(dialog).backgroundColor, closeInside: c !== null && c.right <= d.right && c.top >= d.top, dark: document.documentElement.classList.contains('dark') };
    }));
    const before = await palette();
    res.check("the files dialog's close control sits inside the panel", before?.closeInside, before);
    await page.keyboard.press('Escape');
    await sleep(500);
    res.check('Escape closes the files dialog', (await page.locator('[role="dialog"]').count()) === 0);
    // The dialog is modal over the drawer, so the theme is flipped from the
    // header menu between two openings of it.
    const flipTheme = async (): Promise<void> => {
        await (await sidebarMenuItem(page, '.theme-toggle'))?.click();
        await sleep(400);
        await sidebarMenuClose(page);
    };
    await flipTheme();
    let after: Awaited<ReturnType<typeof palette>> = null;
    try {
        await page.locator('button:has-text("Download Files")').first().click();
        await page.locator('[role="dialog"]:has-text("Files")').first().waitFor({ timeout: 10_000 });
        after = await palette();
        await page.keyboard.press('Escape');
        await sleep(500);
    } catch {
        // reported below
    }
    res.check('the files dialog follows a theme flip', before !== null && after !== null && before.dark !== after.dark && before.bg !== after.bg, [before, after]);
    await flipTheme();
    // Leave the sidebar as found: the blocks after this one open it themselves.
    if (await sidebarIsOpen(page)) await toggleDashboard(page, 600);
}

// A 2x2 opaque red PNG: small enough to inline, and the color is what proves
// the preview decoded and drew the picked image rather than merely sizing a box.
const CLIPBOARD_PNG = Buffer.from('89504e470d0a1a0a0000000d4948445200000002000000020802000000fdd49a73'
    + '0000001049444154789c63f8cfc000440c100a001fee03fd8b5f14d40000000049454e44ae426082', 'hex');

/**
 * An image picked through Upload Image reaches the core as a blob, and
 * anything else is refused. The upload button is the only way binary clipboard
 * content leaves the client unasked, and the panel previews what was picked: a
 * preview showing anything but the pixels of the picked file would be
 * rendering something from somewhere else entirely.
 */
async function clipboardImageCheck(page: Page, res: Checks, dashboard: string): Promise<void> {
    await ev(page, `(() => {
      window.__clipImages = [];
      window.__clipRefusals = [];
      window.addEventListener('message', (e) => {
        const d = e.data;
        if (!d || typeof d !== 'object') return;
        if (d.type === 'clipboardImageUpdate') {
          window.__clipImages.push(d.imageBlob ? d.imageBlob.size : 0);
        }
        if (d.type === 'fileUpload' && d.payload && d.payload.status === 'warning') {
          window.__clipRefusals.push(d.payload.fileName || '');
        }
      });
    })()`);
    const images = (): Promise<number[]> => within(page.evaluate(() => window.__clipImages ?? []));
    const wasOpen = await sidebarIsOpen(page);
    if (!(await pickClipboardImage(page, dashboard, { name: 'clip.png', mimeType: 'image/png', buffer: CLIPBOARD_PNG }))) {
        res.skip(`${dashboard}: the clipboard image path`, 'no Upload Image button in the panel');
        return;
    }
    let got = await images();
    res.check(`${dashboard}: a picked image reaches the core whole`, got.length === 1 && got[0] === CLIPBOARD_PNG.length, got);
    if (dashboard !== 'default') {
        // The dialog closed the menu; the panel shows the pick again when reopened.
        await openClipboardPanel(page, dashboard);
        await sleep(400);
    }
    // Nothing points at a URL for the picked file: the preview draws its pixels.
    const urls = await page.locator('img[src^="blob:"], img[src^="data:"]').count();
    const drawn = await within(page.evaluate(() => {
        const canvas = document.querySelector('canvas[role="img"]');
        if (!(canvas instanceof HTMLCanvasElement)) return null;
        const pixel = canvas.getContext('2d')?.getImageData(0, 0, 1, 1).data;
        return { w: canvas.width, h: canvas.height, pixel: pixel ? [...pixel] : [] };
    }));
    res.check(`${dashboard}: the preview draws the picked image itself`, urls === 0 && drawn !== null && drawn.w === 2 && drawn.h === 2
        && drawn.pixel[0] === 255 && drawn.pixel[1] === 0 && drawn.pixel[2] === 0, drawn);
    if (dashboard !== 'default') {
        for (let i = 0; i < 2; i++) {
            await page.keyboard.press('Escape');
            await sleep(200);
        }
    }
    await pickClipboardImage(page, dashboard, { name: 'clip.txt', mimeType: 'text/plain', buffer: Buffer.from('not an image') });
    got = await images();
    const refusals = await within(page.evaluate(() => window.__clipRefusals ?? []));
    res.check(`${dashboard}: anything but an image is refused, not sent`, got.length === 1 && got[0] === CLIPBOARD_PNG.length && refusals.length === 1, [got, refusals]);
    if (dashboard === 'default') {
        // Leave the sidebar as found: the checks after this one open it themselves.
        await page.locator('.sidebar-section-header:has-text("Clipboard")').first().click();
        await sleep(400);
        if (!wasOpen && (await sidebarIsOpen(page))) await toggleDashboard(page, 600);
    } else {
        await page.keyboard.press('Escape');
        await sleep(300);
    }
}

const hidePad = (page: Page): Promise<void> => post(page, { type: 'TOUCH_GAMEPAD_VISIBILITY', payload: { visible: false, targetDivId: 'touch-gamepad-host' } });

/**
 * The touch gamepad is drawn over the whole viewport, so the dashboard's own
 * panels have to sit above it: two controls stacked on the same pixels compete
 * for the same tap, and the one that wins is the one nobody can see. Only the
 * page's own origin drives it: a frame of another posting its setup draws
 * nothing.
 */
async function touchGamepadLayerCheck(page: Page, res: Checks, dashboard: string): Promise<void> {
    await ev(page, `(() => {
      const frame = document.createElement('iframe');
      frame.sandbox = 'allow-scripts';
      frame.style.display = 'none';
      frame.srcdoc = "<script>parent.postMessage({type: 'TOUCH_GAMEPAD_SETUP', payload: " +
        "{targetDivId: 'touch-gamepad-host', visible: true}}, '*');<" + "/script>";
      document.body.appendChild(frame);
    })()`);
    await sleep(1200);
    const drawn = (): Promise<number> => ev<number>(page, "document.querySelectorAll('#universal-touch-gamepad-controls-overlay *').length");
    const foreign = await drawn();
    res.check(`${dashboard}: a frame of another origin cannot set the touch gamepad up`, foreign === 0, foreign);
    await post(page, { type: 'TOUCH_GAMEPAD_SETUP', payload: { targetDivId: 'touch-gamepad-host', visible: true } });
    await sleep(1200);
    const shown = await drawn();
    res.check(`${dashboard}: the touch gamepad draws its controls`, shown > 0, shown);

    let panel = '.sidebar';
    if (dashboard === 'default') {
        if (!(await sidebarIsOpen(page))) await toggleDashboard(page);
    } else {
        // A bar layout's dialogs are portaled to the body rather than into
        // the dashboard root, so they are the half of its chrome that has to
        // clear the pad on their own footing. Its menus sit at the top out of
        // the pad's way; the file manager the Files item opens is the one
        // drawn over it.
        panel = '[data-slot="dialog-overlay"]';
        const opened = await barOpenMenuItem(page, 'Files');
        if (opened) await sleep(1500);
        if (!opened || (await page.locator(panel).count()) === 0) {
            res.skip(`${dashboard}: a panel over the touch gamepad`, 'no panel opened');
            await hidePad(page);
            return;
        }
    }
    // elementsFromPoint reports the whole hit-test stack, so one sample says
    // both that the two overlap there and which of them takes the press.
    const hits = await within(page.evaluate((selector) => {
        const stacks = { contested: [] as string[], padOnly: 0, covers: false };
        const panelEl = document.querySelector(selector);
        const pad = document.getElementById('universal-touch-gamepad-controls-overlay');
        if (!panelEl || !pad) return stacks;
        const pr = panelEl.getBoundingClientRect();
        stacks.covers = pr.left <= 0 && pr.top <= 0 && pr.right >= innerWidth && pr.bottom >= innerHeight;
        for (const el of pad.querySelectorAll('*')) {
            const r = el.getBoundingClientRect();
            if (!r.width || !r.height) continue;
            const stack = document.elementsFromPoint(Math.round(r.left + r.width / 2), Math.round(r.top + r.height / 2));
            const overPanel = stack.findIndex((e) => panelEl.contains(e) || e === panelEl);
            const overPad = stack.findIndex((e) => pad.contains(e));
            if (overPanel >= 0 && overPad >= 0) stacks.contested.push(overPanel < overPad ? 'panel' : 'pad');
            else if (overPad >= 0) stacks.padOnly++;
        }
        return stacks;
    }, panel));
    res.check(`${dashboard}: an open panel takes the taps of the controls under it`, hits.contested.length > 0 && hits.contested.every((w) => w === 'panel'),
        `${JSON.stringify(hits.contested.slice(0, 8))} contested, ${hits.padOnly} clear`);
    if (hits.covers) res.skip(`${dashboard}: controls clear of it still take their own`, 'the panel covers the viewport');
    else res.check(`${dashboard}: controls clear of it still take their own`, hits.padOnly > 0, hits.padOnly);
    await hidePad(page);
    await sleep(500);
    if (dashboard === 'default') {
        // Leave the sidebar as found, like the layout check before it.
        if (await sidebarIsOpen(page)) await toggleDashboard(page, 600);
    } else {
        await closeDialogs(page);
    }
}

/** The chrome each layout draws over the stream, which blurs what lies behind it with the translucency level. */
const GLASS: Record<string, string> = { default: '.sidebar', wish: '.toolbar', edge: EDGE_BUTTON };

interface Prefs {
    level: string | null;
    translucency: string | null;
    strip: string | null;
}

/** The two chrome preferences the layouts keep, as stored, and the level the page carries. */
function barPrefs(page: Page): Promise<Prefs> {
    return within(page.evaluate((): Prefs => {
        const stored = (name: string): string | null => {
            for (const k of Object.keys(localStorage)) if (k.endsWith(name)) return localStorage.getItem(k);
            return null;
        };
        return { level: document.documentElement.dataset.translucency ?? null, translucency: stored('bar_translucency'), strip: stored('bar_strip') };
    }));
}

/**
 * The layout's translucency button: in the footer of the sidebar header's
 * menu, the drawer and the menu opened first; in the toolbar menu's footer,
 * beside the layout and theme toggles, the menu opened first; or a button of
 * the edge fan.
 */
async function translucencyButton(page: Page, dashboard: string): Promise<Locator | null> {
    let found: Locator;
    if (dashboard === 'default') {
        await sidebarOpen(page);
        return sidebarMenuItem(page, '.translucency-button');
    } else if (dashboard === 'wish') {
        found = page.locator('[role="menu"] .translucency-button');
        if (!(await found.count())) {
            const menu = await barTool(page, BAR_MENU_TRIGGERS);
            if (menu === null) return null;
            await menu.click();
            await sleep(600);
        }
    } else {
        return barTool(page, '.translucency-button');
    }
    return (await found.count()) ? found.first() : null;
}

/**
 * The layout's translucency button steps the page through its four levels and
 * back to solid: the page carries the level on its root (data-translucency),
 * the button reads pressed above solid, and the browser keeps each level. It
 * is left on the second level, which a page opened later has to come up at
 * (translucencyKeptCheck).
 */
async function translucencyCheck(page: Page, res: Checks, dashboard: string): Promise<void> {
    const start = await barPrefs(page);
    res.check(`${dashboard}: the page starts solid`, start.level === '0', start);
    const seen: (Prefs & { pressed: string | null })[] = [];
    for (let i = 0; i < 4; i++) {
        const button = await translucencyButton(page, dashboard);
        if (button === null) break;
        await button.click();
        await sleep(400);
        let pressed: string | null;
        try {
            pressed = await button.getAttribute('aria-pressed', { timeout: 2000 });
        } catch {
            pressed = 'gone after the press';
        }
        seen.push({ ...(await barPrefs(page)), pressed });
    }
    const levels = seen.map((s) => s.level);
    res.check(`${dashboard}: the translucency button steps the page through its four levels and back to solid`, JSON.stringify(levels) === JSON.stringify(['1', '2', '3', '0']), levels);
    res.check(`${dashboard}: the button reads pressed above solid, and the browser keeps each level`,
        JSON.stringify(seen.map((s) => s.pressed)) === JSON.stringify(['true', 'true', 'true', 'false'])
        && JSON.stringify(seen.map((s) => s.translucency)) === JSON.stringify(['1', '2', '3', '0']), seen.map((s) => [s.pressed, s.translucency]));
    for (let i = 0; i < 2; i++) {
        const button = await translucencyButton(page, dashboard);
        if (button !== null) {
            await button.click();
            await sleep(400);
        }
    }
    if (dashboard === 'default') {
        await sidebarClose(page);
    } else {
        await barCloseMenus(page);
        if (await isEdge(page)) await edgeCloseFan(page);
    }
}

/** A page opened later in the browser comes up at the level picked, the layout's chrome blurring what lies behind it at that level. */
async function translucencyKeptCheck(page: Page, res: Checks, dashboard: string): Promise<void> {
    const kept = await barPrefs(page);
    const blur = await within(page.evaluate((sel) => {
        const el = document.querySelector(sel);
        return el ? getComputedStyle(el).backdropFilter : '';
    }, GLASS[dashboard]));
    res.check(`${dashboard}: a page opened later comes up at the translucency picked, its chrome blurred to match`, kept.level === '2' && blur.includes('blur(10px)'), [kept, blur]);
}

/** A dashboard server of `layout` in `mode`, its env on top. */
async function dashboardServer(host: Host, label: string, layout: string, mode = 'websockets', env: Record<string, string> = {}): Promise<string> {
    return (await host.start({ label, mode, env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: layout, ...env } })).url;
}

/** A context of `browser` at 1440x900 taking `mode`. */
async function modeContext(browser: Browser, mode: string, options: Parameters<Browser['newContext']>[0] = {}): Promise<BrowserContext> {
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, ...options });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    return ctx;
}

const waitVideo = (page: Page, mode: string, timeoutS?: number): ReturnType<typeof waitWsVideo> =>
    (mode === 'webrtc' ? waitWrVideo(page, timeoutS ?? 60) : waitWsVideo(page, timeoutS ?? 45));

/**
 * One layout's settings loop and mode-switch round trip. The server runs
 * Turbo, so the still test display keeps sending the frames the stats strip
 * times on their way to the wire.
 */
async function dashBlock(host: Host, dashboard: string): Promise<Checks> {
    const res = new Checks(`dash-${dashboard}`);
    const url = await dashboardServer(host, `dash-${dashboard}`, dashboard, 'websockets', { SELKIES_VIDEO_STREAMING_MODE: 'true' });
    // The served document names the tab. Left to the client script, the
    // browser shows the host URL there until the bundle runs.
    const shell = (await curl(url, '/'))[1];
    res.check('the served page names the tab', shell.includes('<title>Selkies</title>'), shell.slice(0, 200));
    const browser = await launchChromium();
    try {
        // hasTouch stands in for a 2-in-1, whose touchscreen sits alongside a
        // keyboard and mouse: the header still has to offer gaming mode there.
        const ctx = await modeContext(browser, 'websockets', { deviceScaleFactor: 1, hasTouch: true });
        // Records the core's clipboard postMessages so a check can tell that a
        // server push arrived before a panel was opened.
        await ctx.addInitScript({ content: `
          window.__clipMsgs = [];
          window.addEventListener('message', (e) => {
            if (e.data && e.data.type === 'clipboardContentUpdate') window.__clipMsgs.push(e.data);
          });
        ` });
        await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: url }).catch(() => undefined);
        const page = await ctx.newPage();
        const consoleErrors: string[] = [];
        page.on('console', (m) => { if (m.type() === 'error') consoleErrors.push(m.text()); });
        page.on('pageerror', (e) => consoleErrors.push(String(e)));
        await page.goto(url, { waitUntil: 'load' });
        await sleep(9000);

        const chrome = await within(page.evaluate((sel) => ({ body: document.body.innerText.length, sidebar: Boolean(document.querySelector(sel)) }), CHROME));
        res.check('dashboard chrome renders', chrome.sidebar || chrome.body > 400, chrome);
        const info = await waitWsVideo(page);
        res.check('video streams', info !== null, info);

        // Positive control for the gates block below.
        const section = dashboard === 'default'
            ? (await page.locator('.sidebar-section-header:has-text("Gamepads")').count()) > 0
            : (await barGamepadPanel(page)).pads;
        res.check('gamepads section shown by default', section, section);
        await translucencyCheck(page, res, dashboard);

        if (dashboard !== 'default') {
            await barClipboardSeedCheck(host, page, res);
            await barStatsFitCheck(page, res);
        }
        await gamingModeCheck(page, res, dashboard);
        if (dashboard === 'default') {
            await sidebarLayoutCheck(page, res);
            await statsOverlayCheck(page, res);
        }
        await touchGamepadLayerCheck(page, res, dashboard);
        await resizeCheck(page, res, dashboard);
        await clipboardImageCheck(page, res, dashboard);

        const st = await host.mark();
        let changed = false;
        try {
            const opened = await openVideoSettings(page, dashboard, '#framerateSlider');
            await sleep(1000);
            if (await page.locator('#framerateSlider').count()) {
                // The slider carries an index into its frame rate stops, and the
                // display's own stop sits right after the listed stop of its
                // rate, so two stops down is a change of rate from whatever the
                // default is.
                await ev(page, `(() => {
                  const el = document.getElementById('framerateSlider');
                  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
                  setter.call(el, String(Number(el.value) > 1 ? Number(el.value) - 2 : 2));
                  el.dispatchEvent(new Event('input', {bubbles: true}));
                  el.dispatchEvent(new Event('change', {bubbles: true}));
                })()`);
                changed = true;
            } else {
                process.stdout.write(`${dashboard}: framerateSlider not found after open: ${opened}\n`);
            }
        } catch (e) {
            process.stdout.write(`${dashboard} slider err: ${String(e)}\n`);
        }
        await sleep(3000);
        const newlog = await host.log(st);
        const applied = newlog.includes('Applying video settings') || newlog.includes('Updated framerate to') || newlog.toLowerCase().includes('framerate');
        res.check('UI framerate change applied server-side', changed && applied, newlog.slice(-160));

        await openVideoSettings(page, dashboard, '#streamModeSelect');
        const sel = page.locator('#streamModeSelect').first();
        if (await sel.count()) await sel.selectOption({ value: 'webrtc' });
        else await post(page, { type: 'mode', mode: 'webrtc' });
        await sleep(1500);
        // The dashboard may call /api/switch; do it directly if not flipped yet.
        const status: unknown = JSON.parse((await curl(url, '/api/status'))[1] || '{}');
        const current: unknown = status !== null && typeof status === 'object' ? Reflect.get(status, 'current_mode') : null;
        if (current !== 'webrtc') {
            const [s, body] = await curl(url, '/api/switch', 'POST', { mode: 'webrtc' });
            res.check('mode switch api', s === 200, body.slice(0, 60));
        }
        await sleep(6000);
        await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'webrtc';" });
        const page2 = await ctx.newPage();
        await page2.goto(url, { waitUntil: 'load' });
        const info2 = await waitWrVideo(page2, 45);
        res.check('video flows in webrtc after dashboard switch', info2 !== null, info2);
        await page2.close();

        const [s, body] = await curl(url, '/api/switch', 'POST', { mode: 'websockets' });
        res.check('mode switch back to websocket api', s === 200, body.slice(0, 60));
        await sleep(4000);
        await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
        const page3 = await ctx.newPage();
        await page3.goto(url, { waitUntil: 'load' });
        const info3 = await waitWsVideo(page3);
        res.check('video flows back in websockets', info3 !== null, info3);
        await translucencyKeptCheck(page3, res, dashboard);
        // On the fresh page (its input context is attached, so the chord is
        // live), and last: the demotion leaves the page without a sidebar.
        if (dashboard === 'default') await viewerCheck(page3, res);
        await page3.close();

        // The signaling handshake is refused (409, 503) while the server the
        // page streams from switches modes, and the page reconnects past it.
        const benign = ['Failed to load resource', 'Unexpected server response:', 'Unexpected response code: 409', 'Unexpected response code: 503',
            'ResizeObserver', 'server shutting down', 'Error getting media devices'];
        const realErrors = consoleErrors.filter((e) => !benign.some((b) => e.includes(b)));
        res.check('no console errors', realErrors.length === 0, realErrors.join('; ').slice(0, 150));
    } finally {
        await browser.close();
        await host.stop();
    }
    return res;
}

/**
 * ui_sidebar_show_shortcuts=false and ui_sidebar_show_webcam=false must hide
 * the shortcuts UI and the webcam toggle in BOTH layouts;
 * ui_sidebar_show_gamepads=false hides the gamepads section (the pads part of
 * a bar layout's panel) and nothing else, so the gamepad input toggle stays;
 * and enable_resize=false offers no resolution to pick on the primary, whose
 * size the server keeps.
 */
async function gatesBlock(host: Host, dashboard: string): Promise<Checks> {
    const res = new Checks(`gates-${dashboard}`);
    let url = await dashboardServer(host, `gates-${dashboard}`, dashboard, 'websockets', { SELKIES_UI_SIDEBAR_SHOW_SHORTCUTS: 'false', SELKIES_UI_SHOW_SIDEBAR: 'false' });
    let browser = await launchChromium();
    try {
        const page = await (await modeContext(browser, 'websockets')).newPage();
        await page.goto(url, { waitUntil: 'load' });
        await sleep(8000);
        const chrome = await within(page.evaluate((sel) => Boolean(document.querySelector(sel)), CHROME));
        res.check('ui_show_sidebar=false hides chrome', !chrome, chrome);
        // The shortcuts gate needs the sidebar enabled, so it gets its own boot.
    } finally {
        await browser.close();
    }
    await host.stop();
    url = await dashboardServer(host, `gates-${dashboard}-2`, dashboard, 'websockets', {
        SELKIES_UI_SIDEBAR_SHOW_SHORTCUTS: 'false', SELKIES_UI_SIDEBAR_SHOW_WEBCAM: 'false',
        SELKIES_UI_SIDEBAR_SHOW_GAMEPADS: 'false', SELKIES_ENABLE_RESIZE: 'false',
    });
    browser = await launchChromium();
    try {
        const page = await (await modeContext(browser, 'websockets')).newPage();
        await page.goto(url, { waitUntil: 'load' });
        await sleep(8000);
        const body = await ev<string>(page, 'document.body.innerText');
        // 'Shortcuts' is a section/menu label in both layouts, and the label
        // text shows up in innerText even when the section exists only as a
        // DOM label, so this catches a section that was merely collapsed.
        const hasShortcuts = body.includes('Shortcuts');
        res.check('ui_sidebar_show_shortcuts=false hides Shortcuts section', !hasShortcuts, hasShortcuts);
        // The webcam gate hides one entry of the core-button group while its
        // siblings stay, so the microphone control doubles as proof that the
        // group itself was reached (sidebar opened / stream menu expanded).
        let mic: boolean;
        let cam: boolean;
        let pad: boolean;
        let section: boolean;
        if (dashboard === 'default') {
            await page.locator('.toggle-handle').first().click().catch(() => undefined);
            await sleep(800);
            [mic, cam, pad] = await within(page.evaluate((): [boolean, boolean, boolean] => [
                Boolean(document.querySelector('button[title$="Microphone"]')),
                Boolean(document.querySelector('button[title$="Webcam"]')),
                Boolean(document.querySelector('button[title$="Gamepad Input"]'))]));
            section = (await page.locator('.sidebar-section-header:has-text("Gamepads")').count()) > 0;
        } else {
            const menu = await barMenuText(page);
            const gamepads = await barGamepadPanel(page);
            [mic, cam, pad] = [menu.includes('Microphone'), menu.includes('Webcam'), gamepads.input];
            section = gamepads.pads;
        }
        res.check('core buttons reachable (microphone toggle present)', mic, mic);
        res.check('ui_sidebar_show_webcam=false hides webcam toggle', !cam, cam);
        res.check('ui_sidebar_show_gamepads=false keeps the gamepad input toggle', pad, pad);
        res.check('ui_sidebar_show_gamepads=false hides the gamepads section', !section, section);
        await openScreenSettings(page, dashboard, 'button:has-text("Scale Locally")');
        const preset = await page.locator('#resolutionPresetSelect').count();
        // The button beside the resolution controls, so an unopened panel cannot pass.
        const scale = await page.locator('button:has-text("Scale Locally")').count();
        res.check('screen settings reachable (scale-locally button present)', scale > 0, scale);
        res.check('enable_resize=false offers no resolution preset on the primary', preset === 0, preset);
    } finally {
        await browser.close();
        await host.stop();
    }
    return res;
}

/**
 * A deployment that configures a resolution turns HiDPI off, and the core has
 * to stream that way. The dashboard resolves the default; the core starts
 * from its own stored value, so without a push it streams pixel-perfect on
 * every load while the toggle reads off. useCssScaling in the settings the
 * core sends is what it is actually applying. Both cores resolve it, so both
 * transports are driven.
 */
async function hidpiDefaultBlock(host: Host, dashboard: string, mode = 'websockets'): Promise<Checks> {
    const res = new Checks(`hidpi-${dashboard}-${mode}`);
    const url = await dashboardServer(host, `hidpi-${dashboard}-${mode}`, dashboard, mode, { SELKIES_MANUAL_WIDTH: '1280', SELKIES_MANUAL_HEIGHT: '800' });
    // The core persists every useCssScaling it applies, so the stored value
    // is what the session is running; the built-in default is the opposite,
    // so a stored "true" can only come from the configuration being honored.
    const applied = `(() => {
      for (let i = 0; i < localStorage.length; i++) {
        const k = localStorage.key(i);
        if (k.endsWith('useCssScaling')) return localStorage.getItem(k);
      }
      return null;
    })()`;
    try {
        const browser = await launchChromium();
        try {
            const page = await (await modeContext(browser, mode, { deviceScaleFactor: 2 })).newPage();
            await page.goto(url, { waitUntil: 'load' });
            await sleep(10_000);
            const first = await ev<string | null>(page, applied);
            res.check('a configured resolution turns HiDPI off in the core', first === 'true', first);
            await page.reload({ waitUntil: 'load' });
            await sleep(10_000);
            const again = await ev<string | null>(page, applied);
            res.check('the reload comes back with HiDPI still off', again === 'true', again);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

// Sends every server candidate the browser learns to a black hole and keeps
// the browser's own from the server, as a network that drops UDP both ways
// leaves the two agents: each session's checks go unanswered until the
// browser gives it up as failed, while the signaling socket, like the page,
// still reaches the server.
const UDP_BLACKHOLE_JS = String.raw`
  (() => {
    const Orig = window.RTCPeerConnection;
    if (!Orig) return;
    const hole = (c) => c.replace(/(candidate:\S+ \d+ \S+ \d+ )\S+( \d+ typ)/, (m, head, tail) => head + '192.0.2.1' + tail);
    const setRemote = Orig.prototype.setRemoteDescription;
    Orig.prototype.setRemoteDescription = function(desc) {
      if (desc && desc.sdp) {
        desc = {type: desc.type, sdp: desc.sdp.split('\r\n').map((l) => l.startsWith('a=candidate:') ? hole(l) : l).join('\r\n')};
      }
      return setRemote.call(this, desc);
    };
    const addCandidate = Orig.prototype.addIceCandidate;
    Orig.prototype.addIceCandidate = function(c) {
      if (c && c.candidate) c = {candidate: hole(c.candidate), sdpMid: c.sdpMid, sdpMLineIndex: c.sdpMLineIndex};
      return addCandidate.call(this, c);
    };
    const handler = Object.getOwnPropertyDescriptor(Orig.prototype, 'onicecandidate');
    Object.defineProperty(Orig.prototype, 'onicecandidate', {
      configurable: true,
      get() { return handler.get.call(this); },
      set(fn) { handler.set.call(this, (e) => (e.candidate && e.candidate.candidate) ? undefined : fn(e)); },
    });
  })();
`;

/**
 * A WebRTC session whose media path fails beside a working signaling socket
 * raises a notice offering WebSockets, whichever engine gives the path up.
 * With dual mode on, its button switches the server and the page comes back
 * streaming over WebSockets; with it off, the notice has no button and says
 * who can switch.
 */
async function transportAdviceBlock(host: Host, dashboard: string, dual: boolean, engine: Engine = 'chromium'): Promise<Checks> {
    const res = new Checks(`transport-advice-${dashboard}-${dual ? 'dual' : 'single'}${engine === 'chromium' ? '' : `-${engine}`}`);
    const url = await dashboardServer(host, `advice-${dashboard}-${dual ? 'dual' : 'single'}-${engine}`, dashboard, 'webrtc', { SELKIES_ENABLE_DUAL_MODE: dual ? 'true' : 'false' });
    const notice = '[data-sonner-toast]:has-text("WebRTC cannot connect")';
    const button = '[data-sonner-toast] button:has-text("Switch to WebSockets")';
    try {
        const browser = engine === 'chromium' ? await launchChromium() : await launchBrowser(engine);
        try {
            const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
            await ctx.addInitScript({ content: UDP_BLACKHOLE_JS });
            const page = await ctx.newPage();
            const t0 = Date.now();
            await page.goto(url, { waitUntil: 'load' });
            let shown = true;
            try {
                await page.waitForSelector(notice, { timeout: 90_000 });
            } catch {
                shown = false;
            }
            res.check('a failing media path raises the WebSockets notice', shown, `after ${((Date.now() - t0) / 1000).toFixed(1)}s`);
            const text = shown ? await page.locator(notice).first().innerText() : '';
            const hasButton = shown && (await page.locator(button).count()) > 0;
            const py = (b: boolean): string => (b ? 'True' : 'False');
            res.check('the notice has the switch exactly where the mode menu would', shown && hasButton === dual, `button=${py(hasButton)} dual=${py(dual)}`);
            res.check('its text says what the reader can do', text.includes('administrator') !== dual, text.replace(/\n/g, ' | ').slice(0, 160));
            if (dual && hasButton) {
                // The switch answers, then the core reloads the page into the new mode.
                await Promise.all([page.waitForNavigation({ waitUntil: 'load', timeout: 30_000 }), page.locator(button).first().click()]);
                const ok = (await waitWsVideo(page, 45)) !== null;
                const mode = await ev<string | null>(page, 'window.__SELKIES_STREAMING_MODE__ || null');
                res.check('its switch brings the page back streaming over WebSockets', ok && mode === 'websockets', `video=${py(ok)} mode=${mode}`);
            }
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

// A preset the local density would never ask for, and the pick its own size
// derives: the shorter side against the 1080 rows 96 DPI is for.
const DPI_PRESET = '2560x1440';
const DPI_PRESET_PICK = 120;

/** The desktop's Xft.dpi, 96 when unset: X's own default, which is what an application reads when nothing overrides it. */
function xftDpi(): number {
    const out = spawnSync('xrdb', ['-query'], { encoding: 'utf8', env: { ...process.env, DISPLAY: process.env.E2E_DISPLAY ?? '' } }).stdout ?? '';
    const line = out.split('\n').find((l) => l.startsWith('Xft.dpi'));
    return line ? Math.trunc(Number(line.split(':', 2)[1])) : 96;
}

/** The desktop's Xft.dpi, polled until it reads `want` or time runs out. */
async function waitXft(want: number, timeoutS = 20): Promise<number> {
    let got = xftDpi();
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until && got !== want;) {
        await sleep(500);
        got = xftDpi();
    }
    return got;
}

/**
 * A resolution picked in the dashboard carries the UI-scaling default with
 * it. The picker and the desktop have to land on the same number: the
 * framebuffer asked for is what the desktop's UI is sized from, the screen
 * showing it says nothing about it, and the dashboard posts its own derived
 * value over the core's whenever the two differ. Driven through the control a
 * user uses, at dpr 1, where the display itself would ask for 96 whatever the
 * resolution is, in either layout; the dpi default mirror audit holds the
 * dashboard's formula to the core's.
 */
async function dpiForResolutionBlock(host: Host, dashboard: string): Promise<Checks> {
    const res = new Checks(`dpi-for-resolution-${dashboard}`);
    const url = await dashboardServer(host, `dpi-${dashboard}`, dashboard);
    try {
        const browser = await launchChromium();
        try {
            const ctx = await browser.newContext({ viewport: { width: 1000, height: 700 }, deviceScaleFactor: 1 });
            const page = await ctx.newPage();
            await page.goto(url, { waitUntil: 'load' });
            res.check('video flowing', Boolean(await waitWsVideo(page, 30)), '');
            await openScreenSettings(page, dashboard, '#uiScalingSelect');
            // The desktop's own DPI is left where the last session put it
            // until something changes it, so the picker is where the starting
            // pick reads: the display's, with no resolution of its own to follow.
            const before = await page.locator('#uiScalingSelect').inputValue();
            res.check("the picker starts on the display's own pick", before === '96', before);
            // A bar layout's tab holds the picker and the presets in two cards, one open at a time.
            if (dashboard !== 'default') await openScreenSettings(page, dashboard, '#resolutionPresetSelect');
            await page.selectOption('#resolutionPresetSelect', DPI_PRESET);
            await sleep(1000);
            const dpi = await waitXft(DPI_PRESET_PICK);
            res.check(`a ${DPI_PRESET} preset scales the desktop to its own pick`, dpi === DPI_PRESET_PICK, `Xft.dpi=${dpi}`);
            if (dashboard !== 'default') await openScreenSettings(page, dashboard, '#uiScalingSelect');
            const shown = await page.locator('#uiScalingSelect').inputValue();
            res.check('and the picker shows the same number', shown === String(DPI_PRESET_PICK), shown);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

const ARROWS = "[...document.querySelectorAll('.screen-placement-overlay button')]";

/**
 * A refused second-display window leaves the placement arrows up. The window
 * is opened from an async continuation, so the click's transient activation
 * may be spent by then and the browser refuses it. The page is given a
 * window.open that refuses every one, which is what a popup blocker does (the
 * headless shell honors a switch for it, a full Chrome in its new headless
 * mode does not): clearing the arrows on a window that never opened is the
 * button appearing to do nothing at all.
 */
async function secondScreenBlock(host: Host, dashboard: string): Promise<Checks> {
    const res = new Checks(`second-screen-${dashboard}`);
    const url = await dashboardServer(host, `second-${dashboard}`, dashboard);
    try {
        const browser = await launchChromium(['--block-new-web-contents']);
        try {
            const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
            await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';window.open = () => null;" });
            const page = await ctx.newPage();
            await page.goto(url, { waitUntil: 'load' });
            await sleep(8000);
            let opened: boolean;
            if (dashboard !== 'default') {
                opened = await barOpenMenuItem(page, 'Add a second screen');
            } else {
                opened = await openScreenSettings(page, dashboard);
                await ev(page, "(() => { const add = [...document.querySelectorAll('button')].find(b => /add screen/i.test(b.textContent)); if (add) add.click(); })()");
            }
            await sleep(2000);
            const shown = await ev<number>(page, `${ARROWS}.length`);
            res.check('the button offers a placement when it cannot choose one', shown > 0, `${shown} arrows, menu reached=${opened ? 'True' : 'False'}`);
            await ev(page, `${ARROWS}[0] && ${ARROWS}[0].click()`);
            await sleep(1500);
            const still = await ev<number>(page, `${ARROWS}.length`);
            res.check('a refused window leaves the arrows up rather than clearing them', still === shown, `${still} of ${shown} arrows`);
            res.check('no second window was opened', ctx.pages().length === 1, `${ctx.pages().length} page(s)`);
            if (dashboard === 'default') await arrowThemeCheck(page, res);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

/**
 * The placement arrows are the theme's filled button in either theme: a stale
 * color here would be the one control on the page that never follows the theme.
 */
async function arrowThemeCheck(page: Page, res: Checks): Promise<void> {
    const seen = new Map<string, string>();
    for (const flip of [false, true]) {
        if (flip) {
            await (await sidebarMenuItem(page, '.theme-toggle'))?.click();
            await sleep(500);
            await sidebarMenuClose(page);
        }
        const theme = await ev<string>(page, "document.documentElement.classList.contains('light') ? 'light' : 'dark'");
        const colors = await within(page.evaluate(() => {
            const arrow = document.querySelector('.screen-placement-overlay button');
            const swatch = document.createElement('div');
            swatch.style.background = 'var(--primary)';
            document.body.appendChild(swatch);
            const out = { arrow: arrow ? getComputedStyle(arrow).backgroundColor : '', token: getComputedStyle(swatch).backgroundColor };
            swatch.remove();
            return out;
        }));
        seen.set(theme, colors.arrow);
        res.check(`placement arrows use the ${theme} theme's button fill`, colors.arrow === colors.token, JSON.stringify(colors));
    }
    res.check('the arrow fill follows a theme flip', seen.size === 2 && new Set(seen.values()).size === 2, JSON.stringify(Object.fromEntries(seen)));
}

/** Whether the server logs a second display client joining. */
async function waitSecondDisplay(host: Host, timeoutS = 15): Promise<boolean> {
    return host.saw('display2', timeoutS);
}

/**
 * With one adjacent screen the button places the display on it, unasked. The
 * arrows exist for every other case; this is the branch that skips them, and
 * it needs a browser that reports two screens. Chromium is given a faked
 * screen list and the window-management permission over CDP (a persistent
 * context, which is where a browser-level grant lands), since the Window
 * Management API answers nothing without it.
 */
async function secondScreenAutoBlock(host: Host, dashboard: string): Promise<Checks> {
    const res = new Checks(`second-screen-auto-${dashboard}`);
    const url = await dashboardServer(host, `second-auto-${dashboard}`, dashboard);
    const profile = mkdtempSync(join(tmpdir(), `selkies-screens-${dashboard}-`));
    try {
        const ctx = await chromium.launchPersistentContext(profile, {
            headless: true, args: [...BROWSER_ARGS, '--screen-info={0,0 1280x1024}{1280,0 1280x1024}'], viewport: { width: 1280, height: 900 },
        });
        try {
            const pages = ctx.pages();
            const page = pages.length ? pages[0] : await ctx.newPage();
            await (await ctx.newCDPSession(page)).send('Browser.grantPermissions', { origin: url, permissions: ['windowManagement'] });
            await page.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
            await page.goto(url, { waitUntil: 'load' });
            await sleep(8000);
            res.check('the browser reports the second screen', (await ev<boolean>(page, 'window.screen.isExtended')) === true);
            const opened: Page[] = [];
            ctx.on('page', (pg) => opened.push(pg));
            // Clicked through the browser, not evaluate(): opening a window
            // needs the transient activation only a real gesture carries.
            if (dashboard !== 'default') {
                await barOpenMenuItem(page, 'Add a second screen');
            } else {
                await openScreenSettings(page, dashboard);
                await page.getByRole('button', { name: /add screen/i }).first().click();
            }
            await sleep(4000);
            const arrows = await ev<number>(page, `${ARROWS}.length`);
            res.check('it places the display without asking which side', arrows === 0 && opened.length === 1, `${arrows} arrows, ${opened.length} window(s)`);
            if (opened.length) {
                await opened[0].waitForLoadState('load');
                res.check('on the side the adjacent screen is', opened[0].url().endsWith('#display2-right'), opened[0].url().slice(-24));
                await sleep(8000);
                const status: unknown = JSON.parse((await curl(url, '/api/status'))[1] || '{}');
                res.check('and the server takes the second display', await waitSecondDisplay(host), String(status !== null && typeof status === 'object' ? Reflect.get(status, 'current_mode') : null));
            }
        } finally {
            await ctx.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

/**
 * Click the raw pointer motion toggle in either layout's screen settings.
 * @returns True when the toggle was found and clicked.
 */
async function clickRawPointerMotion(page: Page, dashboard: string): Promise<boolean> {
    if (!(await openScreenSettings(page, dashboard, '#rawPointerMotionToggle'))) return false;
    const toggle = page.locator('#rawPointerMotionToggle');
    if (!(await toggle.count())) return false;
    await toggle.first().scrollIntoViewIfNeeded();
    await toggle.first().click();
    return true;
}

/**
 * The raw pointer motion toggle reaches the running client and is kept. On
 * this platform the setting resolves on (the server's default, and Linux is
 * not the platform the client turns it off for), so the toggle starts on; a
 * click turns it off in the live Input, the core persists the pick, and a
 * reload comes back off against the server's default of on. Driven on both
 * transports, since each core applies the setting itself.
 */
async function rawPointerMotionBlock(host: Host, dashboard: string, mode = 'websockets'): Promise<Checks> {
    const res = new Checks(`raw-motion-${dashboard}-${mode}`);
    const url = await dashboardServer(host, `raw-${dashboard}-${mode}`, dashboard, mode);
    const state = 'window.webrtcInput ? window.webrtcInput.constructor.rawPointerMotion : null';
    const stored = `(() => {
      for (let i = 0; i < localStorage.length; i++) {
        const k = localStorage.key(i);
        if (k.endsWith('_raw_pointer_motion')) return localStorage.getItem(k);
      }
      return null;
    })()`;
    try {
        const browser = await launchChromium();
        try {
            const page = await (await modeContext(browser, mode)).newPage();
            await page.goto(url, { waitUntil: 'load' });
            await sleep(8000);
            let now = await ev<boolean | null>(page, state);
            res.check("the client starts with raw pointer motion on, the server's default", now === true, now);
            const clicked = await clickRawPointerMotion(page, dashboard);
            res.check('the toggle is offered in the screen settings', clicked, clicked);
            await sleep(1500);
            now = await ev<boolean | null>(page, state);
            res.check('a click turns raw pointer motion off in the running client', now === false, now);
            const kept = await ev<string | null>(page, stored);
            res.check('the core persists the pick', kept === 'false', kept);
            await page.reload({ waitUntil: 'load' });
            await sleep(8000);
            now = await ev<boolean | null>(page, state);
            res.check('a reload keeps the pick over the server default', now === false, now);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}


const streamLines = (log: string): string[] => log.split('\n').filter((l) => l.includes('Stream settings active'));

/**
 * A rate-control pick streams at the quality the dashboard shows. The
 * operator sets a CRF and a bitrate of its own. Picking CRF, then CBR again,
 * the dashboard shows the operator's value for the mode picked, and the stream
 * has to run at it: the server never moves off it, and the capture the switch
 * restarts comes up at it. Over WebRTC the core restates the value the new
 * mode reads after the mode itself, so a value of the core's own would move
 * the stream off what the page shows.
 */
async function rateControlBlock(host: Host, dashboard: string, mode = 'websockets'): Promise<Checks> {
    const res = new Checks(`rate-control-${dashboard}-${mode}`);
    const url = await dashboardServer(host, `rate-${dashboard}-${mode}`, dashboard, mode, { SELKIES_VIDEO_CRF: '30', SELKIES_VIDEO_BITRATE: '4000' });
    const steps = [
        { pick: 'crf', label: 'Video CRF (', shown: '(30)', line: 'CRF: 30', moved: /Updated CRF live: \d+ -> (\d+)/g, value: '30' },
        { pick: 'cbr', label: 'Video Bitrate (', shown: '(4 Mbps)', line: 'CBR 4000', moved: /Updated video bitrate: \d+ -> (\d+)/g, value: '4000' },
    ];
    try {
        const browser = await launchChromium();
        try {
            const page = await (await modeContext(browser, mode)).newPage();
            await page.goto(url, { waitUntil: 'load' });
            await waitVideo(page, mode);
            await sleep(2000);
            for (const step of steps) {
                const mark = await host.mark();
                res.check(`${step.pick}: the menu takes the pick`, await pickRateControl(page, dashboard, step.pick), '');
                await host.saw('Stream settings active', 15, mark);
                await sleep(2000);
                const log = await host.log(mark);
                const lines = streamLines(log);
                const text = await page.locator('label', { hasText: step.label }).first().innerText({ timeout: 3000 });
                res.check(`${step.pick}: the dashboard shows the operator's value`, text.includes(step.shown), text);
                const targets = [...log.matchAll(step.moved)].map((m) => m[1]);
                res.check(`${step.pick}: the server keeps the value the dashboard shows`, targets.every((t) => t === step.value), targets);
                const last = lines.at(-1);
                res.check(`${step.pick}: the restarted capture runs at it`, last?.includes(step.line) ?? false, last ?? 'no stream line');
            }
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

/** Open either layout's video settings, where the rate control and its value show. */
async function pickRateControlOpen(page: Page, dashboard: string): Promise<boolean> {
    try {
        if (!(await page.locator('#rateControlSelect').count())) await openVideoSettings(page, dashboard, '#rateControlSelect');
        return (await page.locator('#rateControlSelect').count()) > 0;
    } catch {
        return false;
    }
}

/**
 * A page with no picks of its own shows what the display streams with. The
 * server keeps a display's settings when its page goes, so a page whose
 * storage was cleared, or one in another browser, sends no picks and streams
 * with the ones an earlier page made; its dashboard has to show those, not the
 * boot values. A page picks CRF in the menu and 30 fps through the dashboards'
 * settings message; then its storage and cookies are cleared, and a page opens
 * in a fresh browser context.
 */
async function settingsFollowBlock(host: Host, dashboard: string, mode = 'websockets'): Promise<Checks> {
    const res = new Checks(`settings-follow-${dashboard}-${mode}`);
    const url = await dashboardServer(host, `follow-${dashboard}-${mode}`, dashboard, mode, { SELKIES_VIDEO_CRF: '30' });
    const shown = async (page: Page, step: string): Promise<void> => {
        await waitVideo(page, mode);
        await sleep(3000);
        if (!(await pickRateControlOpen(page, dashboard))) {
            res.check(`${step}: the video settings open`, false, '');
            return;
        }
        const text = await ev<string>(page, 'document.body.innerText');
        const line = streamLines(await host.log()).at(-1) ?? '';
        const fps = /FPS: (\d+)/.exec(line);
        res.check(`${step}: the dashboard shows the CRF the server streams with`, line.includes('CRF: 30') && text.includes('Video CRF (30)'), line.slice(0, 90));
        res.check(`${step}: the dashboard shows the frame rate the server streams with`, fps !== null && text.includes(`(${fps[1]} FPS)`), fps ? fps[1] : null);
    };
    try {
        const browser = await launchChromium();
        try {
            const ctx = await modeContext(browser, mode);
            const page = await ctx.newPage();
            await page.goto(url, { waitUntil: 'load' });
            await waitVideo(page, mode);
            await sleep(2000);
            res.check('the menu takes a CRF pick', await pickRateControl(page, dashboard, 'crf'), '');
            await post(page, { type: 'settings', settings: { framerate: 30 } });
            await sleep(3000);
            await ev(page, 'localStorage.clear()');
            await ctx.clearCookies();
            await page.reload({ waitUntil: 'load' });
            await shown(page, 'storage cleared');
            const other = await modeContext(browser, mode);
            await page.close();
            const page2 = await other.newPage();
            await page2.goto(url, { waitUntil: 'load' });
            await shown(page2, 'another browser');
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

/** The Turbo and paint-over switches of either layout, opening its video settings first. */
async function paintOverSwitches(page: Page, dashboard: string): Promise<[Locator, Locator]> {
    if (!(await page.locator('#videoStreamingModeToggle').count())) await openVideoSettings(page, dashboard, '#videoStreamingModeToggle');
    return [page.locator('#videoStreamingModeToggle'), page.locator('#usePaintOverQualityToggle')];
}

/** Either layout's CPU encoding threads switch, opening its video settings first. */
async function cpuThreadingSwitch(page: Page, dashboard: string): Promise<Locator> {
    if (!(await page.locator('#cpuEncodeThreadingToggle').count())) await openVideoSettings(page, dashboard, '#cpuEncodeThreadingToggle');
    return page.locator('#cpuEncodeThreadingToggle');
}

/** The threads pixelflux's last "Encoder: software" line names, 0 without one. */
async function encoderThreadsLogged(host: Host): Promise<number> {
    const lines = (await host.log()).split('\n').filter((l) => l.includes('Encoder: software'));
    const found = lines.length ? /(\d+) threads?\./.exec(lines[lines.length - 1]) : null;
    return found ? Number(found[1]) : 0;
}

/**
 * The CPU encoding threads switch shows the server's value while the session
 * encodes in software. A session with software encoding forced shows the
 * switch, on as the server defaults it; turning it off stores the choice, the
 * server rebuilds the capture on the encoder reopened with one thread, and
 * video comes from it, counted from before the switch since a still screen
 * sends the restart's key frame alone. A VP8 session shows it off, the
 * server's default for VP8, and pixelflux codes on one thread; turned on it
 * runs threaded, and a reload keeps the pick. An operator's true keeps VP8
 * threaded, a locked value hides the switch, and a JPEG session, whose stripes
 * it codes one after another, shows it. A page that moves from x264 to VP8
 * sees it go off, the server's value for the new encoder.
 */
async function cpuThreadingBlock(host: Host, dashboard: string): Promise<Checks> {
    const res = new Checks(`cpu-threading-${dashboard}`);
    for (const kase of ['unset', 'locked', 'jpeg', 'vp8', 'vp8-operator', 'to-vp8', 'vp8-webrtc']) {
        let env: Record<string, string> = { SELKIES_ENCODER: 'h264enc', SELKIES_USE_CPU: 'true' };
        if (kase === 'locked') env.SELKIES_CPU_ENCODE_THREADING = 'false|locked';
        else if (kase === 'jpeg') env = { SELKIES_ENCODER: 'jpeg' };
        else if (kase === 'vp8' || kase === 'vp8-operator' || kase === 'vp8-webrtc') {
            env.SELKIES_ENCODER = 'vp8enc';
            if (kase === 'vp8-operator') env.SELKIES_CPU_ENCODE_THREADING = 'true';
        } else if (kase === 'to-vp8') env.SELKIES_ENCODER = 'h264enc,vp8enc';
        const mode = kase === 'vp8-webrtc' ? 'webrtc' : 'websockets';
        const url = await dashboardServer(host, `cpu-${dashboard}-${kase}`, dashboard, mode, env);
        try {
            const browser = await launchBrowser('chromium');
            try {
                const page = await (await modeContext(browser, mode)).newPage();
                await page.goto(url, { waitUntil: 'load' });
                if (kase === 'vp8-webrtc') {
                    res.check(`${kase}: video starts`, Boolean(await waitWrVideo(page, 60)), '');
                    await sleep(2000);
                } else if (kase === 'jpeg') {
                    // JPEG stripes are not counted as video chunks; the page is given time to settle.
                    await sleep(8000);
                } else {
                    res.check(`${kase}: video starts`, (await waitWsVideo(page, 45)) !== null, '');
                    await sleep(2000);
                }
                let sw = await cpuThreadingSwitch(page, dashboard);
                await sleep(1000);
                const one = async (s: Locator): Promise<boolean> => (await s.count()) === 1;
                if (kase === 'locked') {
                    res.check('locked by the operator: the switch is hidden', (await sw.count()) === 0, '');
                } else if (kase === 'jpeg') {
                    res.check('a JPEG session shows it', await one(sw), '');
                } else if (kase === 'vp8-webrtc') {
                    res.check("over WebRTC a VP8 session shows it off too, the server's default", (await one(sw)) && !(await switchOn(sw.first())), '');
                    const t = await encoderThreadsLogged(host);
                    res.check('and pixelflux codes it on one thread', t === 1, t);
                } else if (kase === 'vp8-operator') {
                    res.check("an operator's true keeps VP8 threaded: shown on", (await one(sw)) && (await switchOn(sw.first())), '');
                    const t = await encoderThreadsLogged(host);
                    res.check('and pixelflux codes it threaded', t > 1, t);
                } else if (kase === 'vp8') {
                    res.check("a VP8 session shows the switch off, the server's default", (await one(sw)) && !(await switchOn(sw.first())), '');
                    let t = await encoderThreadsLogged(host);
                    res.check('and pixelflux codes it on one thread', t === 1, t);
                    if (await sw.count()) {
                        await sw.first().click({ timeout: 5000 });
                        await sleep(4000);
                        t = await encoderThreadsLogged(host);
                        res.check('turned on, VP8 runs threaded', t > 1, t);
                        await page.reload({ waitUntil: 'load' });
                        await waitWsVideo(page, 45);
                        await sleep(3000);
                        const again = await cpuThreadingSwitch(page, dashboard);
                        await sleep(1000);
                        res.check('a reload keeps the pick', (await one(again)) && (await switchOn(again.first())), '');
                        t = await encoderThreadsLogged(host);
                        res.check('and the server runs it', t > 1, t);
                    }
                } else if (kase === 'to-vp8') {
                    res.check('x264 first: shown on', (await one(sw)) && (await switchOn(sw.first())), '');
                    await openVideoSettings(page, dashboard, '#encoderSelect');
                    await page.selectOption('#encoderSelect', 'vp8enc');
                    await sleep(6000);
                    sw = await cpuThreadingSwitch(page, dashboard);
                    res.check("moved to VP8, it reads off, the server's value for VP8", (await one(sw)) && !(await switchOn(sw.first())), '');
                    let t = await encoderThreadsLogged(host);
                    res.check('and pixelflux codes VP8 on one thread', t === 1, t);
                    // The page stored the server's off unmarked; sent back after
                    // a reload it would pin x264 to one thread too.
                    await page.reload({ waitUntil: 'load' });
                    await waitWsVideo(page, 45);
                    await sleep(3000);
                    await cpuThreadingSwitch(page, dashboard);
                    await openVideoSettings(page, dashboard, '#encoderSelect');
                    await page.selectOption('#encoderSelect', 'h264enc');
                    await sleep(6000);
                    sw = await cpuThreadingSwitch(page, dashboard);
                    res.check('after a reload, back on x264 it reads on: the echo was no request', (await one(sw)) && (await switchOn(sw.first())), '');
                    t = await encoderThreadsLogged(host);
                    res.check('and x264 runs threaded', t > 1, t);
                } else {
                    res.check('a software session shows the switch', await one(sw), '');
                    if (await sw.count()) {
                        res.check('on, as the server defaults it', await switchOn(sw.first()), '');
                        const restarts = (await host.log()).split("Video parameters changed for 'primary'").length - 1;
                        const before = await ev<number>(page, 'window.videoChunksReceived || 0');
                        await sw.first().click({ timeout: 5000 });
                        await sleep(3000);
                        const storedOff = await ev<boolean>(page, "Object.entries(localStorage).some(([k, v]) => k.endsWith('_cpu_encode_threading') && v === 'false')");
                        res.check('turning it off reads off and is stored off', !(await switchOn(sw.first())) && storedOff, '');
                        const log = await host.log();
                        res.check('the server rebuilds the capture for it', log.split("Video parameters changed for 'primary'").length - 1 > restarts, log.split('\n').slice(-6).join('\n'));
                        const after = await ev<number>(page, 'window.videoChunksReceived || 0');
                        res.check('and video comes from the reopened encoder', after > before, [before, after]);
                    }
                }
            } finally {
                await browser.close();
            }
        } finally {
            await host.stop();
        }
    }
    return res;
}

/**
 * With the session socket on the page (socket_worker=false, or a policy that
 * forbids blob workers), the settings the server sends at connect can arrive
 * before the dashboard's listeners are up, WebKit's first among them; the
 * dashboard still starts from them, so the controls they gate come up enabled.
 */
async function pageSocketBlock(host: Host, dashboard: string, engine: Engine): Promise<Checks> {
    const res = new Checks(`page-socket-${dashboard}-${engine}`);
    const url = await dashboardServer(host, `page-socket-${dashboard}-${engine}`, dashboard);
    try {
        const browser = await launchBrowser(engine);
        try {
            const page = await (await modeContext(browser, 'websockets')).newPage();
            await page.goto(`${url}?socket_worker=false`, { waitUntil: 'load' });
            await waitWsVideo(page, 45);
            await sleep(2000);
            const [turbo] = await paintOverSwitches(page, dashboard);
            res.check('the settings the server sent reach the dashboard', await turbo.isEnabled(), `Turbo switch disabled=${await turbo.getAttribute('disabled')}`);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

/**
 * Paint-over follows Turbo where nothing pins it, at a constant quality.
 * Turbo starts off. A video encoder under Turbo sends every frame and leaves
 * no still screen to clean up, so the dashboard shows paint-over off under
 * Turbo and on without it, as the server defaults it, and sends the server
 * each value it derives that differs from the server's. A pick the user
 * makes, or an operator's value, holds through a Turbo change. Under CBR, the
 * default, a still screen is cleaned up to the paint-over quality whatever the
 * switch says, so the dashboard hides the switch there and shows that quality.
 */
async function paintOverBlock(host: Host, dashboard: string, engine: Engine): Promise<Checks> {
    const res = new Checks(`paint-over-${dashboard}-${engine}`);
    const path = engine === 'firefox' ? '?socket_worker=false' : '';
    for (const operator of [null, 'true']) {
        const who = operator ? 'operator on' : 'unset';
        const env: Record<string, string> = { SELKIES_RATE_CONTROL_MODE: 'crf' };
        if (operator) env.SELKIES_USE_PAINT_OVER_QUALITY = operator;
        const url = await dashboardServer(host, `paint-${dashboard}-${engine}-${operator ?? 'unset'}`, dashboard, 'websockets', env);
        try {
            const browser = await launchBrowser(engine);
            try {
                const ctx = await modeContext(browser, 'websockets');
                // The settings the core sends. Playwright reports the worker
                // socket's frames in Chromium and WebKit but no WebSocket
                // frames at all in Firefox, whose socket therefore stays on the
                // page (socket_worker=false), where its sends are recorded.
                await ctx.addInitScript({ content: `(() => {
                    const send = WebSocket.prototype.send;
                    window.__settingsSent = [];
                    WebSocket.prototype.send = function (data) {
                        if (typeof data === 'string' && data.startsWith('SETTINGS,')) window.__settingsSent.push(data);
                        return send.call(this, data);
                    };
                })();` });
                const page = await ctx.newPage();
                const frames: string[] = [];
                page.on('websocket', (ws) => { ws.on('framesent', (f) => { if (typeof f.payload === 'string') frames.push(f.payload); }); });
                const pushed = async (): Promise<unknown[]> => {
                    // One record or the other: Playwright's own Firefox reports
                    // the frames the page records as well, and counting both
                    // doubles every push.
                    const sent = frames.length ? frames : await ev<string[]>(page, 'window.__settingsSent || []');
                    return sent.filter((m) => m.startsWith('SETTINGS,') && m.includes('use_paint_over_quality'))
                        .map((m) => {
                            const parsed: unknown = JSON.parse(m.slice('SETTINGS,'.length));
                            const value: unknown = parsed !== null && typeof parsed === 'object' ? Reflect.get(parsed, 'use_paint_over_quality') : undefined;
                            return value;
                        });
                };
                const flip = async (sw: Locator): Promise<void> => {
                    await sw.click({ timeout: 5000 });
                    await sleep(2000);
                };
                await page.goto(`${url}${path}`, { waitUntil: 'load' });
                await waitWsVideo(page, 45);
                await sleep(2000);
                const [turbo, paint] = await paintOverSwitches(page, dashboard);
                res.check(`${who}: Turbo starts off`, !(await switchOn(turbo)), '');
                if (operator) {
                    res.check(`${who}: paint-over shows the operator's value`, await switchOn(paint), '');
                    await flip(turbo);
                    res.check(`${who}: Turbo on leaves it on`, await switchOn(paint), '');
                    await flip(turbo);
                    const p = await pushed();
                    res.check(`${who}: and never sends it off`, !p.includes(false), p);
                } else {
                    res.check(`${who}: paint-over defaults on without Turbo`, await switchOn(paint), '');
                    const served = await ev<unknown>(page, 'window.use_paint_over_quality');
                    let p = await pushed();
                    res.check(`${who}: as the server defaults it, so nothing turns it off there`, served === true && !p.includes(false), [served, p]);
                    await flip(turbo);
                    res.check(`${who}: turning Turbo on turns paint-over off`, !(await switchOn(paint)), '');
                    p = await pushed();
                    res.check(`${who}: and sends it off`, p.length > 0 && p[p.length - 1] === false, p);
                    await flip(turbo);
                    res.check(`${who}: turning Turbo off turns it on again`, await switchOn(paint), '');
                    p = await pushed();
                    res.check(`${who}: and sends it on`, p.length > 0 && p[p.length - 1] === true, p);
                    await flip(paint);
                    const mark = (await pushed()).length;
                    await flip(turbo);
                    await flip(turbo);
                    res.check(`${who}: a user's pick holds through a Turbo change`, !(await switchOn(paint)), '');
                    p = await pushed();
                    res.check(`${who}: and nothing sends it on after`, !p.slice(mark).includes(true), p);
                }
            } finally {
                await browser.close();
            }
        } finally {
            await host.stop();
        }
    }
    const url = await dashboardServer(host, `paint-${dashboard}-${engine}-cbr`, dashboard);
    try {
        const browser = await launchBrowser(engine);
        try {
            const page = await (await modeContext(browser, 'websockets')).newPage();
            await page.goto(`${url}${path}`, { waitUntil: 'load' });
            await waitWsVideo(page, 45);
            await sleep(2000);
            const [turbo, paint] = await paintOverSwitches(page, dashboard);
            const quality = page.locator('#videoPaintoverCRFSlider');
            res.check('CBR: the paint-over switch is hidden', (await turbo.count()) === 1 && (await paint.count()) === 0, `switches ${await turbo.count()} ${await paint.count()}`);
            res.check('CBR: the paint-over quality is shown', (await quality.count()) === 1, await quality.count());
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

// The languages the dashboard ships, by the locale a browser would ask for.
const LOCALES = ['en-US', 'es-ES', 'zh-CN', 'hi-IN', 'pt-BR', 'fr-FR', 'ru-RU', 'de-DE', 'tr-TR', 'it-IT',
    'nl-NL', 'ar-SA', 'ko-KR', 'ja-JP', 'vi-VN', 'th-TH', 'fil-PH', 'da-DK', 'zh-TW'];
// What of the open sidebar reaches past its content's right edge, or is cut
// off inside a box: the padding hid the key palette's last column, which a
// wider font then pushed out of the sidebar.
const SPILL = `(() => {
  const bar = document.querySelector('.sidebar.is-open');
  if (!bar) return null;
  const edge = bar.getBoundingClientRect().right - parseFloat(getComputedStyle(bar).paddingRight);
  const out = bar.scrollWidth > bar.clientWidth + 1 ? ['sidebar scrolls sideways'] : [];
  for (const el of bar.querySelectorAll('*')) {
    const r = el.getBoundingClientRect();
    if (!r.width || !r.height) continue;
    const clipped = getComputedStyle(el).overflowX !== 'visible' && el.scrollWidth > el.clientWidth + 1;
    if (r.right > edge + 1 || clipped) out.push(\`\${el.className || el.tagName} \${(el.innerText || '').trim().slice(0, 30)}\`);
  }
  return out.slice(0, 5);
})()`;
const PHONE = { width: 320, height: 568 };
// The sidebar in DejaVu Sans, the default sans of CI's Ubuntu runners and
// wider than most desktop fonts, so a host whose own font is narrower sees
// what a wider one spills.
const WIDE_FONT = ".sidebar, .sidebar * { font-family: 'DejaVu Sans', sans-serif !important; }";
const ALL_LAYOUTS = 'default,wish,edge';

/**
 * The sidebar, every section open and on a touch screen the key palette too,
 * keeps its contents inside it in every language the dashboard ships, on a
 * desktop window, a touch laptop's (the 280-pixel sidebar with the palette
 * open) and a 320-pixel phone, its header at its fullest: every layout
 * allowed, as a server left at its defaults allows them, so the layout toggle
 * is in it. Each bar layout's buttons and its soft keys fit that phone.
 */
async function layoutBlock(host: Host): Promise<Checks> {
    const res = new Checks('dash-layout');
    let url = await dashboardServer(host, 'layout', ALL_LAYOUTS);
    try {
        const browser = await launchChromium();
        try {
            for (const [form, viewport, touch] of [['desktop', { width: 1280, height: 900 }, false], ['touch laptop', { width: 1280, height: 900 }, true], ['phone', PHONE, true]] as const) {
                const spilled: Record<string, unknown> = {};
                for (const locale of LOCALES) {
                    const ctx = await browser.newContext({ viewport, locale, hasTouch: touch, isMobile: touch && viewport === PHONE });
                    await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
                    const page = await ctx.newPage();
                    await page.goto(url, { waitUntil: 'load' });
                    await waitChunk(page, 40);
                    await page.addStyleTag({ content: WIDE_FONT });
                    if (!(await sidebarIsOpen(page))) {
                        await page.locator('.toggle-handle').first().click({ force: true });
                        await sleep(800);
                    }
                    const heads = page.locator('.sidebar-section-header');
                    const n = await heads.count();
                    for (let i = 0; i < n; i++) {
                        if ((await heads.nth(i).getAttribute('aria-expanded')) !== 'true') await heads.nth(i).click({ force: true });
                    }
                    const palette = page.locator('.key-palette-toggle').first();
                    if ((await palette.count()) && (await palette.isVisible())) await palette.click({ force: true });
                    await sleep(600);
                    const found = await ev<string[] | null>(page, SPILL);
                    if (found === null || found.length) spilled[locale] = found;
                    await ctx.close();
                }
                res.check(`default: the sidebar holds its contents in every language (${form})`, Object.keys(spilled).length === 0, spilled);
            }
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    for (const layout of ['wish', 'edge']) {
        url = await dashboardServer(host, `layout-${layout}`, layout);
        try {
            const browser = await launchChromium();
            try {
                const ctx = await browser.newContext({ viewport: PHONE, hasTouch: true, isMobile: true });
                await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
                const page = await ctx.newPage();
                await page.goto(url, { waitUntil: 'load' });
                await waitChunk(page, 40);
                await page.touchscreen.tap(PHONE.width / 2, PHONE.height / 2);
                await sleep(500);
                const keys = await barTool(page, '.soft-keys-button');
                const soft = (): Promise<number> => page.locator('[data-soft-keys]').count();
                res.check(`${layout}: a touch client's soft keys start shut, their button in the bar`, keys !== null && !(await soft()), [keys !== null, await soft()]);
                if (keys !== null) await keys.click();
                await sleep(1000);
                res.check(`${layout}: the button opens the soft keys`, (await soft()) === 1, await soft());
                const reach = await ev<number>(page, "Math.max(...[...document.querySelectorAll('#dashboard-root button')].filter((b) => b.offsetParent).map((b) => b.getBoundingClientRect().right))");
                res.check(`${layout}: the bar and the soft keys fit a 320-pixel phone`, reach <= PHONE.width, reach);
                // The toolbar folds by its own button, the edge button by Ctrl+Shift+M alone.
                if (layout === 'wish') await page.locator('button[aria-label="Hide toolbar"]').first().click();
                else await post(page, { type: 'toggleDashboard' });
                await sleep(800);
                res.check(`${layout}: the soft keys go with the bar when it folds away`, !(await soft()), await soft());
            } finally {
                await browser.close();
            }
        } finally {
            await host.stop();
        }
    }
    return res;
}

interface Slot {
    tool: string | null;
    label: string | null;
    more: boolean;
    inside: boolean;
    over: string | null;
}

/**
 * Each slot of the open edge fan: the tool it holds (its class, or its label
 * where it has none), its label, whether it lies inside the window, and
 * whether a press at its center lands on it or on what covers it.
 */
function edgeSlots(page: Page): Promise<Slot[]> {
    return within(page.evaluate((): Slot[] => [...document.querySelectorAll('[data-edge-fan]')].map((slot) => {
        const b = slot.querySelector('button');
        const r = slot.getBoundingClientRect();
        const label = b ? b.getAttribute('aria-label') : null;
        const top = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        return {
            tool: (b ? [...b.classList].find((c) => /-button$/.test(c)) : undefined) ?? label, label,
            more: b?.classList.contains('fan-more-button') ?? false,
            inside: r.left >= 0 && r.top >= 0 && r.right <= innerWidth && r.bottom <= innerHeight,
            over: !top || slot.contains(top) ? null : `${top.tagName.toLowerCase()}.${String(top.className).trim().split(/\s+/).slice(0, 4).join('.')}`,
        };
    })));
}

/** What the fan offers a touch client of a server left at its defaults, each as edgeSlots names it. */
const EDGE_TOUCH_TOOLS = ['settings-button', 'Menu', 'stats-button', 'fullscreen-button', 'gaming-mode-button',
    'touch-gamepad-button', 'trackpad-mode-button', 'soft-keys-button', 'Gamepads', 'files-button',
    'figures-strip-button', 'translucency-button'];

/**
 * Every page of the open fan from the first, each the list of its slots,
 * turned by the dots button until the fan is back on its first. The dots
 * button is pressed through the page, so a walk reads every page even where
 * something covers it; its slot says what does.
 */
async function edgeWalk(page: Page): Promise<Slot[][]> {
    const pages: Slot[][] = [];
    for (let i = 0; i < 8; i++) {
        await sleep(700);
        pages.push(await edgeSlots(page));
        const more = page.locator(EDGE_MORE);
        if (!(await more.count())) break;
        await more.first().dispatchEvent('click');
        await sleep(300);
        if ((await edgeFanPage(page))[0] === 1) break;
    }
    return pages;
}

/** How many pixels the edge button is sunk out of the window along its first axis, as its transform stands now. */
function edgeSunk(page: Page): Promise<number> {
    return within(page.evaluate(() => {
        const b = document.querySelector('.edge-button');
        return b ? Math.round(Math.abs(new DOMMatrixReadOnly(getComputedStyle(b).transform).m41)) : -1;
    }));
}

/** Tap the edge button on a touch screen, at the middle of what of it is in the window. */
async function edgeTap(page: Page): Promise<void> {
    const box = await page.locator(EDGE_BUTTON).first().boundingBox();
    const view = page.viewportSize();
    if (!box || !view) return;
    const [left, top] = [Math.max(box.x, 0), Math.max(box.y, 0)];
    const [right, bottom] = [Math.min(box.x + box.width, view.width), Math.min(box.y + box.height, view.height)];
    await page.touchscreen.tap((left + right) / 2, (top + bottom) / 2);
    await sleep(600);
}

const fanShut = async (page: Page): Promise<boolean> => !(await edgeFanOpen(page)) && !(await page.locator(EDGE_FAN).count());
const misplaced = (slots: Slot[]): unknown => {
    const bad = slots.filter((s) => !s.inside || s.over).map((s) => [s.tool, s.inside, s.over]);
    return bad.length ? bad : slots.length;
};

/**
 * The button rests sunk into its corner and comes out for a pointer resting
 * there or a keyboard focus; a tap opens the fan inside the window, and a
 * second tap, Escape, and a press outside it each shut it. Escape shuts a
 * dialog a fan button opened, as it does over the other layouts, rather than
 * the fan the dialog covers; a panel a fan button floats opens with the fan
 * shut, and the fan opened again over it leaves every button in reach.
 */
async function edgeFanChecks(page: Page, res: Checks): Promise<void> {
    const rest = await within(page.evaluate(() => {
        const r = document.querySelector('.edge-button')?.getBoundingClientRect();
        return { right: Math.round(r?.right ?? 0), top: Math.round(r?.top ?? 0), w: innerWidth };
    }));
    res.check('at rest the button sinks into the top right corner, partly out of the window', rest.right > rest.w && rest.top < 0, rest);
    res.check('the fan starts shut', await fanShut(page));
    // The pointer thrown into the corner, where what of the button shows is,
    // and left there: the button comes out, and has to stay out under it, not
    // slide back in once the pointer lies outside the round button.
    await page.mouse.move(700, 450);
    await sleep(500);
    await page.mouse.move(rest.w - 1, 1);
    const held: number[] = [];
    for (let i = 0; i < 24; i++) {
        await sleep(250);
        held.push(await edgeSunk(page));
    }
    const out = held.includes(0) ? held.indexOf(0) : held.length;
    res.check('a pointer resting in the corner brings the button out and keeps it out', out < held.length && held.slice(out).every((v) => v === 0), `sunk px every 250 ms: ${JSON.stringify(held)}`);
    await page.mouse.move(700, 450);
    await sleep(500);
    await page.keyboard.press('Shift');
    await ev(page, "document.querySelector('.edge-button').focus()");
    await sleep(600);
    const focused = await within(page.evaluate(() => {
        const b = document.querySelector('.edge-button');
        if (!b) return null;
        const cs = getComputedStyle(b);
        return {
            visible: b.matches(':focus-visible'), glow: b.classList.contains('edge-halo'),
            ring: cs.boxShadow.split(/,(?![^(]*\))/).some((part) => !/^\s*rgba\(0, 0, 0, 0\)/.test(part) && /0px 0px 0px ([3-9]|\d{2})px/.test(part)),
        };
    }));
    const sunk = await edgeSunk(page);
    res.check('a keyboard focus brings the button out of its corner, its glow giving way to its focus ring',
        focused !== null && focused.visible && !focused.glow && focused.ring && sunk === 0, { ...focused, sunk });
    await ev(page, 'document.activeElement && document.activeElement.blur()');
    await sleep(400);
    await page.locator(EDGE_BUTTON).first().click();
    await sleep(700);
    let slots = await edgeSlots(page);
    res.check('a tap opens the fan, every button inside the window and taking its own press', (await edgeFanOpen(page)) && slots.length > 0 && slots.every((s) => s.inside && !s.over), misplaced(slots));
    res.check("a desktop's fan holds every tool on one page", slots.length > 0 && !slots.some((s) => s.more), slots.map((s) => s.tool));
    await page.locator(EDGE_BUTTON).first().click();
    await sleep(500);
    res.check('a second tap shuts it', await fanShut(page));
    await edgeOpenFan(page);
    await page.keyboard.press('Escape');
    await sleep(500);
    res.check('Escape shuts it', await fanShut(page));
    await edgeOpenFan(page);
    await page.mouse.click(700, 500);
    await sleep(500);
    res.check('a press outside it shuts it', await fanShut(page));
    const files = await barTool(page, '.files-button');
    if (files === null) {
        res.check('the fan offers the files', false);
        return;
    }
    await files.click();
    await sleep(1000);
    const shown = await page.locator('[role="dialog"]').count();
    await page.keyboard.press('Escape');
    await sleep(600);
    const left = { dialog: await page.locator('[role="dialog"]').count(), fan: await edgeFanOpen(page) };
    res.check('Escape shuts a dialog the fan opened in one press, as over the other layouts', shown > 0 && !left.dialog, `dialog shown=${shown}, after one Escape ${JSON.stringify(left)}`);
    await closeDialogs(page);
    const pads = await barTool(page, 'button[aria-label="Gamepads"]');
    if (pads === null) {
        res.check('the fan offers the gamepads panel', false);
        return;
    }
    await pads.click();
    await sleep(800);
    const panel = (await page.getByText('Rumble', { exact: true }).count()) > 0;
    res.check('a panel the fan floats opens with the fan shut, so neither lies over the other', panel && (await fanShut(page)), { panel, fan: await edgeFanOpen(page) });
    await edgeOpenFan(page);
    await sleep(400);
    slots = await edgeSlots(page);
    const covered = slots.filter((s) => s.over).map((s) => [s.tool, s.over]);
    res.check("the fan opened again over the panel has every button taking its own press, the panel's toggle among them", slots.length > 0 && covered.length === 0, covered.length ? covered : slots.length);
    await barClosePanel(page, 'button[aria-label="Gamepads"]');
    await edgeCloseFan(page);
}

const latestT = (page: Page): Promise<number | null> => ev<number | null>(page, 'window.stream_stats && window.stream_stats.latest ? window.stream_stats.latest.t : null');

/**
 * The figures strip rides over the stream from the start, its figures
 * moving; its button takes it off and keeps that, and puts it back.
 */
async function edgeStripChecks(page: Page, res: Checks): Promise<void> {
    const strip = '.stream-strip-marks';
    for (const until = Date.now() + 10_000; Date.now() < until && !(await page.locator(strip).count());) await sleep(500);
    const first = await latestT(page);
    await sleep(2500);
    const later = await latestT(page);
    res.check('the figures strip rides over the stream from the start, its figures coming', (await page.locator(strip).count()) === 1 && first !== null && later !== first, [first, later]);
    const button = await barTool(page, '.figures-strip-button');
    if (button === null) {
        res.check("the fan offers the figures strip's switch", false);
        return;
    }
    const on = await button.getAttribute('aria-pressed');
    await button.click();
    await sleep(600);
    const off = { strip: await page.locator(strip).count(), pressed: await button.getAttribute('aria-pressed'), stored: (await barPrefs(page)).strip };
    res.check('its button takes the strip off the stream and keeps that', on === 'true' && off.strip === 0 && off.pressed === 'false' && off.stored === 'false', [on, off]);
    await button.click();
    await sleep(1500);
    res.check('and puts it back', (await page.locator(strip).count()) === 1 && (await barPrefs(page)).strip === 'true', await page.locator(strip).count());
    await edgeCloseFan(page);
}

/** The dock the page keeps, [placement, offset] as stored. */
function edgeStoredDock(page: Page): Promise<(string | null)[]> {
    return within(page.evaluate(() => ['edge_placement', 'edge_offset'].map((name) => {
        for (const k of Object.keys(localStorage)) if (k.endsWith(`_${name}`) || k === name) return localStorage.getItem(k);
        return null;
    })));
}

/**
 * Dragged to a corner, the button docks flush in it and the page keeps the
 * dock; the fan opens from there inside the window; a reload comes back to it.
 * Dragged to the middle of an edge, it docks there, in proportion.
 */
async function edgeDockChecks(page: Page, res: Checks): Promise<void> {
    const view = page.viewportSize() ?? { width: 1440, height: 900 };
    await edgeDrag(page, 4, view.height - 4);
    const dock = await edgeDock(page);
    let stored = await edgeStoredDock(page);
    const inCorner = (d: Rect | null): boolean => d !== null && d.left === 0 && Math.abs(d.bottom - d.h) <= 1;
    res.check('dragged to the bottom left, the button docks flush in that corner', inCorner(dock), dock);
    const pair = JSON.stringify(stored);
    res.check('and the page keeps that corner', pair === JSON.stringify(['left', '1']) || pair === JSON.stringify(['bottom', '0']), stored);
    await page.locator(EDGE_BUTTON).first().click();
    await sleep(700);
    const slots = await edgeSlots(page);
    res.check('a tap after the drag opens the fan from the corner, inside the window', (await edgeFanOpen(page)) && slots.length > 0 && slots.every((s) => s.inside && !s.over), misplaced(slots));
    await edgeCloseFan(page);
    await page.reload({ waitUntil: 'load' });
    await waitWsVideo(page);
    const again = await edgeDock(page);
    res.check('a reload comes back to the corner', inCorner(again), again);
    await edgeDrag(page, view.width - 4, view.height / 2);
    const side = await edgeDock(page);
    stored = await edgeStoredDock(page);
    const middle = side ? (side.top + side.bottom) / 2 / side.h : 0;
    res.check('dragged to the middle of the right edge, it docks there', side !== null && Math.abs(side.right - side.w) <= 1 && middle > 0.45 && middle < 0.55 && stored[0] === 'right', [side, stored]);
    await page.reload({ waitUntil: 'load' });
    await waitWsVideo(page);
    const kept = await edgeDock(page);
    res.check('and a reload keeps it there', kept !== null && side !== null && kept.left === side.left && kept.top === side.top, [side, kept]);
}

/**
 * A touch client has more tools than a phone's fan holds: the fan pages them,
 * each page ending with the dots button and the last turning back to the
 * first, every button of every page inside the window and taking its own
 * press, and the pages together holding every tool once, as many as a
 * desktop's pages hold, the stats laid over the stream covering none of them.
 * A tap of the touch screen opens the fan.
 */
async function edgePagingChecks(browser: Browser, url: string, res: Checks): Promise<void> {
    const found: Record<string, string[]> = {};
    for (const [name, viewport, mobile] of [['phone', { width: 390, height: 844 }, true], ['desktop', { width: 1440, height: 900 }, false]] as const) {
        const ctx = await browser.newContext({ viewport, deviceScaleFactor: 1, hasTouch: true, isMobile: mobile });
        await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
        const page = await ctx.newPage();
        await page.goto(url, { waitUntil: 'load' });
        await waitChunk(page, 40);
        await page.touchscreen.tap(viewport.width / 2, viewport.height / 2);
        await sleep(800);
        await edgeTap(page);
        res.check(`${name}: a tap of the touch screen opens the fan`, await edgeFanOpen(page));
        if (!(await edgeOpenFan(page))) {
            await ctx.close();
            continue;
        }
        const pages = await edgeWalk(page);
        const tools = pages.flatMap((slots) => slots.filter((s) => !s.more).map((s) => s.tool ?? ''));
        found[name] = tools;
        const total = pages.length > 1 ? (await edgeFanPage(page))[1] : 1;
        if (name === 'phone') res.check("phone: the fan pages a touch client's tools", pages.length > 1, pages.map((p) => p.length));
        res.check(`${name}: each page ends with the dots button, the last turning back to the first`,
            pages.length > 1 ? pages.every((slots) => slots.length > 0 && slots[slots.length - 1].more) : true,
            [...pages.map((slots) => [slots.length, slots.length ? slots[slots.length - 1].tool : null]), `of ${total}`]);
        const outside = pages.flatMap((slots, i) => slots.filter((s) => !s.inside).map((s) => [i + 1, s.tool]));
        res.check(`${name}: every button of every page lies inside the window`, outside.length === 0, outside);
        const covered = pages.flatMap((slots, i) => slots.filter((s) => s.over).map((s) => [i + 1, s.tool, s.over]));
        res.check(`${name}: every button of every page takes its own press`, covered.length === 0, covered);
        const set = new Set(tools);
        const missing = EDGE_TOUCH_TOOLS.filter((t) => !set.has(t)).sort();
        const twice = [...new Set(tools.filter((t, i) => tools.indexOf(t) !== i))].sort();
        res.check(`${name}: the pages hold every tool once`, tools.length === set.size && missing.length === 0, `missing ${JSON.stringify(missing)}, twice ${JSON.stringify(twice)}`);
        const stats = await barTool(page, '.stats-button');
        if (stats !== null) {
            await stats.click();
            await sleep(1000);
            await edgeOpenFan(page);
            await sleep(600);
            const over = (await edgeSlots(page)).filter((s) => s.over).map((s) => [s.tool, s.over]);
            res.check(`${name}: with the stats over the stream, every button of the fan takes its own press, the stats' toggle among them`,
                (await page.locator('.stats-overlay').count()) > 0 && over.length === 0, over);
            await barClosePanel(page, '.stats-button');
        }
        await ctx.close();
    }
    if (found.phone && found.desktop) {
        const phone = new Set(found.phone);
        const desktop = new Set(found.desktop);
        const diff = [...new Set([...phone].filter((t) => !desktop.has(t)).concat([...desktop].filter((t) => !phone.has(t))))].sort();
        res.check("a phone's pages hold the same tools as a desktop's", diff.length === 0, diff);
    }
}

/** The edge layout's own: its fan, its figures strip, its dock, and its pages. */
async function edgeBlock(host: Host): Promise<Checks> {
    const res = new Checks('edge');
    const url = await dashboardServer(host, 'edge', 'edge');
    try {
        const browser = await launchChromium();
        try {
            const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
            await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
            const page = await ctx.newPage();
            await page.goto(url, { waitUntil: 'load' });
            res.check('video streams', (await waitWsVideo(page)) !== null);
            for (const [name, run] of [['edge_fan_checks', edgeFanChecks], ['edge_strip_checks', edgeStripChecks], ['edge_dock_checks', edgeDockChecks]] as const) {
                try {
                    await run(page, res);
                } catch (e) {
                    res.check(`${name} ran`, false, String(e).split('\n')[0].slice(0, 200));
                }
            }
            await ctx.close();
            await edgePagingChecks(browser, url, res);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

/** The layout the page shows, told by its chrome. */
async function shownLayout(page: Page): Promise<string | null> {
    if (await isEdge(page)) return 'edge';
    if (await page.locator('.toggle-handle').count()) return 'default';
    return (await page.locator('.toolbar').count()) ? 'wish' : null;
}

/**
 * With all three layouts allowed, each layout's toggle steps to the next in
 * the registry's order: the sidebar's to the toolbar, the toolbar menu's to
 * the edge button, and the edge fan's back to the sidebar; the page keeps the
 * pick over a reload.
 */
async function layoutToggleBlock(host: Host): Promise<Checks> {
    const res = new Checks('layout-toggle');
    const url = await dashboardServer(host, 'layout-toggle', ALL_LAYOUTS);
    try {
        const status: unknown = JSON.parse((await curl(url, '/api/status'))[1] || '{}');
        const field = (k: string): unknown => (status !== null && typeof status === 'object' ? Reflect.get(status, k) : undefined);
        res.check('the server allows the three layouts, the sidebar first',
            field('ui_layout') === 'default' && JSON.stringify(field('ui_layout_allowed')) === JSON.stringify(['default', 'wish', 'edge']), [field('ui_layout'), field('ui_layout_allowed')]);
        const browser = await launchChromium();
        try {
            const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
            await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'websockets';" });
            const page = await ctx.newPage();
            await page.goto(url, { waitUntil: 'load' });
            res.check('video streams', (await waitWsVideo(page)) !== null);
            let shown = await shownLayout(page);
            res.check('the page opens on the sidebar', shown === 'default', shown);
            await sidebarOpen(page);
            let toggle: Locator | null = await sidebarMenuItem(page, '.layout-toggle');
            if (toggle !== null) {
                await toggle.click();
                await sleep(1500);
            }
            shown = await shownLayout(page);
            res.check("the sidebar's toggle steps to the toolbar", shown === 'wish', shown);
            await barShow(page);
            const triggers = page.locator(BAR_MENU_TRIGGERS);
            if (await triggers.count()) {
                await triggers.first().click();
                await sleep(600);
                toggle = page.locator('[role="menu"] .layout-toggle');
                if (await toggle.count()) {
                    await toggle.first().click();
                    await sleep(1500);
                }
            }
            shown = await shownLayout(page);
            res.check("the toolbar menu's toggle steps to the edge button", shown === 'edge', shown);
            await page.reload({ waitUntil: 'load' });
            await waitWsVideo(page);
            shown = await shownLayout(page);
            res.check('a reload keeps the pick', shown === 'edge', shown);
            const fan = await barTool(page, '.layout-fan-button');
            if (fan !== null) {
                await fan.click();
                await sleep(1500);
            }
            shown = await shownLayout(page);
            res.check("the edge fan's toggle steps back to the sidebar", shown === 'default', shown);
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
    return res;
}

const LAYOUTS = ['default', 'wish', 'edge'] as const;
const ENGINES: readonly Engine[] = ['chromium', 'firefox', 'webkit'];
type Run = (host: Host) => Promise<Checks[]>;
const perLayout = (block: (host: Host, layout: string) => Promise<Checks | Checks[]>): Run => async (host) => {
    const out: Checks[] = [];
    for (const layout of LAYOUTS) out.push(...[await block(host, layout)].flat());
    return out;
};

const RUNS: Record<string, Run> = {
    default: async (h) => [await dashBlock(h, 'default')],
    wish: async (h) => [await dashBlock(h, 'wish')],
    edge: async (h) => [await dashBlock(h, 'edge')],
    fan: async (h) => [await edgeBlock(h)],
    'layout-toggle': async (h) => [await layoutToggleBlock(h)],
    gates: perLayout(gatesBlock),
    hidpi: perLayout((h, l) => hidpiDefaultBlock(h, l)),
    'hidpi-webrtc': perLayout((h, l) => hidpiDefaultBlock(h, l, 'webrtc')),
    'cpu-threading': perLayout(cpuThreadingBlock),
    'raw-motion': perLayout((h, l) => rawPointerMotionBlock(h, l)),
    'raw-motion-webrtc': perLayout((h, l) => rawPointerMotionBlock(h, l, 'webrtc')),
    'rate-control': perLayout((h, l) => rateControlBlock(h, l)),
    'rate-control-webrtc': perLayout((h, l) => rateControlBlock(h, l, 'webrtc')),
    'settings-follow': perLayout((h, l) => settingsFollowBlock(h, l)),
    'settings-follow-webrtc': perLayout((h, l) => settingsFollowBlock(h, l, 'webrtc')),
    'paint-over': perLayout(async (h, l) => { const out: Checks[] = []; for (const e of ENGINES) out.push(await paintOverBlock(h, l, e)); return out; }),
    'page-socket': perLayout(async (h, l) => { const out: Checks[] = []; for (const e of ENGINES) out.push(await pageSocketBlock(h, l, e)); return out; }),
    'transport-advice': async (h) => {
        const out: Checks[] = [];
        for (const l of LAYOUTS) for (const dual of [true, false]) out.push(await transportAdviceBlock(h, l, dual));
        for (const e of ['firefox', 'webkit'] as const) out.push(await transportAdviceBlock(h, 'default', true, e));
        return out;
    },
    'dpi-resolution': perLayout(dpiForResolutionBlock),
    'second-screen': async (h) => [...await perLayout(secondScreenBlock)(h), await secondScreenAutoBlock(h, 'default')],
    layout: async (h) => [await layoutBlock(h)],
};

for (const [which, run] of Object.entries(RUNS)) {
    test(which, async () => {
        const host = new Host(suiteOf(import.meta.filename));
        const failures: string[] = [];
        try {
            for (const res of await run(host)) {
                try {
                    res.finish();
                } catch (e) {
                    failures.push(String(e));
                }
            }
        } finally {
            await host.close();
        }
        if (failures.length) throw new Error(failures.join('\n'));
    });
}
