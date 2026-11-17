/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The ICE topologies end to end: a browser connects through each of them.
//
// Each block starts a WebRTC server with one topology setting and drives
// Chromium, Firefox, and WebKit through Playwright, reading what the browser's
// own RTCPeerConnection.getStats() says about the nominated candidate pair.
// The port range confines the server's candidate to its window, yields the one
// port left of a window whose others are held, and carries a second display in
// a window with two ports to spare; the UDP mux puts every session, a
// controller and a shared viewer at once, on the one port; the TCP mux carries
// a session whose UDP candidates were stripped from the offer, as a network
// that blocks UDP would, while an unstripped session still prefers UDP;
// ICE-lite makes the browser take the controlling role of an a=ice-lite offer;
// and the combined block runs all of them together, on the Wayland backend too
// since the transport owes the capture nothing. Over every path video decodes
// (WebKit's decoder permitting) and, on X11, a key typed on the page, carried
// by the data channel on the same pair, reaches the X keymap.
import { createSocket, type Socket as UdpSocket } from 'node:dgram';
import { createServer, type Server as TcpServer } from 'node:net';
import { networkInterfaces } from 'node:os';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { engineLaunch, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWrVideo, within } from '../lib/stream.ts';

const ENGINES: readonly Engine[] = ['chromium', 'firefox', 'webkit'];
const KEY_X = 0x78;

// Keeps every RTCPeerConnection reachable, records the remote SDP, and with
// window.__stripUdp set withholds UDP candidates in both directions, the
// offer's from the browser and the browser's own from the trickle, the way a
// network that blocks UDP leaves both agents: with only the offer stripped,
// the server, as the controlling agent, still checks the browser's UDP
// candidates and Chromium nominates the peer-reflexive UDP pair it discovers.
const PAGE_JS = String.raw`
  (() => {
    window.__pcs = [];
    window.__remoteSdp = '';
    const strip = !!window.__stripUdp;
    const Orig = window.RTCPeerConnection;
    const Wrapped = function(...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    Wrapped.prototype = Orig.prototype;
    Object.setPrototypeOf(Wrapped, Orig);
    window.RTCPeerConnection = Wrapped;
    const setRemote = Orig.prototype.setRemoteDescription;
    Orig.prototype.setRemoteDescription = function(desc) {
      let sdp = desc && desc.sdp ? desc.sdp : '';
      window.__remoteSdp = sdp;
      if (strip && sdp) {
        sdp = sdp.split('\r\n').filter(l => !(l.startsWith('a=candidate:') && / udp /i.test(l))).join('\r\n');
        desc = {type: desc.type, sdp};
      }
      return setRemote.call(this, desc);
    };
    const addCandidate = Orig.prototype.addIceCandidate;
    Orig.prototype.addIceCandidate = function(c) {
      if (strip && c && c.candidate && / udp /i.test(c.candidate)) return Promise.resolve();
      return addCandidate.call(this, c);
    };
    const handler = Object.getOwnPropertyDescriptor(Orig.prototype, 'onicecandidate');
    Object.defineProperty(Orig.prototype, 'onicecandidate', {
      configurable: true,
      get() { return handler.get.call(this); },
      set(fn) {
        handler.set.call(this, (e) => {
          if (strip && e.candidate && / udp /i.test(e.candidate.candidate || '')) return;
          return fn(e);
        });
      },
    });
  })();
`;

const STATS_JS = `(async () => {
  const out = [];
  for (const pc of window.__pcs) {
    const rep = await pc.getStats();
    const byId = {};
    rep.forEach(r => byId[r.id] = r);
    let selected = null, iceRole = null;
    rep.forEach(r => {
      if (r.type !== 'transport') return;
      if (r.selectedCandidatePairId) selected = r.selectedCandidatePairId;
      if (r.iceRole) iceRole = r.iceRole;
    });
    const pairs = [];
    rep.forEach(r => {
      if (r.type !== 'candidate-pair') return;
      if (r.id !== selected && !(r.nominated && r.state === 'succeeded')) return;
      const l = byId[r.localCandidateId] || {}, rm = byId[r.remoteCandidateId] || {};
      pairs.push({selected: r.id === selected, state: r.state, bytesReceived: r.bytesReceived || 0,
                  localType: l.candidateType, localProtocol: l.protocol,
                  remoteType: rm.candidateType, remoteProtocol: rm.protocol,
                  remoteAddress: rm.address || rm.ip || '', remotePort: rm.port, remoteTcpType: rm.tcpType});
    });
    out.push({connection: pc.connectionState, iceRole, pairs, remoteSdp: window.__remoteSdp});
  }
  return out;
})()`;

