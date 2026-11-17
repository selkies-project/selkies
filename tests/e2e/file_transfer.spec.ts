/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// File transfer end to end: uploads into `file_manager_path`, downloads out
// of it, on both transports and under every `file_transfers` policy value.
//
// websockets / webrtc: the server's upload and download routes, driven the
// way the client drives them (POST /api/upload with the X-Upload-Path header
// and the chunked X-Upload-* headers; GET /api/files/<name> and the directory
// index), with the bytes compared on disk and on the wire; then both layouts
// on a real page: the Files UI's upload button feeds the browser's file
// chooser and the upload lands in the directory. The download then comes back
// the way each dashboard offers it -- the file manager through the index it
// renders in an iframe, whose link is both fetched and clicked, and Wish's
// file manager through the Download button on the uploaded file's row. The
// websockets block also drives one file above the client's slicing threshold
// through the page so the client's chunked path is exercised end to end.
//
// policy: `file_transfers=upload`, `download`, and `none` each refuse the
// other direction on the wire and hide its button in both layouts; the
// manager's JSON listing and its folder creation follow the same two
// permissions; with Basic auth the view-only password (the server's viewer
// ceiling) can neither upload nor list or download files, and a
// shared/viewer page never starts an upload at all.
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { chmodSync, closeSync, constants, existsSync, lstatSync, mkdirSync, openSync, readdirSync, readFileSync, rmSync, statSync, symlinkSync, unlinkSync, writeFileSync, writeSync } from 'node:fs';
import { request as httpRequest } from 'node:http';
import { dirname, join } from 'node:path';

import type { Browser, Locator, Page, Request } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { barOpenMenuItem, closeDialogs, sidebarSection } from '../lib/dashboard.ts';
import { DASHBOARD_DIST, sleep, suiteOf, WORKDIR } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { rowDownload, UPLOAD_JS, waitUploadEnd, type Upload } from '../lib/transfers.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

const LAYOUTS = ['default', 'wish', 'edge'] as const;
const FILES_DIR = join(WORKDIR, 'file-manager');
// One byte over the client's UPLOAD_CHUNK_BYTES: the smallest file the page
// slices into two POSTs.
const CHUNKED_BYTES = 64 * 1024 * 1024 + 1;
const BASIC_USER = 'user';
const BASIC_PASSWORD = 'secret';
const VIEWONLY_PASSWORD = 'look';

type Headers = Record<string, string>;

/**
 * One request against the server at `port`, its path sent as it is written
 * (a `..` in it included); 4xx and 5xx are answers, not errors.
 */
function request(port: number, method: string, path: string, headers: Headers = {}, body?: Buffer): Promise<[number, Headers, Buffer]> {
    return new Promise((resolve, reject) => {
        const req = httpRequest({ host: 'localhost', port, method, path, headers, timeout: 60_000 }, (res) => {
            const chunks: Buffer[] = [];
            res.on('data', (c: Buffer) => chunks.push(c));
            res.on('end', () => {
                const lower: Headers = {};
                for (const [k, v] of Object.entries(res.headers)) lower[k.toLowerCase()] = Array.isArray(v) ? v.join(', ') : String(v ?? '');
                resolve([res.statusCode ?? 0, lower, Buffer.concat(chunks)]);
            });
            res.on('error', reject);
        });
        req.on('error', reject);
        req.on('timeout', () => { req.destroy(new Error('timed out')); });
        if (body) req.write(body);
        req.end();
    });
}

/** POST /api/upload with the headers lib/file-upload.ts sends; the status and body. */
async function clientUpload(port: number, name: string, data: Buffer, extra: Headers = {}, auth: Headers = {}): Promise<[number | string, Buffer]> {
    const headers: Headers = { 'Content-Type': 'application/octet-stream', 'X-Upload-Path': encodeURIComponent(name).replace(/%2F/g, '/'), ...extra, ...auth };
    const [status, , body] = await request(port, 'POST', '/api/upload', headers, data);
    return [status, body];
}

const basic = (password: string): Headers => ({ Authorization: `Basic ${Buffer.from(`${BASIC_USER}:${password}`).toString('base64')}` });

/** An empty file-manager directory seeded with one downloadable file. */
function freshFilesDir(): void {
    rmSync(FILES_DIR, { recursive: true, force: true });
    mkdirSync(FILES_DIR, { recursive: true });
    writeFileSync(join(FILES_DIR, 'seed.txt'), 'seeded before the server started\n');
}

const onDisk = (rel: string): string => join(FILES_DIR, rel);
const sha = (data: Buffer): string => createHash('sha256').update(data).digest('hex');

function fileSha(path: string): string | null {
    try {
        return sha(readFileSync(path));
    } catch {
        return null;
    }
}

