/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// End-to-end regression checks: the X11/Wayland x WebSockets/WebRTC matrix
// streaming to a real browser, plus three regression-prone paths: Prometheus
// collector re-registration on a mode switch, the WebRTC pacer draining
// large packets, and multipart clipboard reassembly after a client cleanup.
import { spawn } from 'node:child_process';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host, suitesBin } from '../lib/host.ts';
import { launchClient, serverClipboardPushed } from '../lib/page.ts';
import { closeBrowser, pageFps, realConsole, waitWrVideo, waitWsVideo } from '../lib/stream.ts';

/**
 * Wait for video on `page` as its transport carries it.
 * @param page The client.
 * @param mode The transport.
 * @returns The video's size, or null.
 */
function waitVideo(page: Page, mode: string): ReturnType<typeof waitWsVideo> {
    return mode === 'webrtc' ? waitWrVideo(page) : waitWsVideo(page);
}

/**
 * Poll until the page's clipboard state carries the pushed text.
 * @param page The client.
 * @param text The text.
 * @param timeoutS How long to wait.
 * @returns Whether it came.
 */
async function waitClip(page: Page, text: string, timeoutS = 30): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (await serverClipboardPushed(page, text)) return true;
        await sleep(500);
    }
    return false;
}

/**
 * GET or POST against the server, its status and body; a failed request is
 * status 0 with the error as its body.
 * @param url The server.
 * @param path The route.
 * @param body JSON to POST.
 * @returns Status and body text.
 */
async function call(url: string, path: string, body?: unknown): Promise<[number, string]> {
    try {
        const res = await fetch(url + path, {
            method: body === undefined ? 'GET' : 'POST',
            ...(body === undefined ? {} : { headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }),
            signal: AbortSignal.timeout(10_000),
        });
        return [res.status, await res.text()];
    } catch (e) {
        return [0, String(e)];
    }
}

/** The last `n` lines of the log. */
async function tail(host: Host, n: number): Promise<string> {
    return (await host.log()).split('\n').filter(Boolean).slice(-n).join('\n');
}

/** Every backend x transport combination streams video to a real browser. */
async function blockMatrix(r: Checks, host: Host): Promise<void> {
    for (const wayland of [false, true]) {
        for (const mode of ['websockets', 'webrtc']) {
            const name = `${wayland ? 'wayland' : 'x11'}/${mode}`;
            let url: string;
            try {
                url = (await host.start({ label: `matrix-${wayland ? 'wl' : 'x11'}-${mode}`, mode, wayland })).url;
            } catch (e) {
                r.check(`${name} server up`, false, String(e));
                continue;
            }
            r.check(`${name} server up`, true);
            try {
                const { browser, page, consoleErrors, notFound } = await launchClient(url, { mode });
                try {
                    const ok = await waitVideo(page, mode);
                    r.check(`${name} video flowing`, ok, ok);
                    const real = realConsole(consoleErrors, notFound);
                    r.check(`${name} console clean`, !real.errors.length && !real.notFound.length,
                        [real.errors.slice(0, 2), real.notFound.slice(0, 2)]);
                } finally {
                    await browser.close();
                }
            } catch (e) {
                r.check(`${name} client`, false, String(e));
            } finally {
                await host.stop();
            }
        }
    }
}

/**
 * Switching transports twice must not trip DuplicateTimeseries: every
 * collector registered has to be released, or the second entry into a mode
 * raises at the metrics' construction and the service dies before it serves.
 */
