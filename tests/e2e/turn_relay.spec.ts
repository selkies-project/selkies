/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The TURN relay path: a WebRTC client that is only allowed relay candidates
// still gets the stream, through a TURN server the selkies settings named.
//
// A coturn turnserver is started on loopback with a shared secret; the server
// is configured with turn_host/turn_port/turn_shared_secret (and the same host
// as its STUN server) so /api/turn hands the page HMAC credentials for it, and
// the page is pinned to iceTransportPolicy "relay" through the client's own
// turn_switch store key. The checks read RTCPeerConnection.getStats(): the
// selected candidate pair's local candidate is a relay on the coturn relay
// address, its byte count grows while video decodes, and input typed on the
// page (carried by the data channel over that same pair) reaches the X keymap.
//
// Without a turnserver binary the suite skips with the command that enables
// it: install coturn (apt install coturn, which provides /usr/bin/turnserver)
// or point E2E_TURNSERVER at a turnserver binary.
import { spawn, type ChildProcess } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { createSocket } from 'node:dgram';
import { accessSync, constants, openSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { onPath } from '../lib/desktop.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { realConsole, waitWrVideo, within } from '../lib/stream.ts';

const SECRET = randomBytes(16).toString('hex');
const REALM = 'selkies.test';
// Relay allocations come out of this port range; narrow so two runs on one
// host can only collide with each other, never with the servers under test.
const RELAY_PORTS = [45000, 45199] as const;
const KEY_X = 0x78;

// Pins the client to relay candidates (the wr core reads turn_switch from its
// localStorage namespace) and keeps every RTCPeerConnection reachable.
const PAGE_JS = `
  (() => {
    const app = (location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_');
    try { localStorage.setItem(app + '_turn_switch', 'true'); } catch (e) {}
    window.__pcs = [];
    const Orig = window.RTCPeerConnection;
    const Wrapped = function(...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    Wrapped.prototype = Orig.prototype;
    Object.setPrototypeOf(Wrapped, Orig);
    window.RTCPeerConnection = Wrapped;
  })();
`;

const STATS_JS = `(async () => {
  const out = [];
  for (const pc of window.__pcs) {
    const rep = await pc.getStats();
    const byId = {};
    rep.forEach(r => byId[r.id] = r);
    let selected = null;
    rep.forEach(r => { if (r.type === 'transport' && r.selectedCandidatePairId) selected = r.selectedCandidatePairId; });
    const pairs = [];
    rep.forEach(r => {
      if (r.type !== 'candidate-pair') return;
      if (r.id !== selected && !(r.nominated && r.state === 'succeeded')) return;
      const l = byId[r.localCandidateId] || {}, rm = byId[r.remoteCandidateId] || {};
      pairs.push({selected: r.id === selected, state: r.state, bytesReceived: r.bytesReceived || 0,
                  local: l.candidateType, localAddress: (l.address || l.ip) + ':' + l.port,
                  relayProtocol: l.relayProtocol, remote: rm.candidateType});
    });
    out.push({connection: pc.connectionState, policy: pc.getConfiguration().iceTransportPolicy, pairs});
  }
  return out;
})()`;

interface Pair {
    selected: boolean;
    bytesReceived: number;
    local: string;
    localAddress: string;
}

interface Stat {
    policy: string;
    pairs: Pair[];
}

/**
 * A runnable turnserver: E2E_TURNSERVER when it names an executable file, else
 * one found on PATH. A non-runnable override is treated as absent so the suite
 * skips with the install hint rather than crashing on exec.
 */
function turnserverBinary(): string | null {
    const override = process.env.E2E_TURNSERVER;
    if (override) {
        try {
            accessSync(override, constants.X_OK);
            return statSync(override).isFile() ? override : null;
        } catch {
            return null;
        }
    }
    return onPath('turnserver');
}

function freeUdpPort(): Promise<number> {
    return new Promise((resolve) => {
        const probe = createSocket('udp4');
        probe.bind(0, '127.0.0.1', () => {
            const { port } = probe.address();
            probe.close(() => { resolve(port); });
        });
    });
}

/** Whether a STUN binding request to `port` is answered within half a second. */
function stunAnswers(port: number): Promise<boolean> {
    return new Promise((resolve) => {
        const probe = createSocket('udp4');
        const done = (ok: boolean): void => { clearTimeout(timer); probe.close(); resolve(ok); };
        const timer = setTimeout(() => { done(false); }, 500);
        probe.on('message', (data) => { done(data[0] === 0x01 && data[1] === 0x01); });
        probe.on('error', () => { done(false); });
        probe.send(Buffer.concat([Buffer.from('000100002112a442', 'hex'), randomBytes(12)]), port, '127.0.0.1');
    });
}

/**
 * coturn on loopback with the shared-secret (HMAC) credential scheme.
 * Loopback peers are allowed because both ends of this run live on 127.0.0.1,
 * which coturn refuses to relay to by default; TLS, the CLI port, and the TCP
 * relay are off so nothing but the one UDP listener binds.
 */
async function startTurnserver(binary: string, port: number): Promise<ChildProcess> {
    const log = join(WORKDIR, 'turnserver.log');
    const out = openSync(log, 'w');
    const proc = spawn(binary, ['-n', '-v', '--listening-ip=127.0.0.1', '--relay-ip=127.0.0.1',
        `--listening-port=${port}`, `--min-port=${RELAY_PORTS[0]}`, `--max-port=${RELAY_PORTS[1]}`,
        `--realm=${REALM}`, '--use-auth-secret', `--static-auth-secret=${SECRET}`,
        '--no-tls', '--no-dtls', '--no-cli', '--no-tcp-relay', '--no-multicast-peers',
        '--allow-loopback-peers', '--fingerprint', '--relay-threads=1',
        '--log-file=stdout', '--simple-log',
        `--pidfile=${join(WORKDIR, 'turnserver.pid')}`, `--userdb=${join(WORKDIR, 'turnserver.db')}`], { stdio: ['ignore', out, out] });
    // A STUN binding answered on the port is the readiness signal.
    for (const until = Date.now() + 15_000; Date.now() < until;) {
        if (proc.exitCode !== null) throw new Error(`turnserver exited ${proc.exitCode}; see ${log}`);
        if (await stunAnswers(port)) return proc;
        await sleep(300);
    }
    proc.kill('SIGTERM');
    throw new Error(`turnserver did not answer STUN on 127.0.0.1:${port}; see ${log}`);
}

async function stopTurnserver(proc: ChildProcess): Promise<void> {
    if (proc.exitCode !== null || proc.signalCode !== null) return;
    const exited = new Promise<void>((resolve) => { proc.on('exit', () => { resolve(); }); });
    proc.kill('SIGTERM');
    const timer = setTimeout(() => proc.kill('SIGKILL'), 10_000);
    await exited;
    clearTimeout(timer);
}

const stats = (page: Page): Promise<Stat[]> => within(page.evaluate<Stat[]>(STATS_JS));

/** The selected pairs across every connection (the nominated, succeeded ones where the engine reports no selection). */
function relayPairs(all: Stat[]): Pair[] {
    const pairs = all.flatMap((s) => s.pairs);
    const selected = pairs.filter((p) => p.selected);
    return selected.length ? selected : pairs;
}

const receivedBytes = async (page: Page): Promise<number> => relayPairs(await stats(page)).reduce((a, p) => a + p.bytesReceived, 0);

test('turn_relay', async () => {
    const res = new Checks('turn-relay');
    const binary = turnserverBinary();
    if (!binary) {
        res.skip('turn relay', 'no turnserver binary; install coturn (apt install coturn) or set E2E_TURNSERVER=/path/to/turnserver');
        res.finish();
        return;
    }
    const host = new Host(suiteOf(import.meta.filename));
    const port = await freeUdpPort();
    const turn = await startTurnserver(binary, port);
    try {
        const { url } = await host.start({ label: 'turn', mode: 'webrtc', env: {
            SELKIES_TURN_HOST: '127.0.0.1', SELKIES_TURN_PORT: String(port), SELKIES_TURN_SHARED_SECRET: SECRET,
            SELKIES_TURN_PROTOCOL: 'udp', SELKIES_TURN_TLS: 'false', SELKIES_STUN_HOST: '127.0.0.1', SELKIES_STUN_PORT: String(port),
            // The JSON file and a REST endpoint outrank the shared secret when
            // present; neither may reach this run (E2E_TURN_REST_URI included).
            SELKIES_RTC_CONFIG_JSON: join(WORKDIR, 'no-rtc-config.json'), SELKIES_TURN_REST_URI: '',
        } });
        res.check('server: HMAC TURN credentials source selected', await host.saw('Using short-term shared secret HMAC for TURN credentials', 5));
        const reply = await fetch(`${url}/api/turn`);
        const config: unknown = reply.status === 200 ? await reply.json() : {};
        const servers: unknown = config !== null && typeof config === 'object' ? Reflect.get(config, 'iceServers') : [];
        const entries = (Array.isArray(servers) ? servers : []).filter((s: unknown) => {
            const urls: unknown = s !== null && typeof s === 'object' ? Reflect.get(s, 'urls') : [];
            return Array.isArray(urls) && urls.some((u) => typeof u === 'string' && u.startsWith('turn:'));
        });
        const first: unknown = entries[0];
        const get = (k: string): unknown => {
            const v: unknown = first !== null && typeof first === 'object' ? Reflect.get(first, k) : undefined;
            return v;
        };
        const username = get('username');
        res.check('/api/turn names the loopback TURN server with HMAC credentials',
            reply.status === 200 && entries.length > 0 && JSON.stringify(get('urls')) === JSON.stringify([`turn:127.0.0.1:${port}?transport=udp`])
            && typeof username === 'string' && username.includes(':') && Boolean(get('credential')), `${reply.status} ${JSON.stringify(entries)}`);

        const browser = await launchChromium();
        try {
            const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
            await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'webrtc';" });
            await ctx.addInitScript({ content: PAGE_JS });
            const page = await ctx.newPage();
            const consoleErrors: string[] = [];
            page.on('console', (m) => { if (m.type() === 'error') consoleErrors.push(m.text()); });
            page.on('pageerror', (e) => consoleErrors.push(String(e)));
            await page.goto(`${url}/`, { waitUntil: 'load' });
            const info = await waitWrVideo(page, 60);
            res.check('video: <video> receiving with the client pinned to relay', info !== null, info);
            const all = await stats(page);
            res.check("the page's connection runs iceTransportPolicy relay", all.length > 0 && all.every((s) => s.policy === 'relay'), all.map((s) => s.policy));
            const selected = relayPairs(all);
            res.check("the selected candidate pair's local candidate is a relay", selected.length > 0 && selected.every((p) => p.local === 'relay'), selected);
            res.check('the relay address is the coturn relay on loopback', selected.length > 0 && selected.every((p) => {
                const relayPort = Number(p.localAddress.slice(p.localAddress.lastIndexOf(':') + 1));
                return p.localAddress.startsWith('127.0.0.1:') && relayPort >= RELAY_PORTS[0] && relayPort <= RELAY_PORTS[1];
            }), selected.map((p) => p.localAddress));
            // The encoder sends on damage, so an idle desktop produces no
            // media however long the wait, and moving the pointer makes none
            // of its own: the cursor rides a channel of its own rather than
            // the framebuffer. The desktop is repainted instead, and the
            // counter polled rather than sampled once, since a fixed window
            // can fall between frames.
            const before = await receivedBytes(page);
            let after = before;
            const repaint = await host.paint([], { paint: [0, 0, 640, 480, 0x3B5B1F], second: 0x76B63E, periodMs: 500 });
            let polls = 0;
            try {
                for (const until = Date.now() + 20_000; after <= before && Date.now() < until;) {
                    polls++;
                    await sleep(500);
                    after = await receivedBytes(page);
                }
            } finally {
                await repaint.release();
            }
            res.check('bytes keep arriving over the relayed pair', after > before, `${before} -> ${after} over ${polls} repaints`);
            // Input rides the data channel on the same transport.
            await page.mouse.click(640, 360);
            await sleep(400);
            await page.keyboard.down('x');
            await sleep(800);
            const pressed = await host.keyHeld(KEY_X);
            await page.keyboard.up('x');
            await sleep(300);
            res.check('input: key press through the relay reached the X keymap', pressed === true, pressed);
            res.check('input: key release through the relay', (await host.keyHeld(KEY_X)) === false);
            const turnLog = readFileSync(join(WORKDIR, 'turnserver.log'), 'utf8');
            res.check('coturn logged the allocation under the shared-secret realm', turnLog.includes('ALLOCATE') && turnLog.includes(REALM), '');
            const real = realConsole(consoleErrors).errors.filter((e) => !e.includes('Wake Lock'));
            res.check('no console errors (filtered)', real.length === 0, real.join('; ').slice(0, 160));
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
        await stopTurnserver(turn);
        await host.close();
    }
    res.finish();
});
