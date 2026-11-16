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
import { join } from 'node:path';
import { createInterface } from 'node:readline';

import { fields, numberOr, stringOr } from '../../web/selkies-web-core/lib/narrow.ts';
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

/**
 * The `suites-host` program: `SELKIES_SUITES_HOST`, else the debug build in
 * the target directory cargo uses, built first where cargo is at hand.
 * @returns Its path.
 * @throws {Error} When it is neither named nor built.
 */
function program(): string {
    const named = process.env.SELKIES_SUITES_HOST;
    if (named) return named;
    const target = process.env.CARGO_TARGET_DIR || join(REPO, 'target');
    const built = join(target, 'debug', 'suites-host');
    const cargo = spawnSync('cargo', ['build', '--quiet', '--locked', '-p', 'selkies-suites', '--bin', 'suites-host'],
        { cwd: REPO, stdio: ['ignore', 'inherit', 'inherit'] });
    if (cargo.error === undefined && cargo.status !== 0) throw new Error(`cargo build of suites-host exited ${cargo.status}`);
    if (!existsSync(built)) throw new Error(`${built} is not built and cargo is not on PATH to build it`);
    return built;
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