async function waitFile(path: string, timeoutS = 15): Promise<boolean> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        if (existsSync(path)) return true;
        await sleep(250);
    }
    return false;
}

/** Hidden staging files anywhere under the file-manager directory: a finished or discarded transfer must leave none behind. */
function stagingLeftovers(): string[] {
    const found: string[] = [];
    const walk = (dir: string): void => {
        for (const entry of readdirSync(dir, { withFileTypes: true })) {
            const path = join(dir, entry.name);
            if (entry.isDirectory()) walk(path);
            else if (entry.name.startsWith('.selkies-upload-')) found.push(path);
        }
    };
    walk(FILES_DIR);
    return found;
}

const has = (body: Buffer, needle: string | Buffer): boolean => body.includes(needle);
const mode = (path: string): number => lstatSync(path).mode & 0o7777;
const oct = (n: number): string => `0o${n.toString(8)}`;
const r = (v: unknown): string => JSON.stringify(v);

function mkfifo(path: string): void {
    const made = spawnSync('mkfifo', [path]);
    if (made.status !== 0) throw new Error(`mkfifo ${path} failed`);
}

function entriesOf(body: Buffer): Record<string, unknown>[] {
    const parsed: unknown = JSON.parse(body.toString());
    const list: unknown = parsed !== null && typeof parsed === 'object' ? Reflect.get(parsed, 'entries') : undefined;
    return Array.isArray(list) ? list.filter((e): e is Record<string, unknown> => e !== null && typeof e === 'object').map((e) => ({ ...e })) : [];
}

/**
 * The HTTP routes as the client uses them. `full` also covers slicing, the
 * refusals, and the index details; the other transport's block repeats only
 * the plain round trip, since the routes are transport-independent.
 */
