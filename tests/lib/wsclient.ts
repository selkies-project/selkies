/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * A WebSocket client on the wire, for the suites that hold the server to what
 * a handshake answers: the HTTP status of a refused upgrade, the subprotocol
 * selected, and the close code, over TCP or a Unix socket.
 * @module
 */

import { randomBytes } from 'node:crypto';
import { connect, type Socket } from 'node:net';

/** Where the socket connects: a loopback port or a Unix socket's path. */
export type Target = { port: number } | { path: string };

/** A message the server sent. */
export type Received = string | Buffer;

/** A masked client frame of `opcode` carrying `payload`. */
function frame(opcode: number, payload: Buffer): Buffer {
    const mask = randomBytes(4);
    const n = payload.length;
    let head: Buffer;
    if (n < 126) {
        head = Buffer.from([0x80 | opcode, 0x80 | n]);
    } else if (n < 65536) {
        head = Buffer.from([0x80 | opcode, 0x80 | 126, n >> 8, n & 0xff]);
    } else {
        head = Buffer.alloc(10);
        head[0] = 0x80 | opcode;
        head[1] = 0x80 | 127;
        head.writeBigUInt64BE(BigInt(n), 2);
    }
    const body = Buffer.from(payload);
    for (let i = 0; i < body.length; i++) body[i] ^= mask[i % 4];
    return Buffer.concat([head, mask, body]);
}

/** A connected data socket, or the server's refusal of the upgrade. */
export class RawWs {
    /** The HTTP status that refused the upgrade, null once upgraded. */
    refused: number | null = null;
    /** The subprotocol the server selected. */
    protocol: string | null = null;
    /** The close code the server sent, once it closed. */
    closeCode: number | null = null;
    closed = false;
    private readonly socket: Socket;
    private buf = Buffer.alloc(0);
    private readonly inbox: Received[] = [];
    private waiter: (() => void) | null = null;

    private constructor(socket: Socket) {
        this.socket = socket;
    }

    /**
     * Connect and complete the handshake, or learn why it was refused.
     * @param target The server.
     * @param path The request path, with any query.
     * @param protocols Subprotocols to offer.
     * @param timeoutS How long the handshake may take.
     * @returns The socket; `refused` is set where the upgrade was refused.
     */
    static open(target: Target, path: string, protocols: readonly string[] = [], timeoutS = 10): Promise<RawWs> {
        return new Promise((resolve, reject) => {
            const socket = 'path' in target ? connect(target.path) : connect(target.port, '127.0.0.1');
            const ws = new RawWs(socket);
            let upgraded = false;
            const timer = setTimeout(() => { socket.destroy(); reject(new Error('no handshake answer')); }, timeoutS * 1000);
            socket.on('connect', () => {
                const offered = protocols.length ? `Sec-WebSocket-Protocol: ${protocols.join(', ')}\r\n` : '';
                socket.write(`GET ${path} HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n`
                    + `Sec-WebSocket-Key: ${randomBytes(16).toString('base64')}\r\nSec-WebSocket-Version: 13\r\n${offered}\r\n`);
            });
            socket.on('data', (data: Buffer) => {
                ws.buf = Buffer.concat([ws.buf, data]);
                if (!upgraded) {
                    const end = ws.buf.indexOf('\r\n\r\n');
                    if (end < 0) return;
                    const head = ws.buf.subarray(0, end).toString('latin1').split('\r\n');
                    ws.buf = ws.buf.subarray(end + 4);
                    upgraded = true;
                    clearTimeout(timer);
                    const status = Number(head[0].split(' ')[1] ?? 0);
                    if (status !== 101) {
                        ws.refused = status;
                        ws.closed = true;
                        socket.destroy();
                        resolve(ws);
                        return;
                    }
                    const chosen = head.find((l) => l.toLowerCase().startsWith('sec-websocket-protocol:'));
                    ws.protocol = chosen ? chosen.slice(chosen.indexOf(':') + 1).trim() : null;
                    resolve(ws);
                }
                ws.parse();
            });
            socket.on('error', (e) => {
                if (!upgraded) {
                    clearTimeout(timer);
                    reject(e);
                }
                ws.end();
            });
            socket.on('close', () => { ws.end(); });
        });
    }

    private end(): void {
        this.closed = true;
        this.waiter?.();
    }

    private parse(): void {
        while (this.buf.length >= 2) {
            const opcode = this.buf[0] & 0x0f;
            let size = this.buf[1] & 0x7f;
            let at = 2;
            if (size === 126) {
                if (this.buf.length < 4) return;
                size = this.buf.readUInt16BE(2);
                at = 4;
            } else if (size === 127) {
                if (this.buf.length < 10) return;
                size = Number(this.buf.readBigUInt64BE(2));
                at = 10;
            }
            if (this.buf.length < at + size) return;
            const payload = Buffer.from(this.buf.subarray(at, at + size));
            this.buf = this.buf.subarray(at + size);
            if (opcode === 1) this.inbox.push(payload.toString('utf8'));
            else if (opcode === 2) this.inbox.push(payload);
            else if (opcode === 8) {
                this.closeCode = payload.length >= 2 ? payload.readUInt16BE(0) : 1005;
                this.socket.end(frame(8, payload.subarray(0, 2)));
                this.end();
            } else if (opcode === 9) {
                this.socket.write(frame(10, payload));
            }
            this.waiter?.();
        }
    }

    /** Send a text message. */
    send(text: string): void {
        if (!this.closed) this.socket.write(frame(1, Buffer.from(text)));
    }

    /**
     * The next message, waiting up to `timeoutMs` for it.
     * @returns The message, or null when none came or the socket closed.
     */
    async next(timeoutMs: number): Promise<Received | null> {
        const until = Date.now() + timeoutMs;
        for (;;) {
            const got = this.inbox.shift();
            if (got !== undefined) return got;
            if (this.closed || Date.now() >= until) return null;
            await new Promise<void>((resolve) => {
                const timer = setTimeout(() => { this.waiter = null; resolve(); }, until - Date.now());
                this.waiter = () => { clearTimeout(timer); this.waiter = null; resolve(); };
            });
        }
    }

    /** Close the socket with a close frame. */
    async close(): Promise<void> {
        if (this.closed) return;
        this.socket.end(frame(8, Buffer.from([0x03, 0xe8])));
        await new Promise<void>((resolve) => {
            const timer = setTimeout(() => { this.socket.destroy(); resolve(); }, 2000);
            this.socket.once('close', () => { clearTimeout(timer); resolve(); });
        });
        this.closed = true;
    }
}
