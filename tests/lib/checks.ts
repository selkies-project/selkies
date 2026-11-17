/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * A suite's checks: one `PASS`/`FAIL  [block] name  detail` line each as it
 * is made, the lines the suites' driver reads in every language, and a
 * failure of the test at its end when any check failed, so every check of a
 * block runs and is reported before the block fails.
 * @module
 */

/** One check made. */
interface Made {
    name: string;
    ok: boolean;
    detail: string;
}

/**
 * Text for a check's detail: a string as it is, anything else as JSON.
 * @param detail What the check saw.
 * @returns Its text.
 */
function said(detail: unknown): string {
    if (typeof detail === 'string') return detail;
    if (detail === undefined) return '';
    try {
        return JSON.stringify(detail) ?? typeof detail;
    } catch {
        return typeof detail;
    }
}

/** The checks of one block, printed as they are made. */
export class Checks {
    readonly block: string;
    private readonly made: Made[] = [];
    private readonly skipped: { name: string; reason: string }[] = [];

    /** @param block The name the block's lines carry in brackets. */
    constructor(block: string) {
        this.block = block;
    }

    /**
     * Record one check and print its line.
     * @param name What is held.
     * @param ok Whether it held; any truthy value passes.
     * @param detail What was seen, for the line.
     */
    check(name: string, ok: unknown, detail: unknown = ''): void {
        const text = said(detail);
        this.made.push({ name, ok: Boolean(ok), detail: text.slice(0, 160) });
        process.stdout.write(`${ok ? 'PASS' : 'FAIL'}  [${this.block}] ${name}  ${text.slice(0, 110)}\n`);
    }

    /**
     * Record a check the host cannot observe: not a failure here, but the
     * driver fails a run that skips.
     * @param name What would have been held.
     * @param reason Why it cannot be.
     */
    skip(name: string, reason: unknown = ''): void {
        this.skipped.push({ name, reason: said(reason) });
        process.stdout.write(`SKIP  [${this.block}] ${name}  ${said(reason).slice(0, 110)}\n`);
    }

    /** @returns The names of the checks that failed. */
    failed(): string[] {
        return this.made.filter((m) => !m.ok).map((m) => m.name);
    }

    /**
     * Print the block's pass count and fail the test when any check failed.
     * @throws {Error} Naming the failed checks.
     */
    finish(): void {
        const failed = this.summary();
        if (failed.length) throw new Error(`[${this.block}] failed: ${failed.join('; ')}`);
    }

    /**
     * Print the block's pass count, as `finish` does, without failing.
     * @returns The names of the checks that failed.
     */
    summary(): string[] {
        const failed = this.failed();
        const tail = this.skipped.length ? `, ${this.skipped.length} skipped` : '';
        process.stdout.write(`[${this.block}] ${this.made.length - failed.length}/${this.made.length} passed${tail}\n`);
        return failed;
    }

    /** @returns The counts, each failure as `name: detail` and each skip as `name: reason`. */
    tally(): { pass: number; fail: number; skip: number; failed: string[]; skipped: string[] } {
        const failed = this.made.filter((m) => !m.ok);
        return {
            pass: this.made.length - failed.length, fail: failed.length, skip: this.skipped.length,
            failed: failed.map((m) => `${m.name}: ${m.detail}`),
            skipped: this.skipped.map((s) => `${s.name}: ${s.reason}`),
        };
    }
}

/**
 * Print every block's pass count, then fail the test when any block had a
 * failed check, so a run of several blocks reports them all.
 * @param blocks The blocks, in the order they ran.
 * @throws {Error} Naming each block's failed checks.
 */
export function finishAll(blocks: readonly Checks[]): void {
    const errors: string[] = [];
    for (const block of blocks) {
        try {
            block.finish();
        } catch (e) {
            errors.push(e instanceof Error ? e.message : String(e));
        }
    }
    if (errors.length) throw new Error(errors.join('\n'));
}
