/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Secure mode binds the API routes to the session token, on a real server.
//
// routes:      with a master token set and Basic auth off, /api/upload,
//              /api/files/ (listing and download), /api/turn, and
//              /api/metrics refuse a request without a token and take one as a
//              Bearer header, as ?token=, or as the client's cookie; a
//              viewer-role token can neither upload nor list or download
//              files; the master token is accepted everywhere;
//              /api/screenshot serves any session token, a viewer's included,
//              and challenges a request without one even for a display that
//              does not exist; liveness and the static client stay open; the
//              data WebSocket still takes its token in the handshake, and a
//              token entry with no role connects as a viewer that cannot
//              displace the controller.
// websockets:  a browser page loaded with ?token= streams over WebSockets,
//              sets the API cookie, uploads through the file input with a
//              Bearer header, and opens the file listing (whose links keep the
//              token); a viewer page's upload is refused.
// fragment:    a page loaded with #token= through a reverse proxy that
//              records every request line, in Chromium, Firefox, and WebKit,
//              as a controller and as a #shared viewer: it streams, its data
//              socket offers the token as a subprotocol on a URL without a
//              query, the cookie and the Bearer header carry it, the listing
//              is served on the cookie, and no request line the proxy saw
//              carries a token.
// webrtc:      the same page over WebRTC fetches its TURN configuration with
//              the Bearer header and streams; loaded with #token= through the
//              proxy, in Chromium and Firefox, it does so with no token in any
//              request line.
// dashboards:  both layouts open their file manager with the page's token
//              and the listing renders inside the modal, and on a #token= page
//              the file manager's URL and the listing's links carry none; the
//              sidebar's sharing links carry no token, the sharer's own
//              included, and it switches transport on that same token, asking
//              the user for nothing (the master token is the operator's, not a
//              session user's, so a prompt for it would be unanswerable).
// tokens-file: a table in tokens_file is provisioned at start, so the file's
//              token reaches the API and a page streams over each transport
//              with no POST, and a file renamed over it without that token
//              revokes it.
// legacy:      without a master token nothing changes: the routes are open
//              with Basic auth off and Basic-gated with it on, where the
//              view-only password uploads, lists, and downloads nothing.
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { request as httpRequest } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import type { Browser, Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchBrowser, type Engine } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { barOpenMenuItem } from '../lib/dashboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within, type VideoSize } from '../lib/stream.ts';
import { RawWs } from '../lib/wsclient.ts';
import { WsTap } from '../lib/wstap.ts';

const MASTER = 'e2e-master-token';
const CTRL_TOKEN = 'e2e-ctrl-Qx9';
const VIEW_TOKEN = 'e2e-view-Zt4';
const ROLELESS_TOKEN = 'e2e-player-Rk2';
const COOKIE = 'selkies_token';
const BEARER_CHALLENGE = 'Bearer realm="Selkies Restricted"';
const ENGINES: readonly Engine[] = ['chromium', 'firefox', 'webkit'];
const LAYOUTS = ['default', 'wish', 'edge'] as const;

const SCRATCH = mkdtempSync(join(tmpdir(), 'selkies-secure-mode-'));
const FILES_DIR = join(SCRATCH, 'files');

declare global {
    interface Window {
        __wsFrames?: number;
        __uploads?: { status?: string }[];
        videoChunksReceived?: number;
    }
}

interface Reply {
    status: number;
    headers: Record<string, string>;
    body: Buffer;
}

/** One request against the server; 4xx/5xx and redirects are answers, not errors. */
function request(port: number, method: string, path: string, headers: Record<string, string> = {}, body?: Buffer | string): Promise<Reply> {
    return new Promise((resolve, reject) => {
        // By literal: a page asked for over the IPv6 loopback is sent to the
        // IPv4 one, and this client is not a browser to follow it.
        // A body names its length, as Python's client does: Node frames none
        // for a GET of its own accord, and the bytes would then open the next
        // request on the kept-alive connection.
        const framed = body === undefined ? headers : { 'Content-Length': String(Buffer.byteLength(body)), ...headers };
        const req = httpRequest({ host: '127.0.0.1', port, method, path, headers: framed, timeout: 15_000 }, (r) => {
            const chunks: Buffer[] = [];
            r.on('data', (c: Buffer) => chunks.push(c));
            r.on('end', () => {
                const flat: Record<string, string> = {};
                for (const [k, v] of Object.entries(r.headers)) if (v !== undefined) flat[k.toLowerCase()] = Array.isArray(v) ? v.join(', ') : v;
                resolve({ status: r.statusCode ?? 0, headers: flat, body: Buffer.concat(chunks) });
            });
        });
        req.on('error', reject);
        req.on('timeout', () => { req.destroy(new Error('timeout')); });
        if (body !== undefined) req.write(body);
        req.end();
    });
}

const bearer = (token: string): Record<string, string> => ({ Authorization: `Bearer ${token}` });
const cookie = (token: string): Record<string, string> => ({ Cookie: `${COOKIE}=${encodeURIComponent(token)}` });
const isPng = (b: Buffer): boolean => b.subarray(0, 4).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47]));
const lower = (b: Buffer): string => b.toString('latin1').toLowerCase();

async function upload(port: number, name: string, data: string, headers: Record<string, string> = {}): Promise<number> {
    return (await request(port, 'POST', '/api/upload', { ...headers, 'X-Upload-Path': encodeURIComponent(name), 'Content-Type': 'application/octet-stream' }, data)).status;
}

async function postTokens(port: number, table: unknown): Promise<number> {
    return (await request(port, 'POST', '/api/tokens', { ...bearer(MASTER), 'Content-Type': 'application/json' }, JSON.stringify(table))).status;
}

/** An empty file-manager directory holding one downloadable file. */
function freshFilesDir(): void {
    rmSync(FILES_DIR, { recursive: true, force: true });
    mkdirSync(join(FILES_DIR, 'sub'), { recursive: true });
    writeFileSync(join(FILES_DIR, 'hello.txt'), 'hello from the file manager\n');
}

