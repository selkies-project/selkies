/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The dashboard as a user reaches it, in each of its layouts: the sidebar
 * (`default`), the toolbar (`wish`), and the edge button (`edge`) with its
 * fan of tools. A helper opens what it needs the way a click does, and says
 * whether it got there.
 * @module
 */

import type { Locator, Page } from '@playwright/test';

import { sleep } from './env.ts';
import { within } from './stream.ts';

/** The bar's buttons that open a menu. */
export const BAR_MENU_TRIGGERS = '[data-slot="tooltip-trigger"][aria-haspopup="menu"]';
/** Anything in the toolbar, which the chrome checks look for. */
export const WISH_CHROME = '[data-slot="button-group"]';
/** The edge layout's floating button: a tap opens its fan of tools. */
export const EDGE_BUTTON = '.edge-button';
/** A slot of the edge layout's open fan, each holding one button. */
export const EDGE_FAN = '[data-edge-fan]';
/** The fan's dots button, the last slot of each page where the window holds fewer than its tools. */
export const EDGE_MORE = `${EDGE_FAN} .fan-more-button`;
/** Any layout's chrome. */
export const CHROME = `.sidebar, .toolbar, ${WISH_CHROME}, ${EDGE_BUTTON}`;

/**
 * Open the sidebar layout's drawer where it is shut.
 * @param page The dashboard.
 */
export async function sidebarOpen(page: Page): Promise<void> {
    if (!(await page.locator('.sidebar.is-open').count())) {
        await page.locator('.toggle-handle').first().click();
        await sleep(800);
    }
}

/**
 * Shut the sidebar layout's drawer where it is open.
 * @param page The dashboard.
 */
export async function sidebarClose(page: Page): Promise<void> {
    if (await page.locator('.sidebar.is-open').count()) {
        await sidebarMenuClose(page);
        await page.locator('.toggle-handle').first().click();
        await sleep(800);
    }
}

/**
 * The sidebar header's menu button, whose menu holds fullscreen, gaming mode,
 * a touch client's switches, and the layout, translucency, and theme controls.
 */
export const SIDEBAR_MENU = '.sidebar-menu-button';
/** The open header menu. */
const SIDEBAR_MENU_POPUP = '.sidebar-menu-popup';

/**
 * Find the item `selector` names in the open drawer's header menu, opening the
 * menu the way a click does. A menu without it is left shut, so a touch made
 * next reaches the stream, and is opened again on the next call, since what
 * it offers changes once the page sees a touch.
 * @param page The dashboard, its drawer open.
 * @param selector The item's class, as its action names it.
 * @returns The item, or null where the menu does not offer it.
 */
export async function sidebarMenuItem(page: Page, selector: string): Promise<Locator | null> {
    const item = page.locator(`${SIDEBAR_MENU_POPUP} ${selector}`);
    if (await item.count()) return item.first();
    await sidebarMenuClose(page);
    const button = page.locator(`.sidebar.is-open ${SIDEBAR_MENU}`);
    if (!(await button.count())) return null;
    await button.first().click();
    await sleep(400);
    if (await item.count()) return item.first();
    await sidebarMenuClose(page);
    return null;
}

/**
 * Shut the sidebar's header menu where it is open.
 * @param page The dashboard.
 */
export async function sidebarMenuClose(page: Page): Promise<void> {
    const popup = page.locator(SIDEBAR_MENU_POPUP);
    if (!(await popup.count())) return;
    // Escape reaches the menu only from inside it: the stream takes the keys
    // elsewhere. A press outside it closes it too, past the backdrop it holds.
    await popup.first().focus().catch(() => {});
    await page.keyboard.press('Escape');
    await sleep(300);
    if (await popup.count()) {
        await page.mouse.click(1, 1);
        await sleep(300);
    }
}

/**
 * Unfold (or fold) the sidebar section whose header carries `name`, the
 * drawer opened first.
 * @param page The dashboard.
 * @param name The section's header text.
 * @param open Unfold, or fold.
 * @returns True when the header was found.
 */
