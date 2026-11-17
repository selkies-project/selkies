/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The webcam suites' camera and device: the camera every engine captures
// (pixelflux's virtual_camera example, published through the V4L2 interposer
// so each engine reads the same pixels at the same rate), and the server's
// virtual /dev/video0 read back through the interposer by tools/v4l2probe.
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { existsSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { chromium, firefox, webkit } from '@playwright/test';

import { BROWSER_ARGS, FF_E2E_PROFILE, openh264Prefs, openh264Version, type Engine } from './browsers.ts';
import { onPath } from './desktop.ts';
import { REPO, sleep, TOOLS } from './env.ts';
import { within } from './stream.ts';

const ADDON = join(REPO, 'addons', 'v4l2-interposer');
export const INTERPOSER = join(ADDON, 'selkies_v4l2_interposer.so');
const PROBE = join(TOOLS, 'v4l2probe');

/** The interposer and the probe, built. */
export function build(): void {
    for (const [dir, target] of [[ADDON, ''], [TOOLS, 'v4l2probe']]) {
        const made = spawnSync('make', ['-C', dir, ...(target ? [target] : [])], { stdio: ['ignore', 'ignore', 'inherit'] });
        if (made.status !== 0) throw new Error(`make -C ${dir} ${target} exited ${made.status}`);
    }
}

// Camera content is authored in YCbCr, which is what the device carries and
// what the samples are checked against; a JPEG round trip leaves a flat color
// where it started, so the published pixels and the device's are the same ones.
export type Ycc = readonly [number, number, number];
export const GREEN: Ycc = [150, 44, 21];
export const BLACK: Ycc = [16, 128, 128];
// Two flat halves in one frame: a turn relayed on the wire has to swap them in
// the device picture, which a solid frame could never show.
export const LEFT_HALF: Ycc = [150, 44, 21];
export const RIGHT_HALF: Ycc = [80, 200, 120];

// The rate the camera publishes at, and the share of it the device must
// carry. Whichever rung the client's ladder settled on has to sustain the
// camera; one that cannot is the failure this guards, and detailed content --
// where the choice is a real one -- is held to more.
export const CAMERA_FPS = 30;
export const RATE_FLOOR = 0.4;
export const DETAIL_RATE_FLOOR = 0.6;

// The picture reaches the device through whichever codec the engine chose, so
// a flat color arrives a few steps off and how many depends on that choice.
// The colors checked against each other are tens of steps apart, so a
// tolerance wide enough for any of those round trips still tells them apart.
const COLOR_TOL = 20;

export const near = (got: readonly number[] | undefined, want: Ycc, tol = COLOR_TOL): boolean => got !== undefined && want.every((w, i) => Math.abs(got[i] - w) <= tol);

/** What a page paints into a camera frame before it is encoded. */
export type FrameSpec = { kind: 'flat'; color: Ycc } | { kind: 'split' } | { kind: 'detail'; count: number };

// Draws the frames a spec names on a canvas and encodes each as a JPEG at
// quality 0.92, YCbCr converted to RGB as JFIF does; base64 strings come back.
const FRAMES_JS = `async ({ spec, width, height, left, right }) => {
  const rgb = ([y, cb, cr]) => [y + 1.402 * (cr - 128), y - 0.344136 * (cb - 128) - 0.714136 * (cr - 128), y + 1.772 * (cb - 128)]
    .map((v) => Math.max(0, Math.min(255, Math.round(v))));
  const css = (c) => 'rgb(' + rgb(c).join(',') + ')';
  const encode = async (canvas) => {
    const blob = await canvas.convertToBlob({ type: 'image/jpeg', quality: 0.92 });
    const bytes = new Uint8Array(await blob.arrayBuffer());
    let s = '';
    for (const b of bytes) s += String.fromCharCode(b);
    return btoa(s);
  };
  const out = [];
  if (spec.kind === 'flat' || spec.kind === 'split') {
    const c = new OffscreenCanvas(width, height);
    const g = c.getContext('2d');
    g.fillStyle = css(spec.kind === 'flat' ? spec.color : left);
    g.fillRect(0, 0, width, height);
    if (spec.kind === 'split') {
      g.fillStyle = css(right);
      g.fillRect(width / 2, 0, width / 2, height);
    }
    out.push(await encode(c));
    return out;
  }
  // A detailed scene that pans under moving grain: what an HD lens costs an encoder.
  let seed = 7;
  const rand = () => { seed = (seed * 1103515245 + 12345) >>> 0; return seed / 4294967296; };
  const scene = new OffscreenCanvas(width * 2, height);
  const s = scene.getContext('2d');
  s.fillStyle = css([90, 128, 128]);
  s.fillRect(0, 0, width * 2, height);
  for (let i = 0; i < 900; i++) {
    const x = Math.floor(rand() * width * 2), y = Math.floor(rand() * height);
    const w = 6 + Math.floor(rand() * 74), h = 6 + Math.floor(rand() * 74);
    s.fillStyle = css([Math.floor(rand() * 256), Math.floor(rand() * 256), Math.floor(rand() * 256)]);
    s.fillRect(x, y, w, h);
  }
  const grain = new Uint8Array(width * 2 * height);
  crypto.getRandomValues(grain.subarray(0, Math.min(grain.length, 65536)));
  for (let i = 65536; i < grain.length; i += 65536) crypto.getRandomValues(grain.subarray(i, Math.min(grain.length, i + 65536)));
  const base = s.getImageData(0, 0, width * 2, height).data;
  for (let f = 0; f < spec.count; f++) {
    const off = Math.floor(f * width / spec.count);
    const c = new OffscreenCanvas(width, height);
    const g = c.getContext('2d');
    const img = g.createImageData(width, height);
    for (let y = 0; y < height; y++) {
      for (let x = 0; x < width; x++) {
        const from = (y * width * 2 + x + off) * 4;
        const speck = grain[y * width * 2 + (width - off) + x] >> 3;
        const to = (y * width + x) * 4;
        img.data[to] = Math.min(255, base[from] + speck);
        img.data[to + 1] = Math.min(255, base[from + 1] + speck);
        img.data[to + 2] = Math.min(255, base[from + 2] + speck);
        img.data[to + 3] = 255;
      }
    }
    g.putImageData(img, 0, 0);
    out.push(await encode(c));
  }
  return out;
}`;

/**
 * The JPEG frames a camera shows, made in a throwaway headless Chromium.
 * @param spec What the frames show.
 * @param width Their width.
 * @param height Their height.
 * @returns The files.
 */
export async function frames(spec: FrameSpec, width = 640, height = 480): Promise<Buffer[]> {
    const browser = await chromium.launch({ headless: true, args: [...BROWSER_ARGS] });
    try {
        const page = await browser.newPage();
        const got = await page.evaluate(`(${FRAMES_JS})(${JSON.stringify({ spec, width, height, left: LEFT_HALF, right: RIGHT_HALF })})`);
        return (Array.isArray(got) ? got : []).map((b) => Buffer.from(String(b), 'base64'));
    } finally {
        await browser.close();
    }
}

/** pixelflux's virtual_camera example: SELKIES_TEST_CAMERA where CI hands a built one in, else the target directory's, built first where cargo is at hand. */
export function cameraBin(): string {
    const given = process.env.SELKIES_TEST_CAMERA;
    if (given) return given;
    const path = join(process.env.CARGO_TARGET_DIR || join(REPO, 'target'), 'debug', 'examples', 'virtual_camera');
    if (onPath('cargo')) spawnSync('cargo', ['build', '--quiet', '--locked', '-p', 'pixelflux', '--example', 'virtual_camera'], { cwd: REPO, stdio: 'inherit' });
    if (!existsSync(path)) throw new Error(`${path} is not built and SELKIES_TEST_CAMERA names no camera`);
    return path;
}

/**
 * The camera every engine captures. pixelflux's virtual_camera example
 * publishes the frames through the interposer, so Firefox and Chromium read
 * the same pixels at the same rate rather than each engine's own synthetic
 * fake, and what the camera is showing is the test's to choose.
 */
export class PublishedCamera {
    readonly sockDir = mkdtempSync(join(tmpdir(), 'selkies-cam-'));
    private proc: ChildProcess | null = null;
    private readonly pictures: readonly Buffer[];
    private readonly width: number;
    private readonly height: number;

    constructor(pictures: readonly Buffer[], width = 640, height = 480) {
        this.pictures = pictures;
        this.width = width;
        this.height = height;
    }

    async start(pixelFormat = 'I420'): Promise<PublishedCamera> {
        const socket = join(this.sockDir, 'selkies_webcam0.sock');
        // Reachable only over its own socket, which is the browser's to read:
        // a PipeWire node would also be found by the probe and the device
        // under test would then be this camera rather than the server's.
        const argv = ['--socket', socket, '--width', String(this.width), '--height', String(this.height), '--fps', String(CAMERA_FPS), '--pixel-format', pixelFormat, '--always', '--no-pipewire'];
        this.pictures.forEach((frame, i) => {
            const path = join(this.sockDir, `frame${i}.jpg`);
            writeFileSync(path, frame);
            argv.push('--picture', path);
        });
        this.proc = spawn(cameraBin(), argv, { stdio: 'ignore' });
        for (const until = Date.now() + 10_000; !existsSync(socket) && Date.now() < until;) await sleep(100);
        await sleep(500);
        return this;
    }

    async stop(): Promise<void> {
        const proc = this.proc;
        if (proc === null || proc.exitCode !== null || proc.signalCode !== null) return;
        const gone = new Promise<void>((resolve) => { proc.once('exit', () => { resolve(); }); });
        proc.kill('SIGTERM');
        const timer = setTimeout(() => proc.kill('SIGKILL'), 5000);
        await gone;
        clearTimeout(timer);
    }
}

/**
 * The environment of a process whose /dev/video0 is the camera serving
 * `sockDir` and no other: the interposer's default source falls back to a
 * PipeWire node named selkies-webcam, which on a shared session daemon may be
 * another suite's camera.
 * @param sockDir The camera's socket directory.
 * @returns The environment.
 */
export function interposerEnv(sockDir: string): NodeJS.ProcessEnv {
    return { ...process.env, LD_PRELOAD: INTERPOSER, SELKIES_WEBCAM_SOCKET_PATH: sockDir, SELKIES_WEBCAM_SOURCE: 'socket' };
}

/** What the probe read from the device. */
export interface Probed {
    rc: number;
    samples: Map<string, [number, number, number]>;
    fields: Record<string, string>;
}

const key = (x: number, y: number): string => `${x},${y}`;

/** The server's /dev/video0, read through the interposer from the server's runtime directory. */
export class Device {
    readonly runtime: string;

    constructor(runtime: string) {
        this.runtime = runtime;
    }

    /**
     * Read `frames` frames, sampling the pixels named.
     * @param frames How many.
     * @param timeoutMs How long the probe waits for each.
     * @param samples The pixels sampled.
     * @param dump A file the last frame is written to.
     * @returns What it read.
     */
    async probe(frames: number, timeoutMs = 4000, samples: readonly (readonly [number, number])[] = [[320, 240]], dump?: string): Promise<Probed> {
        const args = ['--timeout', String(timeoutMs), ...samples.flatMap(([x, y]) => ['--sample', key(x, y)]), ...(dump ? ['--dump', dump] : []), '/dev/video0', String(frames)];
        // Run beside the test, not in its place: Playwright drives Chromium from
        // this process, and a new worker (the page's camera encoder) waits on it
        // to attach before it runs, so a probe that held the event loop would
        // keep the very capture it is reading from starting.
        const [status, stdout] = await new Promise<[number | null, string]>((resolve) => {
            const child = spawn(PROBE, args, { env: interposerEnv(process.env.SELKIES_WEBCAM_SOCKET_PATH ?? this.runtime), stdio: ['ignore', 'pipe', 'ignore'] });
            let text = '';
            child.stdout.setEncoding('utf8');
            child.stdout.on('data', (chunk: string) => { text += chunk; });
            const timer = setTimeout(() => { child.kill('SIGKILL'); }, 60_000);
            child.once('error', () => { clearTimeout(timer); resolve([null, text]); });
            child.once('close', (code) => { clearTimeout(timer); resolve([code, text]); });
        });
        if (status === null) return { rc: -1, samples: new Map(), fields: { error: 'timeout' } };
        const out: Probed = { rc: status, samples: new Map(), fields: {} };
        for (const line of stdout.split('\n')) {
            const at = line.indexOf('=');
            if (at < 0) continue;
            const [k, v] = [line.slice(0, at), line.slice(at + 1)];
            if (k === 'sample') {
                const [pos, yuv] = v.split(':');
                const [y, u, w] = yuv.split(',').map(Number);
                out.samples.set(pos, [y, u, w]);
            } else {
                out.fields[k] = v;
            }
        }
        return out;
    }

    /** The device's current pixel format, as the interposer reports it. */
    async format(): Promise<string> {
        return (await this.probe(1, 2000)).fields.format ?? '';
    }

    /**
     * Poll until the device reports `fourcc`. The probe is itself a reader,
     * so the gaps between polls are what leave the server free to re-create
     * the device.
     */
    async waitFormat(fourcc: string, timeoutS = 20): Promise<boolean> {
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
            if ((await this.format()) === fourcc) return true;
            await sleep(2000);
        }
        return false;
    }

    /** An interposer client that holds the device open until it is terminated. */
    reader(): ChildProcess {
        return spawn(PROBE, ['--timeout', '60000', '/dev/video0', '100000'], { env: interposerEnv(process.env.SELKIES_WEBCAM_SOCKET_PATH ?? this.runtime), stdio: 'ignore' });
    }

    /**
     * Poll until the device shows this uplink's picture, then measure it. A
     * device that exists is not yet a device carrying the camera: the server
     * decodes what arrives, and the picture lands a moment after the first
     * frame does -- sooner or later depending on the engine and the rung it
     * took. Waiting on the picture rather than on a fixed delay keeps that out
     * of the checks.
     */
    async waitForPicture(wants: readonly (readonly [readonly [number, number], Ycc])[], frameCount = 30, timeoutS = 30): Promise<Probed> {
        const points = wants.map(([p]) => p);
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
            const r = await this.probe(5, 3000, points);
            // Every sample, not just one: a frame caught mid-transition is a
            // single flat color, which one sample on its own cannot tell from
            // the picture.
            if (r.rc === 0 && wants.every(([[x, y], want]) => near(r.samples.get(key(x, y)), want))) break;
            await sleep(500);
        }
        return await this.probe(frameCount, 4000, points);
    }
}

