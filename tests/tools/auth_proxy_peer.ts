/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */


/**
 * The audit's wire peers (`tests/tools/auth_proxy_audit.ts`): the plain HTTP
 * upstream behind the proxy, which records every request and upgrade that
 * reaches it, and the clients in front of it, a verifying HTTPS requester
 * and a raw TLS socket for upgrades. Both are real sockets, so what the
 * audit holds is what crossed the wire.
 * @module
 */

import { createServer, type IncomingHttpHeaders, type Server } from 'node:http';
import { request as httpsRequest } from 'node:https';
import { createServer as createNetServer, type AddressInfo, type Socket } from 'node:net';
import { checkServerIdentity, connect, type TLSSocket } from 'node:tls';
import type { AuthProxy } from '../lib/authproxy.ts';

export const WAIT = 5000;
export const SETTLE = 200;
export const PUBLIC_HOST = 'selkies.example:8443';
export const EARLY = Buffer.from('early-bytes-after-the-head');
export const GREETING = Buffer.from('hello-from-upstream');

/** Waits `ms` milliseconds. */
export const pause = (ms: number): Promise<void> => new Promise((resolve) => { setTimeout(resolve, ms); });

/**
 * Wait for `ready` to hold, polling, for at most `ms`.
 * @returns Whether it held in time.
 */
export async function until(ready: () => boolean, ms = WAIT): Promise<boolean> {
    const end = performance.now() + ms;
    while (!ready()) {
        if (performance.now() >= end) return false;
        await pause(5);
    }
    return true;
}

/** What the upstream saw of one plain request. */
interface Seen {
    method: string;
    url: string;
    headers: IncomingHttpHeaders;
    trailers: IncomingHttpHeaders;
    body: Buffer;
}

/** What the upstream saw of one upgrade. */
interface Upgraded {
    headers: IncomingHttpHeaders;
    received: Buffer[];
    closed: boolean;
}

/** The plain HTTP server behind the proxy, recording everything that reaches it. */
export interface Upstream {
    server: Server;
    port: number;
    seen: Seen[];
    upgrades: Upgraded[];
    sockets: Set<Socket>;
    errors: string[];
}

/**
 * Start the upstream: a plain request is answered 201 with its own body
 * (404 for `/missing`), an upgrade is answered 101 with a greeting and
 * then echoes every byte it is sent, the bytes that came with the head first.
 * @returns The listening upstream.
 */
