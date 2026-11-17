/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the image tier's items share: the client browser, the cell an item runs
// in, the tester site inside the session, and the decoded-picture sample.
//
// A cell is one image x transport x backend x engine. Its client browser is the
// engine a user would run, headed on the slot's own X display (`E2E_DISPLAY`),
// so focus, the X clipboard, and pointer lock are real there: Chrome's focus
// emulation is turned off on every page, and the Firefox profile drops the
// clipboard testing pref the e2e tier sets. WebKit runs headless (see
// `launchClient`), which is one more way it cannot stand in for Safari.
import { spawn, spawnSync } from 'node:child_process';
import { appendFileSync, existsSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { chromium, webkit } from '@playwright/test';

import { fields, isRecord, numberOr, parseJson, stringOr } from '../../web/selkies-web-core/lib/narrow.ts';
import { firefoxPersistentContext, type Engine } from '../lib/browsers.ts';
import type { Checks } from '../lib/checks.ts';
import { REPO, sleep, WORKDIR } from '../lib/env.ts';
import { stripTypes } from '../lib/static.ts';
import { waitWrVideo, waitWsVideo, within, type VideoSize } from '../lib/stream.ts';
import type { Ran, Target } from './target.ts';

const HERE = dirname(fileURLToPath(import.meta.url));

// Inside the session: the tester site's root and port.
export const SITE_ROOT = '/tmp/selkies-imagetest';
export const SITE_PORT = 8765;
export const SITE_URL = `http://127.0.0.1:${SITE_PORT}`;

/**
 * Call the function `script` spells, with `arg`: Playwright for Node takes a
 * string as an expression, so a function in one is called explicitly.
 */
export function callJs(page: Page, script: string, arg?: unknown): Promise<unknown> {
    return page.evaluate(`(${script})(${arg === undefined ? '' : JSON.stringify(arg)})`);
}

/** A word for a shell, quoted. */
export function quote(word: string): string {
    return /^[A-Za-z0-9_./:=@,+%-]+$/.test(word) ? word : `'${word.replaceAll("'", "'\\''")}'`;
}

// The core's messages to the dashboard that the items read back, and the
// sizes of what the page writes to the local clipboard.
export const TAP_JS = `
  window.__itWrites = [];
  if (navigator.clipboard) {
    const wt = navigator.clipboard.writeText && navigator.clipboard.writeText.bind(navigator.clipboard);
    const w = navigator.clipboard.write && navigator.clipboard.write.bind(navigator.clipboard);
    if (wt) navigator.clipboard.writeText = (t) => { window.__itWrites.push({type: 'text/plain', size: t.length}); return wt(t); };
    if (w) navigator.clipboard.write = async (items) => {
      for (const it of items) for (const ty of it.types) {
        try { const b = await it.getType(ty); window.__itWrites.push({type: ty, size: b.size}); } catch (e) {}
      }
      return w(items);
    };
  }
  window.__it = {settings: null, stats: null, roles: [], clip: [], status: null, msgs: {}};
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || !d.type) return;
    window.__it.msgs[d.type] = (window.__it.msgs[d.type] || 0) + 1;
    if (d.type === 'serverSettings') window.__it.settings = d.payload;
    else if (d.type === 'stats') window.__it.stats = d;
    else if (d.type === 'clientRoleUpdate') window.__it.roles.push(d.role);
    else if (d.type === 'clipboardContentUpdate') window.__it.clip.push(d);
    else if (d.type === 'pipelineStatusUpdate') window.__it.status = d;
    else if (d.type === 'gamingModeUpdate') window.__it.gaming = !!d.active;
  });
`;

// The decoded picture, read at its own resolution from whichever sink shows it.
export const SAMPLE_JS = `
(points) => {
  const v = document.querySelector('video');
  let src = null, w = 0, h = 0, kind = '';
  if (v && v.videoWidth > 0 && v.readyState >= 2 && v.style.display !== 'none') {
    src = v; w = v.videoWidth; h = v.videoHeight; kind = 'video';
  } else {
    for (const c of document.querySelectorAll('canvas')) {
      if (c.width >= 320 && c.style.display !== 'none') { src = c; w = c.width; h = c.height; kind = 'canvas'; break; }
    }
  }
  if (!src) return null;
  const oc = document.createElement('canvas'); oc.width = w; oc.height = h;
  const g = oc.getContext('2d', {willReadFrequently: true}); g.drawImage(src, 0, 0, w, h);
  const px = ([fx, fy]) => {
    const x = Math.min(w - 1, Math.round(fx * w)), y = Math.min(h - 1, Math.round(fy * h));
    const d = g.getImageData(x, y, 1, 1).data; return [d[0], d[1], d[2]];
  };
  const row = g.getImageData(Math.round(0.4 * w), Math.round(0.5 * h), 16, 1).data;
  const band = [];
  for (let i = 0; i < 16; i++) band.push([row[i * 4], row[i * 4 + 1], row[i * 4 + 2]]);
  return {kind, w, h, points: points.map(px), band};
}
`;

// How far the decoded picture strays from texture.html's blocks: the mean
// absolute error over every block centre, per channel, in levels.
export const TEXTURE_JS = `
(seed) => {
  const v = document.querySelector('video');
  let src = null, w = 0, h = 0;
  if (v && v.videoWidth > 0 && v.readyState >= 2 && v.style.display !== 'none') { src = v; w = v.videoWidth; h = v.videoHeight; }
  else for (const c of document.querySelectorAll('canvas'))
    if (c.width >= 320 && c.style.display !== 'none') { src = c; w = c.width; h = c.height; break; }
  if (!src) return null;
  const oc = document.createElement('canvas'); oc.width = w; oc.height = h;
  const g = oc.getContext('2d', {willReadFrequently: true}); g.drawImage(src, 0, 0, w, h);
  const d = g.getImageData(0, 0, w, h).data;
  let err = 0, n = 0;
  for (let by = 0; by * 16 + 8 < h; by++)
    for (let bx = 0; bx * 16 + 8 < w; bx++) {
      let k = (Math.imul(bx, 73856093) ^ Math.imul(by, 19349663) ^ Math.imul(seed, 83492791)) >>> 0;
      k = Math.imul(k ^ (k >>> 15), 2246822507) >>> 0;
      const i = ((by * 16 + 8) * w + bx * 16 + 8) * 4;
      err += Math.abs(d[i] - (k & 255)) + Math.abs(d[i + 1] - ((k >>> 8) & 255)) + Math.abs(d[i + 2] - ((k >>> 16) & 255));
      n += 3;
    }
  return Math.round(err / n * 10) / 10;
}
`;

type Rgb = readonly [number, number, number];

// pattern.html's quadrants, as fractions of the picture, and their colors.
export const QUADRANTS: readonly (readonly [readonly [number, number], Rgb])[] = [
    [[0.25, 0.2], [40, 120, 220]], [[0.75, 0.2], [255, 0, 0]],
    [[0.25, 0.8], [20, 160, 60]], [[0.75, 0.8], [230, 230, 230]],
];

// How loud what the page plays is, 0-100: the WebSockets client's own meter, or
// for a WebRTC <video> an analyser over its incoming audio track.
const AUDIO_LEVEL_JS = `() => {
  const v = document.querySelector('video');
  const tracks = v && v.srcObject && v.srcObject.getAudioTracks ? v.srcObject.getAudioTracks() : [];
  if (!tracks.length) return window.currentAudioLevel || 0;
  if (!window.__itAn) {
    const ac = new AudioContext(), an = ac.createAnalyser();
    ac.createMediaStreamSource(new MediaStream(tracks)).connect(an);
    ac.resume(); window.__itAn = an;
  }
  const buf = new Float32Array(2048); window.__itAn.getFloatTimeDomainData(buf);
  let sum = 0; for (const x of buf) sum += x * x;
  return Math.min(100, Math.round(Math.sqrt(sum / buf.length) * 141));
}`;

/** The loudest the page's audio got over `secs` seconds. */
export async function audioLevel(page: Page, secs = 8): Promise<number> {
    let best = 0;
    for (const until = Date.now() + secs * 1000; Date.now() < until;) {
        try {
            best = Math.max(best, Math.trunc(numberOr(await callJs(page, AUDIO_LEVEL_JS)) ?? 0));
        } catch {
            // the page is between states
        }
        await sleep(400);
    }
    return best;
}

/** A decoded-picture sample: its sink, size, the points asked for, and the chroma band. */
export interface Sample {
    kind: string;
    w: number;
    h: number;
    points: number[][];
    band: number[][];
}

function asSample(value: unknown): Sample | null {
    if (!isRecord(value)) return null;
    const rows = (v: unknown): number[][] => (Array.isArray(v) ? v.map((p) => (Array.isArray(p) ? p.map((n) => numberOr(n) ?? 0) : [])) : []);
    return {
        kind: stringOr(value.kind) ?? '', w: numberOr(value.w) ?? 0, h: numberOr(value.h) ?? 0,
        points: rows(value.points), band: rows(value.band),
    };
}

export function near(rgb: readonly number[] | undefined, want: Rgb, tolerance = 28): boolean {
    return Array.isArray(rgb) && rgb.length >= 3 && want.every((b, i) => Math.abs(rgb[i] - b) <= tolerance);
}

/** Whether a decoded picture shows pattern.html's four quadrants. */
export function patternMatches(sample: Sample | null): boolean {
    return sample !== null && QUADRANTS.every(([, want], i) => near(sample.points[i], want));
}

/** Whether the one-pixel red and blue columns survived (4:4:4), or null off the band. */
export function chromaKept(sample: Sample | null): boolean | null {
    if (!sample?.band.length) return null;
    const reds = sample.band.filter((p) => p[0] > 150 && p[2] < 100).length;
    const blues = sample.band.filter((p) => p[2] > 150 && p[0] < 100).length;
    return reds >= 4 && blues >= 4;
}

/** The slot's X display, which the client browser starts on. */
export function requireDisplay(): string {
    const display = process.env.E2E_DISPLAY;
    if (!display) throw new Error('E2E_DISPLAY is not set: the image tier runs headed on the slot display');
    return display;
}

/** The environment a client browser starts in: the slot's X display, never the shell's. */
export function clientEnv(): Record<string, string | undefined> {
    return { ...process.env, DISPLAY: requireDisplay() };
}

/** A steady 440 Hz tone for Chrome's fake microphone, whose own beeps are too sparse to measure. */
export function toneWav(): string {
    const path = join(WORKDIR, 'tone-440.wav');
    if (existsSync(path)) return path;
    const rate = 48_000;
    const samples = rate * 2;
    const data = Buffer.alloc(samples * 2);
    for (let i = 0; i < samples; i += 1) data.writeInt16LE(Math.trunc(12_000 * Math.sin((2 * Math.PI * 440 * i) / rate)), i * 2);
    const head = Buffer.alloc(44);
    head.write('RIFF', 0);
    head.writeUInt32LE(36 + data.length, 4);
    head.write('WAVEfmt ', 8);
    head.writeUInt32LE(16, 16);
    head.writeUInt16LE(1, 20);
    head.writeUInt16LE(1, 22);
    head.writeUInt32LE(rate, 24);
    head.writeUInt32LE(rate * 2, 28);
    head.writeUInt16LE(2, 32);
    head.writeUInt16LE(16, 34);
    head.write('data', 36);
    head.writeUInt32LE(data.length, 40);
    writeFileSync(path, Buffer.concat([head, data]));
    return path;
}

/** What closes a client browser, and the context its pages open in. */
export interface Client {
    closer: Browser | BrowserContext;
    ctx: BrowserContext;
}

/**
 * A headed client browser for `engine` on the slot's display. It accepts the
 * session's self-signed certificate, plays media without a gesture, and offers
 * fake camera and microphone devices, as the checklist's testers do with their
 * own browsers' flags.
 */
export async function launchClient(engine: Engine): Promise<Client> {
    const env = clientEnv();
    if (engine === 'chromium') {
        const args = ['--ignore-certificate-errors', '--use-fake-device-for-media-stream',
            `--use-file-for-fake-audio-capture=${toneWav()}`,
            '--use-fake-ui-for-media-stream', '--autoplay-policy=no-user-gesture-required',
            '--no-first-run', '--no-default-browser-check', '--password-store=basic',
            '--window-position=0,0'];
        const browser = await chromium.launch({ headless: false, args, env, executablePath: process.env.E2E_CHROME || undefined });
        const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1, ignoreHTTPSErrors: true });
        // What a user's "Allow" on Chrome's clipboard prompt grants once.
        await ctx.grantPermissions(['clipboard-read', 'clipboard-write']);
        return { closer: browser, ctx };
    }
    if (engine === 'firefox') {
        const prefs = { 'dom.events.testing.asyncClipboard': false, 'media.navigator.streams.fake': true, 'media.navigator.permission.disabled': true };
        const ctx = await firefoxPersistentContext({ viewport: { width: 1280, height: 720 }, prefs, headed: true, env, ignoreHTTPSErrors: true });
        return { closer: ctx, ctx };
    }
    // Headed WebKitGTK's web process dies on an Xvfb display (no DRI3), so
    // WebKit runs headless as in the e2e tier: no X focus or clipboard of its own.
    const browser = await webkit.launch({ headless: true, env });
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1, ignoreHTTPSErrors: true });
    try {
        await ctx.grantPermissions(['camera', 'microphone']);
    } catch {
        // WebKit without the devices
    }
    return { closer: browser, ctx };
}