async function apiChecks(res: Checks, port: number, full: boolean): Promise<void> {
    const payload = randomBytes(1 << 20);
    let [status, body] = await clientUpload(port, 'sub dir/plain.bin', payload);
    res.check('plain POST /api/upload accepted', status === 200 && has(body, '"success"'), `${status} ${body.subarray(0, 60).toString()}`);
    res.check('the plain upload landed byte for byte', (await waitFile(onDisk('sub dir/plain.bin'), 5)) && fileSha(onDisk('sub dir/plain.bin')) === sha(payload));
    let [st, headers, got] = await request(port, 'GET', '/api/files/sub%20dir/plain.bin');
    res.check('GET /api/files/<name> serves the same bytes as an attachment', st === 200 && sha(got) === sha(payload) && (headers['content-disposition'] ?? '').includes('attachment'),
        `${st} ${headers['content-disposition'] ?? 'None'}`);
    [st, , got] = await request(port, 'GET', '/api/files/');
    res.check('the file index lists the root', st === 200 && has(got, 'seed.txt') && has(got, 'sub dir/'), `${st} ${got.subarray(0, 40).toString()}`);
    if (!full) return;

    // The client slices above its threshold; three slices of this transfer
    // exercise the create / append / finalize legs of the server's .part path.
    const slices = [randomBytes(300 * 1024), randomBytes(200 * 1024), randomBytes(100 * 1024)];
    const total = slices.reduce((n, s) => n + s.length, 0);
    let offset = 0;
    const statuses: [number | string, boolean][] = [];
    for (const [i, part] of slices.entries()) {
        const extra: Headers = { 'X-Upload-Id': 'e2e-transfer-1', 'X-Upload-Offset': String(offset), 'X-Upload-Total': String(total) };
        if (i === slices.length - 1) extra['X-Upload-Final'] = '1';
        [status, body] = await clientUpload(port, 'chunked.bin', part, extra);
        statuses.push([status, has(body, '"complete": true') || has(body, '"complete":true')]);
        offset += part.length;
    }
    res.check('chunked slices accepted, only the final one completes', r(statuses.map(([s]) => s)) === r([200, 200, 200]) && r(statuses.map(([, c]) => c)) === r([false, false, true]), statuses);
    res.check('the chunked upload is the concatenation of its slices', fileSha(onDisk('chunked.bin')) === sha(Buffer.concat(slices)));
    res.check('no staging file is left behind', stagingLeftovers().length === 0, stagingLeftovers());
    writeFileSync(onDisk('setid.bin'), '');
    chmodSync(onDisk('setid.bin'), 0o4755);
    const planted = mode(onDisk('setid.bin'));
    [status] = await clientUpload(port, 'setid.bin', Buffer.from('replaced'));
    const after = mode(onDisk('setid.bin'));
    res.check('an upload over a set-ID file keeps its permission bits and drops set-ID', planted === 0o4755 && status === 200 && after === 0o755, `${oct(planted)} ${status} ${oct(after)}`);
    [status] = await clientUpload(port, 'broken.bin', Buffer.from('first'), { 'X-Upload-Id': 'e2e-transfer-2', 'X-Upload-Offset': '0', 'X-Upload-Total': '10' });
    const [status2, body2] = await clientUpload(port, 'broken.bin', Buffer.from('later'), { 'X-Upload-Id': 'e2e-transfer-2', 'X-Upload-Offset': '9', 'X-Upload-Total': '10', 'X-Upload-Final': '1' });
    res.check('a slice at the wrong offset is refused with 409 and the transfer discarded', status === 200 && status2 === 409 && !existsSync(onDisk('broken.bin')) && stagingLeftovers().length === 0,
        `${status} ${status2} ${body2.subarray(0, 60).toString()}`);
    // Where the staging file would be if its name came from the destination
    // alone, which any account sharing the directory can compute.
    const fifo = onDisk(`.selkies-upload-${createHash('sha256').update(onDisk('planted.bin')).digest('hex').slice(0, 16)}.part`);
    mkfifo(fifo);
    try {
        [status, body] = await clientUpload(port, 'planted.bin', Buffer.from('landed'), { 'X-Upload-Id': 'e2e-transfer-3', 'X-Upload-Offset': '0', 'X-Upload-Total': '6', 'X-Upload-Final': '1' });
    } catch (e) {
        [status, body] = [e instanceof Error ? e.name : 'Error', Buffer.alloc(0)];
    } finally {
        // A reader releases a server thread stuck opening the FIFO for writing.
        const reader = openSync(fifo, constants.O_RDONLY | constants.O_NONBLOCK);
        unlinkSync(fifo);
        closeSync(reader);
    }
    res.check('a FIFO planted at a staging name computed from the destination is never opened', status === 200 && fileSha(onDisk('planted.bin')) === sha(Buffer.from('landed')), `${status} ${body.subarray(0, 60).toString()}`);
    [status] = await clientUpload(port, 'x.bin', Buffer.from('x'), { 'X-Upload-Id': 'lonely' });
    res.check('X-Upload-Id without X-Upload-Offset is a 400', status === 400, status);
    [status] = await clientUpload(port, '../escape.bin', Buffer.from('x'));
    const escaped = existsSync(join(dirname(FILES_DIR), 'escape.bin'));
    res.check('a path that escapes the root is refused', status === 400 && !escaped, status);

    [st, , got] = await request(port, 'GET', '/api/files/sub%20dir/plain.bin', { Range: 'bytes=100-199' });
    res.check('a Range request is answered with the slice', st === 206 && got.equals(payload.subarray(100, 200)), `${st} ${got.length}`);
    [st, headers] = await request(port, 'HEAD', '/api/files/sub%20dir/plain.bin');
    res.check('HEAD carries the length without a body', st === 200 && headers['content-length'] === String(payload.length), `${st} ${headers['content-length'] ?? 'None'}`);
    [st, headers] = await request(port, 'GET', '/api/files/sub%20dir');
    res.check('a directory without its slash redirects to it', (st === 301 || st === 308) && (headers.location ?? '').endsWith('/api/files/sub%20dir/'), `${st} ${headers.location ?? 'None'}`);
    [st, , got] = await request(port, 'GET', '/api/files/sub%20dir/');
    res.check('a subdirectory index lists its files and the parent link', st === 200 && has(got, 'plain.bin') && has(got, '../'), st);
    const odd = Buffer.concat([Buffer.from(`${FILES_DIR}/odd-`), Buffer.from([0xff]), Buffer.from('.bin')]);
    writeFileSync(odd, '');
    try {
        [st, , got] = await request(port, 'GET', '/api/files/');
    } finally {
        unlinkSync(odd);
    }
    res.check('a name that is not UTF-8 is listed with the rest', st === 200 && has(got, 'seed.txt') && has(got, 'href="odd-%FF.bin"'), `${st} ${got.subarray(0, 60).toString()}`);
    [st] = await request(port, 'GET', '/api/files/missing.bin');
    res.check('a missing file is a 404', st === 404, st);
    [st] = await request(port, 'GET', '/api/files/../seed.txt');
    res.check('a traversal in the download path is refused', st === 403 || st === 404, st);

    // The file manager's JSON API, which the dashboard's file manager speaks
    // instead of the index: one directory at a time, and the changes as their
    // own verbs.
    [st, , got] = await request(port, 'GET', '/api/file-manager/browse');
    const names = new Set(st === 200 ? entriesOf(got).map((e) => e.name) : []);
    res.check('GET /api/file-manager/browse lists the root as JSON', st === 200 && names.has('seed.txt') && names.has('sub dir'), `${st} ${r([...names].map(String).sort().slice(0, 4))}`);
    [st, , got] = await request(port, 'GET', `/api/file-manager/browse?path=${encodeURIComponent('sub dir')}`);
    const entries = st === 200 ? entriesOf(got) : [];
    res.check('GET /api/file-manager/browse?path=<dir> lists that directory', st === 200 && r(entries.map((e) => e.name)) === r(['plain.bin']) && entries[0].is_dir === false && entries[0].size === payload.length,
        `${st} ${r(entries.slice(0, 1))}`);
    [st] = await request(port, 'GET', `/api/file-manager/browse?path=${encodeURIComponent('sub dir/plain.bin')}`);
    res.check('browse on a file rather than a directory is a 404', st === 404, st);
    [st] = await request(port, 'GET', `/api/file-manager/browse?path=${encodeURIComponent('../..')}`);
    res.check('browse refuses a path that escapes the root', st === 400, st);
    symlinkSync(dirname(FILES_DIR), onDisk('_escape'));
    [st] = await request(port, 'GET', '/api/file-manager/browse?path=_escape');
    res.check('browse refuses a symlink leaving the root', st === 400, st);
    unlinkSync(onDisk('_escape'));

    const jsonPost = (verb: string, body: unknown): Promise<[number, Headers, Buffer]> => request(port, 'POST', `/api/file-manager/${verb}`, { 'Content-Type': 'application/json' }, Buffer.from(JSON.stringify(body)));
    [st] = await jsonPost('mkdir', { path: 'json dir' });
    res.check('POST /api/file-manager/mkdir creates a directory', st === 200 && existsSync(onDisk('json dir')) && statSync(onDisk('json dir')).isDirectory(), st);
    [st] = await jsonPost('mkdir', { path: 'json dir' });
    res.check('creating a directory that exists is a 409', st === 409, st);
    [st] = await jsonPost('mkdir', { path: '../outside' });
    res.check('mkdir out of the root is refused', st === 400, st);

    // Nothing the manager offers changes or removes an entry that is already
    // there, so these spellings of the root have nothing to act on at all and
    // the served tree has to survive every one of them.
    for (const [spelling, label] of [['', 'an empty path'], ['.', 'a dot'], ['sub dir/..', 'a round trip']]) {
        [st] = await jsonPost('mkdir', { path: spelling });
        res.check(`mkdir naming the root as ${label} is refused`, st === 400, st);
    }
    [st, , got] = await request(port, 'GET', '/api/file-manager/browse');
    const survived = new Set(st === 200 ? entriesOf(got).map((e) => e.name) : []);
    res.check('the served tree survives a mkdir naming its root', st === 200 && ['seed.txt', 'sub dir', 'json dir'].every((n) => survived.has(n)), `${st} ${r([...survived].map(String).sort().slice(0, 4))}`);

    for (const verb of ['rename', 'delete']) {
        [st] = await jsonPost(verb, { path: 'seed.txt', name: 'x' });
        // The web root's GET catch-all answers any other method with 405.
        res.check(`POST /api/file-manager/${verb} is not served`, st === 405, st);
    }

    // A folder comes back as one archive rather than file by file, and the
    // archive is read back to be sure it is what the directory holds.
    [st, headers, got] = await request(port, 'GET', `/api/file-manager/zip?path=${encodeURIComponent('sub dir')}`);
    res.check('GET /api/file-manager/zip returns the folder as a zip', st === 200 && got.subarray(0, 2).toString() === 'PK' && has(got, 'plain.bin') && (headers['content-type'] ?? '').includes('zip'), st);
    [st] = await request(port, 'GET', '/api/file-manager/zip');
    res.check('a zip naming the root as an empty path is refused', st === 404, st);
    [st] = await request(port, 'GET', '/api/file-manager/zip?path=seed.txt');
    res.check('a zip on a file rather than a directory is refused', st === 404, st);
    [st, headers] = await request(port, 'GET', `/api/file-manager/zip?path=${encodeURIComponent('sub dir')}`, { Range: 'bytes=3-' });
    res.check('a Range resuming a zip is refused', st === 405 && headers.allow === 'GET', st);
    [st] = await request(port, 'GET', `/api/file-manager/zip?path=${encodeURIComponent('sub dir')}`, { 'If-Modified-Since': 'Thu, 01 Jan 2026 00:00:00 GMT' });
    res.check('a conditional GET revalidating a zip is refused', st === 405, st);
    // A FIFO would wedge the pack's read of it: the pack skips anything that
    // is not a regular file, so the archive comes back complete without the
    // pipe instead of hanging the request.
    const pipe = onDisk(join('sub dir', 'pipe'));
    mkfifo(pipe);
    try {
        [st, , got] = await request(port, 'GET', `/api/file-manager/zip?path=${encodeURIComponent('sub dir')}`);
    } finally {
        unlinkSync(pipe);
    }
    res.check('a zip skips a FIFO rather than wedging the pack on it', st === 200 && got.subarray(0, 2).toString() === 'PK' && !has(got, 'pipe'), st);
}

