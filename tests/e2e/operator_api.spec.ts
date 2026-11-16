/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The operator API end to end: sessions, recording, and screenshots, with the
// audit trail they leave.
//
// websockets / webrtc: a controller page and a shared viewer page connect;
//   /api/sessions lists both with their roles, an RFC 3339 connection time,
//   and a measured round trip; disconnecting the viewer by id closes it, and
//   the collector hears a connect for each page and the disconnect with its
//   length. A recording started with no body lands in the file-manager
//   directory as a fragmented MP4 that grows while the desktop is captured,
//   its stop reports the frames and bytes written, and both are on the audit
//   trail; a screenshot is a PNG the size of the display, on the trail too
//   with the role that took it. Over websockets the block also runs the
//   view-only credential against every endpoint.
// wayland: the same screenshot and recording against the Wayland backend.
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, openSync, readSync, closeSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join } from 'node:path';

import type { Browser, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { fields, numberOr, stringOr, type Fields } from '../../web/selkies-web-core/lib/narrow.ts';
import { analyze, CAPTURE_RATE, toneWav, type Tone } from '../lib/audio.ts';
import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { Collector } from '../lib/collector.ts';
import { onPath, pulseServer } from '../lib/desktop.ts';
import { sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo } from '../lib/stream.ts';

const FILES_DIR = join(WORKDIR, 'operator-files');
const BASIC_USER = 'user';
const BASIC_PASSWORD = 'secret';
const VIEWONLY_PASSWORD = 'look';
const RFC3339 = /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/;

/** The server the block's requests go to. */
let base = '';

/** One request against the test server: status and body bytes. */
async function request(method: string, path: string, body?: string, headers: Record<string, string> = {}): Promise<[number, Buffer]> {
    const res = await fetch(base + path, { method, headers, body, signal: AbortSignal.timeout(60_000) });
    return [res.status, Buffer.from(await res.arrayBuffer())];
}

/** A JSON request: status and the decoded body, or null. */
async function api(method: string, path: string, body?: unknown, headers: Record<string, string> = {}): Promise<[number, Fields | null]> {
    const extra = { ...headers, ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) };
    const [status, raw] = await request(method, path, body === undefined ? undefined : JSON.stringify(body), extra);
    try {
        return [status, raw.length ? fields(JSON.parse(raw.toString('utf8'))) : null];
    } catch {
        return [status, null];
    }
}

function basic(password: string): Record<string, string> {
    return { Authorization: `Basic ${Buffer.from(`${BASIC_USER}:${password}`).toString('base64')}` };
}

function serverEnv(collector: Collector): Record<string, string> {
    return { SELKIES_FILE_MANAGER_PATH: FILES_DIR, SELKIES_AUDIT_WEBHOOK_URL: collector.url };
}

async function newPage(browser: Browser, url: string, mode: string, hash = ''): Promise<Page> {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    const page = await ctx.newPage();
    await page.goto(`${url}/${hash}`, { waitUntil: 'load' });
    return page;
}

function waitVideo(page: Page, mode: string): ReturnType<typeof waitWsVideo> {
    return mode === 'webrtc' ? waitWrVideo(page, 45) : waitWsVideo(page, 30);
}

async function sessions(): Promise<Fields[]> {
    const [status, body] = await api('GET', '/api/sessions');
    const list = body?.sessions;
    return status === 200 && Array.isArray(list) ? list.map(fields) : [];
}

async function waitSessions(count: number, timeoutS = 20): Promise<Fields[]> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        const found = await sessions();
        if (found.length >= count) return found;
        await sleep(500);
    }
    return sessions();
}

/** The listing once `count` pages are on it with a round trip measured: WebRTC reports one only after the page's first receiver report. */
async function waitMeasured(count: number, timeoutS = 20): Promise<Fields[]> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        const found = await sessions();
        if (found.length >= count && found.every((s) => typeof s.rtt_ms === 'number')) return found;
        await sleep(500);
    }
    return sessions();
}

async function waitGone(id: string, timeoutS = 15): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (!(await sessions()).some((s) => s.id === id)) return true;
        await sleep(500);
    }
    return false;
}

/** The top-level box types of an MP4 file, in order. */
function mp4Boxes(path: string): string[] {
    const boxes: string[] = [];
    const fd = openSync(path, 'r');
    try {
        const size = statSync(path).size;
        const head = Buffer.alloc(16);
        for (let at = 0; at + 8 <= size;) {
            readSync(fd, head, 0, 16, at);
            let length = head.readUInt32BE(0);
            boxes.push(head.toString('latin1', 4, 8));
            if (length === 1) length = Number(head.readBigUInt64BE(8));
            else if (length === 0) break;
            at += length;
        }
    } finally {
        closeSync(fd);
    }
    return boxes;
}