export async function sidebarSection(page: Page, name: string, open = true): Promise<boolean> {
    await sidebarOpen(page);
    const header = page.locator(`.sidebar-section-header:has-text("${name}")`).first();
    if (!(await header.count())) return false;
    if (((await header.getAttribute('aria-expanded')) === 'true') !== open) {
        await header.scrollIntoViewIfNeeded();
        await header.click();
        await sleep(800);
    }
    return true;
}

/**
 * Close the bar's menus left open: a submenu (the clipboard's) takes an
 * Escape of its own before the menu under it, which would take the next click.
 * @param page The dashboard.
 */
export async function barCloseMenus(page: Page): Promise<void> {
    for (let i = 0; i < 3; i++) {
        const menus = page.locator('[role="menu"]');
        if (!(await menus.count())) return;
        // Escape reaches a menu only from within it: a field blurred in place
        // leaves the focus on the page.
        await menus.last().focus({ timeout: 1000 }).catch(() => undefined);
        await page.keyboard.press('Escape');
        await sleep(300);
    }
}

/**
 * Press Escape until no dialog is open.
 * @param page The dashboard.
 * @param presses At most this many.
 */
export async function closeDialogs(page: Page, presses = 3): Promise<void> {
    for (let i = 0; i < presses; i++) {
        if (!(await page.locator('[role="dialog"]').count())) return;
        await page.keyboard.press('Escape');
        await sleep(500);
    }
}

/**
 * @param page The dashboard.
 * @returns Whether it shows the edge layout.
 */
export async function isEdge(page: Page): Promise<boolean> {
    return (await page.locator(EDGE_BUTTON).count()) > 0;
}

/**
 * @param page The dashboard.
 * @returns Whether the edge button's fan is open.
 */
export async function edgeFanOpen(page: Page): Promise<boolean> {
    return (await page.locator(`${EDGE_BUTTON}[aria-expanded="true"]`).count()) > 0;
}

/**
 * The page the fan shows and how many it has, read off its dots button.
 * @param page The dashboard.
 * @returns `[page, pages]` from 1; `[1, 1]` without a dots button.
 */
export async function edgeFanPage(page: Page): Promise<[number, number]> {
    const more = page.locator(EDGE_MORE);
    const label = (await more.count()) ? ((await more.first().getAttribute('aria-label')) ?? '') : '';
    const found = /(\d+)\/(\d+)\s*$/.exec(label);
    return found ? [Number(found[1]), Number(found[2])] : [1, 1];
}

/**
 * Open the edge button's fan. A tap first brings a folded button back or
 * shuts the settings card the button holds, so it is tapped until the fan is up.
 * @param page The dashboard.
 * @returns True when the fan is open.
 */
export async function edgeOpenFan(page: Page): Promise<boolean> {
    const button = page.locator(EDGE_BUTTON).first();
    for (let i = 0; i < 4; i++) {
        if ((await edgeFanOpen(page)) || !(await button.count())) break;
        await button.click({ timeout: 5000 });
        await sleep(600);
    }
    return edgeFanOpen(page);
}

/**
 * Shut the edge button's fan where it is open, by a tap on the button.
 * @param page The dashboard.
 */
export async function edgeCloseFan(page: Page): Promise<void> {
    if (await edgeFanOpen(page)) {
        await page.locator(EDGE_BUTTON).first().click({ timeout: 5000 });
        await sleep(400);
    }
}

/**
 * Whether the element at the center of `locator` is it or inside it: a press lands there.
 * @param locator The element.
 * @returns Whether it takes the press, and a short description of what is there.
 */
export async function takesTap(locator: Locator): Promise<[boolean, string]> {
    return locator.evaluate((el): [boolean, string] => {
        const r = el.getBoundingClientRect();
        const top = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        const name = top ? `${top.tagName.toLowerCase()}.${String(top.className).trim().split(/\s+/).slice(0, 4).join('.')}` : 'nothing';
        return [top !== null && (top === el || el.contains(top)), name];
    });
}

