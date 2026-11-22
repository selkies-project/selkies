/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// 9. Test all of the shortcuts, and especially the pointer lock when
// `Ctrl + Shift + Left Click` or full screen is used.
//
// The client's chords, pressed on the page: Ctrl+Shift+M opens and closes the
// dashboard, Ctrl+Shift+G shows and hides the touch gamepad, Ctrl+Shift+F
// enters fullscreen, Ctrl+Shift+Left Click locks the pointer to the stream, and
// Ctrl+Shift+X turns gaming mode on, which goes fullscreen and locks the
// pointer there. With shortcuts turned off the chords are the session's.
// Headless WebKit has neither fullscreen nor pointer lock to grant.
import type { Page } from '@playwright/test';

import { fields } from '../../../web/selkies-web-core/lib/narrow.ts';
import { sidebarMenuItem } from '../../lib/dashboard.ts';
import { sleep } from '../../lib/env.ts';
import { openSidebar, reveal, type Cell } from '../lib.ts';

export const ITEM = 9;
export const TITLE = 'shortcuts and pointer lock';

interface State {
    sidebar: boolean;
    fullscreen: boolean;
    lock: boolean;
    pad: boolean;
    gaming: boolean;
}

async function chord(page: Page, key: string): Promise<void> {
    await page.keyboard.down('Control');
    await page.keyboard.down('Shift');
    await page.keyboard.press(key);
    await page.keyboard.up('Shift');
    await page.keyboard.up('Control');
    await sleep(1200);
}

async function state(page: Page): Promise<State> {
    const got = fields(await page.evaluate(`({
      sidebar: !!document.querySelector('.sidebar.is-open'),
      fullscreen: !!document.fullscreenElement,
      lock: !!document.pointerLockElement,
      pad: [...document.querySelectorAll('#universal-touch-gamepad-controls-overlay')]
             .some(e => getComputedStyle(e).display !== 'none' && e.children.length > 0),
      gaming: !!(window.__it && window.__it.gaming),
    })`));
    return { sidebar: got.sidebar === true, fullscreen: got.fullscreen === true, lock: got.lock === true, pad: got.pad === true, gaming: got.gaming === true };
}

async function leave(page: Page): Promise<void> {
    await page.evaluate('document.pointerLockElement && document.exitPointerLock()');
    await page.evaluate('document.fullscreenElement && document.exitFullscreen()');
    await sleep(1000);
}

export async function run(cell: Cell): Promise<void> {
    const R = cell.res;
    const page = await cell.open();
    await page.mouse.click(900, 600);
    await sleep(500);
    const a = await state(page);
    await chord(page, 'KeyM');
    let b = await state(page);
    await chord(page, 'KeyM');
    let c = await state(page);
    R.check('Ctrl+Shift+M opens and closes the dashboard', a.sidebar !== b.sidebar && c.sidebar === a.sidebar, `${a.sidebar} -> ${b.sidebar} -> ${c.sidebar}`);
    await chord(page, 'KeyG');
    b = await state(page);
    await chord(page, 'KeyG');
    c = await state(page);
    R.check('Ctrl+Shift+G shows and hides the touch gamepad', b.pad && !c.pad, `${b.pad} -> ${c.pad}`);
    if (cell.engine === 'webkit') {
        R.skip('fullscreen, pointer lock, gaming mode', 'headless WebKit grants neither fullscreen nor pointer lock');
    } else {
        await chord(page, 'KeyF');
        const f = await state(page);
        R.check('Ctrl+Shift+F enters fullscreen', f.fullscreen, f);
        await page.keyboard.press('Escape');
        await sleep(1000);
        await leave(page);
        await page.keyboard.down('Control');
        await page.keyboard.down('Shift');
        await page.mouse.click(700, 400);
        await page.keyboard.up('Shift');
        await page.keyboard.up('Control');
        await sleep(1200);
        const lk = await state(page);
        R.check('Ctrl+Shift+Left Click locks the pointer to the stream', lk.lock, lk);
        await leave(page);
        await chord(page, 'KeyX');
        let g = await state(page);
        R.check('Ctrl+Shift+X locks the pointer at once, without a click on the stream', g.gaming && g.lock, g);
        if (g.gaming && !g.lock) {
            await page.mouse.click(700, 400);
            await sleep(1500);
            g = await state(page);
        }
        R.check('Ctrl+Shift+X turns gaming mode on: fullscreen with the pointer locked', g.gaming && g.fullscreen && g.lock, g);
        await chord(page, 'KeyX');
        await leave(page);
        const off = await state(page);
        R.check('Ctrl+Shift+X turns gaming mode off again', !off.gaming, off);
        // The dashboard's own button, the way most users enter it: the click on it must leave the
        // keyboard to the stream, or not one key reaches the game.
        await openSidebar(page);
        const button = await sidebarMenuItem(page, '.gaming-mode-button');
        if (button) {
            await button.click({ timeout: 5000 });
            await sleep(1500);
            g = await state(page);
            const keys = await page.evaluate(() => !document.activeElement?.closest('.allow-native-input'));
            R.check('the gaming mode button goes fullscreen and locks the pointer, the keyboard left to the stream',
                g.gaming && g.fullscreen && g.lock && keys, { ...g, keys });
            await chord(page, 'KeyX');
            await leave(page);
        } else {
            R.check('the dashboard shows its gaming mode button', false, 'no .gaming-mode-button');
        }
    }
    // Shortcuts off: the chords are the session's.
    if (await reveal(page, '#keyboardShortcutsToggle')) {
        await page.locator('#keyboardShortcutsToggle').first().click({ timeout: 5000 });
        await sleep(1000);
        await page.evaluate(() => { window.postMessage({ type: 'toggleDashboard' }, window.location.origin); });
        await sleep(800);
        const before = (await state(page)).sidebar;
        await chord(page, 'KeyM');
        const after = (await state(page)).sidebar;
        R.check('with shortcuts off, Ctrl+Shift+M no longer opens the dashboard', after === before, `${before} -> ${after}`);
        await openSidebar(page);
        if (await reveal(page, '#keyboardShortcutsToggle')) await page.locator('#keyboardShortcutsToggle').first().click({ timeout: 5000 });
    } else {
        R.skip('shortcuts off', 'no shortcuts switch in the dashboard');
    }
}
