/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The websockets transport served from a Unix domain socket alone.
//
// With unix_socket set and no port named, the server binds that path instead
// of the TCP addr/port pair, so everything -- the static client, the API, and
// the data WebSocket that carries the stream -- has to work over AF_UNIX. A
// stale socket file left at the path by a dead server must be cleared at start
// (a live one must not be), and the server must remove its own file on
// shutdown.
//
// Checked two ways: a raw client on the socket itself (HTTP over AF_UNIX for
// the API and the client files, a WebSocket over AF_UNIX that completes the
// handshake, sends its settings, and receives video frames), and a browser
// through a TCP-to-Unix forwarder, standing in for the reverse proxy such a
// deployment puts in front of the socket.
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, openSync, readFileSync, writeFileSync } from 'node:fs';
import { request } from 'node:http';
import { connect, createServer, type AddressInfo, type Socket } from 'node:net';
import { join } from 'node:path';

import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { launchClient } from '../lib/page.ts';
import { realConsole, waitWsVideo, within } from '../lib/stream.ts';

const SOCK = join(WORKDIR, 'selkies-ws.sock');

declare global {
    interface Window {
        __wsFrames?: number;
    }
}

/** [status, body] of a GET over the Unix socket; status -1 when it is not answering. */
function unixGet(path: string): Promise<[number, Buffer]> {
    return new Promise((resolve) => {
        const req = request({ socketPath: SOCK, path, host: 'localhost', timeout: 10_000 }, (r) => {
            const chunks: Buffer[] = [];
            r.on('data', (c: Buffer) => chunks.push(c));
            r.on('end', () => { resolve([r.statusCode ?? -1, Buffer.concat(chunks)]); });
        });
        req.on('error', () => { resolve([-1, Buffer.alloc(0)]); });
        req.on('timeout', () => { req.destroy(); });
        req.end();
    });
}

/** A masked client frame of `opcode` carrying `payload`. */
function frame(opcode: number, payload: Buffer): Buffer {
    const mask = randomBytes(4);
    const n = payload.length;
    const head = n < 126 ? Buffer.from([0x80 | opcode, 0x80 | n]) : Buffer.from([0x80 | opcode, 0x80 | 126, n >> 8, n & 0xff]);
    const body = Buffer.from(payload);
    for (let i = 0; i < body.length; i++) body[i] ^= mask[i % 4];
    return Buffer.concat([head, mask, body]);
}

interface Streamed {
    mode: string | null;
    video_frames: number;
    error: string | null;
}

/**
 * Complete the data-WebSocket handshake over the socket, send a primary
 * display's settings, and count the video frames that come back.
 */
function streamOverUnix(seconds = 8): Promise<Streamed> {
    const out: Streamed = { mode: null, video_frames: 0, error: null };
    const settings = {
        displayId: 'primary', initialClientWidth: 1280, initialClientHeight: 720, manual_resolution: false, framerate: 60, encoder: 'h264enc',
        video_crf: 25, video_bitrate: 6000, audio_bitrate: 128000, scaling_dpi: 96, displayPosition: 'right',
    };
    return new Promise((resolve) => {
        const s: Socket = connect(SOCK);
        let buf = Buffer.alloc(0);
        let upgraded = false;
        let done = false;
        let deadline = 0;
        const finish = (error?: string): void => {
            if (done) return;
            done = true;
            if (error) out.error = error.slice(0, 120);
            clearInterval(tick);
            s.destroy();
            resolve(out);
        };
        const tick = setInterval(() => {
            if ((deadline && Date.now() > deadline) || (!deadline && out.mode === null && Date.now() > started + 10_000)) finish(out.mode === null ? 'no first message' : undefined);
        }, 250);
        const started = Date.now();
        s.on('connect', () => {
            s.write(`GET /api/websockets HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: ${randomBytes(16).toString('base64')}\r\nSec-WebSocket-Version: 13\r\n\r\n`);
        });
        s.on('data', (data: Buffer) => {
            buf = Buffer.concat([buf, data]);
            if (!upgraded) {
                const end = buf.indexOf('\r\n\r\n');
                if (end < 0) return;
                const status = buf.subarray(0, end).toString('latin1').split('\r\n')[0];
                if (!status.startsWith('HTTP/1.1 101')) {
                    finish(`handshake: ${status}`);
                    return;
                }
                upgraded = true;
                buf = buf.subarray(end + 4);
            }
            while (buf.length >= 2) {
                const opcode = buf[0] & 0x0f;
                let size = buf[1] & 0x7f;
                let at = 2;
                if (size === 126) {
                    if (buf.length < 4) break;
                    size = buf.readUInt16BE(2);
                    at = 4;
                } else if (size === 127) {
                    if (buf.length < 10) break;
                    size = Number(buf.readBigUInt64BE(2));
                    at = 10;
                }
                if (buf.length < at + size) break;
                const payload = buf.subarray(at, at + size);
                buf = buf.subarray(at + size);
                if (out.mode === null && (opcode === 1 || opcode === 2)) {
                    out.mode = opcode === 1 ? payload.toString('utf8') : JSON.stringify([...payload.subarray(0, 20)]);
                    s.write(frame(1, Buffer.from(`SETTINGS,${JSON.stringify(settings)}`)));
                    deadline = Date.now() + seconds * 1000;
                } else if (opcode === 2 && payload.length && payload[0] === 0x04) {
                    out.video_frames++;
                    if (out.video_frames >= 30) {
                        finish();
                        return;
                    }
                } else if (opcode === 9) {
                    s.write(frame(10, payload));
                } else if (opcode === 8) {
                    finish('closed');
                    return;
                }
            }
        });
        s.on('error', (e) => { finish(String(e)); });
        s.on('close', () => { finish(); });
    });
}