const secureEnv = (extra: Record<string, string> = {}): Record<string, string> => ({ SELKIES_MASTER_TOKEN: MASTER, SELKIES_FILE_MANAGER_PATH: FILES_DIR, SELKIES_ENABLE_METRICS_HTTP: 'true', ...extra });

async function waitFile(path: string, timeoutS = 15): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (existsSync(path)) return true;
        await sleep(250);
    }
    return false;
}

const readText = (path: string): string => (existsSync(path) ? readFileSync(path, 'utf8') : '');

/** The subprotocols a page holding the token in its fragment offers. */
function tokenProtocols(token: string): string[] {
    return ['selkies', `selkies.token.${Buffer.from(token).toString('base64url')}`];
}

interface Handshake {
    messages: string[];
    code: number | null;
    refused: number | null;
    selected: string | null;
}

/** Connect the data socket and collect the handshake's text messages. */
async function wsHandshake(port: number, query: string, seconds = 3, protocols: string[] = []): Promise<Handshake> {
    const out: Handshake = { messages: [], code: null, refused: null, selected: null };
    let ws: RawWs;
    try {
        ws = await RawWs.open({ port }, `/api/websockets${query}`, protocols);
    } catch (e) {
        out.messages.push(`ERROR ${String(e)}`);
        return out;
    }
    if (ws.refused !== null) {
        out.refused = ws.refused;
        return out;
    }
    out.selected = ws.protocol;
    for (const until = Date.now() + seconds * 1000; Date.now() < until;) {
        const msg = await ws.next(500);
        if (typeof msg === 'string') {
            out.messages.push(msg);
            if (msg.startsWith('MK_ACCESS')) break;
        }
        if (ws.closed) break;
    }
    out.code = ws.closeCode;
    await ws.close();
    return out;
}

/**
 * A controller's socket claims the primary, then a token entry with no role
 * connects and sends the primary's settings: the role-less socket's
 * AUTH_SUCCESS payload and whether the controller's socket was closed.
 */
async function rolelessBesideController(port: number): Promise<[unknown, boolean]> {
    const settings = JSON.stringify({ displayId: 'primary', initialClientWidth: 1280, initialClientHeight: 720, framerate: 30, encoder: 'jpeg' });
    await sleep(1000); // past the server's reconnect debounce
    const ctrl = await RawWs.open({ port }, `/api/websockets?token=${CTRL_TOKEN}`);
    ctrl.send(`SETTINGS,${settings}`);
    await sleep(2000);
    let auth: unknown = null;
    const other = await RawWs.open({ port }, `/api/websockets?token=${ROLELESS_TOKEN}`);
    for (const until = Date.now() + 3000; auth === null && Date.now() < until;) {
        const msg = await other.next(3000);
        if (typeof msg === 'string' && msg.startsWith('AUTH_SUCCESS,')) auth = JSON.parse(msg.slice(msg.indexOf(',') + 1));
        if (msg === null) break;
    }
    other.send(`SETTINGS,${settings}`);
    await sleep(3000);
    await other.close();
    // Whatever the controller was sent is drained; a close among it is what is asked.
    while ((await ctrl.next(200)) !== null);
    const closed = ctrl.closeCode !== null;
    await ctrl.close();
    return [auth, closed];
}

async function provision(res: Checks, port: number): Promise<void> {
    const status = await postTokens(port, { [CTRL_TOKEN]: { role: 'controller', slot: 1 }, [VIEW_TOKEN]: { role: 'viewer', slot: null }, [ROLELESS_TOKEN]: { slot: 2 } });
    res.check('tokens provisioned', status === 200, status);
}

const field = (o: unknown, k: string): unknown => {
    const v: unknown = o !== null && typeof o === 'object' ? Reflect.get(o, k) : undefined;
    return v;
};