function pngSize(data: Buffer): [number, number] | null {
    if (!data.subarray(0, 8).equals(Buffer.from('\x89PNG\r\n\x1a\n', 'latin1'))) return null;
    return [data.readUInt32BE(16), data.readUInt32BE(20)];
}

/** Play the microphone suite's tone into the session's sink, the one the recorder's audio reads, for `seconds`. */
function playTone(seconds: number): ChildProcess | null {
    const pulse = pulseServer();
    if (!onPath('paplay') || !pulse) return null;
    const wav = join(mkdtempSync(join(tmpdir(), 'selkies-rec-')), 'tone.wav');
    toneWav(wav, seconds);
    return spawn('paplay', ['--server', pulse, '--device', 'output', wav], { stdio: 'ignore' });
}

/** Wait until the session's sink is playing what reaches it. */
async function waitSinkPlaying(timeoutS = 5): Promise<void> {
    const pulse = pulseServer();
    for (const until = Date.now() + timeoutS * 1000; pulse && Date.now() < until;) {
        const sinks = spawnSync('pactl', ['--server', pulse, 'list', 'short', 'sinks'], { encoding: 'utf8' }).stdout ?? '';
        if (sinks.split('\n').some((l) => l.split('\t')[1] === 'output' && l.trimEnd().endsWith('RUNNING'))) return;
        await sleep(50);
    }
}

/** The file's first audio stream as ffprobe decodes it, with the tone analysis of its second second as mono PCM. */
function audioTrack(path: string): { codec: string; channels: number; frames: number; tone: Tone } | null {
    if (!onPath('ffprobe') || !onPath('ffmpeg')) return null;
    const probe = (spawnSync('ffprobe', ['-v', 'error', '-count_frames', '-select_streams', 'a:0',
        '-show_entries', 'stream=codec_name,channels,nb_read_frames', '-of', 'csv=p=0', path],
    { encoding: 'utf8', timeout: 120_000 }).stdout ?? '').trim().split(',');
    const pcm = spawnSync('ffmpeg', ['-v', 'error', '-i', path, '-vn', '-f', 's16le', '-ac', '1', '-ar', String(CAPTURE_RATE), '-'],
        { timeout: 120_000, maxBuffer: 1 << 28 }).stdout;
    const even = pcm.length - (pcm.length % 2);
    const samples = new Int16Array(pcm.buffer.slice(pcm.byteOffset, pcm.byteOffset + even));
    return {
        codec: probe[0] ?? '',
        channels: Number(probe[1] ?? 0) || 0,
        frames: /^\d+$/.test(probe[2] ?? '') ? Number(probe[2]) : 0,
        tone: analyze(samples.subarray(CAPTURE_RATE, 2 * CAPTURE_RATE)),
    };
}

/** Wait for a child to end, at most `timeoutS`. */
async function ended(child: ChildProcess, timeoutS: number): Promise<void> {
    if (child.exitCode !== null || child.signalCode !== null) return;
    await Promise.race([new Promise((done) => { child.once('exit', done); }), sleep(timeoutS * 1000)]);
}

/**
 * One recording, with a tone playing into the session's sink while it runs,
 * and one screenshot against the running server. The tone starts first: a
 * sound server renders an idle sink's monitor in blocks of up to two
 * seconds, so a capture that starts on silence has its first samples that
 * late, and the track would not span the recording.
 */