/** Whether nothing listens on `port` of the loopback address. */
function tcpRefused(port: number): Promise<boolean> {
    return new Promise((resolve) => {
        const s = connect(port, '127.0.0.1');
        s.setTimeout(2000, () => { s.destroy(); resolve(true); });
        s.on('connect', () => { s.destroy(); resolve(false); });
        s.on('error', () => { resolve(true); });
    });
}

test('ws_unix_socket', async () => {
    const res = new Checks('ws-unix-socket');
    const host = new Host(suiteOf(import.meta.filename));
    // Every accepted connection piped onto the socket, both ways, each side ending the other.
    const proxy = createServer((client) => {
        const upstream = connect(SOCK);
        client.pipe(upstream).on('error', () => { client.destroy(); });
        upstream.pipe(client).on('error', () => { upstream.destroy(); });
        client.on('error', () => { upstream.destroy(); });
        upstream.on('error', () => { client.destroy(); });
    });
    try {
        await host.staleSocket(SOCK);
        let pid: number;
        try {
            // Turbo, so the still test display keeps sending the frames counted.
            pid = (await host.start({ label: 'ws-unix', mode: 'websockets', socket: SOCK, env: { SELKIES_VIDEO_STREAMING_MODE: 'true' } })).pid;
        } catch (e) {
            res.check('a dead socket file at the path is cleared and the server starts', false, String(e));
            res.finish();
            return;
        }
        res.check('a dead socket file at the path is cleared and the server starts', true);
        res.check('the server announces the unix endpoint', await host.saw(`Selkies server running on unix://${SOCK}`, 10), '');
        const listeners = await host.listeners(pid);
        res.check('no TCP listener on the port', listeners.length === 0, listeners.slice(0, 1));

        let [status, body] = await unixGet('/api/status');
        let mode: unknown = null;
        if (status === 200) {
            const parsed: unknown = JSON.parse(body.toString() || '{}');
            mode = parsed !== null && typeof parsed === 'object' ? Reflect.get(parsed, 'current_mode') : null;
        }
        res.check('api/status answers over the socket', status === 200 && mode === 'websockets', [status, mode]);
        res.check('api/health answers over the socket', (await unixGet('/api/health'))[0] === 200);
        [status, body] = await unixGet('/');
        res.check('the client is served over the socket', status === 200 && body.toString().toLowerCase().includes('<html'), status);

        const got = await streamOverUnix();
        res.check('data WebSocket handshake completes over the socket', got.mode?.startsWith('MODE') ?? false, got);
        res.check('video frames flow over the socket', got.video_frames >= 30, got);
        res.check('capture started for the raw client', await host.saw("Capture started for 'primary'", 10), '');

        await new Promise<void>((done) => { proxy.listen(0, '127.0.0.1', done); });
        const address: AddressInfo | string | null = proxy.address();
        const port = address !== null && typeof address === 'object' ? address.port : 0;
        for (const until = Date.now() + 10_000; Date.now() < until && (await tcpRefused(port));) await sleep(250);
        const { browser, page, consoleErrors, notFound } = await launchClient(`http://127.0.0.1:${port}`, { mode: 'websockets' });
        try {
            const info = await waitWsVideo(page, 30);
            res.check('browser through the forwarder: video canvas painted', info !== null, info);
            let frames = 0;
            for (const until = Date.now() + 10_000; Date.now() < until && frames < 24;) {
                frames = await within(page.evaluate(() => window.__wsFrames ?? 0));
                await sleep(500);
            }
            res.check('browser through the forwarder: frames flowing', frames >= 24, frames);
            const real = realConsole(consoleErrors, notFound).errors;
            res.check('browser console clean', real.length === 0, JSON.stringify(real.slice(0, 2)));
        } finally {
            await browser.close();
        }

        // A live socket is never unlinked from under its owner: a second
        // server on the same path refuses to start, and the first keeps
        // serving. It runs as the first was started: its environment and
        // its arguments, which name the socket.
        const environ = readFileSync(`/proc/${pid}/environ`, 'utf8').split('\0').filter(Boolean);
        const args = readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0').filter(Boolean).slice(1);
        const env = Object.fromEntries(environ.map((kv) => [kv.slice(0, kv.indexOf('=')), kv.slice(kv.indexOf('=') + 1)]));
        const secondLog = join(WORKDIR, 'selkies-ws-unix.log.second');
        writeFileSync(secondLog, '');
        const out = openSync(secondLog, 'a');
        const second = spawn(await host.serverBinary(), args, { env, cwd: WORKDIR, stdio: ['ignore', out, out], detached: true });
        const code = await new Promise<number | null>((resolve) => {
            const timer = setTimeout(() => { second.kill('SIGTERM'); resolve(null); }, 60_000);
            second.on('exit', (c) => { clearTimeout(timer); resolve(c); });
        });
        const secondSaid = readFileSync(secondLog, 'utf8');
        res.check('a second server on the live socket refuses to start', code !== null && code !== 0 && secondSaid.includes('already listening'), code);
        res.check('the first server still answers after the refused start', (await unixGet('/api/status'))[0] === 200);
        await host.stop();
        res.check('the socket file is removed on shutdown', !existsSync(SOCK), SOCK);
    } finally {
        proxy.close();
        await host.close();
    }
    res.finish();
});