/** Close whatever Radix menu is open so the next open starts from a closed menubar. */
async function closeMenus(page: Page): Promise<void> {
    for (let i = 0; i < 4; i++) {
        if ((await page.locator('[role="menu"]').count()) === 0) return;
        await page.keyboard.press('Escape');
        await sleep(300);
    }
}

/** Reach the Files controls: the sidebar's Files section, or a bar layout's menu's Files item, which opens the file manager dialog. */
async function openFilesUi(page: Page, dashboard: string): Promise<boolean> {
    if (dashboard === 'default') {
        try {
            await sidebarSection(page, 'Files');
            return (await page.locator('#files-content').count()) > 0;
        } catch (e) {
            process.stdout.write(`      (files section: ${String(e)})\n`);
            return false;
        }
    }
    await closeMenus(page);
    return barOpenMenuItem(page, 'Files');
}

/**
 * Whether the layout shows the upload and the download. The sidebar's Files
 * section keeps two buttons, Upload Files and Download Files, the second
 * opening the file manager. A bar layout has one menu entry that opens the
 * manager, which carries both: uploads through its Upload button, downloads
 * through the Download button each file's row offers, so the manager's own
 * buttons are what its answer is read from.
 */
async function filesButtons(page: Page, dashboard: string): Promise<[boolean, boolean]> {
    if (dashboard === 'default') {
        return [(await page.locator('#files-content button:has-text("Upload Files")').count()) > 0, (await page.locator('#files-content button:has-text("Download Files")').count()) > 0];
    }
    const dialog = page.locator('[role="dialog"]:has-text("Files")');
    if (!(await dialog.count())) return [false, false];
    // Every row carries the Download button, folders included, so its
    // presence is what says the session may fetch at all.
    return [(await dialog.locator('button[aria-label="Upload"]').count()) > 0, (await dialog.locator('button[aria-label="Download"]').count()) > 0];
}

