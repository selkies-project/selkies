/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// 5. Test file upload and download, preferably for large files over 100MB.
// Make sure it's fast enough and uses up the available bandwidth well.
//
// A 150 MiB file goes up through the Files section's Upload Files button and
// its file chooser, and must land in the session byte for byte; another comes
// down through the Download Files listing's own link. Each is timed and set
// against the link's capacity, measured first with a plain TCP stream each way
// between the same two hosts (when the target's address can be reached
// directly, as a pod's can from inside its cluster), whose far end is the
// in-session helper's `bw`.
import { spawn } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { rmSync, writeFileSync } from 'node:fs';
import { connect } from 'node:net';
import { dirname, join } from 'node:path';

import { sleep, WORKDIR } from '../../lib/env.ts';
import { rowDownload, UPLOAD_JS, waitUploadEnd } from '../../lib/transfers.ts';
import { reveal, SITE_ROOT, type Cell } from '../lib.ts';

export const ITEM = 5;
export const TITLE = 'file upload and download over 100 MB';
const SIZE = 150 * 1024 * 1024;
const PORT = 8791;
// A transfer has to reach this share of the measured capacity, or 100 Mbit/s
// where the link is faster than a user's would be (a pod network runs at
// several Gbit/s, past what one TLS stream through the browser carries).
const FLOOR = 0.4;
const ENOUGH = 12.5;

type Link = { down: number; up: number } | { error: string } | null;

/** One TCP stream to the helper: `mode` d reads `n` bytes from it, u writes them; MB/s. */
function stream(host: string, mode: 'd' | 'u', n: number): Promise<number> {
    return new Promise((resolve, reject) => {
        const socket = connect({ host, port: PORT, timeout: 10_000 });
        let t0 = 0;
        let got = 0;
        socket.once('connect', () => {
            socket.setTimeout(0);
            socket.write(mode);
            t0 = performance.now();
            if (mode === 'u') {
                const block = randomBytes(1 << 20);
                let sent = 0;
                const pump = (): void => {
                    while (sent < n) {
                        sent += block.length;
                        if (!socket.write(block)) {
                            socket.once('drain', pump);
                            return;
                        }
                    }
                    socket.end();
                };
                pump();
            }
        });
        socket.on('data', (b: Buffer) => { got += b.length; });
        socket.once('timeout', () => { socket.destroy(new Error('connect timed out')); });
        socket.once('error', reject);
        socket.once('close', () => {
            const secs = (performance.now() - t0) / 1000;
            resolve(Math.round(10 * ((mode === 'd' ? got : n) / secs / 1e6)) / 10);
        });
    });
}

/** MB/s down and up over one TCP stream to the session's host, or null where it cannot be reached. */
async function capacity(cell: Cell, n = 100 * 1024 * 1024): Promise<Link> {
    const host = new URL(cell.target.url).hostname.replace(/^\[|\]$/g, '');
    if (host === '127.0.0.1' || host === 'localhost') return null;
    await cell.target.sh(`(setsid timeout 150 ${SITE_ROOT}/selkies-image-site bw ${PORT} ${n} > /dev/null 2>&1 &)`);
    await sleep(1500);
    try {
        const down = await stream(host, 'd', n);
        const up = await stream(host, 'u', n);
        return { down, up };
    } catch (e) {
        return { error: String(e) };
    }
}

/** Bytes curl fetched from `url` and the seconds it took. */
function curl(url: string): Promise<[number, number]> {
    return new Promise((resolve) => {
        const t0 = performance.now();
        const child = spawn('curl', ['-sk', '-o', '/dev/null', '-w', '%{size_download}', url], { stdio: ['ignore', 'pipe', 'ignore'] });
        let out = '';
        child.stdout.on('data', (b: Buffer) => { out += b.toString(); });
        const timer = setTimeout(() => { child.kill('SIGKILL'); }, 600_000);
        child.once('close', () => {
            clearTimeout(timer);
            resolve([Number(out) || 0, (performance.now() - t0) / 1000]);
        });
        child.once('error', () => {
            clearTimeout(timer);
            resolve([0, 1]);
        });
    });
}