const VIDEO_RTP_JS = `(async () => {
  const out = {received: 0, decoded: 0, codec: null};
  for (const pc of window.__pcs || []) {
    const rep = await pc.getStats();
    const byId = {};
    rep.forEach(r => byId[r.id] = r);
    rep.forEach(r => {
      if (r.type === 'inbound-rtp' && (r.kind === 'video' || r.mediaType === 'video')) {
        out.received += r.packetsReceived || 0;
        out.decoded += r.framesDecoded || 0;
        const c = byId[r.codecId];
        if (c) out.codec = c.mimeType;
      }
    });
  }
  return out;
})()`;

interface Pair {
    remoteProtocol: string | null;
    remotePort: number | null;
    remoteAddress: string;
    remoteTcpType: string | null;
}

interface Stat {
    connection: string;
    iceRole: string | null;
    pairs: Pair[];
    remoteSdp: string;
}

/** The [family, address] pairs a mux binds: the host's own, loopback and IPv6 link-local left out; loopback stands in on a host with no other. */
function muxAddresses(): [4 | 6, string][] {
    const out: [4 | 6, string][] = [];
    for (const adapter of Object.values(networkInterfaces())) {
        for (const a of adapter ?? []) {
            if (a.family === 'IPv4' && a.address !== '127.0.0.1') out.push([4, a.address]);
            else if (a.family === 'IPv6' && a.address !== '::1' && a.scopeid === 0) out.push([6, a.address]);
        }
    }
    return out.length ? out : [[4, '127.0.0.1']];
}

type Held = UdpSocket | TcpServer;

/** A socket of `kind` bound to `address`:`port` (0 for any), or null where that is taken. */
function bind(kind: 'udp' | 'tcp', family: 4 | 6, address: string, port: number): Promise<[Held, number] | null> {
    return new Promise((resolve) => {
        if (kind === 'udp') {
            const s = createSocket(family === 6 ? 'udp6' : 'udp4');
            s.once('error', () => { s.close(); resolve(null); });
            s.bind({ address, port, exclusive: true }, () => { resolve([s, s.address().port]); });
        } else {
            const s = createServer();
            s.once('error', () => { resolve(null); });
            s.listen({ host: address, port, exclusive: true }, () => {
                const got = s.address();
                resolve([s, got !== null && typeof got === 'object' ? got.port : 0]);
            });
        }
    });
}

const release = (held: Held[]): void => {
    for (const h of held) {
        if ('send' in h) h.close();
        else h.close();
    }
};

/**
 * A port number free for every socket kind on every address a mux binds. A
 * mux binds one socket per host address rather than a wildcard, and the
 * addresses leave loopback out, so a number probed on loopback -- or over IPv4
 * alone -- says nothing about the addresses it will take. Every address is held
 * at once here, so the number is free across all of them together.
 */
async function freePort(...kinds: ('udp' | 'tcp')[]): Promise<number> {
    const addresses = muxAddresses();
    for (let attempt = 0; attempt < 50; attempt++) {
        const held: Held[] = [];
        let port = 0;
        let ok = true;
        try {
            for (const kind of kinds) {
                for (const [family, address] of addresses) {
                    const got = await bind(kind, family, address, port);
                    if (!got) {
                        ok = false;
                        break;
                    }
                    held.push(got[0]);
                    port = port || got[1];
                }
                if (!ok) break;
            }
            if (ok) return port;
        } finally {
            release(held);
        }
    }
    throw new Error('no port free on every mux address');
}

/**
 * A window of `held` + `free` consecutive UDP ports, its lowest `held` bound.
 * Every port is bound on every host address first, so a window another
 * process sits in is found here rather than mid-check, and the high ones are
 * then released: a gather has to retry past what stays held to reach them.
 * @returns The sockets holding the low ports, the window, and the free ports.
 */
async function portWindow(held: number, free: number): Promise<[Held[], [number, number], number[]]> {
    const addresses = muxAddresses();
    for (let attempt = 0; attempt < 20; attempt++) {
        const base = Math.min(await freePort('udp'), 65536 - held - free);
        const bound: Held[] = [];
        const spare: Held[] = [];
        let ok = true;
        for (let offset = 0; offset < held + free && ok; offset++) {
            for (const [family, address] of addresses) {
                const got = await bind('udp', family, address, base + offset);
                if (!got) {
                    ok = false;
                    break;
                }
                (offset < held ? bound : spare).push(got[0]);
            }
        }
        release(spare);
        if (!ok) {
            release(bound);
            continue;
        }
        return [bound, [base, base + held + free - 1], Array.from({ length: free }, (_, i) => base + held + i)];
    }
    throw new Error(`no window of ${held + free} free UDP ports on every address`);
}