async function recordingRound(res: Checks, label: string, collector: Collector, expectSize: [number, number] | null): Promise<void> {
    const tone = playTone(3);
    if (tone !== null) await waitSinkPlaying();
    const [startStatus, started] = await api('POST', '/api/recording');
    const path = stringOr(started?.path) ?? '';
    res.check(`${label}: a recording starts into the file-manager directory`,
        startStatus === 200 && started !== null && started.active === true && dirname(path) === FILES_DIR
        && /^recording-\d{8}T\d{6}Z\.mp4$/.test(basename(path)), [startStatus, started]);
    const [again] = await api('POST', '/api/recording');
    res.check(`${label}: a second start is a conflict`, again === 409, again);
    await sleep(4000);
    const [liveStatus, live] = await api('GET', '/api/recording');
    res.check(`${label}: the status shows the recording growing`,
        liveStatus === 200 && live !== null && live.active === true && (numberOr(live.frames) ?? 0) > 0 && (numberOr(live.bytes) ?? 0) > 0, live);
    const [stopStatus, final] = await api('DELETE', '/api/recording');
    const frames = numberOr(final?.frames) ?? 0;
    const bytes = numberOr(final?.bytes) ?? 0;
    res.check(`${label}: stopping reports the frames and bytes written`,
        stopStatus === 200 && final !== null && !final.active && frames > 0 && bytes > 0
        && (numberOr(final.duration_s) ?? 0) >= 2 && !final.error, final);
    const boxes = existsSync(path) ? mp4Boxes(path) : [];
    res.check(`${label}: the file is a fragmented MP4 that plays from the first frame`,
        existsSync(path) && boxes[0] === 'ftyp' && boxes[1] === 'moov' && boxes.includes('moof') && statSync(path).size >= bytes,
        existsSync(path) ? boxes.slice(0, 6) : 'missing');
    if (onPath('ffprobe')) {
        const probe = (spawnSync('ffprobe', ['-v', 'error', '-count_frames', '-select_streams', 'v:0',
            '-show_entries', 'stream=codec_name,nb_read_frames', '-of', 'csv=p=0', path],
        { encoding: 'utf8', timeout: 120_000 }).stdout ?? '').trim();
        const count = probe.split(',')[1] ?? '';
        res.check(`${label}: ffprobe decodes every frame of it as H.264`,
            probe.startsWith('h264,') && /^\d+$/.test(count) && Number(count) === frames, [probe, frames]);
    } else {
        res.skip(`${label}: ffprobe decodes every frame of it as H.264`, 'no ffprobe on PATH');
    }
    const audio = existsSync(path) ? audioTrack(path) : null;
    if (tone === null) {
        res.skip(`${label}: the session's audio is in the file as an Opus track`, 'no paplay or sound server');
    } else if (audio === null) {
        res.skip(`${label}: the session's audio is in the file as an Opus track`, 'no ffprobe or ffmpeg on PATH');
    } else {
        await ended(tone, 10);
        const audioFrames = numberOr(final?.audio_frames) ?? 0;
        res.check(`${label}: the session's audio is in the file as an Opus track, every packet the status counted`,
            audio.codec === 'opus' && audio.channels === 2 && audio.frames === audioFrames && audioFrames > 0,
            [audio.codec, audio.channels, audio.frames, final?.audio_frames]);
        const duration = numberOr(final?.duration_s) ?? 0;
        res.check(`${label}: the track spans the recording at one packet per 20 ms`, Math.abs(audio.frames * 0.02 - duration) < 1, [audio.frames, duration]);
        res.check(`${label}: the tone played while it recorded is what the track holds`, audio.tone.ratio > 0.8 && audio.tone.rms > 500, audio.tone);
    }
    const [nothing] = await api('DELETE', '/api/recording');
    res.check(`${label}: stopping nothing is a conflict`, nothing === 409, nothing);
    const starts = await collector.waitFor('recording.start', 1);
    const stops = await collector.waitFor('recording.stop', 1);
    const start = starts.at(-1);
    const stop = stops.at(-1);
    res.check(`${label}: the audit carries the start and the stop with the file's figures`,
        start !== undefined && start.filename === path && stop !== undefined && stop.filename === path
        && stop.frames === frames && stop.size_bytes === bytes, [starts.slice(-1), stops.slice(-1)]);
    const [shotStatus, png] = await request('GET', '/api/screenshot');
    const size = pngSize(png);
    res.check(`${label}: a screenshot is a PNG of the display`,
        shotStatus === 200 && size !== null && (expectSize === null || (size[0] === expectSize[0] && size[1] === expectSize[1])),
        [shotStatus, size, expectSize]);
    const shots = await collector.waitFor('screenshot', 1);
    const shot = shots.at(-1);
    res.check(`${label}: the audit carries the screenshot with its display, size, and role`,
        shot !== undefined && shot.display === 'primary' && shot.size_bytes === png.length && shot.role === 'controller', shots.slice(-1));
}

function freshFiles(): void {
    rmSync(FILES_DIR, { recursive: true, force: true });
    mkdirSync(FILES_DIR, { recursive: true });
}

