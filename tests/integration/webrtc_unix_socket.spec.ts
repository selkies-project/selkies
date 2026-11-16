/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// WebRTC media and the data channel over a server on a Unix socket alone:
//
//   selkies --mode webrtc --unix-socket <workdir>/selkies-webrtc.sock   (no TCP listener)
//   a TCP-to-Unix proxy here, standing in for a reverse proxy (HTTP only)
//   Chrome connects to the proxy's port and does WebRTC: signaling through
//   the proxy, media over the server's own UDP ICE sockets.
//
// The server is given no port, as under jupyter-server-proxy, so nothing
// listens on TCP and its signaling peer has to reach the endpoint through
// the socket as well.
import { existsSync } from 'node:fs';
import { connect, createServer, type AddressInfo } from 'node:net';
import { join } from 'node:path';

import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { realConsole, waitWrVideo, within } from '../lib/stream.ts';

test('webrtc-unix', async () => {
    const sock = join(WORKDIR, 'selkies-webrtc.sock');
    const host = new Host(suiteOf(import.meta.filename));
    // Every accepted connection piped onto the socket, both ways, each side
    // ending the other.
    const proxy = createServer((client) => {
        const upstream = connect(sock);
        client.pipe(upstream).on('error', () => { client.destroy(); });
        upstream.pipe(client).on('error', () => { upstream.destroy(); });
        client.on('error', () => { upstream.destroy(); });
        upstream.on('error', () => { client.destroy(); });
    });
    const res = new Checks('webrtc-unix');
    try {
        const server = await host.start({ label: 'webrtc', mode: 'webrtc', socket: sock, env: { SELKIES_TURN_REST_URI: '' } });
        await new Promise<void>((done) => { proxy.listen(0, '127.0.0.1', done); });
        const address = proxy.address();
        const port = typeof address === 'object' && address !== null ? (address satisfies AddressInfo).port : 0;
        const listeners = await host.listeners(server.pid);
        res.check('server owns no TCP listener', listeners.length === 0, listeners.slice(0, 1));
        res.check('unix socket bound', existsSync(sock), sock);

        const browser = await launchChromium();
        const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 } });
        await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'webrtc';" });
        const page = await ctx.newPage();
        const errors: string[] = [];
        page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
        page.on('pageerror', (e) => { errors.push(String(e)); });
        await page.goto(`http://127.0.0.1:${port}`, { waitUntil: 'load' });
        const info = await waitWrVideo(page, 60);
        res.check('WebRTC video receives through the proxied signaling', info !== null, info);
        // The data channel carries the mode and stream notification once media is up.
        await sleep(2000);
        const real = realConsole(errors).errors;
        res.check('console clean', real.length === 0, real.slice(0, 2));
        await within(browser.close(), 'the browser');
    } finally {
        proxy.close();
        await host.close();
    }
    res.finish();
});
