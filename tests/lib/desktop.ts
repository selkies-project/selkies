/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The desktop side the suites drive through command-line tools: the sound
 * server's modules (`pactl`), the Wayland compositor's selection
 * (`wl-clipboard`), and the X display's window manager and windows
 * (`openbox`, `wmctrl`).
 * @module
 */

import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';
import { delimiter, join } from 'node:path';

import { sleep, WORKDIR } from './env.ts';

/**
 * Load a null sink named `name`. Nothing is loaded when a sink of that name
 * exists already: pactl would otherwise make a second one under the same
 * name. A sink left behind outlives the run that made it, so the id goes to
 * `pulseUnload` once finished.
 * @param name The sink's name.
 * @param options Further module arguments, `key=value` each.
 * @returns The module's id, or null when none was loaded.
 */
export function pulseNullSink(name: string, options: Record<string, string> = {}): string | null {
    const existing = spawnSync('pactl', ['list', 'short', 'sinks'], { encoding: 'utf8', timeout: 10_000 }).stdout ?? '';
    if (existing.split('\n').some((line) => line.split('\t')[1] === name)) return null;
    const args = [`sink_name=${name}`, ...Object.entries(options).map(([k, v]) => `${k}=${v}`)];
    const id = (spawnSync('pactl', ['load-module', 'module-null-sink', ...args], { encoding: 'utf8', timeout: 10_000 }).stdout ?? '').trim();
    return /^\d+$/.test(id) ? id : null;
}

/**
 * Play a tone into `sink`. The capture is gated on silence, so a check that
 * audio reaches a client has to give it something to hear. A browser on this
 * host plays what it receives back into the same sink, so once audio flows it
 * sustains itself after the tone is unloaded.
 * @param sink The sink.
 * @param frequency The tone, in Hz.
 * @returns The module's id, to unload; null without `pactl`.
 */
export function pulseSine(sink = 'output', frequency = 440): string | null {
    const run = spawnSync('pactl', ['load-module', 'module-sine', `sink=${sink}`, `frequency=${frequency}`], { encoding: 'utf8', timeout: 10_000 });
    const id = (run.stdout ?? '').trim();
    return /^\d+$/.test(id) ? id : null;
}

/**
 * Unload a module `pulseSine` loaded; nothing for null.
 * @param id The module.
 */
export function pulseUnload(id: string | null): void {
    if (id) spawnSync('pactl', ['unload-module', id], { stdio: 'ignore', timeout: 10_000 });
}

/**
 * The sound server's address: the one `PULSE_SERVER` names, else the one
 * `pactl info` reports.
 * @returns The address, or null without one.
 */
export function pulseServer(): string | null {
    if (process.env.PULSE_SERVER) return process.env.PULSE_SERVER;
    const info = spawnSync('pactl', ['info'], { encoding: 'utf8', timeout: 10_000 }).stdout ?? '';
    const line = info.split('\n').find((l) => l.startsWith('Server String:'));
    return line ? line.slice('Server String:'.length).trim() : null;
}

/**
 * The environment of a wl-clipboard call on `socket`, under the run's
 * runtime directory where the server's compositor listens.
 * @param socket The compositor's socket name.
 * @returns The environment.
 */
function wlEnv(socket: string): NodeJS.ProcessEnv {
    return { ...process.env, WAYLAND_DISPLAY: socket, XDG_RUNTIME_DIR: join(WORKDIR, 'run') };
}

/**
 * The compositor's selection text.
 * @param socket The compositor's socket name.
 * @param timeoutS How long `wl-paste` is given.
 * @returns The text; empty when there is none.
 * @throws {Error} When wl-clipboard is not installed.
 */
export function wlPaste(socket: string, timeoutS = 6): string {
    const run = spawnSync('wl-paste', ['-n'], { encoding: 'utf8', timeout: timeoutS * 1000, env: wlEnv(socket) });
    missing(run.error);
    return run.stdout ?? '';
}

/**
 * Fail on a tool that is not installed, rather than read its absence as an
 * empty clipboard.
 * @param error What starting the tool raised.
 * @throws {Error} When the tool is not on PATH.
 */
function missing(error: Error | undefined): void {
    if (error !== undefined && 'code' in error && error.code === 'ENOENT') {
        throw new Error(`${error.message}: the Wayland clipboard checks need wl-clipboard`);
    }
}

/**
 * Set the compositor's selection, with one retry: `wl-copy` can stall on
 * protocol dispatch while the compositor is under input load, and an attempt
 * still running at its limit is killed.
 * @param socket The compositor's socket name.
 * @param text The text.
 * @param timeoutS How long each attempt is given.
 * @returns Whether it took.
 * @throws {Error} When wl-clipboard is not installed.
 */
export async function wlCopy(socket: string, text: string, timeoutS = 10): Promise<boolean> {
    for (let attempt = 1; attempt <= 2; attempt++) {
        const run = spawnSync('wl-copy', [text], {
            timeout: timeoutS * 1000, killSignal: 'SIGKILL', env: wlEnv(socket), stdio: 'ignore',
        });
        missing(run.error);
        if (run.error === undefined || attempt === 2) return run.status === 0;
        await sleep(1000);
    }
    return false;
}

/**
 * The first executable named `name` on PATH.
 * @param name The program.
 * @returns Its path, or null.
 */
export function onPath(name: string): string | null {
    for (const dir of (process.env.PATH ?? '').split(delimiter)) {
        if (dir && existsSync(join(dir, name))) return join(dir, name);
    }
    return null;
}

/** The test X display, from `E2E_DISPLAY`. */
function testDisplay(): string {
    const display = process.env.E2E_DISPLAY;
    if (!display) throw new Error('E2E_DISPLAY is not set: point it at a throwaway X server for the suites to drive');
    return display;
}

/** A program the suite started, and whether it has exited. */
export interface Started {
    child: ChildProcess;
    /** @returns Its exit code or signal, null while it runs. */
    exited: () => number | string | null;
    /** End it, if it still runs. */
    stop: () => void;
}

/**
 * Start `argv` on the test X display, its output dropped.
 * @param argv The program and its arguments.
 * @returns The running program.
 */
export function startOnDisplay(argv: readonly string[]): Started {
    const [program, ...args] = argv;
    const child = spawn(program, args, { env: { ...process.env, DISPLAY: testDisplay() }, stdio: 'ignore' });
    let ended: number | string | null = null;
    child.on('exit', (code, signal) => { ended = signal ?? code ?? 0; });
    child.on('error', () => { ended = 'error'; });
    return { child, exited: () => ended, stop: () => { if (ended === null) child.kill('SIGTERM'); } };
}

/**
 * The geometry of the session's pcmanfm-qt desktop window on the test X
 * display, through `wmctrl`.
 * @returns `[x, y, w, h]`, or null when no such window runs.
 */
export function desktopWindow(): [number, number, number, number] | null {
    const listing = spawnSync('wmctrl', ['-l', '-G', '-x'], { encoding: 'utf8', env: { ...process.env, DISPLAY: testDisplay() } }).stdout ?? '';
    for (const line of listing.split('\n')) {
        const cols = line.split(/\s+/).filter(Boolean);
        if (cols.length >= 7 && cols[6].startsWith('pcmanfm-qt')) {
            const [x, y, w, h] = cols.slice(2, 6).map(Number);
            return [x, y, w, h];
        }
    }
    return null;
}
