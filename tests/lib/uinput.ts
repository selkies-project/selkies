/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The /dev/uinput emulator the gamepad suites put under the server
 * (`tests/tools/uinput_shim.c`), which records the ioctls and the event
 * writes a kernel gamepad would receive, and the synthetic W3C gamepad the
 * page is given to press (`tests/tools/pad_init.ts`).
 * @module
 */

import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { TOOLS, WORKDIR } from './env.ts';
import { stripTypes } from './static.ts';

declare global {
    interface Window {
        /** Press (1) or release (0) a button of the synthetic pad (`pad_init.ts`). */
        __padPress: (index: number, value: number) => void;
    }
}

/** Linux input event codes (linux/input-event-codes.h). */
export const EV_KEY = 0x01;
export const BTN_A = 0x130;
export const BTN_Y = 0x134;

/** The name of the standard pad the server publishes. */
export const STANDARD_XPAD = 'Microsoft X-Box 360 pad';

/** One input event: type, code, value. */
export type InputEvent = readonly [number, number, number];

/** A run's emulator: the environment that puts it under the server and where it records. */
export interface Shim {
    env: Record<string, string>;
    stream: string;
    log: string;
}

/**
 * The environment that points the server at the emulator, building it first
 * when it is not built; both recording files start empty.
 * @param tag Distinguishes this run's recording files in the work directory.
 * @returns The environment and the files.
 * @throws {Error} When the emulator does not build.
 */
export function shimEnv(tag: string): Shim {
    const lib = join(TOOLS, 'uinput_shim.so');
    if (!existsSync(lib)) {
        const made = spawnSync('make', ['-s', '-C', TOOLS, 'uinput_shim.so'], { stdio: 'inherit' });
        if (made.status !== 0) throw new Error(`make uinput_shim.so exited ${made.status}`);
    }
    const stream = join(WORKDIR, `uinput-${tag}-stream.bin`);
    const log = join(WORKDIR, `uinput-${tag}-shim.log`);
    for (const path of [stream, log]) writeFileSync(path, '');
    return {
        env: { LD_PRELOAD: lib, UINPUT_SHIM_STREAM: stream, UINPUT_SHIM_LOG: log, SELKIES_UINPUT_GAMEPAD: 'true' },
        stream,
        log,
    };
}

/**
 * The events an emulator's stream holds, one per 24-byte `input_event`,
 * timestamps dropped.
 * @param path The stream file.
 * @returns Each event's type, code and value.
 */
export function decode(path: string): InputEvent[] {
    const blob = readFileSync(path);
    const events: InputEvent[] = [];
    for (let o = 0; o + 24 <= blob.length; o += 24) {
        events.push([blob.readUInt16LE(o + 16), blob.readUInt16LE(o + 18), blob.readInt32LE(o + 20)]);
    }
    return events;
}

/**
 * Whether `events` holds the event `want`.
 * @param events The events.
 * @param want The event.
 * @returns True when one matches.
 */
export function has(events: readonly InputEvent[], want: InputEvent): boolean {
    return events.some((e) => e[0] === want[0] && e[1] === want[1] && e[2] === want[2]);
}

/**
 * How many devices the emulator created under `name`. It funnels every
 * uinput descriptor of the process into one log, so bare `DEV_CREATE` lines
 * count devices other parts of the session publish too; each device names
 * itself in its `DEV_SETUP` line.
 * @param log The emulator's log.
 * @param name The device name, as given to `UI_DEV_SETUP`.
 * @returns The `DEV_SETUP` lines carrying it.
 */
export function created(log: string, name: string): number {
    return readFileSync(log, 'utf8').split('\n').filter((l) => l.includes('DEV_SETUP') && l.includes(`name=${name}`)).length;
}

/** @returns The init script installing the synthetic gamepad: `pad_init.ts` with its types stripped. */
export function padInitScript(): string {
    return stripTypes(readFileSync(join(TOOLS, 'pad_init.ts'), 'utf8'));
}
