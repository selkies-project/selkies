/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Large clipboard (multipart) parity: push a ~2 MB text payload from the X
// server to the page (server->client), on both transports. Confirms the
// multipart chunked path carries a payload of many chunks whole.
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { launchClient } from '../lib/page.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const res = new Checks(`bigclip-${mode}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const { url } = await host.start({ label: mode, mode });
            const client = await launchClient(url, { mode });
            try {
                const { page } = client;
                const info = mode === 'websockets' ? await waitWsVideo(page) : await waitWrVideo(page, 45);
                res.check('video up', info !== null, info);
                await sleep(1500);
                const big = `BIGCLIP-${'x'.repeat(2 * 1024 * 1024)}`;
                const owner = await host.ownClipboard(big);
                await page.waitForTimeout(3500);
                const got = await within(page.evaluate('window.__clipMsgs.map(m => ({text: m.text, t: m.totalLength}))'));
                await owner.release();
                // The live text is posted as a truncated preview (bounded
                // memory in the dashboard); the authoritative signal is
                // totalLength and the real browser clipboard holding the full
                // payload.
                const lengths = (Array.isArray(got) ? got : []).map((g): unknown => (g !== null && typeof g === 'object' ? Reflect.get(g, 't') : undefined));
                const okLen = lengths.some((t) => typeof t === 'number' && t >= 2_000_000);
                let local = '';
                try {
                    const read = await within(page.evaluate('navigator.clipboard.readText()'));
                    local = typeof read === 'string' ? read : '';
                } catch (e) {
                    process.stdout.write(`clipboard read err: ${String(e).slice(0, 80)}\n`);
                }
                const okLocal = local.startsWith('BIGCLIP-') && local.length >= 2_000_000;
                res.check('server->client large clipboard totalLength arrives', okLen, `totalLengths ${JSON.stringify(lengths)}`);
                res.check('server->client large clipboard lands in navigator.clipboard', okLocal, `local len ${local.length}`);
                await sleep(500);
            } finally {
                await client.browser.close();
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
