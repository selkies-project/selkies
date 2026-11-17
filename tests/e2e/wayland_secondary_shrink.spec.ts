/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A left-hand secondary that shrinks keeps the second screen (Wayland).
//
// Each display is a screen of the session compositor's own, and the
// compositor refuses to move an output into room a live one still holds. A
// secondary placed left of the primary sits at the origin, so when its
// rectangle shrinks -- a page whose density changed, a smaller browser window
// -- the primary has to move into the room that output gives up: the layout
// pass must shrink the output first and grow it back only on the capture
// start that follows a move the other way, or the second screen is dropped on
// every shrink. Driven against the in-process pixelflux compositor on both
// transports: raw WebSocket clients on the websockets transport, and two
// Chromium pages whose secondary viewport shrinks on the WebRTC transport,
// where the page's own resize request carries the new rectangle.
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { launchClient, newPage } from '../lib/page.ts';
import { waitWrVideo } from '../lib/stream.ts';
import { RawWs } from '../lib/wsclient.ts';

const settings = (displayId: string, width: number, height: number, position = 'left'): string => `SETTINGS,${JSON.stringify({
    displayId, initialClientWidth: width, initialClientHeight: height, manual_resolution: false, framerate: 30, encoder: 'jpeg',
    video_crf: 25, video_bitrate: 6000, audio_bitrate: 128000, scaling_dpi: 96, displayPosition: position,
})}`;

type Rect = { x: number; y: number; w: number; h: number };