/** What an image brings, read once per target. */
export type Facts = Record<string, unknown>;

/** `path` on the tester site for the browser following `ch`, made unique so it reloads. */
export function channelUrl(path: string, ch: string): string {
    return `${path}${path.includes('?') ? '&' : '?'}ch=${ch}&n=${process.hrtime.bigint()}`;
}

/** One image x transport x backend x engine, as an item sees it. */
export class Cell {
    readonly id: string;
    readonly target: Target;
    readonly label: string;
    readonly transport: string;
    readonly backend: string;
    readonly engine: string;
    readonly ctx: BrowserContext | null;
    readonly facts: Facts;
    readonly outdir: string;
    res!: Checks;

    constructor(target: Target, label: string, transport: string, backend: string, engine: string, ctx: BrowserContext | null, facts: Facts, outdir: string) {
        this.target = target;
        this.label = label;
        this.transport = transport;
        this.backend = backend;
        this.engine = engine;
        this.ctx = ctx;
        this.facts = facts;
        this.outdir = outdir;
        this.id = `${label}/${transport}/${backend}/${engine}`;
    }

    // The client side.

    /**
     * A new page of the session in the cell's browser (or `ctx`), with the
     * `init` scripts installed before the client starts; its stream up when `wait`.
     */
    async open(urlHash = '', options: { wait?: boolean; timeoutS?: number; init?: readonly string[]; ctx?: BrowserContext } = {}): Promise<Page> {
        const ctx = options.ctx ?? this.ctx;
        if (!ctx) throw new Error('a cell without a browser opens no page');
        // A browser whose renderer died can leave a new page waiting forever.
        const page = await within(ctx.newPage(), 'the browser', 60);
        await page.addInitScript({ content: TAP_JS });
        for (const script of options.init ?? []) await page.addInitScript({ content: script });
        if (process.env.E2E_IMAGE_CONSOLE) {
            const log = join(this.outdir, `${this.id.replaceAll('/', '-')}-console.log`);
            page.on('console', (m) => { appendFileSync(log, `${new Date().toISOString().slice(11, 19)} ${m.type()}: ${m.text()}\n`); });
        }
        await page.goto(`${this.target.url}/${urlHash}`, { waitUntil: 'load', timeout: 60_000 });
        if (this.engine === 'chromium') {
            const cdp = await ctx.newCDPSession(page);
            await cdp.send('Emulation.setFocusEmulationEnabled', { enabled: false });
        }
        if (options.wait ?? true) await this.video(page, options.timeoutS ?? 45);
        return page;
    }

