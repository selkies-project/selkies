/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The WebRTC pacer on a constrained link, as numbers.
//
// A real server in WebRTC mode streams the test display (an animated terminal
// moving under pacer_load_gen.sh, and a tone, as the capture sends nothing from
// a silent desktop) to the crate's headless client (webrtc-peer), whose every
// ICE candidate is a relay shaping one leg each way. Regimes: `flat` passes
// datagrams through; `shaped` holds both legs at 5.5 Mbit/s and 15 ms;
// `osc` swings both between 8 and 2.8 Mbit/s every four seconds. Each regime
// runs with the pacer off and on; `osc` and `osc2` run PACER_REPEATS times
// (three by default). Read at the client over the measured window: the audio
// packets' arrival gaps, each frame's span from its first packet to its last,
// the one-way delay abs-send-time gives, the data channel's pings, and the
// rates; from the server, its CPU and its GOP resets.
//
// Under an oscillating link the pacer settles into one of two behaviors from
// run to run on identical code -- passing video through with few GOP resets and
// letting audio gap, or purging hard and protecting audio at the cost of later
// video -- and a single sample of that reads as a solid number while being
// neither. Repeats make the spread visible instead.
//
// The numbers are the point of this benchmark and are left to the reader; what
// is checked is that every cell completed with media flowing, which is what
// breaks when the pacer or the ICE path regresses. It is deliberately not
// checked that the pacer improves any of them: on the oscillating regime the
// same code lands either side of the trade from run to run, so a
// single-sample comparison is a coin flip dressed as a gate.
//
// PACER_WINDOW_S sets the window (32 s under `osc`, 25 s otherwise),
// PACER_E2E_LOAD=0 leaves the load generator out, and SELKIES_ENCODER,
// SELKIES_CONGESTION_CONTROL and SELKIES_WEBRTC_PACER_* pass to the server.
import { spawn, type ChildProcess } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { test } from '@playwright/test';

import { fields, numberOr, parseJson } from '../../web/selkies-web-core/lib/narrow.ts';
import { Checks } from '../lib/checks.ts';
import { pulseSine, pulseUnload } from '../lib/desktop.ts';
import { sleep, suiteOf, TOOLS, WORKDIR } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { runSession, type LinkShape, type Peer, type Session } from '../lib/peer.ts';

const REPEATS = Math.max(1, Number.parseInt(process.env.PACER_REPEATS ?? '3', 10) || 1);
const LOAD_GEN = process.env.PACER_E2E_LOAD !== '0';
// Linux's clock ticks, which /proc's CPU times count in.
const CLOCK_TICKS = 100;

/** A percentile of an ascending list of seconds, in milliseconds. */
function pct(sorted: readonly number[], p: number): number | null {
    if (!sorted.length) return null;
    return Math.round(100_000 * sorted[Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length))]) / 100;
}

function spread(values: readonly number[]): { p50: number | null; p99: number | null; max: number | null } {
    const sorted = [...values].sort((a, b) => a - b);
    const top = sorted.at(-1);
    return { p50: pct(sorted, 50), p99: pct(sorted, 99), max: top === undefined ? null : Math.round(100_000 * top) / 100 };
}

