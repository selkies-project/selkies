/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Narrow UDP links for a WebRTC page, in the suite's own process: a relay
 * per candidate of the server's, each direction through a shaper with a rate,
 * a one-way delay and a tail-drop cap, as the pacer rig's relays are.
 * @module
 */

import { createSocket, type Socket } from 'node:dgram';
import { isIPv6 } from 'node:net';

/**
 * One direction's token-bucket and FIFO virtual clock: exact departure times
 * at the rate, then the delay, with a switch-fidelity tail drop past
 * CAP_BYTES queued (no unbounded bufferbloat). With no rate, a datagram only
 * waits out the delay.
 */
export class Shaper implements Leg {
    static readonly CAP_BYTES = 250_000;
    readonly rate: number | null;
    readonly delayS: number;
    private freeAt = performance.now() / 1000;
    private queued = 0;
    dropped = 0;
    shaped = 0;

    /**
     * @param rateBps The link's rate in bits per second; null for none.
     * @param delayS Its one-way delay.
     */
    constructor(rateBps: number | null = null, delayS = 0) {
        this.rate = rateBps;
        this.delayS = delayS;
    }

    /**
     * Schedule `emit` at the datagram's shaped departure, or drop it at the cap.
     * @param size The datagram's length.
     * @param emit What sends it.
     */
    push(size: number, emit: () => void): void {
        this.shaped++;
        if (this.rate === null) {
            if (this.delayS) setTimeout(emit, this.delayS * 1000);
            else emit();
            return;
        }
        if (this.queued + size > Shaper.CAP_BYTES) {
            this.dropped++;
            return;
        }
        this.queued += size;
        const now = performance.now() / 1000;
        const depart = Math.max(now, this.freeAt) + size * 8 / this.rate;
        this.freeAt = depart;
        setTimeout(() => { this.queued -= size; emit(); }, (Math.max(depart - now, 0) + this.delayS) * 1000);
    }
}

/** One direction of a relayed link: what holds each datagram until it leaves. */
export interface Leg {
    push(size: number, emit: () => void): void;
}

/**
 * A link whose rate steps while it runs: datagrams wait in one FIFO queue,
 * tail-dropped past `Shaper.CAP_BYTES`, and leave at the rate the link has
 * when their turn comes, so a queue built while the link was narrow drains at
 * the rate it comes back with, as a bottleneck's queue does.
 */
export class SteppedShaper implements Leg {
    private rateBps: number;
    readonly delayS: number;
    private readonly queue: [number, () => void][] = [];
    private queued = 0;
    private freeAt = 0;
    /** When the datagram at the head leaves, once it has been planned. */
    private headDepart: number | null = null;
    private timer: NodeJS.Timeout | null = null;
    dropped = 0;
    shaped = 0;

    /**
     * @param rateBps The link's rate in bits per second.
     * @param delayS Its one-way delay.
     */
    constructor(rateBps: number, delayS = 0) {
        this.rateBps = rateBps;
        this.delayS = delayS;
    }

    /** The rate now, in bits per second. */
    get rate(): number {
        return this.rateBps;
    }

    /** Change the rate: what is queued leaves at the new one. */
    set rate(bps: number) {
        this.rateBps = bps;
        this.headDepart = null;
        if (this.timer !== null) {
            clearTimeout(this.timer);
            this.timer = null;
        }
        this.pump();
    }

    push(size: number, emit: () => void): void {
        this.shaped++;
        if (this.queued + size > Shaper.CAP_BYTES) {
            this.dropped++;
            return;
        }
        this.queued += size;
        this.queue.push([size, emit]);
        if (this.timer === null) this.pump();
    }

    /**
     * Send every datagram due by now (a millisecond's grace, the timers'
     * grain), then wait for the next. The head's departure is fixed when it
     * reaches the head, so the wait for it is not charged to it again.
     */
    private pump(): void {
        for (;;) {
            const now = performance.now() / 1000;
            const head = this.queue[0];
            if (!head) return;
            this.headDepart ??= Math.max(this.freeAt, now) + head[0] * 8 / this.rateBps;
            const depart = this.headDepart;
            if (depart > now + 0.001) {
                this.timer = setTimeout(() => { this.timer = null; this.pump(); }, (depart - now) * 1000);
                return;
            }
            this.queue.shift();
            this.queued -= head[0];
            this.freeAt = depart;
            this.headDepart = null;
            if (this.delayS) setTimeout(head[1], this.delayS * 1000);
            else head[1]();
        }
    }
}