interface Opened {
    browser: Browser | null;
    ctx: BrowserContext;
    page: Page;
    errors: string[];
}

/** A page of `engine` on the server, instrumented. */
async function openEngine(url: string, engine: Engine, stripUdp = false, hash = ''): Promise<Opened> {
    const [browser, ctx] = await engineLaunch(engine);
    await ctx.addInitScript({ content: "window.__SELKIES_STREAMING_MODE__ = 'webrtc';" });
    if (stripUdp) await ctx.addInitScript({ content: 'window.__stripUdp = true;' });
    await ctx.addInitScript({ content: PAGE_JS });
    const pages = ctx.pages();
    const page = engine === 'firefox' && pages.length ? pages[0] : await ctx.newPage();
    const errors: string[] = [];
    page.on('pageerror', (e) => errors.push(String(e)));
    await page.goto(`${url}/${hash}`, { waitUntil: 'load' });
    return { browser, ctx, page, errors };
}

async function closeEngine({ browser, ctx }: Opened): Promise<void> {
    try {
        await ctx.close();
    } finally {
        if (browser) await browser.close();
    }
}

/** The nominated pairs of every peer connection on the page, once one exists. */
async function nominatedPairs(page: Page, timeoutS = 30): Promise<[Stat[], Pair[]]> {
    let stats: Stat[] = [];
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        stats = (await within(page.evaluate<Stat[] | null>(STATS_JS))) ?? [];
        if (stats.some((s) => s.pairs.length)) break;
        await sleep(500);
    }
    return [stats, stats.flatMap((s) => s.pairs)];
}

async function videoCheck(res: Checks, tag: string, page: Page, engine: Engine): Promise<void> {
    const info = await waitWrVideo(page, 45);
    if (info === null && engine === 'webkit') {
        const rtp = await within(page.evaluate<{ received: number; decoded: number; codec: string | null }>(VIDEO_RTP_JS));
        if (rtp.received > 0 && rtp.decoded === 0) {
            res.skip(`${tag}: video decodes`, `WebKit received ${rtp.received} video packets (${rtp.codec}) but decoded none: no H.264 decoder in this build`);
            return;
        }
        res.check(`${tag}: video decodes`, false, rtp);
        return;
    }
    res.check(`${tag}: video decodes`, info !== null, info);
}

async function inputCheck(res: Checks, host: Host, tag: string, page: Page): Promise<void> {
    await page.mouse.click(640, 360);
    await sleep(400);
    let pressed: boolean | null = null;
    for (let i = 0; i < 3; i++) {
        await page.keyboard.down('x');
        await sleep(800);
        pressed = await host.keyHeld(KEY_X);
        await page.keyboard.up('x');
        await sleep(400);
        if (pressed) break;
    }
    res.check(`${tag}: a typed key reaches the X keymap over the pair`, pressed === true, pressed);
    res.check(`${tag}: and its release`, (await host.keyHeld(KEY_X)) === false);
}

interface Want {
    port?: number;
    window?: [number, number];
    lite?: boolean;
}

async function pairChecks(res: Checks, tag: string, page: Page, protocol: string, want: Want, engine: Engine = 'chromium'): Promise<Pair[]> {
    const [stats, pairs] = await nominatedPairs(page);
    res.check(`${tag}: a nominated pair exists`, pairs.length > 0, stats);
    if (pairs.length) {
        const pair = pairs[0];
        res.check(`${tag}: the server's candidate is ${protocol}`, (pair.remoteProtocol ?? '').toLowerCase() === protocol, pair);
        if (want.port !== undefined) res.check(`${tag}: on port ${want.port}`, Number(pair.remotePort ?? 0) === want.port, pair);
        if (want.window !== undefined) {
            const got = Number(pair.remotePort ?? 0);
            res.check(`${tag}: inside the window ${want.window[0]}-${want.window[1]}`, want.window[0] <= got && got <= want.window[1], pair);
        }
    }
    if (want.lite !== undefined && stats.length) {
        const sdp = stats[0].remoteSdp;
        res.check(`${tag}: the offer ${want.lite ? 'says' : 'does not say'} a=ice-lite`, sdp.includes('a=ice-lite') === want.lite);
        if (want.lite && engine === 'chromium') res.check(`${tag}: the browser took the controlling role`, stats[0].iceRole === 'controlling', stats[0].iceRole);
    }
    return pairs;
}