    /** The stream's picture size once a frame arrived, else null. */
    video(page: Page, timeoutS = 45): Promise<VideoSize | null> {
        return this.transport === 'webrtc' ? waitWrVideo(page, timeoutS) : waitWsVideo(page, timeoutS);
    }

    async sample(page: Page, points?: readonly (readonly [number, number])[]): Promise<Sample | null> {
        try {
            return asSample(await callJs(page, SAMPLE_JS, points ?? QUADRANTS.map(([p]) => p)));
        } catch {
            return null;
        }
    }

    /** The decoded picture once it shows the session's pattern page, else the last sample. */
    async waitPattern(page: Page, timeoutS = 20): Promise<Sample | null> {
        let s: Sample | null = null;
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
            s = await this.sample(page);
            if (patternMatches(s)) return s;
            await sleep(500);
        }
        return s;
    }

    async textureError(page: Page, seed = 1): Promise<number | null> {
        try {
            return numberOr(await callJs(page, TEXTURE_JS, seed)) ?? null;
        } catch {
            return null;
        }
    }

    async settings(page: Page): Promise<Record<string, unknown>> {
        return fields(await page.evaluate('window.__it && window.__it.settings || {}'));
    }

    async shot(page: Page, name: string): Promise<string> {
        const path = join(this.outdir, `${this.id.replaceAll('/', '-')}-${name}.png`);
        try {
            await page.screenshot({ path });
        } catch {
            // a page gone
        }
        return path;
    }

    /** A command on the client's X display (xdotool, xclip): what a local app does there. */
    xclient(args: readonly string[], data?: Buffer, timeoutS = 15): Promise<Ran> {
        return new Promise((resolve) => {
            const child = spawn(args[0], args.slice(1), { env: clientEnv(), stdio: ['pipe', 'pipe', 'pipe'] });
            const out: Buffer[] = [];
            const err: Buffer[] = [];
            child.stdout.on('data', (b: Buffer) => { out.push(b); });
            child.stderr.on('data', (b: Buffer) => { err.push(b); });
            child.stdin.on('error', () => { /* a command that reads nothing */ });
            child.stdin.end(data ?? Buffer.alloc(0));
            let timedOut = false;
            let done = false;
            const finish = (code: number | null): void => {
                if (done) return;
                done = true;
                clearTimeout(timer);
                child.stdout.destroy();
                child.stderr.destroy();
                resolve({ code: timedOut ? 124 : (code ?? 1), stdout: Buffer.concat(out), stderr: Buffer.concat(err) });
            };
            const timer = setTimeout(() => { timedOut = true; child.kill('SIGKILL'); }, timeoutS * 1000);
            // `xclip -i` forks a child that keeps the pipes open while it owns the
            // selection: the command is done when it exits, with a moment for its output.
            child.once('exit', (code) => { setTimeout(() => { finish(code); }, 1000); });
            child.once('close', finish);
            child.once('error', () => { finish(127); });
        });
    }

    // The session side.

    /** Move the session browser following channel `ch` to `path` on the tester site. */
    async go(path: string, ch = 'main'): Promise<void> {
        await this.target.sh(`echo ${quote(channelUrl(path, ch))} > ${SITE_ROOT}/next-${ch}`);
    }

    /** The tester page's latest report `name`; with `where`, the first one it accepts. */
    async report(name: string, timeoutS = 15, where?: (report: Record<string, unknown>) => boolean): Promise<Record<string, unknown> | null> {
        let last: Record<string, unknown> | null = null;
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
            const raw = await this.target.out(`cat ${SITE_ROOT}/out/${name}.json 2>/dev/null`);
            if (raw) {
                const parsed = parseJson(raw);
                last = isRecord(parsed) ? parsed : null;
                if (last !== null && (!where || where(last))) return last;
            }
            await sleep(500);
        }
        return where ? null : last;
    }

    async clearReport(name: string): Promise<void> {
        await this.target.sh(`rm -f ${SITE_ROOT}/out/${name}.json`);
    }
}