/**
 * Turn the fan to its next page with the dots button, clicked as a user does.
 * @param page The dashboard.
 * @returns False without a dots button, or where something else takes the press.
 */
export async function edgeTurnPage(page: Page): Promise<boolean> {
    const more = page.locator(EDGE_MORE);
    if (!(await more.count())) return false;
    try {
        await more.first().click({ timeout: 3000 });
    } catch {
        process.stdout.write(`      (the fan's dots button takes no press: ${(await takesTap(more.first()))[1]} is over it)\n`);
        return false;
    }
    await sleep(500);
    return true;
}

/**
 * Close any bar menu left open, whose trigger would only close it, and bring
 * the bar's buttons into reach: the toolbar back where Ctrl+Shift+M or gaming
 * mode slid it up, or the edge button's fan open on its first page.
 * @param page The dashboard.
 */
export async function barShow(page: Page): Promise<void> {
    await barCloseMenus(page);
    if (await isEdge(page)) {
        if ((await edgeFanOpen(page)) && (await edgeFanPage(page))[0] !== 1) await edgeCloseFan(page);
        await edgeOpenFan(page);
        return;
    }
    const show = page.locator('.toolbar button[aria-label="Show toolbar"]');
    if (await show.count()) {
        await show.first().click();
        await sleep(600);
    }
}

/**
 * A bar layout's button `selector`, brought into reach as a user reaches it:
 * the toolbar's, the bar shown where it slid up, or the edge fan's, the fan
 * opened and turned with its dots button to the page that holds it.
 * @param page The dashboard.
 * @param selector The button (the class `panels/tools.tsx` gives it, or its label).
 * @returns Its locator, or null where the layout offers no such button.
 */
export async function barTool(page: Page, selector: string): Promise<Locator | null> {
    await barShow(page);
    if (!(await isEdge(page))) {
        const found = page.locator(`.toolbar ${selector}`);
        return (await found.count()) ? found.first() : null;
    }
    for (let i = 0; i < 8; i++) {
        const found = page.locator(`${EDGE_FAN} ${selector}`);
        if (await found.count()) return found.first();
        if (!(await edgeTurnPage(page)) || (await edgeFanPage(page))[0] === 1) return null;
    }
    return null;
}

/**
 * Open the bar's menu that carries `label` and click that item, each menu
 * tried in turn until one renders the item (a submenu trigger or a checkbox
 * item counts).
 * @param page The dashboard.
 * @param label The item's text.
 * @returns True when it was found and clicked.
 */
export async function barOpenMenuItem(page: Page, label: string): Promise<boolean> {
    await barShow(page);
    const triggers = page.locator(BAR_MENU_TRIGGERS);
    const count = await triggers.count();
    for (let i = 0; i < count; i++) {
        try {
            await triggers.nth(i).click();
            await sleep(500);
            const item = page.locator(`[role="menu"] [role^="menuitem"]:has-text("${label}")`).first();
            if (await item.count()) {
                await item.click();
                await sleep(800);
                return true;
            }
            await page.keyboard.press('Escape');
            await sleep(200);
        } catch {
            // A trigger that went away with the last menu: the next one is tried.
        }
    }
    return false;
}

/**
 * The text of every menu the bar opens, each opened in turn and shut again.
 * @param page The dashboard.
 * @returns The menus' text.
 */
export async function barMenuText(page: Page): Promise<string> {
    await barShow(page);
    let text = '';
    const triggers = page.locator(BAR_MENU_TRIGGERS);
    const count = await triggers.count();
    for (let i = 0; i < count; i++) {
        try {
            await triggers.nth(i).click();
            await sleep(600);
            text += `${await page.evaluate(() => [...document.querySelectorAll('[role="menu"]')].map((m) => m instanceof HTMLElement ? m.innerText : '').join('\n'))}\n`;
            await page.keyboard.press('Escape');
            await sleep(200);
        } catch {
            // As barOpenMenuItem.
        }
    }
    return text;
}

