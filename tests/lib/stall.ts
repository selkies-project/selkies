/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What an engine that stopped answering was doing: the host's load and
 * memory, and every WebKit thread's name, state, kernel wait channel, CPU
 * share over a second and stack, threads with one stack grouped. Printed, and
 * kept in the work directory as well, the only record a suite that passes on
 * its retry leaves.
 * @module
 */

import { spawnSync } from 'node:child_process';
import { appendFileSync, readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

import type { Page } from '@playwright/test';

import { sleep, WORKDIR } from './env.ts';

const WEBKIT_PROCESSES = ['MiniBrowser', 'WPE', 'WebKit'];
const GDB_THREAD = /^Thread \d+ \(.*?LWP (\d+)\)/;
const GDB_FRAME = /^#\d+\s+(?:(0x[0-9a-f]+) in )?(\S+)/;
/** How many times each process has been dumped: twice is enough to tell a hang from a slow spell. */
const dumped = new Map<number, number>();

const read = (path: string): string | null => {
    try {
        return readFileSync(path, 'utf8');
    } catch {
        return null;
    }
};

/** PIDs below `root`, from /proc. */
function descendants(root: number): number[] {
    const children = new Map<number, number[]>();
    for (const entry of readdirSync('/proc')) {
        if (!/^\d+$/.test(entry)) continue;
        const stat = read(`/proc/${entry}/stat`);
        const ppid = Number(stat?.slice(stat.lastIndexOf(')') + 1).trim().split(/\s+/)[1]);
        if (!Number.isInteger(ppid)) continue;
        children.set(ppid, [...(children.get(ppid) ?? []), Number(entry)]);
    }
    const found: number[] = [];
    for (const todo = [root]; todo.length;) {
        const kids = children.get(todo.pop() ?? -1) ?? [];
        found.push(...kids);
        todo.push(...kids);
    }
    return found;
}

interface Thread { name: string; state: string; ticks: number; wchan: string; start: number }

/** Each thread of `pid`, by TID. */
function threads(pid: number): Map<number, Thread> {
    const out = new Map<number, Thread>();
    let tids: string[];
    try {
        tids = readdirSync(`/proc/${pid}/task`);
    } catch {
        return out;
    }
    for (const tid of tids) {
        const stat = read(`/proc/${pid}/task/${tid}/stat`);
        if (stat === null) continue;
        const close = stat.lastIndexOf(')');
        const fields = stat.slice(close + 1).trim().split(/\s+/);
        out.set(Number(tid), {
            name: stat.slice(stat.indexOf('(') + 1, close),
            state: fields[0],
            ticks: Number(fields[11]) + Number(fields[12]),
            wchan: read(`/proc/${pid}/task/${tid}/wchan`)?.trim() || '-',
            start: Number(fields[19]),
        });
    }
    return out;
}

/**
 * Every thread's backtrace from gdb, one "function (library)" line a frame,
 * by TID: under `sudo -n` where it works, Yama refusing a non-child otherwise.
 */
function gdbStacks(pid: number): Map<number, string[]> {
    const gdb = ['timeout', '60', 'gdb', '-p', String(pid), '-batch', '-nx', '-iex', 'set debuginfod enabled off', '-ex', 'thread apply all bt 40'];
    let text = '';
    for (const cmd of [['sudo', '-n', ...gdb], gdb]) {
        const run = spawnSync(cmd[0], cmd.slice(1), { encoding: 'utf8', timeout: 90_000 });
        text = run.stdout ?? '';
        if (text.includes('(LWP ')) break;
    }
    // A frame without a symbol is named by its offset in the file mapped there,
    // which holds across runs of one build; one outside any file is JIT code.
    const maps: [bigint, bigint, bigint, string][] = [];
    for (const row of (read(`/proc/${pid}/maps`) ?? '').split('\n')) {
        const cols = row.split(/\s+/);
        if (cols.length > 5 && cols[5].startsWith('/')) {
            const [start, end] = cols[0].split('-').map((x) => BigInt(`0x${x}`));
            maps.push([start, end, BigInt(`0x${cols[2]}`), cols[5].slice(cols[5].lastIndexOf('/') + 1)]);
        }
    }
    const stacks = new Map<number, string[]>();
    let frames: string[] | null = null;
    for (const line of text.split('\n')) {
        const thread = GDB_THREAD.exec(line);
        if (thread) {
            frames = [];
            stacks.set(Number(thread[1]), frames);
            continue;
        }
        const frame = GDB_FRAME.exec(line);
        if (!frame || frames === null) continue;
        const [, addr, func] = frame;
        if (func === '??') {
            const at = BigInt(addr ?? '0x0');
            const hit = maps.find(([start, end]) => start <= at && at < end);
            frames.push(hit ? `${hit[3]}+0x${(at - hit[0] + hit[2]).toString(16)}` : `?? ${addr ?? ''}`);
        } else {
            const from = line.lastIndexOf(' from ');
            const path = from >= 0 ? line.slice(from + ' from '.length) : '';
            const lib = path.slice(path.lastIndexOf('/') + 1);
            frames.push(lib ? `${func} (${lib})` : func);
        }
    }
    return stacks;
}

/**
 * Print, and keep in the work directory, what the engine was doing when a
 * call to it went unanswered.
 * @param why What did not answer.
 * @param page The page, whose last console lines are printed too.
 */
export async function reportStall(why: string, page?: Page): Promise<void> {
    const lines = [`[stall] ${why}`];
    if (page) {
        lines[0] += ` (${page.url().slice(0, 200)})`;
        try {
            const said = [...(await page.consoleMessages()).slice(-40).map((m) => `${m.type()}: ${m.text()}`),
                ...(await page.pageErrors()).slice(-10).map((e) => `pageerror: ${String(e)}`)];
            lines.push(...said.map((s) => `[stall] console ${s.slice(0, 300)}`));
        } catch (e) {
            lines.push(`[stall] console unread: ${String(e)}`);
        }
    }
    try {
        const load = (read('/proc/loadavg') ?? '').split(/\s+/).slice(0, 3).join(' ');
        const avail = Number(/MemAvailable:\s+(\d+)/.exec(read('/proc/meminfo') ?? '')?.[1] ?? 0);
        lines.push(`[stall] load ${load}, ${Math.floor(avail / 1024)} MB available`);
        const procs: [number, string][] = [];
        for (const pid of descendants(process.pid)) {
            const name = read(`/proc/${pid}/comm`)?.trim();
            if (!name || !WEBKIT_PROCESSES.some((p) => name.startsWith(p)) || (dumped.get(pid) ?? 0) >= 2) continue;
            dumped.set(pid, (dumped.get(pid) ?? 0) + 1);
            procs.push([pid, name]);
        }
        const before = new Map(procs.map(([pid]) => [pid, threads(pid)]));
        if (procs.length) await sleep(1000);
        // USER_HZ, which Linux fixes at 100 for /proc's tick counts.
        const tick = 100;
        const now = Number((read('/proc/uptime') ?? '0').split(/\s+/)[0]) * tick;
        for (const [pid, name] of procs) {
            const after = threads(pid);
            const stacks = gdbStacks(pid);
            const age = after.has(pid) ? (now - (after.get(pid)?.start ?? now)) / tick : 0;
            lines.push(`[stall] ${name} ${pid}: ${after.size} threads, up ${age.toFixed(0)}s${stacks.size ? '' : ', no stacks (gdb absent or refused)'}`);
            const groups = new Map<string, string[]>();
            for (const tid of [...after.keys()].sort((a, b) => Number(a !== pid) - Number(b !== pid))) {
                const t = after.get(tid);
                if (!t) continue;
                const cpu = Math.floor(((t.ticks - (before.get(pid)?.get(tid)?.ticks ?? t.ticks)) * 100) / tick);
                const key = (stacks.get(tid) ?? []).join('\n');
                groups.set(key, [...(groups.get(key) ?? []), `${t.name} ${tid} ${t.state} ${t.wchan} ${cpu}%`]);
            }
            for (const [frames, labels] of groups) {
                lines.push(`[stall]   ${labels.join('; ')}`);
                if (frames) lines.push(...frames.split('\n').map((f) => `[stall]     ${f}`));
            }
        }
    } catch (e) {
        lines.push(`[stall] report cut short: ${String(e)}`);
    }
    const text = lines.join('\n');
    process.stdout.write(`${text}\n`);
    try {
        appendFileSync(join(WORKDIR, `browser-stall-${process.pid}.log`), `${text}\n`);
    } catch {
        // the printed copy stands
    }
}
