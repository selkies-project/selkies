/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The runner's half of the BrowserStack smoke: the certificate the page is served with, an X
// display and a server per transport, a window on each display that records the input the
// server injects, and the BrowserStack Local tunnel the remote browsers reach the servers by.

import { execFile, execFileSync, spawn, type ChildProcess } from 'node:child_process';
import { mkdirSync, openSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { request } from 'node:https';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const run = promisify(execFile);
export const ROOT = fileURLToPath(new URL('../..', import.meta.url));
// BrowserStack sends Safari and iOS requests for localhost to this name, which resolves to the
// loopback address; the certificate names both
export const HOST = 'bs-local.com';

export async function certificate(dir: string): Promise<{ cert: string; key: string }> {
  const cert = join(dir, 'cert.pem');
  const key = join(dir, 'key.pem');
  await run('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes',
    '-days', '2', '-subj', `/CN=${HOST}`, '-addext', `subjectAltName=DNS:${HOST},DNS:localhost,IP:127.0.0.1`,
    '-keyout', key, '-out', cert]);
  return { cert, key };
}

// The sound server's address, which the servers' runtime directories of their own would hide
async function pulseServer(): Promise<string | undefined> {
  if (process.env.PULSE_SERVER) return process.env.PULSE_SERVER;
  const { stdout } = await run('pactl', ['info']);
  return /^Server String:\s*(.+)$/m.exec(stdout)?.[1];
}

export async function display(name: string): Promise<void> {
  await run(join(ROOT, 'scripts/ci/test-stack.sh'), [], { env: { ...process.env, E2E_DISPLAY: name } });
}

export async function screenSize(name: string): Promise<[number, number]> {
  const { stdout } = await run('xdpyinfo', ['-display', name]);
  const m = /dimensions:\s+(\d+)x(\d+) pixels/.exec(stdout);
  if (!m) throw new Error(`xdpyinfo gave no dimensions for ${name}`);
  return [Number(m[1]), Number(m[2])];
}

export interface Server {
  mode: string;
  port: number;
  display: string;
  log: string;
  proc: ChildProcess;
}

// Over the run's own certificate, which names the loopback address
function answers(port: number, ca: string): Promise<boolean> {
  return new Promise((resolve) => {
    const req = request({ host: '127.0.0.1', port, path: '/api/status', ca, timeout: 2000 },
      (res) => { res.resume(); resolve(res.statusCode === 200); });
    req.on('error', () => resolve(false));
    req.on('timeout', () => { req.destroy(); resolve(false); });
    req.end();
  });
}

// A server from a clean environment, as the e2e tier starts one, so nothing the shell running
// the smoke sets reconfigures it
export async function server(binary: string, mode: string, port: number, displayName: string,
  tls: { cert: string; key: string }, dir: string, logDir: string): Promise<Server> {
  const home = join(dir, `home-${mode}`);
  const runtime = join(dir, `run-${mode}`);
  mkdirSync(home, { recursive: true });
  mkdirSync(runtime, { recursive: true, mode: 0o700 });
  const pulse = await pulseServer();
  const env: Record<string, string> = {
    PATH: process.env.PATH ?? '/usr/bin:/bin',
    HOME: home,
    XDG_RUNTIME_DIR: runtime,
    DISPLAY: displayName,
    ...(pulse ? { PULSE_SERVER: pulse } : {}),
    SELKIES_PORT: String(port),
    SELKIES_MODE: mode,
    SELKIES_ENABLE_BASIC_AUTH: 'false',
    // The page needs a secure context for WebCodecs
    SELKIES_ENABLE_HTTPS: 'true',
    SELKIES_HTTPS_CERT: tls.cert,
    SELKIES_HTTPS_KEY: tls.key,
    // Every frame encoded, so a still test screen still streams
    SELKIES_VIDEO_STREAMING_MODE: 'true',
    // A web client build of the tree's own in place of the one the server embeds
    ...(process.env.SELKIES_WEB_ROOT ? { SELKIES_WEB_ROOT: process.env.SELKIES_WEB_ROOT } : {}),
  };
  const log = join(logDir, `server-${mode}.log`);
  const out = openSync(log, 'w');
  const proc = spawn(binary, [], { env, stdio: ['ignore', out, out], detached: true });
  const ca = readFileSync(tls.cert, 'utf8');
  const deadline = Date.now() + 75_000;
  while (Date.now() < deadline) {
    if (proc.exitCode !== null) throw new Error(`the ${mode} server exited during startup; see ${log}`);
    if (await answers(port, ca)) return { mode, port, display: displayName, log, proc };
    await new Promise((r) => setTimeout(r, 500));
  }
  proc.kill('SIGTERM');
  throw new Error(`the ${mode} server did not come up in time; see ${log}`);
}