/**
 * Shut the floating panel the bar's button `selector` opened: by that
 * button, as it opened it, or by the panel's own close button where the
 * panel lies over the button.
 * @param page The dashboard.
 * @param selector The button.
 */
export async function barClosePanel(page: Page, selector: string): Promise<void> {
    const button = await barTool(page, selector);
    if (button !== null && (await takesTap(button))[0]) {
        await button.click();
    } else {
        const close = page.locator('[data-drag-handle] button[aria-label="Close"]');
        if (await close.count()) {
            process.stdout.write(`      (the panel lies over its button ${selector}; shut by its own close button)\n`);
            await close.first().click();
        } else if (button !== null) {
            // A panel without a close button of its own (the stats) and lying over
            // its toggle: the walk goes on past it.
            process.stdout.write(`      (the panel lies over its button ${selector} and has no close button; pressed through)\n`);
            await button.dispatchEvent('click');
        }
    }
    await sleep(300);
}

/**
 * What the bar's Gamepads panel offers, opened from its button and closed
 * again: the gamepad input switch (`input`), and the pads part that
 * ui_sidebar_show_gamepads gates (`pads`: told by its Rumble switch).
 * @param page The dashboard.
 * @returns What it showed.
 */
export async function barGamepadPanel(page: Page): Promise<{ input: boolean; pads: boolean }> {
    const button = await barTool(page, 'button[aria-label="Gamepads"]');
    if (button === null) return { input: false, pads: false };
    await button.click();
    await sleep(600);
    // A label clips its text in a span of its own, so the text is matched, not the label.
    const got = {
        input: (await page.getByText('Gamepad Input', { exact: true }).count()) > 0,
        pads: (await page.getByText('Rumble', { exact: true }).count()) > 0,
    };
    await barClosePanel(page, 'button[aria-label="Gamepads"]');
    return got;
}

/**
 * Open the bar's settings card (its Settings2 button: the toolbar's, or the
 * edge fan's) unless it is open, and switch it to the tab labeled `tab`. A
 * tab folds its cards in an accordion with one open at a time: where
 * `reveal` names a control the open card does not hold, the cards are opened
 * in turn until one does.
 * @param page The dashboard.
 * @param tab The tab's label.
 * @param reveal A control to bring into the open card.
 * @returns True when the tab was reached.
 */
export async function openBarSettingsTab(page: Page, tab: string, reveal = ''): Promise<boolean> {
    await barCloseMenus(page);
    if (!(await page.locator('[role="tab"]').count())) {
        const trigger = await barTool(page, '.settings-button');
        if (trigger === null) return false;
        await trigger.click({ force: true, timeout: 3000 });
        await sleep(1000);
    }
    const tabButton = page.locator(`[role="tab"]:has-text("${tab}")`).first();
    if (!(await tabButton.count())) return false;
    await tabButton.click();
    await sleep(800);
    if (reveal && !(await page.locator(reveal).count())) {
        const cards = page.locator('[role="tabpanel"] [data-slot="accordion-trigger"]');
        const count = await cards.count();
        for (let i = 0; i < count; i++) {
            if ((await cards.nth(i).getAttribute('aria-expanded')) === 'true') continue;
            await cards.nth(i).click();
            await sleep(500);
            if (await page.locator(reveal).count()) break;
        }
    }
    return true;
}

/**
 * Reach the video settings: the sidebar's Video section, or a bar layout's
 * settings card on its Video tab holding `reveal` open.
 * @param page The dashboard.
 * @param layout The dashboard's layout.
 * @param reveal A control to bring into the open card.
 * @returns True when the controls were reached.
 */
export function openVideoSettings(page: Page, layout: string, reveal = ''): Promise<boolean> {
    return layout === 'default' ? sidebarSection(page, 'Video') : openBarSettingsTab(page, 'Video', reveal);
}

/**
 * Reach the screen settings: the sidebar's Screen section, or a bar layout's
 * settings card on its Resolution tab holding `reveal` open.
 * @param page The dashboard.
 * @param layout The dashboard's layout.
 * @param reveal A control to bring into the open card.
 * @returns True when the controls were reached.
 */