/** A session's window, summarized. */
function measureWindow({ peer, rtp, messages }: Session): Record<string, unknown> {
    const audio = rtp.filter((p) => p.media === 'audio');
    // A FEC stream is not negotiated by the client, so it is no part of what it receives.
    const video = rtp.filter((p) => p.media === 'video' && !peer.fec.has(p.ssrc));
    const audioTimes = audio.map((p) => p.t);
    const gaps = audioTimes.slice(1).map((t, i) => t - audioTimes[i]);
    const frames = new Map<number, [number, number]>();
    for (const { t, ts } of video) {
        const span = frames.get(ts);
        if (span) {
            span[0] = Math.min(span[0], t);
            span[1] = Math.max(span[1], t);
        } else {
            frames.set(ts, [t, t]);
        }
    }
    const pings: number[] = [];
    for (const { text, wall } of messages) {
        if (!text.includes('"ping"')) continue;
        const message = fields(parseJson(text));
        const start = numberOr(fields(message.data).start_time);
        if (message.type === 'ping' && start !== undefined) pings.push(start - wall);
    }
    const bytes = (packets: typeof rtp): number => packets.reduce((sum, p) => sum + p.len + 40, 0);
    const mbps = (packets: typeof rtp): number | null => {
        if (packets.length <= 2) return null;
        return Math.round(1000 * (bytes(packets) * 8 / 1e6 / (packets[packets.length - 1].t - packets[0].t))) / 1000;
    };
    const ow = (packets: typeof rtp): number[] => packets.flatMap((p) => (p.ow === null ? [] : [p.ow]));
    return {
        ping_ms: { n: pings.length, ...spread(pings) },
        audio_pkts: audio.length,
        audio_gap_ms: spread(gaps),
        audio_gaps_over_60ms: gaps.filter((g) => g > 0.060).length,
        video_pkts: video.length,
        video_frames: frames.size,
        frame_span_ms: spread([...frames.values()].map(([a, b]) => b - a)),
        ow_video_ms: spread(ow(video)),
        ow_audio_ms: spread(ow(audio)),
        audio_mbps: mbps(audio),
        video_mbps: mbps(video),
    };
}

/** A process's CPU time in seconds, or null once it is gone. */
function cpuSeconds(pid: number): number | null {
    try {
        const stat = readFileSync(`/proc/${pid}/stat`, 'utf8');
        const after = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
        // utime and stime, the 14th and 15th fields, past the name.
        return (Number(after[11]) + Number(after[12])) / CLOCK_TICKS;
    } catch {
        return null;
    }
}

async function runCell(host: Host, pacerOn: boolean, regime: string): Promise<Record<string, unknown>> {
    let up: LinkShape = {};
    let down: LinkShape = {};
    if (regime === 'shaped') {
        // Client -> server carries input; server -> client carries media.
        up = { rate_bps: 5.5e6, delay_s: 0.015 };
        down = { rate_bps: 5.5e6, delay_s: 0.015 };
    } else if (regime === 'osc') {
        up = { rate_bps: 8e6, delay_s: 0.015 };
        down = { rate_bps: 8e6, delay_s: 0.015 };
    }
    const env: Record<string, string> = {
        SELKIES_WEBRTC_PACER: pacerOn ? 'true' : 'false',
        SELKIES_TURN_REST_URI: '',
        SELKIES_STUN_HOST: '',
        SELKIES_VIDEO_BITRATE: '8000',
        SELKIES_CONGESTION_CONTROL: 'false',
        SELKIES_DEBUG: 'true',
    };
    for (const [k, v] of Object.entries(process.env)) {
        if (v !== undefined && (k.startsWith('SELKIES_WEBRTC_PACER_') || k === 'SELKIES_CONGESTION_CONTROL' || k === 'SELKIES_ENCODER')) env[k] = v;
    }
    const cellStart = performance.now();
    const { port, pid } = await host.start({ label: `${regime}-${pacerOn ? 'on' : 'off'}`, mode: 'webrtc', env });
    const cpu0 = cpuSeconds(pid);
    const t0 = performance.now();
    let window: Record<string, unknown>;
    let load: ChildProcess | null = null;
    let logBefore = 0;
    let cellLog = '';
    let cpu: number | null = null;
    const tone = pulseSine();
    try {
        if (LOAD_GEN) load = spawn(join(TOOLS, 'pacer_load_gen.sh'), [], { stdio: 'ignore', detached: true });
        // The listener answers before the service has registered its signaling
        // peer; a session asked for in between is refused.
        await host.saw('Registered peer server-', 30);
        logBefore = await host.mark();
        // The oscillating regime needs to be watched across many periods: the
        // pacer brakes multiplicatively from a wide-open start and recovers
        // between troughs, so a window covering only a few of them reports
        // whichever side of that it happened to be on.
        const ms = Number(process.env.PACER_WINDOW_S ?? (regime === 'osc' ? '32' : '25'));
        // The link swings from the cell's start, the server's boot included: 8 Mbit/s for
        // four seconds, then 2.8, and so on.
        const oscillate = async (peer: Peer): Promise<void> => {
            if (regime !== 'osc') return;
            let rate = 8e6;
            while (!peer.ended) {
                const phase = Math.floor((performance.now() - cellStart) / 4000) % 2 === 0 ? 8e6 : 2.8e6;
                if (phase !== rate) {
                    rate = phase;
                    peer.setRate(rate);
                }
                await sleep(50);
            }
        };
        const session = await runSession(`ws://127.0.0.1:${port}/api/ws`, up, down, ms, 8.0, { alongside: oscillate });
        window = { ...measureWindow(session), video_saw: session.rtp.filter((p) => p.media === 'video').length, audio_saw: session.rtp.filter((p) => p.media === 'audio').length };
    } finally {
        if (load?.pid !== undefined) {
            // The generator's terminal shares its process group; anything else on
            // the host matching its name is not this cell's to end.
            try {
                process.kill(-load.pid, 'SIGKILL');
            } catch {
                // Already gone.
            }
        }
        pulseUnload(tone);
        cellLog = await host.log(logBefore);
        const cpu1 = cpuSeconds(pid);
        if (cpu0 !== null && cpu1 !== null) cpu = Math.round(1000 * (100 * (cpu1 - cpu0)) / ((performance.now() - t0) / 1000)) / 10;
        await host.stop();
    }
    const count = (needle: string): number => cellLog.split(needle).length - 1;
    return {
        pacer: pacerOn, regime, ...window,
        server_cpu_pct: cpu,
        server_resets: count('GOP reset'),
        server_keyreq_lines: count('requestKeyframe') + count('keyframe requested'),
    };
}

