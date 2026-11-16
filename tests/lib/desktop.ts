/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The desktop side the suites drive through command-line tools: the sound
 * server's modules (`pactl`) and the Wayland compositor's selection
 * (`wl-clipboard`).
 * @module
 */

import { spawnSync } from 'node:child_process';
import { join } from 'node:path';

import { sleep, WORKDIR } from './env.ts';

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
 * protocol dispatch while the compositor is under input load.
 * @param socket The compositor's socket name.
 * @param text The text.
 * @param timeoutS How long each attempt is given.
 * @returns Whether it took.
 * @throws {Error} When wl-clipboard is not installed.
 */
export async function wlCopy(socket: string, text: string, timeoutS = 10): Promise<boolean> {
    for (let attempt = 1; attempt <= 2; attempt++) {
        const run = spawnSync('wl-copy', [text], { timeout: timeoutS * 1000, env: wlEnv(socket), stdio: 'ignore' });
        missing(run.error);
        if (run.error === undefined || attempt === 2) return run.status === 0;
        spawnSync('pkill', ['-9', '-f', 'wl-copy'], { stdio: 'ignore' });
        await sleep(1000);
    }
    return false;
}