export const sampleAt = (r: Probed, x: number, y: number): [number, number, number] | undefined => r.samples.get(key(x, y));
export const samplesText = (r: Probed): string => JSON.stringify(Object.fromEntries(r.samples));

export const CAM_JS = `
  window.__camStatus = [];
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || !d.type) return;
    if ((d.type === 'sidebarButtonStatusUpdate' || d.type === 'pipelineStatusUpdate') && d.webcam !== undefined) {
      window.__camStatus.push(!!d.webcam);
    }
  });
`;

/** Switch the webcam through the dashboard's pipeline control. */
export const toggle = (page: Page, enabled: boolean): Promise<unknown> => within(page.evaluate(`window.postMessage({type: 'pipelineControl', pipeline: 'webcam', enabled: ${enabled}}, window.location.origin)`));

/** What the page reported of its webcam so far. */
export async function camStatus(page: Page): Promise<unknown[]> {
    const got = await within(page.evaluate('window.__camStatus'));
    return Array.isArray(got) ? got.map((v): unknown => v) : [];
}

/** Whether the page's last webcam report is `value` within `timeoutS`. */
export async function waitStatus(page: Page, value: boolean, timeoutS = 20): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if ((await camStatus(page)).at(-1) === value) return true;
        await sleep(250);
    }
    return false;
}

