/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The headless WebRTC client of the crate (src/bin/webrtc-peer.rs) behind a
// shaped link: a session of it, and what it reports as it arrives.
import { spawn, type ChildProcess } from 'node:child_process';
import { createInterface } from 'node:readline';

import { fields, numberOr, parseJson, stringOr } from '../../web/selkies-web-core/lib/narrow.ts';
import { sleep } from './env.ts';
import { suitesBin } from './host.ts';

/** One direction of the link, as the peer takes it; `{}` passes datagrams through. */
export interface LinkShape {
    /** A token bucket of this rate, its queue tail-dropped past 250 kB. */
    rate_bps?: number | null;
    delay_s?: number;
    /** A share of datagrams lost, drawn from a stream seeded by `seed`. */
    loss?: number;
    seed?: number;
    /** `[period_s, length_s]`: nothing gets through for `length_s` of every `period_s`. */
    outage?: readonly [number, number];
}

/** An RTP packet as it arrived, before any repair or reordering. */
export interface RtpArrival {
    /** Seconds on the peer's clock. */
    t: number;
    media: string;
    ssrc: number;
    seq: number;
    ts: number;
    /** The sequence number a retransmission repairs; null on media. */
    osn: number | null;
    /** The first four bytes of the media it carries. */
    head: Buffer;
    len: number;
    /** The one-way delay its abs-send-time gives, in seconds. */
    ow: number | null;
}

/** A text data channel message. */
export interface ChannelMessage {
    t: number;
    /** The system clock in seconds. */
    wall: number;
    label: string;
    text: string;
}

/** What a shaper did over the session. */
export interface ShaperCount {
    shaped: number;
    lost: number;
    dropped: number;
}

/** A running peer and everything it has reported. */
export class Peer {
    readonly rtp: RtpArrival[] = [];
    readonly messages: ChannelMessage[] = [];
    /** Each NACK the client sent (the packets it names) and each picture request, by time. */
    readonly nacks: { t: number; n: number }[] = [];
    readonly plis: number[] = [];
    readonly states: string[] = [];
    /** Retransmission SSRC to the SSRC it repairs, and the FEC SSRCs, of what the server sends. */
    rtx = new Map<number, number>();
    fec = new Set<number>();
    readonly shapers = new Map<string, ShaperCount>();
    videoSeen = 0;
    audioSeen = 0;
    /** Whether `stop` was called. */
    ended = false;
    private readonly child: ChildProcess;
    private readonly exited: Promise<void>;
    private readonly drained: Promise<void>;
    private readonly onMessage: ((message: ChannelMessage) => void) | undefined;

    /**
     * @param url The server's signaling socket.
     * @param up The client-to-server direction.
     * @param down The server-to-client direction.
     * @param options `env` over the inherited environment; `onMessage` sees every channel message.
     */
    constructor(url: string, up: LinkShape, down: LinkShape, options: { env?: Record<string, string>; onMessage?: (message: ChannelMessage) => void } = {}) {
        this.onMessage = options.onMessage;
        this.child = spawn(suitesBin('webrtc-peer'), [url, JSON.stringify(up), JSON.stringify(down)], {
            stdio: ['pipe', 'pipe', 'inherit'],
            env: { ...process.env, ...options.env },
        });
        this.exited = new Promise((resolve) => {
            this.child.once('exit', () => { resolve(); });
        });
        const stdout = this.child.stdout;
        this.drained = stdout
            ? new Promise((resolve) => {
                const lines = createInterface({ input: stdout });
                lines.on('line', (line) => { this.take(line); });
                lines.once('close', () => { resolve(); });
            })
            : Promise.resolve();
    }

