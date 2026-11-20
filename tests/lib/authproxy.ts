/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * An HTTPS reverse proxy with HTTP Basic authentication in front of a plain
 * HTTP server, for the suites that run a session the way a deployment behind
 * one does: the page and its WebSocket arrive over TLS with the proxy's
 * credential, the server behind sees plain HTTP, and the forwarding headers
 * say how the client came. It listens on loopback, on a port of its own, with
 * a certificate and credential made for the one run and kept in memory.
 *
 * A request and an upgrade are authenticated separately by the same check and
 * a denial is answered before a connection to the upstream is made. What the
 * check consumes, the `Authorization` header, is not forwarded; the client's
 * own `Host` is, and `X-Forwarded-For`, `X-Forwarded-Host`, and
 * `X-Forwarded-Proto` are set over anything the client sent. A request is
 * relayed as an HTTP request, in each direction without the headers that
 * describe only the hop (`Connection` and every header it nominates) and
 * with its chunked body's trailers, which a plain pipe would drop. An upgrade
 * is written to the upstream as the head the client sent, with the bytes that
 * followed it. Its `Connection` and `Upgrade` stay; every other header that
 * describes only the hop, and every header its `Connection` nominates, is
 * dropped as in a request. Then both sockets are piped one into the other,
 * so what the upstream answers (101 or not) and every frame after it pass
 * untouched. Every failure closes both ends of the
 * connection it happened on and is kept in {@link AuthProxy.errors} for the
 * caller to read.
 * @module
 */

import { spawn } from 'node:child_process';
import { createHash, randomBytes, timingSafeEqual } from 'node:crypto';
import { request as httpRequest, type IncomingMessage, type ServerResponse } from 'node:http';
import { createServer, type Server } from 'node:https';
import { connect, type AddressInfo, type Socket } from 'node:net';
import type { Duplex } from 'node:stream';

/** The proxy's own certificate and the key it was made with, as PEM. */
interface Identity {
    key: string;
    cert: string;
}

const CHALLENGE = 'Basic realm="selkies", charset="UTF-8"';
const DENIED = `HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: ${CHALLENGE}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n`;
const BAD_GATEWAY = 'HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n';

/** Headers the proxy sets itself, and the credential it consumes: never taken from the client. */
const OWN_HEADERS = new Set(['authorization', 'x-forwarded-for', 'x-forwarded-host', 'x-forwarded-proto']);

/** Headers that describe one hop: not carried onto the upstream request or back from its response. */
const HOP_HEADERS = new Set(['connection', 'keep-alive', 'proxy-connection', 'te', 'expect']);

/** The header names that a `Connection` header in the flat `raw` list nominates as describing the hop too. */
function nominated(raw: readonly string[]): Set<string> {
    const names = new Set<string>();
    for (let i = 0; i + 1 < raw.length; i += 2) {
        if ((raw[i] ?? '').toLowerCase() !== 'connection') continue;
        for (const token of (raw[i + 1] ?? '').split(',')) names.add(token.trim().toLowerCase());
    }
    return names;
}

/** The flat name/value list `raw` without the headers whose lowercase names `drop` holds, spelling and order kept. */
function without(raw: readonly string[], drop: ReadonlySet<string>): string[] {
    const kept: string[] = [];
    for (let i = 0; i + 1 < raw.length; i += 2) {
        if (!drop.has((raw[i] ?? '').toLowerCase())) kept.push(raw[i] ?? '', raw[i + 1] ?? '');
    }
    return kept;
}

/** The flat trailer list `raw` as the name/value pairs `addTrailers` takes. */
function pairs(raw: readonly string[]): [string, string][] {
    const list: [string, string][] = [];
    for (let i = 0; i + 1 < raw.length; i += 2) list.push([raw[i] ?? '', raw[i + 1] ?? '']);
    return list;
}

/**
 * Make a self-signed certificate for `127.0.0.1` and its private key without
 * a file: openssl writes both to a pipe (through `cat`, since a socket pair
 * cannot be opened as `/dev/stdout`) and they are read from its output.
 * @returns The PEM of both.
 * @throws {Error} When openssl cannot be run or does not return both.
 */
