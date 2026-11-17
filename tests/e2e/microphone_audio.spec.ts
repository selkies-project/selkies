/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The microphone uplink's sound: a browser's microphone, switched on through
// the dashboard's pipeline control, must come out of the server's recordable
// virtual source carrying what the browser captured -- over the WebSocket
// (WebCodecs Opus in 0x02 frames) and over WebRTC (the Opus track on the
// reserved sendonly transceiver) -- and go quiet again when switched off.
// Chromium captures a WAV of a known tone as its fake microphone, so the PCM
// recorded from the server's PulseAudio/PipeWire source is checked for that
// tone (a Goertzel detector against the signal's RMS), not merely for a
// source that exists; with WebCodecs audio taken away the WebSocket uplink
// encodes on libopus in WASM, and where the engine refuses a 24 kHz context
// for the stream (Firefox before 148) it captures at its own rate and
// resamples, and either way must carry the tone alike. The operator lock
// (`microphone_enabled=false|locked`) must withhold the uplink on both
// transports. Audio is independent of the capture backend, so the blocks run
// against the X test display only.
import { spawnSync } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import type { Browser, Page } from '@playwright/test';
import { chromium, test, webkit } from '@playwright/test';

import { analyze, record, TONE_HZ, toneWav } from '../lib/audio.ts';
import { BROWSER_ARGS } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { onPath } from '../lib/desktop.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { NO_WEBCODECS_AUDIO_JS } from '../lib/page.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

// What WebKit's mock microphone mostly carries; it takes no file of its own.
const WEBKIT_MOCK_HZ = 150;
// The recordable source the server provisions; PipeWire's pulse server
// prefixes virtual sources with the sink they hang off, PulseAudio does not.
const VIRTUAL_MIC = 'SelkiesVirtualMic';

const MIC_JS = `
  window.__micStatus = [];
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || !d.type) return;
    if ((d.type === 'sidebarButtonStatusUpdate' || d.type === 'pipelineStatusUpdate') && d.microphone !== undefined) {
      window.__micStatus.push(!!d.microphone);
    }
  });
`;

// A context that refuses a stream at another rate, as Firefox before 148
// does the 24 kHz one the client first asks for.
const FIXED_RATE_JS = `
(() => {
  const create = AudioContext.prototype.createMediaStreamSource;
  AudioContext.prototype.createMediaStreamSource = function (stream) {
    if (this.sampleRate === 24000) {
      throw new DOMException('Connecting AudioNodes from AudioContexts with different sample-rate is currently not supported.', 'NotSupportedError');
    }
    return create.call(this, stream);
  };
})();
`;

const pactl = (...args: string[]): string => spawnSync('pactl', args, { encoding: 'utf8' }).stdout ?? '';

/** Names of the sources the sound server currently exposes. */
const sources = (): string[] => pactl('list', 'short', 'sources').split('\n').filter((l) => l.includes('\t')).map((l) => l.split('\t')[1]);

/**
 * Unload a virtual microphone an earlier suite's server left loaded (a server
 * killed at its teardown unloads nothing), so the one checked here is the one
 * its first mic data provisions.
 */
function unloadLeftMic(): void {
    for (const line of pactl('list', 'short', 'modules').split('\n')) {
        const fields = line.split('\t');
        if (fields.length > 2 && fields[1] === 'module-virtual-source' && fields[2].includes(VIRTUAL_MIC)) spawnSync('pactl', ['unload-module', fields[0]]);
    }
}

/** The virtual microphone source once the server has provisioned it. */
async function virtualMicSource(timeoutS = 25): Promise<string | null> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        const found = sources().find((name) => name.endsWith(VIRTUAL_MIC));
        if (found) return found;
        await sleep(500);
    }
    return null;
}

const toggle = (page: Page, enabled: boolean): Promise<unknown> => within(page.evaluate(`window.postMessage({type: 'pipelineControl', pipeline: 'microphone', enabled: ${enabled}}, window.location.origin)`));
const statuses = async (page: Page): Promise<unknown[]> => {
    const got = await within(page.evaluate('window.__micStatus'));
    return Array.isArray(got) ? got.map((v): unknown => v) : [];
};

async function waitStatus(page: Page, value: boolean, timeoutS = 20): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if ((await statuses(page)).at(-1) === value) return true;
        await sleep(250);
    }
    return false;
}

/**
 * Chromium with the tone as its microphone, on the stream page; or WebKit
 * with its own mock microphone, which takes no file. The headless shell has
 * no media capture; the full Chromium build (new headless mode) or the system
 * Chrome E2E_CHROME names is needed.
 */
async function launch(url: string, wav: string, mode: string, initJs: string, engine: 'chromium' | 'webkit'): Promise<[Browser, Page, string[]]> {
    let browser: Browser;
    if (engine === 'webkit') {
        browser = await webkit.launch({ headless: true });
    } else {
        const executablePath = process.env.E2E_CHROME || undefined;
        browser = await chromium.launch({ headless: true, args: [...BROWSER_ARGS, '--use-fake-device-for-media-stream', `--use-file-for-fake-audio-capture=${wav}`],
            ...(executablePath ? { executablePath } : { channel: 'chromium' }) });
    }
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, permissions: ['microphone'] });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: MIC_JS });
    if (initJs) await ctx.addInitScript({ content: initJs });
    const page = await ctx.newPage();
    const errors: string[] = [];
    page.on('pageerror', (e) => errors.push(String(e)));
    await page.goto(`${url}/`, { waitUntil: 'load' });
    return [browser, page, errors];
}