export function openScreenSettings(page: Page, layout: string, reveal = ''): Promise<boolean> {
    return layout === 'default' ? sidebarSection(page, 'Screen') : openBarSettingsTab(page, 'Resolution', reveal);
}

declare global {
    interface Window {
        videoChunksReceived?: number;
    }
}

/**
 * Wait for the first video chunk, by which the dashboard has the server's
 * settings. It takes a canvas of any size: a phone's is under 640 pixels.
 * @param page The client.
 * @param timeoutS How long to wait.
 * @returns Whether one came.
 */
export async function waitChunk(page: Page, timeoutS = 15): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (await within(page.evaluate(() => (window.videoChunksReceived ?? 0) > 0))) return true;
        await sleep(250);
    }
    return false;
}

/**
 * Open the dashboard's clipboard panel.
 * @param page The client.
 * @param dashboard The layout.
 * @returns Whether it opened.
 */
export async function openClipboardPanel(page: Page, dashboard: string): Promise<boolean> {
    if (dashboard === 'default') {
        if (!(await within(page.evaluate(() => Boolean(document.querySelector('.sidebar.is-open')))))) {
            await within(page.evaluate(() => { window.postMessage({ type: 'toggleDashboard' }, window.location.origin); }));
            await sleep(800);
        }
        if ((await page.locator('#dashboardClipboardTextarea').count()) === 0) {
            await page.locator('.sidebar-section-header:has-text("Clipboard")').first().click();
            await sleep(600);
        }
        return (await page.locator('#dashboardClipboardTextarea').count()) > 0;
    }
    return barOpenMenuItem(page, 'Clipboard');
}

/**
 * Choose `file` through the clipboard panel's Upload Image button, as a user
 * does. The browser's file dialog takes the window's focus while it is open,
 * which closes any menu around the button (a bar's panel is a submenu).
 * Playwright answers the dialog without showing it, so the blur the dialog
 * would cause is dispatched between the click and the answer.
 * @param page The client.
 * @param dashboard The layout.
 * @param file The file picked.
 * @returns False when the panel or its button is not there.
 */
export async function pickClipboardImage(page: Page, dashboard: string, file: { name: string; mimeType: string; buffer: Buffer }): Promise<boolean> {
    if (!(await openClipboardPanel(page, dashboard))) return false;
    const button = page.locator('button:has-text("Upload Image")').first();
    if ((await button.count()) === 0) return false;
    const chooser = page.waitForEvent('filechooser', { timeout: 5000 });
    await button.click();
    const picked = await chooser;
    await within(page.evaluate(() => window.dispatchEvent(new Event('blur'))));
    await sleep(400);
    await picked.setFiles(file);
    await sleep(800);
    return true;
}

/**
 * Pick `mode` (cbr or crf) in either layout's rate-control menu, opening the video settings first where they are closed.
 * @returns Whether the menu was found and the pick made.
 */
export async function pickRateControl(page: Page, dashboard: string, mode: string): Promise<boolean> {
    try {
        if (!(await page.locator('#rateControlSelect').count())) await openVideoSettings(page, dashboard, '#rateControlSelect');
        await page.selectOption('#rateControlSelect', mode, { timeout: 5000 });
        return true;
    } catch {
        return false;
    }
}

/** Whether a dashboard switch reads on. */
export async function switchOn(sw: Locator): Promise<boolean> {
    return (await sw.getAttribute('aria-checked')) === 'true';
}

/** The sidebar's apps row, which opens the apps dialog. */
export const SIDEBAR_APPS = '.sidebar-section-link:has-text("Apps")';

/** Open the apps dialog from the sidebar's apps row; false when it never appears. */
export async function openAppsModal(page: Page): Promise<boolean> {
    await page.locator('.toggle-handle').first().click();
    await sleep(600);
    const row = page.locator(SIDEBAR_APPS).first();
    if ((await row.count()) === 0) return false;
    await row.click();
    await sleep(1000);
    return (await page.locator('[role="dialog"]:has-text("Apps")').count()) > 0;
}