async function transportBlock(res: Checks, host: Host, mode: string): Promise<void> {
    freshFiles();
    let collector = await Collector.start();
    const { url } = await host.start({ label: mode, mode, env: serverEnv(collector) });
    base = url;
    const browser = await launchChromium();
    try {
        const owner = await newPage(browser, url, mode);
        const viewer = await newPage(browser, url, mode, '#shared');
        res.check('the controller page streams', (await waitVideo(owner, mode)) !== null);
        res.check('the viewer page streams', (await waitVideo(viewer, mode)) !== null);
        let listed = await waitSessions(2);
        const roles = listed.map((s) => String(s.role)).sort();
        res.check('both pages are listed with their roles and transport',
            JSON.stringify(roles) === '["controller","viewer"]' && listed.every((s) => s.transport === mode), listed);
        listed = await waitMeasured(2);
        res.check('each listing carries an RFC 3339 connection time and a measured round trip',
            listed.every((s) => RFC3339.test(stringOr(s.connected_at) ?? ''))
            && listed.every((s) => typeof s.rtt_ms === 'number' && s.rtt_ms >= 0 && s.rtt_ms < 5000),
            listed.map((s) => [s.role, s.connected_at, s.rtt_ms]));
        const connects = (await collector.waitFor('session.connect', 2)).slice(-2);
        res.check('the audit heard both connections with their roles',
            JSON.stringify(connects.map((e) => String(e.role)).sort()) === '["controller","viewer"]' && connects.every((e) => e.transport === mode), connects);
        const viewerId = stringOr(listed.find((s) => s.role === 'viewer')?.id) ?? '';
        const [status] = await request('DELETE', `/api/sessions/${viewerId}`);
        res.check('the viewer is disconnected by id', status === 204 && await waitGone(viewerId), status);
        const gone = (await collector.waitFor('session.disconnect', 1)).at(-1);
        res.check("the audit heard the disconnect with the connection's length",
            gone !== undefined && gone.role === 'viewer' && gone.transport === mode && (numberOr(gone.duration_s) ?? 0) > 0, gone ? [gone] : []);
        const [unknown] = await request('DELETE', '/api/sessions/nope');
        res.check('an unknown session is not found', unknown === 404, unknown);
        await recordingRound(res, mode, collector, await host.rootSize());
        await viewer.context().close();
        await owner.context().close();
    } finally {
        await browser.close();
    }
    await collector.stop();

    if (mode === 'websockets') {
        collector = await Collector.start();
        ({ url: base } = await host.start({
            label: `${mode}-basic`, mode, env: {
                ...serverEnv(collector), SELKIES_ENABLE_BASIC_AUTH: 'true', SELKIES_BASIC_AUTH_USER: BASIC_USER,
                SELKIES_BASIC_AUTH_PASSWORD: BASIC_PASSWORD, SELKIES_BASIC_AUTH_VIEWONLY_PASSWORD: VIEWONLY_PASSWORD,
            },
        }));
        const look = basic(VIEWONLY_PASSWORD);
        const own = basic(BASIC_PASSWORD);
        let [status] = await request('GET', '/api/sessions', undefined, look);
        res.check('view-only: may list sessions', status === 200, status);
        [status] = await request('GET', '/api/screenshot', undefined, look);
        res.check('view-only: may take a screenshot', status === 200, status);
        [status] = await request('POST', '/api/recording', undefined, look);
        res.check('view-only: may not record', status === 403, status);
        [status] = await request('DELETE', '/api/sessions/x', undefined, look);
        res.check('view-only: may not disconnect', status === 403, status);
        [status] = await request('GET', '/api/sessions');
        res.check('no credential is challenged', status === 401, status);
        const [recording, started] = await api('POST', '/api/recording', undefined, own);
        res.check('the main password records', recording === 200 && started?.active === true, recording);
        const [stopped] = await api('DELETE', '/api/recording', undefined, own);
        res.check('and stops', stopped === 200, stopped);
        await collector.stop();
    }
}

async function waylandBlock(res: Checks, host: Host): Promise<void> {
    freshFiles();
    const collector = await Collector.start();
    try {
        try {
            base = (await host.start({ label: 'wayland', mode: 'websockets', wayland: true, env: serverEnv(collector) })).url;
        } catch (e) {
            res.skip('the Wayland backend starts', String(e).slice(0, 120));
            return;
        }
        const browser = await launchChromium();
        try {
            const page = await newPage(browser, base, 'websockets');
            res.check('a page streams the Wayland session', (await waitVideo(page, 'websockets')) !== null);
            await recordingRound(res, 'wayland', collector, null);
            const [status] = await request('GET', '/api/screenshot?display=display9');
            res.check('an output the compositor does not have is not found', status === 404, status);
        } finally {
            await browser.close();
        }
    } finally {
        await collector.stop();
    }
}

for (const selector of ['websockets', 'webrtc', 'wayland']) {
    test(selector, async () => {
        const res = new Checks(`operator-${selector}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await (selector === 'wayland' ? waylandBlock(res, host) : transportBlock(res, host, selector));
        } finally {
            await host.close();
        }
        res.finish();
    });
}