export function stop(proc: ChildProcess): void {
  if (proc.exitCode === null && proc.pid) {
    try {
      process.kill(proc.pid, 'SIGTERM');
    } catch {
      // gone already
    }
  }
}

export function logSize(path: string): number {
  return statSync(path).size;
}

export function logSince(path: string, offset: number): string {
  return readFileSync(path).subarray(offset).toString();
}

export interface XEvent {
  at: number;
  kind: 'KeyPress' | 'ButtonPress';
  x: number;
  y: number;
  keysym?: string;
  button?: number;
}

// A window over the whole screen, mapped without a window manager: it gets the buttons pressed
// over it and, the focus following the pointer, the keys, each decoded the way an application
// decodes it
export class XWatcher {
  events: XEvent[] = [];
  private proc: ChildProcess;

  constructor(displayName: string) {
    this.proc = spawn('stdbuf', ['-oL', 'xev', '-display', displayName, '-geometry', '8192x4096+0+0',
      '-event', 'keyboard', '-event', 'button'], { stdio: ['ignore', 'pipe', 'ignore'] });
    let block: string[] = [];
    createInterface({ input: this.proc.stdout! }).on('line', (line) => {
      if (line.trim()) {
        block.push(line);
        return;
      }
      this.parse(block.join(' '));
      block = [];
    });
  }

  private parse(text: string): void {
    const kind = /^(KeyPress|ButtonPress) event/.exec(text)?.[1] as XEvent['kind'] | undefined;
    const root = /root:\((-?\d+),(-?\d+)\)/.exec(text);
    if (!kind || !root) return;
    const keysym = /\(keysym 0x[0-9a-f]+, ([^)]+)\)/.exec(text)?.[1];
    const button = /button (\d+)/.exec(text)?.[1];
    this.events.push({ at: Date.now(), kind, x: Number(root[1]), y: Number(root[2]), keysym,
      button: button ? Number(button) : undefined });
  }

  async wait(since: number, match: (e: XEvent) => boolean, ms: number): Promise<XEvent | undefined> {
    const deadline = Date.now() + ms;
    for (;;) {
      const hit = this.events.find((e) => e.at >= since && match(e));
      if (hit || Date.now() > deadline) return hit;
      await new Promise((r) => setTimeout(r, 100));
    }
  }

  close(): void {
    this.proc.kill('SIGTERM');
  }
}

// The tunnel, taking the key from a file only this user reads so it stays off the command line,
// and reaching the servers' ports and nothing else
export async function tunnel(binary: string, key: string, id: string, ports: number[], dir: string): Promise<void> {
  const config = join(dir, 'tunnel.yml');
  writeFileSync(config, `key: ${JSON.stringify(key)}\n`, { mode: 0o600 });
  const only = ports.flatMap((p) => [HOST, String(p), '1', 'localhost', String(p), '1']).join(',');
  try {
    const { stdout } = await run(binary, ['--config-file', config, '--local-identifier', id, '--only', only,
      '--disable-dashboard', '--log-file', join(dir, 'tunnel.log'), '--daemon', 'start'], { timeout: 120_000 });
    const state = JSON.parse(stdout.trim().split('\n').pop() ?? '{}');
    if (state.state !== 'connected') throw new Error(`the tunnel did not connect: ${state.message?.message ?? stdout}`);
  } finally {
    rmSync(config, { force: true });
  }
}

export async function tunnelStop(binary: string, id: string): Promise<void> {
  await run(binary, ['--local-identifier', id, '--daemon', 'stop'], { timeout: 60_000 }).catch(() => undefined);
}

export function tunnelStopSync(binary: string, id: string): void {
  try {
    execFileSync(binary, ['--local-identifier', id, '--daemon', 'stop'], { timeout: 30_000, stdio: 'ignore' });
  } catch {
    // stopped already, or never started
  }
}