async function ephemeralIdentity(): Promise<Identity> {
    const script = 'openssl req -x509 -newkey rsa:2048 -nodes -keyout /dev/stdout -out /dev/stdout '
        + '-subj /CN=127.0.0.1 -addext subjectAltName=IP:127.0.0.1 -days 1 2>/dev/null | cat';
    const child = spawn('sh', ['-c', script], { stdio: ['ignore', 'pipe', 'ignore'], signal: AbortSignal.timeout(15_000) });
    const out: Buffer[] = [];
    child.stdout.on('data', (chunk: Buffer) => { out.push(chunk); });
    const code = await new Promise<number | null>((resolve, reject) => {
        child.once('error', reject);
        child.once('close', resolve);
    });
    const blocks = [...Buffer.concat(out).toString('latin1').matchAll(/-----BEGIN ([A-Z ]+)-----[\s\S]*?-----END \1-----/g)];
    const key = blocks.find((b) => b[1]?.endsWith('PRIVATE KEY'))?.[0];
    const cert = blocks.find((b) => b[1] === 'CERTIFICATE')?.[0];
    if (key === undefined || cert === undefined) throw new Error(`openssl returned no certificate and key (exit ${String(code)})`);
    return { key, cert };
}

/** The SHA-256 of `text`, which the credential is compared by so that its length is not. */
const digest = (text: string): Buffer => createHash('sha256').update(text).digest();

/** The proxy, listening on a port of its own on loopback. */
export class AuthProxy {
    /** Where clients reach the proxy: `https://127.0.0.1:<port>`. */
    readonly url: string;
    /** The credential the proxy asks for, for a browser context's `httpCredentials`. */
    readonly httpCredentials: { username: string; password: string };
    /** The proxy's certificate, which a client trusts as its only authority; not secret. */
    readonly certificate: string;
    private readonly server: Server;
    private readonly upstream: number;
    private readonly expected: Buffer;
    private readonly sockets = new Set<Socket>();
    private readonly failures: string[] = [];
    private served = 0;
    private upgraded = 0;
    private denials = 0;
    private stopping = false;
    private stopped: Promise<void> | null = null;

    private constructor(server: Server, upstream: number, port: number, certificate: string) {
        this.server = server;
        this.upstream = upstream;
        this.url = `https://127.0.0.1:${port}`;
        this.certificate = certificate;
        this.httpCredentials = { username: 'selkies', password: randomBytes(18).toString('base64url') };
        const { username, password } = this.httpCredentials;
        this.expected = digest(`Basic ${Buffer.from(`${username}:${password}`).toString('base64')}`);
        server.on('connection', (socket) => { this.track(socket); });
        server.on('secureConnection', (socket) => { socket.on('error', (e) => { this.record('client', e); }); });
        server.on('tlsClientError', (e, socket) => {
            this.record('tls', e);
            socket.destroy();
        });
        server.on('request', (req, res) => { this.forward(req, res); });
        server.on('upgrade', (req, socket, head) => { this.relay(req, socket, head); });
    }

    /**
     * Listen on a free loopback port and relay to `upstream` on `127.0.0.1`.
     * @param upstream The port of the plain HTTP server behind the proxy.
     * @returns The listening proxy.
     * @throws {Error} When the certificate cannot be made or the port bound.
     */
    static async start(upstream: number): Promise<AuthProxy> {
        const { key, cert } = await ephemeralIdentity();
        const server = createServer({ key, cert });
        await new Promise<void>((resolve, reject) => {
            server.once('error', reject);
            server.listen(0, '127.0.0.1', () => { resolve(); });
        });
        const address: AddressInfo | string | null = server.address();
        if (address === null || typeof address !== 'object') throw new Error('the proxy has no port');
        return new AuthProxy(server, upstream, address.port, cert);
    }

    /** The authenticated requests relayed so far. */
    get requests(): number {
        return this.served;
    }

    /** The authenticated upgrades relayed so far. */
    get upgrades(): number {
        return this.upgraded;
    }

    /** The requests and upgrades denied for their credentials so far. */
    get rejected(): number {
        return this.denials;
    }

    /** What went wrong on a connection so far, one line each; empty while every flow went through. */
    get errors(): readonly string[] {
        return this.failures;
    }

    /** Stop listening and destroy every connection, both sides of an upgrade included; calling it again waits for the first. */
    stop(): Promise<void> {
        this.stopped ??= this.shut();
        return this.stopped;
    }

    private async shut(): Promise<void> {
        this.stopping = true;
        const closed = new Promise<void>((resolve) => { this.server.close(() => { resolve(); }); });
        for (const socket of this.sockets) socket.destroy();
        await closed;
    }

    private track(socket: Socket): void {
        this.sockets.add(socket);
        socket.once('close', () => { this.sockets.delete(socket); });
    }