test('pacer', async () => {
    const res = new Checks('pacer');
    const host = new Host(suiteOf(import.meta.filename));
    const rows: Record<string, unknown>[] = [];
    try {
        const cells: [boolean, string][] = [
            [false, 'osc'], [true, 'osc'], [false, 'osc2'], [true, 'osc2'],
            [false, 'flat'], [true, 'flat'], [false, 'shaped'], [true, 'shaped'],
        ];
        for (const [pacerOn, cell] of cells) {
            const regime = cell.startsWith('osc') ? 'osc' : cell;
            for (let run = 0; run < (regime === 'osc' ? REPEATS : 1); run += 1) {
                process.stdout.write(`=== cell pacer=${pacerOn} regime=${cell} run=${run + 1}\n`);
                const row = { ...await runCell(host, pacerOn, regime), cell, run: run + 1 };
                rows.push(row);
                process.stdout.write(`${JSON.stringify(row)}\n`);
            }
        }
    } finally {
        await host.close();
    }
    const out = join(WORKDIR, 'pacer_e2e_results.json');
    writeFileSync(out, JSON.stringify(rows, null, 1));
    process.stdout.write(`DONE -> ${out}\n`);
    const empty = rows.filter((r) => !r.video_pkts || !r.audio_pkts).map((r) => `pacer=${String(r.pacer)} ${String(r.cell)}`);
    res.check('every cell carried media', empty.length === 0, empty.length ? `FAILED: ${empty.join(', ')}` : 'all cells carried media');
    for (const cell of [...new Set(rows.map((r) => String(r.cell)))].sort()) {
        for (const pacerOn of [false, true]) {
            const runs = rows.filter((r) => r.cell === cell && r.pacer === pacerOn);
            if (!runs.length) continue;
            const num = (v: unknown): number => (typeof v === 'number' ? v : 0);
            const lat = runs.map((r) => num(fields(r.ow_video_ms).p50)).sort((a, b) => a - b);
            const gaps = runs.map((r) => num(r.audio_gaps_over_60ms)).sort((a, b) => a - b);
            const resets = runs.map((r) => num(r.server_resets)).sort((a, b) => a - b);
            // Which side of the trade each run landed on, so a spread that comes
            // from the pacer switching behavior is not read as measurement noise.
            const purging = runs.filter((r) => num(r.server_resets) > 60).length;
            process.stdout.write(`RESULT ${cell} pacer=${pacerOn ? 'True' : 'False'}: n=${runs.length} `
                + `one-way video p50 ${lat[0].toFixed(0)}-${lat[lat.length - 1].toFixed(0)} ms, `
                + `audio gaps ${gaps[0]}-${gaps[gaps.length - 1]}, GOP resets ${resets[0]}-${resets[resets.length - 1]}, `
                + `purging in ${purging}/${runs.length}\n`);
        }
    }
    res.finish();
});