/** The open file manager dialog, or null when it is not up. */
async function managerDialog(page: Page): Promise<Locator | null> {
    const dialog = page.locator('[role="dialog"]:has-text("Files")').first();
    return (await dialog.count()) ? dialog : null;
}

/** A Chromium page on the dashboard, with the upload recorder installed. */
async function dashboardPage(url: string, mode: string, hash = '', credentials?: { username: string; password: string }): Promise<[Browser, Page]> {
    const browser = await launchChromium();
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1, ...(credentials ? { httpCredentials: credentials } : {}) });
    await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
    await ctx.addInitScript({ content: UPLOAD_JS });
    const page = await ctx.newPage();
    await page.goto(`${url}/${hash}`, { waitUntil: 'load' });
    return [browser, page];
}

const waitVideo = (page: Page, mode: string): Promise<unknown> => (mode === 'webrtc' ? waitWrVideo(page, 45) : waitWsVideo(page, 30));

/** The video the page has had so far: the chunks the WebSockets core counts, or over WebRTC the frames its `<video>` decoded. */
async function framesSeen(page: Page, mode: string): Promise<number> {
    if (mode === 'webrtc') {
        return Number(await within(page.evaluate(`(() => { const v = document.querySelector('video');
          return v && v.getVideoPlaybackQuality ? v.getVideoPlaybackQuality().totalVideoFrames : 0; })()`)));
    }
    return Number(await within(page.evaluate('window.videoChunksReceived || 0')));
}

async function choose(page: Page, button: Locator, name: string, buffer: Buffer): Promise<boolean> {
    try {
        const [chooser] = await Promise.all([page.waitForEvent('filechooser', { timeout: 8000 }), button.click()]);
        await chooser.setFiles({ name, mimeType: 'application/octet-stream', buffer });
        return true;
    } catch (e) {
        process.stdout.write(`      (file chooser: ${String(e)})\n`);
        return false;
    }
}