    private take(line: string): void {
        const e = fields(parseJson(line));
        const t = numberOr(e.t) ?? 0;
        switch (e.kind) {
            case 'rtp': {
                const media = stringOr(e.media) ?? '';
                if (media === 'video') this.videoSeen += 1;
                if (media === 'audio') this.audioSeen += 1;
                this.rtp.push({
                    t, media,
                    ssrc: numberOr(e.ssrc) ?? 0,
                    seq: numberOr(e.seq) ?? 0,
                    ts: numberOr(e.ts) ?? 0,
                    osn: numberOr(e.osn) ?? null,
                    head: Buffer.from(stringOr(e.head) ?? '', 'hex'),
                    len: numberOr(e.len) ?? 0,
                    ow: numberOr(e.ow) ?? null,
                });
                break;
            }
            case 'dc': {
                const message = { t, wall: numberOr(e.wall) ?? 0, label: stringOr(e.label) ?? '', text: stringOr(e.text) ?? '' };
                this.messages.push(message);
                this.onMessage?.(message);
                break;
            }
            case 'fb':
                if (e.type === 'nack') this.nacks.push({ t, n: numberOr(e.n) ?? 0 });
                else if (e.type === 'pli') this.plis.push(t);
                break;
            case 'state':
                this.states.push(stringOr(e.state) ?? '');
                break;
            case 'streams':
                this.rtx = new Map(Object.entries(fields(e.rtx)).map(([repair, primary]) => [Number(repair), numberOr(primary) ?? 0]));
                this.fec = new Set(Array.isArray(e.fec) ? e.fec.map((s) => numberOr(s) ?? 0) : []);
                break;
            case 'shaper': {
                const dir = stringOr(e.dir);
                if (dir) this.shapers.set(dir, { shaped: numberOr(e.shaped) ?? 0, lost: numberOr(e.lost) ?? 0, dropped: numberOr(e.dropped) ?? 0 });
                break;
            }
            default:
                break;
        }
    }

    /** Set a rate: of both directions, or of one. `null` unshapes it. */
    setRate(rateBps: number | null, direction?: 'up' | 'down'): void {
        const line = direction ? { [direction]: { rate_bps: rateBps } } : { rate_bps: rateBps };
        this.child.stdin?.write(`${JSON.stringify(line)}\n`);
    }

    /** End the session and wait for the peer's last report (its shapers' counts) to be read. */
    async stop(): Promise<void> {
        if (this.ended) return;
        this.ended = true;
        this.child.stdin?.end();
        const timeout = sleep(10_000).then(() => { this.child.kill('SIGKILL'); });
        await Promise.race([this.exited, timeout]);
        await Promise.all([this.exited, this.drained]);
    }
}

/** The packets of a session's window, and the peer for what it counted over the whole session. */
export interface Session {
    peer: Peer;
    /** The RTP arrivals and channel messages of the measured window. */
    rtp: RtpArrival[];
    messages: ChannelMessage[];
}

/**
 * One session as the pacer suites measure it: wait up to `warmupS` for both
 * media to arrive, two seconds more, then a window of `measureS`.
 * @param url The server's signaling socket.
 * @param up The client-to-server direction.
 * @param down The server-to-client direction.
 * @param measureS The window's length.
 * @param warmupS The longest wait for media before it.
 * @param options As the {@link Peer} takes them; `alongside`, started with the peer and awaited
 * once it has ended (it watches `peer.ended`); `during`, run while the window is open.
 * @returns The window, once the peer has ended.
 */
export async function runSession(
    url: string, up: LinkShape, down: LinkShape, measureS: number, warmupS: number,
    options: {
        env?: Record<string, string>;
        onMessage?: (message: ChannelMessage) => void;
        alongside?: (peer: Peer) => Promise<void>;
        during?: (peer: Peer) => Promise<void>;
    } = {},
): Promise<Session> {
    const peer = new Peer(url, up, down, options);
    const alongside = options.alongside?.(peer);
    try {
        const t0 = performance.now();
        while (performance.now() - t0 < warmupS * 1000) {
            await sleep(500);
            if (peer.videoSeen > 0 && peer.audioSeen > 0) break;
        }
        await sleep(2000);
        const rtpMark = peer.rtp.length;
        const messageMark = peer.messages.length;
        await Promise.all([sleep(measureS * 1000), options.during?.(peer)]);
        const rtp = peer.rtp.slice(rtpMark);
        const messages = peer.messages.slice(messageMark);
        return { peer, rtp, messages };
    } finally {
        await peer.stop();
        await alongside;
    }
}
