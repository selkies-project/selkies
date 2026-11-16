/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The host side of a browser suite: `suites-host`, the program of
 * `crates/selkies-suites` that starts the server under test in the sandbox
 * the Rust suites use and reads what a suite needs off the host. It speaks
 * JSON lines on its standard streams and ends with this process.
 * @module
 */

import { spawn, spawnSync, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { createInterface } from 'node:readline';

import { fields, isRecord, numberOr, stringOr, type Fields } from '../../web/selkies-web-core/lib/narrow.ts';
import { REPO } from './env.ts';

/** A server to start, as `server.start` takes it. */
export interface ServerOptions {
    /** Names its log: `<suite>-<label>.log`. */
    label: string;
    /** `websockets` (the default) or `webrtc`. */
    mode?: string;
    /** Capture from the server's own Wayland compositor. */
    wayland?: boolean;
    /** Capture this X display in place of the test display. */
    display?: string;
    /** Serve on this Unix socket alone, with no TCP port. */
    socket?: string;
    /** Serve on this port, a page's origin kept across servers, in place of a free one. */
    port?: number;
    /** Set over what the harness sets. */
    env?: Record<string, string>;
}

/** A started server. */
export interface Started {
    /** Its port; 0 on a Unix socket. */
    port: number;
    /** Its process id, which leads its process group. */
    pid: number;
    /** Where its log is kept. */
    log: string;
    /** `http://127.0.0.1:<port>`. */
    url: string;
}

let found: string | undefined;

/**
 * The `suites-host` program: `SELKIES_SUITES_HOST`, else the debug build in
 * the target directory cargo uses, built first, with the crate's other
 * programs, where cargo is at hand.
 * @returns Its path.
 * @throws {Error} When it is neither named nor built.
 */
function program(): string {
    if (found !== undefined) return found;
    const named = process.env.SELKIES_SUITES_HOST;
    if (named) return (found = named);
    const target = process.env.CARGO_TARGET_DIR || join(REPO, 'target');
    const built = join(target, 'debug', 'suites-host');
    const cargo = spawnSync('cargo', ['build', '--quiet', '--locked', '-p', 'selkies-suites', '--bins'],
        { cwd: REPO, stdio: ['ignore', 'inherit', 'inherit'] });
    if (cargo.error === undefined && cargo.status !== 0) throw new Error(`cargo build of the suites' programs exited ${cargo.status}`);
    if (!existsSync(built)) throw new Error(`${built} is not built and cargo is not on PATH to build it`);
    return (found = built);
}

/**
 * Another program of `crates/selkies-suites` (its src/bin), built beside `suites-host`.
 * @param name The program (`motion-scene`).
 * @returns Its path.
 */
export function suitesBin(name: string): string {
    return join(dirname(program()), name);
}

/** Something the host program keeps until it is released. */
export interface Held {
    handle: number;
    release: () => Promise<void>;
}

/** A `wl-observer` on a compositor: its event lines as they arrive. */
export interface Observer extends Held {
    /**
     * The first event of `kind` whose fields named in `match` hold those values.
     * @param kind The event's kind (`kbd_key`, `ptr_motion`, ...).
     * @param timeoutS How long to wait.
     * @param match Fields it must carry.
     * @returns The event, or null.
     */
    wait: (kind: string, timeoutS?: number, match?: Record<string, unknown>) => Promise<Fields | null>;
    /** @returns What it printed that was no event line (an error). */
    output: () => Promise<string[]>;
    /**
     * Its event lines.
     * @param from The first one wanted.
     * @returns The lines from it on.
     */
    lines: (from?: number) => Promise<Fields[]>;
    /**
     * Whether its surface mapped.
     * @param timeoutS How long to wait.
     * @returns True once it did.
     */
    ready: (timeoutS?: number) => Promise<boolean>;
}

/**
 * Two numbers, as the program answers a position or a size.
 * @param value The answer's field.
 * @returns The pair, or null.
 */
function pair(value: unknown): [number, number] | null {
    if (!Array.isArray(value) || value.length !== 2) return null;
    const a: unknown = value[0];
    const b: unknown = value[1];
    return typeof a === 'number' && typeof b === 'number' ? [a, b] : null;
}

/** A pending request's settlement. */
interface Waiter {
    resolve: (value: unknown) => void;
    reject: (error: Error) => void;
}

/** A running `suites-host`, for one suite. */
export class Host {
    private readonly child: ChildProcessWithoutNullStreams;
    private readonly waiting = new Map<number, Waiter>();
    private next = 1;
    private gone: Error | null = null;

    /** @param suite The suite its servers' logs are named after. */
    constructor(suite: string) {
        this.child = spawn(program(), [], {
            env: { ...process.env, SELKIES_SUITE: suite },
            stdio: ['pipe', 'pipe', 'pipe'],
        });
        this.child.stderr.on('data', (chunk: Buffer) => { process.stderr.write(chunk); });
        createInterface({ input: this.child.stdout }).on('line', (line) => { this.settle(line); });
        this.child.on('exit', (code, signal) => {
            this.gone = new Error(`suites-host exited (${signal ?? code ?? 'unknown'})`);
            for (const waiter of this.waiting.values()) waiter.reject(this.gone);
            this.waiting.clear();
        });
    }

    /**
     * Settle the request one answer line names.
     * @param line The answer.
     */
    private settle(line: string): void {
        let answer: unknown;
        try {
            answer = JSON.parse(line);
        } catch {
            process.stderr.write(`suites-host: ${line}\n`);
            return;
        }
        const said = fields(answer);
        const id = numberOr(said.id);
        const waiter = id === undefined ? undefined : this.waiting.get(id);
        if (id === undefined || waiter === undefined) return;
        this.waiting.delete(id);
        const error = stringOr(said.error);
        if (error !== undefined) waiter.reject(new Error(error));
        else waiter.resolve(said.ok);
    }

    /**
     * Ask the program one thing.
     * @param op The operation.
     * @param args What it takes.
     * @returns Its answer.
     */
    request(op: string, args: Record<string, unknown> = {}): Promise<unknown> {
        if (this.gone) return Promise.reject(this.gone);
        const id = this.next++;
        return new Promise((resolve, reject) => {
            this.waiting.set(id, { resolve, reject });
            // The request's own id is the program's to answer by; what a request
            // names (an observer) goes by `handle`.
            this.child.stdin.write(`${JSON.stringify({ ...args, id, op })}\n`);
        });
    }

    /**
     * Start a server, stopping the one before it, and wait until it answers.
     * @param options The server.
     * @returns Where it is.
     */
    async start(options: ServerOptions): Promise<Started> {
        const ok = fields(await this.request('server.start', { ...options }));
        const port = numberOr(ok.port) ?? 0;
        return { port, pid: numberOr(ok.pid) ?? 0, log: stringOr(ok.log) ?? '', url: `http://127.0.0.1:${port}` };
    }

    /**
     * Stop the running server.
     * @returns The end of its log.
     */
    async stop(): Promise<string> {
        return stringOr(fields(await this.request('server.stop')).tail) ?? '';
    }

    /**
     * The log's length now: a mark for `saw`.
     * @returns The mark.
     */
    async mark(): Promise<number> {
        return numberOr(fields(await this.request('server.mark')).mark) ?? 0;
    }

    /**
     * The running server's log from a mark on.
     * @param from A mark; 0 reads the whole log.
     * @returns The text.
     */
    async log(from = 0): Promise<string> {
        return stringOr(fields(await this.request('server.log', { from })).text) ?? '';
    }

    /**
     * Whether `needle` reaches the running server's log past `from` within
     * `timeoutS`.
     * @param needle The text.
     * @param timeoutS How long to wait.
     * @param from A mark; 0 reads the whole log.
     * @returns Whether it did.
     */
    async saw(needle: string, timeoutS = 12, from = 0): Promise<boolean> {
        const ok = fields(await this.request('server.saw', { needle, from, timeout_ms: Math.round(timeoutS * 1000) }));
        return ok.saw === true;
    }

    /**
     * Whether the key typing `keysym` is held on the X display.
     * @param keysym The keysym (a Latin-1 character's is its code).
     * @returns Whether it is down; null where no keycode carries it.
     */
    async keyHeld(keysym: number): Promise<boolean | null> {
        const held = fields(await this.request('x11.key_held', { keysym })).held;
        return typeof held === 'boolean' ? held : null;
    }

    /** @returns Where the X pointer is on the root, or null. */
    async pointer(): Promise<[number, number] | null> {
        return pair(fields(await this.request('x11.pointer')).at);
    }

    /** @returns The X display's window manager, its pid 0 when unknown; null for none. */
    async windowManager(): Promise<{ pid: number; name: string } | null> {
        const manager = fields(await this.request('x11.window_manager')).manager;
        if (!isRecord(manager)) return null;
        return { pid: numberOr(manager.pid) ?? 0, name: stringOr(manager.name) ?? '' };
    }

    /** @returns The X root's size, read on a fresh connection, or null. */
    async rootSize(): Promise<[number, number] | null> {
        return pair(fields(await this.request('x11.root_size')).size);
    }

    /**
     * CLIPBOARD's text on the X display.
     * @param timeoutS How long its owner is given to answer.
     * @returns The text, or null when none came.
     */
    async x11Clipboard(timeoutS = 8): Promise<string | null> {
        return stringOr(fields(await this.request('x11.clipboard', { timeout_ms: Math.round(timeoutS * 1000) })).text) ?? null;
    }

    /**
     * Own CLIPBOARD on the X display, serving `payload` until released.
     * @param payload The text handed to every requestor.
     * @returns The owner.
     */
    async ownClipboard(payload: string): Promise<Held> {
        return this.held(await this.request('x11.own_clipboard', { payload }));
    }

    /**
     * Map solid override-redirect windows on the X display until released,
     * and one flipping between two colors on a period where `flicker` names it.
     * @param paints Each window's `[x, y, w, h, 0xRRGGBB]`.
     * @param flicker The flipping window, its second color and its period.
     * @returns The windows.
     */
    async paint(paints: readonly (readonly number[])[], flicker?: { paint: readonly number[]; second: number; periodMs: number }): Promise<Held> {
        const args: Record<string, unknown> = { paints };
        if (flicker) args.flicker = { paint: flicker.paint, second: flicker.second, period_ms: flicker.periodMs };
        return this.held(await this.request('x11.paint', args));
    }

    /**
     * A private Xvfb of the suite's own, until released.
     * @param width Its screen's width, which Xvfb fixes as its largest.
     * @param height Its screen's height.
     * @param extra Further Xvfb arguments.
     * @returns Its display and its handle.
     */
    async xvfb(width = 1280, height = 720, extra: readonly string[] = []): Promise<Held & { display: string }> {
        const answer = await this.request('xvfb.start', { width, height, extra });
        return { ...this.held(answer), display: stringOr(fields(answer).display) ?? '' };
    }

    /**
     * Run `wl-observer`, or another Wayland client of the crate, on the
     * server's compositor at `socket`.
     * @param socket The compositor's socket name.
     * @param env Added to the client's environment.
     * @param program The client (`wl-motion-scene`).
     * @returns The observer.
     */
    async observe(socket: string, env: Record<string, string> = {}, program = 'wl-observer'): Promise<Observer> {
        const held = this.held(await this.request('wl.observe', { socket, program, env: { WLOBS_DURATION: '60', ...env } }));
        return {
            ...held,
            wait: async (kind, timeoutS = 8, match = {}) => {
                const event = fields(await this.request('wl.wait', { handle: held.handle, kind, match, timeout_ms: Math.round(timeoutS * 1000) })).event;
                return isRecord(event) ? event : null;
            },
            lines: async (from = 0) => {
                const lines = fields(await this.request('wl.lines', { handle: held.handle, from })).lines;
                return Array.isArray(lines) ? lines.map(fields) : [];
            },
            output: async () => {
                const lines = fields(await this.request('wl.output', { handle: held.handle })).lines;
                return Array.isArray(lines) ? lines.filter((l): l is string => typeof l === 'string') : [];
            },
            ready: async (timeoutS = 10) => {
                const event = fields(await this.request('wl.wait', { handle: held.handle, kind: 'mapped', timeout_ms: Math.round(timeoutS * 1000) })).event;
                return isRecord(event);
            },
        };
    }

    /**
     * Something held until released, from the id an answer names.
     * @param answer The answer.
     * @returns Its handle.
     */
    private held(answer: unknown): Held {
        const handle = numberOr(fields(answer).handle) ?? 0;
        return { handle, release: async () => { await this.request('release', { handle }); } };
    }

    /**
     * The TCP addresses a process itself listens on.
     * @param pid The process.
     * @returns `tcp <hex address:port>` per listening socket.
     */
    async listeners(pid: number): Promise<string[]> {
        const found = fields(await this.request('listeners', { pid })).listeners;
        return Array.isArray(found) ? found.filter((l): l is string => typeof l === 'string') : [];
    }

    /** End the program, which stops its server. */
    async close(): Promise<void> {
        if (this.gone) return;
        const ended = new Promise<void>((done) => { this.child.once('exit', () => { done(); }); });
        this.child.stdin.end();
        await ended;
    }
}