async function runRoutes(host: Host): Promise<Checks> {
    const res = new Checks('routes');
    freshFilesDir();
    const { port } = await host.start({ label: 'routes', mode: 'websockets', env: secureEnv() });
    const req = (method: string, path: string, headers: Record<string, string> = {}, body?: Buffer | string): Promise<Reply> => request(port, method, path, headers, body);
    await provision(res, port);

    for (const [method, path] of [['POST', '/api/upload'], ['GET', '/api/files/'], ['GET', '/api/files/hello.txt'], ['GET', '/api/turn'], ['GET', '/api/metrics']] as const) {
        const r = await req(method, path, { 'X-Upload-Path': 'nope.txt' }, 'x');
        res.check(`no token: ${method} ${path} refused with the Bearer challenge`, r.status === 401 && r.headers['www-authenticate'] === BEARER_CHALLENGE, `${r.status} ${JSON.stringify(r.headers['www-authenticate'] ?? null)}`);
    }
    for (const path of ['/api/status', '/api/health', '/']) {
        const r = await req('GET', path);
        res.check(`no token: GET ${path} stays open`, r.status === 200, r.status);
    }
    let r = await req('GET', '/');
    res.check('the static client is served without a token', lower(r.body).includes('<html') || lower(r.body).includes('<!doctype'), r.body.subarray(0, 60).toString());

    // The screenshot: any session token sees the screen, a viewer's included,
    // as the view-only password does; without one, even a display that does
    // not exist is a challenge, never a 404 that tells which displays do.
    for (const path of ['/api/screenshot', '/api/screenshot?display=display9']) {
        r = await req('GET', path);
        res.check(`no token: GET ${path} refused with the Bearer challenge`, r.status === 401 && r.headers['www-authenticate'] === BEARER_CHALLENGE && !isPng(r.body), `${r.status} ${JSON.stringify(r.headers['www-authenticate'] ?? null)}`);
    }
    for (const [label, headers, query] of [['Bearer controller', bearer(CTRL_TOKEN), ''], ['cookie controller', cookie(CTRL_TOKEN), ''], ['?token= controller', {}, `?token=${CTRL_TOKEN}`],
        ['Bearer master', bearer(MASTER), ''], ['Bearer viewer', bearer(VIEW_TOKEN), ''], ['cookie viewer', cookie(VIEW_TOKEN), ''], ['?token= viewer', {}, `?token=${VIEW_TOKEN}`],
        ['Bearer roleless', bearer(ROLELESS_TOKEN), '']] as const) {
        r = await req('GET', `/api/screenshot${query}`, headers);
        res.check(`${label}: the screenshot is served`, r.status === 200 && isPng(r.body) && r.headers['content-type'] === 'image/png', `${r.status} ${r.headers['content-type']}`);
    }
    r = await req('GET', '/api/screenshot', bearer(CTRL_TOKEN.slice(0, -1)));
    res.check('Bearer prefix of a token: the screenshot is refused', r.status === 401 && !isPng(r.body), r.status);

    res.check('Bearer controller: upload accepted', (await upload(port, 'up-bearer.txt', 'bearer', bearer(CTRL_TOKEN))) === 200);
    res.check('Bearer controller: the upload landed', (await waitFile(join(FILES_DIR, 'up-bearer.txt'), 5)) && readText(join(FILES_DIR, 'up-bearer.txt')) === 'bearer');
    res.check('cookie controller: upload accepted (no Origin)', (await upload(port, 'up-cookie.txt', 'cookie', cookie(CTRL_TOKEN))) === 200);
    res.check('cookie controller: the upload landed', await waitFile(join(FILES_DIR, 'up-cookie.txt'), 5));
    r = await req('POST', '/api/upload', { ...cookie(CTRL_TOKEN), Origin: 'https://evil.example', 'X-Upload-Path': 'csrf.txt' }, 'x');
    res.check('cookie controller: a cross-site POST is refused by Origin', r.status === 403 && !existsSync(join(FILES_DIR, 'csrf.txt')), r.status);
    r = await req('POST', `/api/upload?token=${CTRL_TOKEN}`, { 'X-Upload-Path': 'up-query.txt' }, 'q');
    res.check('?token= controller: upload accepted', r.status === 200, r.status);
    res.check('Bearer master: upload accepted', (await upload(port, 'up-master.txt', 'm', bearer(MASTER))) === 200);
    let status = await upload(port, 'up-viewer.txt', 'v', bearer(VIEW_TOKEN));
    res.check('Bearer viewer: upload refused', status === 403 && !existsSync(join(FILES_DIR, 'up-viewer.txt')), status);
    status = await upload(port, 'up-viewer-cookie.txt', 'v', cookie(VIEW_TOKEN));
    res.check('cookie viewer: upload refused', status === 403, status);
    res.check('Bearer prefix of a token: upload refused', (await upload(port, 'up-prefix.txt', 'p', bearer(CTRL_TOKEN.slice(0, -1)))) === 401);

    for (const [label, headers, path] of [['Bearer controller', bearer(CTRL_TOKEN), '/api/files/'], ['cookie controller', cookie(CTRL_TOKEN), '/api/files/'],
        ['?token= controller', {}, `/api/files/?token=${CTRL_TOKEN}`], ['Bearer master', bearer(MASTER), '/api/files/']] as const) {
        r = await req('GET', path, headers);
        res.check(`${label}: the file listing is served`, r.status === 200 && r.body.includes('hello.txt'), `${r.status} ${r.body.subarray(0, 40).toString()}`);
    }
    for (const [label, headers, path] of [['Bearer viewer', bearer(VIEW_TOKEN), '/api/files/'], ['cookie viewer', cookie(VIEW_TOKEN), '/api/files/'], ['?token= viewer', {}, `/api/files/?token=${VIEW_TOKEN}`]] as const) {
        r = await req('GET', path, headers);
        res.check(`${label}: the file listing is refused`, r.status === 403 && !r.body.includes('hello.txt'), `${r.status} ${r.body.subarray(0, 40).toString()}`);
    }
    r = await req('GET', '/api/files/hello.txt', cookie(CTRL_TOKEN));
    res.check('cookie controller: a download is served as an attachment', r.status === 200 && r.body.toString() === 'hello from the file manager\n' && (r.headers['content-disposition'] ?? '').includes('attachment'), `${r.status} ${r.headers['content-disposition']}`);
    r = await req('GET', `/api/files/hello.txt?token=${VIEW_TOKEN}`);
    res.check('?token= viewer: a download is refused', r.status === 403 && !r.body.includes('hello'), r.status);
    r = await req('GET', '/api/files/hello.txt', bearer(VIEW_TOKEN));
    res.check('Bearer viewer: a download is refused', r.status === 403 && !r.body.includes('hello'), r.status);
    r = await req('GET', '/api/files/sub', bearer(CTRL_TOKEN));
    res.check('a directory without its slash redirects (token carried by the header)', [301, 308].includes(r.status) && (r.headers.location ?? '').endsWith('/api/files/sub/'), `${r.status} ${r.headers.location}`);
    r = await req('GET', `/api/files/sub?token=${CTRL_TOKEN}`);
    res.check('the redirect keeps the ?token= query', [301, 308].includes(r.status) && (r.headers.location ?? '').includes(`token=${CTRL_TOKEN}`), `${r.status} ${r.headers.location}`);

    // The file manager's JSON API: bound like the index, a viewer refused
    // every route of it, and the folder it creates or archives refused to
    // another site.
    for (const [method, path] of [['GET', '/api/file-manager/browse'], ['POST', '/api/file-manager/mkdir'], ['GET', '/api/file-manager/zip?path=sub']] as const) {
        r = await req(method, path, { 'Content-Type': 'application/json' }, method === 'POST' ? '{"path": "anon-dir"}' : undefined);
        res.check(`no token: ${method} ${path} refused with the Bearer challenge`, r.status === 401 && r.headers['www-authenticate'] === BEARER_CHALLENGE, `${r.status} ${JSON.stringify(r.headers['www-authenticate'] ?? null)}`);
    }
    for (const [label, headers, path] of [['Bearer controller', bearer(CTRL_TOKEN), '/api/file-manager/browse'], ['cookie controller', cookie(CTRL_TOKEN), '/api/file-manager/browse'],
        ['?token= controller', {}, `/api/file-manager/browse?token=${CTRL_TOKEN}`], ['Bearer master', bearer(MASTER), '/api/file-manager/browse']] as const) {
        r = await req('GET', path, headers);
        res.check(`${label}: the JSON listing is served`, r.status === 200 && r.body.includes('hello.txt'), `${r.status} ${r.body.subarray(0, 40).toString()}`);
    }
    for (const [label, headers, query] of [['Bearer viewer', bearer(VIEW_TOKEN), ''], ['cookie viewer', cookie(VIEW_TOKEN), ''], ['?token= viewer', {}, `?token=${VIEW_TOKEN}`], ['Bearer roleless', bearer(ROLELESS_TOKEN), '']] as const) {
        r = await req('GET', `/api/file-manager/browse${query}`, headers);
        res.check(`${label}: the JSON listing is refused`, r.status === 403 && !r.body.includes('hello.txt'), `${r.status} ${r.body.subarray(0, 40).toString()}`);
        r = await req('POST', `/api/file-manager/mkdir${query}`, { ...headers, 'Content-Type': 'application/json' }, '{"path": "viewer-dir"}');
        res.check(`${label}: creating a folder is refused`, r.status === 403 && !existsSync(join(FILES_DIR, 'viewer-dir')), r.status);
        r = await req('GET', `/api/file-manager/zip${query}${query ? '&' : '?'}path=sub`, headers);
        res.check(`${label}: a folder archive is refused`, r.status === 403 && r.body.subarray(0, 2).toString() !== 'PK', r.status);
    }
    r = await req('POST', '/api/file-manager/mkdir', { ...cookie(CTRL_TOKEN), Origin: 'https://evil.example', 'Content-Type': 'text/plain' }, '{"path": "csrf-dir"}');
    res.check('cookie controller: a cross-site folder creation is refused', r.status === 403 && !existsSync(join(FILES_DIR, 'csrf-dir')), r.status);
    r = await req('GET', '/api/file-manager/zip?path=sub', { ...cookie(CTRL_TOKEN), 'Sec-Fetch-Site': 'cross-site' });
    res.check('cookie controller: a cross-site folder archive is refused', r.status === 403 && r.body.subarray(0, 2).toString() !== 'PK', r.status);
    r = await req('POST', '/api/file-manager/mkdir', { ...bearer(CTRL_TOKEN), 'Content-Type': 'application/json' }, '{"path": "made-by-controller"}');
    const made = join(FILES_DIR, 'made-by-controller');
    res.check('Bearer controller: a folder is created', r.status === 200 && existsSync(made) && statSync(made).isDirectory(), r.status);

    for (const [label, headers] of [['Bearer controller', bearer(CTRL_TOKEN)], ['cookie viewer', cookie(VIEW_TOKEN)], ['Bearer master', bearer(MASTER)]] as const) {
        r = await req('GET', '/api/metrics', headers);
        res.check(`${label}: metrics served`, r.status === 200 && r.body.includes('# '), `${r.status} ${r.body.subarray(0, 40).toString()}`);
    }
    r = await req('GET', '/api/turn', bearer(CTRL_TOKEN));
    res.check('Bearer controller: /api/turn passes the gate (409: WebRTC inactive)', r.status === 409, r.status);

    let hs = await wsHandshake(port, `?token=${CTRL_TOKEN}`);
    res.check('the data WebSocket still authenticates with ?token=', hs.messages.some((m) => m.startsWith('AUTH_SUCCESS')), `${hs.code} ${JSON.stringify(hs.messages.slice(0, 3))}`);
    hs = await wsHandshake(port, `?token=${CTRL_TOKEN.slice(0, -1)}`, 2);
    res.check('the data WebSocket still refuses a wrong token', hs.code === 4001, `${hs.code} ${JSON.stringify(hs.messages.slice(0, 2))}`);
    hs = await wsHandshake(port, '', 3, tokenProtocols(CTRL_TOKEN));
    res.check('the data WebSocket authenticates with the token subprotocol and selects selkies', hs.messages.some((m) => m.startsWith('AUTH_SUCCESS')) && hs.selected === 'selkies', `${hs.selected} ${hs.code} ${JSON.stringify(hs.messages.slice(0, 3))}`);
    hs = await wsHandshake(port, '', 2, tokenProtocols(CTRL_TOKEN.slice(0, -1)));
    res.check('the data WebSocket refuses a wrong subprotocol token', hs.code === 4001, `${hs.code} ${JSON.stringify(hs.messages.slice(0, 2))}`);
    hs = await wsHandshake(port, '', 3, tokenProtocols(CTRL_TOKEN).slice(1));
    res.check('the token subprotocol without selkies is refused before the upgrade', hs.refused === 400, hs.refused);
    hs = await wsHandshake(port, '');
    res.check('a handshake with no token is refused before the upgrade', hs.refused === 401, hs.refused);
    const [auth, closed] = await rolelessBesideController(port);
    res.check('a token entry with no role is told it is a viewer', field(auth, 'role') === 'viewer', auth);
    res.check("its primary settings leave the controller's socket open", !closed, '');
    const log = await host.log();
    res.check('session tokens never reach the server log', !log.includes(CTRL_TOKEN) && !log.includes(VIEW_TOKEN) && !log.includes(ROLELESS_TOKEN), '');
    await host.stop();
    return res;
}

