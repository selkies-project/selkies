/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// 10. Test audio/videos or anything else on the web browsers. Moreover, you
// should test IME in your own language.
//
// A clip with a moving picture and a tone plays in the session's own Chrome and
// Firefox: it has to play there, its sound has to reach the client, and its
// picture has to keep changing in the stream. Then Korean is composed through
// an IME on the client (Chrome's IME composition over CDP) and has to land in a
// text field on the session's desktop. Firefox's and WebKit's drivers offer no
// composition, so there it is not checked.
import type { Page } from '@playwright/test';

import { numberOr, stringOr } from '../../../web/selkies-web-core/lib/narrow.ts';
import { sleep } from '../../lib/env.ts';
import { audioLevel, sessionBrowser, stopSessionBrowser, type Cell } from '../lib.ts';

export const ITEM = 10;
export const TITLE = "media playback in the session's browsers, IME";
const WORD = '한국어 입력';

/** How many of several samples of the stream's picture differ from the one before. */
async function changing(cell: Cell, page: Page, secs = 4): Promise<number> {
    const points: [number, number][] = [];
    for (let x = 1; x < 5; x += 1) for (let y = 1; y < 4; y += 1) points.push([x / 5, y / 4]);
    let last: string | null = null;
    let changes = 0;
    for (let i = 0; i < Math.trunc(secs / 0.5); i += 1) {
        const s = await cell.sample(page, points);
        const sig = JSON.stringify(s ? s.points : null);
        if (last !== null && sig !== last) changes += 1;
        last = sig;
        await sleep(500);
    }
    return changes;
}

export async function run(cell: Cell): Promise<void> {
    const R = cell.res;
    const page = await cell.open();
    await page.mouse.click(900, 600);
    for (const which of ['chrome', 'firefox']) {
        await cell.clearReport('play');
        if (!await sessionBrowser(cell.target, { which, start: '/play.html', kiosk: true, profile: 'play' })) {
            R.skip(`playback in the session's ${which}`, 'not installed in the image');
            continue;
        }
        const rep = await cell.report('play', 25, (r) => (numberOr(r.t) ?? 0) > 1.0 && !r.paused);
        R.check(`a clip plays in the session's ${which}`, rep !== null, await cell.report('play', 1));
        const level = await audioLevel(page, 8);
        R.check(`its sound reaches the client from the session's ${which}`, level > 0, `level ${level}`);
        const moved = await changing(cell, page);
        R.check(`its picture keeps moving in the stream from the session's ${which}`, moved >= 3, `${moved} of 7 changed`);
        await stopSessionBrowser(cell.target, which, 'play');
    }
    // IME: composed on the client, committed into a field on the session's desktop.
    await cell.go('/ime.html');
    await sleep(3000);
    await cell.clearReport('ime');
    await page.mouse.click(640, 360);
    await sleep(500);
    if (cell.engine !== 'chromium' || !cell.ctx) {
        R.skip('Korean composed on the client', `${cell.engine}'s driver offers no IME composition, and the `
            + 'client takes no scripted composition events for a real one');
        await cell.go('/pattern.html');
        return;
    }
    const cdp = await cell.ctx.newCDPSession(page);
    const chars = [...WORD];
    for (let i = 1; i <= chars.length; i += 1) {
        await cdp.send('Input.imeSetComposition', { text: chars.slice(0, i).join(''), selectionStart: i, selectionEnd: i });
        await sleep(50);
    }
    await cdp.send('Input.insertText', { text: WORD });
    const rep = await cell.report('ime', 15, (r) => (stringOr(r.value) ?? '').includes(WORD));
    R.check("Korean composed on the client lands in a field on the session's desktop", rep !== null,
        JSON.stringify((stringOr((await cell.report('ime', 1))?.value) ?? '').slice(-30)));
    await cell.go('/pattern.html');
}
