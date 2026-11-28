/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The web client's TypeScript as a browser takes it. A page that imports
 * `lib/input.ts` straight from the repository needs the types removed on the
 * way out, and an init script is a classic script: both are stripped with
 * Node's `stripTypeScriptTypes`, the removal the Node audits under
 * `tests/tools` rely on when they import the same modules. Types are blanked
 * rather than deleted, so a stack trace's lines and columns stay the
 * source's. Only stripping is done: a module that needs a real transform (an
 * enum, a namespace, a parameter property) fails here and in the audits
 * alike, which keeps the client inside the subset both can run.
 * @module
 */

import { readFileSync, readdirSync, realpathSync, statSync } from 'node:fs';
import { Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { stripTypeScriptTypes } from 'node:module';
import { extname, join, resolve, sep } from 'node:path';

/**
 * The JavaScript a TypeScript module strips to.
 * @param source The module's text.
 * @returns The same text with every type annotation blanked out.
 * @throws {Error} When the module needs more than stripping.
 */
export function stripTypes(source: string): string {
    return stripTypeScriptTypes(source);
}

/** Content types by extension, for what is served as it is. */
const TYPES: Readonly<Record<string, string>> = {
    '.html': 'text/html; charset=utf-8',
    '.js': 'text/javascript; charset=utf-8',
    '.mjs': 'text/javascript; charset=utf-8',
    '.css': 'text/css; charset=utf-8',
    '.json': 'application/json',
    '.wasm': 'application/wasm',
    '.svg': 'image/svg+xml',
    '.png': 'image/png',
    '.txt': 'text/plain; charset=utf-8',
};

/** A static server on loopback. */
export interface Served {
    server: Server;
    /** `http://127.0.0.1:<port>`. */
    url: string;
    close: () => Promise<void>;
}

/**
 * The path under `root` that a request's `pathname` names, found through the
 * directory listings rather than taken from the request: every segment,
 * percent-decoded, has to be a name its directory lists, so `..`, `.`, a
 * segment holding a slash and a name that is not there all lead nowhere, and
 * what comes back is built only from names the tree already holds.
 * @param root The directory served.
 * @param pathname The request's path, still percent-encoded.
 * @returns The path, or undefined when no listed name matches.
 */
function locate(root: string, pathname: string): string | undefined {
    let at = root;
    for (const raw of pathname.split('/')) {
        if (raw === '') continue;
        let wanted: string;
        let names: string[];
        try {
            wanted = decodeURIComponent(raw);
            names = readdirSync(at);
        } catch {
            return undefined;
        }
        const name = names.find((n) => n === wanted);
        if (name === undefined) return undefined;
        at = join(at, name);
    }
    return at;
}

/** The largest POST body served, in bytes. */
const MAX_POST = 16 * 1024 * 1024;

/**
 * Serve `root` on a loopback port, every `.ts` file but a declaration file as
 * the JavaScript its types strip to (kept by path and modification time,
 * since a page imports dozens of modules on every load), anything else as it
 * is; a module that cannot be stripped is a 500, not half a page.
 * @param root The directory served.
 * @param onPost Takes each POST's path and body, answered 204; without it a POST is served as a GET.
 * @returns The server, once it listens.
 */
export async function serveTypeScript(served: string, onPost?: (path: string, body: Buffer) => void): Promise<Served> {
    const root = resolve(served);
    const realRoot = realpathSync(root);
    const inside = (p: string, base: string): boolean => p === base || p.startsWith(base + sep);
    const stripped = new Map<string, { stamp: number; body: Buffer }>();
    const server = new Server((req, res) => {
        // Nothing served is ever read as another type: an error's text stays text.
        res.setHeader('X-Content-Type-Options', 'nosniff');
        const url = new URL(req.url ?? '/', 'http://127.0.0.1');
        if (req.method === 'POST' && onPost) {
            // A page's report, which is small: a body past the cap is refused, not held.
            const chunks: Buffer[] = [];
            let size = 0;
            req.on('data', (c: Buffer) => {
                size += c.length;
                if (size > MAX_POST) {
                    res.writeHead(413).end();
                    req.destroy();
                    return;
                }
                chunks.push(c);
            });
            req.on('end', () => {
                if (size > MAX_POST) return;
                onPost(url.pathname, Buffer.concat(chunks));
                res.writeHead(204).end();
            });
            return;
        }
        const path = locate(root, url.pathname);
        if (path === undefined) {
            res.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' }).end('not found\n');
            return;
        }
        let stamp: number;
        try {
            // A symbolic link in the tree may point out of it: what is read is
            // where the path really leads, and that has to be inside too.
            if (!inside(realpathSync(path), realRoot)) {
                res.writeHead(403).end();
                return;
            }
            const st = statSync(path);
            if (!st.isFile()) throw new Error('not a file');
            stamp = st.mtimeMs;
        } catch {
            res.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' }).end('not found\n');
            return;
        }
        let body: Buffer;
        let type: string;
        if (path.endsWith('.ts') && !path.endsWith('.d.ts')) {
            let hit = stripped.get(path);
            if (hit === undefined || hit.stamp !== stamp) {
                try {
                    hit = { stamp, body: Buffer.from(stripTypes(readFileSync(path, 'utf8'))) };
                } catch (e) {
                    res.writeHead(500, { 'Content-Type': 'text/plain; charset=utf-8' })
                        .end(`cannot strip types from ${path.slice(root.length)}: ${String(e)}\n`);
                    return;
                }
                stripped.set(path, hit);
            }
            body = hit.body;
            type = 'text/javascript; charset=utf-8';
        } else {
            body = readFileSync(path);
            type = TYPES[extname(path)] ?? 'application/octet-stream';
        }
        res.writeHead(200, { 'Content-Type': type, 'Content-Length': body.length, 'Cache-Control': 'no-store' }).end(body);
    });
    await new Promise<void>((done) => { server.listen(0, '127.0.0.1', done); });
    const address = server.address();
    const port = typeof address === 'object' && address !== null ? (address satisfies AddressInfo).port : 0;
    return {
        server,
        url: `http://127.0.0.1:${port}`,
        close: () => new Promise<void>((done) => { server.closeAllConnections(); server.close(() => { done(); }); }),
    };
}