async function sessionChecks(res: Checks, host: Host, tag: string, page: Page, engine: Engine, protocol: string, want: Want = {}, wayland = false): Promise<Pair[]> {
    await videoCheck(res, tag, page, engine);
    const pairs = await pairChecks(res, tag, page, protocol, want, engine);
    if (!wayland) await inputCheck(res, host, tag, page);
    return pairs;
}

const noErrors = (res: Checks, tag: string, errors: string[]): void => { res.check(`${tag}: no page errors`, errors.length === 0, errors); };

async function portrangeBlock(host: Host): Promise<Checks> {
    const res = new Checks('ice-portrange');
    const low = Math.min(await freePort('udp'), 65535 - 40);
    let window: [number, number] = [low, low + 40];
    let { url } = await host.start({ label: 'portrange', mode: 'webrtc', env: { SELKIES_WEBRTC_PORT_RANGE: `${window[0]}-${window[1]}` } });
    try {
        for (const engine of ENGINES) {
            const o = await openEngine(url, engine);
            try {
                await sessionChecks(res, host, engine, o.page, engine, 'udp', { window });
                noErrors(res, engine, o.errors);
            } finally {
                await closeEngine(o);
            }
        }
    } finally {
        await host.stop();
    }

    const first = await portWindow(15, 1);
    let held = first[0];
    window = first[1];
    const free = first[2];
    let tag = 'a window with one port left';
    try {
        ({ url } = await host.start({ label: 'portrange-one-left', mode: 'webrtc', env: { SELKIES_WEBRTC_PORT_RANGE: `${window[0]}-${window[1]}` } }));
        try {
            const o = await openEngine(url, 'chromium');
            try {
                await sessionChecks(res, host, tag, o.page, 'chromium', 'udp', { port: free[0] });
                noErrors(res, tag, o.errors);
            } finally {
                await closeEngine(o);
            }
        } finally {
            await host.stop();
        }
    } finally {
        release(held);
    }

    // Two displays in a window with two ports to spare: the second display's
    // gather reaches one only past the held ports and the first display's.
    const second = await portWindow(8, 2);
    held = second[0];
    window = second[1];
    tag = 'a window with two ports to spare';
    try {
        ({ url } = await host.start({ label: 'portrange-two-spare', mode: 'webrtc', env: { SELKIES_WEBRTC_PORT_RANGE: `${window[0]}-${window[1]}` } }));
        try {
            const opened: [string, Opened][] = [];
            try {
                for (const [label, hash] of [['primary', ''], ['second display', '#display2-right']] as const) opened.push([label, await openEngine(url, 'chromium', false, hash)]);
                const sockets: string[] = [];
                for (const [label, o] of opened) {
                    const where = `${tag}: ${label}`;
                    await videoCheck(res, where, o.page, 'chromium');
                    const pairs = await pairChecks(res, where, o.page, 'udp', { window });
                    sockets.push(...pairs.slice(0, 1).map((p) => `${p.remoteAddress}:${Number(p.remotePort ?? 0)}`));
                    noErrors(res, where, o.errors);
                }
                res.check(`${tag}: the displays hold a socket each`, new Set(sockets).size === 2, sockets);
            } finally {
                for (const [, o] of opened) await closeEngine(o);
            }
        } finally {
            await host.stop();
        }
    } finally {
        release(held);
    }
    return res;
}

async function udpmuxBlock(host: Host): Promise<Checks> {
    const res = new Checks('ice-udpmux');
    const port = await freePort('udp');
    const { url } = await host.start({ label: 'udpmux', mode: 'webrtc', env: { SELKIES_WEBRTC_UDP_MUX_PORT: String(port) } });
    try {
        res.check('the server announced the shared UDP port', await host.saw(`WebRTC UDP mux: every session's host candidates share UDP port ${port}`, 5));
        for (const engine of ENGINES) {
            const o = await openEngine(url, engine);
            try {
                await sessionChecks(res, host, engine, o.page, engine, 'udp', { port });
                if (engine === 'chromium') {
                    const viewer = await o.ctx.newPage();
                    await viewer.goto(`${url}/#shared`, { waitUntil: 'load' });
                    await videoCheck(res, 'chromium viewer', viewer, engine);
                    await pairChecks(res, 'chromium viewer', viewer, 'udp', { port });
                    const [again] = await nominatedPairs(o.page, 5);
                    res.check("the controller's session stays up beside the viewer's on the one port", again.length > 0 && again[0].connection === 'connected', again);
                    await viewer.close();
                }
                noErrors(res, engine, o.errors);
            } finally {
                await closeEngine(o);
            }
        }
    } finally {
        await host.stop();
    }
    return res;
}