/** Open the dashboard's sidebar, as its edge handle does; whether it is open. */
export async function openSidebar(page: Page): Promise<boolean> {
    const open = (): Promise<boolean> => page.evaluate(() => Boolean(document.querySelector('.sidebar.is-open')));
    if (!await open()) {
        try {
            await page.locator('.toggle-handle').first().click({ timeout: 3000 });
        } catch {
            await page.evaluate(() => { window.postMessage({ type: 'toggleDashboard' }, window.location.origin); });
        }
        await sleep(800);
    }
    return open();
}

/**
 * Bring a sidebar control into the DOM: open the sidebar, then try each
 * collapsed section until the control renders. Whether it is there.
 */
export async function reveal(page: Page, sel: string): Promise<boolean> {
    if (await page.locator(sel).count()) return true;
    await openSidebar(page);
    const heads = page.locator('.sidebar-section-header');
    const n = await heads.count();
    for (let i = 0; i < n; i += 1) {
        if (await page.locator(sel).count()) break;
        try {
            await heads.nth(i).scrollIntoViewIfNeeded({ timeout: 2000 });
            await heads.nth(i).click({ timeout: 3000 });
            await sleep(500);
            if (!await page.locator(sel).count()) {
                await heads.nth(i).click({ timeout: 3000 });
                await sleep(300);
            }
        } catch {
            // the next section
        }
    }
    return (await page.locator(sel).count()) > 0;
}