async function blockSwitch(r: Checks, host: Host): Promise<void> {
    const { url } = await host.start({ label: 'switch', mode: 'websockets', env: { SELKIES_ENABLE_METRICS_HTTP: 'true' } });
    try {
        // The Prometheus registry is served from the streaming port; there is no
        // second listener to point a scraper at.
        const [st, body] = await call(url, '/api/metrics');
        r.check('metrics served on the streaming port', st === 200 && body.includes('fps'), `${st} ${body.slice(0, 80)}`);
        for (const target of ['webrtc', 'websockets', 'webrtc']) {
            const [accepted, error] = await call(url, '/api/switch', { mode: target });
            if (accepted === 0) {
                r.check(`switch -> ${target} accepted`, false, error);
                break;
            }
            r.check(`switch -> ${target} accepted`, accepted === 200 || accepted === 202, accepted);
            let got: string | null = null;
            for (const until = Date.now() + 30_000; Date.now() < until;) {
                const [code, status] = await call(url, '/api/status');
                if (code === 200 && status.includes(target)) {
                    got = target;
                    break;
                }
                await sleep(500);
            }
            r.check(`mode is ${target} after switch`, got === target, await tail(host, 4));
        }
        const log = await host.log();
        r.check('no DuplicateTimeseries', !/Duplicated timeseries/i.test(log),
            log.split('\n').filter((l) => l.includes('Duplicated')).join('\n').slice(0, 200));
        r.check('no unexpected service termination', !/Service terminated unexpectedly/i.test(log));
    } finally {
        await host.stop();
    }
}

/**
 * The server's clipboard survives a client cleanup and reconnect: the
 * multipart assembler is built by a factory, and a cleanup that replaces it
 * with a bare literal strips its methods and drops every later transfer.
 */
async function blockClipboard(r: Checks, host: Host): Promise<void> {
    const payload = `selkies-clip-${'y'.repeat(200_000)}`;
    for (const mode of ['websockets', 'webrtc']) {
        const { url } = await host.start({ label: `clipboard-${mode}`, mode });
        try {
            const owner = await host.ownClipboard(payload);
            try {
                const { browser, page } = await launchClient(url, { mode });
                try {
                    r.check(`${mode}: first connect streams`, Boolean(await waitVideo(page, mode)));
                    r.check(`${mode}: multipart clipboard arrives`, await waitClip(page, payload));
                    await page.reload({ waitUntil: 'load' });
                    r.check(`${mode}: reconnect streams`, Boolean(await waitVideo(page, mode)));
                    r.check(`${mode}: multipart clipboard arrives after reconnect`, await waitClip(page, payload));
                } finally {
                    await browser.close();
                }
            } finally {
                await owner.release();
            }
        } finally {
            await host.stop();
        }
    }
}

/**
 * The WebRTC pacer must attach and keep draining, not wedge the stream. A
 * moving scene keeps frames due the whole block: a still screen sends none
 * once its paint-over ends, which would read as a wedged pacer.
 */
async function blockPacer(r: Checks, host: Host): Promise<void> {
    const { url } = await host.start({ label: 'pacer', mode: 'webrtc', env: { SELKIES_WEBRTC_PACER: 'true' } });
    const display = process.env.E2E_DISPLAY ?? '';
    const painter = spawn(suitesBin('motion-scene'), [display, '1280', '720', '60'], { stdio: 'ignore' });
    try {
        const { browser, page } = await launchClient(url, { mode: 'webrtc' });
        try {
            r.check('webrtc video flowing', Boolean(await waitWrVideo(page)));
            r.check('pacer enabled', await host.saw('pacer enabled', 25), await tail(host, 3));
            // A wedged pacer stops the stream: frames must still climb after the
            // queue has had time to fill.
            await sleep(6000);
            const fps = await pageFps(page);
            r.check('stream still alive with pacer', Boolean(fps), fps);
            r.check('no pacer queue overflow purge storm', !/pacer.*purge.*purge.*purge/i.test(await host.log()));
        } finally {
            await closeBrowser(browser);
        }
    } finally {
        painter.kill('SIGKILL');
        await host.stop();
    }
}

const BLOCKS: readonly [string, (r: Checks, host: Host) => Promise<void>][] = [
    ['matrix', blockMatrix],
    ['switch', blockSwitch],
    ['clipboard', blockClipboard],
    ['pacer', blockPacer],
];

for (const [name, block] of BLOCKS) {
    test(name, async () => {
        process.stdout.write(`=== ${name} ===\n`);
        const res = new Checks(name);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await block(res, host);
        } catch (e) {
            res.check('block completed', false, String(e));
        } finally {
            await host.close();
        }
        res.finish();
    });
}
