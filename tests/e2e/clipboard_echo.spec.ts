/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The session's clipboard coming back from the browser is no copy.
//
// What the session copies reaches the browser's clipboard, and the browser
// can hand it back changed in form only: Chromium on Windows writes plain
// text with CRLF line ends, and markup comes back in the engine's own
// wrapping. Read on focus, or on the paste chord the page holds until its
// clipboard is sent, that form must not go back over the session's
// clipboard, or a file manager's cut (a URI list marked as a cut beside its
// plain text) becomes plain text, and the file manager's paste offers to save
// the clipboard as a new file instead of moving the files. A session
// application stands in for the file manager here, owning the clipboard with
// the formats Dolphin offers for a cut.
import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { gestures, pageWithClipboard, readRich } from '../lib/clipboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { within } from '../lib/stream.ts';

const FILES = ['file:///home/ubuntu/Documents/report.odt', 'file:///home/ubuntu/Documents/notes.txt'];
// Dolphin's cut: the URI list (CRLF-separated, as the format wants), the cut mark, and the paths as text.
const CUT: Record<string, string> = {
    'text/uri-list': `${FILES.join('\r\n')}\r\n`,
    'application/x-kde-cutselection': '1',
    'text/plain': FILES.join('\n'),
    UTF8_STRING: FILES.join('\n'),
};
const HTML = '<p>from <b>the session</b></p>';
const PLAIN = 'from the session';

const focus = async (page: Page): Promise<void> => {
    await within(page.evaluate(() => { window.dispatchEvent(new Event('focus')); }));
};

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const res = new Checks(`clipecho-${mode}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const { url } = await host.start({ label: mode, mode, env: { SELKIES_WEB_ROOT: DASHBOARD_DIST } });
            const [browser, page] = await pageWithClipboard(url, mode);
            let source: Awaited<ReturnType<Host['ownClipboard']>> | null = null;
            try {
                source = await host.ownClipboard(null, CUT);
                await gestures(page);
                const landed = await within(page.evaluate(() => navigator.clipboard.readText().catch((e: unknown) => `read failed: ${e instanceof Error ? e.name : String(e)}`)));
                res.check('a cut in the session reaches the browser as its paths', landed === FILES.join('\n'), landed);
                // What Chromium on Windows keeps for that text: the same, with CRLF line ends.
                await within(page.evaluate((t) => navigator.clipboard.writeText(t), FILES.join('\r\n')));
                await page.mouse.click(700, 450);
                await page.keyboard.press('Control+v');
                await sleep(2500);
                await focus(page);
                await sleep(2500);
                let owns = await source.owns();
                res.check("the paste chord leaves the session's cut its own (CRLF read-back)", owns, owns ? '' : "the session's clipboard was taken over");
                const mark = await host.x11Convert('application/x-kde-cutselection');
                res.check('the file manager still finds its cut mark', mark !== null && mark.toString() === '1', mark?.toString() ?? null);

                await source.release();
                source = await host.ownClipboard(null, { 'text/html': HTML, 'text/plain': PLAIN, UTF8_STRING: PLAIN });
                await gestures(page);
                const local = await readRich(page);
                res.check('a formatted copy in the session reaches the browser', local !== null && typeof local === 'object' && local.text === PLAIN, local);
                await focus(page);
                await sleep(2500);
                await page.mouse.click(700, 450);
                await page.keyboard.press('Control+v');
                await sleep(2500);
                owns = await source.owns();
                res.check('its markup read back is no copy: the session keeps its own', owns, owns ? '' : "the session's clipboard was taken over");

                // Control: a copy made in the browser afterwards still reaches the session.
                await within(page.evaluate((t) => navigator.clipboard.writeText(t), 'copied in the browser'));
                await focus(page);
                await sleep(2500);
                const plain = await host.x11Convert('UTF8_STRING');
                res.check('a real copy in the browser still reaches the session', plain?.toString() === 'copied in the browser', plain?.toString() ?? null);
            } finally {
                await source?.release();
                await browser.close();
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