export async function run(cell: Cell): Promise<void> {
    const R = cell.res;
    const page = await cell.open();
    await page.evaluate(UPLOAD_JS);
    const link = await capacity(cell);
    const name = `it-${cell.engine}-${cell.transport}-${Math.floor(Date.now() / 1000)}.bin`;
    const local = join(WORKDIR, name);
    // The payload is let go once written: 150 MiB need not stay held through the transfer.
    const digest = ((): string => {
        const data = randomBytes(SIZE);
        writeFileSync(local, data);
        return createHash('sha256').update(data).digest('hex');
    })();
    R.check('the Files section opens', await reveal(page, 'button:has-text("Upload Files")'));
    let t0 = performance.now();
    let chosen = false;
    try {
        const [chooser] = await Promise.all([
            page.waitForEvent('filechooser', { timeout: 8000 }),
            page.locator('button:has-text("Upload Files")').first().click(),
        ]);
        await chooser.setFiles(local);
        chosen = true;
    } catch (e) {
        process.stdout.write(`      (file chooser: ${String(e)})\n`);
    }
    const ups = await waitUploadEnd(page, name, 600);
    const upSecs = (performance.now() - t0) / 1000;
    const where = await cell.target.out(`find ~ /config -maxdepth 4 -name ${name} 2>/dev/null | head -1`);
    const remote = where ? await cell.target.out(`sha256sum ${where} | cut -c1-64`) : '';
    const upRate = Math.round(10 * (SIZE / upSecs / 1e6)) / 10;
    R.check(`a ${SIZE >> 20} MiB upload through Upload Files lands byte for byte`, chosen && remote === digest,
        `${where || 'not found'} in ${upSecs.toFixed(1)} s (${upRate} MB/s); ${ups.length ? JSON.stringify(ups.slice(-1)) : 'no progress'}`);
    rmSync(local, { force: true });

    // Download: a file of the session's through the file manager the Download Files button opens.
    const down = `down-${name}`;
    const folder = where ? dirname(where) : '~/Desktop';
    await cell.target.sh(`head -c ${SIZE} /dev/urandom > ${folder}/${down}`);
    const want = await cell.target.out(`sha256sum ${folder}/${down} | cut -c1-64`);
    let gotSha: string | null = '';
    let downSecs: number | null = null;
    let why = '';
    try {
        await reveal(page, 'button:has-text("Download Files")');
        await page.locator('button:has-text("Download Files")').first().click({ timeout: 5000 });
        const dialog = page.locator('[role="dialog"]:has-text("Files")').first();
        await dialog.waitFor({ timeout: 45_000 });
        R.check("Download Files opens the session's file manager", (await dialog.count()) > 0);
        t0 = performance.now();
        const [found, sha] = await rowDownload(page, dialog, down, 600);
        gotSha = sha;
        downSecs = (performance.now() - t0) / 1000;
        if (!found) why = 'the manager never listed the file';
    } catch (e) {
        why = `${e instanceof Error ? `${e.name}: ${e.message}` : String(e)}`.slice(0, 160);
    }
    const downRate = downSecs ? Math.round(10 * (SIZE / downSecs / 1e6)) / 10 : null;
    R.check(`a ${SIZE >> 20} MiB download through the manager's row arrives byte for byte`, gotSha === want && Boolean(want),
        `in ${downSecs === null ? 'None' : downSecs.toFixed(1)} s (${String(downRate)} MB/s) ${why}`);
    // The same file fetched by curl: what the server's route gives without a browser.
    const [fetched, curlSecs] = await curl(`${cell.target.url}/api/files/${down}`);
    const curlRate = Math.round(10 * (fetched / curlSecs / 1e6)) / 10;
    await cell.target.sh(`rm -f ${folder}/${down} ${where}`);
    if (link && 'down' in link) {
        for (const [way, rate, cap] of [['upload', upRate, link.up], ['download', downRate ?? 0, link.down]] as const) {
            R.check(`the ${way} is fast enough for the link (${rate} of ${cap} MB/s)`, rate >= Math.min(FLOOR * cap, ENOUGH),
                `${Math.round((100 * rate) / cap)}% of a plain TCP stream; curl on the route ${curlRate} MB/s down`);
        }
    } else {
        R.skip('throughput against the link', `the session's host cannot be reached directly (${JSON.stringify(link)})`);
    }
}