/** The request lines holding the token, as it is or percent-encoded. */
function linesCarrying(lines: readonly string[], token: string): string[] {
    const needles = [token, encodeURIComponent(token)];
    return lines.filter((l) => needles.some((n) => l.includes(n)));
}

type Seen = [string, string, Record<string, string>];

/** A page on the server (or on `base`, a proxy in front of it) with the given query and hash. */
async function launch(url: string, query: string, mode: string, hash = '', engine: Engine = 'chromium', base = ''): Promise<[Browser, Page, Seen[]]> {
    const browser = await launchBrowser(engine);
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    const page = await ctx.newPage();
    const requests: Seen[] = [];
    page.on('request', (r) => requests.push([r.method(), r.url(), r.headers()]));
    await page.addInitScript({ content: `
      window.__wsFrames = 0;
      (() => {
        const WS = window.WebSocket;
        window.WebSocket = function(...a) {
          const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
          s.addEventListener('message', (e) => { if (e.data instanceof ArrayBuffer) window.__wsFrames++; });
          return s;
        };
        window.WebSocket.prototype = WS.prototype;
        Object.setPrototypeOf(window.WebSocket, WS);
      })();
      window.__uploads = [];
      window.addEventListener('message', (e) => {
        if (e.data && e.data.type === 'fileUpload') window.__uploads.push(e.data.payload);
      });
    ` });
    await page.goto(`${base || url}/${query}${hash}`, { waitUntil: 'load' });
    return [browser, page, requests];
}