const CLIP_ARGS = '-f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 '
    + '-t 12 -c:v libvpx -b:v 800k -c:a libopus -b:a 96k -shortest';

const HELPER = 'selkies-image-site';

/**
 * The in-session helper (crates/selkies-image-site) for a session whose
 * `uname -m` is `machine`: `SELKIES_IMAGE_SITE`, else the static musl build in
 * the target directory cargo uses, built first where cargo is at hand.
 * @throws {Error} When it is neither named nor built.
 */
export function helperFor(machine: string): string {
    const named = process.env.SELKIES_IMAGE_SITE;
    if (named) return named;
    const triple = `${machine}-unknown-linux-musl`;
    const target = process.env.CARGO_TARGET_DIR || join(REPO, 'target');
    const built = join(target, triple, 'release', HELPER);
    const cargo = spawnSync('cargo', ['build', '--quiet', '--locked', '--release', '-p', HELPER, '--target', triple],
        { cwd: REPO, stdio: ['ignore', 'inherit', 'inherit'] });
    if (cargo.error === undefined && cargo.status !== 0) throw new Error(`cargo build of ${HELPER} for ${triple} exited ${cargo.status}`);
    if (!existsSync(built)) throw new Error(`${built} is not built and cargo is not on PATH to build it (rustup target add ${triple})`);
    return built;
}

