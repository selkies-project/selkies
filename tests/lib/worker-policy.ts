/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * A reverse proxy in the suite's own process that lets a real browser meet the
 * server's Content-Security-Policy over real loopback sockets. Playwright's
 * `route.fulfill` serves synthetic responses, and native worker WebSockets
 * opened from such a page fail with `ERR_BLOCKED_BY_LOCAL_NETWORK_ACCESS_CHECKS`;
 * a proxy the browser reaches by TCP has no such page. HTTP responses pass
 * through with their status, raw header list (duplicates and order kept) and
 * undecoded body, plus one more `worker-src 'none'` policy when restricted,
 * which the browser intersects with the server's own. An Upgrade becomes an
 * opaque TCP tunnel, so the WebSocket handshake and frames are the server's
 * own. `Host`, and an `Origin` naming the proxy, are rewritten to the
 * upstream's, as the server's origin check compares them.
 * @module
 */

import { createServer, request, type IncomingMessage, type ServerResponse } from 'node:http';
import { connect, type Socket } from 'node:net';
import { pipeline } from 'node:stream';

/** What one proxied HTTP response carried, for the assertions on the policy. */
export interface ProxiedResponse {
    /** The request path, query included. */
    path: string;
    /** The upstream's status. */
    status: number;
    /** The Content-Security-Policy values the upstream sent. */
    sent: string[];
    /** The Content-Security-Policy values the browser was given. */
    delivered: string[];
}

/** A running proxy. */
export interface WorkerPolicyProxy {
    /** The proxy's origin, `http://127.0.0.1:<port>`. */
    url: string;
    /** Every HTTP response forwarded so far, in order. */
    responses: ProxiedResponse[];
    /** Destroys every socket the proxy owns and stops listening. */
    close(): Promise<void>;
}

const EXTRA_POLICY = "worker-src 'none'";

/**
 * The values of every header called `name` in a raw `[name, value, ...]` list.
 * @param raw The raw header list.
 * @param name The lower-case header name.
 * @returns The values, in order.
 */
function valuesOf(raw: readonly string[], name: string): string[] {
    const values: string[] = [];
    for (let i = 0; i + 1 < raw.length; i += 2) {
        const k = raw[i], v = raw[i + 1];
        if (k !== undefined && v !== undefined && k.toLowerCase() === name) values.push(v);
    }
    return values;
}

/**
 * Start a proxy in front of `upstreamUrl`.
 * @param upstreamUrl The server's origin, `http://host:port`.
 * @param restricted Whether to add a `worker-src 'none'` policy to every response.
 * @returns The proxy, listening on an ephemeral loopback port.
 * @throws When the listener cannot be bound.
 */
export async function workerPolicyProxy(upstreamUrl: string, restricted: boolean): Promise<WorkerPolicyProxy> {
    const upstream = new URL(upstreamUrl);
    const upstreamPort = Number(upstream.port || 80);
    const sockets = new Set<Socket>();
    const responses: ProxiedResponse[] = [];
    let proxyOrigin = '';

    const track = (socket: Socket): void => {
        sockets.add(socket);
        socket.once('close', () => sockets.delete(socket));
    };
    const report = (what: string, url: string | undefined, err: Error): void => {
        console.error(`worker-policy proxy: ${what} ${(url ?? '').split('?')[0]}: ${err.message}`);
    };

    /** The request's raw headers with Host and a matching Origin pointed at the upstream. */
    const forwardHeaders = (raw: readonly string[]): string[] => {
        const out: string[] = [];
        for (let i = 0; i + 1 < raw.length; i += 2) {
            const name = raw[i], rawValue = raw[i + 1];
            if (name === undefined || rawValue === undefined) continue;
            const lower = name.toLowerCase();
            let value = rawValue;
            if (lower === 'host') value = upstream.host;
            else if (lower === 'origin' && value === proxyOrigin) value = upstream.origin;
            out.push(name, value);
        }
        return out;
    };

    const onRequest = (req: IncomingMessage, res: ServerResponse): void => {
        const path = req.url ?? '/';
        const up = request({
            host: upstream.hostname,
            port: upstreamPort,
            method: req.method,
            path,
            headers: forwardHeaders(req.rawHeaders),
            agent: false,
        });
        up.once('socket', track);
        up.once('response', (upRes) => {
            const sent = valuesOf(upRes.rawHeaders, 'content-security-policy');
            const raw = [...upRes.rawHeaders];
            if (restricted) raw.push('content-security-policy', EXTRA_POLICY);
            const delivered = restricted ? [...sent, EXTRA_POLICY] : sent;
            responses.push({ path, status: upRes.statusCode ?? 0, sent, delivered });
            res.writeHead(upRes.statusCode ?? 502, upRes.statusMessage, raw);
            pipeline(upRes, res, (err) => {
                if (err) report('response', path, err);
            });
        });
        pipeline(req, up, (err) => {
            if (!err) return;
            report('request', path, err);
            if (res.headersSent) res.destroy(err);
            else res.writeHead(502).end();
        });
    };

    const onUpgrade = (req: IncomingMessage, client: Socket, head: Buffer): void => {
        const tunnel = connect(upstreamPort, upstream.hostname);
        track(tunnel);
        const fail = (err: Error): void => {
            report('upgrade', req.url, err);
            tunnel.destroy();
            client.destroy();
        };
        client.once('error', fail);
        tunnel.once('error', fail);
        tunnel.once('connect', () => {
            const lines = [`${req.method ?? 'GET'} ${req.url ?? '/'} HTTP/1.1`];
            const raw = forwardHeaders(req.rawHeaders);
            for (let i = 0; i < raw.length; i += 2) lines.push(`${raw[i]}: ${raw[i + 1]}`);
            tunnel.write(`${lines.join('\r\n')}\r\n\r\n`);
            if (head.length) tunnel.write(head);
            client.pipe(tunnel);
            tunnel.pipe(client);
        });
        tunnel.once('close', () => client.destroy());
        client.once('close', () => tunnel.destroy());
    };

    const server = createServer(onRequest);
    server.on('connection', track);
    server.on('upgrade', onUpgrade);
    server.on('clientError', (err, socket) => {
        report('client', undefined, err);
        socket.destroy(err);
    });
    await new Promise<void>((resolve, reject) => {
        server.once('error', reject);
        server.listen(0, '127.0.0.1', () => {
            server.off('error', reject);
            resolve();
        });
    });
    const address = server.address();
    if (address === null || typeof address === 'string') throw new Error('worker-policy proxy: no listening port');
    proxyOrigin = `http://127.0.0.1:${address.port}`;

    return {
        url: proxyOrigin,
        responses,
        close: () =>
            new Promise<void>((resolve, reject) => {
                server.close((err) => (err ? reject(err) : resolve()));
                for (const socket of sockets) socket.destroy();
            }),
    };
}