/** Every layout the server calculated in `log`, by display id. */
function layoutsIn(log: string): Record<string, Rect>[] {
    const out: Record<string, Rect>[] = [];
    for (const line of log.split('\n')) {
        if (!line.includes('Layout calculated') || !line.includes('Layouts: ')) continue;
        try {
            const parsed: unknown = JSON.parse(line.split('Layouts: ', 2)[1].trim().replace(/'/g, '"'));
            if (parsed !== null && typeof parsed === 'object') {
                const rects: Record<string, Rect> = {};
                for (const [id, r] of Object.entries(parsed)) {
                    if (r !== null && typeof r === 'object') rects[id] = { x: Number(Reflect.get(r, 'x')), y: Number(Reflect.get(r, 'y')), w: Number(Reflect.get(r, 'w')), h: Number(Reflect.get(r, 'h')) };
                }
                out.push(rects);
            }
        } catch {
            // not a layout literal
        }
    }
    return out;
}

/** Text messages a socket receives within `seconds`; a socket the server closed yields what it got and a final KILL marker. */
async function drain(ws: RawWs, seconds: number): Promise<string[]> {
    const got: string[] = [];
    for (const until = Date.now() + seconds * 1000; Date.now() < until;) {
        const msg = await ws.next(Math.max(1, until - Date.now()));
        if (msg === null) {
            if (ws.closed) {
                got.push(`KILL socket closed: ${ws.closeCode ?? 'no close frame'}`);
                break;
            }
            continue;
        }
        if (typeof msg === 'string') got.push(msg);
    }
    return got;
}

/**
 * Wait for any of `needles` past `mark`, reading `sockets` meanwhile: the
 * streams they carry would otherwise pile up unread. Whether one showed up,
 * and the text messages read, with a KILL marker for a socket the server
 * closed.
 */
async function waitDraining(host: Host, mark: number, needles: readonly string[], sockets: readonly RawWs[], timeoutS = 20): Promise<[boolean, string[]]> {
    const got: string[] = [];
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        const log = await host.log(mark);
        if (needles.some((n) => log.includes(n))) return [true, got];
        for (const ws of sockets) got.push(...(await drain(ws, 0.2)));
        await sleep(50);
    }
    return [false, got];
}

const r = (v: unknown): string => JSON.stringify(v);

/** Ask the secondary for a new size and check the pass that realizes it. */
async function resizeSecondary(host: Host, res: Checks, primary: RawWs, secondary: RawWs, width: number, height: number, primaryX: number, label: string): Promise<void> {
    const mark = await host.mark();
    if (secondary.closed) {
        res.check(`${label}: the secondary's socket is still open for its resize`, false, `closed ${secondary.closeCode ?? ''}`);
        return;
    }
    secondary.send(`r,${width}x${height},display2`);
    // A kept capture follows the layout live; one the pass rebuilt starts again.
    const [followed, msgs] = await waitDraining(host, mark, ["Capture 'display2' followed the new layout live", "Capture started for 'display2'"], [primary, secondary], 45);
    res.check(`${label}: the secondary's capture follows the new size`, followed, '');
    await drain(primary, 1.0);
    const secondaryMsgs = [...msgs.filter((m) => !m.startsWith('PIPELINE_RESETTING')), ...(await drain(secondary, 2.0))];
    const tail = await host.log(mark);
    const lines = tail.split('\n');
    res.check(`${label}: the compositor refused no output move or creation`,
        !tail.includes('RepositionOutput 0: rejected') && !tail.includes('CreateOutput 2: rejected') && !tail.includes('cannot create an output') && !tail.includes('cannot move the primary'),
        lines.filter((l) => l.includes('rejected')).slice(0, 3));
    res.check(`${label}: the secondary was not dropped`, !tail.includes('dropped on Wayland') && !secondaryMsgs.some((m) => m.startsWith('KILL')), secondaryMsgs.slice(0, 3));
    res.check(`${label}: the secondary output was kept rather than recreated`, !tail.includes('Output 2 created') && !tail.includes('recreating it'), lines.filter((l) => l.includes('Output 2')).slice(0, 4));
    const outputs = lines.filter((l) => l.includes('[Wayland] Output') || l.includes('Configuring Output'));
    res.check(`${label}: the compositor's outputs hold the new rectangles`,
        outputs.some((l) => l.includes(`${width}x${height}`) && l.includes('Output 2')) && outputs.some((l) => l.includes(`repositioned to (${primaryX}, 0)`)), outputs.slice(0, 6));
    const after = layoutsIn(tail).at(-1) ?? {};
    res.check(`${label}: the layout carries the new secondary size and the moved primary`, after.display2?.w === width && after.primary?.x === primaryX, after);
}

async function drive(host: Host, res: Checks, port: number): Promise<void> {
    const primary = await RawWs.open({ port }, '/api/websockets');
    try {
        await primary.next(10_000);
        let mark = await host.mark();
        primary.send(settings('primary', 1920, 1080));
        res.check('primary capture starts', await host.saw("Capture started for 'primary'", 45, mark), '');
        await drain(primary, 1.0);
        const secondary = await RawWs.open({ port }, '/api/websockets');
        try {
            await secondary.next(10_000);
            mark = await host.mark();
            secondary.send(settings('display2', 1280, 720));
            const [started] = await waitDraining(host, mark, ["Capture started for 'display2'"], [primary, secondary], 45);
            res.check('secondary capture starts left of the primary', started, '');
            const before = layoutsIn(await host.log(mark)).at(-1) ?? {};
            res.check("the primary is laid out at the secondary's right edge", before.primary?.x === 1280 && before.display2?.x === 0, before);
            await drain(primary, 1.0);
            await drain(secondary, 1.0);

            await resizeSecondary(host, res, primary, secondary, 960, 540, 960, 'shrink');
            await resizeSecondary(host, res, primary, secondary, 1280, 720, 1280, 'grow back');
            await resizeSecondary(host, res, primary, secondary, 1024, 800, 1024, 'narrower and taller');

            primary.send('STOP_VIDEO');
            secondary.send('STOP_VIDEO');
            await sleep(500);
        } finally {
            await secondary.close();
        }
    } finally {
        await primary.close();
    }
}

/** Two Chromium pages: the secondary, left of the primary, shrinks its viewport and the page asks the server for the smaller stream itself. */
async function driveWebrtc(host: Host, res: Checks, url: string): Promise<void> {
    const compositorLines = async (mark: number): Promise<string[]> => (await host.log(mark)).split('\n').filter((l) => l.includes('[Wayland] Output') || l.includes('Configuring Output') || l.includes('rejected'));
    const client = await launchClient(url, { mode: 'webrtc' });
    try {
        res.check('primary video flows', Boolean(await waitWrVideo(client.page, 45)), '');
        let mark = await host.mark();
        const dpage = await newPage(client.context, url, { mode: 'webrtc', hash: '#display2-left' });
        const video = await waitWrVideo(dpage, 60);
        res.check('secondary video flows left of the primary', Boolean(video), video);
        res.check('the primary moved right of the secondary', await host.saw('Output 0 repositioned to (1280, 0)', 20, mark), (await compositorLines(mark)).slice(0, 6));
        for (const [label, w, h, primaryX] of [['shrink', 960, 540, 960], ['grow back', 1280, 720, 1280], ['narrower and taller', 1024, 800, 1024]] as const) {
            mark = await host.mark();
            await dpage.setViewportSize({ width: w, height: h });
            const moved = await host.saw(`Output 0 repositioned to (${primaryX}, 0)`, 30, mark);
            const lines = await compositorLines(mark);
            res.check(`${label}: the primary moved to the secondary's new edge`, moved, lines.slice(0, 6));
            res.check(`${label}: the compositor refused no output move or creation`, !lines.some((l) => l.includes('rejected')), lines.slice(0, 6));
            res.check(`${label}: the secondary output was kept rather than recreated`, !lines.some((l) => l.includes('Output 2 created') || l.includes('destroyed')), lines.slice(0, 6));
            res.check(`${label}: the secondary was not dropped`, !(await host.log(mark)).includes('dropped on Wayland'), '');
            let sized = false;
            let info: unknown = null;
            for (const until = Date.now() + 30_000; Date.now() < until && !sized;) {
                const got = await waitWrVideo(dpage, 5);
                info = got ?? {};
                sized = got?.w === w;
            }
            res.check(`${label}: the secondary's stream comes at the new size`, sized, r(info));
        }
        await dpage.close();
    } finally {
        await client.browser.close();
    }
}

for (const which of ['websockets', 'webrtc']) {
    test(which, async () => {
        const res = new Checks(`wl-secondary-shrink-${which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const { url, port } = await host.start({ label: which, mode: which, wayland: true });
            if (which === 'webrtc') await driveWebrtc(host, res, url);
            else await drive(host, res, port);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
