/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Selkies' web client on real browsers BrowserStack runs: each case opens the page a server on
// this machine serves over HTTPS through the BrowserStack Local tunnel, waits for the stream,
// then presses a button over the middle of the page and, on a desktop, a key, and checks the X
// display received them where they were made. One server and display per transport, and one
// browser on each at a time, so the input a case reads back is its own.
//
//   BROWSERSTACK_USERNAME=... BROWSERSTACK_ACCESS_KEY=... BROWSERSTACK_LOCAL=/path/to/BrowserStackLocal \
//     node tests/browserstack/smoke.ts
//
// SELKIES_TEST_SERVER names the server (`selkies` on PATH otherwise), SELKIES_WEB_ROOT a web client
// it serves in place of its own (scripts/ci/build-web.sh writes dist/web), BROWSERSTACK_DISPLAYS the X
// displays the WebSockets and WebRTC servers capture (":99,:98"), BROWSERSTACK_CASES a subset of
// the cases ("websockets:edge-windows,..."), and BROWSERSTACK_OUT where the report, the
// screenshots and the server logs go (browserstack-out).

import { appendFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { remote } from 'webdriverio';
import * as stack from './stack.ts';

type Mode = 'websockets' | 'webrtc';

interface Platform {
  caps: WebdriverIO.Capabilities;
  // The session drives a touch screen, and has no keyboard to type with
  touch: boolean;
  // Safari takes the self-signed certificate only through BrowserStack's own command, after
  // the warning has loaded
  safari: boolean;
}

const PLATFORMS: Record<string, Platform> = {
  'edge-windows': {
    caps: { browserName: 'MicrosoftEdge', browserVersion: 'latest', 'bstack:options': { os: 'Windows', osVersion: '11' } },
    touch: false,
    safari: false,
  },
  'safari-macos': {
    caps: { browserName: 'safari', 'bstack:options': { os: 'OS X', osVersion: 'Golden Gate' } },
    touch: false,
    safari: true,
  },
  'chrome-android': {
    caps: { browserName: 'chrome', 'bstack:options': { deviceName: 'Google Pixel 11', osVersion: '17.0', realMobile: true } },
    touch: true,
    safari: false,
  },
};

// WebRTC runs with the server's default ICE servers and no relay, so a case needs a direct path
// between the runner and the browser's network, which no session on BrowserStack's Pixel found
const CASES: { mode: Mode; platform: string }[] = [
  { mode: 'websockets', platform: 'edge-windows' },
  { mode: 'websockets', platform: 'safari-macos' },
  { mode: 'websockets', platform: 'chrome-android' },
  { mode: 'webrtc', platform: 'edge-windows' },
  { mode: 'webrtc', platform: 'safari-macos' },
];

const PORTS: Record<Mode, number> = { websockets: 8443, webrtc: 8444 };
// From the page load: the WebRTC server waits 5 s for a relay candidate before it answers
const STREAM_MS = 60_000;
const INPUT_MS = 8_000;
// How far from the middle of the screen a press made over the middle of the page may land
const CENTER_SLACK = 0.05;
// The client's long press (LONG_PRESS_MS in web/selkies-web-core/lib/input.ts): a lift its page
// handles this long after the touch comes after the long press's right click (#565)
const LONG_PRESS_MS = 750;
// Its tap (_TAP_MAX_DURATION), and how many taps a case makes that the driver held longer
const TAP_MAX_MS = 250;
const TAP_ATTEMPTS = 3;

// A plain script, not a promise: Safari's driver returns one unresolved
const STATE = `const v = document.querySelector('#videoStream') || document.querySelector('video');
return { secure: window.isSecureContext, decoder: typeof VideoDecoder,
  chunks: typeof window.videoChunksReceived === 'number' ? window.videoChunksReceived : null,
  fps: window.fps || 0, video: v ? [v.readyState, v.videoWidth, v.videoHeight] : null,
  paused: v ? v.paused : null, time: v ? v.currentTime : null,
  play: (b => !!b && !b.classList.contains('hidden') && b.offsetParent !== null)(document.querySelector('#playButton')) };`;

// What a failed case reports from the page: the client's status, whether its Play Stream button
// is up, the video's source, and what a script's own play() of the video gets
const DIAGNOSE = `const v = document.querySelector('#videoStream') || document.querySelector('video');
const b = document.querySelector('#playButton');
if (v && !window.__smokePlay) { window.__smokePlay = 'pending';
  v.play().then(() => { window.__smokePlay = 'played'; }, (e) => { window.__smokePlay = e.name; }); }
return { status: (document.querySelector('#status') || {}).textContent || null,
  playButton: b ? b.className || 'shown' : 'none', muted: v ? v.muted : null,
  tracks: v && v.srcObject ? v.srcObject.getTracks().map((t) => t.kind + ':' + t.readyState).join(' ') : null,
  play: window.__smokePlay || null };`;

// Counts of the events the page gets while the case presses, so a press that reaches the page
// but not the display reads apart from one the driver never made
const COUNT_INPUT = `window.__smokeInput = {};
for (const t of ['touchstart', 'touchend', 'pointerdown', 'pointerup', 'mousedown', 'keydown'])
  document.addEventListener(t, (e) => { window.__smokeInput[t] = (window.__smokeInput[t] || 0) + 1;
    if (t === 'touchstart' || t === 'mousedown') { window.__smokeTouchAt = e.timeStamp; window.__smokeTouchWall = Date.now();
      const el = e.target; window.__smokeInput.target = el.id || String(el.className || '').slice(0, 40) || el.tagName; }
    if (t === 'touchend') { window.__smokeInput.heldMs = Math.round(e.timeStamp - window.__smokeTouchAt);
      window.__smokeInput.handledMs = Date.now() - window.__smokeTouchWall; } }, true);`;
const INPUT_COUNTS = `return JSON.stringify(window.__smokeInput || {});`;

interface State {
  secure: boolean;
  decoder: string;
  chunks: number | null;
  fps: number;
  video: [number, number, number] | null;
  paused: boolean | null;
  time: number | null;
  // The client's Play Stream button, over the page while playback waits for a gesture
  play: boolean;
}

interface Result {
  mode: Mode;
  platform: string;
  ok: boolean;
  stream: string;
  input: string;
  note: string;
}

function need(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set`);
  return value;
}

const pause = (ms: number) => new Promise((r) => setTimeout(r, ms));

function streaming(mode: Mode, prev: State | undefined, now: State): boolean {
  if (!now.secure || now.fps <= 0) return false;
  if (mode === 'websockets') return prev?.chunks != null && now.chunks != null && now.chunks > prev.chunks;
  return now.video !== null && now.video[0] >= 2 && now.video[1] > 0 && now.paused === false
    && prev?.time != null && now.time != null && now.time > prev.time;
}

function describe(s: State | undefined): string {
  if (!s) return 'no page state';
  const video = s.video ? `video ${s.video[1]}x${s.video[2]} readyState ${s.video[0]}${s.paused ? ' paused' : ''}` : 'no video element';
  return `secure ${s.secure}, VideoDecoder ${s.decoder}, ${s.fps} fps, chunks ${s.chunks}, ${video}`;
}

function centered(e: stack.XEvent, size: [number, number]): boolean {
  return Math.abs(e.x - size[0] / 2) <= size[0] * CENTER_SLACK && Math.abs(e.y - size[1] / 2) <= size[1] * CENTER_SLACK;
}

async function runCase(mode: Mode, name: string, server: stack.Server, watcher: stack.XWatcher,
  ctx: { user: string; key: string; id: string; out: string; build: string }): Promise<Result> {
  const platform = PLATFORMS[name];
  const result: Result = { mode, platform: name, ok: false, stream: '-', input: '-', note: '' };
  const logFrom = stack.logSize(server.log);
  const caps: WebdriverIO.Capabilities = {
    ...platform.caps,
    acceptInsecureCerts: true,
    'bstack:options': {
      ...(platform.caps['bstack:options'] as object),
      local: true,
      localIdentifier: ctx.id,
      projectName: 'selkies',
      buildName: ctx.build,
      sessionName: `${mode} ${name}`,
      idleTimeout: 120,
    },
  };
  let browser: WebdriverIO.Browser | undefined;
  try {
    browser = await remote({ protocol: 'https', hostname: 'hub-cloud.browserstack.com', port: 443, path: '/wd/hub',
      user: ctx.user, key: ctx.key, capabilities: caps, logLevel: 'warn', connectionRetryTimeout: 300_000 });
    await browser.url(`https://${stack.HOST}:${server.port}/`);
    if (platform.safari) {
      await pause(2000);
      await browser.execute('browserstack_executor: {"action": "acceptSsl"}');
      await pause(3000);
    }

    const deadline = Date.now() + STREAM_MS;
    let prev: State | undefined;
    let now: State | undefined;
    let gesture = false;
    for (;;) {
      await pause(2000);
      prev = now;
      now = (await browser.execute(STATE)) as State;
      // Where the browser's autoplay policy holds playback for a gesture, the user's is a press
      // of the button the client shows for it
      if (now.play && !gesture) {
        gesture = true;
        await (await browser.$('#playButton')).click();
        continue;
      }
      if (streaming(mode, prev, now)) break;
      if (Date.now() > deadline) {
        result.stream = `no stream in ${STREAM_MS / 1000} s: ${describe(now)}`;
        await browser.execute(DIAGNOSE);
        await pause(1500);
        result.stream += `; page ${JSON.stringify(await browser.execute(DIAGNOSE))}`;
        if (mode === 'webrtc') {
          const log = stack.logSince(server.log, logFrom);
          result.note = /rides (udp|tcp)/.test(log) ? 'an ICE path was found' : 'no ICE path between the runner and the browser';
        }
        return result;
      }
    }
    result.stream = describe(now) + (gesture ? ', after the Play Stream gesture' : '');

    const overlay = await browser.$('#overlayInput');
    let press: stack.XEvent | undefined;
    let seen: { heldMs?: number; handledMs?: number } = {};
    let size: [number, number] = [0, 0];
    // BrowserStack's driver holds a short tap anywhere from tens of milliseconds to a second,
    // and a still press the client times past a tap and short of a long press clicks nothing
    // by design: such a tap, as the page's own event timestamps measure it, is made again
    for (let attempt = 1; ; attempt++) {
      await browser.execute(COUNT_INPUT);
      const since = Date.now();
      if (platform.touch) {
        await browser.action('pointer', { parameters: { pointerType: 'touch' } })
          .move({ origin: overlay }).down().pause(60).up().perform();
      } else {
        await overlay.click();
      }
      press = await watcher.wait(since, (e) => e.kind === 'ButtonPress' && (e.button === 1 || platform.touch), INPUT_MS);
      size = await stack.screenSize(server.display);
      seen = JSON.parse((await browser.execute(INPUT_COUNTS)) as string);
      const held = seen.heldMs ?? 0;
      if (press || !platform.touch || held < TAP_MAX_MS || held >= LONG_PRESS_MS || attempt === TAP_ATTEMPTS) break;
    }
    const tap = platform.touch ? `, a tap held ${seen.heldMs} ms and handled ${seen.handledMs} ms apart` : '';
    if (!press) {
      result.input = `no button press reached the display (${size[0]}x${size[1]}); the page got ${JSON.stringify(seen)}`;
      return result;
    }
    if (!centered(press, size)) {
      result.input = `button ${press.button} at ${press.x},${press.y}, not the middle of ${size[0]}x${size[1]}${tap}`;
      return result;
    }
    result.input = `button ${press.button} at ${press.x},${press.y} of ${size[0]}x${size[1]}${tap}`;
    if (press.button !== 1) {
      // A long press's right click, from a tap held or handled past the long press
      if (!(platform.touch && press.button === 3
        && Math.max(seen.heldMs ?? 0, seen.handledMs ?? 0) >= LONG_PRESS_MS)) return result;
      result.note = (seen.heldMs ?? 0) >= LONG_PRESS_MS ? 'a long press: the driver held the tap past it'
        : 'a long press: the page handled the lift past it (#565)';
    }
    if (!platform.touch) {
      const typed = Date.now();
      await browser.keys('q');
      // Xvfb repeats an XTEST press held past its delay, so one press or more
      const key = await watcher.wait(typed, (e) => e.kind === 'KeyPress', INPUT_MS);
      if (!key || key.keysym !== 'q') {
        result.input += key ? `; typed q, the display got ${key.keysym}`
          : `; typed q, no key reached the display; the page got ${await browser.execute(INPUT_COUNTS)}`;
        return result;
      }
      result.input += '; key q';
    }
    result.ok = true;
    return result;
  } catch (e) {
    result.note = String(e).split('\n')[0].slice(0, 300);
    return result;
  } finally {
    if (browser) {
      await browser.saveScreenshot(join(ctx.out, `${mode}-${name}.png`)).catch(() => undefined);
      const status = { status: result.ok ? 'passed' : 'failed', reason: result.ok ? 'streamed and took input' : `${result.stream} ${result.input} ${result.note}`.slice(0, 250) };
      await browser.execute(`browserstack_executor: ${JSON.stringify({ action: 'setSessionStatus', arguments: status })}`).catch(() => undefined);
      await browser.deleteSession().catch(() => undefined);
    }
  }
}

