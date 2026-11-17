/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The figures a page shows reach it only while its stats are open, on both
// transports: nothing periodic arrives before a dashboard says so, what the
// server then publishes as `stream_stats` is what the sampler reads on this
// host, the memory total being the cgroup's limit where one is set, it keeps
// arriving, and it stops when the stats shut. The page is also told what the
// stream runs on (`stream_info`, from the capture's own report) without
// asking.
import { existsSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';

import type { Page } from '@playwright/test';
import { test } from '@playwright/test';

import { launchChromium } from '../lib/browsers.ts';
import { Checks } from '../lib/checks.ts';
import { sleep, suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import { waitWrVideo, waitWsVideo, within } from '../lib/stream.ts';

function read(directory: string, name: string): string {
    try {
        return readFileSync(join(directory, name), 'utf8').trim();
    } catch {
        return '';
    }
}

const isDigits = (v: string | undefined): v is string => v !== undefined && /^\d+$/.test(v);

/** The host's memory total and use, as a system-wide sampler reads them. */
function hostMemory(): [number, number] {
    const info = new Map<string, number>();
    for (const line of readFileSync('/proc/meminfo', 'utf8').split('\n')) {
        const [key, value] = line.split(':');
        if (value) info.set(key.trim(), parseInt(value, 10) * 1024);
    }
    const total = info.get('MemTotal') ?? 0;
    const used = total - (info.get('MemFree') ?? 0) - (info.get('Buffers') ?? 0) - (info.get('Cached') ?? 0) - (info.get('SReclaimable') ?? 0);
    return [total, used >= 0 ? used : total - (info.get('MemAvailable') ?? 0)];
}

/**
 * The session's memory total and use as the server reports them, read
 * independently: where the session's cgroup caps CPU or memory below the
 * host, its memory limit (the host's total without one) and its use less the
 * page cache the kernel can drop (the inactive file pages); otherwise the
 * system-wide figures.
 */
function memoryReference(): [number, number] {
    const [hostTotal, hostUsed] = hostMemory();
    let lines: string[] = [];
    try {
        lines = readFileSync('/proc/self/cgroup', 'utf8').split('\n');
    } catch {
        // no cgroup
    }
    for (const line of lines) {
        const parts = line.split(':');
        if (parts.length < 3) continue;
        const controllers = parts[1];
        const path = parts.slice(2).join(':').replace(/^\/+/, '');
        let d: string;
        let files: [string, string, string];
        let limits: string[];
        if (controllers === '') {
            d = join('/sys/fs/cgroup', path);
            files = ['memory.current', 'memory.max', 'inactive_file'];
            limits = ['cpu.max', 'memory.max'];
        } else if (controllers.split(',').includes('memory')) {
            d = join('/sys/fs/cgroup', controllers, path);
            if (!(existsSync(d) && statSync(d).isDirectory())) d = join('/sys/fs/cgroup', controllers);
            files = ['memory.usage_in_bytes', 'memory.limit_in_bytes', 'total_inactive_file'];
            limits = ['memory.limit_in_bytes'];
        } else {
            continue;
        }
        const limited = limits.some((name) => {
            const value = read(d, name).split(/\s+/)[0];
            return isDigits(value) && (name.startsWith('cpu') || Number(value) < hostTotal);
        });
        const current = read(d, files[0]);
        const limit = read(d, files[1]);
        if (!limited || !isDigits(current)) continue;
        let cache = 0;
        for (const stat of read(d, 'memory.stat').split('\n')) {
            const [key, value] = stat.split(' ');
            if (key === files[2] && isDigits(value)) cache = Number(value);
        }
        const total = isDigits(limit) && Number(limit) < hostTotal ? Number(limit) : hostTotal;
        return [total, Math.max(0, Number(current) - cache)];
    }
    return [hostTotal, hostUsed];
}

const STATS_JS = `(() => {
    const latest = window.stream_stats && window.stream_stats.latest;
    return latest && latest.mem_total ? [latest.cpu_percent, latest.mem_total, latest.mem_used, latest.t] : null;
})()`;
const HISTORY_JS = 'window.stream_stats ? window.stream_stats.history.length : -1';
const INFO_JS = 'window.stream_info';

const js = (page: Page, code: string): Promise<unknown> => within(page.evaluate(code));
const stats = async (page: Page): Promise<number[] | null> => {
    const got = await js(page, STATS_JS);
    return Array.isArray(got) ? got.map(Number) : null;
};
const history = async (page: Page): Promise<number> => Number(await js(page, HISTORY_JS));
const statsOpen = (page: Page, open: boolean): Promise<unknown> => js(page, `window.postMessage({ type: 'statsOpen', open: ${open} }, window.location.origin)`);

async function waitFor<T>(fn: () => Promise<T>, timeoutS = 20): Promise<T> {
    for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
        const got = await fn();
        if (got) return got;
        await sleep(500);
    }
    return fn();
}

async function block(host: Host, res: Checks, mode: string): Promise<void> {
    const { url } = await host.start({ label: mode, mode });
    const browser = await launchChromium();
    try {
        const ctx = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1 });
        await ctx.addInitScript({ content: `window.__SELKIES_STREAMING_MODE__ = '${mode}';` });
        const page = await ctx.newPage();
        await page.goto(url, { waitUntil: 'load' });
        res.check('video streams', (mode === 'webrtc' ? await waitWrVideo(page, 45) : await waitWsVideo(page, 30)) !== null);
        await sleep(4000);
        res.check('nothing periodic arrives while the stats are shut', (await stats(page)) === null && (await history(page)) === 0, await history(page));
        const info = await waitFor(() => js(page, INFO_JS));
        const f = info !== null && typeof info === 'object' ? { ...info } : {};
        res.check('the page is told what the stream runs on without asking', Boolean(info) && Reflect.get(f, 'backend') === 'x11' && Boolean(Reflect.get(f, 'encoder')) && Boolean(Reflect.get(f, 'capture')), info);
        await statsOpen(page, true);
        const first = await waitFor(() => stats(page));
        const [total, used] = memoryReference();
        res.check('opened, the page holds the session\'s CPU and memory figures', first !== null && first[0] >= 0 && first[0] <= 100 && first[2] > 0 && first[2] <= first[1], first);
        res.check("the memory total is what the session's own cgroup allows", first !== null && first[1] === total, [first?.[1] ?? null, total]);
        res.check("the use it shows is the cgroup's, near what the sampler reads here", first !== null && Math.abs(first[2] - used) < Math.max(total * 0.05, 256 * 1024 * 1024), [first?.[2] ?? null, used]);
        let later = await stats(page);
        for (const until = Date.now() + 12_000; Date.now() < until && JSON.stringify(later) === JSON.stringify(first);) {
            await sleep(500);
            later = await stats(page);
        }
        res.check('the figures keep arriving', later !== null && JSON.stringify(later) !== JSON.stringify(first), [first, later]);
        res.check('and the history grows with them', (await history(page)) >= 2, await history(page));
        await statsOpen(page, false);
        await sleep(3000);
        res.check('shut, the history is empty and stays empty', (await stats(page)) === null && (await history(page)) === 0, await history(page));
    } finally {
        await browser.close();
        await host.stop();
    }
}

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const res = new Checks(`system-usage-${mode}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await block(host, res, mode);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
