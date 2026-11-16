/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The operator's collector the server's audit webhook posts to: it records
 * every event and its headers, and stalls its answers on demand.
 * @module
 */

import { createServer, type IncomingHttpHeaders, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';

import { fields, type Fields } from '../../web/selkies-web-core/lib/narrow.ts';
import { sleep } from './env.ts';

/** A running collector. */
export class Collector {
    readonly events: Fields[] = [];
    readonly headers: IncomingHttpHeaders[] = [];
    /** While true, every answer waits. */
    stall = false;
    url = '';
    private readonly server: Server;

    private constructor() {
        this.server = createServer((req, res) => {
            const chunks: Buffer[] = [];
            req.on('data', (c: Buffer) => { chunks.push(c); });
            req.on('end', () => {
                void (async () => {
                    while (this.stall) await sleep(50);
                    try {
                        this.events.push(fields(JSON.parse(Buffer.concat(chunks).toString('utf8'))));
                    } catch {
                        this.events.push({});
                    }
                    this.headers.push(req.headers);
                    res.writeHead(204).end();
                })();
            });
        });
    }

    /** @returns A collector listening on loopback. */
    static async start(): Promise<Collector> {
        const collector = new Collector();
        await new Promise<void>((done) => { collector.server.listen(0, '127.0.0.1', done); });
        const address = collector.server.address();
        const port = typeof address === 'object' && address !== null ? (address satisfies AddressInfo).port : 0;
        collector.url = `http://127.0.0.1:${port}/audit`;
        return collector;
    }

    /**
     * The events of `kind` once `count` of them have arrived, or time is up.
     * @param kind The event (`session.connect`).
     * @param count How many to wait for.
     * @param timeoutS How long.
     * @returns Those events.
     */
    async waitFor(kind: string, count: number, timeoutS = 15): Promise<Fields[]> {
        const of = (): Fields[] => this.events.filter((e) => e.event === kind);
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until && of().length < count;) await sleep(200);
        return of();
    }

    /** Stop listening. */
    async stop(): Promise<void> {
        this.stall = false;
        this.server.closeAllConnections();
        await new Promise<void>((done) => { this.server.close(() => { done(); }); });
    }
}