/** Upload through the dashboard's controls and download through its file manager. */
async function uiRoundTrip(res: Checks, page: Page, dashboard: string, mode: string): Promise<void> {
    const name = `${dashboard}-${mode}.bin`;
    const payload = randomBytes(256 * 1024);
    const folder = `into ${mode}`;
    // Listed when the manager opens, for the upload into a folder below.
    mkdirSync(onDisk(folder), { recursive: true });
    res.check(`${dashboard}: the Files UI opens`, await openFilesUi(page, dashboard));
    const ended = (ups: Upload[]): boolean => ups.some((u) => u.fileName === name && u.status === 'end');
    if (dashboard === 'default') {
        // The sidebar's Upload Files button asks the core's uploader for its
        // picker straight away, with no manager in between.
        const chosen = await choose(page, page.locator('#files-content button:has-text("Upload Files")').first(), name, payload);
        res.check(`${dashboard}: Upload Files opens the browser's file chooser`, chosen);
        const ups = await waitUploadEnd(page, name);
        res.check(`${dashboard}: the upload reports its end to the dashboard`, ended(ups), ups.slice(-2));
        res.check(`${dashboard}: the upload landed in file_manager_path byte for byte`, (await waitFile(onDisk(name), 5)) && fileSha(onDisk(name)) === sha(payload));
        // The chooser took the focus; the manager is reached through the section again.
        await openFilesUi(page, dashboard);
        await page.locator('#files-content button:has-text("Download Files")').first().click();
        await sleep(1500);
    } else {
        // The manager's own Upload button asks the core's uploader for its picker.
        const scope = (await managerDialog(page)) ?? page.locator('body');
        const chosen = await choose(page, scope.locator('button[aria-label="Upload"]').first(), name, payload);
        res.check(`${dashboard}: Upload opens the browser's file chooser`, chosen);
        const ups = await waitUploadEnd(page, name);
        res.check(`${dashboard}: the upload reports its end to the dashboard`, ended(ups), ups.slice(-2));
        res.check(`${dashboard}: the upload landed in file_manager_path byte for byte`, (await waitFile(onDisk(name), 5)) && fileSha(onDisk(name)) === sha(payload));
    }

    // The manager lists the directory over the JSON API and downloads from a
    // row's own action rather than from a link in a rendered listing.
    const dialog = await managerDialog(page);
    if (dialog === null) {
        res.check(`${dashboard}: the file manager lists the upload`, false, 'the dialog is not open');
        return;
    }
    const chunks = await framesSeen(page, mode);
    const [found, downloaded] = await rowDownload(page, dialog, name);
    res.check(`${dashboard}: the file manager lists the upload`, found);
    res.check(`${dashboard}: the row's Download action hands over the same bytes`, downloaded === sha(payload), `${name} -> ${downloaded ?? 'None'}`);
    // A download that navigates the tab fires beforeunload, on which the core
    // closes the session even though the attachment keeps the page.
    await sleep(2000);
    res.check(`${dashboard}: the download leaves the stream running`, (await framesSeen(page, mode)) > chunks);
    // The folder the manager shows is where its Upload sends the picked
    // files: the request names it to the core's uploader.
    const inner = `${dashboard}-${mode}-in-folder.bin`;
    const innerPayload = randomBytes(4096);
    try {
        await dialog.locator(`button:has-text("${folder}")`).first().click();
        await dialog.locator(`nav button:has-text("${folder}")`).first().waitFor({ timeout: 10_000 });
        const [chooser] = await Promise.all([page.waitForEvent('filechooser', { timeout: 8000 }), dialog.locator('button[aria-label="Upload"]').first().click()]);
        await chooser.setFiles({ name: inner, mimeType: 'application/octet-stream', buffer: innerPayload });
    } catch (e) {
        process.stdout.write(`      (upload into a folder: ${String(e)})\n`);
    }
    const target = onDisk(join(folder, inner));
    res.check(`${dashboard}: an upload from inside a folder lands in that folder`, (await waitFile(target, 10)) && fileSha(target) === sha(innerPayload), target);
    await closeDialogs(page);
}

/**
 * One file above the client's slicing threshold, set on the core's file input
 * (what the Upload Files button's chooser feeds): the page posts it as slices
 * and the server reassembles it, reporting its progress as it goes.
 */