/** A launched page: what closes it, the page, and its errors as they happen. */
export interface CamPage {
    close: () => Promise<void>;
    page: Page;
    errors: string[];
}

/**
 * The dashboard in `engine` with the published camera as its only device.
 * Every engine captures that camera through the interposer rather than an
 * engine-specific fake, so what they are asked to encode is the same.
 */
export async function launchCam(engine: Engine, url: string, camSock: string, mode: string, initJs = ''): Promise<CamPage> {
    const env = interposerEnv(camSock);
    const viewport = { width: 1280, height: 720 };
    let ctx: BrowserContext;
    let browser: Browser | null = null;
    const executable = process.env.E2E_FIREFOX || undefined;
    if (engine === 'firefox') {
        const prefs = {
            'media.navigator.permission.disabled': true,
            // Stated, not omitted: the persistent profile carries whatever a
            // previous run left behind, and a fake camera persisted there would
            // be captured instead of the published one.
            'media.navigator.streams.fake': false,
            'media.autoplay.default': 0,
            'media.autoplay.blocking_policy': 0,
            'media.autoplay.block-webaudio': false,
            'media.gmp-gmpopenh264.enabled': true,
            ...openh264Prefs(),
        };
        const firefoxBase = { headless: true, args: ['--allow-downgrade'], firefoxUserPrefs: prefs, env, ...(executable ? { executablePath: executable, channel: 'moz-firefox' } : {}) };
        if (openh264Version()) {
            // The side-loaded OpenH264 lives in the persistent e2e profile.
            ctx = await firefox.launchPersistentContext(FF_E2E_PROFILE, { ...firefoxBase, viewport });
        } else {
            browser = await firefox.launch(firefoxBase);
            ctx = await browser.newContext({ viewport });
        }
    } else if (engine === 'webkit') {
        // The Safari stand-in. It takes no capture flags, so the grant is the
        // context's and the camera is the published one the interposer puts
        // in front of it, the same as the other two.
        browser = await webkit.launch({ headless: true, env });
        ctx = await browser.newContext({ viewport, permissions: ['camera'] });
    } else {
        // The headless shell has no media capture; the full Chromium build
        // (new headless mode) or the system Chrome E2E_CHROME names is needed.
        const chrome = process.env.E2E_CHROME || undefined;
        browser = await chromium.launch({ headless: true, args: [...BROWSER_ARGS, '--use-fake-ui-for-media-stream'], env, ...(chrome ? { executablePath: chrome } : { channel: 'chromium' }) });
        ctx = await browser.newContext({ viewport, permissions: ['camera'] });
    }
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: CAM_JS });
    if (initJs) await ctx.addInitScript({ content: initJs });
    const page = await ctx.newPage();
    const errors: string[] = [];
    page.on('pageerror', (e) => errors.push(String(e)));
    await page.goto(`${url}/`, { waitUntil: 'load' });
    const owner = browser;
    return { close: () => (owner ?? ctx).close(), page, errors };
}
