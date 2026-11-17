/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// 2. Check that the microphone/webcam works (use a web tester in both Firefox
// and Chrome). The toggle for the microphone/webcam is up at the top.
//
// The client browser offers fake devices (Chrome's
// --use-fake-device-for-media-stream, Firefox's media.navigator.streams.fake),
// the toggles at the top of the dashboard turn each on, and a tester page in
// the session's own Chrome and Firefox opens the microphone and camera the
// session offers its apps: the microphone has to carry sound, the camera a
// picture that changes. The server's log has to say it brought each device up.
import type { Page } from '@playwright/test';

import { fields, numberOr } from '../../../web/selkies-web-core/lib/narrow.ts';
import { sleep } from '../../lib/env.ts';
import { within } from '../../lib/stream.ts';
import { openSidebar, sessionBrowser, stopSessionBrowser, type Cell } from '../lib.ts';

export const ITEM = 2;
export const TITLE = 'microphone and webcam';

/** Click a toggle in the dashboard's top action bar by its title, opening the sidebar first. */
async function action(page: Page, title: string): Promise<boolean> {
    const btn = page.locator(`.sidebar-action-buttons button[title="${title}"]`);
    await openSidebar(page);
    if (!await btn.count()) return false;
    await btn.first().click({ timeout: 5000 });
    await sleep(1000);
    return true;
}

/**
 * Whether the client's own fake camera gives a picture, in a page of its own.
 * Headless WebKit's mock source takes its web process down when played, and a
 * page opened after that can wait forever, so WebKit is not asked.
 */
async function cameraPlays(cell: Cell): Promise<boolean> {
    if (cell.engine === 'webkit' || !cell.ctx) return false;
    const page = await within(cell.ctx.newPage(), 'the browser', 60);
    try {
        await page.goto(`${cell.target.url}/api/status`, { timeout: 30_000 });
        return Boolean(await page.evaluate(async () => {
            const s = await navigator.mediaDevices.getUserMedia({ video: true });
            const v = document.createElement('video');
            v.muted = true;
            v.srcObject = s;
            await v.play();
            await new Promise((r) => { setTimeout(r, 1500); });
            const ok = v.videoWidth > 16;
            s.getTracks().forEach((t) => { t.stop(); });
            return ok;
        }));
    } catch {
        return false;
    } finally {
        try {
            await page.close();
        } catch {
            // gone with its browser
        }
    }
}

type Report = Record<string, unknown> | null;

function heard(r: Report): boolean {
    return (numberOr(fields(r?.audio).rms) ?? 0) > 0.003;
}

function seen(r: Report): boolean {
    const v = fields(r?.video);
    return r?.video !== undefined && (numberOr(v.frames) ?? 0) > 4 && (numberOr(v.changes) ?? 0) > 2 && (numberOr(v.w) ?? 0) > 0;
}

export async function run(cell: Cell): Promise<void> {
    const R = cell.res;
    const page = await cell.open();
    const probe: unknown = await page.evaluate(async () => {
        try {
            const s = await navigator.mediaDevices.getUserMedia({ audio: true, video: true });
            const r = s.getTracks().map((t) => `${t.kind}:${t.label}`);
            s.getTracks().forEach((t) => { t.stop(); });
            return r;
        } catch (e) {
            return String(e);
        }
    });
    if (!Array.isArray(probe)) {
        R.skip('microphone and webcam', `this browser offers no fake devices: ${String(probe)}`);
        return;
    }
    const camera = await cameraPlays(cell);
    R.check("the top bar's microphone toggle is there and takes a click", await action(page, 'Enable Microphone'));
    R.check("the top bar's webcam toggle is there and takes a click", await action(page, 'Enable Webcam'));
    await sleep(3000);
    // Each device announces itself once per server, so the whole log is read.
    const log = await cell.target.selkiesLog(50_000);
    for (const which of ['chrome', 'firefox']) {
        await cell.clearReport('media');
        // Without the client's camera there is no session camera, and asking for one fails the microphone too.
        const kinds = camera ? 'audio,video' : 'audio';
        if (!await sessionBrowser(cell.target, { which, start: `/media.html?kind=${kinds}`, kiosk: false, profile: 'media' })) {
            R.skip(`the session's ${which}`, 'not installed in the image');
            continue;
        }
        const r = await cell.report('media', 30, (got) => (heard(got) && (seen(got) || !camera)) || Boolean(got.error))
            ?? await cell.report('media', 1);
        R.check(`a tester in the session's ${which} hears the client's microphone`, heard(r), r?.audio ?? r?.error);
        if (camera) {
            R.check(`a tester in the session's ${which} sees the client's webcam`, seen(r), r?.video ?? r?.error);
        } else {
            R.skip(`a tester in the session's ${which} sees the client's webcam`, `the client's own camera shows no picture in ${cell.engine} (its mock source)`);
        }
        await stopSessionBrowser(cell.target, which, 'media');
    }
    const lines = log.split('\n');
    R.check('the server brought up its virtual microphone', log.includes('Virtual microphone') || log.toLowerCase().includes('microphone'),
        lines.filter((l) => l.includes('icrophone')).slice(-2));
    if (camera) {
        R.check('the server brought up its virtual webcam', log.includes('Virtual webcam') || log.toLowerCase().includes('webcam'),
            lines.filter((l) => l.includes('ebcam')).slice(-2));
    }
    await action(page, 'Disable Microphone');
    await action(page, 'Disable Webcam');
    await cell.go('/pattern.html');
}
