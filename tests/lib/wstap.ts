/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * A relay in front of the server under test that records every text message
 * the browser sends on a WebSocket, and every request line. The client's
 * socket lives in its socket worker, which a page-side hook never sees, so a
 * suite that has to count what the worker sends (frame acks) reads them here;
 * one that has to know what a proxy's access log would record reads the
 * request lines. Bytes pass through unchanged either way: the tap reads the
 * browser's side of each connection as it goes by.
 * @module
 */

import { connect, createServer, type AddressInfo, type Server, type Socket } from 'node:net';

/** A text message the browser sent, and when (seconds on the monotonic clock). */
export interface Tapped {
    at: number;
    text: string;
}

const now = (): number => performance.now() / 1000;

/** Reads one connection's browser side: HTTP request heads, then WebSocket frames once upgraded. */
class Reader {
    private buf = Buffer.alloc(0);
    private ws = false;
    private body = 0;
    private chunked = false;
    private fragments: Buffer[] | null = null;
    private readonly tap: WsTap;

    constructor(tap: WsTap) {
        this.tap = tap;
    }

    feed(data: Buffer): void {
        this.buf = Buffer.concat([this.buf, data]);
        for (;;) {
            if (this.ws ? !this.frame() : !this.http()) return;
        }
    }

    /** One step of HTTP: a body's bytes skipped, or a request head read. */
    private http(): boolean {
        if (this.body > 0) {
            const n = Math.min(this.body, this.buf.length);
            this.buf = this.buf.subarray(n);
            this.body -= n;
            return n > 0;
        }
        if (this.chunked) return this.chunk();
        const end = this.buf.indexOf('\r\n\r\n');
        if (end < 0) return false;
        const head = this.buf.subarray(0, end).toString('latin1').split('\r\n');
        this.buf = this.buf.subarray(end + 4);
        const [method = '', path = ''] = (head[0] ?? '').split(' ');
        const header = (name: string): string[] => head.slice(1)
            .filter((l) => l.toLowerCase().startsWith(`${name}:`)).map((l) => l.slice(name.length + 1).trim());
        const upgrade = header('upgrade').some((v) => v.toLowerCase() === 'websocket');
        const protocols = upgrade ? header('sec-websocket-protocol').flatMap((h) => h.split(',')).map((p) => p.trim()).filter(Boolean) : [];
        const shown = protocols.map((p) => (p.startsWith('selkies.token.') ? 'selkies.token.*' : p)).join(',');
        this.tap.requests.push(`${method} ${path} ${shown}`.trimEnd());
        if (upgrade) {
            this.ws = true;
        } else if (header('transfer-encoding').some((v) => v.toLowerCase().includes('chunked'))) {
            this.chunked = true;
        } else {
            this.body = Number(header('content-length')[0] ?? '0') || 0;
        }
        return true;
    }

    /** One chunk of a chunked body skipped. */
    private chunk(): boolean {
        const eol = this.buf.indexOf('\r\n');
        if (eol < 0) return false;
        const size = parseInt(this.buf.subarray(0, eol).toString('latin1'), 16) || 0;
        if (size === 0) {
            // The last chunk, then the (empty) trailer.
            const end = this.buf.indexOf('\r\n\r\n');
            if (end < 0) return false;
            this.buf = this.buf.subarray(end + 4);
            this.chunked = false;
            return true;
        }
        if (this.buf.length < eol + 2 + size + 2) return false;
        this.buf = this.buf.subarray(eol + 2 + size + 2);
        return true;
    }

    /** One WebSocket frame from the browser, unmasked; a text message is recorded whole. */
    private frame(): boolean {
        const b = this.buf;
        if (b.length < 2) return false;
        let size = b[1] & 0x7F;
        let at = 2;
        if (size === 126) {
            if (b.length < 4) return false;
            size = b.readUInt16BE(2);
            at = 4;
        } else if (size === 127) {
            if (b.length < 10) return false;
            size = Number(b.readBigUInt64BE(2));
            at = 10;
        }
        const masked = (b[1] & 0x80) !== 0;
        const key = masked ? b.subarray(at, at + 4) : null;
        if (masked) at += 4;
        if (b.length < at + size) return false;
        const payload = Buffer.from(b.subarray(at, at + size));
        if (key) for (let i = 0; i < payload.length; i++) payload[i] ^= key[i % 4];
        this.buf = b.subarray(at + size);
        const fin = (b[0] & 0x80) !== 0;
        const opcode = b[0] & 0x0F;
        if (opcode === 1) this.fragments = [payload];
        else if (opcode === 0 && this.fragments) this.fragments.push(payload);
        else return true;
        if (fin) {
            const text = Buffer.concat(this.fragments).toString('utf8');
            this.tap.messages.push({ at: now(), text: text.slice(0, 80) });
            this.fragments = null;
        }
        return true;
    }
}

/** The relay, listening on a port of its own. */
export class WsTap {
    readonly messages: Tapped[] = [];
    readonly requests: string[] = [];
    private readonly server: Server;
    private readonly upstream: number;
    private readonly sockets = new Set<Socket>();

    private constructor(server: Server, upstream: number) {
        this.server = server;
        this.upstream = upstream;
        server.on('connection', (client) => { this.relay(client); });
    }

    /**
     * Listen on `listen` (a free port for 0) and relay to `upstream`.
     * @returns The listening tap.
     */
    static async start(upstream: number, listen = 0): Promise<WsTap> {
        const server = createServer();
        const tap = new WsTap(server, upstream);
        await new Promise<void>((resolve, reject) => {
            server.once('error', reject);
            server.listen(listen, '127.0.0.1', () => { resolve(); });
        });
        return tap;
    }

    /** The tap's port. */
    get port(): number {
        const address: AddressInfo | string | null = this.server.address();
        return address !== null && typeof address === 'object' ? address.port : 0;
    }

    private relay(client: Socket): void {
        const upstream = connect(this.upstream, '127.0.0.1');
        const reader = new Reader(this);
        for (const s of [client, upstream]) {
            this.sockets.add(s);
            s.on('error', () => undefined);
            s.on('close', () => { this.sockets.delete(s); });
        }
        client.on('data', (data: Buffer) => { reader.feed(data); });
        client.pipe(upstream);
        upstream.pipe(client);
        client.on('close', () => upstream.destroy());
        upstream.on('close', () => client.destroy());
    }

    /** Stop listening and drop every connection. */
    async stop(): Promise<void> {
        for (const s of this.sockets) s.destroy();
        await new Promise<void>((resolve) => { this.server.close(() => { resolve(); }); });
    }
}
