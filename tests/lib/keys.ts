/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Typed keys read back as an application reads them. On X11 a focused libX11
 * window decodes the key events it receives through XLookupString
 * (`tools/x11_keyobs.c`), so the X server's own translation is what is
 * checked; on Wayland the observer's key events are resolved with
 * libxkbcommon against the keymap the seat delivered ahead of them
 * (`tools/xkb_lookup.c`). Both tools are built into the work directory
 * against the libraries `pkg-config` finds.
 * @module
 */

import { spawn, spawnSync, type ChildProcessByStdio } from 'node:child_process';
import type { Readable, Writable } from 'node:stream';
import { existsSync, statSync, unlinkSync } from 'node:fs';
import { join } from 'node:path';
import { createInterface } from 'node:readline';

import { fields, numberOr, stringOr, type Fields } from '../../web/selkies-web-core/lib/narrow.ts';
import { sleep, TOOLS, WORKDIR } from './env.ts';
import type { Observer } from './host.ts';

/**
 * A tool of `tests/tools`, built into the work directory unless a build
 * newer than its source is there.
 * @param name The tool (`x11_keyobs`).
 * @param packages What `pkg-config` names its libraries; none for a tool
 *     that loads what it needs at run time.
 * @param extra Further compiler arguments (`-ldl`).
 * @returns Its path.
 * @throws {Error} When pkg-config or the compiler fails.
 */
export function builtTool(name: string, packages: readonly string[], extra: readonly string[] = []): string {
    const source = join(TOOLS, `${name}.c`);
    const out = join(WORKDIR, name);
    if (existsSync(out) && statSync(out).mtimeMs >= statSync(source).mtimeMs) return out;
    const ask = (args: string[]): string => {
        const run = spawnSync('pkg-config', args, { encoding: 'utf8' });
        if (run.status !== 0) throw new Error(`pkg-config ${args.join(' ')}: ${(run.stderr ?? '').trim() || String(run.error)}`);
        return run.stdout.trim();
    };
    const flags = packages.length ? ask(['--cflags', '--libs', ...packages]).split(/\s+/).filter(Boolean) : [];
    const rpath = packages.length ? [`-Wl,-rpath,${ask(['--variable=libdir', packages[0]])}`] : [];
    const cc = spawnSync('cc', ['-O2', '-o', out, source, ...flags, ...rpath, ...extra], { encoding: 'utf8', timeout: 180_000 });
    if (cc.status !== 0) throw new Error(`cc ${name}: ${(cc.stderr ?? '').slice(0, 300)}`);
    return out;
}

/** One key event the X window received: pressed, keycode, XKB group, keysym. */
export type XKey = [boolean, number, number, number];

/** A focused libX11 window on an X display, decoding what it receives. */
export class X11Keys {
    private readonly child: ChildProcessByStdio<Writable, Readable, null>;
    private readonly events: XKey[] = [];
    private readonly marks: (() => void)[] = [];
    private ready = false;

    /** @param display The X display. */
    constructor(display: string) {
        this.child = spawn(builtTool('x11_keyobs', ['x11']), [display], { stdio: ['pipe', 'pipe', 'inherit'] });
        createInterface({ input: this.child.stdout }).on('line', (line) => {
            let got: Fields;
            try {
                got = fields(JSON.parse(line));
            } catch {
                return;
            }
            if (got.kind === 'ready') this.ready = true;
            else if (got.kind === 'synced') this.marks.shift()?.();
            else this.events.push([got.press === true, numberOr(got.keycode) ?? 0, numberOr(got.group) ?? 0, numberOr(got.keysym) ?? 0]);
        });
    }

    /** Wait for the window to have the focus. */
    async focused(timeoutS = 10): Promise<boolean> {
        for (const until = Date.now() + timeoutS * 1000; !this.ready && Date.now() < until;) await sleep(20);
        return this.ready;
    }

    /**
     * The key events received since the last drain, after `settleS` and once
     * the server has delivered every event it holds.
     * @param settleS How long to let events arrive first.
     * @returns The events.
     */
    async drain(settleS = 0.25): Promise<XKey[]> {
        await sleep(settleS * 1000);
        await new Promise<void>((done) => {
            this.marks.push(done);
            this.child.stdin.write('sync\n');
        });
        return this.events.splice(0);
    }

    close(): void {
        this.child.stdin.end();
    }
}

/** libxkbcommon: a keymap and its state, and a keysym's character. */
export class Xkb {
    private readonly child: ChildProcessByStdio<Writable, Readable, null>;
    private readonly answers: ((line: string) => void)[] = [];

    constructor() {
        this.child = spawn(builtTool('xkb_lookup', ['xkbcommon']), [], { stdio: ['pipe', 'pipe', 'inherit'] });
        createInterface({ input: this.child.stdout }).on('line', (line) => { this.answers.shift()?.(line); });
    }

    private ask(request: string): Promise<number> {
        return new Promise((done) => {
            this.answers.push((line) => { done(Number(line) || 0); });
            this.child.stdin.write(`${request}\n`);
        });
    }

    /** Load the keymap at `path` as the state to resolve keys against. */
    async load(path: string): Promise<boolean> {
        return (await this.ask(`keymap ${path}`)) === 1;
    }

    /** Take the seat's modifier state. */
    async modifiers(depressed: number, latched: number, locked: number, group: number): Promise<void> {
        await this.ask(`mods ${depressed} ${latched} ${locked} ${group}`);
    }

    /** The keysym an evdev key resolves to now; 0 without a keymap. */
    keysym(evdev: number): Promise<number> {
        return this.ask(`sym ${evdev}`);
    }

    /** The character a keysym types; empty for none. */
    async char(keysym: number): Promise<string> {
        const cp = await this.ask(`char ${keysym}`);
        return cp ? String.fromCodePoint(cp) : '';
    }

    close(): void {
        this.child.stdin.end();
    }
}

/** One key event resolved: pressed, the evdev key, the keysym. */
export type WlKey = [boolean, number, number];

/**
 * The observer's key events resolved as an application resolves them: every
 * keymap and modifier event is replayed in order into the xkb state, so a key
 * is read against the keymap the seat had delivered ahead of it.
 */
export class WlKeys {
    private cursor = 0;
    readonly obs: Observer;
    readonly xkb: Xkb;

    constructor(obs: Observer, xkb: Xkb) {
        this.obs = obs;
        this.xkb = xkb;
    }

    /** @returns How many event lines the observer has printed. */
    async count(): Promise<number> {
        return (await this.obs.lines(0)).length;
    }

    /**
     * The key events from line `start` on, every line up to them replayed.
     * @param start The first line whose keys are wanted.
     * @returns Each key event's pressed, evdev key and keysym.
     */
    async since(start: number): Promise<WlKey[]> {
        const out: WlKey[] = [];
        const lines = await this.obs.lines(this.cursor);
        for (const line of lines) {
            const index = this.cursor++;
            if (line.kind === 'keymap') {
                const path = stringOr(line.path) ?? '';
                await this.xkb.load(path);
                try { unlinkSync(path); } catch { /* the observer took it already */ }
            } else if (line.kind === 'kbd_mods') {
                await this.xkb.modifiers(numberOr(line.dep) ?? 0, numberOr(line.lat) ?? 0, numberOr(line.lock) ?? 0, numberOr(line.group) ?? 0);
            } else if (line.kind === 'kbd_key' && index >= start) {
                const key = numberOr(line.key) ?? 0;
                out.push([line.state === 1, key, await this.xkb.keysym(key)]);
            }
        }
        return out;
    }
}