export async function startUpstream(): Promise<Upstream> {
    const server = createServer();
    const up: Upstream = { server, port: 0, seen: [], upgrades: [], sockets: new Set(), errors: [] };
    server.on('connection', (socket) => {
        up.sockets.add(socket);
        socket.on('close', () => { up.sockets.delete(socket); });
    });
    server.on('request', (req, res) => {
        const parts: Buffer[] = [];
        req.on('data', (chunk: Buffer) => { parts.push(chunk); });
        req.on('end', () => {
            const body = Buffer.concat(parts);
            up.seen.push({ method: req.method ?? '', url: req.url ?? '', headers: req.headers, trailers: req.trailers, body });
            if (req.url === '/missing') {
                res.writeHead(404, { 'Content-Type': 'text/plain' });
                res.end('nope');
                return;
            }
            if (req.url === '/hops') {
                res.writeHead(200, { Connection: 'close, X-Hop-Response', 'X-Hop-Response': 'leak', 'X-Resp-Kept': 'yes' });
                res.end('hop');
                return;
            }
            if (req.url === '/trailers') {
                res.writeHead(200, { 'Transfer-Encoding': 'chunked', Trailer: 'X-Sum' });
                res.write(body);
                res.addTrailers({ 'X-Sum': 'from-upstream' });
                res.end();
                return;
            }
            res.writeHead(201, { 'X-Upstream': 'yes', 'Set-Cookie': ['a=1; Path=/', 'b=2; Path=/'], 'Content-Type': 'application/octet-stream' });
            res.end(body);
        });
    });
    server.on('upgrade', (req, socket, head) => {
        const record: Upgraded = { headers: req.headers, received: [head], closed: false };
        up.upgrades.push(record);
        socket.on('close', () => { record.closed = true; });
        socket.on('data', (chunk: Buffer) => {
            record.received.push(chunk);
            socket.write(chunk);
        });
        socket.on('end', () => { socket.end(); });
        socket.on('error', (e) => { up.errors.push(e.message); });
        const protocol = req.headers['sec-websocket-protocol'];
        socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: audit\r\n`
            + `${typeof protocol === 'string' ? `Sec-WebSocket-Protocol: ${protocol}\r\n` : ''}\r\n`);
        socket.write(GREETING);
        if (head.length) socket.write(head);
    });
    await new Promise<void>((resolve, reject) => {
        server.once('error', reject);
        server.listen(0, '127.0.0.1', () => { resolve(); });
    });
    const address: AddressInfo | string | null = server.address();
    up.port = address !== null && typeof address === 'object' ? address.port : 0;
    return up;
}

/** A free port that nothing listens on. */
export async function deadPort(): Promise<number> {
    const probe = createNetServer();
    await new Promise<void>((resolve, reject) => {
        probe.once('error', reject);
        probe.listen(0, '127.0.0.1', () => { resolve(); });
    });
    const address: AddressInfo | string | null = probe.address();
    const port = address !== null && typeof address === 'object' ? address.port : 0;
    await new Promise<void>((resolve) => { probe.close(() => { resolve(); }); });
    return port;
}

/** The port in a proxy's URL. */
export const portOf = (proxy: AuthProxy): number => Number(new URL(proxy.url).port);

/** The `Authorization` header value for these credentials. */
export const basic = (username: string, password: string): string => `Basic ${Buffer.from(`${username}:${password}`).toString('base64')}`;

/** What a client of the proxy presents, none included. */
interface Credential {
    name: string;
    header: string | null;
}

/** The credentials the proxy must deny, from none to the right password under another scheme. */
export function denied(proxy: AuthProxy): Credential[] {
    const { username, password } = proxy.httpCredentials;
    return [
        { name: 'no credentials', header: null },
        { name: 'a wrong password', header: basic(username, `${password}x`) },
        { name: 'a wrong user', header: basic(`${username}x`, password) },
        { name: 'the right password under another scheme', header: `Bearer ${password}` },
    ];
}

/** A response to a request through the proxy. */
interface Answer {
    status: number;
    headers: IncomingHttpHeaders;
    trailers: IncomingHttpHeaders;
    body: Buffer;
}

/** A request for `ask`; with `trailers` the body goes out chunked, in two chunks, and they follow it. */
export interface Query {
    method: string;
    path: string;
    headers: Record<string, string>;
    body?: Buffer;
    trailers?: Record<string, string>;
}

/**
 * One HTTPS request to the proxy, its certificate verified.
 * @returns The response.
 */
export function ask(proxy: AuthProxy, query: Query): Promise<Answer> {
    const { method, path, headers, body, trailers } = query;
    return new Promise((resolve, reject) => {
        const req = httpsRequest(`${proxy.url}${path}`, {
            method, headers, ca: proxy.certificate, agent: false, signal: AbortSignal.timeout(WAIT),
            // A request whose Host is the public name is still made to the proxy's own address.
            checkServerIdentity: (_name, cert) => checkServerIdentity('127.0.0.1', cert),
        }, (res) => {
            const parts: Buffer[] = [];
            res.on('data', (chunk: Buffer) => { parts.push(chunk); });
            res.on('error', reject);
            res.on('end', () => { resolve({ status: res.statusCode ?? 0, headers: res.headers, trailers: res.trailers, body: Buffer.concat(parts) }); });
        });
        req.on('error', reject);
        if (trailers === undefined || body === undefined) {
            req.end(body);
            return;
        }
        const half = body.length >> 1;
        req.write(body.subarray(0, half));
        req.write(body.subarray(half));
        req.addTrailers(trailers);
        req.end();
    });
}

/** A raw TLS connection to the proxy, everything it sends kept. */
export class Raw {
    readonly socket: TLSSocket;
    readonly errors: string[] = [];
    private readonly chunks: Buffer[] = [];
    closed = false;

    private constructor(socket: TLSSocket) {
        this.socket = socket;
        socket.on('data', (chunk: Buffer) => { this.chunks.push(chunk); });
        socket.on('error', (e) => { this.errors.push(e.message); });
        socket.on('close', () => { this.closed = true; });
    }

    /**
     * Connect to the proxy, its certificate verified.
     * @returns The connection, once the handshake is done.
     */
    static async open(proxy: AuthProxy): Promise<Raw> {
        const socket = connect({ host: '127.0.0.1', port: portOf(proxy), ca: proxy.certificate });
        const raw = new Raw(socket);
        await new Promise<void>((resolve, reject) => {
            const timer = setTimeout(() => { reject(new Error('TLS handshake timed out')); }, WAIT);
            socket.once('secureConnect', () => { clearTimeout(timer); resolve(); });
            socket.once('error', (e) => { clearTimeout(timer); reject(e); });
        });
        return raw;
    }

    /** Everything received so far. */
    get data(): Buffer {
        return Buffer.concat(this.chunks);
    }

    /** The response head received, once whole, as text. */
    get head(): string | null {
        const data = this.data;
        const end = data.indexOf('\r\n\r\n');
        return end < 0 ? null : data.subarray(0, end).toString('latin1');
    }

    /** The bytes after the response head. */
    get rest(): Buffer {
        const data = this.data;
        const end = data.indexOf('\r\n\r\n');
        return end < 0 ? Buffer.alloc(0) : data.subarray(end + 4);
    }

    /** Close from this side and wait for the close. */
    async shut(): Promise<void> {
        this.socket.destroy();
        await until(() => this.closed);
    }
}

/** The status code of a response head, 0 for none. */
export function statusOf(head: string | null): number {
    return Number(/^HTTP\/1\.1 (\d{3})/.exec(head ?? '')?.[1] ?? '0');
}

/**
 * The request head of a WebSocket upgrade.
 * @param auth The `Authorization` value to send, none for null.
 * @returns The head text, ending the headers.
 */
function upgradeHead(auth: string | null): string {
    return [
        'GET /ws/stream?room=1 HTTP/1.1',
        `Host: ${PUBLIC_HOST}`,
        'Upgrade: websocket',
        'Connection: Upgrade',
        'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==',
        'Sec-WebSocket-Version: 13',
        'Sec-WebSocket-Protocol: selkies.token.audit, binary',
        'Origin: https://selkies.example',
        ...(auth === null ? [] : [`Authorization: ${auth}`]),
        '', '',
    ].join('\r\n');
}

/**
 * Upgrade through the proxy with `auth`, the head and `EARLY` in one write.
 * @returns The connection after the proxy has answered or closed.
 */
export async function upgrade(proxy: AuthProxy, auth: string | null): Promise<Raw> {
    const raw = await Raw.open(proxy);
    raw.socket.write(Buffer.concat([Buffer.from(upgradeHead(auth), 'latin1'), EARLY]));
    await until(() => raw.head !== null || raw.closed);
    return raw;
}

/** Whether the upstream's records mention `secret` anywhere in what it saw. */
export function leaks(up: Upstream, secret: string): boolean {
    const seen = JSON.stringify([up.seen.map((s) => [s.url, s.headers]), up.upgrades.map((u) => u.headers)]);
    return seen.includes(secret) || seen.includes(Buffer.from(secret).toString('base64'));
}
