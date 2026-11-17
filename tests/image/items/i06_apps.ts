/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// 6. Check app install, but it would only work when `SELKIES_COMMAND_ENABLED`
// is enabled, so check if it's enabled.
//
// The setting is read first, from what the server tells the page. Enabled, the
// dashboard's Apps panel must be published, and an app from its catalog
// (Geany, small) is installed through the panel's own Install button, found
// installed on the session by the runner, and removed again. Disabled, the
// panel must not be offered. An install is the session's own command, the same
// from every browser, so it runs once per transport, in Chrome.
import type { Page } from '@playwright/test';

import { fields } from '../../../web/selkies-web-core/lib/narrow.ts';
import { openAppsModal } from '../../lib/dashboard.ts';
import { sleep } from '../../lib/env.ts';
import { openSidebar, type Cell } from '../lib.ts';

export const ITEM = 6;
export const TITLE = 'app install (SELKIES_COMMAND_ENABLED)';
export const ENGINES = ['chromium'] as const;
export const ENGINE_NOTE = "an install is the session's command, run once per transport from Chrome";
const APP = 'geany';
const LABEL = 'Geany';

async function settle(page: Page, timeoutS: number): Promise<boolean> {
    const until = Date.now() + timeoutS * 1000;
    await sleep(2000);
    while (Date.now() < until) {
        if ((await page.locator('.app-action-button.running').count()) === 0) return true;
        await sleep(1000);
    }
    return false;
}

export async function run(cell: Cell): Promise<void> {
    const R = cell.res;
    const page = await cell.open();
    await sleep(3000);
    const enabled = Boolean(fields((await cell.settings(page)).command_enabled).value);
    const log = await cell.target.selkiesLog(4000);
    const hidden = log.split('\n').filter((l) => l.includes('Apps panel hidden')).slice(-1);
    await openSidebar(page);
    let shown = (await page.locator('.sidebar-section-header:has-text("Apps")').count()) > 0;
    if (!enabled) {
        R.check('SELKIES_COMMAND_ENABLED is off and the Apps panel is not offered', !shown, `panel shown=${shown}`);
        R.skip('app install', 'SELKIES_COMMAND_ENABLED is off in this image');
        return;
    }
    R.check('SELKIES_COMMAND_ENABLED is on and the Apps panel is published', shown, hidden.length ? hidden : 'no reason logged');
    if (!shown) return;
    await page.locator('.toggle-handle').first().click();
    await sleep(500);
    R.check('the Apps panel opens its catalog', await openAppsModal(page));
    // The catalog opens on one category; the search looks through all of them.
    await page.locator('[role="dialog"] input[type="text"]').first().fill(LABEL);
    await sleep(500);
    const card = page.locator(`[role="dialog"] [data-slot="card"]:has-text("${LABEL}")`).first();
    if (!await card.count()) {
        R.check(`the catalog lists ${LABEL}`, false, 'catalog without it (offline?)');
        return;
    }
    await page.locator(`text=${LABEL}`).first().click();
    await sleep(500);
    const install = page.locator('[role="dialog"] button:has-text("Install")').first();
    if (!await install.count()) {
        R.check(`${LABEL} offers Install`, (await page.locator('[role="dialog"] button:has-text("Remove")').count()) > 0, 'neither Install nor Remove');
    } else {
        await install.click();
        const done = await settle(page, 600);
        const listed = await cell.target.out('selkies-proot list 2>/dev/null');
        R.check(`Install puts ${LABEL} on the session`, done && (await page.locator('.app-action-button.remove').count()) > 0 && listed.includes(APP),
            `settled=${done}; runner lists ${JSON.stringify(listed.slice(0, 120))}`);
        const console = page.locator('[role="dialog"] .app-job-console').first();
        const text = (await console.count()) ? await console.innerText() : '';
        shown = text.includes('Installing') && !text.includes('##pa') && (await console.getAttribute('data-state')) === 'done';
        R.check("the install's console shows what proot-apps printed, its status lines left out", shown, text.slice(-200));
    }
    const remove = page.locator('.app-action-button.remove').first();
    if (await remove.count()) {
        await remove.click();
        const done = await settle(page, 300);
        const listed = await cell.target.out('selkies-proot list 2>/dev/null');
        R.check(`Remove takes ${LABEL} off the session again`, done && !listed.includes(APP), listed.slice(0, 120));
    }
}
