/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// What the pointer-lock and gaming-mode suites read a game's input through:
// the relative motion a page put on the wire, and an SDL2 window in relative
// mouse mode over the session's desktop (tools/sdl_relative_probe.c) standing
// in for the game.
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { openSync, writeSync } from 'node:fs';
import { join } from 'node:path';
import { createInterface } from 'node:readline';

import type { Page } from '@playwright/test';

import { sleep, WORKDIR } from './env.ts';
import { builtTool } from './keys.ts';
import { within } from './stream.ts';

declare global {
    interface Window {
        __wireSent?: unknown[];
    }
}

type Pt = [number, number];

/** Motion messages off the shared wire tap, whichever thread owns the socket. */
export function moves(page: Page): Promise<string[]> {
    return within(page.evaluate(() => (window.__wireSent ?? []).filter((d): d is string => typeof d === 'string' && (d.startsWith('m,') || d.startsWith('m2,')))));
}

export const movesSince = async (page: Page, start: number): Promise<string[]> => (await moves(page)).slice(start);

const exited = (proc: ChildProcess): boolean => proc.exitCode !== null || proc.signalCode !== null;

/**
 * Whether a window manager holds `display`, by the check window it publishes.
 * @param display The X display.
 * @returns Whether one does.
 */
export function x11Managed(display: string): boolean {
    const out = spawnSync('xprop', ['-root', '_NET_SUPPORTING_WM_CHECK'], { encoding: 'utf8', timeout: 5000, env: { ...process.env, DISPLAY: display } }).stdout ?? '';
    return out.includes('window id');
}

export type Line = Record<string, unknown>;

/**
 * The SDL2 relative-mode window on the server's display, its JSON lines
 * collected as they come. On Wayland it connects to `socket`: the capture
 * compositor's, or a nested session's.
 */
export class GameProbe {
    readonly lines: Line[] = [];
    readonly noise: string[] = [];
    private readonly proc: ChildProcess;

    constructor(wayland: boolean, runtime: string, socket: string | null) {
        const env: NodeJS.ProcessEnv = { PATH: process.env.PATH ?? '', HOME: process.env.HOME ?? '', XDG_RUNTIME_DIR: runtime };
        if (process.env.SDL2_LIB) env.SDL2_LIB = process.env.SDL2_LIB;
        if (wayland) {
            env.SDL_VIDEODRIVER = 'wayland';
            env.WAYLAND_DISPLAY = socket ?? '';
        } else {
            env.SDL_VIDEODRIVER = 'x11';
            env.DISPLAY = process.env.E2E_DISPLAY ?? '';
        }
        // Kept as a log the runner collects, so a run that saw nothing says what it did see.
        const log = openSync(join(WORKDIR, 'game-probe.log'), 'w');
        this.proc = spawn(builtTool('sdl_relative_probe', [], ['-ldl']), ['120'], { env, stdio: ['ignore', 'pipe', 'pipe'] });
        const take = (stream: NodeJS.ReadableStream | null): void => {
            if (!stream) return;
            createInterface({ input: stream }).on('line', (raw) => {
                const line = raw.trim();
                try {
                    writeSync(log, `${raw}\n`);
                } catch {
                    // the log is a courtesy
                }
                if (line.startsWith('{')) {
                    try {
                        const parsed: unknown = JSON.parse(line);
                        if (parsed !== null && typeof parsed === 'object') {
                            this.lines.push({ ...parsed });
                            return;
                        }
                    } catch {
                        // not a line of the probe's
                    }
                }
                if (line) this.noise.push(line);
            });
        };
        take(this.proc.stdout);
        take(this.proc.stderr);
    }

    /** First line of `kind` whose fields equal `match`, or null on timeout. */
    async wait(kind: string, timeoutS: number, match: Line = {}): Promise<Line | null> {
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
            const hit = this.lines.find((l) => l.kind === kind && Object.entries(match).every(([k, v]) => l[k] === v));
            if (hit) return hit;
            if (exited(this.proc)) break;
            await sleep(50);
        }
        return null;
    }

    /** Why the window could not run, when it said so. */
    unavailable(): string {
        const said = this.lines.find((l) => l.kind === 'unavailable');
        if (said) return typeof said.reason === 'string' ? said.reason : 'unavailable';
        return this.noise.join(' ').slice(0, 160);
    }

    /** The `kind` lines after index `start`, waited for until `count` are in. */
    async events(kind: string, start: number, count: number, timeoutS: number): Promise<Line[]> {
        const got = (): Line[] => this.lines.slice(start).filter((l) => l.kind === kind);
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until && got().length < count;) await sleep(50);
        return got();
    }

    async stop(): Promise<void> {
        if (exited(this.proc)) return;
        const gone = new Promise<void>((resolve) => { this.proc.once('exit', () => { resolve(); }); });
        this.proc.kill('SIGTERM');
        const timer = setTimeout(() => this.proc.kill('SIGKILL'), 5000);
        await gone;
        clearTimeout(timer);
    }
}

/** The non-zero relative deltas the wire carried, in order. */
export function wireDeltas(messages: readonly string[]): Pt[] {
    return messages.filter((m) => m.startsWith('m2,')).map((m): Pt => {
        const parts = m.split(',');
        return [Number(parts[1]), Number(parts[2])];
    }).filter(([dx, dy]) => dx !== 0 || dy !== 0);
}