/** A relay between the near peer, whose candidate it stands in for, and the far one. */
export interface Relay {
    /** The address advertised to the near side. */
    address: [string, number];
    close: () => void;
}

/**
 * A relay toward `dest`, listening on `host`: what arrives there comes from
 * the near peer and goes out toward `dest` through `up`; replies pass `down`
 * and go to the near peer's last address.
 * @param dest The far peer's address.
 * @param up The near-to-far shaper.
 * @param down The far-to-near shaper.
 * @param host The address to listen on.
 * @returns The relay, once both sockets are bound.
 */
export async function makeRelay(dest: [string, number], up: Leg, down: Leg, host = '127.0.0.1'): Promise<Relay> {
    // The far socket takes the family of the address it reaches, as the server's IPv6
    // candidates need.
    const far: Socket = createSocket(isIPv6(dest[0]) ? 'udp6' : 'udp4');
    const vis: Socket = createSocket('udp4');
    let near: { address: string; port: number } | null = null;
    await new Promise<void>((done, fail) => {
        far.once('error', fail);
        far.connect(dest[1], dest[0], () => { far.off('error', fail); done(); });
    });
    await new Promise<void>((done) => { vis.bind(0, host, done); });
    vis.on('message', (data, from) => {
        near = { address: from.address, port: from.port };
        up.push(data.length, () => { try { far.send(data); } catch { /* the far end is gone */ } });
    });
    far.on('message', (data) => {
        const to = near;
        if (to === null) return;
        down.push(data.length, () => { try { vis.send(data, to.port, to.address); } catch { /* the near end is gone */ } });
    });
    for (const socket of [far, vis]) socket.on('error', () => undefined);
    const bound = vis.address();
    return { address: [bound.address, bound.port], close: () => { far.close(); vis.close(); } };
}

/**
 * An init script putting a page behind relays the test runs: every UDP host
 * candidate of the server's becomes the relay the page's `__shapedRelay`
 * returns for it, every other is dropped, and the page's own candidates are
 * withheld, so the server meets the page as a peer-reflexive candidate behind
 * a relay. The page's connections are kept in `__pcs`.
 */
export const SHAPED_JS = `
  (() => {
    const Orig = window.RTCPeerConnection;
    window.__pcs = [];
    const Wrapped = function(...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    Wrapped.prototype = Orig.prototype;
    Object.setPrototypeOf(Wrapped, Orig);
    window.RTCPeerConnection = Wrapped;
    const CAND = /^(a=)?candidate:(\\S+) (\\d+) (udp) (\\d+) (\\S+) (\\d+) typ host(.*)$/i;
    const shaped = async (line) => {
      const m = line.match(CAND);
      if (!m) return null;
      const [host, port] = await window.__shapedRelay(m[6], Number(m[7]));
      return \`\${m[1] || ''}candidate:\${m[2]} \${m[3]} \${m[4]} \${m[5]} \${host} \${port} typ host\${m[8]}\`;
    };
    const setRemote = Orig.prototype.setRemoteDescription;
    Orig.prototype.setRemoteDescription = async function(desc) {
      if (desc && desc.sdp) {
        const lines = [];
        for (const l of desc.sdp.split('\\r\\n')) {
          if (!l.startsWith('a=candidate:')) { lines.push(l); continue; }
          const r = await shaped(l);
          if (r) lines.push(r);
        }
        desc = {type: desc.type, sdp: lines.join('\\r\\n')};
      }
      return setRemote.call(this, desc);
    };
    const addCandidate = Orig.prototype.addIceCandidate;
    Orig.prototype.addIceCandidate = async function(c) {
      if (!c || !c.candidate) return addCandidate.call(this, c);
      const r = await shaped(c.candidate);
      if (!r) return;
      return addCandidate.call(this, {candidate: r, sdpMid: c.sdpMid, sdpMLineIndex: c.sdpMLineIndex});
    };
    const handler = Object.getOwnPropertyDescriptor(Orig.prototype, 'onicecandidate');
    Object.defineProperty(Orig.prototype, 'onicecandidate', {
      configurable: true,
      get() { return handler.get.call(this); },
      set(fn) { handler.set.call(this, (e) => (e.candidate ? undefined : fn(e))); },
    });
  })();
`;