/**
 * Copy the in-session helper into the session, once.
 * @returns Its path there.
 */
export async function installHelper(target: Target): Promise<string> {
    const path = `${SITE_ROOT}/${HELPER}`;
    const machine = await target.out('uname -m');
    if (!await target.put(path, readFileSync(helperFor(machine || 'x86_64')))) throw new Error(`the helper did not reach ${path}`);
    await target.sh(`chmod 755 ${path}`);
    return path;
}

/**
 * Copy the tester site into the session and serve it on loopback there, with a
 * clip (a moving picture and a tone) made by whichever side has ffmpeg. A .ts
 * file goes as the classic .js script the pages load, its types stripped.
 */
export async function installSite(target: Target): Promise<boolean> {
    const site = join(HERE, 'site');
    for (const entry of readdirSync(site)) {
        if (!statSync(join(site, entry)).isFile()) continue;
        let name = entry;
        let data = readFileSync(join(site, entry));
        if (name.endsWith('.ts')) {
            name = `${name.slice(0, -3)}.js`;
            data = Buffer.from(stripTypes(data.toString()));
        }
        await target.put(`${SITE_ROOT}/site/${name}`, data);
    }
    const clip = join(WORKDIR, 'av.webm');
    if (!existsSync(clip)) spawnSync('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-y', ...CLIP_ARGS.split(' '), clip], { stdio: 'ignore', timeout: 120_000 });
    if (existsSync(clip)) await target.put(`${SITE_ROOT}/site/av.webm`, readFileSync(clip));
    else await target.sh(`ffmpeg -hide_banner -loglevel error -y ${CLIP_ARGS} ${SITE_ROOT}/site/av.webm`, 120);
    const helper = await installHelper(target);
    // A shell of its own for the kill: the pattern would match the launch below.
    await target.sh(`pkill -f '[s]elkies-image-site serve ${SITE_ROOT}'; sleep 0.5`);
    await target.sh(`mkdir -p ${SITE_ROOT}/out; cd ${SITE_ROOT} && (setsid nohup ${helper} serve ${SITE_ROOT} ${SITE_PORT} > site.log 2>&1 &)`);
    for (const until = Date.now() + 15_000; Date.now() < until;) {
        if ((await target.sh(`curl -sf -o /dev/null ${SITE_URL}/it.js`)).code === 0) return true;
        await sleep(500);
    }
    return false;
}