const waitVideo = (page: Page, mode: string): Promise<unknown> => (mode === 'websockets' ? waitWsVideo(page, 30) : waitWrVideo(page));

function toneFile(): string {
    const wav = join(mkdtempSync(join(tmpdir(), 'selkies-mic-')), 'tone.wav');
    toneWav(wav);
    return wav;
}

async function transportBlock(host: Host, res: Checks, mode: string, wasm: boolean, fixedRate: boolean, engine: 'chromium' | 'webkit'): Promise<void> {
    const wav = toneFile();
    unloadLeftMic();
    const { url } = await host.start({ label: res.block, mode });
    try {
        const [browser, page, errors] = await launch(url, wav, mode, (wasm ? NO_WEBCODECS_AUDIO_JS : '') + (fixedRate ? FIXED_RATE_JS : ''), engine);
        try {
            const said: string[] = [];
            page.on('console', (m) => said.push(m.text()));
            res.check('stream up', Boolean(await waitVideo(page, mode)));
            const before = sources().filter((s) => s.endsWith(VIRTUAL_MIC));
            await toggle(page, true);
            res.check('microphone reports active', await waitStatus(page, true), JSON.stringify(await statuses(page)));
            const aboutMic = (): string[] => said.filter((t) => t.includes('icrophone')).slice(0, 3);
            if (fixedRate) res.check("the capture fell back on the engine's rate and resamples", said.some((t) => t.includes('resampled to 24 kHz')), aboutMic());
            if (wasm) res.check('the encode worker fell back on libopus in WASM', said.some((t) => t.includes('Microphone encodes Opus on libopus in WASM')), aboutMic());
            const source = await virtualMicSource();
            res.check('virtual microphone source provisioned on first mic data', source !== null && before.length === 0, `${source ?? 'None'} (before: ${JSON.stringify(before)})`);
            if (source) {
                // pcmflux opens the playback stream off the loop once the first
                // chunk arrives; the sound server then has to route it through
                // the virtual source before the tone can be heard there.
                await sleep(2000);
                const got = analyze(record(source), engine === 'webkit' ? WEBKIT_MOCK_HZ : TONE_HZ);
                res.check("recorded PCM carries the browser's tone", got.rms > 150 && got.ratio > 0.5, JSON.stringify(got));
            }
            await toggle(page, false);
            res.check('microphone reports inactive', await waitStatus(page, false), JSON.stringify(await statuses(page)));
            if (source) {
                // Frames already in flight land for a moment after the stop.
                await sleep(1500);
                const got = analyze(record(source));
                res.check('source goes quiet after disable', got.rms < 50, JSON.stringify(got));
            }
            res.check('no page errors', errors.length === 0, errors.join('; ').slice(0, 200));
        } finally {
            await browser.close();
        }
    } finally {
        await host.stop();
    }
}

async function lockedBlock(host: Host, res: Checks): Promise<void> {
    const wav = toneFile();
    for (const mode of ['websockets', 'webrtc']) {
        unloadLeftMic();
        const { url } = await host.start({ label: `locked-${mode}`, mode, env: { SELKIES_MICROPHONE_ENABLED: 'false|locked' } });
        try {
            const [browser, page] = await launch(url, wav, mode, '', 'chromium');
            try {
                res.check(`${mode}: stream up`, Boolean(await waitVideo(page, mode)));
                await toggle(page, true);
                await sleep(5000);
                const seen = await statuses(page);
                res.check(`${mode}: client settles on microphone off`, seen.length === 0 || seen.at(-1) === false, JSON.stringify(seen));
                res.check(`${mode}: no virtual microphone provisioned`, !sources().some((s) => s.endsWith(VIRTUAL_MIC)), JSON.stringify(sources()));
            } finally {
                await browser.close();
            }
        } finally {
            await host.stop();
        }
    }
}

const BLOCKS: Record<string, { mode: string; wasm: boolean; rate: boolean; engine: 'chromium' | 'webkit' }> = {
    websockets: { mode: 'websockets', wasm: false, rate: false, engine: 'chromium' },
    'websockets-wasm': { mode: 'websockets', wasm: true, rate: false, engine: 'chromium' },
    'websockets-wasm-webkit': { mode: 'websockets', wasm: true, rate: false, engine: 'webkit' },
    'websockets-rate': { mode: 'websockets', wasm: false, rate: true, engine: 'chromium' },
    webrtc: { mode: 'webrtc', wasm: false, rate: false, engine: 'chromium' },
};

for (const which of [...Object.keys(BLOCKS), 'locked']) {
    test(which, async () => {
        const b = BLOCKS[which];
        const name = which === 'locked' ? 'microphone-audio-locked'
            : `microphone-audio-${b.mode}${b.wasm ? '-wasm' : ''}${b.rate ? '-rate' : ''}${b.engine === 'chromium' ? '' : `-${b.engine}`}`;
        const res = new Checks(name);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            const missing = ['pactl', 'parec'].find((tool) => onPath(tool) === null);
            if (missing) res.skip('the suite', `${missing} is not installed`);
            else if (which === 'locked') await lockedBlock(host, res);
            else await transportBlock(host, res, b.mode, b.wasm, b.rate, b.engine);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