const uploads = (page: Page): Promise<{ status?: string }[]> => within(page.evaluate(() => window.__uploads ?? []));

async function waitUploadEnd(page: Page, timeoutS = 20): Promise<{ status?: string }[]> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        const ups = await uploads(page);
        if (ups.some((u) => u.status === 'end' || u.status === 'error')) return ups;
        await sleep(300);
    }
    return uploads(page);
}

async function apiCookie(page: Page): Promise<{ value?: string; path?: string; sameSite?: string }> {
    return (await page.context().cookies()).find((c) => c.name === COOKIE) ?? {};
}

const listingHrefs = (page: Page): Promise<(string | null)[]> => ev<(string | null)[]>(page, "new Promise(r => setTimeout(() => r([...document.querySelectorAll('table#list td a')].map(a => a.getAttribute('href'))), 800))");
const ev = <T>(page: Page, script: string): Promise<T> => within(page.evaluate<T>(script));
const forcedUpload = (page: Page, token: string): Promise<number> => within(page.evaluate(async (t) => {
    const r = await fetch('api/upload', { method: 'POST', body: 'nope', headers: { Authorization: `Bearer ${t}`, 'X-Upload-Path': 'viewer-upload.txt' } });
    return r.status;
}, token));
const uploadPosts = (requests: readonly Seen[]): Seen[] => requests.filter((r) => r[0] === 'POST' && r[1].includes('/api/upload'));
const allBearer = (rs: readonly Seen[], token: string): boolean => rs.length > 0 && rs.every((r) => r[2].authorization === `Bearer ${token}`);

async function runWebsockets(host: Host): Promise<Checks> {
    const res = new Checks('websockets');
    freshFilesDir();
    const { url, port } = await host.start({ label: 'websockets', mode: 'websockets', env: secureEnv() });
    await provision(res, port);
    let [browser, page, requests] = await launch(url, `?token=${CTRL_TOKEN}`, 'websockets');
    try {
        const info = await waitWsVideo(page, 30);
        res.check('controller page streams over WebSockets with ?token=', info !== null, info);
        const c = await apiCookie(page);
        res.check('the page set the API cookie from its token', decodeURIComponent(c.value ?? '') === CTRL_TOKEN && c.path === '/api/' && String(c.sameSite ?? '').toLowerCase() === 'strict', c);
        await page.setInputFiles('#globalFileInput', { name: 'browser-upload.txt', mimeType: 'text/plain', buffer: Buffer.from('from the browser\n') });
        const ups = await waitUploadEnd(page);
        res.check('the upload through the file input completes', ups.some((u) => u.status === 'end'), ups.slice(-2));
        res.check('the upload landed in the file manager directory', (await waitFile(join(FILES_DIR, 'browser-upload.txt'), 5)) && readText(join(FILES_DIR, 'browser-upload.txt')) === 'from the browser\n');
        const posts = uploadPosts(requests);
        res.check('the upload request carried the Bearer token', allBearer(posts, CTRL_TOKEN), posts.map((r) => r[2].authorization));

        const listing = await page.context().newPage();
        await listing.goto(`${url}/api/files/?token=${CTRL_TOKEN}`, { waitUntil: 'load' });
        const hrefs = await listingHrefs(listing);
        res.check('the tokened listing renders and its links keep the token',
            hrefs.length > 0 && hrefs.some((h) => h?.startsWith('hello.txt?token=')) && hrefs.every((h) => h?.includes(`token=${encodeURIComponent(CTRL_TOKEN)}`)), hrefs);
        await listing.goto(`${url}/api/files/`, { waitUntil: 'load' });
        res.check('the bare listing is served on the cookie alone', (await listing.content()).includes('hello.txt'), listing.url());
        const response = await listing.request.get(`${url}/api/files/hello.txt`);
        res.check('a download on the cookie alone is served', response.status() === 200 && (await response.text()) === 'hello from the file manager\n', response.status());
        await listing.close();
    } finally {
        await browser.close();
    }

    [browser, page, requests] = await launch(url, `?token=${VIEW_TOKEN}`, 'websockets');
    try {
        const info = await waitWsVideo(page, 30);
        res.check('viewer page streams over WebSockets with ?token=', info !== null, info);
        // The core itself does not upload for a viewer (the token's role puts
        // the page in shared mode); the server is the backstop, so the request
        // is made from the page directly as a script would.
        await page.setInputFiles('#globalFileInput', { name: 'viewer-upload.txt', mimeType: 'text/plain', buffer: Buffer.from('nope\n') });
        await sleep(3000);
        res.check("the viewer page's file input starts no upload", uploadPosts(requests).length === 0 && !existsSync(join(FILES_DIR, 'viewer-upload.txt')));
        const status = await forcedUpload(page, VIEW_TOKEN);
        res.check('an upload forced from the viewer page is refused by the server', status === 403 && !existsSync(join(FILES_DIR, 'viewer-upload.txt')), status);
        const frames = await ev<number>(page, 'window.videoChunksReceived || window.__wsFrames');
        res.check('the refusal did not reload the page', frames > 0 && page.url().endsWith(`?token=${VIEW_TOKEN}`), page.url());
    } finally {
        await browser.close();
        await host.stop();
    }
    return res;
}

