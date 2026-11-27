/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// A lossy, delayed, rate-limited link recovers cleanly from every cut the
// pacer makes: nothing of an abandoned GOP is delivered after the keyframe that
// replaced it, and a cut the sender resyncs its peer across instead (the
// encoder predicting past the frames it dropped) needs none.
//
// A real server in WebRTC mode streams a scene repainted every frame to the
// crate's headless client (webrtc-peer), whose every ICE candidate on both
// sides is a relay that shapes, delays, and drops packets from a seeded
// stream, over each link in LINKS. The client's NACKs cross the relay, so the
// server's retransmissions are real. Read at the client, per video packet: its
// arrival, its sequence number, whether it repairs an earlier one, and whether
// it opens a keyframe. Read from the server log: the GOP resets and resyncs,
// what each purged, and the pace at the end. The measures are the keyframes
// the client saw, the repairs of a hole's packets that arrived behind the
// keyframe closing the hole, how long a hole took to reach that keyframe, and
// the longest gap between frames, against the resets that caused them.
import { spawn } from 'node:child_process';
import { appendFileSync } from 'node:fs';
import { join } from 'node:path';

import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host, suitesBin } from '../lib/host.ts';
import { runSession, type LinkShape, type RtpArrival } from '../lib/peer.ts';

const WIDTH = 1280;
const HEIGHT = 720;
const MEASURE_S = Number(process.env.LOSS_WINDOW_S ?? '20');

/** Whether RTP sequence number `a` is after `b`, across the 16-bit wrap. */
function uint16Gt(a: number, b: number): boolean {
    return (a < b && b - a > 0x8000) || (a > b && a - b < 0x8000);
}

function uint16Gte(a: number, b: number): boolean {
    return a === b || uint16Gt(a, b);
}

/**
 * Whether an H.264 payload starts a keyframe: an SPS or IDR NAL, on its own,
 * first in an aggregate, or as the first fragment.
 */
function opensKeyframe(head: Buffer): boolean {
    if (head.length === 0) return false;
    const nal = head[0] & 0x1F;
    if (nal === 5 || nal === 7) return true;
    if (nal === 24 && head.length >= 4) return [5, 7].includes(head[3] & 0x1F);
    if (nal === 28 && head.length >= 2) return Boolean(head[1] & 0x80) && (head[1] & 0x1F) === 5;
    return false;
}

interface Seen {
    frames: number;
    keyframes: number;
    repairs: number;
    stale_repairs: number;
    stale_media: number;
    holes: number;
    recovery_ms: [number, number] | null;
    gap_p99_ms: number | null;
    gap_max_ms: number | null;
    [key: string]: unknown;
}

/**
 * What the client saw on the media stream and its repairs (FEC is left out):
 * keyframes, repairs of a hole's packets arriving behind the keyframe that
 * closed the hole, media behind a keyframe from before it, frame gaps, and how
 * long each hole took to reach its keyframe.
 */
function analyze(packets: readonly RtpArrival[], rtx: ReadonlyMap<number, number>): Seen {
    const streams = new Set([...rtx.keys(), ...rtx.values()]);
    const keyframes: [number, number][] = [];
    let staleRepairs = 0;
    let staleMedia = 0;
    let repairs = 0;
    const frames = new Map<number, number>();
    // A hole is fifty or more media packets the client never saw in sequence:
    // [time before, first missing, last missing, time to the keyframe after].
    const holes: [number, number, number, number | null][] = [];
    let last: [number, number] | null = null;
    for (const { t, ts, seq, osn, head, ssrc, media } of packets) {
        if (media !== 'video') continue;
        if (streams.size > 0 && !streams.has(ssrc)) continue;
        if (osn !== null) {
            repairs += 1;
            if (holes.some((h) => h[3] !== null && uint16Gte(osn, h[1]) && uint16Gte(h[2], osn))) staleRepairs += 1;
            continue;
        }
        if (!frames.has(ts) && opensKeyframe(head)) {
            keyframes.push([t, seq]);
            const open = holes.at(-1);
            if (open && open[3] === null) open[3] = t - open[0];
        }
        const key = keyframes.at(-1);
        if (key && key[0] < t && uint16Gt(key[1], seq)) staleMedia += 1;
        if (!frames.has(ts)) frames.set(ts, t);
        if (last !== null && uint16Gt(seq, last[1]) && ((seq - last[1]) & 0xFFFF) >= 50) {
            holes.push([last[0], (last[1] + 1) & 0xFFFF, (seq - 1) & 0xFFFF, null]);
        }
        last = [t, seq];
    }
    const starts = [...frames.values()].sort((a, b) => a - b);
    const gaps = starts.slice(1).map((b, i) => b - starts[i]).sort((a, b) => a - b);
    const recovered = holes.flatMap((h) => (h[3] === null ? [] : [1000 * h[3]])).sort((a, b) => a - b);
    const mid = recovered[Math.floor(recovered.length / 2)];
    const worst = recovered.at(-1);
    return {
        frames: frames.size, keyframes: keyframes.length, repairs,
        stale_repairs: staleRepairs, stale_media: staleMedia, holes: holes.length,
        recovery_ms: mid !== undefined && worst !== undefined ? [Math.round(mid), Math.round(worst)] : null,
        gap_p99_ms: gaps.length ? Math.round(10_000 * gaps[Math.floor(0.99 * (gaps.length - 1))]) / 10 : null,
        gap_max_ms: gaps.length ? Math.round(10_000 * gaps[gaps.length - 1]) / 10 : null,
    };
}

