/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Scroll (wheel) input parity e2e: Chromium over websockets and webrtc, both
// X11 and Wayland. Verifies REL_WHEEL buttons reach the X server (X11) or the
// compositor seat sees scroll (Wayland, through the observer). On Wayland a
// touchpad's stroke reaches the seat as a finger's: fractional values from a
// finger source, as far as the page scrolled, then an axis stop once the
// stroke pauses.
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, type Observer } from '../lib/host.ts';
import { launchClient } from '../lib/page.ts';
import { realConsole, waitWrVideo, waitWsVideo } from '../lib/stream.ts';

const WL = 'wayland-1';

const num = (v: unknown): number => (typeof v === 'number' ? v : NaN);

/** Scroll in a real browser and verify the events land server-side. */
async function wheelBlock(host: Host, mode: string, wayland: boolean, tag: string): Promise<Checks> {
    const res = new Checks(tag);
    const { url } = await host.start({ label: tag, mode, wayland });
    const { browser, page, consoleErrors, notFound } = await launchClient(url, { mode });
    let obs: Observer | null = null;
    try {
        const info = mode === 'websockets' ? await waitWsVideo(page) : await waitWrVideo(page, 45);
        res.check('video up', info !== null, info);
        await sleep(1000);
        await page.mouse.click(640, 360);
        await sleep(500);
        if (wayland) {
            obs = await host.observe(WL);
            await obs.ready();
            await page.mouse.move(10, 10);
        }
        for (let i = 0; i < 4; i++) {
            await page.mouse.wheel(0, 120);
            await sleep(400);
        }
        if (!wayland) {
            // X11: wheel = button 4 (up) / 5 (down). Watch our own
            // override-redirect window (mapped without a WM) and verify XTEST
            // ButtonPress 4/5.
            const watch = await host.watchInput();
            const size = await host.rootSize();
            await sleep(300);
            await page.mouse.move((size?.[0] ?? 1280) / 2, (size?.[1] ?? 720) / 2);
            await sleep(500);
            await page.mouse.wheel(0, 120);
            await sleep(300);
            await page.mouse.wheel(0, -120);
            await sleep(800);
            const events = (await watch.events()).map((e) => [e.kind, e.kind === 'MotionNotify' ? [e.x, e.y] : e.detail]);
            await watch.release();
            const presses = (button: number): number => events.filter(([k, d]) => k === 'ButtonPress' && d === button).length;
            res.check('X11: wheel down reached server (Button5)', presses(5) >= 1, events.slice(0, 6));
            res.check('X11: wheel up reached server (Button4)', presses(4) >= 1, events.slice(0, 6));
        } else if (obs) {
            await sleep(1000);
            const ev = await obs.wait('ptr_axis', 5);
            res.check('Wayland: pointer axis reached compositor', ev !== null, ev);
            // Opposite direction: the compositor must see a second axis event
            // rather than a repeat of the first.
            await page.mouse.wheel(0, -120);
            await sleep(800);
            res.check('Wayland: second pointer axis reached compositor', (await obs.wait('ptr_axis', 5)) !== null);
            // The page has a wheel's samples only at its fourth notch; the
            // ones before are a wheel's all the same.
            const wheelSources = (await obs.lines()).filter((l) => l.kind === 'ptr_axis_source').map((l) => l.source);
            res.check("Wayland: a wheel's notches come from a wheel source, its first ones too", wheelSources.length > 0 && wheelSources.every((s) => s === 0), wheelSources.slice(0, 8));
            // Past the page's one-second wheel reset, a stroke of small pixel
            // deltas is a touchpad's.
            await sleep(1300);
            const mark = (await obs.lines()).length;
            const stroke = [3.5, 6.25, 9.0, 4.75, 2.5];
            for (const d of stroke) {
                await page.mouse.wheel(0, d);
                await sleep(16);
            }
            await sleep(500);
            const seen = (await obs.lines(mark)).filter((l) => ['ptr_axis', 'ptr_axis_source', 'ptr_axis_stop'].includes(String(l.kind)));
            const values = seen.filter((l) => l.kind === 'ptr_axis' && l.axis === 0).map((l) => num(l.value));
            const sources = new Set(seen.filter((l) => l.kind === 'ptr_axis_source').map((l) => l.source));
            let lastAxis = -1;
            seen.forEach((l, i) => { if (l.kind === 'ptr_axis') lastAxis = i; });
            const stopped = seen.slice(lastAxis + 1).some((l) => l.kind === 'ptr_axis_stop');
            res.check("Wayland: a touchpad's stroke comes from a finger source", sources.size === 1 && sources.has(1), seen.slice(0, 4));
            const total = stroke.reduce((a, b) => a + b, 0);
            res.check('Wayland: in fractional values, as far as the page scrolled',
                values.some((v) => v !== Math.trunc(v)) && Math.abs(values.reduce((a, b) => a + b, 0) - total) < 0.1, `${JSON.stringify(values)} against ${total}`);
            res.check('Wayland: and stops once the stroke pauses', stopped, seen.slice(-3));
        }
        const real = realConsole(consoleErrors, notFound).errors;
        res.check('no console errors', real.length === 0, real.join('; ').slice(0, 120));
    } finally {
        await browser.close();
        await obs?.release();
        await host.stop();
    }
    return res;
}

for (const [mode, wayland] of [['websockets', false], ['webrtc', false], ['websockets', true], ['webrtc', true]] as const) {
    const key = `${mode}-${wayland ? 'wl' : 'x11'}`;
    test(key, async () => {
        const host = new Host(suiteOf(import.meta.filename));
        let res: Checks;
        try {
            res = await wheelBlock(host, mode, wayland, key);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