async function chunkedThroughPage(res: Checks, page: Page, dashboard: string): Promise<void> {
    const src = join(WORKDIR, 'chunked-src.bin');
    const fd = openSync(src, 'w');
    for (let i = 0; i < Math.floor(CHUNKED_BYTES / (1 << 20)); i++) writeSync(fd, randomBytes(1 << 20));
    writeSync(fd, randomBytes(CHUNKED_BYTES % (1 << 20)));
    closeSync(fd);
    const requests: Request[] = [];
    page.on('request', (q) => { if (q.method() === 'POST' && q.url().includes('/api/upload')) requests.push(q); });
    const started = Date.now();
    await page.setInputFiles('#globalFileInput', src);
    const shown: number[] = [];
    // A transfer with no visible progress reads as a dashboard that dropped
    // it, which is the whole reason the notification exists: a toast in
    // either layout.
    const sample = async (): Promise<void> => {
        if (!shown.length && (await page.locator('[data-sonner-toast]:has-text("chunked-src.bin")').count()) > 0) shown.push((Date.now() - started) / 1000);
    };
    const ups = await waitUploadEnd(page, 'chunked-src.bin', 180, sample);
    const elapsed = (Date.now() - started) / 1000;
    res.check(`${dashboard}: the transfer is on screen while it runs`, shown.length > 0, shown.length ? `first seen after ${shown[0].toFixed(1)}s of ${elapsed.toFixed(1)}s` : 'never shown');
    res.check('a file above the slicing threshold uploads from the page', ups.some((u) => u.fileName === 'chunked-src.bin' && u.status === 'end'), `${elapsed.toFixed(1)}s ${r(ups.slice(-1))}`);
    const slices = requests.filter((q) => q.headers()['x-upload-id']);
    const offsets = slices.map((q) => Number(q.headers()['x-upload-offset'] ?? -1)).sort((a, b) => a - b);
    res.check('the page sliced it into sequential chunk POSTs', slices.length === 2 && r(offsets) === r([0, CHUNKED_BYTES - 1]) && slices.every((q) => q.headers()['x-upload-total'] === String(CHUNKED_BYTES)), offsets);
    res.check('the reassembled file matches the source', (await waitFile(onDisk('chunked-src.bin'), 10)) && fileSha(onDisk('chunked-src.bin')) === fileSha(src) && stagingLeftovers().length === 0);
    unlinkSync(src);
}

const portOf = (url: string): number => Number(new URL(url).port);
const dash = (layout: string | null, env: Record<string, string>): Record<string, string> => ({ SELKIES_WEB_ROOT: DASHBOARD_DIST, ...(layout ? { SELKIES_UI_LAYOUT: layout } : {}), ...env });

/**
 * Routes and every layout's file UI on one transport. The dashboards' server
 * runs Turbo, so the still display keeps sending the frames that show a
 * download left the stream running.
 */
async function transportBlock(host: Host, res: Checks, mode: string): Promise<void> {
    freshFilesDir();
    let { url } = await host.start({ label: `api-${mode}`, mode, env: { SELKIES_FILE_MANAGER_PATH: FILES_DIR } });
    await apiChecks(res, portOf(url), mode === 'websockets');
    for (const dashboard of LAYOUTS) {
        await host.stop();
        ({ url } = await host.start({ label: `${dashboard}-${mode}`, mode, env: dash(dashboard, { SELKIES_FILE_MANAGER_PATH: FILES_DIR, SELKIES_VIDEO_STREAMING_MODE: 'true' }) }));
        const [browser, page] = await dashboardPage(url, mode);
        try {
            const info = await waitVideo(page, mode);
            res.check(`${dashboard}: video streams over ${mode}`, info !== null, info);
            await uiRoundTrip(res, page, dashboard, mode);
            if (mode === 'websockets') await chunkedThroughPage(res, page, dashboard);
        } finally {
            await browser.close();
        }
    }
}

