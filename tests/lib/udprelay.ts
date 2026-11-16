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
export class Shaper {
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
export async function makeRelay(dest: [string, number], up: Shaper, down: Shaper, host = '127.0.0.1'): Promise<Relay> {
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