async function runFragment(host: Host): Promise<Checks> {
    const res = new Checks('fragment');
    freshFilesDir();
    const { url, port } = await host.start({ label: 'fragment', mode: 'websockets', env: secureEnv() });
    await provision(res, port);
    const tap = await WsTap.start(port);
    const base = `http://127.0.0.1:${tap.port}`;
    try {
        for (const engine of ENGINES) {
            let mark = tap.requests.length;
            const opened = await launch(url, '', 'websockets', `#token=${CTRL_TOKEN}`, engine, base);
            let [browser, page] = opened;
            const requests = opened[2];
            try {
                const info = await waitWsVideo(page, 30);
                res.check(`${engine}: a page opened with #token= streams over WebSockets`, info !== null, info);
                const c = await apiCookie(page);
                res.check(`${engine}: the page set the API cookie from its fragment token`, decodeURIComponent(c.value ?? '') === CTRL_TOKEN && c.path === '/api/', c);
                const name = `${engine}-fragment-upload.txt`;
                await page.setInputFiles('#globalFileInput', { name, mimeType: 'text/plain', buffer: Buffer.from('from the fragment page\n') });
                const ups = await waitUploadEnd(page);
                const posts = uploadPosts(requests);
                res.check(`${engine}: the upload completed on the Bearer token`, ups.some((u) => u.status === 'end') && (await waitFile(join(FILES_DIR, name), 5)) && allBearer(posts, CTRL_TOKEN), ups.slice(-2));
                const listing = await page.context().newPage();
                await listing.goto(`${base}/api/files/`, { waitUntil: 'load' });
                const hrefs = await listingHrefs(listing);
                res.check(`${engine}: the listing is served on the cookie, its links without a token`, (await listing.content()).includes('hello.txt') && hrefs.length > 0 && hrefs.every((h) => !(h ?? '').includes('token')), hrefs);
                await listing.close();
            } finally {
                await browser.close();
            }
            let lines = tap.requests.slice(mark);
            const sockets = lines.filter((l) => (l.split(' ')[1] ?? '').startsWith('/api/websockets'));
            res.check(`${engine}: the data socket's URL has no query and offers the token subprotocol`,
                sockets.some((l) => l.endsWith(' selkies,selkies.token.*')) && sockets.every((l) => !(l.split(' ')[1] ?? '').includes('?')), sockets);
            let carrying = linesCarrying(lines, CTRL_TOKEN);
            res.check(`${engine}: no request line carries the token`, carrying.length === 0, carrying.slice(0, 3));

            mark = tap.requests.length;
            [browser, page] = await launch(url, '', 'websockets', `#shared&token=${VIEW_TOKEN}`, engine, base);
            try {
                const info = await waitWsVideo(page, 30);
                res.check(`${engine}: a #shared&token= viewer page streams`, info !== null, info);
                const status = await forcedUpload(page, VIEW_TOKEN);
                res.check(`${engine}: an upload forced from the viewer page is refused`, status === 403 && !existsSync(join(FILES_DIR, 'viewer-upload.txt')), status);
            } finally {
                await browser.close();
            }
            lines = tap.requests.slice(mark);
            carrying = linesCarrying(lines, VIEW_TOKEN);
            res.check(`${engine}: no request line carries the viewer's token`, carrying.length === 0, carrying.slice(0, 3));
        }
    } finally {
        await tap.stop();
    }
    const log = await host.log();
    res.check('session tokens never reach the server log', !log.includes(CTRL_TOKEN) && !log.includes(VIEW_TOKEN), '');
    await host.stop();
    return res;
}

async function runWebrtc(host: Host): Promise<Checks> {
    const res = new Checks('webrtc');
    freshFilesDir();
    const { url, port } = await host.start({ label: 'webrtc', mode: 'webrtc', env: secureEnv() });
    await provision(res, port);
    let r = await request(port, 'GET', '/api/turn');
    res.check('no token: /api/turn refused', r.status === 401 && r.headers['www-authenticate'] === BEARER_CHALLENGE, `${r.status} ${JSON.stringify(r.headers['www-authenticate'] ?? null)}`);
    r = await request(port, 'GET', '/api/turn', bearer(CTRL_TOKEN));
    res.check('Bearer controller: /api/turn serves the RTC configuration', r.status === 200 && r.body.includes('iceServers'), `${r.status} ${r.body.subarray(0, 60).toString()}`);
    r = await request(port, 'GET', '/api/turn', cookie(VIEW_TOKEN));
    res.check('cookie viewer: /api/turn serves the RTC configuration', r.status === 200, r.status);
    const tap = await WsTap.start(port);
    const base = `http://127.0.0.1:${tap.port}`;
    try {
        for (const engine of ['chromium', 'firefox'] as const) {
            const mark = tap.requests.length;
            const [browser, page, requests] = await launch(url, '', 'webrtc', `#token=${CTRL_TOKEN}`, engine, base);
            try {
                const info = await waitWrVideo(page, 60);
                res.check(`${engine}: a page opened with #token= streams over WebRTC`, info !== null, info);
                const turns = requests.filter((q) => q[1].includes('/api/turn'));
                res.check(`${engine}: its TURN fetch carried the fragment token as Bearer`, allBearer(turns, CTRL_TOKEN), turns.map((q) => q[2].authorization));
            } finally {
                await browser.close();
            }
            const carrying = linesCarrying(tap.requests.slice(mark), CTRL_TOKEN);
            res.check(`${engine}: no request line of the WebRTC page carries the token`, carrying.length === 0, carrying.slice(0, 3));
        }
    } finally {
        await tap.stop();
    }
    const [browser, page, requests] = await launch(url, `?token=${CTRL_TOKEN}`, 'webrtc');
    try {
        const info = await waitWrVideo(page, 60);
        res.check('controller page streams over WebRTC with ?token=', info !== null, info);
        const turns = requests.filter((q) => q[1].includes('/api/turn'));
        res.check('the TURN fetch carried the Bearer token', allBearer(turns, CTRL_TOKEN), turns.map((q) => q[2].authorization));
        const c = await apiCookie(page);
        res.check('the WebRTC page set the same API cookie', decodeURIComponent(c.value ?? '') === CTRL_TOKEN && c.path === '/api/', c);
        await page.setInputFiles('#globalFileInput', { name: 'webrtc-upload.txt', mimeType: 'text/plain', buffer: Buffer.from('over webrtc\n') });
        const ups = await waitUploadEnd(page);
        res.check('the upload from the WebRTC page completes', ups.some((u) => u.status === 'end') && (await waitFile(join(FILES_DIR, 'webrtc-upload.txt'), 5)), ups.slice(-2));
    } finally {
        await browser.close();
    }
    // A collaboration room opens each participant's session this way: every
    // page with its own token and none with #playerN, so each hello names slot 1.
    const mark = await host.mark();
    const pages = [await launch(url, `?token=${CTRL_TOKEN}`, 'webrtc'), await launch(url, `?token=${VIEW_TOKEN}`, 'webrtc')];
    try {
        const shown: (VideoSize | null)[] = [];
        for (const [, p] of pages) shown.push(await waitWrVideo(p, 60));
        await sleep(5000);
        const later = await host.log(mark);
        res.check('a controller page and a viewer page, each with its own token, stream at once', shown.every(Boolean), shown);
        res.check('and neither supersedes the other', !later.includes('Evicting') && !later.includes('takeover storm'), later.split('\n').filter((l) => l.includes('Evicting') || l.includes('storm')).slice(0, 3));
    } finally {
        for (const [b] of pages) await b.close();
        await host.stop();
    }
    return res;
}

