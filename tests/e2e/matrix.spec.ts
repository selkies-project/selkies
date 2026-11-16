/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The transport and backend matrix, {websockets, webrtc} x {x11, wayland},
// each through the whole checklist: status and health, video (and audio on
// WebRTC), keys and pointer reaching the desktop, the clipboard both ways, a
// resize followed, a live settings change, a shared viewer, a second display,
// and a clean console.
import { test } from '@playwright/test';

import { fields, stringOr } from '../../web/selkies-web-core/lib/narrow.ts';
import { engineFromEnv } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { pulseSine, pulseUnload, wlCopy, wlPaste } from '../lib/desktop.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, type Held, type Observer } from '../lib/host.ts';
import { launchClient, newPage, sendClipboardFromClient, serverClipboardPushed, settingsChange } from '../lib/page.ts';
import { realConsole, waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

/** The keysym of `x`. */
const XK_X = 0x78;

const BLOCKS: readonly [string, string, boolean][] = [
    ['ws-x11', 'websockets', false],
    ['wr-x11', 'webrtc', false],
    ['ws-wl', 'websockets', true],
    ['wr-wl', 'webrtc', true],
];

for (const [selector, mode, wayland] of BLOCKS) {
    test(selector, async () => {
        const tag = `${mode}-${wayland ? 'wl' : 'x11'}`;
        const engine = engineFromEnv();
        const res = new Checks(tag);
        const wlSocket = 'wayland-1';
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const server = await host.start({ label: tag, mode, wayland, env: { SELKIES_VIDEO_STREAMING_MODE: 'true' } });
            const status = await fetch(`${server.url}/api/status`, { signal: AbortSignal.timeout(10_000) });
            res.check('server up', true, status.status);
            const stMode = stringOr(fields(await (await fetch(`${server.url}/api/status`, { signal: AbortSignal.timeout(10_000) })).json()).current_mode);
            res.check('api/status mode', stMode === mode, stMode);
            const health = (await fetch(`${server.url}/api/health`, { signal: AbortSignal.timeout(10_000) })).status;
            res.check('api/health', health === 200, health);

            const { browser, context, page, consoleErrors, notFound } = await launchClient(server.url, { mode, engine });
            let obs: Observer | null = null;
            try {
                if (mode === 'websockets') {
                    const info = await waitWsVideo(page);
                    res.check('video: canvas painted', info !== null, info);
                    // Headless rAF throttling zeroes the client's fps metric, so
                    // binary WS frames are counted instead.
                    let frames = 0;
                    for (const until = Date.now() + 10_000; Date.now() < until;) {
                        frames = await within(page.evaluate(() => window.videoChunksReceived || window.__wsFrames || 0));
                        if (frames >= 24) break;
                        await sleep(500);
                    }
                    res.check('video: WS frames flowing', frames >= 24, frames);
                } else {
                    const info = await waitWrVideo(page);
                    res.check('video: <video> receiving', info !== null, info);
                    if (info) res.check('audio: audio track negotiated', (info.audio ?? 0) >= 1, info.audio);
                }
                await sleep(1000);

                if (wayland) {
                    obs = await host.observe(wlSocket);
                    res.check('wl observer mapped', await obs.ready(), '');
                }
                await page.mouse.click(640, 360);
                await sleep(500);
                await page.keyboard.down('x');
                await sleep(800);
                if (obs) {
                    const ev = await obs.wait('kbd_key', 5);
                    res.check('input: key reached wayland seat', ev !== null, ev);
                } else {
                    const pressed = await host.keyHeld(XK_X);
                    res.check('input: key held in X keymap', pressed === true, pressed);
                }
                await page.keyboard.up('x');
                await sleep(300);
                if (obs) {
                    const ev = await obs.wait('kbd_key', 3, { state: 0 });
                    res.check('input: key release reached wayland seat', ev !== null, ev);
                } else {
                    const pressed = await host.keyHeld(XK_X);
                    res.check('input: key released in X keymap', pressed === false, pressed);
                }

                // Wayland pointer events reach the focused surface only, so the aim
                // is inside the observer's own 16x16 surface at 0,0; X11 is absolute.
                if (obs) {
                    await page.mouse.move(8, 8);
                    await sleep(600);
                    const motion = await obs.wait('ptr_motion', 4);
                    res.check('input: pointer motion reached wayland seat', motion !== null && 'x' in motion, motion);
                    await page.mouse.click(8, 8);
                    await sleep(400);
                    const button = await obs.wait('ptr_button', 3, { state: 1 });
                    res.check('input: mouse button reached wayland seat', button !== null, button);
                } else {
                    // The client maps page coordinates onto the stream it is shown, so
                    // the X pointer lands at the page point scaled by the root size over
                    // the video element's own. A session that opened on a root wider than
                    // the element is mapping correctly, and the check follows the same
                    // arithmetic to say so.
                    const box = await within(page.evaluate(() => {
                        const v = document.querySelector('video');
                        if (!v) return null;
                        const r = v.getBoundingClientRect();
                        return { w: r.width, h: r.height, vw: v.videoWidth, vh: v.videoHeight };
                    }));
                    const before = await host.pointer();
                    await page.mouse.move(200, 150);
                    await sleep(600);
                    const after = await host.pointer() ?? [-1, -1];
                    const want = box && box.w && box.h && box.vw && box.vh
                        ? [200 * box.vw / box.w, 150 * box.vh / box.h]
                        : [200, 150];
                    const moved = Math.abs(after[0] - want[0]) <= 4 && Math.abs(after[1] - want[1]) <= 4;
                    res.check('input: pointer moved in X (200,150)', moved,
                        `${JSON.stringify(before)}->${JSON.stringify(after)} want=(${want[0].toFixed(0)},${want[1].toFixed(0)}) ${JSON.stringify(box)}`);
                    await page.mouse.click(300, 200);
                    await sleep(300);
                }

                const probe = `e2e-${tag}-c2s-${Math.floor(Date.now() / 1000)}`;
                await sendClipboardFromClient(page, probe, engine);
                let got: string | null = null;
                for (const until = Date.now() + 8000; Date.now() < until;) {
                    got = wayland ? wlPaste(wlSocket).trim() : await host.x11Clipboard(3);
                    if (got === probe) break;
                    await sleep(500);
                }
                res.check('clipboard: client text reached server', got === probe, JSON.stringify(got).slice(0, 80));

                const pushPayload = `e2e-${tag}-s2c-${Math.floor(Date.now() / 1000)}`;
                let owner: Held | null = null;
                if (wayland) await wlCopy(wlSocket, pushPayload);
                else owner = await host.ownClipboard(pushPayload);
                await page.waitForTimeout(2500);
                const pushed = await serverClipboardPushed(page, pushPayload);
                await owner?.release();
                res.check('clipboard: server push reached page', pushed, '');

                if (!wayland) {
                    const [reqW, reqH] = [1242, 694];
                    await page.setViewportSize({ width: reqW, height: reqH });
                    let realized: [number, number] | null = null;
                    for (const until = Date.now() + 12_000; Date.now() < until;) {
                        realized = await host.rootSize();
                        if (realized && Math.abs(realized[0] - reqW) <= 16 && Math.abs(realized[1] - reqH) <= 16) break;
                        await sleep(500);
                    }
                    res.check('resize: X root follows browser', realized !== null && Math.abs(realized[0] - reqW) <= 16,
                        `req=${reqW}x${reqH} actual=${JSON.stringify(realized)}`);
                } else {
                    await page.setViewportSize({ width: 1242, height: 694 });
                    res.check('resize: wayland realized geometry logged', await host.saw('Wayland realized geometry', 12), '');
                }

                await settingsChange(page, { framerate: 30 });
                const applied = mode === 'websockets'
                    ? await host.saw("Applying video settings for 'primary' live (no restart)", 10)
                    : await host.saw('Updated framerate to', 10);
                res.check('settings: framerate change applied live', applied, '');

                // The wr core turns video_bitrate into the 'vb,' opcode, which the
                // server must clamp to the configured range.
                if (mode === 'webrtc') {
                    await settingsChange(page, { video_bitrate: 1 });
                    await sleep(1500);
                    res.check("F3: 'vb,1' clamped to server range min", await host.saw('clamped to 100', 6), '');
                    await settingsChange(page, { video_bitrate: 6000 });
                }

                const vpage = await newPage(context, server.url, { mode, hash: '#shared' });
                await sleep(4000);
                const vinfo = mode === 'websockets' ? await waitWsVideo(vpage, 12) : await waitWrVideo(vpage, 40);
                res.check('roles: shared viewer gets video', vinfo !== null, vinfo);
                if (mode === 'websockets') {
                    // Audio rides the primary connection, shared viewers included; the
                    // worklet's polled depth proves packets reach playback. The capture
                    // sends nothing while the desktop is silent, so the viewer is given
                    // a tone to hear.
                    const tone = pulseSine();
                    try {
                        let depth = 0;
                        for (const until = Date.now() + 10_000; Date.now() < until;) {
                            depth = await within(vpage.evaluate(() => window.currentAudioBufferSize || 0));
                            if (depth > 0) break;
                            await sleep(500);
                        }
                        res.check('roles: shared viewer plays audio', depth > 0, depth);
                    } finally {
                        pulseUnload(tone);
                    }
                }
                if (!wayland) {
                    await vpage.mouse.move(400, 300);
                    await sleep(500);
                    const at = await host.pointer() ?? [-1, -1];
                    res.check('roles: viewer input gated (pointer unmoved)',
                        !(Math.abs(at[0] - 400) <= 4 && Math.abs(at[1] - 300) <= 4), at);
                }
                await vpage.close();

                const dpage = await newPage(context, server.url, { mode, hash: '#display2-right' });
                await sleep(6000);
                if (mode === 'websockets') {
                    let texts: string[] = [];
                    for (const until = Date.now() + 10_000; Date.now() < until;) {
                        texts = await within(page.evaluate(() => window.__wsTexts ?? []));
                        if (texts.some((t) => t.includes('display2'))) break;
                        await sleep(500);
                    }
                    res.check('second display: config update reached primary page',
                        texts.some((t) => t.includes('display2')), JSON.stringify(texts).slice(0, 90));
                } else {
                    // The data channel carries the same message; the server-side media
                    // graph is what the page consumes, so it is verified at the source.
                    res.check('second display: media graph started (server)', await host.saw("display 'display2'", 10), '');
                }
                const dinfo = mode === 'websockets' ? await waitWsVideo(dpage, 12) : await waitWrVideo(dpage, 40);
                res.check('second display: video flows', dinfo !== null, dinfo);
                if (mode === 'webrtc') {
                    // displayPosition reaches the server through SETTINGS passthrough. A
                    // secondary moved below the primary makes an X root taller and
                    // narrower; a Wayland secondary is recreated at the left edge.
                    const beside = await host.rootSize() ?? [0, 0];
                    await within(dpage.evaluate(() => {
                        window.postMessage({ type: 'settings', settings: { displayPosition: 'down' } }, window.location.origin);
                    }));
                    let movedDown: boolean;
                    let detail: string;
                    if (wayland) {
                        movedDown = await host.saw('moves to +0+', 8);
                        detail = 'log check';
                    } else {
                        let stacked: [number, number] | null = null;
                        for (const until = Date.now() + 8000; Date.now() < until && stacked === null;) {
                            const size = await host.rootSize();
                            if (size && size[1] > beside[1] && size[0] < beside[0]) stacked = size;
                            else await sleep(500);
                        }
                        movedDown = stacked !== null;
                        detail = `${JSON.stringify(beside)} -> ${JSON.stringify(stacked ?? await host.rootSize())}`;
                    }
                    res.check('A9: secondary reposition applied', movedDown, detail);
                }
                await dpage.close();
                await sleep(2000);

                const real = realConsole(consoleErrors, notFound);
                res.check('no console errors (filtered)', real.errors.length === 0, real.errors.join('; ').slice(0, 200));
                res.check('no unexpected 404s', real.notFound.length === 0, real.notFound.slice(0, 2));
            } finally {
                await within(browser.close(), 'the browser').catch(() => undefined);
                await obs?.release();
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