function serverCounts(text: string): Record<string, number | null> {
    const lines = text.split('\n');
    const resets = lines.filter((l) => l.includes('GOP reset'));
    const resyncs = lines.filter((l) => l.includes('the sender resyncs its peer'));
    let purged = 0;
    for (const line of [...resets, ...resyncs]) {
        const n = Number.parseInt((line.split('=> ')[1] ?? '').replace('GOP reset, ', '').split(' ')[0], 10);
        if (Number.isFinite(n)) purged += n;
    }
    const closed = lines.filter((l) => l.includes('pacer closed:'));
    const pace = closed.length ? Number.parseInt((closed[closed.length - 1].split("'pace_bps': ")[1] ?? '').split(',')[0], 10) : Number.NaN;
    return {
        resets: resets.length, resyncs: resyncs.length, purged, pace_bps: Number.isFinite(pace) ? pace : null,
        timeout_resurrects: text.split('resurrecting video optimistically').length - 1,
    };
}

// Links: a wide one with a half-second blackout every four seconds, the burst a
// NACK batch of hundreds of packets answers, over a trickle of loss that keeps
// repairs flowing; and a narrow one the stream cannot fit, which cuts it all
// through the window and must brake the pace to what it carries. Congestion
// control stays off, or it would fit the stream to the narrow link first.
const LINKS: Record<string, LinkShape> = {
    bursts: { rate_bps: 6e6, delay_s: 0.03, loss: 0.01, seed: 7, outage: [4.0, 0.5] },
    narrow: { rate_bps: 2.5e6, delay_s: 0.03 },
};

/** The latest live Prometheus pace, with the complete samples kept as evidence. */
class PaceObserver {
    paceBps: number | null = null;
    readonly path: string;
    private running = true;
    private readonly done: Promise<void>;

    constructor(url: string, pid: number) {
        this.path = join(WORKDIR, `pace-metrics-${pid}.jsonl`);
        this.done = this.observe(url);
    }

    private async observe(url: string): Promise<void> {
        while (this.running) {
            try {
                const response = await fetch(`${url}/api/metrics`, { signal: AbortSignal.timeout(2000) });
                const pacer = (await response.text()).split('\n').filter((l) => l.startsWith('webrtc_pacer_'));
                for (const line of pacer) {
                    if (line.startsWith('webrtc_pacer_pace_bps')) this.paceBps = Math.trunc(Number(line.slice(line.lastIndexOf(' ') + 1)));
                }
                appendFileSync(this.path, `${JSON.stringify({ time: Date.now() / 1000, metrics: pacer })}\n`);
            } catch (e) {
                appendFileSync(this.path, `${JSON.stringify({ time: Date.now() / 1000, error: e instanceof Error ? e.name : String(e) })}\n`);
            }
            await sleep(250);
        }
    }

    async stop(): Promise<void> {
        this.running = false;
        await this.done;
    }
}