/** `file_transfers` values and the viewer ceiling. */
async function policyBlock(host: Host, res: Checks): Promise<void> {
    for (const [value, uploadOk, downloadOk] of [['upload', true, false], ['download', false, true], ['none', false, false]] as const) {
        for (const dashboard of LAYOUTS) {
            freshFilesDir();
            await host.stop();
            const { url } = await host.start({ label: `policy-${value}-${dashboard}`, mode: 'websockets', env: dash(dashboard, { SELKIES_FILE_MANAGER_PATH: FILES_DIR, SELKIES_FILE_TRANSFERS: value }) });
            const port = portOf(url);
            if (dashboard === 'default') {
                const [status] = await clientUpload(port, `${value}.bin`, Buffer.from('policy'));
                const landed = existsSync(onDisk(`${value}.bin`));
                res.check(`file_transfers=${value}: upload ${uploadOk ? 'accepted' : 'refused'}`, uploadOk ? status === 200 && landed : status === 403 && !landed, status);
                const [st, , body] = await request(port, 'GET', '/api/files/seed.txt');
                res.check(`file_transfers=${value}: download ${downloadOk ? 'served' : 'refused'}`, downloadOk ? st === 200 && body.toString().startsWith('seeded') : st === 403, st);
                const [indexed] = await request(port, 'GET', '/api/files/');
                res.check(`file_transfers=${value}: the index follows the download permission`, indexed === (downloadOk ? 200 : 403), indexed);
            } else {
                // The file manager's own routes: its listing is gated like the
                // index, and its folder creation like an upload.
                let [st, , body] = await request(port, 'GET', '/api/file-manager/browse');
                res.check(`file_transfers=${value}: the JSON listing follows the download permission`, downloadOk ? (st === 200 && has(body, '"status": "ok"')) || has(body, '"status":"ok"') : st === 403,
                    `${st} ${body.subarray(0, 60).toString()}`);
                [st, , body] = await request(port, 'POST', '/api/file-manager/mkdir', { 'Content-Type': 'application/json' }, Buffer.from(JSON.stringify({ path: `mkdir-${value}` })));
                const landed = existsSync(onDisk(`mkdir-${value}`));
                res.check(`file_transfers=${value}: creating a folder ${uploadOk ? 'works' : 'is refused'}`, uploadOk ? st === 200 && landed : st === 403 && !landed, `${st} ${body.subarray(0, 60).toString()}`);
            }
            const [browser, page] = await dashboardPage(url, 'websockets');
            try {
                res.check(`file_transfers=${value}: ${dashboard} streams`, (await waitVideo(page, 'websockets')) !== null);
                const opened = await openFilesUi(page, dashboard);
                const [up, down] = await filesButtons(page, dashboard);
                const py = (b: boolean): string => (b ? 'True' : 'False');
                if (value === 'none') {
                    // Neither layout offers the files at all.
                    res.check(`file_transfers=none: ${dashboard} offers neither button`, !up && !down, `opened=${py(opened)} upload=${py(up)} download=${py(down)}`);
                } else {
                    res.check(`file_transfers=${value}: ${dashboard} shows only the allowed button`, opened && up === uploadOk && down === downloadOk, `upload=${py(up)} download=${py(down)}`);
                }
            } finally {
                await browser.close();
            }
        }
    }

    freshFilesDir();
    await host.stop();
    const { url } = await host.start({ label: 'policy-basic', mode: 'websockets', env: dash(null, {
        SELKIES_FILE_MANAGER_PATH: FILES_DIR, SELKIES_ENABLE_BASIC_AUTH: 'true', SELKIES_BASIC_AUTH_USER: BASIC_USER, SELKIES_BASIC_AUTH_PASSWORD: BASIC_PASSWORD, SELKIES_BASIC_AUTH_VIEWONLY_PASSWORD: VIEWONLY_PASSWORD,
    }) });
    const port = portOf(url);
    let [status] = await clientUpload(port, 'anon.bin', Buffer.from('x'));
    res.check('Basic auth: an upload without credentials is challenged', status === 401, status);
    [status] = await clientUpload(port, 'controller.bin', Buffer.from('controller'), {}, basic(BASIC_PASSWORD));
    res.check('Basic auth: the main password uploads', status === 200 && (await waitFile(onDisk('controller.bin'), 5)), status);
    [status] = await clientUpload(port, 'viewer.bin', Buffer.from('viewer'), {}, basic(VIEWONLY_PASSWORD));
    res.check('Basic auth: the view-only password cannot upload', status === 403 && !existsSync(onDisk('viewer.bin')), status);
    let [st, , body] = await request(port, 'GET', '/api/files/controller.bin', basic(VIEWONLY_PASSWORD));
    res.check('Basic auth: the view-only password cannot download', st === 403 && body.toString() !== 'controller', st);
    [st, , body] = await request(port, 'GET', '/api/files/', basic(VIEWONLY_PASSWORD));
    res.check('Basic auth: the view-only password cannot list files', st === 403 && !has(body, 'controller.bin'), st);
    [st, , body] = await request(port, 'GET', '/api/file-manager/browse', basic(VIEWONLY_PASSWORD));
    res.check('Basic auth: the view-only password cannot browse the JSON listing', st === 403 && !has(body, 'entries'), st);
    [st, , body] = await request(port, 'GET', '/api/file-manager/zip', basic(VIEWONLY_PASSWORD));
    res.check('Basic auth: the view-only password cannot take a folder archive', st === 403 && body.subarray(0, 2).toString() !== 'PK', st);

    // The client side of the ceiling: the core closes the upload gate on a
    // shared/viewer page, so its file input starts no request at all.
    const [browser, page] = await dashboardPage(url, 'websockets', '#shared', { username: BASIC_USER, password: BASIC_PASSWORD });
    try {
        res.check('a shared viewer page streams', (await waitVideo(page, 'websockets')) !== null);
        const posts: string[] = [];
        page.on('request', (q) => { if (q.method() === 'POST' && q.url().includes('/api/upload')) posts.push(q.url()); });
        await page.setInputFiles('#globalFileInput', { name: 'shared.bin', mimeType: 'application/octet-stream', buffer: Buffer.from('shared') });
        await sleep(3000);
        res.check("the shared page's file input starts no upload", posts.length === 0 && !existsSync(onDisk('shared.bin')), posts);
    } finally {
        await browser.close();
    }
}

for (const which of ['websockets', 'webrtc', 'policy']) {
    test(which, async () => {
        const res = new Checks(`files-${which}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            if (which === 'policy') await policyBlock(host, res);
            else await transportBlock(host, res, which);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