async function tcpmuxBlock(host: Host): Promise<Checks> {
    const res = new Checks('ice-tcpmux');
    const port = await freePort('tcp');
    const { url } = await host.start({ label: 'tcpmux', mode: 'webrtc', env: { SELKIES_WEBRTC_TCP_MUX_PORT: String(port) } });
    try {
        res.check('the server announced the ICE-TCP port', await host.saw(`WebRTC TCP mux: ICE-TCP accepted on TCP port ${port}`, 5));
        let o = await openEngine(url, 'chromium');
        try {
            const [stats] = await nominatedPairs(o.page);
            const sdp = stats.length ? stats[0].remoteSdp : '';
            const lines = sdp.split(/\r?\n/).filter((l) => l.startsWith('a=candidate:'));
            const passive = lines.filter((l) => l.includes(' tcp ') && l.includes('tcptype passive'));
            res.check('the offer carries a passive TCP candidate on the port beside the UDP ones',
                passive.length > 0 && passive.every((l) => l.split(/\s+/)[5] === String(port)) && lines.some((l) => l.includes(' udp ')), passive.length ? passive : sdp);
            await sessionChecks(res, host, 'chromium with UDP available', o.page, 'chromium', 'udp');
        } finally {
            await closeEngine(o);
        }
        for (const engine of ENGINES) {
            const tag = `${engine} with UDP stripped`;
            o = await openEngine(url, engine, true);
            try {
                const pairs = await sessionChecks(res, host, tag, o.page, engine, 'tcp', { port });
                if (pairs.length && engine === 'chromium') res.check(`${tag}: the server's candidate is the passive one`, pairs[0].remoteTcpType === 'passive', pairs[0]);
                noErrors(res, tag, o.errors);
            } finally {
                await closeEngine(o);
            }
        }
    } finally {
        await host.stop();
    }
    return res;
}

async function iceliteBlock(host: Host): Promise<Checks> {
    const res = new Checks('ice-lite');
    const { url } = await host.start({ label: 'icelite', mode: 'webrtc', env: { SELKIES_WEBRTC_ICE_LITE: 'true' } });
    try {
        res.check('the server announced ICE-lite', await host.saw('WebRTC ICE-lite', 5));
        for (const engine of ENGINES) {
            const o = await openEngine(url, engine);
            try {
                await sessionChecks(res, host, engine, o.page, engine, 'udp', { lite: true });
                noErrors(res, engine, o.errors);
            } finally {
                await closeEngine(o);
            }
        }
    } finally {
        await host.stop();
    }
    return res;
}

async function combinedBlock(host: Host, wayland = false): Promise<Checks> {
    const res = new Checks(wayland ? 'ice-combined-wl' : 'ice-combined');
    const port = await freePort('udp', 'tcp');
    const { url } = await host.start({ label: wayland ? 'combined-wl' : 'combined', mode: 'webrtc', wayland, env: {
        SELKIES_WEBRTC_UDP_MUX_PORT: String(port), SELKIES_WEBRTC_TCP_MUX_PORT: String(port),
        SELKIES_WEBRTC_ICE_LITE: 'true', SELKIES_WEBRTC_PORT_RANGE: '50000-50100',
    } });
    try {
        res.check('a port range beside the UDP mux is warned about at startup', await host.saw('webrtc_port_range is unused while webrtc_udp_mux_port is set', 5));
        for (const [engine, strip] of [['chromium', false], ['chromium', true], ['firefox', true], ['webkit', false]] as const) {
            const tag = `${engine} with UDP ${strip ? 'stripped' : 'available'}`;
            const o = await openEngine(url, engine, strip);
            try {
                await sessionChecks(res, host, tag, o.page, engine, strip ? 'tcp' : 'udp', { port, lite: true }, wayland);
                noErrors(res, tag, o.errors);
            } finally {
                await closeEngine(o);
            }
        }
    } finally {
        await host.stop();
    }
    return res;
}

const BLOCKS: Record<string, (host: Host) => Promise<Checks>> = {
    portrange: portrangeBlock,
    udpmux: udpmuxBlock,
    tcpmux: tcpmuxBlock,
    icelite: iceliteBlock,
    combined: (h) => combinedBlock(h),
    'combined-wl': (h) => combinedBlock(h, true),
};

for (const [name, block] of Object.entries(BLOCKS)) {
    test(name, async () => {
        const host = new Host(suiteOf(import.meta.filename));
        let res: Checks;
        try {
            res = await block(host);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