/** Put `table` at `path` in one rename, readable by its owner alone. */
function writeTokensFile(path: string, table: unknown): void {
    const staged = `${path}.new`;
    writeFileSync(staged, JSON.stringify(table));
    chmodSync(staged, 0o600);
    renameSync(staged, path);
}

/** Poll `path` until it answers `want`; the last status. */
async function waitStatus(port: number, path: string, headers: Record<string, string>, want: number, timeoutS = 15): Promise<number> {
    for (const until = Date.now() + timeoutS * 1000; ;) {
        const { status } = await request(port, 'GET', path, headers);
        if (status === want || Date.now() > until) return status;
        await sleep(500);
    }
}

async function runTokensFile(host: Host): Promise<Checks> {
    const res = new Checks('tokens-file');
    freshFilesDir();
    const path = join(SCRATCH, 'tokens.json');
    for (const mode of ['websockets', 'webrtc']) {
        writeTokensFile(path, { [CTRL_TOKEN]: { role: 'controller', slot: 1 } });
        const { url, port } = await host.start({ label: `tokens-${mode}`, mode, env: secureEnv({ SELKIES_TOKENS_FILE: path }) });
        let status = await waitStatus(port, '/api/files/', bearer(CTRL_TOKEN), 200);
        res.check(`${mode}: the file's token reaches the API with no POST`, status === 200, status);
        const [browser, page] = await launch(url, `?token=${CTRL_TOKEN}`, mode);
        try {
            const info = mode === 'webrtc' ? await waitWrVideo(page, 60) : await waitWsVideo(page, 60);
            res.check(`${mode}: a page streams on the file's token`, info !== null, info);
        } finally {
            await browser.close();
        }
        writeTokensFile(path, { [VIEW_TOKEN]: { role: 'viewer', slot: null } });
        status = await waitStatus(port, '/api/files/', bearer(CTRL_TOKEN), 401);
        res.check(`${mode}: a file renamed over it without the token revokes it`, status === 401, status);
        const log = await host.log();
        res.check(`${mode}: the file's tokens never reach the server log`, !log.includes(CTRL_TOKEN) && !log.includes(VIEW_TOKEN), '');
        await host.stop();
    }
    return res;
}

/** Click through the dashboard to its file manager: the sidebar's Download Files button, or a bar layout's menu's Files item. */
async function openFilesModal(page: Page, dashboard: string): Promise<boolean> {
    try {
        if (dashboard === 'default') {
            await page.locator('.toggle-handle').first().click();
            await sleep(500);
            await page.locator('.sidebar-section-header:has-text("Files")').first().click();
            await sleep(500);
            await page.locator('button:has-text("Download Files")').first().click();
        } else if (!(await barOpenMenuItem(page, 'Files'))) {
            return false;
        }
        await sleep(1000);
        return true;
    } catch (e) {
        process.stdout.write(`      (files modal: ${String(e)})\n`);
        return false;
    }
}

/**
 * The file manager lists the directory over the JSON API, whose requests
 * carry the session token as a Bearer header and never in the URL, however the
 * page itself was handed it.
 */
async function checkManager(res: Checks, page: Page, requests: readonly Seen[], label: string, dashboard: string): Promise<void> {
    const dialog = '[role="dialog"]:has-text("hello.txt")';
    for (const until = Date.now() + 15_000; Date.now() < until && !(await page.locator(dialog).count());) await sleep(500);
    res.check(`${dashboard}: the file manager lists the directory on the ${label} page`, (await page.locator(dialog).count()) > 0);
    const browses = requests.filter((r) => r[1].includes('/api/file-manager/browse'));
    res.check(`${dashboard}: the listing's requests carry the Bearer token on the ${label} page`, allBearer(browses, CTRL_TOKEN), browses.map((r) => r[2].authorization));
    res.check(`${dashboard}: no listing request carries the token in its URL on the ${label} page`, browses.length > 0 && browses.every((r) => !r[1].includes('token')), browses.map((r) => r[1]));
}

