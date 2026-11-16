/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The processes under this one, read from /proc.
 * @module
 */

import { readdirSync, readFileSync } from 'node:fs';

/**
 * The parent of every process now, from /proc.
 * @returns Each pid's parent pid.
 */
function parents(): Map<number, number> {
    const found = new Map<number, number>();
    for (const entry of readdirSync('/proc')) {
        if (!/^\d+$/.test(entry)) continue;
        try {
            // The name in parentheses may hold spaces and parentheses itself.
            const stat = readFileSync(`/proc/${entry}/stat`, 'utf8');
            const ppid = Number(stat.slice(stat.lastIndexOf(')') + 2).split(' ')[1]);
            found.set(Number(entry), ppid);
        } catch {
            // Gone between the listing and the read.
        }
    }
    return found;
}

/**
 * Every process below `root`, at any depth.
 * @param root The process.
 * @returns Their pids.
 */
export function descendants(root: number = process.pid): number[] {
    const tree = parents();
    const found: number[] = [];
    const todo = [root];
    while (todo.length) {
        const parent = todo.pop();
        for (const [pid, ppid] of tree) {
            if (ppid === parent) {
                found.push(pid);
                todo.push(pid);
            }
        }
    }
    return found;
}

/**
 * A process's name, as the kernel keeps it.
 * @param pid The process.
 * @returns Its name, or an empty string once it is gone.
 */
export function nameOf(pid: number): string {
    try {
        return readFileSync(`/proc/${pid}/comm`, 'utf8').trim();
    } catch {
        return '';
    }
}

/**
 * SIGKILL every process under this one whose name matches.
 * @param matches Whether a (lower-cased) name is one to kill.
 * @returns How many were signalled.
 */
export function killDescendants(matches: (name: string) => boolean): number {
    let killed = 0;
    for (const pid of descendants()) {
        if (!matches(nameOf(pid).toLowerCase())) continue;
        try {
            process.kill(pid, 'SIGKILL');
            killed++;
        } catch {
            // Gone already.
        }
    }
    return killed;
}