    /** Keep a failure for the caller, unless it is the proxy's own stop that caused it. */
    private record(where: string, error: Error): void {
        if (!this.stopping) this.failures.push(`${where}: ${error.message}`);
    }

    /** Whether the request carries the credential, compared in constant time. */
    private authorized(req: IncomingMessage): boolean {
        const given = req.headers.authorization;
        return typeof given === 'string' && timingSafeEqual(digest(given), this.expected);
    }

    /**
     * The request's headers as the upstream gets them, as a flat name/value
     * list in the client's own spelling and order: without the credential,
     * any forwarding header, the hop's own headers, and those its
     * `Connection` nominates, and with the proxy's forwarding headers. An
     * upgrade (`keep`) is the one exception to the hop filtering: its
     * `Connection` and `Upgrade` are the point, so they stay, as the client
     * sent them, while every other hop header still goes.
     */
    private headersFor(req: IncomingMessage, keep: boolean): string[] {
        const hop = new Set([...HOP_HEADERS, ...nominated(req.rawHeaders)]);
        if (keep) {
            hop.delete('connection');
            hop.delete('upgrade');
        }
        const drop = new Set([...OWN_HEADERS, ...hop]);
        const headers = without(req.rawHeaders, drop);
        headers.push('X-Forwarded-For', req.socket.remoteAddress ?? '', 'X-Forwarded-Proto', 'https');
        if (req.headers.host !== undefined) headers.push('X-Forwarded-Host', req.headers.host);
        return headers;
    }

    private forward(req: IncomingMessage, res: ServerResponse): void {
        if (!this.authorized(req)) {
            this.denials++;
            req.resume();
            res.writeHead(401, { 'WWW-Authenticate': CHALLENGE, 'Content-Length': '0', Connection: 'close' });
            res.end();
            return;
        }
        this.served++;
        const up = httpRequest({
            host: '127.0.0.1', port: this.upstream, method: req.method, path: req.url, agent: false,
            headers: [...this.headersFor(req, false), 'Connection', 'close'],
        });
        up.on('socket', (socket) => { this.track(socket); });
        up.on('response', (answer) => {
            const headers = without(answer.rawHeaders, new Set([...HOP_HEADERS, ...nominated(answer.rawHeaders)]));
            res.writeHead(answer.statusCode ?? 502, answer.statusMessage, headers);
            answer.on('error', (e) => {
                this.record('upstream response', e);
                res.destroy();
            });
            answer.pipe(res, { end: false });
            answer.on('end', () => {
                if (answer.rawTrailers.length > 0) res.addTrailers(pairs(answer.rawTrailers));
                res.end();
            });
        });
        up.on('error', (e) => {
            if (res.destroyed) return;
            this.record('upstream request', e);
            if (res.headersSent) {
                res.destroy();
            } else {
                res.writeHead(502, { 'Content-Length': '0', Connection: 'close' });
                res.end();
            }
        });
        req.on('error', (e) => {
            this.record('client request', e);
            up.destroy();
        });
        res.on('close', () => { up.destroy(); });
        req.pipe(up, { end: false });
        req.on('end', () => {
            if (req.rawTrailers.length > 0) up.addTrailers(pairs(req.rawTrailers));
            up.end();
        });
    }

    private relay(req: IncomingMessage, socket: Duplex, head: Buffer): void {
        socket.on('error', (e) => {
            this.record('client upgrade', e);
            socket.destroy();
        });
        if (!this.authorized(req)) {
            this.denials++;
            socket.end(DENIED);
            return;
        }
        this.upgraded++;
        const upstream = connect(this.upstream, '127.0.0.1');
        this.track(upstream);
        let linked = false;
        upstream.once('connect', () => {
            linked = true;
            const lines = [`${req.method ?? 'GET'} ${req.url ?? '/'} HTTP/1.1`];
            const headers = this.headersFor(req, true);
            for (let i = 0; i + 1 < headers.length; i += 2) lines.push(`${headers[i] ?? ''}: ${headers[i + 1] ?? ''}`);
            upstream.write(Buffer.concat([Buffer.from(`${lines.join('\r\n')}\r\n\r\n`, 'latin1'), head]));
            socket.pipe(upstream);
            upstream.pipe(socket);
        });
        upstream.on('error', (e) => {
            this.record('upstream upgrade', e);
            upstream.destroy();
            if (linked) socket.destroy();
            else socket.end(BAD_GATEWAY, () => { socket.destroy(); });
        });
        socket.once('close', () => { upstream.destroy(); });
        upstream.once('close', () => { socket.end(); });
    }
}