async function runDashboards(host: Host): Promise<Checks> {
    const res = new Checks('dashboards');
    for (const dashboard of LAYOUTS) {
        if (!existsSync(join(DASHBOARD_DIST, 'index.html'))) {
            res.skip(`${dashboard}: dist not built`, DASHBOARD_DIST);
            continue;
        }
        freshFilesDir();
        const { url, port } = await host.start({ label: `dash-${dashboard}`, mode: 'websockets', env: { SELKIES_WEB_ROOT: DASHBOARD_DIST, SELKIES_UI_LAYOUT: dashboard, ...secureEnv({ SELKIES_ENABLE_DUAL_MODE: 'true' }) } });
        await provision(res, port);
        let [browser, page, requests] = await launch(url, '', 'websockets', `#token=${CTRL_TOKEN}`);
        try {
            const info = await waitWsVideo(page, 30);
            res.check(`${dashboard}: the dashboard page streams with #token=`, info !== null, info);
            res.check(`${dashboard}: the files modal opens on the #token= page`, await openFilesModal(page, dashboard));
            await checkManager(res, page, requests, '#token=', dashboard);
        } finally {
            await browser.close();
        }
        [browser, page, requests] = await launch(url, `?token=${CTRL_TOKEN}`, 'websockets');
        const navigations: string[] = [];
        const prompts: string[] = [];
        page.on('request', (q) => { if (q.isNavigationRequest() && q.frame() === page.mainFrame()) navigations.push(q.url()); });
        page.on('dialog', (d) => { prompts.push(d.message()); void d.accept(MASTER); });
        try {
            const info = await waitWsVideo(page, 30);
            res.check(`${dashboard}: the dashboard page streams with ?token=`, info !== null, info);
            res.check(`${dashboard}: the files modal opens`, await openFilesModal(page, dashboard));
            await checkManager(res, page, requests, '?token=', dashboard);
            if (dashboard === 'default') {
                // A controller switches on the token its own page was opened
                // with. Nobody is asked to paste the master token: it is an
                // operator credential a session user does not hold.
                await page.keyboard.press('Escape');
                await sleep(500);
                await page.locator('.sidebar-section-header:has-text("Sharing")').first().click();
                await sleep(500);
                const shared = await ev<string[]>(page, "(() => { const links = []; for (const b of document.querySelectorAll('#sharing-content button[aria-label]')) links.push(b.getAttribute('aria-label')); return links; })()");
                res.check('default: the sharing section lists the links to copy', shared.length >= 1, shared);
                const shareable = await ev<string>(page, "location.href.split('#')[0].split('?')[0]");
                res.check('default: the shareable address carries no token, the sharer\'s own included', !shareable.includes('token'), shareable);
                await page.locator('.sidebar-section-header:has-text("Video")').first().click();
                await sleep(500);
                const baseNavs = navigations.length;
                await page.locator('#streamModeSelect').first().selectOption({ value: 'webrtc' });
                let switched = false;
                for (const until = Date.now() + 20_000; Date.now() < until && !switched;) {
                    // Yields to Playwright, so a dialog would reach the handler.
                    await page.waitForTimeout(500);
                    const s = await request(port, 'GET', '/api/status');
                    const parsed: unknown = s.status === 200 ? JSON.parse(s.body.toString()) : null;
                    switched = field(parsed, 'current_mode') === 'webrtc';
                }
                res.check('default: the switch went through on the session token', switched);
                res.check('default: nothing asked the user for the master token', prompts.length === 0, prompts);
                res.check('default: the switch did not reload the page', navigations.length === baseNavs, navigations.slice(baseNavs));
                const switches = requests.filter((q) => q[0] === 'POST' && q[1].includes('/api/switch'));
                res.check('default: the dashboard presents its own token on the switch', switches.length > 0 && switches[0][2].authorization === `Bearer ${CTRL_TOKEN}`, switches.map((q) => q[2].authorization));
            }
        } finally {
            await browser.close();
            await host.stop();
        }
    }
    return res;
}

async function runLegacy(host: Host): Promise<Checks> {
    const res = new Checks('legacy');
    freshFilesDir();
    let { port } = await host.start({ label: 'legacy-open', mode: 'websockets', env: { SELKIES_FILE_MANAGER_PATH: FILES_DIR, SELKIES_ENABLE_METRICS_HTTP: 'true' } });
    for (const [method, path] of [['GET', '/api/files/'], ['GET', '/api/files/hello.txt'], ['GET', '/api/metrics']] as const) {
        const r = await request(port, method, path, { ...bearer('junk'), ...cookie('junk') });
        res.check(`no master token, Basic off: ${method} ${path} open`, r.status === 200, r.status);
    }
    res.check('no master token, Basic off: upload open', (await upload(port, 'legacy.txt', 'l')) === 200);
    await host.stop();

    const basic = { Authorization: `Basic ${Buffer.from('user:secret').toString('base64')}` };
    const viewonly = { Authorization: `Basic ${Buffer.from('user:look').toString('base64')}` };
    ({ port } = await host.start({ label: 'legacy-basic', mode: 'websockets', env: {
        SELKIES_FILE_MANAGER_PATH: FILES_DIR, SELKIES_ENABLE_METRICS_HTTP: 'true', SELKIES_ENABLE_BASIC_AUTH: 'true',
        SELKIES_BASIC_AUTH_USER: 'user', SELKIES_BASIC_AUTH_PASSWORD: 'secret', SELKIES_BASIC_AUTH_VIEWONLY_PASSWORD: 'look',
    } }));
    for (const path of ['/api/files/', '/api/metrics', '/']) {
        const r = await request(port, 'GET', path);
        res.check(`Basic on: GET ${path} challenges with Basic`, r.status === 401 && (r.headers['www-authenticate'] ?? '').startsWith('Basic realm='), `${r.status} ${JSON.stringify(r.headers['www-authenticate'] ?? null)}`);
    }
    let r = await request(port, 'GET', '/api/files/', bearer(CTRL_TOKEN));
    res.check('Basic on, no master token: a Bearer token is not a credential', r.status === 401, r.status);
    r = await request(port, 'GET', '/api/files/', basic);
    res.check('Basic on: Basic credentials list files', r.status === 200 && r.body.includes('hello.txt'), r.status);
    res.check('Basic on: Basic credentials upload', (await upload(port, 'basic.txt', 'b', basic)) === 200);
    res.check('Basic on: the view-only password cannot upload', (await upload(port, 'viewonly.txt', 'v', viewonly)) === 403);
    r = await request(port, 'GET', '/api/files/', viewonly);
    res.check('Basic on: the view-only password cannot list files', r.status === 403 && !r.body.includes('hello.txt'), r.status);
    r = await request(port, 'GET', '/api/files/hello.txt', viewonly);
    res.check('Basic on: the view-only password cannot download', r.status === 403 && !r.body.includes('hello'), r.status);
    await host.stop();
    return res;
}

const BLOCKS: Record<string, (host: Host) => Promise<Checks>> = {
    routes: runRoutes, websockets: runWebsockets, fragment: runFragment, webrtc: runWebrtc,
    'tokens-file': runTokensFile, dashboards: runDashboards, legacy: runLegacy,
};

for (const [name, block] of Object.entries(BLOCKS)) {
    test(name, async () => {
        const host = new Host(suiteOf(import.meta.filename));
        let res: Checks;
        try {
            res = await block(host);
        } finally {
            await host.stop();
            await host.close();
        }
        res.finish();
    });
}