async function session(host: Host, display: string, link: string): Promise<Seen> {
    const started = await host.start({
        label: link, mode: 'webrtc', display, env: {
            SELKIES_WEBRTC_PACER: 'true', SELKIES_CONGESTION_CONTROL: 'false', SELKIES_STUN_HOST: '',
            SELKIES_TURN_REST_URI: '', SELKIES_VIDEO_BITRATE: '4000', SELKIES_DEBUG: 'true',
            SELKIES_ENABLE_METRICS_HTTP: 'true',
        },
    });
    const observer = new PaceObserver(started.url, started.pid);
    try {
        await host.saw('Registered peer server-', 30);
        const before = await host.mark();
        // A browser asks for a picture a few times a second at most.
        const { peer, rtp } = await runSession(
            `ws://127.0.0.1:${started.port}/api/ws`,
            { delay_s: 0.03, loss: 0.005, seed: 11 }, LINKS[link], MEASURE_S, 8.0,
            { env: { PEER_PLI_INTERVAL_S: '0.4' } },
        );
        const seen = analyze(rtp, peer.rtx);
        // The server logs the pacer's close when it handles the client's
        // hangup, which lands a few milliseconds after the client returns.
        let text = await host.log(before);
        for (const until = Date.now() + 5000; !text.includes('pacer closed:') && Date.now() < until;) {
            await sleep(50);
            text = await host.log(before);
        }
        Object.assign(seen, serverCounts(text));
        seen.pace_bps ??= observer.paceBps;
        seen.pace_source = text.includes('pacer closed:') ? 'pacer closed' : observer.path;
        const down = peer.shapers.get('down');
        seen.lost_down = down ? down.lost + down.dropped : 0;
        seen.nack = peer.nacks.length;
        seen.nacked = peer.nacks.reduce((sum, n) => sum + n.n, 0);
        seen.pli = peer.plis.length;
        const streams: Record<number, number> = {};
        for (const p of rtp) streams[p.ssrc] = (streams[p.ssrc] ?? 0) + 1;
        seen.streams = streams;
        seen.rtx = Object.fromEntries(peer.rtx);
        return seen;
    } finally {
        await observer.stop();
        await host.stop();
    }
}

for (const link of Object.keys(LINKS)) {
    test(link, async () => {
        const res = new Checks('webrtc-loss-recovery');
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const { display } = await host.xvfb(WIDTH, HEIGHT);
            const painter = spawn(suitesBin('motion-scene'), [display, String(WIDTH), String(HEIGHT), '60'], { stdio: 'ignore' });
            try {
                const seen = await session(host, display, link);
                process.stdout.write(`      ${link}: ${JSON.stringify(seen)}\n`);
                const num = (key: string): number => {
                    const value = seen[key];
                    return typeof value === 'number' ? value : 0;
                };
                res.check(`${link}: video crossed the link and the client saw keyframes`,
                    seen.frames > (link === 'bursts' ? 15 : 5) * MEASURE_S && seen.keyframes >= 1, seen);
                // The narrow link resets the pacer every few hundred milliseconds, and a repair
                // queued into a GOP the next reset abandons is dropped with it: a run there sees
                // a handful of repairs or none.
                if (link === 'bursts') {
                    res.check('bursts: the link lost packets and the client had them repaired',
                        num('lost_down') > 0 && seen.repairs > 0, seen);
                } else {
                    res.check('narrow: the link lost packets', num('lost_down') > 0, seen);
                }
                res.check(`${link}: the pacer cut its queue at least once, abandoning the GOP or resyncing the peer`,
                    num('resets') + num('resyncs') >= 1, seen);
                res.check(`${link}: no hole's packet was repaired behind the keyframe that closed the hole`,
                    seen.stale_repairs === 0, seen);
                res.check(`${link}: no media from before a keyframe arrived behind it`, seen.stale_media === 0, seen);
                res.check(`${link}: every abandoned GOP was replaced by a keyframe in time`, num('timeout_resurrects') === 0, seen);
                if (link === 'bursts') {
                    // The blackout is half a second; what follows it is the keyframe's
                    // own way through the link, not a queue of abandoned video ahead of it.
                    res.check('bursts: a hole closes within a few hundred milliseconds of the link coming back',
                        seen.recovery_ms !== null && seen.recovery_ms[0] < 800, seen);
                } else {
                    // The pace climbs back a quarter per second after each brake, so
                    // what it reads at the end is where that climb was, below the ceiling.
                    res.check('narrow: loss spread through the window still sizes brakes',
                        typeof seen.pace_bps === 'number' && seen.pace_bps < 10_000_000, seen);
                }
            } finally {
                painter.kill('SIGTERM');
            }
        } finally {
            await host.close();
        }
        res.finish();
    });
}