// The session's own browsers, started on the desktop like a user's.
const SESSION_CHROME = 'google-chrome chromium chromium-browser';
const SESSION_FLAGS = '--no-first-run --no-default-browser-check --password-store=basic --disable-features=Translate '
    + '--autoplay-policy=no-user-gesture-required --use-fake-ui-for-media-stream '
    + '--disable-session-crashed-bubble --test-type';

/**
 * Start the session's Chrome (or Firefox) on the tester site, following the
 * channel named after its profile, Chrome with `flags` added; whether the
 * browser is installed.
 */
export async function sessionBrowser(target: Target, options: { which?: string; start?: string; kiosk?: boolean; profile?: string; flags?: string } = {}): Promise<boolean> {
    const { which = 'chrome', start = '/pattern.html', kiosk = true, profile = 'main', flags = '' } = options;
    const url = `${SITE_URL}${channelUrl(start, profile)}`;
    await target.sh(`rm -f ${SITE_ROOT}/next-${profile}`);
    let cmd: string;
    if (which === 'chrome') {
        const exe = await target.out(`for b in ${SESSION_CHROME}; do command -v $b && break; done`);
        if (!exe) return false;
        // A kiosk fills the screen; otherwise an app window, which has no tabs or toolbar.
        const mode = kiosk ? `--kiosk ${quote(url)}` : `--app=${quote(url)}`;
        cmd = `${exe} ${SESSION_FLAGS} ${flags} --user-data-dir=${SITE_ROOT}/chrome-${profile} ${mode}`;
    } else {
        const exe = await target.out('command -v firefox');
        if (!exe) return false;
        const prof = `${SITE_ROOT}/firefox-${profile}`;
        const prefs = 'user_pref("media.autoplay.default", 0);\n'
            + 'user_pref("media.navigator.permission.disabled", true);\n'
            + 'user_pref("browser.shell.checkDefaultBrowser", false);\n'
            + 'user_pref("datareporting.policy.dataSubmissionEnabled", false);\n'
            + 'user_pref("browser.aboutwelcome.enabled", false);\n';
        await target.put(`${prof}/user.js`, Buffer.from(prefs));
        cmd = `${exe} --no-remote --profile ${prof} ${kiosk ? '--kiosk ' : ''}${quote(url)}`;
    }
    await target.sh(`(setsid nohup ${cmd} > ${SITE_ROOT}/${which}-${profile}.log 2>&1 &)`);
    return true;
}

/** End a session browser `sessionBrowser` started, by its profile directory. */
export async function stopSessionBrowser(target: Target, which = 'chrome', profile = 'main'): Promise<void> {
    const flag = which === 'chrome' ? `user-data-dir=${SITE_ROOT}/chrome-${profile}` : `${SITE_ROOT}/firefox-${profile}`;
    // The bracket keeps the pattern from matching this shell's own command line;
    // the wait lets the profile's lock go before the next start reuses it.
    const pattern = quote(`[${flag[0]}]${flag.slice(1)}`);
    await target.sh(`pkill -f -- ${pattern}; for i in $(seq 40); do pgrep -f -- ${pattern} > /dev/null || break; `
        + `sleep 0.25; done; pkill -9 -f -- ${pattern}; true`, 30);
    await sleep(500);
}