function report(results: Result[], out: string): void {
  const rows = results.map((r) => `| ${r.mode} | ${r.platform} | ${r.ok ? 'pass' : 'FAIL'} | ${r.stream} | ${r.input} | ${r.note} |`);
  const table = ['| Transport | Browser | Result | Stream | Input | Note |', '| --- | --- | --- | --- | --- | --- |', ...rows].join('\n');
  console.log(table);
  writeFileSync(join(out, 'report.json'), JSON.stringify(results, null, 1));
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, `## BrowserStack\n\n${table}\n`);
}

async function main(): Promise<number> {
  const user = need('BROWSERSTACK_USERNAME');
  const key = need('BROWSERSTACK_ACCESS_KEY');
  const tunnelBinary = need('BROWSERSTACK_LOCAL');
  const binary = process.env.SELKIES_TEST_SERVER || 'selkies';
  const picked = process.env.BROWSERSTACK_CASES?.split(',').map((s) => s.trim()).filter(Boolean);
  const cases = CASES.filter((c) => !picked || picked.includes(`${c.mode}:${c.platform}`));
  if (!cases.length) throw new Error(`BROWSERSTACK_CASES matches none of ${CASES.map((c) => `${c.mode}:${c.platform}`).join(', ')}`);
  const [wsDisplay, wrDisplay] = (process.env.BROWSERSTACK_DISPLAYS ?? ':99,:98').split(',');
  const displays: Record<Mode, string> = { websockets: wsDisplay, webrtc: wrDisplay };
  const out = resolve(process.env.BROWSERSTACK_OUT ?? 'browserstack-out');
  mkdirSync(out, { recursive: true });
  // The certificate's key and the tunnel's key file and log stay out of `out`, which CI uploads
  const work = mkdtempSync(join(tmpdir(), 'selkies-browserstack-'));
  const id = `selkies-${process.env.GITHUB_RUN_ID ?? 'local'}-${process.pid}-${Date.now()}`;
  const build = `selkies ${process.env.GITHUB_RUN_ID ? `run ${process.env.GITHUB_RUN_ID}` : 'local'}`;
  const modes = [...new Set(cases.map((c) => c.mode))];
  const servers: stack.Server[] = [];
  const watchers: stack.XWatcher[] = [];
  let tunnelUp = false;
  const cleanup = () => {
    watchers.forEach((w) => w.close());
    servers.forEach((s) => stack.stop(s.proc));
  };
  const abort = () => {
    if (tunnelUp) stack.tunnelStopSync(tunnelBinary, id);
    cleanup();
    rmSync(work, { recursive: true, force: true });
    process.exit(1);
  };
  process.once('SIGTERM', abort);
  process.once('SIGINT', abort);
  try {
    const tls = await stack.certificate(work);
    const ready: Partial<Record<Mode, [stack.Server, stack.XWatcher]>> = {};
    for (const mode of modes) {
      await stack.display(displays[mode]);
      const watcher = new stack.XWatcher(displays[mode]);
      watchers.push(watcher);
      const server = await stack.server(binary, mode, PORTS[mode], displays[mode], tls, work, out);
      servers.push(server);
      ready[mode] = [server, watcher];
    }
    await stack.tunnel(tunnelBinary, key, id, modes.map((m) => PORTS[m]), work);
    tunnelUp = true;
    const ctx = { user, key, id, out, build };
    const results: Result[] = [];
    // The transports side by side, each one's browsers in turn
    await Promise.all(modes.map(async (mode) => {
      const [server, watcher] = ready[mode]!;
      for (const c of cases.filter((k) => k.mode === mode)) {
        const r = await runCase(mode, c.platform, server, watcher, ctx);
        console.log(`${r.ok ? 'PASS' : 'FAIL'} ${mode} ${c.platform}: ${r.stream}; ${r.input}${r.note ? `; ${r.note}` : ''}`);
        results.push(r);
      }
    }));
    results.sort((a, b) => CASES.findIndex((c) => c.mode === a.mode && c.platform === a.platform)
      - CASES.findIndex((c) => c.mode === b.mode && c.platform === b.platform));
    report(results, out);
    const failed = results.filter((r) => !r.ok).length;
    console.log(`${results.length - failed} passed, ${failed} failed`);
    return failed ? 1 : 0;
  } finally {
    if (tunnelUp) await stack.tunnelStop(tunnelBinary, id);
    cleanup();
    rmSync(work, { recursive: true, force: true });
  }
}

process.exitCode = await main().catch((e) => {
  console.error(String(e));
  return 1;
});
