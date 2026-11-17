/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * File transfers as a user makes them through the dashboard: the core's upload
 * progress, and a download from the file manager's row.
 * @module
 */
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';

import type { Locator, Page } from '@playwright/test';

import { sleep } from './env.ts';
import { within } from './stream.ts';

// Records the core's upload progress messages, so a check can wait for the
// end of a transfer the page started.
export const UPLOAD_JS = `
  window.__uploads = [];
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'fileUpload') window.__uploads.push(e.data.payload);
  });
`;

function fileSha(path: string): string | null {
    try {
        return createHash('sha256').update(readFileSync(path)).digest('hex');
    } catch {
        return null;
    }
}

/**
 * Download `name` out of the file manager, the way a user does: the Download
 * button on its row, which opens the file as an attachment. Whether the row
 * was found, and the hash of the bytes the browser was handed.
 */
export async function rowDownload(page: Page, dialog: Locator, name: string, timeoutS = 30): Promise<[boolean, string | null]> {
    const row = dialog.locator(`text="${name}"`).first();
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until && !(await row.count());) await sleep(400);
    if (!(await row.count())) return [false, null];
    try {
        // The row is the grid around the name; its Download button is the
        // sibling of the name cell, so it is reached from the row rather than
        // the label.
        const button = row.locator('xpath=ancestor::div[1]').locator('button[aria-label="Download"]').first();
        if (!(await button.count())) return [false, null];
        const [download] = await Promise.all([page.waitForEvent('download', { timeout: 20_000 }), button.click()]);
        const path = await download.path();
        return [true, fileSha(path)];
    } catch (e) {
        process.stdout.write(`      (row download: ${String(e)})\n`);
        return [false, null];
    }
}

export type Upload = Record<string, unknown>;

async function uploads(page: Page): Promise<Upload[]> {
    const got = await within(page.evaluate('window.__uploads'));
    return Array.isArray(got) ? got.filter((u): u is Upload => u !== null && typeof u === 'object').map((u) => ({ ...u })) : [];
}

/** The page's upload messages once `name` reports end or error; `watch` is called on every poll. */
export async function waitUploadEnd(page: Page, name: string, timeoutS = 60, watch?: () => Promise<void>): Promise<Upload[]> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (watch) await watch();
        const ups = await uploads(page);
        if (ups.some((u) => u.fileName === name && (u.status === 'end' || u.status === 'error'))) return ups;
        await sleep(300);
    }
    return uploads(page);
}
